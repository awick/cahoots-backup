// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The daemon's control channel: a unix socket in its home folder.

use anyhow::{Result, bail};
use std::{
    ffi::OsStr,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};
use tokio::net::{UnixListener, UnixStream};

/// A connection to the daemon, or one it accepted.
pub type Stream = UnixStream;

/// The daemon's socket in its home folder.
const SOCK: &str = "cahoots.sock";
/// What it was called before the rename. A client still looks there, after the new name, so
/// that it finds a daemon from before the rename and can replace it. To go in the next version.
const OLD_SOCK: &str = "sdb.sock";

/// Unix socket paths are limited to about 100 bytes, so a deep home puts the socket in a
/// private folder elsewhere and records where in `sock-path`, which only this user can write.
fn sock_path(home: &Path) -> PathBuf {
    let p = home.join(SOCK);
    if p.as_os_str().len() < 100 {
        return p;
    }
    match std::fs::read(pointer(home)) {
        Ok(b) if b.starts_with(b"/") => PathBuf::from(OsStr::from_bytes(b.trim_ascii_end())),
        _ => PathBuf::from(format!("/tmp/cahoots-{}/{}", rustix::process::getuid().as_raw(), sock_name(home))),
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
    tries.extend(bases.iter().map(|b| (b.join(format!("cahoots-{uid}")), false)));
    tries.extend(bases.iter().map(|b| (b.join(format!("cahoots-{uid}-{:016x}", rand::random::<u64>())), true)));
    for (dir, fresh) in tries {
        let p = dir.join(&name);
        if p.as_os_str().len() < 100 && private_dir(&dir, fresh) {
            return Ok(p);
        }
    }
    bail!("found no private folder for the control socket; set CAHOOTS_HOME to a shorter path")
}

/// Where a client looks for the daemon, in order: never a guessed name in a shared folder. A
/// deep home's socket is only where the daemon recorded it, in a folder still private to this
/// user. The name from before the rename comes last (see OLD_SOCK).
fn client_paths(home: &Path) -> Vec<PathBuf> {
    let recorded = || {
        let b = std::fs::read(pointer(home)).ok()?;
        let p = PathBuf::from(OsStr::from_bytes(b.trim_ascii_end()));
        let private = |d: &Path| {
            std::fs::symlink_metadata(d)
                .is_ok_and(|md| md.is_dir() && md.uid() == rustix::process::getuid().as_raw() && md.mode() & 0o077 == 0)
        };
        (p.is_absolute() && p.parent().is_some_and(private)).then_some(p)
    };
    let short = |name: &str| Some(home.join(name)).filter(|p| p.as_os_str().len() < 100);
    let mut paths: Vec<PathBuf> = short(SOCK).or_else(recorded).into_iter().collect();
    paths.extend(short(OLD_SOCK));
    paths
}

/// Connects to the local daemon, but only one run by this user. The kernel says who is at
/// the other end of the connection, whatever was at the path a moment before, so nothing
/// is sent (passphrases and keys among it) to anyone else who got a socket there.
pub async fn connect(home: &Path) -> Option<UnixStream> {
    for path in client_paths(home) {
        let Ok(s) = UnixStream::connect(path).await else { continue };
        if s.peer_cred().is_ok_and(|c| c.uid() == rustix::process::getuid().as_raw()) {
            return Some(s);
        }
    }
    None
}

/// The same for a caller with no async runtime (the GUI). Done on a thread of its own, as
/// the caller may be inside the main runtime, where another cannot run.
#[cfg(feature = "gui")]
pub fn connect_blocking(home: &Path) -> Option<std::os::unix::net::UnixStream> {
    let home = home.to_path_buf();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_io().build().ok()?;
        let s = rt.block_on(async { connect(&home).await?.into_std().ok() })?;
        s.set_nonblocking(false).ok()?;
        Some(s)
    })
    .join()
    .ok()
    .flatten()
}

/// The daemon's end: hands out connections made by this user.
pub struct Listener(UnixListener);

impl Listener {
    /// The next connection. Only this user may drive the daemon, whatever the socket's
    /// permissions say, so anyone else's is dropped.
    pub async fn accept(&self) -> std::io::Result<Stream> {
        loop {
            let (stream, _) = self.0.accept().await?;
            if stream.peer_cred().is_ok_and(|c| c.uid() == rustix::process::getuid().as_raw()) {
                return Ok(stream);
            }
        }
    }
}

pub fn listen(home: &Path) -> Result<Listener> {
    // This daemon holds the home, so a socket under the old name is nobody's any more.
    let _ = std::fs::remove_file(home.join(OLD_SOCK));
    let mut path = home.join(SOCK);
    if path.as_os_str().len() >= 100 {
        path = private_sock(home)?;
        crate::config::write_private(&pointer(home), path.as_os_str().as_bytes())?;
    }
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => bail!("cannot replace {}: {e}", path.display()),
        _ => {}
    }
    // No umask change (it is process wide and other tasks are running); the folder is
    // private and accept() checks each peer's uid, so the brief default mode is harmless.
    let l = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Listener(l))
}

/// Puts the daemon out of reach of new commands.
pub fn unlisten(home: &Path) {
    let _ = std::fs::remove_file(sock_path(home));
}
