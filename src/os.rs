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
