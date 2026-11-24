// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The daemon's control channel: a named pipe. Its name is new each time the daemon starts
//! and is kept, with a key, in a file in the home folder, which only this user can read.
//! Only this user (and the system) may open the pipe. Before anything else is said, the
//! client makes sure the pipe and the program serving it are this user's, and each side
//! shows it holds the key, so nothing (passphrases among it) is sent to a pipe someone else
//! made, and nobody else can drive the daemon.

use super::sys;
use anyhow::Result;
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    sync::{Mutex, Semaphore, mpsc},
};

/// A connection to the daemon.
pub type Stream = NamedPipeClient;

const PREFIX: &str = r"\\.\pipe\cahoots-";
/// How long either side waits for the other to show its key.
const WAIT: Duration = Duration::from_secs(10);
/// Windows' answer when every instance of a pipe is taken for the moment.
const PIPE_BUSY: i32 = 231;
/// How many connections the pipe takes at once, as many as Windows lets a pipe be given;
/// more callers wait their turn.
const INSTANCES: usize = 254;
/// How many callers may be showing their key at once; the next waits until one is done.
const GREETING: usize = 4;

fn pointer(home: &Path) -> PathBuf {
    home.join("pipe")
}

/// Whether `name` is one the daemon could have made: the prefix and 32 hex digits, and so
/// a pipe and nothing else.
fn our_name(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// The pipe's name and key, as the running daemon recorded them, in a file that is this
/// user's own: one somebody else left there is not read.
fn recorded(home: &Path) -> Option<(String, [u8; 32])> {
    let mut f = super::open_file(&pointer(home)).ok()?;
    if !sys::made_by_me(&f) {
        return None;
    }
    let mut text = String::new();
    f.read_to_string(&mut text).ok()?;
    let (name, key) = text.trim_end().split_once('\n')?;
    let key = hex::decode(key).ok()?.try_into().ok()?;
    our_name(name).then(|| (name.to_string(), key))
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

/// Connects to the local daemon, and only through a pipe of this user's, served by a
/// program of this user's that holds this home's key.
pub async fn connect(home: &Path) -> Option<Stream> {
    let (name, key) = recorded(home)?;
    for _ in 0..40 {
        match ClientOptions::new().open(&name) {
            Ok(s) if !sys::served_by_me(&s) => return None,
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
            if !sys::served_by_me(&s) {
                return None;
            }
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

/// An instance of the daemon's pipe, which only this user and the system may open.
fn instance(name: &str, first: bool, most: usize) -> std::io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first).reject_remote_clients(true).max_instances(most);
    sys::create_pipe(&options, name)
}

pub fn listen(home: &Path) -> Result<Listener> {
    listen_to(home, INSTANCES)
}

/// `listen`, with at most `most` connections at once.
fn listen_to(home: &Path, most: usize) -> Result<Listener> {
    let name = format!("{PREFIX}{:032x}", rand::random::<u128>());
    let key = rand::random::<[u8; 32]>();
    // The first of its name, so it is ours and not one somebody had waiting.
    let mut next = Some(instance(&name, true, most)?);
    crate::config::write_private(&pointer(home), format!("{name}\n{}\n", hex::encode(key)).as_bytes())?;
    let (tx, rx) = mpsc::channel(16);
    let greeting = Arc::new(Semaphore::new(GREETING));
    tokio::spawn(async move {
        while !tx.is_closed() {
            let Ok(turn) = greeting.clone().acquire_owned().await else { return };
            // The instance the next caller connects to. When Windows will not make one (every
            // instance busy, or short of memory), it is asked again, less and less often.
            let mut pause = Duration::from_millis(50);
            let mut caller = loop {
                match next.take().map_or_else(|| instance(&name, false, most), Ok) {
                    Ok(waiting) => break waiting,
                    Err(_) if tx.is_closed() => return,
                    Err(_) => {
                        tokio::time::sleep(pause).await;
                        pause = (pause * 2).min(Duration::from_secs(5));
                    }
                }
            };
            if caller.connect().await.is_err() {
                let _ = caller.disconnect();
                tokio::time::sleep(Duration::from_millis(50)).await;
                next = Some(caller);
                continue;
            }
            // The next caller's instance is in place before this one is looked at, so the
            // name is never free for somebody else to take. If Windows will not make one now,
            // this caller is seen to all the same, not left waiting on it: its own connection
            // holds the name meanwhile, and the next instance is made once one can be.
            next = instance(&name, false, most).ok();
            let tx = tx.clone();
            tokio::spawn(async move {
                let shown = tokio::time::timeout(WAIT, answer(&mut caller, &key)).await;
                drop(turn);
                if matches!(shown, Ok(Ok(true))) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_names_the_daemon_makes_are_taken() {
        assert!(our_name(&format!("{PREFIX}{:032x}", rand::random::<u128>())));
        let short = format!("{PREFIX}{:031x}", 1);
        let upper = format!("{PREFIX}{:032X}", u128::MAX);
        for bad in [r"\\.\pipe\cahoots-..\..\x", r"\\.\pipe\other", "", PREFIX, &short, &upper] {
            assert!(!our_name(bad), "{bad}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_is_answered_when_every_connection_is_taken() {
        let home = std::env::temp_dir().join(format!("cahoots-test-full-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&home).unwrap();
        crate::os::private_dir(&home).unwrap();
        let listener = listen_to(&home, 3).unwrap();
        // The third takes the last connection there is: no instance can be made for a
        // fourth, and it is answered all the same.
        let mut held = vec![];
        for n in 1..=3 {
            held.push(connect(&home).await.unwrap_or_else(|| panic!("caller {n} was not answered")));
        }
        // One let go makes room for the next.
        drop(listener.accept().await.unwrap());
        held.remove(0);
        assert!(connect(&home).await.is_some(), "a caller after one left");
        drop(held);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_client_reaches_only_a_pipe_of_this_users() {
        let home = std::env::temp_dir().join(format!("cahoots-test-pipe-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&home).unwrap();
        crate::os::private_dir(&home).unwrap();
        let _listener = listen(&home).unwrap();
        let s = connect(&home).await.expect("the daemon's own pipe");
        // Nobody else may open the pipe at all.
        let (me, listed) = (sys::user().unwrap(), sys::user_as_listed().unwrap());
        let dacl = sys::dacl(&s).unwrap();
        assert!(dacl.starts_with("D:P") && dacl.ends_with(&format!("(A;;FA;;;{listed})(A;;FA;;;SY)")), "{dacl}");
        drop(s);
        #[cfg(feature = "gui")]
        {
            let gui = home.clone();
            let s = tokio::task::spawn_blocking(move || connect_blocking(&gui)).await.unwrap();
            assert!(s.is_some_and(|s| sys::served_by_me(&s)));
        }
        // A pipe of somebody else's is not the daemon's, whatever it is called. Here that
        // can only be one made the ordinary way while an administrator: it belongs to the
        // administrators.
        let stranger = format!("{PREFIX}{:032x}", rand::random::<u128>());
        let _theirs = ServerOptions::new().first_pipe_instance(true).create(&stranger).unwrap();
        let client = ClientOptions::new().open(&stranger).unwrap();
        if sys::owner(&client).unwrap() != me {
            assert!(!sys::served_by_me(&client));
        } else {
            println!("skipped: a pipe of somebody else's needs an administrator");
        }
        // A file is not a pipe, whatever it is called.
        assert!(!sys::served_by_me(&std::fs::File::open(pointer(&home)).unwrap()));
        // A pointer that names something other than one of the daemon's pipes is not followed.
        let (name, key) = recorded(&home).unwrap();
        let other = format!("{PREFIX}..\\..\\x\n{}\n", hex::encode(key));
        crate::config::write_private(&pointer(&home), other.as_bytes()).unwrap();
        assert!(recorded(&home).is_none());
        // Nor is one somebody else left in the home folder.
        let ours = format!("{name}\n{}\n", hex::encode(key));
        crate::config::write_private(&pointer(&home), ours.as_bytes()).unwrap();
        assert!(connect(&home).await.is_some());
        let given = std::process::Command::new("icacls").arg(pointer(&home)).args(["/setowner", "*S-1-5-19"]).output();
        if given.unwrap().status.success() {
            assert!(recorded(&home).is_none(), "a pointer another account owns");
            assert!(connect(&home).await.is_none());
        } else {
            println!("skipped: making a file another account's needs an administrator");
        }
        unlisten(&home);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
