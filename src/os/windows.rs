// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::os::Kind;
use std::{
    fs, io,
    os::windows::{fs::OpenOptionsExt, process::CommandExt},
    path::Path,
    process::Command,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, READ_CONTROL,
};

pub fn host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// Makes a folder this user's alone (and the system's), with all that is in it now and all
/// that is made in it later. A folder someone else owns is refused: whoever owns it could
/// open it to themselves again at any time. Asking again of a folder already private
/// changes nothing.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    let open = |follow: bool| {
        fs::OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT })
            .open(dir)
    };
    let refused = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", dir.display()));
    let (mut f, mut follow) = (open(false)?, false);
    if file_meta(&f)?.kind == Kind::Link {
        // A link made to keep the folder somewhere else must be this user's too, or its
        // owner could point it at another folder later.
        if !sys::owned_by_me(&f)? {
            return Err(refused(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the link belongs to another account",
            )));
        }
        (f, follow) = (open(true)?, true);
    }
    if file_meta(&f)?.kind != Kind::Dir {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    sys::make_private(&f, true, sys::Keep::Mine, |access| files::reopen(dir, follow, &f, access)).map_err(refused)
}

/// A new file only this user (and the system) can open, for writing; fails if the name is
/// taken. Nobody else can open it even for the moment before it is written.
pub fn create_private(path: &Path) -> io::Result<fs::File> {
    sys::create_private(path, windows_sys::Win32::Foundation::GENERIC_WRITE, true)
}

/// Whether this program has nowhere to print, as the windowed one started at login has not.
#[cfg(feature = "windowed")]
pub fn no_output() -> bool {
    use std::os::windows::io::AsRawHandle;
    let out = io::stdout().as_raw_handle();
    out.is_null() || out as isize == -1
}

/// A new file that is carried to another computer (a recovery kit); fails if the name is
/// taken. It takes the permissions of the folder it is in: a list naming this account would
/// shut it to the account that needs it on the next computer, and it is sealed anyway.
pub fn create_carried(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create_new(true).open(path)
}

/// What to do about a store folder this account may not open, as one is that was kept
/// private on another computer: give this account full control of it, as administrator (who
/// may open it, see `sys::Keep::Store`). Its next use then makes it this account's own.
pub fn regain(dir: &Path) -> String {
    let me = sys::user().unwrap_or("%USERNAME%");
    format!(
        "this account may not open it, as it was kept private to the account that used it before (on another \
         computer, perhaps). To use it here, run this once in a terminal opened as administrator, then try again: \
         icacls \"{}\" /grant *{me}:(OI)(CI)F /t",
        dir.display()
    )
}

/// Sets a command to run with no console of its own, apart from this one, and with none of
/// this program's own input or output.
pub fn detached(cmd: &mut Command) -> &mut Command {
    sys::keep_stdio();
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

// Every call into Windows that the standard library has no safe form of, and the only
// unchecked code in cahoots.
#[allow(unsafe_code)]
mod sys;

pub fn uid() -> u32 {
    0
}

/// Where programs keep what belongs to this user on this computer.
pub fn app_data() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from)
}

/// Where plugged-in drives show up, to start a drive's path from.
pub const DRIVES: &str = "D:\\";

/// A path with every link in it followed, in the form people write: Windows' own answer
/// starts with `\\?\`, which says the same.
pub fn real_path(p: impl AsRef<Path>) -> io::Result<std::path::PathBuf> {
    let real = fs::canonicalize(p)?;
    let text = real.to_string_lossy();
    Ok(match (text.strip_prefix(r"\\?\UNC\"), text.strip_prefix(r"\\?\")) {
        (Some(share), _) => format!(r"\\{share}").into(),
        (None, Some(rest)) => rest.into(),
        (None, None) => real.clone(),
    })
}
