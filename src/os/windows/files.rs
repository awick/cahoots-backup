// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Files and folders reached through open folders. A folder held open here cannot be renamed,
//! removed or swapped for a link, and neither can a folder above it, so a name looked up
//! below one is looked up in that very folder.

use crate::os::{Kind, Meta, Recorded};
use std::{
    ffi::{OsStr, OsString},
    fs, io,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

// Windows' own numbers for these.
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const FILE_SHARE_DELETE: u32 = 0x4;

fn not_yet<T>() -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "not supported on Windows yet"))
}

fn refused<T>(why: &'static str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, why))
}

/// What an open handle is, from the handle itself.
fn info(f: &fs::File) -> io::Result<Meta> {
    let (i, md) = (winapi_util::file::information(f)?, f.metadata()?);
    let attrs = md.file_attributes();
    // A link or junction stands for another name; any other reparse point is left alone.
    let kind = if md.file_type().is_symlink() {
        Kind::Link
    } else if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Other
    } else if md.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    };
    let mode = match kind {
        Kind::Dir => 0o040755,
        Kind::Link => 0o120777,
        _ if attrs & FILE_ATTRIBUTE_READONLY != 0 => 0o100444,
        _ => 0o100644,
    };
    Ok(Meta {
        kind,
        dev: i.volume_serial_number(),
        ino: i.file_index(),
        nlink: i.number_of_links(),
        mode,
        uid: 0,
        gid: 0,
        // Windows counts time in 100 ns steps from 1601.
        mtime: (md.last_write_time() as i64 - 116_444_736_000_000_000).saturating_mul(100),
        size: md.len(),
    })
}

fn link_flag(follow: bool) -> u32 {
    if follow { 0 } else { FILE_FLAG_OPEN_REPARSE_POINT }
}

/// Opens whatever is at `path` only to ask about it, leaving everyone free to use it meanwhile.
fn look(path: &Path, follow: bool, access: u32) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | link_flag(follow))
        .open(path)
}

/// Whether Windows could read `name` as anything but one plain name in a folder: a path, a
/// drive, a stream of another file, a device, or a name it would quietly trim.
fn plain(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && !stem.ends_with('0'));
    !name.is_empty()
        && !device
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
}

fn stamp(f: &fs::File, r: &Recorded) {
    let ns = Duration::from_nanos(r.mtime.unsigned_abs());
    let t = if r.mtime < 0 { SystemTime::UNIX_EPOCH - ns } else { SystemTime::UNIX_EPOCH + ns };
    let _ = f.set_times(fs::FileTimes::new().set_accessed(t).set_modified(t));
}

fn plain_file(f: fs::File) -> io::Result<fs::File> {
    if info(&f)?.kind != Kind::File {
        return refused("not a plain file");
    }
    Ok(f)
}

/// An open folder.
pub struct Dir {
    path: PathBuf,
    handle: fs::File,
}

impl Dir {
    /// Opens a folder by its path. Without `follow`, a link in the last place is refused.
    pub fn open(path: &Path, follow: bool) -> io::Result<Dir> {
        // Not shared for deleting, which is what holds it and the folders above it in place.
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | link_flag(follow))
            .open(path)?;
        if info(&handle)?.kind != Kind::Dir {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(Dir { path: path.to_path_buf(), handle })
    }

    fn child(&self, name: &OsStr) -> io::Result<PathBuf> {
        match name.to_str() {
            Some(n) if plain(n) => Ok(self.path.join(n)),
            _ => refused("not a name Windows can keep"),
        }
    }

    pub fn try_clone(&self) -> io::Result<Dir> {
        Ok(Dir { path: self.path.clone(), handle: self.handle.try_clone()? })
    }

    /// A folder inside this one; a link, or anything that is not a folder, is refused.
    pub fn sub(&self, name: &OsStr) -> io::Result<Dir> {
        Dir::open(&self.child(name)?, false)
    }

    /// Makes a folder inside this one. It takes this one's permissions.
    pub fn make(&self, name: &OsStr) -> io::Result<()> {
        fs::create_dir(self.child(name)?)
    }

    /// The folder itself.
    pub fn meta(&self) -> io::Result<Meta> {
        info(&self.handle)
    }

    pub fn stat(&self, name: &OsStr, follow: bool) -> io::Result<Meta> {
        info(&look(&self.child(name)?, follow, FILE_READ_ATTRIBUTES)?)
    }

    /// Hands each name in the folder, with the kind the folder itself records for it, to `f`
    /// until it returns false.
    pub fn each_name(&self, mut f: impl FnMut(&OsStr, Kind) -> bool) {
        let Ok(list) = fs::read_dir(&self.path) else { return };
        for e in list {
            let Ok(e) = e else { return };
            let kind = match (e.file_type(), e.metadata()) {
                (Ok(t), _) if t.is_symlink() => Kind::Link,
                (Ok(_), Ok(m)) if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => Kind::Unknown,
                (Ok(t), _) if t.is_dir() => Kind::Dir,
                (Ok(t), _) if t.is_file() => Kind::File,
                _ => Kind::Unknown,
            };
            if !f(&e.file_name(), kind) {
                return;
            }
        }
    }

    /// Every name in the folder, in no order.
    pub fn names(&self) -> io::Result<Vec<OsString>> {
        fs::read_dir(&self.path)?.map(|e| Ok(e?.file_name())).collect()
    }

    pub fn read_link(&self, name: &OsStr) -> io::Result<Vec<u8>> {
        read_link(&self.child(name)?)
    }

    /// Opens a file for reading, never through a link.
    pub fn open_file(&self, name: &OsStr) -> io::Result<fs::File> {
        open_file(&self.child(name)?)
    }

    /// A new file for writing; fails if the name is taken, by a link too.
    pub fn create_file(&self, name: &OsStr) -> io::Result<fs::File> {
        fs::OpenOptions::new().write(true).create_new(true).open(self.child(name)?)
    }

    pub fn rename(&self, from: &OsStr, to: &Dir, name: &OsStr) -> io::Result<()> {
        fs::rename(self.child(from)?, to.child(name)?)
    }

    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        fs::remove_file(self.child(name)?)
    }

    pub fn remove_dir(&self, name: &OsStr) -> io::Result<()> {
        fs::remove_dir(self.child(name)?)
    }

    pub fn symlink(&self, _name: &OsStr, _target: &[u8]) -> io::Result<()> {
        not_yet()
    }

    /// A folder in the user's profile is already closed to other users.
    pub fn set_private(&self) -> io::Result<()> {
        Ok(())
    }

    /// Puts the recorded time back on the folder.
    pub fn finish(&self, r: &Recorded, _root: bool) {
        if let Ok(f) = look(&self.path, false, FILE_WRITE_ATTRIBUTES) {
            stamp(&f, r);
        }
    }

    pub fn finish_existing(&self, r: &Recorded, root: bool) {
        self.finish(r, root)
    }

    pub fn finish_link(&self, _name: &OsStr, _r: &Recorded, _root: bool) {}
}

pub fn stat(path: &Path, follow: bool) -> io::Result<Meta> {
    info(&look(path, follow, FILE_READ_ATTRIBUTES)?)
}

pub fn read_link(path: &Path) -> io::Result<Vec<u8>> {
    Ok(path_bytes(&fs::read_link(path)?))
}

/// Opens a file for reading, never through a link in the last place.
pub fn open_file(path: &Path) -> io::Result<fs::File> {
    plain_file(fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)?)
}

pub fn file_meta(f: &fs::File) -> io::Result<Meta> {
    info(f)
}

/// Puts the recorded time back on a restored file, and makes it read-only if it was.
pub fn finish_file(f: &fs::File, r: &Recorded, _root: bool) {
    stamp(f, r);
    if r.mode & 0o222 == 0
        && let Ok(md) = f.metadata()
    {
        let mut p = md.permissions();
        p.set_readonly(true);
        let _ = f.set_permissions(p);
    }
}

/// Windows has no owner to hand files to.
pub fn is_root() -> bool {
    false
}

/// To be checked against the owner Windows records, when restore is written for it.
pub fn private_to_me(_m: &Meta) -> bool {
    true
}

pub fn seek_hole(_f: &fs::File, _pos: u64) -> io::Result<u64> {
    not_yet()
}

pub fn seek_data(_f: &fs::File, _pos: u64) -> io::Result<Option<u64>> {
    not_yet()
}

/// How much of a file the disk really holds. Not asked yet: all of it is read.
pub fn allocated(f: &fs::File) -> io::Result<u64> {
    Ok(f.metadata()?.len())
}

/// A path as a restore point records it. To be settled when backup is written for Windows.
pub fn path_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

pub fn bytes_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cahoots-test-{tag}-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_folder_is_never_opened_through_a_junction() {
        let dir = scratch("junction");
        fs::create_dir_all(dir.join("outside")).unwrap();
        fs::create_dir_all(dir.join("real")).unwrap();
        fs::write(dir.join("outside/secret"), "s").unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(dir.join("link"))
            .arg(dir.join("outside"))
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let top = Dir::open(&dir, true).unwrap();
        assert!(top.sub("real".as_ref()).is_ok());
        assert!(top.sub("link".as_ref()).is_err());
        assert_eq!(top.stat("link".as_ref(), false).unwrap().kind, Kind::Link);
        assert_eq!(top.stat("link".as_ref(), true).unwrap().kind, Kind::Dir);
        assert!(Dir::open(&dir.join("link"), false).is_err());
        assert!(Dir::open(&dir.join("link"), true).is_ok());
        drop(top);
        fs::remove_dir(dir.join("link")).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_open_folder_stays_where_it_is() {
        let dir = scratch("held");
        fs::create_dir_all(dir.join("a/b")).unwrap();
        let held = Dir::open(&dir.join("a/b"), false).unwrap();
        assert!(fs::rename(dir.join("a/b"), dir.join("a/c")).is_err(), "itself");
        assert!(fs::rename(dir.join("a"), dir.join("z")).is_err(), "the folder above it");
        assert!(fs::remove_dir(dir.join("a/b")).is_err());
        // What is inside can still be written, renamed and removed.
        held.create_file("x".as_ref()).unwrap();
        held.rename("x".as_ref(), &held, "y".as_ref()).unwrap();
        assert_eq!(held.stat("y".as_ref(), false).unwrap().kind, Kind::File);
        held.remove_file("y".as_ref()).unwrap();
        drop(held);
        fs::rename(dir.join("a"), dir.join("z")).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_plain_names_are_taken() {
        for ok in ["a", "0a1b", ".hidden", "name.txt", "com", "com10", "nulls"] {
            assert!(plain(ok), "{ok}");
        }
        for bad in
            ["", ".", "..", "a/b", r"a\b", "C:", "a:b", "nul", "NUL.txt", "com1", "lpt9.x", "a.", "a ", "a?", "a\tb"]
        {
            assert!(!plain(bad), "{bad}");
        }
    }
}
