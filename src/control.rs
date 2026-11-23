// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Local control channel so the CLI can drive a running daemon.

use crate::node::{Cmd, Node, Out, clean};
#[cfg(feature = "gui")]
pub use crate::os::control::connect_blocking;
pub use crate::os::control::{Listener, Stream, connect, listen, unlisten};
use anyhow::{Result, bail};
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};

const DONE: char = '\u{1}';

/// Identifies a build by its executable's size and modification time, so a client can
/// tell when the running daemon is older than itself.
pub fn fingerprint() -> String {
    crate::os::program()
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

pub async fn serve(node: Arc<Node>, listener: Listener) {
    while let Ok(stream) = listener.accept().await {
        let node = node.clone();
        tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(stream);
            let mut line = String::new();
            if BufReader::new(r).read_line(&mut line).await.is_err() {
                return;
            }
            let (tx, mut rx) = mpsc::unbounded_channel();
            let cmd = serde_json::from_str::<Cmd>(&line);
            // Backups and pruning belong to the daemon: they carry on when the client that
            // asked for them goes away. Anything else stops with its client.
            let detached = cmd.as_ref().is_ok_and(Cmd::detached);
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

pub async fn client(stream: Stream, cmd: &Cmd) -> Result<()> {
    // Whoever reads our output may stop early (`cahoots snapshots | head`): leave quietly then.
    request(stream, cmd, &mut |l| {
        use std::io::Write;
        if writeln!(std::io::stdout(), "{l}").is_err() {
            std::process::exit(0);
        }
    })
    .await
}

pub async fn request(stream: Stream, cmd: &Cmd, line: &mut (dyn FnMut(String) + Send)) -> Result<()> {
    let (r, mut w) = tokio::io::split(stream);
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
