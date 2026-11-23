// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Files and folders reached through open folders. Not written yet: every call says so.

use crate::os::{Kind, Meta, Recorded};
use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

fn not_yet<T>() -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "not supported on Windows yet"))
}

/// An open folder.
pub struct Dir(());

impl Dir {
    pub fn open(_path: &Path, _follow: bool) -> io::Result<Dir> {
        not_yet()
    }

    pub fn try_clone(&self) -> io::Result<Dir> {
        not_yet()
    }

    pub fn sub(&self, _name: &OsStr) -> io::Result<Dir> {
        not_yet()
    }

    pub fn make(&self, _name: &OsStr) -> io::Result<()> {
        not_yet()
    }

    pub fn meta(&self) -> io::Result<Meta> {
        not_yet()
    }

    pub fn stat(&self, _name: &OsStr, _follow: bool) -> io::Result<Meta> {
        not_yet()
    }

    pub fn each_name(&self, _f: impl FnMut(&OsStr, Kind) -> bool) {}

    pub fn names(&self) -> io::Result<Vec<OsString>> {
        not_yet()
    }

    pub fn read_link(&self, _name: &OsStr) -> io::Result<Vec<u8>> {
        not_yet()
    }

    pub fn open_file(&self, _name: &OsStr) -> io::Result<fs::File> {
        not_yet()
    }

    pub fn create_file(&self, _name: &OsStr) -> io::Result<fs::File> {
        not_yet()
    }

    pub fn rename(&self, _from: &OsStr, _to: &Dir, _name: &OsStr) -> io::Result<()> {
        not_yet()
    }

    pub fn remove_file(&self, _name: &OsStr) -> io::Result<()> {
        not_yet()
    }

    pub fn remove_dir(&self, _name: &OsStr) -> io::Result<()> {
        not_yet()
    }

    pub fn symlink(&self, _name: &OsStr, _target: &[u8]) -> io::Result<()> {
        not_yet()
    }

    pub fn set_private(&self) -> io::Result<()> {
        not_yet()
    }

    pub fn finish(&self, _r: &Recorded, _root: bool) {}

    pub fn finish_existing(&self, _r: &Recorded, _root: bool) {}

    pub fn finish_link(&self, _name: &OsStr, _r: &Recorded, _root: bool) {}
}

pub fn stat(_path: &Path, _follow: bool) -> io::Result<Meta> {
    not_yet()
}

pub fn read_link(_path: &Path) -> io::Result<Vec<u8>> {
    not_yet()
}

pub fn open_file(_path: &Path) -> io::Result<fs::File> {
    not_yet()
}

pub fn file_meta(_f: &fs::File) -> io::Result<Meta> {
    not_yet()
}

pub fn finish_file(_f: &fs::File, _r: &Recorded, _root: bool) {}

pub fn is_root() -> bool {
    false
}

pub fn private_to_me(_m: &Meta) -> bool {
    false
}

pub fn seek_hole(_f: &fs::File, _pos: u64) -> io::Result<u64> {
    not_yet()
}

pub fn seek_data(_f: &fs::File, _pos: u64) -> io::Result<Option<u64>> {
    not_yet()
}

pub fn allocated(_f: &fs::File) -> io::Result<u64> {
    not_yet()
}

pub fn path_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

pub fn bytes_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}
