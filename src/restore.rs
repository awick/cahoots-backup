// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::{
    backup::MAX_CHUNK,
    config::Friend,
    node::{Node, Out},
    snapshot::{Entry, Kind, human},
};
use anyhow::{Context, Result, anyhow, bail};
use rustix::{
    fs::{self as rfs, AtFlags, Mode, OFlags, Timespec, Timestamps},
    process::{Gid, Uid},
};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Seek, SeekFrom, Write},
    os::{fd::OwnedFd, unix::ffi::OsStrExt},
    path::{Component, Path, PathBuf},
};

/// Where an entry lands below the restore folder: under the name of the folder that was
/// backed up (`~/Documents/a.txt` gives `Documents/a.txt`), or at its full original path
/// when restoring in place or when two backed up folders share a name.
fn rel(e: &Entry, roots: &[PathBuf]) -> PathBuf {
    let p = e.path();
    let base = roots.iter().find(|r| p.starts_with(r)).and_then(|r| r.parent());
    p.strip_prefix(base.unwrap_or(Path::new("/"))).unwrap_or(&p).to_path_buf()
}

/// Opens the folder `rel` below `top` one name at a time, never passing through a symbolic
/// link, so nothing planted in the restore folder can steer a write somewhere else. On
/// failure it gives the part of `rel` that is in the way, so what lies below it can be skipped.
fn open_dir(top: &OwnedFd, rel: &Path, create: bool) -> Result<OwnedFd, (PathBuf, anyhow::Error)> {
    let mut fd = top.try_clone().map_err(|e| (rel.to_path_buf(), e.into()))?;
    let mut at = PathBuf::new();
    for c in rel.components() {
        let name = match c {
            Component::Normal(n) => n,
            Component::RootDir | Component::CurDir => continue,
            _ => return Err((rel.to_path_buf(), anyhow!("unsafe path"))),
        };
        at.push(name);
        if create {
            match rfs::mkdirat(&fd, name, Mode::from_raw_mode(0o700)) {
                Err(e) if e != rustix::io::Errno::EXIST => return Err((at, e.into())),
                _ => {}
            }
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        fd = rfs::openat(&fd, name, flags, Mode::empty())
            .map_err(|e| (at.clone(), anyhow!("a link or not a folder, refusing to restore through it ({e})")))?;
    }
    Ok(fd)
}

fn stamps(e: &Entry) -> Timestamps {
    let t = Timespec { tv_sec: e.mtime.div_euclid(1_000_000_000), tv_nsec: e.mtime.rem_euclid(1_000_000_000) as _ };
    Timestamps { last_access: t, last_modification: t }
}

/// Applies the recorded owner, permissions and time to an open file or folder. Set-id bits
/// only survive a restore run by root, which also restores the owner they belong to.
fn finish(fd: &impl std::os::fd::AsFd, e: &Entry, root: bool) {
    // Kept only once the file really belongs to that owner, never on one left with root's.
    let owned = root && rfs::fchown(fd, Some(Uid::from_raw(e.uid)), Some(Gid::from_raw(e.gid))).is_ok();
    let keep = if owned { 0o7777 } else { 0o777 };
    let _ = rfs::fchmod(fd, Mode::from_raw_mode((e.mode & keep) as _));
    let _ = rfs::futimens(fd, &stamps(e));
}

/// Makes the link in a new folder only this process can write, gives it its recorded owner
/// and time there, then moves it into place. Done next to the destination, anyone who can
/// write the restore folder could swap in a hard link to another file between the steps and
/// have that file chowned instead.
fn place_link(dir: &OwnedFd, name: &OsStr, target: &[u8], e: &Entry, root: bool) -> Result<()> {
    let tmp = format!(".sdb-tmp-{:016x}", rand::random::<u64>());
    rfs::mkdirat(dir, &tmp, Mode::from_raw_mode(0o700))?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let made = rfs::openat(dir, &tmp, flags, Mode::empty()).map_err(anyhow::Error::from).and_then(|t| {
        // Someone who can write `dir` may have renamed ours away and put their own in its place.
        let st = rfs::fstat(&t)?;
        if st.st_uid != rustix::process::geteuid().as_raw() || u32::from(st.st_mode) & 0o077 != 0 {
            bail!("the temporary folder was replaced");
        }
        rfs::symlinkat(OsStr::from_bytes(target), &t, "l")?;
        if root {
            let (u, g) = (Some(Uid::from_raw(e.uid)), Some(Gid::from_raw(e.gid)));
            let _ = rfs::chownat(&t, "l", u, g, AtFlags::SYMLINK_NOFOLLOW);
        }
        let _ = rfs::utimensat(&t, "l", &stamps(e), AtFlags::SYMLINK_NOFOLLOW);
        let moved = rfs::renameat(&t, "l", dir, name);
        if moved.is_err() {
            let _ = rfs::unlinkat(&t, "l", AtFlags::empty());
        }
        Ok(moved?)
    });
    let _ = rfs::unlinkat(dir, &tmp, AtFlags::REMOVEDIR);
    made
}

/// Reports something that could not be restored. A folder that is in the way is remembered,
/// so what lies below it is skipped quietly rather than reported line by line.
fn skip(out: &Out, to: &Path, at: &Path, err: &anyhow::Error, blocked: Option<&mut Vec<PathBuf>>) {
    match blocked {
        Some(b) => {
            out.say(format!("  skipped {} and everything in it: {err:#}", to.join(at).display()));
            b.push(at.to_path_buf());
        }
        None => out.say(format!("  skipped {}: {err:#}", to.join(at).display())),
    }
}

pub async fn run(
    node: &Node,
    friends: &[Friend],
    entries: Vec<Entry>,
    to: &Path,
    roots: &[String],
    out: &Out,
) -> Result<()> {
    let mut roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    let names: std::collections::HashSet<_> = roots.iter().map(|r| r.file_name().map(|n| n.to_owned())).collect();
    if to == Path::new("/") || names.len() != roots.len() {
        roots.clear();
    }
    let roots = &roots[..];
    let root = rustix::process::geteuid().is_root();
    // `to` is the folder the user chose, so links in it and its parents are followed on purpose.
    fs::create_dir_all(to).with_context(|| to.display().to_string())?;
    let top = rfs::open(to, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
        .with_context(|| to.display().to_string())?;
    let (mut files, mut bytes, mut skipped) = (0u64, 0u64, 0u64);
    let mut told = std::time::Instant::now();
    // A hole the backup skipped is stored as this chunk; it is skipped again here rather
    // than fetched and written, so a sparse file comes back sparse.
    let zero = node.keys.chunk_id(&vec![0; MAX_CHUNK]);
    // Folders that could not be opened; everything below them is skipped.
    let mut blocked: Vec<PathBuf> = Vec::new();
    // The folder most recently written into, kept open for the files that follow in it.
    let mut held: Option<(PathBuf, OwnedFd)> = None;
    for e in &entries {
        let path = rel(e, roots);
        if blocked.iter().any(|b| path.starts_with(b)) {
            skipped += 1;
            continue;
        }
        if e.kind == Kind::Dir {
            if let Err((at, err)) = open_dir(&top, &path, true) {
                skip(out, to, &at, &err, Some(&mut blocked));
                skipped += 1;
            }
            continue;
        }
        let (parent, name) = match (path.parent(), path.file_name()) {
            (Some(p), Some(n)) => (p, n),
            _ => {
                skip(out, to, &path, &anyhow!("unsafe path"), None);
                skipped += 1;
                continue;
            }
        };
        if held.as_ref().is_none_or(|(p, _)| p != parent) {
            held = None;
            match open_dir(&top, parent, true) {
                Ok(fd) => held = Some((parent.to_path_buf(), fd)),
                Err((at, err)) => {
                    skip(out, to, &at, &err, Some(&mut blocked));
                    skipped += 1;
                    continue;
                }
            }
        }
        let dir = &held.as_ref().unwrap().1;
        match &e.kind {
            Kind::Dir => {}
            Kind::Symlink(target) => {
                if let Err(err) = place_link(dir, name, target, e, root) {
                    skip(out, to, &path, &err, None);
                    skipped += 1;
                }
            }
            Kind::File { chunks, size } => {
                let mut written = 0u64;
                let mut tmp: OsString = name.to_owned();
                tmp.push(".sdb-tmp");
                let tmp = &tmp;
                let _ = rfs::unlinkat(dir, tmp, AtFlags::empty());
                // A brand new file, never one (or a link) that was waiting under this name.
                let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
                let fd = match rfs::openat(dir, tmp, flags, Mode::from_raw_mode(0o600)) {
                    Ok(fd) => fd,
                    Err(err) => {
                        skip(out, to, &path, &err.into(), None);
                        skipped += 1;
                        continue;
                    }
                };
                let mut f = fs::File::from(fd);
                let mut copy = async || -> Result<()> {
                    for run in chunks.chunk_by(|a, b| (*a == zero) == (*b == zero)) {
                        if run[0] == zero {
                            let n = (run.len() * MAX_CHUNK) as u64;
                            f.seek(SeekFrom::Current(n as i64))?;
                            written += n;
                            bytes += n;
                            continue;
                        }
                        for batch in run.chunks(8) {
                            for data in node.fetch(friends, batch).await.with_context(|| e.show())? {
                                bytes += data.len() as u64;
                                written += data.len() as u64;
                                if told.elapsed().as_secs() >= 2 {
                                    told = std::time::Instant::now();
                                    out.say(format!("  ... {files} files, {} restored so far", human(bytes)));
                                }
                                f.write_all(&data)?;
                            }
                        }
                    }
                    // A file that ends in a hole gets its length here.
                    f.set_len(written)?;
                    Ok(())
                };
                let copied = copy().await;
                if let Err(err) = copied {
                    let _ = rfs::unlinkat(dir, tmp, AtFlags::empty());
                    return Err(err);
                }
                if written != *size {
                    let _ = rfs::unlinkat(dir, tmp, AtFlags::empty());
                    skip(out, to, &path, &anyhow!("restored {written} bytes but the backup recorded {size}"), None);
                    skipped += 1;
                    continue;
                }
                finish(&f, e, root);
                drop(f);
                if let Err(err) = rfs::renameat(dir, tmp, dir, name) {
                    let _ = rfs::unlinkat(dir, tmp, AtFlags::empty());
                    skip(out, to, &path, &err.into(), None);
                    skipped += 1;
                    continue;
                }
                files += 1;
            }
        }
    }
    drop(held);
    for e in entries.iter().rev().filter(|e| e.kind == Kind::Dir) {
        if let Ok(fd) = open_dir(&top, &rel(e, roots), false) {
            finish(&fd, e, root);
        }
    }
    out.say(format!("restored {files} files ({}) into {}", human(bytes), to.display()));
    if skipped > 0 {
        bail!("{skipped} entries could not be restored (listed above), so the restore is incomplete");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn top(dir: &Path) -> OwnedFd {
        fs::create_dir_all(dir).unwrap();
        rfs::open(dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap()
    }

    #[test]
    fn folders_are_never_opened_through_a_link() {
        let dir = std::env::temp_dir().join(format!("sdb-test-restore-{}", rand::random::<u64>()));
        let top = top(&dir);
        fs::create_dir_all(dir.join("outside")).unwrap();
        std::os::unix::fs::symlink(dir.join("outside"), dir.join("link")).unwrap();
        assert!(open_dir(&top, Path::new("real/deeper"), true).is_ok());
        assert!(open_dir(&top, Path::new("link"), true).is_err());
        assert!(open_dir(&top, Path::new("link/sub"), true).is_err());
        assert!(open_dir(&top, Path::new("real/../outside"), true).is_err());
        assert!(!dir.join("outside/sub").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn set_id_bits_are_dropped_unless_root() {
        let dir = std::env::temp_dir().join(format!("sdb-test-restore-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        let f = fs::File::create(dir.join("helper")).unwrap();
        let e = Entry {
            path: vec![],
            mode: 0o104755,
            uid: 0,
            gid: 0,
            mtime: 1_700_000_000_000_000_000,
            ino: 0,
            kind: Kind::Dir,
        };
        finish(&f, &e, false);
        assert_eq!(f.metadata().unwrap().permissions().mode() & 0o7777, 0o755);
        fs::remove_dir_all(dir).unwrap();
    }
}
