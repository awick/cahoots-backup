// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Local control socket so the CLI can drive a running daemon.

use crate::node::{Cmd, Node, Out, clean};
use anyhow::{Result, bail};
use std::{
    ffi::OsStr,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
};

const DONE: char = '\u{1}';

/// Unix socket paths are limited to about 100 bytes, so a deep home puts the socket in a
/// private folder elsewhere and records where in `sock-path`, which only this user can write.
pub fn sock_path(home: &Path) -> PathBuf {
    let p = home.join("sdb.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    match std::fs::read(pointer(home)) {
        Ok(b) if b.starts_with(b"/") => PathBuf::from(OsStr::from_bytes(b.trim_ascii_end())),
        _ => PathBuf::from(format!("/tmp/sdb-{}/{}", rustix::process::getuid().as_raw(), sock_name(home))),
    }
}

fn pointer(home: &Path) -> PathBuf {
    home.join("sock-path")
}

fn sock_name(home: &Path) -> String {
    let h = blake3::hash(home.as_os_str().as_encoded_bytes()).to_hex();
    format!("{}.sock", &h[..16])
}

/// A folder only this user can use, made now if `fresh` (never one someone left waiting).
fn private_dir(dir: &Path, fresh: bool) -> bool {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if fresh || e.kind() != std::io::ErrorKind::AlreadyExists => return false,
        _ => {}
    }
    std::fs::symlink_metadata(dir)
        .is_ok_and(|md| md.is_dir() && md.uid() == rustix::process::getuid().as_raw() && md.mode() & 0o077 == 0)
}

/// A temp folder others cannot rename our folder out of: their own, or a sticky one like /tmp.
fn safe_base(base: &Path) -> bool {
    let uid = rustix::process::getuid().as_raw();
    base.is_absolute()
        && std::fs::metadata(base).is_ok_and(|md| {
            md.is_dir() && (md.mode() & 0o1000 != 0 || (md.mode() & 0o022 == 0 && (md.uid() == uid || md.uid() == 0)))
        })
}

/// Picks the socket for a deep home: the folder used last time, then a fixed name in the
/// per-user runtime or temp folder, then /tmp. The fixed names are predictable, so if another
/// user already holds them an unpredictable one is used; clients find it through `sock-path`.
fn private_sock(home: &Path) -> Result<PathBuf> {
    let (uid, name) = (rustix::process::getuid().as_raw(), sock_name(home));
    let mut tries = Vec::new();
    if let Ok(b) = std::fs::read(pointer(home))
        && let Some(d) = Path::new(OsStr::from_bytes(b.trim_ascii_end())).parent()
        && d.is_absolute()
    {
        tries.push((d.to_path_buf(), false));
    }
    let bases: Vec<PathBuf> = [std::env::var_os("XDG_RUNTIME_DIR"), std::env::var_os("TMPDIR"), Some("/tmp".into())]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .filter(|b| safe_base(b))
        .collect();
    tries.extend(bases.iter().map(|b| (b.join(format!("sdb-{uid}")), false)));
    tries.extend(bases.iter().map(|b| (b.join(format!("sdb-{uid}-{:016x}", rand::random::<u64>())), true)));
    for (dir, fresh) in tries {
        let p = dir.join(&name);
        if p.as_os_str().len() < 100 && private_dir(&dir, fresh) {
            return Ok(p);
        }
    }
    bail!("found no private folder for the control socket; set SDB_HOME to a shorter path")
}

/// Identifies a build by its executable's size and modification time, so a client can
/// tell when the running daemon is older than itself.
pub fn fingerprint() -> String {
    std::env::current_exe()
        .and_then(std::fs::metadata)
        .map(|m| {
            let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
            format!("{}-{}", m.len(), t.map(|d| d.as_nanos()).unwrap_or(0))
        })
        .unwrap_or_default()
}

static STARTED: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The fingerprint of the executable as it was when this process started.
pub fn started_fingerprint() -> String {
    STARTED.get_or_init(fingerprint).clone()
}

/// Connects to the local daemon, but only through a socket this user owns.
pub async fn connect(home: &Path) -> Option<UnixStream> {
    let path = sock_path(home);
    let md = std::fs::symlink_metadata(&path).ok()?;
    if md.uid() != rustix::process::getuid().as_raw() {
        return None;
    }
    UnixStream::connect(&path).await.ok()
}

/// The daemon's socket path, if it exists and belongs to this user.
#[cfg(feature = "gui")]
pub fn own_socket(home: &Path) -> Option<PathBuf> {
    let path = sock_path(home);
    let md = std::fs::symlink_metadata(&path).ok()?;
    (md.uid() == rustix::process::getuid().as_raw()).then_some(path)
}

pub fn listen(home: &Path) -> Result<UnixListener> {
    let mut path = home.join("sdb.sock");
    if path.as_os_str().len() >= 100 {
        path = private_sock(home)?;
        crate::config::write_private(&pointer(home), path.as_os_str().as_bytes())?;
    }
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => bail!("cannot replace {}: {e}", path.display()),
        _ => {}
    }
    // No umask change (it is process wide and other tasks are running); the folder is
    // private and serve() checks each peer's uid, so the brief default mode is harmless.
    let l = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

pub async fn serve(node: Arc<Node>, listener: UnixListener) {
    while let Ok((stream, _)) = listener.accept().await {
        // Only this user may drive the daemon, whatever the socket's permissions say.
        if !stream.peer_cred().is_ok_and(|c| c.uid() == rustix::process::getuid().as_raw()) {
            continue;
        }
        let node = node.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut line = String::new();
            if BufReader::new(r).read_line(&mut line).await.is_err() {
                return;
            }
            let (tx, mut rx) = mpsc::unbounded_channel();
            let cmd = serde_json::from_str::<Cmd>(&line);
            // Backups and pruning belong to the daemon: they carry on when the client that
            // asked for them goes away. Anything else stops with its client.
            let detached = matches!(cmd, Ok(Cmd::Backup { .. } | Cmd::Prune { .. } | Cmd::Forget { .. }));
            let job = tokio::spawn(async move {
                match cmd {
                    Ok(cmd) => node.run(cmd, &Out(tx)).await,
                    Err(e) => Err(e.into()),
                }
            });
            let mut gone = false;
            while let Some(l) = rx.recv().await {
                if !gone && w.write_all(format!("{l}\n").as_bytes()).await.is_err() {
                    if !detached {
                        job.abort();
                        return;
                    }
                    gone = true;
                }
            }
            let end = match job.await {
                Ok(Ok(())) => format!("{DONE}OK\n"),
                Ok(Err(e)) => format!("{DONE}{}\n", clean(&format!("{e:#}"))),
                Err(e) => format!("{DONE}{}\n", clean(&e.to_string())),
            };
            let _ = w.write_all(end.as_bytes()).await;
        });
    }
}

pub async fn client(stream: UnixStream, cmd: &Cmd) -> Result<()> {
    // Whoever reads our output may stop early (`sdb snapshots | head`): leave quietly then.
    request(stream, cmd, &mut |l| {
        use std::io::Write;
        if writeln!(std::io::stdout(), "{l}").is_err() {
            std::process::exit(0);
        }
    })
    .await
}

pub async fn request(stream: UnixStream, cmd: &Cmd, line: &mut (dyn FnMut(String) + Send)) -> Result<()> {
    let (r, mut w) = stream.into_split();
    w.write_all(format!("{}\n", serde_json::to_string(cmd)?).as_bytes()).await?;
    let mut lines = BufReader::new(r).lines();
    while let Some(l) = lines.next_line().await? {
        match l.strip_prefix(DONE) {
            Some("OK") => return Ok(()),
            Some(e) => bail!("{e}"),
            None => line(l),
        }
    }
    bail!("the daemon closed the connection")
}
