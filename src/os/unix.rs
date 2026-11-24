// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use std::{
    fs, io,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::Path,
    process::Command,
};
use tokio::signal::unix::{SignalKind, signal};

pub fn host_name() -> String {
    rustix::system::uname().nodename().to_string_lossy().into_owned()
}

/// Leaves a folder open to its owner alone. One on a disk that keeps no permissions of its
/// own (FAT, a share, as some systems mount them) is left as it is, if it is this user's, as
/// there is nothing to set; one another user owns is refused (PermissionDenied).
pub fn private_dir(dir: &Path) -> io::Result<()> {
    use rustix::io::Errno;
    match fs::set_permissions(dir, fs::Permissions::from_mode(0o700)) {
        Err(e)
            if [Errno::PERM, Errno::NOTSUP, Errno::ROFS].iter().any(|c| e.raw_os_error() == Some(c.raw_os_error()))
                && fs::metadata(dir).is_ok_and(|m| m.uid() == rustix::process::geteuid().as_raw()) =>
        {
            Ok(())
        }
        done => done,
    }
}

/// What to do about a store folder this account may not open, as one is that another
/// account (perhaps on another computer) used before.
pub fn regain(dir: &Path) -> String {
    format!(
        "this account may not open it, as it belongs to another (perhaps the one that used it on another \
         computer). To use it here, run `sudo chown -R {} \"{}\"`, then try again",
        uid(),
        dir.display()
    )
}

/// A new file only its owner can read, for writing; fails if the name is taken.
pub fn create_private(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

/// A new file that is carried to another computer (a recovery kit); fails if the name is taken.
pub fn create_carried(path: &Path) -> io::Result<fs::File> {
    create_private(path)
}

/// Sets a command to run apart from this terminal, so closing it does not stop the command.
pub fn detached(cmd: &mut Command) -> &mut Command {
    cmd.process_group(0)
}

/// Done when the system or the terminal asks the program to stop.
pub fn stop_signal() -> io::Result<impl Future<Output = ()>> {
    let mut term = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    })
}

pub mod control;

mod files;
pub use files::*;

/// Where programs keep what belongs to this user, on a system that has such a place.
pub fn app_data() -> Option<std::path::PathBuf> {
    None
}

/// This user's number, as launchd names its services.
pub fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Where plugged-in drives show up, to start a drive's path from.
pub const DRIVES: &str = "/Volumes/";

/// A path with every link in it followed.
pub fn real_path(p: impl AsRef<std::path::Path>) -> io::Result<std::path::PathBuf> {
    fs::canonicalize(p)
}
