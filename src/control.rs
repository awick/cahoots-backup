// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Local control socket so the CLI can drive a running daemon.

use crate::node::{Cmd, Node, Out};
use anyhow::{Result, bail};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc,
};

const DONE: char = '\u{1}';

/// Unix socket paths are limited to about 100 bytes, so deep homes fall back to /tmp.
pub fn sock_path(home: &Path) -> PathBuf {
    let p = home.join("sdb.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    let h = blake3::hash(home.as_os_str().as_encoded_bytes()).to_hex();
    PathBuf::from(format!(
        "/tmp/sdb-{}-{}.sock",
        rustix::process::getuid().as_raw(),
        &h[..16]
    ))
}

/// Connects to the local daemon, but only through a socket this user owns.
pub async fn connect(home: &Path) -> Option<UnixStream> {
    use std::os::unix::fs::MetadataExt;
    let path = sock_path(home);
    let md = std::fs::symlink_metadata(&path).ok()?;
    if md.uid() != rustix::process::getuid().as_raw() {
        return None;
    }
    UnixStream::connect(&path).await.ok()
}

pub fn listen(home: &Path) -> Result<UnixListener> {
    let path = sock_path(home);
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

pub async fn serve(node: Arc<Node>, listener: UnixListener) {
    while let Ok((stream, _)) = listener.accept().await {
        let node = node.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut line = String::new();
            if BufReader::new(r).read_line(&mut line).await.is_err() {
                return;
            }
            let (tx, mut rx) = mpsc::unbounded_channel();
            let job = tokio::spawn(async move {
                match serde_json::from_str::<Cmd>(&line) {
                    Ok(cmd) => node.run(cmd, &Out(tx)).await,
                    Err(e) => Err(e.into()),
                }
            });
            while let Some(l) = rx.recv().await {
                if w.write_all(format!("{l}\n").as_bytes()).await.is_err() {
                    job.abort();
                    return;
                }
            }
            let end = match job.await {
                Ok(Ok(())) => format!("{DONE}OK\n"),
                Ok(Err(e)) => format!("{DONE}{e:#}\n"),
                Err(e) => format!("{DONE}{e}\n"),
            };
            let _ = w.write_all(end.as_bytes()).await;
        });
    }
}

pub async fn client(stream: UnixStream, cmd: &Cmd) -> Result<()> {
    request(stream, cmd, &mut |l| println!("{l}")).await
}

pub async fn request(
    stream: UnixStream,
    cmd: &Cmd,
    line: &mut (dyn FnMut(String) + Send),
) -> Result<()> {
    let (r, mut w) = stream.into_split();
    w.write_all(format!("{}\n", serde_json::to_string(cmd)?).as_bytes())
        .await?;
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
