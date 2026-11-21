// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::{BackupSet, Config, Friend},
    crypto::{Id, Keys},
    net::Peer,
    node::{Node, Out},
    snapshot::{self, Entry, Kind, Manifest, human},
};
use anyhow::{Result, anyhow, bail};
use fastcdc::v2020::{FastCDC, StreamCDC};
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;

// Content-defined chunk sizes: an edit only disturbs the chunks around it.
const MIN_CHUNK: usize = 256 << 10;
const AVG_CHUNK: usize = 1 << 20;
const MAX_CHUNK: usize = 4 << 20;
const BATCH_BYTES: usize = 8 << 20;

#[derive(Clone)]
pub struct Excludes {
    globs: GlobSet,
    dirs: Vec<PathBuf>,
}

impl Excludes {
    pub fn new(cfg: &Config, home: &Path) -> Result<Self> {
        let mut b = GlobSetBuilder::new();
        for p in &cfg.exclude {
            b.add(Glob::new(p).map_err(|e| anyhow!("bad exclude pattern {p}: {e}"))?);
        }
        Ok(Excludes { globs: b.build()?, dirs: vec![home.to_path_buf(), cfg.store_dir(home)] })
    }

    pub fn is_excluded(&self, p: &Path) -> bool {
        self.dirs.iter().any(|d| p.starts_with(d)) || self.globs.is_match(p)
    }
}

struct Scan {
    entries: Vec<Entry>,
    index: Vec<Id>,
    files: u64,
    bytes: u64,
    new_bytes: u64,
    skipped: u64,
}

struct Scanner {
    keys: Arc<Keys>,
    known: Vec<Arc<HashSet<Id>>>,
    sent: HashSet<Id>,
    tx: mpsc::Sender<(Id, Vec<u8>)>,
    new_bytes: u64,
}

impl Scanner {
    fn chunk(&mut self, data: &[u8]) -> Result<Id> {
        let id = self.keys.chunk_id(data);
        if !self.sent.contains(&id) && !self.known.iter().all(|k| k.contains(&id)) {
            let blob = self.keys.seal(&id, data)?;
            self.new_bytes += blob.len() as u64;
            self.tx.blocking_send((id, blob)).map_err(|_| anyhow!("upload stopped"))?;
            self.sent.insert(id);
        }
        Ok(id)
    }

    fn have(&self, ids: &[Id]) -> bool {
        ids.iter().all(|id| self.sent.contains(id) || self.known.iter().all(|k| k.contains(id)))
    }
}

fn scan(roots: Vec<PathBuf>, ex: Excludes, prev: HashMap<Vec<u8>, Entry>, mut sc: Scanner, out: Out) -> Result<Scan> {
    let mut res = Scan { entries: vec![], index: vec![], files: 0, bytes: 0, new_bytes: 0, skipped: 0 };
    for root in &roots {
        let walk = walkdir::WalkDir::new(root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| !ex.is_excluded(e.path()));
        for e in walk {
            let (e, md) = match e.and_then(|e| e.metadata().map(|m| (e, m))) {
                Ok(x) => x,
                Err(err) => {
                    out.say(format!("  skipped: {err}"));
                    res.skipped += 1;
                    continue;
                }
            };
            let path = e.path().as_os_str().as_bytes().to_vec();
            let mtime = md.mtime() * 1_000_000_000 + md.mtime_nsec();
            let ft = md.file_type();
            let kind = if ft.is_dir() {
                Kind::Dir
            } else if ft.is_symlink() {
                match fs::read_link(e.path()) {
                    Ok(t) => Kind::Symlink(t.as_os_str().as_bytes().to_vec()),
                    Err(_) => continue,
                }
            } else if ft.is_file() {
                let reuse = prev.get(&path).and_then(|p| match &p.kind {
                    Kind::File { size, chunks }
                        if *size == md.size() && p.mtime == mtime && p.ino == md.ino() && sc.have(chunks) =>
                    {
                        Some(chunks.clone())
                    }
                    _ => None,
                });
                let (size, chunks) = match reuse {
                    Some(c) => (md.size(), c),
                    None => match read_file(e.path(), &mut sc) {
                        Ok(x) => x,
                        Err(err) if err.to_string() == "upload stopped" => return Err(err),
                        Err(err) => {
                            out.say(format!("  skipped {}: {err}", e.path().display()));
                            res.skipped += 1;
                            continue;
                        }
                    },
                };
                res.files += 1;
                res.bytes += size;
                Kind::File { size, chunks }
            } else {
                continue;
            };
            res.entries.push(Entry { path, mode: md.mode(), uid: md.uid(), gid: md.gid(), mtime, ino: md.ino(), kind });
        }
    }
    res.entries.sort_by(|a, b| a.path.cmp(&b.path));
    res.entries.dedup_by(|a, b| a.path == b.path);
    let buf = snapshot::encode_index(&res.entries)?;
    for piece in split_index(&buf) {
        res.index.push(sc.chunk(piece)?);
    }
    res.new_bytes = sc.new_bytes;
    Ok(res)
}

pub fn split_index(buf: &[u8]) -> Vec<&[u8]> {
    FastCDC::new(buf, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK).map(|c| &buf[c.offset..c.offset + c.length]).collect()
}

/// Re-reads local files that look unchanged since the last backup and compares
/// their fingerprints, to catch content that changed without the file being modified.
pub fn check_local(keys: &Keys, last: Vec<Entry>, out: &Out) -> (u64, u64) {
    let (mut checked, mut bad) = (0, 0);
    for e in last {
        let Kind::File { size, chunks } = &e.kind else {
            continue;
        };
        let path = e.path();
        let Ok(md) = fs::symlink_metadata(&path) else {
            continue;
        };
        if md.size() != *size || md.mtime() * 1_000_000_000 + md.mtime_nsec() != e.mtime || md.ino() != e.ino {
            continue;
        }
        let Ok(f) = fs::File::open(&path) else {
            continue;
        };
        let now: Option<Vec<Id>> = StreamCDC::new(f, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
            .map(|c| c.ok().map(|c| keys.chunk_id(&c.data)))
            .collect();
        checked += 1;
        match now {
            Some(ids) if ids == *chunks => {}
            Some(_) => {
                bad += 1;
                out.say(format!(
                    "  CORRUPT? {} no longer matches its backup although it was not modified",
                    path.display()
                ));
            }
            None => {
                bad += 1;
                out.say(format!("  UNREADABLE {}", path.display()));
            }
        }
    }
    (checked, bad)
}

fn read_file(path: &Path, sc: &mut Scanner) -> Result<(u64, Vec<Id>)> {
    let f = fs::File::open(path)?;
    let mut size = 0;
    let mut chunks = vec![];
    for c in StreamCDC::new(f, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK) {
        let c = c.map_err(|e| anyhow!("{e}"))?;
        size += c.data.len() as u64;
        chunks.push(sc.chunk(&c.data)?);
    }
    Ok((size, chunks))
}

struct Target {
    peer: Peer,
    known: Arc<HashSet<Id>>,
    added: Vec<Id>,
    snaps: Vec<(Vec<Id>, String, Vec<u8>)>,
    failed: Option<String>,
    full: bool,
}

async fn flush(targets: &mut [Target], batch: &mut Vec<(Id, Vec<u8>)>, out: &Out) -> Result<()> {
    let results = futures::future::join_all(targets.iter().map(|t| {
        let items: Vec<&(Id, Vec<u8>)> = batch.iter().filter(|(id, _)| !t.known.contains(id)).collect();
        async move {
            if t.failed.is_some() || items.is_empty() {
                return Ok(vec![]);
            }
            t.peer.put_chunks(&items).await?;
            Ok::<_, anyhow::Error>(items.iter().map(|(id, _)| *id).collect())
        }
    }))
    .await;
    for (t, r) in targets.iter_mut().zip(results) {
        match r {
            Ok(ids) => t.added.extend(ids),
            Err(e) => {
                t.full = format!("{e:#}").contains("quota exceeded");
                if !t.full {
                    out.say(format!("  {} dropped out: {e:#}", t.peer.friend.name));
                }
                t.failed = Some(format!("{e:#}"));
            }
        }
    }
    batch.clear();
    if targets.iter().all(|t| t.failed.is_some()) {
        bail!("no friend could accept the backup");
    }
    Ok(())
}

/// Backs up one set (or all of them). When a friend's quota is hit, trims the oldest
/// restore points stored with that friend and tries again.
pub async fn run(
    node: &Arc<Node>,
    tag: Option<String>,
    set: Option<&str>,
    only: Option<&HashSet<String>>,
    out: &Out,
) -> Result<()> {
    let _op = node.op.lock().await;
    let cfg = node.cfg();
    let sets: Vec<BackupSet> =
        cfg.sets.iter().filter(|s| set.is_none_or(|n| n == s.name) && !s.paths.is_empty()).cloned().collect();
    if sets.is_empty() {
        bail!("nothing to back up yet: use `sdb add <path>`");
    }
    let label = sets.len() > 1 || cfg.sets.len() > 1;
    let mut failed = None;
    for s in &sets {
        let prefix = if label { format!("[{}] ", s.name) } else { String::new() };
        node.set_running(&s.name, Some("checking files".into()));
        let res = run_set(node, tag.clone(), s, only, &prefix, out).await;
        node.set_running(&s.name, None);
        if let Err(e) = res {
            node.note(format!("{prefix}backup failed: {e:#}"));
            out.say(format!("{prefix}{e:#}"));
            failed = Some(e);
        }
    }
    match failed {
        Some(e) if sets.len() == 1 => Err(e),
        Some(_) => bail!("not every set could be backed up"),
        None => Ok(()),
    }
}

async fn run_set(
    node: &Arc<Node>,
    tag: Option<String>,
    set: &BackupSet,
    only: Option<&HashSet<String>>,
    prefix: &str,
    out: &Out,
) -> Result<()> {
    let mut skip = HashSet::new();
    for _ in 0..20 {
        let full = run_once(node, tag.clone(), set, &skip, only, prefix, out).await?;
        if full.is_empty() {
            return Ok(());
        }
        for f in full {
            out.say(format!("  {} is at the quota they gave you, removing your oldest restore points there", f.name));
            if !node.evict(&f, out).await.unwrap_or(false) {
                out.say(format!("  {}: quota is too small for even one restore point, skipping them", f.name));
                skip.insert(f.id);
            }
        }
    }
    bail!("could not make enough room within your friends' quotas")
}

async fn run_once(
    node: &Arc<Node>,
    tag: Option<String>,
    set: &BackupSet,
    skip: &HashSet<String>,
    only: Option<&HashSet<String>>,
    prefix: &str,
    out: &Out,
) -> Result<Vec<Friend>> {
    let cfg = node.cfg();
    let friends: Vec<_> = cfg
        .friends
        .iter()
        .filter(|f| set.sends_to(f) && !skip.contains(&f.id) && only.is_none_or(|o| o.contains(&f.id)))
        .cloned()
        .collect();
    if friends.is_empty() {
        bail!("no friends to back up to yet: use `sdb invite` or `sdb join`");
    }
    let mut targets = vec![];
    let mut full = vec![];
    for f in &friends {
        let t = async {
            let peer = node.peer_patient(f).await?;
            let (used, quota) = peer.usage().await?;
            if used > quota {
                full.push(f.clone());
                bail!("over quota");
            }
            let known = node.known(&peer, false).await?;
            let snaps = peer
                .list_snapshots()
                .await?
                .into_iter()
                .filter_map(|(id, blob)| Some((node.open_manifest(&id, &blob).ok()?.index, id, blob)))
                .collect();
            Ok::<_, anyhow::Error>(Target { peer, known, added: vec![], snaps, failed: None, full: false })
        };
        match t.await {
            Ok(t) => targets.push(t),
            Err(e) if e.to_string() == "over quota" => {}
            Err(e) => out.say(format!("  {} is unreachable: {e:#}", f.name)),
        }
    }
    if !full.is_empty() {
        return Ok(full);
    }
    if targets.is_empty() {
        bail!("none of your friends are reachable right now");
    }

    let (tx, mut rx) = mpsc::channel(8);
    let scanner = Scanner {
        keys: node.keys.clone(),
        known: targets.iter().map(|t| t.known.clone()).collect(),
        sent: HashSet::new(),
        tx,
        new_bytes: 0,
    };
    let prev: HashMap<Vec<u8>, Entry> =
        node.load_last_index(&set.name).into_iter().map(|e| (e.path.clone(), e)).collect();
    let ex = Excludes::new(&cfg, &node.home)?;
    let (roots, out2) = (set.paths.clone(), out.clone());
    let job = tokio::task::spawn_blocking(move || scan(roots, ex, prev, scanner, out2));

    let batch_limit =
        if cfg.upload_kbps > 0 { (cfg.upload_kbps as usize * 1024).clamp(64 << 10, BATCH_BYTES) } else { BATCH_BYTES };
    let (mut batch, mut size) = (vec![], 0);
    let (mut total, mut told) = (0u64, std::time::Instant::now());
    while let Some((id, blob)) = rx.recv().await {
        size += blob.len();
        total += blob.len() as u64;
        batch.push((id, blob));
        if told.elapsed().as_secs() >= 2 {
            told = std::time::Instant::now();
            out.say(format!("  ... {} uploaded so far", human(total)));
            node.set_running(&set.name, Some(format!("{} uploaded so far", human(total))));
        }
        if size >= batch_limit || batch.len() >= 512 {
            let began = std::time::Instant::now();
            if flush(&mut targets, &mut batch, out).await.is_err() {
                break;
            }
            if cfg.upload_kbps > 0 {
                // Hold the average upload rate to the limit; local drives do not count.
                let remote = targets.iter().filter(|t| t.failed.is_none() && t.peer.is_remote()).count() as u64;
                let owed = std::time::Duration::from_millis(size as u64 * remote * 1000 / (cfg.upload_kbps * 1024));
                tokio::time::sleep(owed.saturating_sub(began.elapsed())).await;
            }
            size = 0;
        }
    }
    let flushed = flush(&mut targets, &mut batch, out).await;
    let full: Vec<Friend> = targets.iter().filter(|t| t.full).map(|t| t.peer.friend.clone()).collect();
    if flushed.is_err() && !full.is_empty() {
        drop(rx);
        let _ = job.await;
        return Ok(full);
    }
    flushed?;
    let scan = job.await??;

    let clock = chrono::Utc::now();
    let manifest = Manifest {
        version: 1,
        time: clock.timestamp(),
        nanos: clock.timestamp_subsec_nanos(),
        host: cfg.name.clone(),
        roots: set.paths.iter().map(|p| p.display().to_string()).collect(),
        index: scan.index.clone(),
        files: scan.files,
        bytes: scan.bytes,
        new_bytes: scan.new_bytes,
        tag: tag.clone(),
        thinned: 0,
        writer: env!("CARGO_PKG_VERSION").into(),
        set: set.name.clone(),
    };
    // A friend that missed an identical restore point gets that one rather than a new id.
    let existing = match &tag {
        None => targets
            .iter()
            .flat_map(|t| &t.snaps)
            .find(|(idx, _, _)| *idx == scan.index)
            .map(|(_, id, blob)| (id.clone(), blob.clone())),
        Some(_) => None,
    };
    let (snap_id, sealed) = match existing {
        Some(x) => x,
        None => {
            let id = hex::encode(rand::random::<[u8; 8]>());
            let sealed = node.keys.seal_raw(id.as_bytes(), &snapshot::encode_manifest(&manifest)?)?;
            (id, sealed)
        }
    };
    let mut stored = vec![];
    for t in targets.iter_mut().filter(|t| t.failed.is_none()) {
        if tag.is_some() || !t.snaps.iter().any(|(idx, _, _)| *idx == scan.index) {
            match t.peer.put_snapshot(&snap_id, sealed.clone()).await {
                Ok(()) => stored.push(t.peer.friend.name.clone()),
                Err(e) => {
                    t.full = format!("{e:#}").contains("quota exceeded");
                    t.failed = Some(format!("{e:#}"));
                }
            }
        }
    }
    let full: Vec<Friend> = targets.iter().filter(|t| t.full).map(|t| t.peer.friend.clone()).collect();
    node.mark_set(
        &set.name,
        &targets.iter().filter(|t| t.failed.is_none()).map(|t| t.peer.friend.id.clone()).collect::<Vec<_>>(),
        scan.files,
        scan.bytes,
    );
    for t in targets {
        if t.failed.is_none() {
            let mut known = t.known;
            Arc::make_mut(&mut known).extend(t.added);
            node.set_known(&t.peer.friend.id, known).await;
        } else {
            node.drop_known(&t.peer.friend.id).await;
        }
    }
    node.save_index(&set.name, &scan.entries)?;
    let skipped = if scan.skipped > 0 { format!(", {} unreadable skipped", scan.skipped) } else { String::new() };
    if stored.is_empty() && scan.new_bytes > 0 {
        out.say(format!(
            "{prefix}no changes since the last restore point; re-sent {} that friends were missing",
            human(scan.new_bytes)
        ));
    } else if stored.is_empty() {
        out.say(format!(
            "{prefix}no changes since the last restore point ({} files, {}{skipped})",
            scan.files,
            human(scan.bytes)
        ));
    } else {
        let line = format!(
            "{prefix}restore point {snap_id} created on {}: {} files, {}, {} uploaded{skipped}",
            stored.join(", "),
            scan.files,
            human(scan.bytes),
            human(scan.new_bytes)
        );
        node.note(line.clone());
        out.say(line);
        *node.last_backup.lock().unwrap() = Some(snapshot::when(manifest.time));
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise, so chunk boundaries are realistic and the test is repeatable.
    fn noise(len: usize) -> Vec<u8> {
        let mut x = 0x9E3779B97F4A7C15u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 32) as u8
            })
            .collect()
    }

    #[test]
    fn index_pieces_rejoin() {
        let buf = noise(6_000_000);
        let pieces = split_index(&buf);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| p.len() >= MIN_CHUNK || std::ptr::eq(*p, *pieces.last().unwrap())));
        assert!(pieces.iter().all(|p| p.len() <= MAX_CHUNK));
        assert_eq!(pieces.concat(), buf);
    }

    #[test]
    fn an_edit_only_changes_nearby_chunks() {
        let a = noise(24_000_000);
        let mut b = a.clone();
        b.splice(12_000_000..12_000_000, *b"inserted bytes");
        let ids = |buf: &[u8]| -> HashSet<blake3::Hash> { split_index(buf).iter().map(|p| blake3::hash(p)).collect() };
        let (x, y) = (ids(&a), ids(&b));
        let changed = y.difference(&x).count();
        assert!(y.len() >= 8, "expected many chunks, got {}", y.len());
        assert!(changed <= 2, "{changed} of {} chunks changed after a small insert", y.len());
    }

    #[test]
    fn excludes() {
        let mut cfg = Config::new("n".into());
        cfg.exclude.push("**/node_modules".into());
        let ex = Excludes::new(&cfg, Path::new("/home/me/.super-duper-backup")).unwrap();
        assert!(ex.is_excluded(Path::new("/home/me/proj/node_modules")));
        assert!(ex.is_excluded(Path::new("/home/me/.DS_Store")));
        assert!(ex.is_excluded(Path::new("/home/me/.super-duper-backup/store/x")));
        assert!(!ex.is_excluded(Path::new("/home/me/proj/src/main.rs")));
        cfg.exclude.push("[".into());
        assert!(Excludes::new(&cfg, Path::new("/x")).is_err());
    }
}
