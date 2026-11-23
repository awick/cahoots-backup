// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Where a node keeps the encrypted data its friends back up to it.

use crate::{
    crypto::Id,
    os::{Dir, Kind},
    proto::{ID_BATCH, MAX_BLOB, MAX_IDS, MAX_MSG, MAX_SNAPSHOT},
};
use anyhow::{Result, anyhow, bail};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs, io,
    io::{Read, Write},
    mem,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
};

/// Header of every stored chunk file: magic plus layout version.
const FILE_MAGIC: &[u8; 5] = b"SDBC\x01";

/// Most restore points one owner may keep here, so tiny files cannot exhaust the disk's inodes.
const MAX_SNAPSHOTS: usize = 50_000;

/// The largest file the store ever writes: a chunk with its header and checksum.
const MAX_FILE: usize = MAX_BLOB + 37;

/// The smallest chunk file: its header and checksum around no data.
const MIN_FILE: usize = 37;

type Locks = Mutex<HashMap<String, Arc<Mutex<()>>>>;

/// What the store knows about one owner's usage.
#[derive(Default)]
struct Count {
    /// Bytes counted against their quota; None until their folder has been walked.
    used: Option<u64>,
    /// Bumped as each change to their files starts and ends, so a walk that overlapped a change
    /// is not remembered.
    generation: u64,
    /// Changes under way.
    open: u32,
}

pub struct Store {
    root: PathBuf,
    /// Writes are refused once the disk has less free space than this.
    min_free: AtomicU64,
    usage: Mutex<HashMap<String, Count>>,
    /// One lock per owner, held while their files and their usage count change together.
    /// Only ever taken off the async workers.
    locks: Locks,
    /// One first walk per owner at a time. Never held while taking an owner's lock.
    walks: Locks,
    /// Owners whose data was deleted on purpose; nothing is stored for them until they pair again.
    banned: Mutex<HashSet<String>>,
}

/// What a stored file counts against a quota: never less than a disk block,
/// so a flood of tiny files cannot hide inside a byte quota.
pub fn cost(len: u64) -> u64 {
    len.max(4096)
}

fn dir_size(p: &Path) -> u64 {
    walkdir::WalkDir::new(p)
        .follow_root_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| cost(m.len()))
        .sum()
}

fn lock_in(locks: &Locks, owner: &str) -> Arc<Mutex<()>> {
    locks.lock().unwrap().entry(owner.to_string()).or_default().clone()
}

/// Reads at most `max` bytes of a regular file. Type and size come from the open file itself,
/// so a file swapped for a link, a device or a pipe after any check is not read.
fn read_open(f: fs::File, max: usize) -> Option<Vec<u8>> {
    let md = f.metadata().ok()?;
    if !md.is_file() || md.len() > max as u64 {
        return None;
    }
    let mut buf = Vec::with_capacity(md.len() as usize);
    f.take(md.len()).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// A name of exactly `len` lowercase hex digits, as the store writes them.
fn hex_name(name: &OsStr, len: usize) -> bool {
    name.len() == len && name.as_encoded_bytes().iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Hands each well-named chunk file in an owner's open chunks folder (`<2hex>/<2hex><62hex>`) to
/// `f`, with the open folder it sits in, until `f` returns false. Each folder is opened from the
/// one above it and never through a link, so a folder swapped for a link cannot lead outside.
fn each_chunk(chunks: &Dir, mut f: impl FnMut(&Dir, &OsStr, Kind) -> bool) {
    let mut go = true;
    chunks.each_name(|shard, _| {
        let Some(dir) = hex_name(shard, 2).then(|| chunks.sub(shard).ok()).flatten() else { return true };
        dir.each_name(|name, kind| {
            if hex_name(name, 64) && name.as_encoded_bytes().starts_with(shard.as_encoded_bytes()) {
                go = f(&dir, name, kind);
            }
            go
        });
        go
    });
}

pub fn check_owner(owner: &str) -> Result<()> {
    if owner.len() != 64 || !owner.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("bad owner id");
    }
    Ok(())
}

pub fn check_snap(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("bad snapshot id");
    }
    Ok(())
}

impl Store {
    pub fn set_min_free(&self, bytes: u64) {
        self.min_free.store(bytes, Ordering::Relaxed);
    }

    pub fn new(root: PathBuf, min_free: u64) -> Self {
        if fs::create_dir_all(&root).is_ok() {
            // Through a handle that refuses a link, so a link swapped in cannot redirect the change.
            let _ = Dir::open(&root, false).and_then(|d| d.set_private());
        }
        Store {
            root,
            min_free: AtomicU64::new(min_free),
            usage: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            walks: Mutex::new(HashMap::new()),
            banned: Mutex::new(HashSet::new()),
        }
    }

    fn lock(&self, owner: &str) -> Arc<Mutex<()>> {
        lock_in(&self.locks, owner)
    }

    /// Opens a folder under the root one step at a time, never following a link, so a link
    /// planted (or swapped in after a check) anywhere below the root is refused, not followed.
    fn open_dir(&self, dir: &Path, create: bool) -> io::Result<Dir> {
        let rel = dir.strip_prefix(&self.root).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut fd = Dir::open(&self.root, true)?;
        for c in rel.components() {
            let Component::Normal(name) = c else { return Err(io::ErrorKind::InvalidInput.into()) };
            fd = match fd.sub(name) {
                Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                    match fd.make(name) {
                        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                        _ => {}
                    }
                    fd.sub(name)?
                }
                r => r?,
            };
        }
        Ok(fd)
    }

    fn read_at(dir: &Dir, name: &OsStr, max: usize) -> Option<Vec<u8>> {
        read_open(dir.open_file(name).ok()?, max)
    }

    /// Reads a regular file of at most `max` bytes that sits in plain folders under the root.
    fn read_plain(&self, path: &Path, max: usize) -> Option<Vec<u8>> {
        Self::read_at(&self.open_dir(path.parent()?, false).ok()?, path.file_name()?, max)
    }

    /// Removes one stored file and returns what it counted; None if no regular file is there.
    fn remove(&self, path: &Path) -> io::Result<Option<u64>> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return Ok(None) };
        let dir = match self.open_dir(dir, false) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            r => r?,
        };
        match dir.stat(name, false) {
            Ok(m) if m.kind == Kind::File => {
                dir.remove_file(name)?;
                Ok(Some(cost(m.size)))
            }
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn chunk_path(&self, owner: &str, id: &Id) -> PathBuf {
        let h = hex::encode(id);
        self.root.join(owner).join("chunks").join(&h[..2]).join(h)
    }

    fn snap_dir(&self, owner: &str) -> PathBuf {
        self.root.join(owner).join("snapshots")
    }

    pub fn usage(&self, owner: &str) -> Result<u64> {
        check_owner(owner)?;
        Ok(self.used(owner))
    }

    /// The owner's usage if it has been counted already. Never walks their folder, so views
    /// may call it from the async workers.
    pub fn usage_known(&self, owner: &str) -> Option<u64> {
        check_owner(owner).ok()?;
        self.counted(owner, |c| c.used)
    }

    fn counted<T>(&self, owner: &str, f: impl FnOnce(&mut Count) -> T) -> T {
        f(self.usage.lock().unwrap().entry(owner.to_string()).or_default())
    }

    /// The owner's usage. The first count walks their folder without holding their lock, so
    /// it stalls neither their other requests' bookkeeping nor this node's own views; a walk
    /// that overlapped a change still answers, but is not remembered.
    fn used(&self, owner: &str) -> u64 {
        if let Some(n) = self.counted(owner, |c| c.used) {
            return n;
        }
        let walk = lock_in(&self.walks, owner);
        let _one = walk.lock().unwrap();
        let start = match self.counted(owner, |c| c.used.ok_or((c.generation, c.open))) {
            Ok(n) => return n,
            Err(start) => start,
        };
        let n = dir_size(&self.root.join(owner));
        self.counted(owner, |c| match c.used {
            Some(u) => u,
            None => {
                if start == (c.generation, 0) && c.open == 0 {
                    c.used = Some(n);
                }
                n
            }
        })
    }

    /// Makes one change to an owner's files, returning its result and its effect on their
    /// usage, and keeps the count in step. The caller holds the owner's lock.
    fn change<T>(&self, owner: &str, f: impl FnOnce() -> (T, i64)) -> T {
        self.counted(owner, |c| {
            c.generation += 1;
            c.open += 1;
        });
        let (out, delta) = f();
        self.counted(owner, |c| {
            c.used = c.used.map(|u| u.saturating_add_signed(delta));
            c.generation += 1;
            c.open -= 1;
        });
        out
    }

    /// Stores one file for an owner; the caller holds the owner's lock.
    fn write(&self, owner: &str, path: &Path, data: &[u8], quota: u64) -> Result<()> {
        check_owner(owner)?;
        if self.banned.lock().unwrap().contains(owner) {
            bail!("you are not on this node's friend list");
        }
        let name = path.file_name().unwrap();
        // Every step goes through this handle, so nothing swapped in above it can redirect the write.
        let dir = self
            .open_dir(path.parent().unwrap(), true)
            .map_err(|e| anyhow!("the store folder has been tampered with or cannot be opened ({e})"))?;
        let old = match dir.stat(name, false) {
            Ok(m) if m.kind == Kind::File => cost(m.size),
            _ => 0,
        };
        let new = cost(data.len() as u64);
        if self.used(owner).saturating_sub(old) + new > quota {
            bail!("storage quota exceeded ({} GB)", quota >> 30);
        }
        let min_free = self.min_free.load(Ordering::Relaxed);
        if min_free > 0 && fs2::available_space(&self.root).is_ok_and(|free| free < min_free + new) {
            bail!("this computer is too low on disk space to accept more backups");
        }
        let mut tmp = name.to_os_string();
        tmp.push(format!(".tmp{}", rand::random::<u64>()));
        self.change(owner, || {
            let res = (|| -> io::Result<()> {
                dir.create_file(&tmp)?.write_all(data)?;
                dir.rename(&tmp, &dir, name)
            })();
            if res.is_err() {
                let _ = dir.remove_file(&tmp);
            }
            let delta = if res.is_ok() { new as i64 - old as i64 } else { 0 };
            (res, delta)
        })?;
        Ok(())
    }

    pub fn put_chunk(&self, owner: &str, id: &Id, data: &[u8], quota: u64) -> Result<()> {
        let lock = self.lock(owner);
        let _held = lock.lock().unwrap();
        let p = self.chunk_path(owner, id);
        if fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) {
            return Ok(());
        }
        // A checksum of the ciphertext rides along so this node can spot disk rot without any key.
        let mut buf = Vec::with_capacity(data.len() + 37);
        buf.extend_from_slice(FILE_MAGIC);
        buf.extend_from_slice(data);
        buf.extend_from_slice(blake3::hash(data).as_bytes());
        self.write(owner, &p, &buf, quota)
    }

    fn intact(buf: &[u8]) -> bool {
        buf.len() >= MIN_FILE
            && buf[..5] == FILE_MAGIC[..]
            && blake3::hash(&buf[5..buf.len() - 32]).as_bytes() == &buf[buf.len() - 32..]
    }

    /// Returns a chunk only if it still matches its checksum; a damaged one is dropped so the owner sees it as missing.
    pub fn get_chunk(&self, owner: &str, id: &Id) -> Option<Vec<u8>> {
        let p = self.chunk_path(owner, id);
        let mut buf = self.read_plain(&p, MAX_FILE)?;
        if !Self::intact(&buf) {
            let lock = self.lock(owner);
            let _held = lock.lock().unwrap();
            self.change(owner, || {
                let gone = self.remove(&p).ok().flatten();
                ((), -(gone.unwrap_or(0) as i64))
            });
            return None;
        }
        buf.truncate(buf.len() - 32);
        buf.drain(..5);
        Some(buf)
    }

    /// Re-reads everything friends store here and removes chunks that no longer match their checksum.
    /// Every read and delete goes through folders opened from the root, so a folder swapped for
    /// a link mid-scrub cannot point a delete outside the store.
    pub fn scrub(&self) -> (u64, u64) {
        let (mut checked, mut bad) = (0, 0);
        let Ok(root) = self.open_dir(&self.root, false) else { return (0, 0) };
        root.each_name(|owner, _| {
            let ok = owner.to_str().is_some_and(|o| check_owner(o).is_ok());
            let chunks = ok.then(|| root.sub(owner).ok()).flatten().and_then(|d| d.sub("chunks".as_ref()).ok());
            let Some(chunks) = chunks else { return true };
            each_chunk(&chunks, |dir, name, _| {
                let Some(buf) = Self::read_at(dir, name, MAX_FILE) else { return true };
                checked += 1;
                if !Self::intact(&buf) {
                    bad += 1;
                    let _ = dir.remove_file(name);
                }
                true
            });
            true
        });
        if bad > 0 {
            for c in self.usage.lock().unwrap().values_mut() {
                c.used = None;
                c.generation += 1;
            }
        }
        (checked, bad)
    }

    /// Hands every chunk id an owner keeps here to `each`, a batch at a time, until it returns
    /// false. Nothing is gathered in memory, however many chunks there are. Only regular files
    /// in their own shard folder, with a chunk's name and possible size, count, and never more
    /// than one listing may carry, so planted names cannot swell the listing.
    pub fn list_chunks(&self, owner: &str, mut each: impl FnMut(Vec<Id>) -> bool) {
        let dir = self.root.join(owner).join("chunks");
        let Some(chunks) = check_owner(owner).ok().and_then(|_| self.open_dir(&dir, false).ok()) else { return };
        let (mut batch, mut total, mut go) = (Vec::with_capacity(ID_BATCH), 0u64, true);
        each_chunk(&chunks, |dir, name, kind| {
            // The folder says what each entry is, so a listing reads only folders: a stat per
            // chunk would make a large listing from a slow disk crawl. Damaged or empty files are
            // what scrub and verify are for.
            let file = match kind {
                Kind::File => true,
                Kind::Unknown => dir.stat(name, false).is_ok_and(|m| m.kind == Kind::File),
                _ => false,
            };
            let Some(id) = file.then(|| hex::decode(name.as_encoded_bytes()).ok()?.try_into().ok()).flatten() else {
                return true;
            };
            batch.push(id);
            total += 1;
            if batch.len() == ID_BATCH {
                go = each(mem::replace(&mut batch, Vec::with_capacity(ID_BATCH)));
            }
            go && total < MAX_IDS
        });
        if go && !batch.is_empty() {
            each(batch);
        }
    }

    /// Takes the owner's lock per chunk, so a long delete never keeps their other requests waiting for long.
    pub fn delete_chunks(&self, owner: &str, ids: &[Id]) {
        let lock = self.lock(owner);
        for id in ids {
            let p = self.chunk_path(owner, id);
            let _held = lock.lock().unwrap();
            self.change(owner, || {
                let gone = self.remove(&p).ok().flatten();
                ((), -(gone.unwrap_or(0) as i64))
            });
        }
    }

    pub fn put_snapshot(&self, owner: &str, id: &str, data: &[u8], quota: u64) -> Result<()> {
        check_snap(id)?;
        if data.len() > MAX_SNAPSHOT {
            bail!("restore point record too large");
        }
        let lock = self.lock(owner);
        let _held = lock.lock().unwrap();
        let path = self.snap_dir(owner).join(id);
        if fs::symlink_metadata(&path).is_err()
            && fs::read_dir(self.snap_dir(owner)).map(|d| d.count()).unwrap_or(0) >= MAX_SNAPSHOTS
        {
            bail!("too many restore points stored here; prune some first");
        }
        self.write(owner, &path, data, quota)
    }

    /// Every restore point record an owner keeps here. Fails rather than build a reply
    /// larger than one message may be.
    pub fn list_snapshots(&self, owner: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let dir = self.snap_dir(owner);
        let (Ok(fd), Ok(rd)) = (self.open_dir(&dir, false), fs::read_dir(&dir)) else {
            return Ok(vec![]);
        };
        let (mut out, mut total) = (vec![], 0usize);
        for e in rd.flatten() {
            let Ok(name) = e.file_name().into_string() else { continue };
            if check_snap(&name).is_err() {
                continue;
            }
            let Some(data) = Self::read_at(&fd, name.as_ref(), MAX_SNAPSHOT) else { continue };
            total += data.len() + name.len() + 16;
            if total > MAX_MSG - (1 << 20) {
                bail!("too many restore points are stored here to list; delete some");
            }
            out.push((name, data));
        }
        Ok(out)
    }

    pub fn delete_snapshot(&self, owner: &str, id: &str) -> Result<()> {
        check_snap(id)?;
        let lock = self.lock(owner);
        let _held = lock.lock().unwrap();
        let p = self.snap_dir(owner).join(id);
        self.change(owner, || match self.remove(&p) {
            Ok(gone) => (Ok(()), -(gone.unwrap_or(0) as i64)),
            Err(e) => (Err(anyhow!("cannot delete restore point: {e}")), 0),
        })
    }

    /// When this owner last stored a restore point here.
    pub fn last_snapshot(&self, owner: &str) -> Option<i64> {
        use std::time::UNIX_EPOCH;
        fs::read_dir(self.snap_dir(owner))
            .ok()?
            .flatten()
            .filter_map(|e| e.metadata().ok()?.modified().ok())
            .max()
            .and_then(|t| Some(t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64))
    }

    /// Deletes everything an owner stored here and refuses more until `allow` is called for them.
    pub fn forget_owner(&self, owner: &str) -> Result<()> {
        check_owner(owner)?;
        let lock = self.lock(owner);
        let _held = lock.lock().unwrap();
        self.banned.lock().unwrap().insert(owner.to_string());
        let p = self.root.join(owner);
        // Counted afresh next time; the bump also discards any walk running now.
        let res = self.change(owner, || {
            let res = if fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) { fs::remove_dir_all(p) } else { Ok(()) };
            self.counted(owner, |c| c.used = None);
            (res, 0)
        });
        Ok(res?)
    }

    /// Lets an owner store here again after pairing anew.
    pub fn allow(&self, owner: &str) {
        self.banned.lock().unwrap().remove(owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn ids(s: &Store) -> Vec<Id> {
        let mut v = vec![];
        s.list_chunks(OWNER, |b| {
            v.extend(b);
            true
        });
        v
    }

    fn store(min_free: u64) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sdb-test-store-{}", rand::random::<u64>()));
        (Store::new(dir.clone(), min_free), dir)
    }

    #[test]
    fn chunks_round_trip_and_list() {
        let (s, dir) = store(0);
        s.put_chunk(OWNER, &[1; 32], b"ciphertext", 1 << 20).unwrap();
        s.put_chunk(OWNER, &[1; 32], b"ignored: already stored", 1 << 20).unwrap();
        assert_eq!(s.get_chunk(OWNER, &[1; 32]).unwrap(), b"ciphertext");
        assert_eq!(ids(&s), vec![[1u8; 32]]);
        assert!(s.get_chunk(OWNER, &[2; 32]).is_none());
        s.delete_chunks(OWNER, &[[1; 32]]);
        assert!(ids(&s).is_empty());
        assert_eq!(s.usage(OWNER).unwrap(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disk_damage_is_detected_and_dropped() {
        let (s, dir) = store(0);
        s.put_chunk(OWNER, &[3; 32], &[5u8; 1000], 1 << 20).unwrap();
        s.put_chunk(OWNER, &[4; 32], &[6u8; 1000], 1 << 20).unwrap();
        for id in [[3u8; 32], [4u8; 32]] {
            let path = s.chunk_path(OWNER, &id);
            let mut raw = fs::read(&path).unwrap();
            raw[500] ^= 0xff;
            fs::write(&path, raw).unwrap();
        }
        assert!(s.get_chunk(OWNER, &[3; 32]).is_none(), "a damaged chunk must not be served");
        assert_eq!(ids(&s), vec![[4u8; 32]], "and it is removed so the owner sees it missing");
        assert_eq!(s.scrub(), (1, 1));
        assert!(ids(&s).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quota_counts_every_file_as_at_least_a_block() {
        let (s, dir) = store(0);
        s.put_chunk(OWNER, &[1; 32], b"x", 8192).unwrap();
        s.put_chunk(OWNER, &[2; 32], b"y", 8192).unwrap();
        assert_eq!(s.usage(OWNER).unwrap(), 8192);
        let err = s.put_chunk(OWNER, &[3; 32], b"z", 8192).unwrap_err().to_string();
        assert!(err.contains("quota exceeded"), "{err}");
        s.delete_chunks(OWNER, &[[1; 32]]);
        s.put_chunk(OWNER, &[3; 32], b"z", 8192).unwrap();
        // A fresh store sees the same usage by scanning the disk.
        assert_eq!(Store::new(dir.clone(), 0).usage(OWNER).unwrap(), 8192);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_cannot_escape_the_owner_folder() {
        let (s, dir) = store(0);
        assert!(s.usage("../../etc").is_err());
        assert!(s.usage("short").is_err());
        assert!(check_owner(OWNER).is_ok());
        for bad in ["../x", "a/b", "", "zz", &"a".repeat(65)] {
            assert!(s.put_snapshot(OWNER, bad, b"m", 1 << 20).is_err(), "{bad:?}");
            assert!(s.delete_snapshot(OWNER, bad).is_err(), "{bad:?}");
        }
        s.put_snapshot(OWNER, "abc123", b"manifest", 1 << 20).unwrap();
        assert_eq!(s.list_snapshots(OWNER).unwrap(), vec![("abc123".to_string(), b"manifest".to_vec())]);
        assert!(s.last_snapshot(OWNER).is_some());
        s.delete_snapshot(OWNER, "abc123").unwrap();
        assert!(s.list_snapshots(OWNER).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn low_disk_space_refuses_writes() {
        let (s, dir) = store(u64::MAX / 2);
        let err = s.put_chunk(OWNER, &[1; 32], b"x", 1 << 30).unwrap_err().to_string();
        assert!(err.contains("low on disk space"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_writes_and_deletes_keep_the_count_right() {
        let (s, dir) = store(0);
        let s = Arc::new(s);
        let quota = 64 << 10;
        let threads: Vec<_> = (0..8u8)
            .map(|t| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for i in 0..200u32 {
                        let _ = s.put_snapshot(OWNER, "aa", &vec![t; 5000 + (i as usize % 7) * 1000], quota);
                        let _ = s.delete_snapshot(OWNER, "aa");
                        let _ = s.put_chunk(OWNER, &[t; 32], &[t; 3000], quota);
                        s.delete_chunks(OWNER, &[[t; 32]]);
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(s.usage(OWNER).unwrap(), dir_size(&dir.join(OWNER)), "the count matches the disk");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn links_planted_in_the_store_are_not_followed() {
        let (s, dir) = store(0);
        let outside = dir.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("big"), vec![0u8; 100]).unwrap();
        fs::create_dir_all(dir.join(OWNER)).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(OWNER).join("chunks")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(OWNER).join("snapshots")).unwrap();
        assert!(s.put_chunk(OWNER, &[1; 32], b"x", 1 << 20).is_err());
        assert!(s.put_snapshot(OWNER, "abc", b"m", 1 << 20).is_err());
        assert!(ids(&s).is_empty());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1, "nothing was written through the link");
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn scrub_and_listing_keep_to_the_store_layout() {
        let (s, dir) = store(0);
        s.put_chunk(OWNER, &[0x11; 32], b"kept", 1 << 20).unwrap();
        let chunks = dir.join(OWNER).join("chunks");
        let real = s.chunk_path(OWNER, &[0x11; 32]);
        let fake = |id: [u8; 32]| hex::encode(id);
        // Planted names: in the wrong shard, upper case, a link, a folder, a stray temp file. (Sizes
        // are not looked at: listing reads only folders, and damaged files are scrub's and verify's.)
        fs::write(chunks.join("11").join(fake([0x22; 32])), vec![0u8; 100]).unwrap();
        fs::write(chunks.join("11").join(format!("11{}", "A".repeat(62))), vec![0u8; 100]).unwrap();
        fs::create_dir_all(chunks.join("33")).unwrap();
        std::os::unix::fs::symlink(&real, chunks.join("33").join(fake([0x33; 32]))).unwrap();
        fs::create_dir_all(chunks.join("44").join(fake([0x44; 32]))).unwrap();
        fs::write(chunks.join("11").join(format!("{}.tmp1", fake([0x11; 32]))), b"x").unwrap();
        assert_eq!(ids(&s), vec![[0x11u8; 32]], "only the stored chunk is listed");
        // An empty chunk file is for scrub to drop.
        fs::write(chunks.join("11").join(format!("11{}", "0".repeat(62))), b"").unwrap();
        // A shard folder that is a link to outside the store is neither read nor emptied.
        let outside = dir.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join(fake([0x55; 32])), b"victim").unwrap();
        std::os::unix::fs::symlink(&outside, chunks.join("55")).unwrap();
        assert_eq!(s.scrub(), (2, 1), "the empty chunk file in the store is dropped");
        assert!(outside.join(fake([0x55; 32])).exists(), "nothing outside the store was deleted");
        assert!(chunks.join("11").join(fake([0x22; 32])).exists(), "wrong-shard names are not touched");
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn oversize_records_and_forgotten_owners_are_refused() {
        let (s, dir) = store(0);
        assert!(s.put_snapshot(OWNER, "abc", &vec![0u8; MAX_SNAPSHOT + 1], 1 << 30).is_err());
        s.put_snapshot(OWNER, "abc", b"m", 1 << 30).unwrap();
        // A record that grew on disk behind our back is not read into memory.
        fs::write(s.snap_dir(OWNER).join("abd"), vec![0u8; MAX_SNAPSHOT + 1]).unwrap();
        assert_eq!(s.list_snapshots(OWNER).unwrap().len(), 1);
        s.forget_owner(OWNER).unwrap();
        assert!(s.put_chunk(OWNER, &[1; 32], b"x", 1 << 20).is_err(), "no uploads after a purge");
        assert!(!dir.join(OWNER).exists());
        s.allow(OWNER);
        s.put_chunk(OWNER, &[1; 32], b"x", 1 << 20).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
