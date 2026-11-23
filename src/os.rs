// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! What differs between systems, behind one set of names.

use std::path::PathBuf;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

const HOME: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

/// The user's own folder (`~`).
pub fn user_home() -> Option<PathBuf> {
    std::env::var_os(HOME).map(PathBuf::from)
}

/// The same as text, for showing and for filling in `~`.
pub fn home_text() -> Option<String> {
    std::env::var(HOME).ok()
}

/// What a name in a folder is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
    Link,
    /// A pipe, device or socket.
    Other,
    /// The folder does not say; ask with `stat`.
    Unknown,
}

/// What the system records about a file. `mode` is the whole of it, type bits included, and
/// `mtime` is in nanoseconds.
#[derive(Clone, Debug)]
pub struct Meta {
    pub kind: Kind,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub size: u64,
    /// Kept by a cloud service and not on this disk: reading it would fetch it.
    pub away: bool,
}

/// What a restore point recorded about a file, to be put back on it.
pub struct Recorded {
    pub mode: u32,
    // Windows has no owner to put back.
    #[cfg_attr(windows, allow(dead_code))]
    pub uid: u32,
    #[cfg_attr(windows, allow(dead_code))]
    pub gid: u32,
    pub mtime: i64,
}
