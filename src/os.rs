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

/// The `cahoots` command's own file: this program, or the one beside it when this is the
/// windowed program Windows has for the window. Both then name the same program, so a daemon
/// either of them starts is the same daemon.
pub fn program() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if exe.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("cahootsw")) {
        let beside = exe.with_file_name(format!("cahoots{}", std::env::consts::EXE_SUFFIX));
        if beside.is_file() {
            return Ok(beside);
        }
    }
    Ok(exe)
}

const HOME: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

/// The user's own folder (`~`).
pub fn user_home() -> Option<PathBuf> {
    std::env::var_os(HOME).map(PathBuf::from)
}

/// The same as text, for showing and for filling in `~`.
pub fn home_text() -> Option<String> {
    std::env::var(HOME).ok()
}

/// A typed path with `~` at its start filled in: `~` alone or before a separator, never one
/// inside a name, as a Windows short name has (`ANDYWI~1`).
pub fn expand_home(typed: &str) -> String {
    fill_home(typed, home_text().as_deref())
}

fn fill_home(typed: &str, home: Option<&str>) -> String {
    match (typed.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.chars().next().is_none_or(std::path::is_separator) => {
            format!("{home}{rest}")
        }
        _ => typed.to_string(),
    }
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

impl Recorded {
    /// Whether it was closed to everyone but its owner: no permissions for its group or for
    /// others. A restore keeps it so where permissions are kept another way (Windows).
    pub fn private(&self) -> bool {
        self.mode & 0o077 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::fill_home;

    #[test]
    fn only_a_tilde_at_the_start_is_the_home() {
        let home = Some("/home/me");
        assert_eq!(fill_home("~", home), "/home/me");
        assert_eq!(fill_home("~/x", home), "/home/me/x");
        assert_eq!(fill_home("/a/ANDYWI~1/x", home), "/a/ANDYWI~1/x");
        assert_eq!(fill_home("~other/x", home), "~other/x");
        assert_eq!(fill_home("~/x", None), "~/x");
    }
}
