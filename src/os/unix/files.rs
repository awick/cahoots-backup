// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Files and folders reached through open folders, so that a link planted or swapped in
//! along the way is refused rather than followed.

use crate::os::{Kind, Meta, Recorded};
use rustix::{
    fs::{self as rfs, AtFlags, CWD, FileType, Mode, OFlags, Stat, Timespec, Timestamps},
    io::Errno,
    process::{Gid, Uid},
};
use std::{
    ffi::{OsStr, OsString},
    fs, io,
    os::{
        fd::{AsFd, OwnedFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::MetadataExt,
        },
    },
    path::{Path, PathBuf},
};

const FOLDER: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY).union(OFlags::NONBLOCK).union(OFlags::CLOEXEC);
/// Opening a file never follows a link and never waits on a pipe.
const READ: OFlags = OFlags::RDONLY.union(OFlags::NOFOLLOW).union(OFlags::NONBLOCK).union(OFlags::CLOEXEC);
/// A brand new file, never one (or a link) that was waiting under the name.
const NEW: OFlags =
    OFlags::WRONLY.union(OFlags::CREATE).union(OFlags::EXCL).union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);

fn kind(ft: FileType) -> Kind {
    match ft {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Link,
        FileType::Unknown => Kind::Unknown,
        _ => Kind::Other,
    }
}

#[allow(clippy::unnecessary_cast)]
fn meta(st: &Stat) -> Meta {
    Meta {
        kind: kind(FileType::from_raw_mode(st.st_mode)),
        dev: st.st_dev as u64,
        ino: st.st_ino as u64,
        nlink: st.st_nlink as u64,
        mode: st.st_mode as u32,
        uid: st.st_uid as u32,
        gid: st.st_gid as u32,
        mtime: st.st_mtime as i64 * 1_000_000_000 + st.st_mtime_nsec as i64,
        size: st.st_size as u64,
        away: false,
    }
}

fn at(follow: bool) -> AtFlags {
    if follow { AtFlags::empty() } else { AtFlags::SYMLINK_NOFOLLOW }
}

fn stamps(r: &Recorded) -> Timestamps {
    let t = Timespec { tv_sec: r.mtime.div_euclid(1_000_000_000), tv_nsec: r.mtime.rem_euclid(1_000_000_000) as _ };
    Timestamps { last_access: t, last_modification: t }
}

/// Applies the recorded owner, permissions and time to an open file or folder. Set-id bits
/// only survive a restore run by root, which also restores the owner they belong to.
fn finish(fd: &impl AsFd, r: &Recorded, root: bool) {
    // Kept only once the file really belongs to that owner, never on one left with root's.
    let owned = root && rfs::fchown(fd, Some(Uid::from_raw(r.uid)), Some(Gid::from_raw(r.gid))).is_ok();
    let keep = if owned { 0o7777 } else { 0o777 };
    let _ = rfs::fchmod(fd, Mode::from_raw_mode((r.mode & keep) as _));
    let _ = rfs::futimens(fd, &stamps(r));
}

/// An open folder.
pub struct Dir(OwnedFd);

impl Dir {
    /// Opens a folder by its path. Without `follow`, a link in the last place is refused.
    pub fn open(path: &Path, follow: bool) -> io::Result<Dir> {
        let flags = if follow { FOLDER } else { FOLDER | OFlags::NOFOLLOW };
        Ok(Dir(rfs::open(path, flags, Mode::empty())?))
    }

    pub fn try_clone(&self) -> io::Result<Dir> {
        Ok(Dir(self.0.try_clone()?))
    }

    /// A folder inside this one; a link, or anything that is not a folder, is refused.
    pub fn sub(&self, name: &OsStr) -> io::Result<Dir> {
        Ok(Dir(rfs::openat(&self.0, name, FOLDER | OFlags::NOFOLLOW, Mode::empty())?))
    }

    /// Makes a folder inside this one that only its owner can use.
    pub fn make(&self, name: &OsStr) -> io::Result<()> {
        Ok(rfs::mkdirat(&self.0, name, Mode::RWXU)?)
    }

    /// The same: every folder made here is its owner's alone until it is finished.
    pub fn make_private(&self, name: &OsStr) -> io::Result<()> {
        self.make(name)
    }

    /// The folder itself.
    pub fn meta(&self) -> io::Result<Meta> {
        Ok(meta(&rfs::fstat(&self.0)?))
    }

    pub fn stat(&self, name: &OsStr, follow: bool) -> io::Result<Meta> {
        Ok(meta(&rfs::statat(&self.0, name, at(follow))?))
    }

    /// Hands each name in the folder, with the kind the folder itself records for it, to `f`
    /// until it returns false.
    pub fn each_name(&self, mut f: impl FnMut(&OsStr, Kind) -> bool) {
        let Ok(mut d) = rfs::Dir::read_from(&self.0) else { return };
        while let Some(Ok(e)) = d.read() {
            if !f(OsStr::from_bytes(e.file_name().to_bytes()), kind(e.file_type())) {
                return;
            }
        }
    }

    /// Every name in the folder, in no order.
    pub fn names(&self) -> io::Result<Vec<OsString>> {
        let mut names = vec![];
        for d in rfs::Dir::read_from(&self.0)? {
            let d = d?;
            let name = d.file_name().to_bytes();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name.to_vec()));
            }
        }
        Ok(names)
    }

    pub fn read_link(&self, name: &OsStr) -> io::Result<Vec<u8>> {
        Ok(rfs::readlinkat(&self.0, name, vec![])?.into_bytes())
    }

    /// Opens a file for reading, never through a link and never waiting on a pipe.
    pub fn open_file(&self, name: &OsStr) -> io::Result<fs::File> {
        Ok(rfs::openat(&self.0, name, READ, Mode::empty())?.into())
    }

    /// A new file only its owner can read, for writing; fails if the name is taken.
    pub fn create_file(&self, name: &OsStr) -> io::Result<fs::File> {
        Ok(rfs::openat(&self.0, name, NEW, Mode::RUSR | Mode::WUSR)?.into())
    }

    /// The same, for a store's files: here a file's own mode is all there is.
    pub fn create_plain(&self, name: &OsStr) -> io::Result<fs::File> {
        self.create_file(name)
    }

    pub fn rename(&self, from: &OsStr, to: &Dir, name: &OsStr) -> io::Result<()> {
        Ok(rfs::renameat(&self.0, from, &to.0, name)?)
    }

    /// The same, but never replacing what is there, under its own name or any other the disk
    /// counts as the same (`README` for `readme`, on a disk where case does not count).
    pub fn rename_new(&self, from: &OsStr, to: &Dir, name: &OsStr) -> io::Result<()> {
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
        match rfs::renameat_with(&self.0, from, &to.0, name, rfs::RenameFlags::NOREPLACE) {
            // A disk (or a system) that cannot be asked to: looked at just before instead.
            Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {}
            done => return Ok(done?),
        }
        if rfs::statat(&to.0, name, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        self.rename(from, to, name)
    }

    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        Ok(rfs::unlinkat(&self.0, name, AtFlags::empty())?)
    }

    pub fn remove_dir(&self, name: &OsStr) -> io::Result<()> {
        Ok(rfs::unlinkat(&self.0, name, AtFlags::REMOVEDIR)?)
    }

    /// Makes a link here that will be moved into `_into`, the folder its target is read from.
    pub fn symlink(&self, name: &OsStr, target: &[u8], _into: &Dir) -> io::Result<()> {
        Ok(rfs::symlinkat(OsStr::from_bytes(target), &self.0, name)?)
    }

    /// Leaves the folder open to its owner alone. One on a disk that keeps no permissions of
    /// its own (FAT, as Linux mounts it) is left as it is, if it is this user's, as there is
    /// nothing to set; one another user owns is refused.
    pub fn set_private(&self) -> io::Result<()> {
        match rfs::fchmod(&self.0, Mode::RWXU) {
            Err(Errno::PERM | Errno::NOTSUP | Errno::ROFS)
                if rfs::fstat(&self.0).is_ok_and(|st| st.st_uid == rustix::process::geteuid().as_raw()) =>
            {
                Ok(())
            }
            done => Ok(done?),
        }
    }

    /// The same for a store: on Unix nobody but its owner (and root) opens a private folder.
    pub fn set_private_store(&self) -> io::Result<()> {
        self.set_private()
    }

    /// Puts what was recorded back on a folder this restore made.
    pub fn finish(&self, r: &Recorded, root: bool) {
        finish(&self.0, r, root)
    }

    /// A folder that was already there keeps its owner and any set-id bits; it gets the recorded
    /// permissions and time only if it already belongs to the recorded owner (or, not as root,
    /// to us, as fchmod allows nothing else).
    pub fn finish_existing(&self, r: &Recorded, root: bool) {
        let Ok(st) = rfs::fstat(&self.0) else { return };
        // Owned by root and private, exactly as a restore makes its folders: only root could have
        // made it, so it is an earlier, stopped restore's own and is finished as one made now.
        if root && st.st_uid == 0 && st.st_mode & 0o7777 == 0o700 {
            return finish(&self.0, r, root);
        }
        if root && st.st_uid != r.uid {
            return;
        }
        let special = Mode::from_raw_mode(st.st_mode) & (Mode::SUID | Mode::SGID | Mode::SVTX);
        let _ = rfs::fchmod(&self.0, Mode::from_raw_mode((r.mode & 0o777) as _) | special);
        let _ = rfs::futimens(&self.0, &stamps(r));
    }

    /// Puts the recorded owner and time on a link in this folder, not on what it points at.
    pub fn finish_link(&self, name: &OsStr, r: &Recorded, root: bool) {
        if root {
            let (u, g) = (Some(Uid::from_raw(r.uid)), Some(Gid::from_raw(r.gid)));
            let _ = rfs::chownat(&self.0, name, u, g, AtFlags::SYMLINK_NOFOLLOW);
        }
        let _ = rfs::utimensat(&self.0, name, &stamps(r), AtFlags::SYMLINK_NOFOLLOW);
    }
}

pub fn stat(path: &Path, follow: bool) -> io::Result<Meta> {
    Ok(meta(&rfs::statat(CWD, path, at(follow))?))
}

pub fn read_link(path: &Path) -> io::Result<Vec<u8>> {
    Ok(rfs::readlinkat(CWD, path, vec![])?.into_bytes())
}

/// Opens a file for reading, never through a link in the last place and never waiting on a pipe.
pub fn open_file(path: &Path) -> io::Result<fs::File> {
    Ok(rfs::open(path, READ, Mode::empty())?.into())
}

pub fn file_meta(f: &fs::File) -> io::Result<Meta> {
    Ok(meta(&rfs::fstat(f)?))
}

/// Puts what was recorded back on a restored file.
pub fn finish_file(f: &fs::File, r: &Recorded, root: bool) {
    finish(f, r, root)
}

/// A Unix disk leaves holes where a file is skipped over by itself.
pub fn set_sparse(_f: &fs::File) -> io::Result<()> {
    Ok(())
}

/// Whether this process may give files to other owners.
pub fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// Whether something belongs to this user and is closed to everyone else.
pub fn private_to_me(m: &Meta) -> bool {
    m.uid == rustix::process::geteuid().as_raw() && m.mode & 0o077 == 0
}

/// Where the next hole in a file starts, at or after `pos`.
pub fn seek_hole(f: &fs::File, pos: u64) -> io::Result<u64> {
    Ok(rfs::seek(f, rfs::SeekFrom::Hole(pos))?)
}

/// Where data next starts at or after `pos`; None when only a hole is left.
pub fn seek_data(f: &fs::File, pos: u64) -> io::Result<Option<u64>> {
    match rfs::seek(f, rfs::SeekFrom::Data(pos)) {
        Ok(d) => Ok(Some(d)),
        Err(Errno::NXIO) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// How much of a file the disk really holds.
pub fn allocated(f: &fs::File) -> io::Result<u64> {
    Ok(f.metadata()?.blocks().saturating_mul(512))
}

/// A path as a restore point records it.
pub fn path_bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_bytes().to_vec()
}

pub fn bytes_path(b: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(b))
}

/// A recorded name as this system keeps it, and whether it had to be changed.
pub fn name(n: &[u8]) -> (OsString, bool) {
    (OsStr::from_bytes(n).to_os_string(), false)
}

/// Where a Windows drive is. Nowhere, on this system.
pub fn drive_root(_letter: u8) -> Option<PathBuf> {
    None
}
