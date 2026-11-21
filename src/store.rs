// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Where a node keeps the encrypted data its friends back up to it.

use crate::crypto::Id;
use anyhow::{Result, bail};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Header of every stored chunk file: magic plus layout version.
const FILE_MAGIC: &[u8; 5] = b"SDBC\x01";

/// Most restore points one owner may keep here, so tiny files cannot exhaust the disk's inodes.
const MAX_SNAPSHOTS: usize = 50_000;

pub struct Store {
    root: PathBuf,
    /// Writes are refused once the disk has less free space than this.
    min_free: u64,
    usage: Mutex<HashMap<String, u64>>,
}

/// What a stored file counts against a quota: never less than a disk block,
/// so a flood of tiny files cannot hide inside a byte quota.
fn cost(len: u64) -> u64 {
    len.max(4096)
}

fn dir_size(p: &Path) -> u64 {
    walkdir::WalkDir::new(p)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| cost(m.len()))
        .sum()
}

pub fn check_owner(owner: &str) -> Result<()> {
    if owner.len() != 64 || !owner.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("bad owner id");
    }
    Ok(())
}

fn check_snap(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("bad snapshot id");
    }
    Ok(())
}

impl Store {
    pub fn new(root: PathBuf, min_free: u64) -> Self {
        use std::os::unix::fs::PermissionsExt;
        if fs::create_dir_all(&root).is_ok() {
            let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        }
        Store {
            root,
            min_free,
            usage: Mutex::new(HashMap::new()),
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
        let mut u = self.usage.lock().unwrap();
        Ok(*u
            .entry(owner.to_string())
            .or_insert_with(|| dir_size(&self.root.join(owner))))
    }

    fn adjust(&self, owner: &str, delta: i64) {
        if let Some(u) = self.usage.lock().unwrap().get_mut(owner) {
            *u = u.saturating_add_signed(delta);
        }
    }

    fn write(&self, owner: &str, path: &Path, data: &[u8], quota: u64) -> Result<()> {
        let old = fs::metadata(path).map(|m| cost(m.len())).unwrap_or(0);
        let new = cost(data.len() as u64);
        if self.usage(owner)? - old.min(self.usage(owner)?) + new > quota {
            bail!("storage quota exceeded ({} GB)", quota >> 30);
        }
        if self.min_free > 0
            && fs2::available_space(&self.root).is_ok_and(|free| free < self.min_free + new)
        {
            bail!("this computer is too low on disk space to accept more backups");
        }
        fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension(format!("tmp{}", rand::random::<u32>()));
        fs::write(&tmp, data)?;
        fs::rename(&tmp, path)?;
        self.adjust(owner, new as i64 - old as i64);
        Ok(())
    }

    pub fn put_chunk(&self, owner: &str, id: &Id, data: &[u8], quota: u64) -> Result<()> {
        let p = self.chunk_path(owner, id);
        if p.exists() {
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
        buf.len() >= 37
            && buf[..5] == FILE_MAGIC[..]
            && blake3::hash(&buf[5..buf.len() - 32]).as_bytes() == &buf[buf.len() - 32..]
    }

    /// Returns a chunk only if it still matches its checksum; a damaged one is dropped so the owner sees it as missing.
    pub fn get_chunk(&self, owner: &str, id: &Id) -> Option<Vec<u8>> {
        let p = self.chunk_path(owner, id);
        let mut buf = fs::read(&p).ok()?;
        if !Self::intact(&buf) {
            if fs::remove_file(&p).is_ok() {
                self.adjust(owner, -(cost(buf.len() as u64) as i64));
            }
            return None;
        }
        buf.truncate(buf.len() - 32);
        buf.drain(..5);
        Some(buf)
    }

    /// Re-reads everything friends store here and removes chunks that no longer match their checksum.
    pub fn scrub(&self) -> (u64, u64) {
        let (mut checked, mut bad) = (0, 0);
        for e in walkdir::WalkDir::new(&self.root)
            .min_depth(4)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
        {
            let Ok(buf) = fs::read(e.path()) else {
                continue;
            };
            checked += 1;
            if !Self::intact(&buf) && e.path().extension().is_none() {
                bad += 1;
                let _ = fs::remove_file(e.path());
            }
        }
        if bad > 0 {
            self.usage.lock().unwrap().clear();
        }
        (checked, bad)
    }

    pub fn list_chunks(&self, owner: &str) -> Vec<Id> {
        walkdir::WalkDir::new(self.root.join(owner).join("chunks"))
            .min_depth(2)
            .into_iter()
            .flatten()
            .filter_map(|e| hex::decode(e.file_name().to_str()?).ok()?.try_into().ok())
            .collect()
    }

    pub fn delete_chunks(&self, owner: &str, ids: &[Id]) {
        for id in ids {
            let p = self.chunk_path(owner, id);
            if let Ok(m) = fs::metadata(&p) {
                if fs::remove_file(&p).is_ok() {
                    self.adjust(owner, -(cost(m.len()) as i64));
                }
            }
        }
    }

    pub fn put_snapshot(&self, owner: &str, id: &str, data: &[u8], quota: u64) -> Result<()> {
        check_snap(id)?;
        let path = self.snap_dir(owner).join(id);
        if !path.exists()
            && fs::read_dir(self.snap_dir(owner))
                .map(|d| d.count())
                .unwrap_or(0)
                >= MAX_SNAPSHOTS
        {
            bail!("too many restore points stored here; prune some first");
        }
        self.write(owner, &path, data, quota)
    }

    pub fn list_snapshots(&self, owner: &str) -> Vec<(String, Vec<u8>)> {
        let Ok(rd) = fs::read_dir(self.snap_dir(owner)) else {
            return vec![];
        };
        rd.flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                check_snap(&name).ok()?;
                Some((name, fs::read(e.path()).ok()?))
            })
            .collect()
    }

    pub fn delete_snapshot(&self, owner: &str, id: &str) -> Result<()> {
        check_snap(id)?;
        let p = self.snap_dir(owner).join(id);
        if let Ok(m) = fs::metadata(&p) {
            fs::remove_file(&p)?;
            self.adjust(owner, -(cost(m.len()) as i64));
        }
        Ok(())
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

    pub fn forget_owner(&self, owner: &str) -> Result<()> {
        check_owner(owner)?;
        self.usage.lock().unwrap().remove(owner);
        let p = self.root.join(owner);
        if p.exists() {
            fs::remove_dir_all(p)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn store(min_free: u64) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sdb-test-store-{}", rand::random::<u64>()));
        (Store::new(dir.clone(), min_free), dir)
    }

    #[test]
    fn chunks_round_trip_and_list() {
        let (s, dir) = store(0);
        s.put_chunk(OWNER, &[1; 32], b"ciphertext", 1 << 20)
            .unwrap();
        s.put_chunk(OWNER, &[1; 32], b"ignored: already stored", 1 << 20)
            .unwrap();
        assert_eq!(s.get_chunk(OWNER, &[1; 32]).unwrap(), b"ciphertext");
        assert_eq!(s.list_chunks(OWNER), vec![[1u8; 32]]);
        assert!(s.get_chunk(OWNER, &[2; 32]).is_none());
        s.delete_chunks(OWNER, &[[1; 32]]);
        assert!(s.list_chunks(OWNER).is_empty());
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
        assert!(
            s.get_chunk(OWNER, &[3; 32]).is_none(),
            "a damaged chunk must not be served"
        );
        assert_eq!(
            s.list_chunks(OWNER),
            vec![[4u8; 32]],
            "and it is removed so the owner sees it missing"
        );
        assert_eq!(s.scrub(), (1, 1));
        assert!(s.list_chunks(OWNER).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn quota_counts_every_file_as_at_least_a_block() {
        let (s, dir) = store(0);
        s.put_chunk(OWNER, &[1; 32], b"x", 8192).unwrap();
        s.put_chunk(OWNER, &[2; 32], b"y", 8192).unwrap();
        assert_eq!(s.usage(OWNER).unwrap(), 8192);
        let err = s
            .put_chunk(OWNER, &[3; 32], b"z", 8192)
            .unwrap_err()
            .to_string();
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
            assert!(
                s.put_snapshot(OWNER, bad, b"m", 1 << 20).is_err(),
                "{bad:?}"
            );
            assert!(s.delete_snapshot(OWNER, bad).is_err(), "{bad:?}");
        }
        s.put_snapshot(OWNER, "abc123", b"manifest", 1 << 20)
            .unwrap();
        assert_eq!(
            s.list_snapshots(OWNER),
            vec![("abc123".to_string(), b"manifest".to_vec())]
        );
        assert!(s.last_snapshot(OWNER).is_some());
        s.delete_snapshot(OWNER, "abc123").unwrap();
        assert!(s.list_snapshots(OWNER).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn low_disk_space_refuses_writes() {
        let (s, dir) = store(u64::MAX / 2);
        let err = s
            .put_chunk(OWNER, &[1; 32], b"x", 1 << 30)
            .unwrap_err()
            .to_string();
        assert!(err.contains("low on disk space"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }
}
