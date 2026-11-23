// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use std::{
    fs, io,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::Path,
    process::Command,
};
use tokio::signal::unix::{SignalKind, signal};

pub fn host_name() -> String {
    rustix::system::uname().nodename().to_string_lossy().into_owned()
}

/// Leaves a folder open to its owner alone.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// A new file only its owner can read, for writing; fails if the name is taken.
pub fn create_private(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
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

/// This user's number, as launchd names its services.
pub fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}
