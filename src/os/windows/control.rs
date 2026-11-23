// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The daemon's control channel: a named pipe. Its name is new each time the daemon starts
//! and is kept, with a key, in a file in the home folder, which only this user can read.
//! Each side shows it holds the key before anything else is said, so nothing (passphrases
//! among it) is sent to a pipe someone else made, and nobody else can drive the daemon.

use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    sync::{Mutex, mpsc},
};

/// A connection to the daemon.
pub type Stream = NamedPipeClient;

const PREFIX: &str = r"\\.\pipe\cahoots-";
/// How long either side waits for the other to show its key.
const WAIT: Duration = Duration::from_secs(10);
/// Windows' answer when every instance of a pipe is taken for the moment.
const PIPE_BUSY: i32 = 231;

fn pointer(home: &Path) -> PathBuf {
    home.join("pipe")
}

/// The pipe's name and key, as the running daemon recorded them.
fn recorded(home: &Path) -> Option<(String, [u8; 32])> {
    let text = std::fs::read_to_string(pointer(home)).ok()?;
    let (name, key) = text.trim_end().split_once('\n')?;
    let key = hex::decode(key).ok()?.try_into().ok()?;
    name.starts_with(PREFIX).then(|| (name.to_string(), key))
}

/// What one side sends to show it holds the key, bound to both sides' fresh numbers.
fn proof(key: &[u8; 32], who: &[u8], client: &[u8; 32], daemon: &[u8; 32]) -> blake3::Hash {
    blake3::Hasher::new_keyed(key).update(who).update(client).update(daemon).finalize()
}

/// The client's side: true once the daemon has shown the key and been shown it.
async fn greet<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, key: &[u8; 32]) -> std::io::Result<bool> {
    let (mine, mut theirs, mut shown) = (rand::random::<[u8; 32]>(), [0u8; 32], [0u8; 32]);
    s.write_all(&mine).await?;
    s.read_exact(&mut theirs).await?;
    s.read_exact(&mut shown).await?;
    if proof(key, b"daemon", &mine, &theirs) != blake3::Hash::from_bytes(shown) {
        return Ok(false);
    }
    s.write_all(proof(key, b"client", &mine, &theirs).as_bytes()).await?;
    Ok(true)
}

/// The daemon's side of the same.
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, key: &[u8; 32]) -> std::io::Result<bool> {
    let (mine, mut theirs, mut shown) = (rand::random::<[u8; 32]>(), [0u8; 32], [0u8; 32]);
    s.read_exact(&mut theirs).await?;
    s.write_all(&mine).await?;
    s.write_all(proof(key, b"daemon", &theirs, &mine).as_bytes()).await?;
    s.read_exact(&mut shown).await?;
    Ok(proof(key, b"client", &theirs, &mine) == blake3::Hash::from_bytes(shown))
}

/// Connects to the local daemon, and only to the one that holds this home's key.
pub async fn connect(home: &Path) -> Option<Stream> {
    let (name, key) = recorded(home)?;
    for _ in 0..40 {
        match ClientOptions::new().open(&name) {
            Ok(mut s) => {
                let known = tokio::time::timeout(WAIT, greet(&mut s, &key)).await;
                return matches!(known, Ok(Ok(true))).then_some(s);
            }
            Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(_) => return None,
        }
    }
    None
}

/// The same for a caller with no async runtime (the GUI), on a thread of its own so that a
/// pipe that never answers cannot hold the caller up.
#[cfg(feature = "gui")]
pub fn connect_blocking(home: &Path) -> Option<std::fs::File> {
    use std::{
        io::{Read, Write},
        os::windows::fs::OpenOptionsExt,
    };
    // Whoever made the pipe may learn who we are, and no more than that.
    const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
    let (name, key) = recorded(home)?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let greeted = (|| -> Option<std::fs::File> {
            let mut open = std::fs::OpenOptions::new();
            open.read(true).write(true).security_qos_flags(SECURITY_IDENTIFICATION);
            let mut s = (0..40).find_map(|_| match open.open(&name) {
                Ok(s) => Some(Some(s)),
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                    std::thread::sleep(Duration::from_millis(50));
                    None
                }
                Err(_) => Some(None),
            })??;
            let (mine, mut theirs, mut shown) = (rand::random::<[u8; 32]>(), [0u8; 32], [0u8; 32]);
            s.write_all(&mine).ok()?;
            s.read_exact(&mut theirs).ok()?;
            s.read_exact(&mut shown).ok()?;
            if proof(&key, b"daemon", &mine, &theirs) != blake3::Hash::from_bytes(shown) {
                return None;
            }
            s.write_all(proof(&key, b"client", &mine, &theirs).as_bytes()).ok()?;
            Some(s)
        })();
        let _ = tx.send(greeted);
    });
    rx.recv_timeout(WAIT).ok().flatten()
}

/// The daemon's end: hands out connections whose other end has shown the key.
pub struct Listener(Mutex<mpsc::Receiver<NamedPipeServer>>);

impl Listener {
    pub async fn accept(&self) -> std::io::Result<NamedPipeServer> {
        self.0.lock().await.recv().await.ok_or_else(|| std::io::ErrorKind::BrokenPipe.into())
    }
}

pub fn listen(home: &Path) -> Result<Listener> {
    let name = format!("{PREFIX}{:032x}", rand::random::<u128>());
    let key = rand::random::<[u8; 32]>();
    // The first of its name, so it is ours and not one somebody had waiting.
    let mut next = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&name)?;
    crate::config::write_private(&pointer(home), format!("{name}\n{}\n", hex::encode(key)).as_bytes())?;
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        while !tx.is_closed() {
            if next.connect().await.is_err() {
                let _ = next.disconnect();
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            // The next caller's instance is in place before this one is looked at.
            let Ok(new) = ServerOptions::new().reject_remote_clients(true).create(&name) else { return };
            let mut caller = std::mem::replace(&mut next, new);
            let tx = tx.clone();
            tokio::spawn(async move {
                if matches!(tokio::time::timeout(WAIT, answer(&mut caller, &key)).await, Ok(Ok(true))) {
                    let _ = tx.send(caller).await;
                }
            });
        }
    });
    Ok(Listener(Mutex::new(rx)))
}

/// Puts the daemon out of reach of new commands.
pub fn unlisten(home: &Path) {
    let _ = std::fs::remove_file(pointer(home));
}
