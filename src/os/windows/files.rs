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
const FILE_ATTRIBUTE_HIDDEN_SYSTEM: u32 = 0x2 | 0x4;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Kept by a cloud service such as OneDrive and not on this disk: reading it would fetch it.
const FILE_ATTRIBUTE_AWAY: u32 = 0x1000 | 0x4_0000 | 0x40_0000;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const FILE_SHARE_DELETE: u32 = 0x4;

fn refused<T>(why: &'static str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, why))
}

/// What an open handle is, from the handle itself.
fn info(f: &fs::File) -> io::Result<Meta> {
    let (i, md) = (winapi_util::file::information(f)?, f.metadata()?);
    let attrs = md.file_attributes();
    // A link or junction stands for another name; any other reparse point is left alone. So
    // are the hidden system junctions Windows keeps for the old names of its own folders
    // (`My Documents`, `Application Data`): nobody made them and nobody can read through them.
    let own_junction = attrs & FILE_ATTRIBUTE_HIDDEN_SYSTEM == FILE_ATTRIBUTE_HIDDEN_SYSTEM;
    let kind = if md.file_type().is_symlink() && !own_junction {
        Kind::Link
    } else if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Other
    } else if md.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    };
    // Windows keeps who may open a file in a list of its own, not in these bits, and a backup
    // does not read that list for every file: what it records is what a file of that kind
    // usually is, open to be read, and a restore gives it the folder's usual permissions.
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
        away: kind == Kind::File && attrs & FILE_ATTRIBUTE_AWAY != 0,
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
        || stem.strip_prefix("COM").or(stem.strip_prefix("LPT")).is_some_and(|n| {
            let mut n = n.chars();
            matches!((n.next(), n.next()), (Some('1'..='9' | '¹' | '²' | '³'), None))
        });
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

    /// Makes a folder inside this one that only this user (and the system) can open, as can
    /// all that is made in it, from the moment it exists.
    pub fn make_private(&self, name: &OsStr) -> io::Result<()> {
        if !self.local() {
            return self.make(name);
        }
        super::sys::create_private_dir(&self.child(name)?)
    }

    /// Whether this folder is on a disk of this computer's own. Elsewhere (a share, a NAS) who
    /// may open what is the server's to say, and it may not know this account: a list naming
    /// this account there can shut out the account that made it. So nothing is made private
    /// there; it takes what the folder passes on.
    fn local(&self) -> bool {
        use std::path::{Component, Prefix};
        let path = std::path::absolute(&self.path).unwrap_or_else(|_| self.path.clone());
        match path.components().next() {
            Some(Component::Prefix(p)) => match p.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => super::sys::local_drive(letter),
                _ => false,
            },
            _ => false,
        }
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

    /// A new file only this user (and the system) can open, for writing; fails if the name is
    /// taken, by a link too. Nobody else can read it while it is written, whatever the folder
    /// passes on: `finish_file` gives it the folder's permissions once it is whole.
    pub fn create_file(&self, name: &OsStr) -> io::Result<fs::File> {
        if !self.local() {
            return self.create_plain(name);
        }
        super::sys::create_private(&self.child(name)?, super::sys::RESTORING, false)
    }

    /// A new file for writing that takes the permissions the folder passes on, as a store's
    /// files do: whoever may open the store, here or on another computer, may open all in it.
    /// Fails if the name is taken, by a link too.
    pub fn create_plain(&self, name: &OsStr) -> io::Result<fs::File> {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(self.child(name)?)
    }

    pub fn rename(&self, from: &OsStr, to: &Dir, name: &OsStr) -> io::Result<()> {
        let (from, to) = (self.child(from)?, to.child(name)?);
        match fs::rename(&from, &to) {
            // Windows will not replace a read-only file, as the others do: it is made writable
            // first. Only a plain file, never what a link there points at.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let md = fs::symlink_metadata(&to)?;
                if !md.is_file() || !md.permissions().readonly() {
                    return Err(e);
                }
                let mut open = md.permissions();
                // On Windows this clears the one read-only flag and opens nothing to anyone.
                #[allow(clippy::permissions_set_readonly_false)]
                open.set_readonly(false);
                look(&to, false, FILE_WRITE_ATTRIBUTES)?.set_permissions(open)?;
                fs::rename(&from, &to)
            }
            done => done,
        }
    }

    /// The same, but never replacing what is there, under any name Windows knows it by: a
    /// short name (`REPORT~1.TXT`) or another spelling of the same name fails too.
    pub fn rename_new(&self, from: &OsStr, to: &Dir, name: &OsStr) -> io::Result<()> {
        super::sys::rename_new(&self.child(from)?, &to.child(name)?)
    }

    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        fs::remove_file(self.child(name)?)
    }

    pub fn remove_dir(&self, name: &OsStr) -> io::Result<()> {
        fs::remove_dir(self.child(name)?)
    }

    /// Makes a link here that will be moved into `into`. Windows wants to know whether it
    /// points at a folder, which a target that does not start at a drive only says read from
    /// the folder the link ends up in. It allows a link at all only to an administrator or
    /// with Developer Mode on.
    pub fn symlink(&self, name: &OsStr, target: &[u8], into: &Dir) -> io::Result<()> {
        let (link, target) = (self.child(name)?, bytes_path(target));
        if into.holds_folder(&target) {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }

    /// Whether a link's target, read from this folder, names a folder. The target is what
    /// whoever made the link wrote, so it is never followed and never looked up as a path:
    /// it is walked one plain name at a time through open folders, from this one or, for one
    /// that starts at a drive (`C:\Users\me`), from the top of that drive, and a link, or
    /// anything else, on the way makes it a file. One that starts at a drive that is not a
    /// disk of this computer (a share given a letter), a share, a device, the top of the
    /// current drive or a drive's current folder (`C:sub`) is taken for a file without looking:
    /// opening `\\host\share` would sign in to that host as this user.
    fn holds_folder(&self, target: &Path) -> bool {
        use std::path::{Component, Prefix};
        let mut parts = target.components().peekable();
        let mut base = self.path.clone();
        if let Some(Component::Prefix(p)) = parts.peek() {
            let Prefix::Disk(letter) = p.kind() else { return false };
            parts.next();
            if parts.next() != Some(Component::RootDir) || !super::sys::local_drive(letter) {
                return false;
            }
            let Some(root) = drive_root(letter) else { return false };
            base = root;
        }
        let (mut names, mut up) = (vec![], 0);
        for c in parts {
            match c {
                Component::Normal(n) => names.push(n),
                Component::CurDir => {}
                // Windows takes `..` away with the name before it, without looking.
                Component::ParentDir if names.pop().is_none() => up += 1,
                Component::ParentDir => {}
                Component::Prefix(_) | Component::RootDir => return false,
            }
        }
        for _ in 0..up {
            if !base.pop() {
                return false;
            }
        }
        let start = if base == self.path { self.try_clone() } else { Dir::open(&base, false) };
        let Ok(mut at) = start else { return false };
        let Some(last) = names.pop() else { return true };
        for n in names {
            match at.sub(n) {
                Ok(d) => at = d,
                Err(_) => return false,
            }
        }
        at.stat(last, false).is_ok_and(|m| m.kind == Kind::Dir)
    }

    /// Makes the folder, and all in it, this user's alone (and the system's). It is the
    /// folder held open that is changed, so a link swapped in for its name cannot turn the
    /// change elsewhere.
    pub fn set_private(&self) -> io::Result<()> {
        self.keep(super::sys::Keep::Mine)
    }

    /// The same for a store, which this computer's administrators may open too (see
    /// `sys::Keep::Store`), and which is taken over from another account where it may be.
    pub fn set_private_store(&self) -> io::Result<()> {
        self.keep(super::sys::Keep::Store)
    }

    fn keep(&self, keep: super::sys::Keep) -> io::Result<()> {
        super::sys::make_private(&self.handle, true, keep, |access| reopen(&self.path, false, &self.handle, access))
    }

    /// Puts the recorded time back on a folder this restore made, and keeps it this user's
    /// alone if it was closed to everyone else (it was made so, then).
    pub fn finish(&self, r: &Recorded, root: bool) {
        if r.private() {
            let _ = self.set_private();
        }
        self.finish_existing(r, root)
    }

    /// A folder that was already there keeps its permissions, and gets only the time.
    pub fn finish_existing(&self, r: &Recorded, _root: bool) {
        if let Ok(f) = look(&self.path, false, FILE_WRITE_ATTRIBUTES) {
            stamp(&f, r);
        }
    }

    pub fn finish_link(&self, _name: &OsStr, _r: &Recorded, _root: bool) {}
}

/// Opens `path` once more, with `access`, and makes sure it is still what `held` has open.
pub(super) fn reopen(path: &Path, follow: bool, held: &fs::File, access: u32) -> io::Result<fs::File> {
    let (f, was) = (look(path, follow, access)?, info(held)?);
    let now = info(&f)?;
    if (now.dev, now.ino) != (was.dev, was.ino) {
        return refused("it was replaced while being changed");
    }
    Ok(f)
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

/// Puts the recorded time back on a restored file, and makes it read-only if it was. Made
/// open to this user alone, it stays so if it was closed to everyone else (as a file from
/// a Unix backup with no permissions for its group or others is), and otherwise takes the
/// permissions of the folder it is in, as any file made there does. A Windows backup
/// records every file as open to be read (see `info`), so its files all take the folder's.
pub fn finish_file(f: &fs::File, r: &Recorded, _root: bool) {
    stamp(f, r);
    if r.mode & 0o222 == 0
        && let Ok(md) = f.metadata()
    {
        let mut p = md.permissions();
        p.set_readonly(true);
        let _ = f.set_permissions(p);
    }
    if !r.private() {
        let _ = super::sys::inherit(f);
    }
}

/// Marks a file being written as one the disk may leave holes in, as a Unix disk does by
/// itself: what is skipped over then takes no room. A disk that cannot (FAT, exFAT) fails,
/// and the file is written whole.
pub fn set_sparse(f: &fs::File) -> io::Result<()> {
    super::sys::set_sparse(f)
}

/// Windows has no owner to hand files to.
pub fn is_root() -> bool {
    false
}

/// Asked of a folder a restore has just made, before it is given a recorded owner and mode.
/// Windows has neither to give, only a time, so there is nothing here to keep from anyone.
pub fn private_to_me(_m: &Meta) -> bool {
    true
}

/// Where the next hole in a file starts, at or after `pos`; the end of the file counts as
/// one. A disk that cannot say (FAT, exFAT) fails, and the file is then read whole.
pub fn seek_hole(f: &fs::File, pos: u64) -> io::Result<u64> {
    let (len, mut at) = (f.metadata()?.len(), pos);
    while at < len {
        match super::sys::first_data(f, at, len)? {
            Some((start, end)) if start == at => at = end,
            _ => return Ok(at),
        }
    }
    Ok(len.max(pos))
}

/// Where data next starts at or after `pos`; None when only a hole is left.
pub fn seek_data(f: &fs::File, pos: u64) -> io::Result<Option<u64>> {
    let len = f.metadata()?.len();
    Ok(super::sys::first_data(f, pos, len)?.map(|(start, _)| start))
}

/// How much of a file the disk really holds.
pub fn allocated(f: &fs::File) -> io::Result<u64> {
    super::sys::allocated(f)
}

/// A path as a restore point records it: `/` between names, as every system writes it, with
/// the drive kept in front (`C:/Users/me`).
pub fn path_bytes(p: &Path) -> Vec<u8> {
    let text = p.to_string_lossy();
    // The long form Windows itself hands out says the same without it.
    let text = match (text.strip_prefix(r"\\?\UNC\"), text.strip_prefix(r"\\?\")) {
        (Some(share), _) => format!(r"\\{share}"),
        (None, Some(rest)) => rest.to_string(),
        (None, None) => text.into_owned(),
    };
    text.replace('\\', "/").into_bytes()
}

pub fn bytes_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).replace('/', "\\"))
}

/// A recorded name as Windows can keep it, and whether it had to be changed: what Windows
/// would refuse or read as something else is spelled out as `%` and its number.
pub fn name(n: &[u8]) -> (OsString, bool) {
    let text = String::from_utf8_lossy(n);
    if plain(&text) {
        let changed = matches!(text, std::borrow::Cow::Owned(_));
        return (text.into_owned().into(), changed);
    }
    let spell = |c: char| format!("%{:02X}", c as u32);
    let mut out = String::new();
    for c in text.chars() {
        if c.is_control() || r#"<>:"/\|?*%"#.contains(c) { out.push_str(&spell(c)) } else { out.push(c) }
    }
    if let Some(last) = out.pop() {
        if last == '.' || last == ' ' { out.push_str(&spell(last)) } else { out.push(last) }
    }
    // A device's name, or no name at all.
    if !plain(&out) {
        out.insert(0, '%');
    }
    (out.into(), true)
}

pub fn drive_root(letter: u8) -> Option<PathBuf> {
    Some(PathBuf::from(format!("{}:\\", letter as char)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cahoots-test-{tag}-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Who may open `path`, as Windows writes it, read without following a link.
    fn dacl(path: &Path) -> String {
        const READ_CONTROL: u32 = 0x2_0000;
        super::super::sys::dacl(&look(path, false, READ_CONTROL).unwrap()).unwrap()
    }

    /// Whether only this user and the system may open `path`; with `own`, by a list of its
    /// own that takes nothing from the folder it is in.
    fn only_mine(path: &Path, own: bool) -> bool {
        let (text, me) = (dacl(path), super::super::sys::user_as_listed().unwrap());
        let (flags, entries) = text.strip_prefix("D:").unwrap().split_once('(').unwrap();
        let who: Vec<&str> =
            entries.split(')').filter_map(|e| e.rsplit(';').next()).filter(|w| !w.is_empty()).collect();
        (!own || flags.contains('P')) && !who.is_empty() && who.iter().all(|w| *w == me || *w == "SY")
    }

    /// Hands `path` to another account, as an administrator can; false where this is not one.
    fn give_away(path: &Path) -> bool {
        let to = |who: &str| {
            std::process::Command::new("icacls").arg(path).args(["/setowner", who]).output().unwrap().status.success()
        };
        // LOCAL SERVICE: an account that is neither this user nor an administrator.
        to("*S-1-5-19")
    }

    /// Past Windows' 260-character limit on a path, as a long name restored into a deep
    /// folder is: the calls in sys.rs must manage it as the standard library's do.
    #[test]
    fn long_paths_are_made_and_moved() {
        let dir = scratch("long");
        let deep = dir.join("d".repeat(120));
        fs::create_dir_all(&deep).unwrap();
        let held = Dir::open(&deep, false).unwrap();
        let (tmp, name) = (OsStr::new(".0a1b2c3d-1.sdb-tmp"), "n".repeat(240));
        assert!(deep.join(&name).as_os_str().len() > 300);
        held.make(OsStr::new("sub")).unwrap();
        drop(held.create_file(tmp).unwrap());
        held.rename_new(tmp, &held, OsStr::new(&name)).unwrap();
        assert!(deep.join(&name).is_file() && deep.join("sub").is_dir());
        drop(held);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_private_folder_is_this_users_alone() {
        let dir = scratch("private");
        fs::create_dir_all(dir.join("old")).unwrap();
        fs::write(dir.join("old/secret"), "s").unwrap();
        assert!(!only_mine(&dir.join("old/secret"), false), "a folder in the temporary folder starts open to others");
        crate::os::private_dir(&dir).unwrap();
        assert!(only_mine(&dir, true), "{}", dacl(&dir));
        // What was in it already, and what is made in it later, is closed too.
        assert!(only_mine(&dir.join("old"), false), "{}", dacl(&dir.join("old")));
        assert!(only_mine(&dir.join("old/secret"), false), "{}", dacl(&dir.join("old/secret")));
        fs::write(dir.join("new"), "n").unwrap();
        assert!(only_mine(&dir.join("new"), false));
        // Asking again is harmless, and a file made private is private by a list of its own.
        crate::os::private_dir(&dir).unwrap();
        let key = scratch("key").join("identity.key");
        crate::os::create_private(&key).unwrap();
        assert!(only_mine(&key, true), "{}", dacl(&key));
        assert!(crate::os::create_private(&key).is_err(), "the name is taken");
        fs::remove_dir_all(key.parent().unwrap()).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_store_folder_is_made_private_through_its_handle() {
        let dir = scratch("store");
        fs::create_dir_all(dir.join("friend/chunks")).unwrap();
        fs::write(dir.join("friend/chunks/c"), "c").unwrap();
        let held = Dir::open(&dir, false).unwrap();
        held.set_private().unwrap();
        held.set_private().unwrap();
        assert!(only_mine(&dir, true), "{}", dacl(&dir));
        assert!(only_mine(&dir.join("friend/chunks/c"), false), "{}", dacl(&dir.join("friend/chunks/c")));
        // The folder is still held, and still works.
        held.create_file("x".as_ref()).unwrap();
        assert!(only_mine(&dir.join("x"), false));
        assert!(fs::rename(&dir, dir.with_extension("moved")).is_err());
        drop(held);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Whether `path` has a list of its own, and who it lets in.
    fn who(path: &Path) -> (bool, Vec<String>) {
        let text = dacl(path);
        let (flags, entries) = text.strip_prefix("D:").unwrap().split_once('(').unwrap();
        let who = entries.split(')').filter_map(|e| e.rsplit(';').next()).filter(|w| !w.is_empty());
        (flags.contains('P'), who.map(str::to_string).collect())
    }

    #[test]
    fn a_store_is_open_to_administrators_and_its_files_take_its_list() {
        let dir = scratch("store-admins");
        fs::create_dir_all(dir.join("friend")).unwrap();
        let held = Dir::open(&dir, false).unwrap();
        held.set_private_store().unwrap();
        held.set_private_store().unwrap();
        let me = super::super::sys::user().unwrap().to_string();
        let listed = super::super::sys::user_as_listed().unwrap();
        assert_eq!(who(&dir), (true, vec![listed.clone(), "SY".into(), "BA".into()]), "{}", dacl(&dir));
        // A chunk file has no list of its own: whoever may open the store may open it.
        drop(held.sub("friend".as_ref()).unwrap().create_plain("c".as_ref()).unwrap());
        assert_eq!(who(&dir.join("friend/c")), (false, vec![listed, "SY".into(), "BA".into()]));
        assert!(held.sub("friend".as_ref()).unwrap().create_plain("c".as_ref()).is_err(), "the name is taken");
        drop(held);
        // Another account's, which this one has full control of (as one is once given it, see
        // `regain`): taken over, and so this account's alone (and the administrators') again.
        if give_away(&dir) {
            Dir::open(&dir, false).unwrap().set_private_store().unwrap();
            let owner = super::super::sys::owner(&look(&dir, false, 0x2_0000).unwrap()).unwrap();
            assert_eq!(owner, me);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_store_another_account_keeps_is_used_all_the_same() {
        let dir = scratch("store-theirs");
        let me = super::super::sys::user().unwrap();
        let icacls = |args: &[&str]| std::process::Command::new("icacls").arg(&dir).args(args).output().unwrap();
        // Another account's, which this one may use but not close, unless it may take it over
        // (as an administrator with the right to take what is anyone's, as over ssh, is).
        let usable = format!("*{me}:(OI)(CI)M");
        if !give_away(&dir) || !icacls(&["/inheritance:r", "/grant:r", &usable]).status.success() {
            println!("skipped: making a folder another account's needs an administrator");
            return fs::remove_dir_all(dir).unwrap();
        }
        let s = crate::store::Store::new(dir.clone(), 0).unwrap();
        let owner = || super::super::sys::owner(&look(&dir, false, 0x2_0000).unwrap()).unwrap();
        match &s.warning {
            Some(w) => assert!(w.contains("another account") && w.contains("used as it is"), "{w}"),
            None => assert_eq!(owner(), me, "taken over"),
        }
        s.put_chunk(&"a".repeat(64), &[1; 32], b"sealed", 1 << 20).unwrap();
        assert_eq!(s.get_chunk(&"a".repeat(64), &[1; 32]).unwrap(), b"sealed");
        // One this account may not open at all says how to get it back.
        give_away(&dir);
        assert!(icacls(&["/inheritance:r", "/grant:r", &format!("*{me}:(D)")]).status.success());
        match crate::store::Store::new(dir.clone(), 0) {
            Err(e) => assert!(format!("{e:#}").contains(&format!("icacls \"{}\" /grant *{me}:", dir.display()))),
            Ok(_) => println!("skipped: this account may open anything (the right to back up all files)"),
        }
        assert!(icacls(&["/grant:r", &format!("*{me}:(OI)(CI)F"), "/t"]).status.success());
        drop(s);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_folder_another_account_owns_is_refused() {
        let dir = scratch("theirs");
        if !give_away(&dir) {
            println!("skipped: making a folder another account's needs an administrator");
            return fs::remove_dir_all(dir).unwrap();
        }
        let e = crate::os::private_dir(&dir).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        assert!(e.to_string().contains("another account"), "{e}");
        assert!(Dir::open(&dir, false).unwrap().set_private().is_err());
        assert!(!only_mine(&dir, true), "nothing was changed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_restored_file_is_this_users_alone_while_written_and_after_if_it_was() {
        let dir = scratch("restored");
        let held = Dir::open(&dir, false).unwrap();
        // As a file made here in the usual way is.
        fs::write(dir.join("usual"), "u").unwrap();
        let r = |mode| Recorded { mode, uid: 0, gid: 0, mtime: 0 };
        for (name, mode) in [("open", 0o100644), ("shut", 0o100600), ("shut-to-others", 0o100640)] {
            let f = held.create_file(name.as_ref()).unwrap();
            assert!(only_mine(&dir.join(name), true), "{name} while written: {}", dacl(&dir.join(name)));
            finish_file(&f, &r(mode), false);
        }
        // The same entries; Windows marks a list it has worked out itself (`AI`).
        let entries = |name: &str| dacl(&dir.join(name)).split_once('(').unwrap().1.to_string();
        assert_eq!(entries("open"), entries("usual"));
        assert!(!only_mine(&dir.join("open"), false), "{}", dacl(&dir.join("open")));
        assert!(only_mine(&dir.join("shut"), true), "{}", dacl(&dir.join("shut")));
        assert_eq!(entries("shut-to-others"), entries("usual"));
        // A folder that was private is made so, with what is put in it; any other takes this
        // one's permissions.
        held.make("usual-dir".as_ref()).unwrap();
        held.make_private("shut-dir".as_ref()).unwrap();
        assert!(only_mine(&dir.join("shut-dir"), true), "{}", dacl(&dir.join("shut-dir")));
        assert!(!only_mine(&dir.join("usual-dir"), false));
        let inside = held.sub("shut-dir".as_ref()).unwrap().create_file("f".as_ref()).unwrap();
        finish_file(&inside, &r(0o100644), false);
        assert!(only_mine(&dir.join("shut-dir/f"), false), "{}", dacl(&dir.join("shut-dir/f")));
        held.sub("usual-dir".as_ref()).unwrap().finish(&r(0o040700), false);
        assert!(only_mine(&dir.join("usual-dir"), true), "{}", dacl(&dir.join("usual-dir")));
        assert!(held.make_private("shut-dir".as_ref()).is_err(), "the name is taken");
        // A folder that passes nothing on (its list is for itself alone) must not leave what
        // is restored into it open to nobody.
        held.make("bare".as_ref()).unwrap();
        let me = super::super::sys::user().unwrap();
        let alone = std::process::Command::new("icacls")
            .arg(dir.join("bare"))
            .args(["/inheritance:r", "/grant:r", &format!("*{me}:F")])
            .output()
            .unwrap();
        assert!(alone.status.success());
        let bare = held.sub("bare".as_ref()).unwrap().create_file("f".as_ref()).unwrap();
        finish_file(&bare, &r(0o100644), false);
        assert!(only_mine(&dir.join("bare/f"), true), "{}", dacl(&dir.join("bare/f")));
        // This folder is on a disk of this computer's own; one reached as a share is not.
        assert!(held.local());
        drop((inside, bare, held));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_link_target_is_never_looked_up() {
        let dir = scratch("targets");
        fs::create_dir_all(dir.join("sub/inner")).unwrap();
        fs::write(dir.join("file"), "f").unwrap();
        let here = Dir::open(&dir, false).unwrap();
        let folder = |t: &str| here.holds_folder(&bytes_path(t.as_bytes()));
        for yes in ["sub", "sub/inner", "./sub/", "sub/../sub", "."] {
            assert!(folder(yes), "{yes}");
        }
        let top = dir.file_name().unwrap().to_str().unwrap();
        assert!(folder(&format!("../{top}/sub")));
        // Named from the top of a drive of this computer, walked from there the same way.
        let windows = std::env::var("SystemRoot").unwrap();
        let whole = dir.join("sub").to_str().unwrap().to_string();
        for yes in [&windows, &windows.replace('\\', "/"), &whole, &format!("{whole}\\..\\sub\\inner")] {
            assert!(folder(yes), "{yes}");
        }
        assert!(!folder(&dir.join("file").to_string_lossy()));
        // Folders, but named from a share, a device, the top of the current drive or a drive's
        // current folder: taken for files unlooked at.
        let verbatim = format!(r"\\?\{windows}");
        for no in ["file", "none", "sub/none", "/Windows", "//./pipe/x", "//host/share/x", "C:sub", "nul", &verbatim] {
            assert!(!folder(no), "{no}");
        }
        // Nor is a drive that is not a disk of this computer looked at.
        assert!(super::super::sys::local_drive(windows.as_bytes()[0]));
        assert!(!super::super::sys::local_drive(b'?'));
        drop(here);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_link_to_a_folder_named_from_its_drive_comes_back_a_folder_link() {
        use std::os::windows::fs::FileTypeExt;
        let dir = scratch("drive-link");
        fs::create_dir_all(dir.join("sub")).unwrap();
        let here = Dir::open(&dir, false).unwrap();
        match here.symlink("l".as_ref(), dir.join("sub").to_str().unwrap().as_bytes(), &here) {
            Err(e) => println!("skipped: making links needs Developer Mode or an administrator ({e})"),
            Ok(()) => assert!(fs::symlink_metadata(dir.join("l")).unwrap().file_type().is_symlink_dir()),
        }
        // One made elsewhere to be moved here, as a restore does, is read from here too.
        fs::create_dir_all(dir.join("tmp")).unwrap();
        let tmp = here.sub("tmp".as_ref()).unwrap();
        if tmp.symlink("r".as_ref(), b"sub", &here).is_ok() {
            assert!(fs::symlink_metadata(dir.join("tmp/r")).unwrap().file_type().is_symlink_dir());
            assert!(tmp.symlink("f".as_ref(), b"tmp", &here).is_ok());
            assert!(fs::symlink_metadata(dir.join("tmp/f")).unwrap().file_type().is_symlink_dir());
            assert!(tmp.symlink("n".as_ref(), b"none", &here).is_ok());
            assert!(fs::symlink_metadata(dir.join("tmp/n")).unwrap().file_type().is_symlink_file());
        }
        drop((tmp, here));
        let _ = fs::remove_dir(dir.join("l"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn restoring_a_link_does_not_open_its_target() {
        use tokio::net::windows::named_pipe::ServerOptions;
        let dir = scratch("link");
        let pipe = format!(r"\\.\pipe\cahoots-test-{:032x}", rand::random::<u128>());
        let server = ServerOptions::new().first_pipe_instance(true).create(&pipe).unwrap();
        let here = Dir::open(&dir, false).unwrap();
        let target = pipe.replace('\\', "/");
        // Making it may fail, without the right to make links; it must not look either way.
        let _ =
            tokio::task::spawn_blocking(move || here.symlink("l".as_ref(), target.as_bytes(), &here)).await.unwrap();
        let opened = tokio::time::timeout(Duration::from_millis(200), server.connect()).await;
        assert!(opened.is_err(), "the link's target was opened");
        let _ = fs::remove_file(dir.join("l"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn holes_are_found_where_the_disk_has_none() {
        use std::io::{Seek, SeekFrom, Write};
        let dir = scratch("sparse");
        let path = dir.join("sparse");
        let mut f = fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
        let (len, at) = (64u64 << 20, 32u64 << 20);
        super::super::sys::set_sparse(&f).unwrap();
        f.write_all(&[1; 4096]).unwrap();
        f.seek(SeekFrom::Start(at)).unwrap();
        f.write_all(&[2; 4096]).unwrap();
        f.set_len(len).unwrap();
        let f = open_file(&path).unwrap();
        assert!(allocated(&f).unwrap() < 1 << 20, "{}", allocated(&f).unwrap());
        let hole = seek_hole(&f, 0).unwrap();
        assert!((4096..1 << 20).contains(&hole), "{hole}");
        assert_eq!(seek_hole(&f, hole).unwrap(), hole);
        let data = seek_data(&f, hole).unwrap().unwrap();
        assert!((at - (1 << 20)..=at).contains(&data), "{data}");
        let end = seek_hole(&f, data).unwrap();
        assert!((at + 4096..len).contains(&end), "{end}");
        assert_eq!(seek_data(&f, end).unwrap(), None);
        // A file with no holes is all data, its end the only hole.
        fs::write(dir.join("plain"), [3; 10000]).unwrap();
        let plain = open_file(&dir.join("plain")).unwrap();
        assert!(allocated(&plain).unwrap() >= 10000);
        assert_eq!((seek_data(&plain, 0).unwrap(), seek_hole(&plain, 0).unwrap()), (Some(0), 10000));
        drop((f, plain));
        fs::remove_dir_all(dir).unwrap();
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
        // One marked the way Windows marks its own is not offered as a link to back up.
        let attrib = |args: &[&str], path: &Path| {
            assert!(std::process::Command::new("attrib").args(args).arg(path).status().unwrap().success());
        };
        attrib(&["+s", "+h", "/l"], &dir.join("link"));
        assert_eq!(top.stat("link".as_ref(), false).unwrap().kind, Kind::Other);
        assert!(top.sub("link".as_ref()).is_err());
        attrib(&["-s", "-h", "/l"], &dir.join("link"));
        // A file a cloud service has taken off the disk is still a file, and says so.
        fs::write(dir.join("real/cloud.txt"), "x").unwrap();
        assert!(!top.sub("real".as_ref()).unwrap().stat("cloud.txt".as_ref(), false).unwrap().away);
        attrib(&["+o"], &dir.join("real/cloud.txt"));
        let cloud = top.sub("real".as_ref()).unwrap().stat("cloud.txt".as_ref(), false).unwrap();
        assert!(cloud.away && cloud.kind == Kind::File);
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
        // A read-only file is replaced like any other.
        let mut shut = fs::metadata(dir.join("a/b/y")).unwrap().permissions();
        shut.set_readonly(true);
        fs::set_permissions(dir.join("a/b/y"), shut).unwrap();
        fs::write(dir.join("a/b/new"), "new").unwrap();
        held.rename("new".as_ref(), &held, "y".as_ref()).unwrap();
        assert_eq!(fs::read_to_string(dir.join("a/b/y")).unwrap(), "new");
        held.remove_file("y".as_ref()).unwrap();
        drop(held);
        fs::rename(dir.join("a"), dir.join("z")).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recorded_paths_and_names_cross_over() {
        assert_eq!(path_bytes(Path::new(r"C:\Users\me\a.txt")), b"C:/Users/me/a.txt");
        assert_eq!(path_bytes(Path::new(r"\\?\C:\Users\me")), b"C:/Users/me");
        assert_eq!(path_bytes(Path::new(r"\\?\UNC\nas\share\x")), b"//nas/share/x");
        assert_eq!(bytes_path(b"C:/Users/me/a.txt"), Path::new(r"C:\Users\me\a.txt"));
        let name = |n: &str| name(n.as_bytes());
        assert_eq!(name("notes.txt"), ("notes.txt".into(), false));
        assert_eq!(name("50% done"), ("50% done".into(), false));
        assert_eq!(name("a:b"), ("a%3Ab".into(), true));
        assert_eq!(name(r"back\slash"), ("back%5Cslash".into(), true));
        assert_eq!(name("what?"), ("what%3F".into(), true));
        assert_eq!(name("ends."), ("ends%2E".into(), true));
        assert_eq!(name("NUL"), ("%NUL".into(), true));
        assert_eq!(name("com1.txt"), ("%com1.txt".into(), true));
        assert!(plain(name("a<b>|c*\t ").0.to_str().unwrap()));
    }

    #[test]
    fn only_plain_names_are_taken() {
        for ok in ["a", "0a1b", ".hidden", "name.txt", "com", "com0", "com10", "comp", "Come.docx", "lpts", "nulls"] {
            assert!(plain(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            r"a\b",
            "C:",
            "a:b",
            "nul",
            "NUL.txt",
            "com1",
            "lpt9.x",
            "COM².txt",
            "a.",
            "a ",
            "a?",
            "a\tb",
        ] {
            assert!(!plain(bad), "{bad}");
        }
    }
}
