// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use std::{fs, io, os::windows::process::CommandExt, path::Path, process::Command};

pub fn host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// A folder in the user's profile is already closed to other users.
pub fn private_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// A new file for writing; fails if the name is taken.
pub fn create_private(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create_new(true).open(path)
}

/// Sets a command to run with no console of its own, apart from this one.
pub fn detached(cmd: &mut Command) -> &mut Command {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
}

/// Done when the user or the system asks the program to stop.
pub fn stop_signal() -> io::Result<impl Future<Output = ()>> {
    use tokio::signal::windows::{ctrl_close, ctrl_shutdown};
    let (mut close, mut shutdown) = (ctrl_close()?, ctrl_shutdown()?);
    Ok(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = close.recv() => {}
            _ = shutdown.recv() => {}
        }
    })
}

pub mod control;

mod files;
pub use files::*;

pub fn uid() -> u32 {
    0
}
