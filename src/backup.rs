// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::{BackupSet, Config, Friend},
    crypto::{Id, Keys},
    net::{self, Peer},
    node::{Node, Out},
    os,
    snapshot::{self, Entry, Kind, Manifest, human},
};
use anyhow::{Result, anyhow, bail};
use fastcdc::v2020::{FastCDC, StreamCDC};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsStr,
    fs,
    io::{Read, Seek},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

// Content-defined chunk sizes: an edit only disturbs the chunks around it.
const MIN_CHUNK: usize = 256 << 10;
const AVG_CHUNK: usize = 1 << 20;
pub const MAX_CHUNK: usize = 4 << 20;
const BATCH_BYTES: usize = 8 << 20;
/// Holes are skipped in whole zero chunks rather than read, so a sparse disk image costs
/// what it stores, not what it claims. Most empty space one file may claim: far beyond
/// any real disk image, but each 4 MiB of it still adds an id to the index.
const MAX_HOLES: u64 = 2 << 40;
/// Chunk ids one restore point may hold that cost no disk: zero chunks standing for holes,
/// and the chunk lists of further hard links to a file already listed. Past this (32 MiB of
/// index, 4 TiB of holes) files are left out, and said to be, rather than let a few
/// `truncate` or `ln` calls grow every index without bound. Files unchanged since the last
/// restore point are counted first, so new ones cannot push them out.
const RUN_FREE_IDS: u64 = 1 << 20;
/// Most files, folders and links one set's restore point lists, and most bytes of their names
/// (and link targets). Far beyond a real set, but a few empty files with long names cost
/// next to no disk and each would add its name to every index, so past this they are left
/// out, and said to be, rather than let an index grow without bound or past what a destination
/// takes (MAX_SNAPSHOT). Files unchanged since the last restore point are counted first.
const MAX_ENTRIES: u64 = 20_000_000;
const MAX_NAME_BYTES: u64 = 2 << 30;
/// Most empty space read as zeros in one file: holes too small to skip, and every hole
/// on a filesystem that cannot say where they are.
const MAX_ZEROS_READ: u64 = 16 << 30;
/// A run stops starting on changed files after this long, or after skipping this much
/// empty space, and leaves the rest to the next run so other work gets its turn.
const RUN_TIME: Duration = Duration::from_secs(30 * 60);
const RUN_HOLES: u64 = 16 << 40;
/// Chunks waiting for one destination. A destination that moves nothing for this long
/// while another sits idle is dropped from the run.
const LANE_DEPTH: usize = 16;
const LAG: Duration = Duration::from_secs(600);

/// Changed files a run left for later, per set, with the run that first left each one:
/// the next run starts with the longest waiting, so none waits forever.
static DEFERRED: Mutex<BTreeMap<String, HashMap<Vec<u8>, u64>>> = Mutex::new(BTreeMap::new());
static RUNS: AtomicU64 = AtomicU64::new(0);

/// Whether the last run of this set ran out of time and left changed files for the next.
pub fn deferred(set: &str) -> bool {
    DEFERRED.lock().unwrap().get(set).is_some_and(|m| !m.is_empty())
}

#[derive(Clone)]
pub struct Excludes {
    globs: GlobSet,
    dirs: Vec<PathBuf>,
}

impl Excludes {
    /// What one set leaves out: its exclusion list, what was left out of it by hand,
    /// and always cahoots's own folders.
    pub fn new(cfg: &Config, set: &BackupSet, home: &Path) -> Result<Self> {
        let mut b = GlobSetBuilder::new();
        let list = cfg.list_for(set);
        for p in list.patterns.iter().map(String::as_str).chain([crate::config::ALWAYS_EXCLUDED]) {
            // Windows takes `A` and `a` for one name, so a pattern does too.
            let glob = GlobBuilder::new(p).case_insensitive(cfg!(windows)).build();
            b.add(glob.map_err(|e| anyhow!("bad exclude pattern {p}: {e}"))?);
        }
        let mut dirs = vec![home.to_path_buf(), cfg.store_dir(home)];
        dirs.extend(set.skip.iter().cloned());
        Ok(Excludes { globs: b.build()?, dirs })
    }

    /// For a walk that never enters a folder it has left out.
    pub fn is_excluded(&self, p: &Path) -> bool {
        self.dirs.iter().any(|d| within(p, d, cfg!(windows))) || self.globs.is_match(p)
    }

    /// For a path met on its own, such as a file change below `root`: left out itself, or inside
    /// something the walk from `root` would skip. Folders above the root are not looked at, as
    /// the walk never meets them.
    pub fn covers(&self, p: &Path, root: &Path) -> bool {
        self.dirs.iter().any(|d| within(p, d, cfg!(windows)))
            || p.ancestors().take_while(|a| a.starts_with(root)).any(|a| self.globs.is_match(a))
    }
}

/// Whether `p` is `dir` or inside it. Windows takes `A` and `a` for one name (`caseless`), and
/// a folder typed as `d:\backups` is walked as `D:\Backups`.
fn within(p: &Path, dir: &Path, caseless: bool) -> bool {
    if !caseless {
        return p.starts_with(dir);
    }
    let fold = |c: std::path::Component| c.as_os_str().to_string_lossy().to_lowercase();
    let mut p = p.components();
    dir.components().all(|d| p.next().is_some_and(|c| fold(c) == fold(d)))
}

struct Scan {
    entries: Vec<Entry>,
    index: Vec<Id>,
    files: u64,
    bytes: u64,
    new_bytes: u64,
    skipped: u64,
    /// Files a cloud service keeps elsewhere, new or changed since they were last backed up:
    /// reading one would fetch it.
    away: u64,
    /// Files refused outright (sparse, or past RUN_FREE_IDS): data the user expects backed up
    /// but that is not.
    refused: u64,
    /// Changed files left for the next run.
    deferred: u64,
}

/// Where a file's chunks go: the upload while backing up, a fingerprint while checking.
trait Sink {
    fn data(&mut self, data: &[u8]) -> Result<Id>;
    /// A chunk of MAX_CHUNK zeros, hashed once rather than once per hole.
    fn zeros(&mut self) -> Result<Id>;
}

struct Scanner {
    keys: Arc<Keys>,
    known: Vec<Arc<HashSet<Id>>>,
    sent: HashSet<Id>,
    tx: mpsc::Sender<(Id, Vec<u8>)>,
    new_bytes: u64,
    zero: Option<Id>,
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

    /// Whether at least one destination of this run holds every one of `ids`.
    fn held_whole(&self, ids: &[Id]) -> bool {
        self.known.iter().any(|k| ids.iter().all(|id| self.sent.contains(id) || k.contains(id)))
    }
}

impl Sink for Scanner {
    fn data(&mut self, data: &[u8]) -> Result<Id> {
        self.chunk(data)
    }

    fn zeros(&mut self) -> Result<Id> {
        if let Some(id) = self.zero {
            return Ok(id);
        }
        let id = self.chunk(&vec![0; MAX_CHUNK])?;
        self.zero = Some(id);
        Ok(id)
    }
}

struct Fingerprint<'a> {
    keys: &'a Keys,
    zero: Option<Id>,
}

impl Sink for Fingerprint<'_> {
    fn data(&mut self, data: &[u8]) -> Result<Id> {
        Ok(self.keys.chunk_id(data))
    }

    fn zeros(&mut self) -> Result<Id> {
        Ok(*self.zero.get_or_insert_with(|| self.keys.chunk_id(&vec![0; MAX_CHUNK])))
    }
}

fn scan(
    set: String,
    roots: Vec<PathBuf>,
    ex: Excludes,
    prev: HashMap<Vec<u8>, Entry>,
    mut sc: Scanner,
    out: Out,
) -> Result<Scan> {
    let run = RUNS.fetch_add(1, Ordering::Relaxed);
    let mut res = Scan {
        entries: vec![],
        index: vec![],
        files: 0,
        bytes: 0,
        new_bytes: 0,
        skipped: 0,
        away: 0,
        refused: 0,
        deferred: 0,
    };
    let mut entries: Vec<Option<Entry>> = vec![];
    let mut free = Free::new(RUN_FREE_IDS, sc.keys.chunk_id(&vec![0; MAX_CHUNK]));
    let mut room = Room::new(MAX_ENTRIES, MAX_NAME_BYTES).keep(&prev);
    // The chunks of each file with more than one name, so a further link is not read again.
    let mut linked: HashMap<(u64, u64), (u64, Vec<Id>)> = HashMap::new();
    let refuse = |res: &mut Scan, path: &[u8]| {
        out.say(format!("  NOT BACKED UP {}: {FREE_FULL}", os::bytes_path(path).display()));
        res.refused += 1;
    };
    // Changed files (entry, device, inode, more than one name) are read after the walk, longest
    // waiting first.
    let mut todo: Vec<(usize, u64, u64, bool)> = vec![];
    for f in Walk::new(&roots, &ex) {
        let f = match f {
            Ok(f) => f,
            Err(err) => {
                out.say(format!("  skipped: {err}"));
                res.skipped += 1;
                continue;
            }
        };
        let (md, path) = (&f.md, os::path_bytes(&f.path));
        let named = path.len() + f.link.as_ref().map_or(0, |t| t.len());
        if !room.take(named, prev.contains_key(&path)) {
            if room.refused == 1 {
                out.say(format!("  NOT BACKED UP {}: {ROOM_FULL}", f.path.display()));
            }
            res.refused += 1;
            continue;
        }
        let kind = match f.ft {
            os::Kind::Dir => Kind::Dir,
            os::Kind::Link => match f.link {
                Some(t) => Kind::Symlink(t),
                None => continue,
            },
            os::Kind::File => {
                let link = (md.nlink > 1).then_some((md.dev, md.ino));
                let same = prev.get(&path).and_then(|p| match &p.kind {
                    Kind::File { size, chunks } if *size == md.size && p.mtime == md.mtime && p.ino == md.ino => {
                        Some(chunks)
                    }
                    _ => None,
                });
                // DECIDED, AFTER GOING BOTH WAYS: what an unchanged online-only file needs to
                // stay in the backup. It cannot be read again without fetching it, so the rule
                // for other files (every destination has every chunk, or it is read and sent
                // again) does not fit. Two rules were tried and each was a bug:
                //  - Needing every destination to hold it took every such file out of the new
                //    restore points, everywhere, the day a destination was added.
                //  - Needing nothing listed files that no destination of the run held (the
                //    one that did was removed), so a restore point promised what nothing
                //    could restore, and said nothing.
                // So: it stays if at least one destination of this run holds all of it. Where
                // another lacks chunks, `verify` (which the daemon runs) copies them over from
                // that one. If none holds all of it, it is left out and counted as not fetched.
                let reuse = same.filter(|c| if md.away { sc.held_whole(c) } else { sc.have(c) }).cloned();
                match reuse {
                    Some(chunks) => {
                        if !free.take(link, &chunks) {
                            refuse(&mut res, &path);
                            continue;
                        }
                        if let Some(l) = link {
                            linked.insert(l, (md.size, chunks.clone()));
                        }
                        res.files += 1;
                        res.bytes += md.size;
                        Kind::File { size: md.size, chunks }
                    }
                    None if md.away => {
                        res.away += 1;
                        continue;
                    }
                    None => {
                        todo.push((entries.len(), md.dev, md.ino, link.is_some()));
                        Kind::File { size: 0, chunks: vec![] }
                    }
                }
            }
            _ => continue,
        };
        entries.push(Some(Entry { path, mode: md.mode, uid: md.uid, gid: md.gid, mtime: md.mtime, ino: md.ino, kind }));
    }

    let waiting = DEFERRED.lock().unwrap().get(&set).cloned().unwrap_or_default();
    let path_of = |i: usize, entries: &[Option<Entry>]| entries[i].as_ref().map(|e| e.path.clone()).unwrap_or_default();
    todo.sort_by_key(|(i, ..)| waiting.get(&path_of(*i, &entries)).copied().unwrap_or(u64::MAX));
    let began = Instant::now();
    let (mut started, mut holes) = (false, 0u64);
    let mut left = HashMap::new();
    for (i, dev, ino, many) in todo {
        let (key, link) = (path_of(i, &entries), many.then_some((dev, ino)));
        // Another name of a file already read this run: the same chunks, without reading it again.
        if let Some((size, chunks)) = link.and_then(|l| linked.get(&l)) {
            if free.take(link, chunks) {
                res.files += 1;
                res.bytes += size;
                if let Some(e) = entries[i].as_mut() {
                    e.kind = Kind::File { size: *size, chunks: chunks.clone() };
                }
            } else {
                entries[i] = None;
                refuse(&mut res, &key);
            }
            continue;
        }
        // At least one file per run, so the longest waiting always gets its turn.
        if started && (began.elapsed() > RUN_TIME || holes > RUN_HOLES) {
            // Out of time: keep what the last restore point had and come back to it.
            left.insert(key.clone(), waiting.get(&key).copied().unwrap_or(run));
            res.deferred += 1;
            entries[i] =
                prev.get(&key).filter(|p| matches!(&p.kind, Kind::File { chunks, .. } if sc.have(chunks))).cloned();
            match entries[i].as_ref().map(|e| (e.size(), &e.kind)) {
                Some((size, Kind::File { chunks, .. })) if free.take(link, chunks) => {
                    res.files += 1;
                    res.bytes += size;
                }
                Some(_) => {
                    entries[i] = None;
                    refuse(&mut res, &key);
                }
                None => {}
            }
            continue;
        }
        started = true;
        let path = &os::bytes_path(&key);
        match read_file(path, dev, ino, &mut sc) {
            Ok((size, chunks, skipped)) => {
                holes += skipped;
                if let Some(l) = link {
                    linked.insert(l, (size, chunks.clone()));
                }
                if !free.take(link, &chunks) {
                    entries[i] = None;
                    refuse(&mut res, &key);
                    continue;
                }
                res.files += 1;
                res.bytes += size;
                if let Some(e) = entries[i].as_mut() {
                    e.kind = Kind::File { size, chunks };
                }
            }
            Err(err) if err.to_string() == "upload stopped" => return Err(err),
            Err(err) => {
                entries[i] = None;
                if err.to_string().starts_with("sparse file") {
                    out.say(format!("  NOT BACKED UP {}: {err}", path.display()));
                    res.refused += 1;
                } else {
                    out.say(format!("  skipped {}: {err}", path.display()));
                    res.skipped += 1;
                }
            }
        }
    }
    DEFERRED.lock().unwrap().insert(set, left);
    if room.refused > 1 {
        out.say(format!("  NOT BACKED UP: {} more files and folders, for the same reason", room.refused - 1));
    }

    res.entries = entries.into_iter().flatten().collect();
    res.entries.sort_by(|a, b| a.path.cmp(&b.path));
    res.entries.dedup_by(|a, b| a.path == b.path);
    let buf = snapshot::encode_index(&res.entries)?;
    for piece in split_index(&buf) {
        res.index.push(sc.chunk(piece)?);
    }
    res.new_bytes = sc.new_bytes;
    Ok(res)
}

const ROOM_FULL: &str = "this set already has as many files and folders as one restore point holds \
     (split it into smaller sets); it and others after it were left out";

/// Counts the entries a run lists, and the bytes of their names (see MAX_ENTRIES). Those that
/// were in the last restore point have room kept for them, so new ones cannot push them out.
struct Room {
    entries: u64,
    bytes: u64,
    /// Entries turned away so far.
    refused: u64,
}

impl Room {
    fn new(entries: u64, bytes: u64) -> Self {
        Room { entries, bytes, refused: 0 }
    }

    /// Keeps room for all the last restore point listed, before the walk meets any of it:
    /// taken off as they came, new entries met first would use up the room of known ones
    /// met later, which are let in all the same, and the set would outgrow its bounds.
    fn keep(mut self, prev: &HashMap<Vec<u8>, Entry>) -> Self {
        for e in prev.values() {
            let link = if let Kind::Symlink(t) = &e.kind { t.len() } else { 0 };
            self.entries = self.entries.saturating_sub(1);
            self.bytes = self.bytes.saturating_sub((e.path.len() + link) as u64 + 64);
        }
        self
    }

    /// Whether one more entry with `name` bytes of names fits. One that was in the last
    /// restore point is always let in: `keep` made room for it.
    fn take(&mut self, name: usize, known: bool) -> bool {
        if known {
            return true;
        }
        let name = name as u64 + 64;
        if self.entries == 0 || name > self.bytes {
            self.refused += 1;
            return false;
        }
        self.entries -= 1;
        self.bytes -= name;
        true
    }
}

const FREE_FULL: &str = "this set already has as many sparse files and extra hard links as one restore point holds";

/// Counts the index ids a run lets in that cost no disk (see RUN_FREE_IDS).
struct Free {
    left: u64,
    zero: Id,
    /// Files with more than one name, as (device, inode), that one name already paid for.
    listed: HashSet<(u64, u64)>,
}

impl Free {
    fn new(left: u64, zero: Id) -> Self {
        Free { left, zero, listed: HashSet::new() }
    }

    /// Whether one more file fits. Its first name costs its zero chunks, any further name of the
    /// same file (`link`) its whole chunk list.
    fn take(&mut self, link: Option<(u64, u64)>, chunks: &[Id]) -> bool {
        let again = link.is_some_and(|l| self.listed.contains(&l));
        let cost = if again { chunks.len() } else { chunks.iter().filter(|c| **c == self.zero).count() } as u64;
        if cost > self.left {
            return false;
        }
        self.left -= cost;
        if let Some(l) = link {
            self.listed.insert(l);
        }
        true
    }
}

/// One thing the walk found. `into` is the folder to go into, as (device, inode).
struct Found {
    path: PathBuf,
    ft: os::Kind,
    md: os::Meta,
    link: Option<Vec<u8>>,
    into: Option<(u64, u64)>,
}

/// Where the walk looks: at a root as the user named it, or at a name in an open folder.
#[derive(Clone, Copy)]
enum At<'a> {
    Root(&'a Path),
    In(&'a os::Dir, &'a OsStr),
}

impl At<'_> {
    fn stat(self, follow: bool) -> std::io::Result<os::Meta> {
        match self {
            At::Root(path) => os::stat(path, follow),
            At::In(dir, name) => dir.stat(name, follow),
        }
    }

    fn read_link(self) -> std::io::Result<Vec<u8>> {
        match self {
            At::Root(path) => os::read_link(path),
            At::In(dir, name) => dir.read_link(name),
        }
    }
}

/// Looks at one name without following it if it is a link (but into a root given as a link to
/// a folder, as the user named it).
fn found(at: At, path: PathBuf) -> Result<Found> {
    let md = at.stat(false).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let ft = md.kind;
    if ft == os::Kind::Link && matches!(at, At::Root(_)) {
        // A root named through a link is the folder it names, recorded as that folder: restore
        // then makes a folder there and puts what it holds inside, not a link it cannot enter.
        if let Some(md) = at.stat(true).ok().filter(|t| t.kind == os::Kind::Dir) {
            return Ok(Found { path, ft: os::Kind::Dir, into: Some((md.dev, md.ino)), md, link: None });
        }
    }
    let link = match ft {
        os::Kind::Link => at.read_link().ok(),
        _ => None,
    };
    let into = match ft {
        os::Kind::Dir => Some((md.dev, md.ino)),
        _ => None,
    };
    Ok(Found { path, ft, md, link, into })
}

/// Walks the roots in name order, leaving out what `ex` excludes and never following a link
/// below a root. Each folder is opened without following a link and checked to be the very one
/// its parent listed, and what it holds is looked at relative to that open folder, so a folder
/// swapped for a link mid-walk is never entered. One folder is open at a time.
struct Walk<'a> {
    ex: &'a Excludes,
    stack: Vec<std::vec::IntoIter<Result<Found>>>,
}

impl<'a> Walk<'a> {
    fn new(roots: &[PathBuf], ex: &'a Excludes) -> Self {
        let top: Vec<_> =
            roots.iter().filter(|r| !ex.is_excluded(r)).map(|r| found(At::Root(r.as_path()), r.clone())).collect();
        Walk { ex, stack: vec![top.into_iter()] }
    }

    fn list(&self, path: &Path, (dev, ino): (u64, u64), root: bool) -> Result<Vec<Result<Found>>> {
        let dir = os::Dir::open(path, root)?;
        let md = dir.meta()?;
        if (md.dev, md.ino) != (dev, ino) {
            bail!("it was replaced while being backed up");
        }
        let mut names = dir.names()?;
        names.sort();
        Ok(names
            .iter()
            .filter_map(|n| {
                let p = path.join(n);
                (!self.ex.is_excluded(&p)).then(|| found(At::In(&dir, n), p))
            })
            .collect())
    }
}

impl Iterator for Walk<'_> {
    type Item = Result<Found>;

    fn next(&mut self) -> Option<Result<Found>> {
        loop {
            let root = self.stack.len() == 1;
            let f = match self.stack.last_mut()?.next() {
                Some(Ok(f)) => f,
                Some(Err(e)) => return Some(Err(e)),
                None => {
                    self.stack.pop();
                    continue;
                }
            };
            if let Some(into) = f.into {
                match self.list(&f.path, into, root) {
                    Ok(inside) => self.stack.push(inside.into_iter()),
                    // A folder that cannot be read, or was swapped, is skipped with all it holds.
                    Err(e) => return Some(Err(anyhow!("{}: {e}", f.path.display()))),
                }
            }
            return Some(Ok(f));
        }
    }
}

pub fn split_index(buf: &[u8]) -> Vec<&[u8]> {
    FastCDC::new(buf, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK).map(|c| &buf[c.offset..c.offset + c.length]).collect()
}

/// Re-reads local files that look unchanged since the last backup and compares
/// their fingerprints, to catch content that changed without the file being modified.
pub fn check_local(keys: &Keys, last: Vec<Entry>, out: &Out) -> (u64, u64) {
    let (mut checked, mut bad) = (0, 0);
    let mut fp = Fingerprint { keys, zero: None };
    for e in last {
        let Kind::File { size, chunks } = &e.kind else {
            continue;
        };
        let path = e.path();
        let Ok(md) = os::stat(&path, false) else {
            continue;
        };
        if md.size != *size || md.mtime != e.mtime || md.ino != e.ino || md.away {
            continue;
        }
        let Ok((f, len)) = open_scanned(&path, md.dev, md.ino) else {
            continue;
        };
        checked += 1;
        match chunk_file(&f, len, &mut fp) {
            Ok((_, ids, _)) if ids == *chunks => {}
            // A sparse file backed up before holes were skipped was read in full then: compare
            // it that way, or, when that would mean reading a huge hole, leave it unchecked.
            Ok((_, _, skipped)) if skipped > 0 && len > MAX_ZEROS_READ => checked -= 1,
            Ok((_, _, skipped)) if skipped > 0 && plain_ids(&f, len, &mut fp).is_ok_and(|p| p == *chunks) => {}
            Ok(_) => {
                bad += 1;
                out.say(format!(
                    "  CORRUPT? {} no longer matches its backup although it was not modified",
                    path.display()
                ));
            }
            Err(err) => {
                bad += 1;
                out.say(format!("  UNREADABLE {}: {err}", path.display()));
            }
        }
    }
    (checked, bad)
}

/// Opens the file the scan just looked at. A link, pipe or different file swapped in
/// since then is refused rather than read, and a pipe cannot make the open wait.
fn open_scanned(path: &Path, dev: u64, ino: u64) -> Result<(fs::File, u64)> {
    let f = os::open_file(path)?;
    let md = os::file_meta(&f)?;
    if md.kind != os::Kind::File || md.dev != dev || md.ino != ino {
        bail!("it was replaced while being backed up");
    }
    Ok((f, md.size))
}

/// The stretches of a file to skip rather than read, as (start, length) in whole zero
/// chunks. What is left of each hole, and holes smaller than a chunk, are read as zeros.
fn holes(f: &fs::File, len: u64) -> Result<Vec<(u64, u64)>> {
    let unknown = || -> Result<Vec<(u64, u64)>> {
        // The filesystem cannot say where its holes are, so all of them would be read.
        let empty = len.saturating_sub(os::allocated(f)?);
        if empty > MAX_ZEROS_READ {
            bail!("sparse file with {} of empty space on a filesystem that cannot skip it", human(empty));
        }
        Ok(vec![])
    };
    let (mut out, mut pos, mut skip, mut read) = (vec![], 0u64, 0u64, 0u64);
    while pos < len {
        let hole = match os::seek_hole(f, pos) {
            Ok(h) => h.clamp(pos, len),
            Err(_) => return unknown(),
        };
        if hole >= len {
            break;
        }
        let data = match os::seek_data(f, hole) {
            Ok(Some(d)) => d.clamp(hole + 1, len),
            Ok(None) => len,
            Err(_) => return unknown(),
        };
        let whole = (data - hole) / MAX_CHUNK as u64 * MAX_CHUNK as u64;
        if whole > 0 {
            out.push((hole, whole));
        }
        skip += whole;
        read += data - hole - whole;
        if skip > MAX_HOLES {
            bail!("sparse file claiming over {} of empty space, not backed up", human(MAX_HOLES));
        }
        if read > MAX_ZEROS_READ {
            bail!("sparse file with too many small holes to read, not backed up");
        }
        pos = data;
    }
    Ok(out)
}

/// Splits an open file into chunks without reading its holes. A file without holes
/// chunks exactly as a plain read would, so its chunks match earlier backups.
/// Returns the size covered, the chunk ids and how much empty space was skipped.
fn chunk_file(f: &fs::File, len: u64, sink: &mut dyn Sink) -> Result<(u64, Vec<Id>, u64)> {
    let (mut size, mut chunks, mut skipped, mut pos) = (0u64, vec![], 0u64, 0u64);
    for (start, n) in holes(f, len)?.into_iter().chain([(len, 0)]) {
        if start > pos {
            let mut r = f;
            r.seek(std::io::SeekFrom::Start(pos))?;
            let mut got = 0;
            // Never read past the size seen at open, however fast the file grows.
            for c in StreamCDC::new(r.take(start - pos), MIN_CHUNK, AVG_CHUNK, MAX_CHUNK) {
                let c = c.map_err(|e| anyhow!("{e}"))?;
                got += c.data.len() as u64;
                chunks.push(sink.data(&c.data)?);
            }
            size += got;
            if got < start - pos {
                // It shrank while being read: keep what was there.
                break;
            }
        }
        for _ in 0..n / MAX_CHUNK as u64 {
            chunks.push(sink.zeros()?);
        }
        size += n;
        skipped += n;
        pos = start + n;
    }
    Ok((size, chunks, skipped))
}

/// The chunks a plain read of the whole file gives, holes and all, as backups made before
/// holes were skipped recorded them.
fn plain_ids(f: &fs::File, len: u64, sink: &mut dyn Sink) -> Result<Vec<Id>> {
    let mut r = f;
    r.seek(std::io::SeekFrom::Start(0))?;
    StreamCDC::new(r.take(len), MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
        .map(|c| sink.data(&c.map_err(|e| anyhow!("{e}"))?.data))
        .collect()
}

fn read_file(path: &Path, dev: u64, ino: u64, sc: &mut Scanner) -> Result<(u64, Vec<Id>, u64)> {
    let (f, len) = open_scanned(path, dev, ino)?;
    chunk_file(&f, len, sc)
}

/// A sealed chunk on its way to several destinations, held once.
type Shared = Arc<(Id, Vec<u8>)>;

struct Target {
    peer: Peer,
    known: Arc<HashSet<Id>>,
    added: Vec<Id>,
    /// The newest restore point of this set that the destination holds.
    newest: Option<Newest>,
    failed: Option<String>,
    full: bool,
}

struct Newest {
    at: (i64, u32),
    index: Vec<Id>,
    id: String,
    blob: Vec<u8>,
}

/// One destination's share of the upload, taken at its own pace so that a slow
/// destination does not hold every batch of the others.
async fn lane(
    peer: Peer,
    mut rx: mpsc::Receiver<Shared>,
    limit: usize,
    kbps: u64,
    remote: u64,
    done: Arc<AtomicU64>,
) -> Result<Vec<Id>> {
    let mut added = vec![];
    let mut open = true;
    while open {
        let (mut batch, mut size) = (vec![], 0);
        while size < limit && batch.len() < 512 {
            match rx.recv().await {
                Some(item) => {
                    size += item.1.len();
                    batch.push(item);
                }
                None => {
                    open = false;
                    break;
                }
            }
        }
        if batch.is_empty() {
            break;
        }
        let began = Instant::now();
        let items: Vec<&(Id, Vec<u8>)> = batch.iter().map(|i| &**i).collect();
        // Each piece the friend reads counts as progress, so a slow but steady friend is never
        // taken for a stuck one, however long one batch takes it.
        peer.put_chunks_ticking(&items, Some(done.clone())).await?;
        added.extend(batch.iter().map(|i| i.0));
        done.fetch_add(1, Ordering::Relaxed);
        if kbps > 0 && peer.is_remote() {
            // Hold the average upload rate, shared by the remote destinations, to the limit;
            // local drives do not count.
            let owed = Duration::from_millis(size as u64 * remote * 1000 / (kbps * 1024));
            tokio::time::sleep(owed.saturating_sub(began.elapsed())).await;
        }
    }
    Ok(added)
}

struct Lane {
    tx: Option<mpsc::Sender<Shared>>,
    task: tokio::task::JoinHandle<Result<Vec<Id>>>,
    done: Arc<AtomicU64>,
}

/// A restore point's record no destination would take is not sent, and the run says why
/// rather than leave every destination to refuse it.
fn fits(sealed: &[u8]) -> Result<()> {
    if sealed.len() > crate::proto::MAX_SNAPSHOT {
        bail!(
            "this set's restore point record would be {}, more than the {} a destination takes: \
             the set lists too many files and folders; split it into smaller sets",
            human(sealed.len() as u64),
            human(crate::proto::MAX_SNAPSHOT as u64)
        );
    }
    Ok(())
}

/// How a destination given up on for taking too long is named (see `net::in_time`).
const TOO_SLOW: &str = "is too slow";
const TOO_SLOW_LEFT_OUT: &str = "is too slow and was left out of this backup; it catches up next time";

fn too_slow(t: &mut Target, out: &Out) {
    out.say(format!("  {} {TOO_SLOW_LEFT_OUT}", t.peer.friend.name));
    t.failed = Some("too slow".into());
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
        bail!("nothing to back up yet: use `cahoots add <path>`");
    }
    let label = sets.len() > 1 || cfg.sets.len() > 1;
    let mut failed = None;
    for s in &sets {
        let prefix = if label { format!("[{}] ", s.name) } else { String::new() };
        node.set_running(&s.name, Some("checking files".into()));
        let res = match check_roots(node, &cfg, s).await {
            Ok(()) => run_set(node, tag.clone(), s, only, &prefix, out).await,
            Err(e) => Err(e),
        };
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

/// A folder that is not there, or cannot be read at all, is not backed up as an empty one:
/// with its disk unplugged, or on a new computer set up from the recovery kit, that would
/// make the newest restore point one with nothing in it. The set waits instead.
async fn check_roots(node: &Arc<Node>, cfg: &Config, set: &BackupSet) -> Result<()> {
    let (ex, roots) = (Excludes::new(cfg, set, &node.home)?, set.paths.clone());
    let looked = tokio::task::spawn_blocking(move || {
        for root in roots.iter().filter(|r| !ex.is_excluded(r)) {
            // A root that is itself a link to nothing is recorded as that link.
            let seen = fs::symlink_metadata(root).and_then(|md| match fs::metadata(root) {
                Ok(to) if to.is_dir() => fs::read_dir(root)?.next().transpose().map(|_| ()),
                Err(e) if !md.is_symlink() => Err(e),
                _ => Ok(()),
            });
            if let Err(e) = seen {
                bail!(
                    "{0} is missing or cannot be read ({e}). Nothing was backed up, so that its files are not \
                     recorded as deleted. If it is gone for good, run `cahoots rm {0}`",
                    root.display()
                );
            }
        }
        Ok(())
    });
    looked.await?
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
            let whose = if f.cloud.is_some() { "the cap you set" } else { "the quota they gave you" };
            out.say(format!("  {} is at {whose}, removing your oldest restore points there", f.name));
            if !node.evict(&f, out).await.unwrap_or(false) {
                out.say(format!("  {}: quota is too small for even one restore point, skipping them", f.name));
                skip.insert(f.id);
            }
        }
    }
    bail!("could not make enough room within your friends' quotas")
}

/// Restore points deleted on purpose, as kept by the node: an id everywhere, or
/// "destination|id" at one place.
fn forgotten(home: &Path) -> HashSet<String> {
    fs::read(home.join("forgotten.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// What asking a destination before a backup found.
enum Probed {
    Ready(Box<Target>),
    Full,
    /// A friend who takes no backups from this node.
    Closed,
    /// One that took longer to answer than a destination may, and is left out of this run.
    Slow,
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
        bail!("no friends to back up to yet: use `cahoots invite` or `cahoots join`");
    }
    // A forgotten restore point is never reused for a new backup: the next listing deletes
    // it again, which would silently take the newest backup with it.
    let gone = &forgotten(&node.home);
    let mut targets = vec![];
    let mut full = vec![];
    // All destinations are contacted at once, so a slow or silent one does not hold up the rest.
    let names = &cfg;
    // Each in bounded time, by what this node expects to hear rather than by what the
    // destination sends: one that trickles its answers is left out, as a lane that does is.
    let probes = futures::future::join_all(friends.iter().map(|f| async move {
        let ask = async {
            let peer = node.peer_patient(f).await?;
            let closed = !peer.accepts().await;
            node.set_closed(&f.id, closed);
            if closed {
                return Ok(Err(Probed::Closed));
            }
            let (used, quota) = peer.usage().await?;
            Ok::<_, anyhow::Error>(if used > quota { Err(Probed::Full) } else { Ok(peer) })
        };
        // Only a friend's answers are short. A drive's first count reads every file on it, and
        // cloud storage's lists the whole bucket under a deadline of its own.
        let asked = if f.is_friend() { tokio::time::timeout(net::ask_time(), ask).await } else { Ok(ask.await) };
        let peer = match asked {
            Err(_) => return Ok(Probed::Slow),
            Ok(r) => match r? {
                Ok(peer) => peer,
                Err(probed) => return Ok(probed),
            },
        };
        // Its listing is bounded by the chunks this node knows of (see `Node::known`).
        let known = match node.known(&peer, false).await {
            Err(e) if format!("{e:#}").contains(TOO_SLOW) => return Ok(Probed::Slow),
            r => r?,
        };
        let Ok(listed) = tokio::time::timeout(net::snapshots_time(), peer.list_snapshots()).await else {
            return Ok(Probed::Slow);
        };
        let newest = listed?
            .into_iter()
            .filter(|(id, _)| !gone.contains(id) && !gone.contains(&format!("{}|{id}", f.id)))
            .filter_map(|(id, blob)| {
                let m = node.open_manifest(&id, &blob).ok()?;
                (names.set_name_now(&m.set) == set.name).then_some(Newest {
                    at: (m.time, m.nanos),
                    index: m.index,
                    id,
                    blob,
                })
            })
            .max_by_key(|n| n.at);
        Ok::<_, anyhow::Error>(Probed::Ready(Box::new(Target {
            peer,
            known,
            added: vec![],
            newest,
            failed: None,
            full: false,
        })))
    }))
    .await;
    let mut closed = 0;
    for (f, probe) in friends.iter().zip(probes) {
        match probe {
            Ok(Probed::Ready(t)) => targets.push(*t),
            Ok(Probed::Full) => full.push(f.clone()),
            Ok(Probed::Closed) => {
                closed += 1;
                out.say(format!("  {} does not accept backups from this computer: nothing is sent there", f.name));
            }
            Ok(Probed::Slow) => out.say(format!("  {} {TOO_SLOW_LEFT_OUT}", f.name)),
            Err(e) => out.say(format!("  {} is unreachable: {e:#}", f.name)),
        }
    }
    if !full.is_empty() {
        return Ok(full);
    }
    if targets.is_empty() && closed == friends.len() {
        // Nothing to do and nothing wrong here: the warnings say where no backup arrives.
        return Ok(vec![]);
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
        zero: None,
    };
    let prev: HashMap<Vec<u8>, Entry> =
        node.load_last_index(&set.name).into_iter().map(|e| (e.path.clone(), e)).collect();
    let ex = Excludes::new(&cfg, set, &node.home)?;
    let (name, roots, out2) = (set.name.clone(), set.paths.clone(), out.clone());
    let job = tokio::task::spawn_blocking(move || scan(name, roots, ex, prev, scanner, out2));

    let batch_limit =
        if cfg.upload_kbps > 0 { (cfg.upload_kbps as usize * 1024).clamp(64 << 10, BATCH_BYTES) } else { BATCH_BYTES };
    let remote = targets.iter().filter(|t| t.peer.is_remote()).count() as u64;
    let mut lanes: Vec<Lane> = targets
        .iter()
        .map(|t| {
            let (ltx, lrx) = mpsc::channel(LANE_DEPTH);
            let done = Arc::new(AtomicU64::new(0));
            let (peer, out, kbps, d) = (t.peer.clone(), out.clone(), cfg.upload_kbps, done.clone());
            let task = tokio::spawn(async move {
                let name = peer.friend.name.clone();
                let r = lane(peer, lrx, batch_limit, kbps, remote, d).await;
                if let Err(e) = &r {
                    if !format!("{e:#}").contains("quota exceeded") {
                        out.say(format!("  {name} dropped out: {e:#}"));
                    }
                }
                r
            });
            Lane { tx: Some(ltx), task, done }
        })
        .collect();
    let (mut total, mut told) = (0u64, Instant::now());
    while let Some((id, blob)) = rx.recv().await {
        total += blob.len() as u64;
        if told.elapsed().as_secs() >= 2 {
            told = Instant::now();
            out.say(format!("  ... {} uploaded so far", human(total)));
            node.set_running(&set.name, Some(format!("{} uploaded so far", human(total))));
        }
        let item = Arc::new((id, blob));
        for i in 0..lanes.len() {
            if targets[i].known.contains(&item.0) {
                continue;
            }
            while let Some(ltx) = lanes[i].tx.clone() {
                let mark = lanes[i].done.load(Ordering::Relaxed);
                match tokio::time::timeout(LAG, ltx.send(item.clone())).await {
                    Ok(Ok(())) => break,
                    // It stopped with an error; its result says which.
                    Ok(Err(_)) => lanes[i].tx = None,
                    Err(_) => {
                        // Waiting on one destination that moved nothing all that time, while
                        // another has nothing to do.
                        let stuck = lanes[i].done.load(Ordering::Relaxed) == mark;
                        let idle = stuck
                            && lanes.iter().enumerate().any(|(j, l)| {
                                j != i && l.tx.as_ref().is_some_and(|t| t.capacity() == t.max_capacity())
                            });
                        if idle {
                            lanes[i].tx = None;
                            lanes[i].task.abort();
                            too_slow(&mut targets[i], out);
                        }
                    }
                }
            }
        }
        if lanes.iter().all(|l| l.tx.is_none()) {
            break;
        }
    }
    // The rest finish what they were given. One that makes no progress for a long time
    // while another has finished is dropped, rather than holding up the restore point.
    let mut live = vec![];
    for (i, l) in lanes.into_iter().enumerate() {
        drop(l.tx);
        live.push((i, l.task, l.done));
    }
    let mut results: Vec<Option<Result<Vec<Id>>>> = targets.iter().map(|_| None).collect();
    while !live.is_empty() {
        let marks: Vec<u64> = live.iter().map(|(_, _, d)| d.load(Ordering::Relaxed)).collect();
        let next = tokio::time::timeout(LAG, futures::future::select_all(live.iter_mut().map(|(_, h, _)| h))).await;
        match next {
            Ok((r, k, rest)) => {
                drop(rest);
                let (i, _, _) = live.swap_remove(k);
                results[i] = Some(r.unwrap_or_else(|e| Err(anyhow!("{e}"))));
            }
            Err(_) => {
                let finished = results.iter().any(|r| matches!(r, Some(Ok(_))));
                let mut k = 0;
                live.retain(|(i, h, d)| {
                    let stuck = finished && d.load(Ordering::Relaxed) == marks[k];
                    k += 1;
                    if stuck {
                        h.abort();
                        too_slow(&mut targets[*i], out);
                    }
                    !stuck
                });
            }
        }
    }
    for (t, r) in targets.iter_mut().zip(results) {
        match r {
            _ if t.failed.is_some() => {}
            Some(Ok(ids)) => t.added = ids,
            Some(Err(e)) => {
                t.full = format!("{e:#}").contains("quota exceeded");
                t.failed = Some(format!("{e:#}"));
            }
            None => t.failed = Some("did not finish".into()),
        }
    }
    let full: Vec<Friend> = targets.iter().filter(|t| t.full).map(|t| t.peer.friend.clone()).collect();
    if targets.iter().all(|t| t.failed.is_some()) {
        drop(rx);
        if !full.is_empty() {
            let _ = job.await;
            return Ok(full);
        }
        bail!("no friend could accept the backup");
    }
    let scan = job.await??;

    let clock = chrono::Utc::now();
    let manifest = Manifest {
        version: 1,
        time: clock.timestamp(),
        nanos: clock.timestamp_subsec_nanos(),
        host: cfg.name.clone(),
        roots: set.paths.iter().map(|p| String::from_utf8_lossy(&os::path_bytes(p)).into_owned()).collect(),
        index: scan.index.clone(),
        files: scan.files,
        bytes: scan.bytes,
        new_bytes: scan.new_bytes,
        tag: tag.clone(),
        thinned: 0,
        writer: env!("CARGO_PKG_VERSION").into(),
        set: set.name.clone(),
    };
    // A destination is up to date only when its newest restore point of this set is this
    // very state: files put back the way they were once still make a new restore point.
    let needy: Vec<usize> = (0..targets.len())
        .filter(|&i| {
            let t = &targets[i];
            t.failed.is_none() && (tag.is_some() || t.newest.as_ref().is_none_or(|n| n.index != scan.index))
        })
        .collect();
    // A destination that missed an identical restore point gets that one rather than a new
    // id, provided it would be the newest there too and was not dropped there on purpose.
    let existing = match &tag {
        None => targets
            .iter()
            .filter_map(|t| t.newest.as_ref())
            .filter(|n| n.index == scan.index)
            .max_by_key(|n| n.at)
            .filter(|n| {
                needy.iter().all(|&i| {
                    let t = &targets[i];
                    t.newest.as_ref().is_none_or(|o| o.at < n.at)
                        && !gone.contains(&format!("{}|{}", t.peer.friend.id, n.id))
                })
            })
            .map(|n| (n.id.clone(), n.blob.clone())),
        Some(_) => None,
    };
    let (snap_id, sealed) = match existing {
        Some(x) => x,
        None => {
            let id = hex::encode(rand::random::<[u8; 8]>());
            let sealed = node.keys.seal_raw(id.as_bytes(), &snapshot::encode_manifest(&manifest)?)?;
            fits(&sealed)?;
            (id, sealed)
        }
    };
    let mut stored = vec![];
    for i in needy {
        let t = &mut targets[i];
        match t.peer.put_snapshot(&snap_id, sealed.clone()).await {
            Ok(()) => stored.push(t.peer.friend.name.clone()),
            Err(e) => {
                t.full = format!("{e:#}").contains("quota exceeded");
                t.failed = Some(format!("{e:#}"));
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
    let mut skipped = String::new();
    if scan.skipped > 0 {
        skipped += &format!(", {} unreadable skipped", scan.skipped);
    }
    if scan.away > 0 {
        skipped += &format!(", {} online-only files not fetched to back up", scan.away);
    }
    if scan.refused > 0 {
        skipped += &format!(", {} files NOT backed up", scan.refused);
    }
    if scan.deferred > 0 {
        skipped += &format!(", {} changed files left for the next run", scan.deferred);
    }
    if stored.is_empty() && scan.new_bytes > 0 {
        out.say(format!(
            "{prefix}no changes since the last restore point; re-sent {} that friends were missing{skipped}",
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
    if scan.refused > 0 {
        // Data the user expects to be safe is not: say so where it will be seen, not just in passing.
        let line = format!(
            "{prefix}WARNING: {} files are NOT in this restore point (marked NOT BACKED UP above)",
            scan.refused
        );
        node.note(line.clone());
        out.say(line);
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn an_online_only_file_needs_one_destination_that_holds_all_of_it() {
        let (tx, _rx) = mpsc::channel(1);
        let (a, b) = ([1; 32], [2; 32]);
        let known = |ids: &[Id]| Arc::new(ids.iter().copied().collect::<HashSet<Id>>());
        let keys = Arc::new(Keys::new(&[7; 32]));
        let mut sc = Scanner {
            keys,
            known: vec![known(&[a, b]), known(&[a])],
            sent: HashSet::new(),
            tx,
            new_bytes: 0,
            zero: None,
        };
        assert!(sc.held_whole(&[a, b]) && !sc.have(&[a, b]));
        // Spread over two destinations, neither with all of it, it is not held.
        sc.known = vec![known(&[a]), known(&[b])];
        assert!(!sc.held_whole(&[a, b]));
        sc.known = vec![];
        assert!(!sc.held_whole(&[a]));
    }

    #[test]
    fn its_own_folders_are_left_out_however_their_names_are_typed() {
        let (dir, typed) = (Path::new("/Data/Backups"), Path::new("/data/BACKUPS/aa/chunk"));
        assert!(within(Path::new("/Data/Backups/aa"), dir, false) && within(dir, dir, false));
        assert!(!within(typed, dir, false));
        assert!(within(typed, dir, true) && within(Path::new("/DATA/backups"), dir, true));
        // A name that only starts the same is another folder, and so is the one above.
        assert!(!within(Path::new("/data/backups2/x"), dir, true) && !within(Path::new("/data"), dir, true));
    }

    #[test]
    fn a_set_lists_only_so_many_entries_and_names() {
        let mut r = Room::new(3, 1000);
        assert!(r.take(10, false) && r.take(10, false) && r.take(10, false));
        assert!(!r.take(10, false), "a fourth new entry");
        // One the last restore point had still gets in.
        assert!(r.take(10, true));
        assert_eq!(r.refused, 1);
        // Long names use up the room as surely as many entries do.
        let mut r = Room::new(100, 1000);
        assert!(r.take(800, false));
        assert!(!r.take(800, false));
        assert!(r.take(10, false));
        // What the last restore point listed has its room kept, wherever the walk meets it:
        // new entries met first do not get it as well.
        let known = |p: &str| {
            let path = p.as_bytes().to_vec();
            (path.clone(), Entry { path, mode: 0, uid: 0, gid: 0, mtime: 0, ino: 0, kind: Kind::Dir })
        };
        let mut r = Room::new(3, 1000).keep(&HashMap::from([known("/z/a"), known("/z/b")]));
        assert!(r.take(10, false));
        assert!(!r.take(10, false), "a second new entry, with two known still to come");
        assert!(r.take(4, true) && r.take(4, true));
    }

    #[test]
    fn a_record_too_large_for_a_destination_is_not_sent() {
        assert!(fits(&vec![0; crate::proto::MAX_SNAPSHOT]).is_ok());
        let err = fits(&vec![0; crate::proto::MAX_SNAPSHOT + 1]).unwrap_err();
        assert!(format!("{err:#}").contains("split it into smaller sets"), "{err:#}");
    }

    #[cfg(unix)]
    #[test]
    fn a_swapped_or_sparse_file_is_not_read() {
        let dir = std::env::temp_dir().join(format!("sdb-test-scan-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("secret"), "owner only").unwrap();
        fs::write(dir.join("plain"), "hello").unwrap();
        let seen = fs::symlink_metadata(dir.join("plain")).unwrap();
        assert_eq!(open_scanned(&dir.join("plain"), seen.dev(), seen.ino()).unwrap().1, 5);
        // The file is replaced by a link to something else after the scan looked at it.
        fs::remove_file(dir.join("plain")).unwrap();
        std::os::unix::fs::symlink(dir.join("secret"), dir.join("plain")).unwrap();
        assert!(open_scanned(&dir.join("plain"), seen.dev(), seen.ino()).is_err());
        // Or by a different regular file.
        fs::remove_file(dir.join("plain")).unwrap();
        fs::rename(dir.join("secret"), dir.join("plain")).unwrap();
        fs::write(dir.join("other"), "x").unwrap();
        let other = fs::symlink_metadata(dir.join("other")).unwrap();
        assert!(open_scanned(&dir.join("plain"), other.dev(), other.ino()).is_err());
        // A hole beyond any real disk image is refused before anything is read.
        let sparse = fs::File::create(dir.join("sparse")).unwrap();
        if sparse.set_len(MAX_HOLES + (1 << 30)).is_ok() {
            let err = holes(&sparse, MAX_HOLES + (1 << 30)).unwrap_err();
            assert!(err.to_string().starts_with("sparse file"), "{err}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_walk_never_leaves_the_set() {
        let dir = std::env::temp_dir().join(format!("sdb-test-walk-{}", rand::random::<u64>()));
        let (root, outside) = (dir.join("root"), dir.join("outside"));
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("a/x"), "x").unwrap();
        fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("b")).unwrap();
        std::os::unix::fs::symlink(&root, dir.join("via")).unwrap();
        let ex = Excludes::new(&Config::new("n".into()), &BackupSet::default(), Path::new("/nonexistent")).unwrap();
        let names = |r: &Path| -> Vec<String> {
            Walk::new(&[r.to_path_buf()], &ex)
                .map(|f| f.unwrap().path.strip_prefix(r).unwrap().display().to_string())
                .collect()
        };
        assert_eq!(names(&root), ["", "a", "a/x", "b"]);
        // A root given as a link is followed; links below it are not.
        assert_eq!(names(&dir.join("via")), ["", "a", "a/x", "b"]);
        // ...and recorded as the folder it names, so a restore can make a folder there.
        let top = Walk::new(&[dir.join("via")], &ex).next().unwrap().unwrap();
        assert!(top.ft == os::Kind::Dir && top.link.is_none());
        // A folder swapped for a link, or for another folder, after its parent listed it.
        let walk = Walk::new(&[], &ex);
        let st = fs::symlink_metadata(root.join("a")).unwrap();
        fs::rename(root.join("a"), dir.join("a")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("a")).unwrap();
        assert!(walk.list(&root.join("a"), (st.dev(), st.ino()), false).is_err());
        fs::remove_file(root.join("a")).unwrap();
        fs::create_dir(root.join("a")).unwrap();
        assert!(walk.list(&root.join("a"), (st.dev(), st.ino()), false).is_err());
        fs::remove_dir(root.join("a")).unwrap();
        fs::rename(dir.join("a"), root.join("a")).unwrap();
        assert_eq!(walk.list(&root.join("a"), (st.dev(), st.ino()), false).unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn free_index_ids_are_bounded() {
        let (zero, data) = ([0; 32], [1; 32]);
        let mut free = Free::new(10, zero);
        // A sparse file costs its zero chunks.
        assert!(free.take(None, &[data, zero, zero, zero, zero]));
        assert_eq!(free.left, 6);
        // The first name of a linked file likewise, a further name its whole list.
        assert!(free.take(Some((1, 2)), &[data, data, data]));
        assert_eq!(free.left, 6);
        assert!(free.take(Some((1, 2)), &[data, data, data]));
        assert_eq!(free.left, 3);
        assert!(!free.take(Some((1, 2)), &[data, data, data, data]));
        assert!(!free.take(None, &[zero; 4]));
        assert!(free.take(None, &[data; 100]));
        assert_eq!(free.left, 3);
    }

    #[test]
    fn hard_links_are_read_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("sdb-test-links-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a"), noise(3 << 20)).unwrap();
        fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        fs::hard_link(dir.join("a"), dir.join("c")).unwrap();
        let keys = Arc::new(Keys::new(&[7; 32]));
        let (tx, mut rx) = mpsc::channel(64);
        let drain = std::thread::spawn(move || while rx.blocking_recv().is_some() {});
        let sc = Scanner { keys, known: vec![], sent: HashSet::new(), tx, new_bytes: 0, zero: None };
        let (otx, _orx) = tokio::sync::mpsc::unbounded_channel();
        let ex = Excludes { globs: GlobSetBuilder::new().build().unwrap(), dirs: vec![] };
        let res = scan("links".into(), vec![dir.clone()], ex, HashMap::new(), sc, Out(otx)).unwrap();
        drain.join().unwrap();
        let lists: Vec<_> =
            res.entries.iter().filter(|e| matches!(e.kind, Kind::File { .. })).map(|e| &e.kind).collect();
        assert_eq!(lists.len(), 3);
        assert!(lists.iter().all(|k| *k == lists[0] && matches!(k, Kind::File { size, .. } if *size == 3 << 20)));
        assert_eq!(res.refused, 0);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Collects what a file chunks into, zeros included.
    struct Collect(Vec<u8>);

    impl Sink for Collect {
        fn data(&mut self, data: &[u8]) -> Result<Id> {
            self.0.extend_from_slice(data);
            Ok([1; 32])
        }

        fn zeros(&mut self) -> Result<Id> {
            self.0.resize(self.0.len() + MAX_CHUNK, 0);
            Ok([0; 32])
        }
    }

    #[test]
    fn holes_become_zero_chunks_without_changing_the_content() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("sdb-test-holes-{}", rand::random::<u64>()));
        let mut f = fs::File::create(&path).unwrap();
        let (head, tail) = (noise(3 << 20), noise(1 << 20));
        f.write_all(&head).unwrap();
        f.seek(std::io::SeekFrom::Start((3 << 20) + (64 << 20) + 12345)).unwrap();
        f.write_all(&tail).unwrap();
        // A trailing hole too.
        f.set_len((3 << 20) + (64 << 20) + 12345 + (1 << 20) + (9 << 20)).unwrap();
        drop(f);
        let f = fs::File::open(&path).unwrap();
        let len = f.metadata().unwrap().len();
        let mut sink = Collect(vec![]);
        let (size, _, skipped) = chunk_file(&f, len, &mut sink).unwrap();
        assert_eq!(size, len);
        assert_eq!(sink.0, fs::read(&path).unwrap());
        assert_eq!(skipped % MAX_CHUNK as u64, 0);
        // A file without holes chunks exactly as a plain read does.
        fs::write(&path, noise(6 << 20)).unwrap();
        let f = fs::File::open(&path).unwrap();
        let (_, ids, skipped) = chunk_file(&f, 6 << 20, &mut Collect(vec![])).unwrap();
        assert_eq!(skipped, 0);
        let plain = StreamCDC::new(fs::File::open(&path).unwrap(), MIN_CHUNK, AVG_CHUNK, MAX_CHUNK).count();
        assert_eq!(ids.len(), plain);
        fs::remove_file(path).unwrap();
    }

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
        let home = Path::new("/home/me/.cahoots");
        let mut set = BackupSet { skip: vec!["/home/me/proj/big".into()], ..Default::default() };
        let ex = Excludes::new(&cfg, &set, home).unwrap();
        assert!(ex.is_excluded(Path::new("/home/me/proj/node_modules")));
        assert!(ex.is_excluded(Path::new("/home/me/.DS_Store")));
        assert!(ex.is_excluded(Path::new("/home/me/.cahoots/store/x")));
        assert!(ex.is_excluded(Path::new("/home/me/proj/big/file")));
        assert!(ex.is_excluded(Path::new("/home/me/proj/a.sdb-tmp")));
        assert!(!ex.is_excluded(Path::new("/home/me/proj/src/main.rs")));
        assert!(!ex.is_excluded(Path::new("/home/me/proj/target/debug")));
        assert!(!ex.is_excluded(Path::new("/home/me/proj/.git/config")));
        set.exclude_list = crate::config::DEVELOPER.into();
        let dev = Excludes::new(&cfg, &set, home).unwrap();
        assert!(dev.is_excluded(Path::new("/home/me/proj/target")));
        let proj = Path::new("/home/me/proj");
        assert!(dev.covers(Path::new("/home/me/proj/target/debug/a.o"), proj));
        assert!(!dev.covers(Path::new("/home/me/proj/src/main.rs"), proj));
        // A set whose folder sits inside one with a listed name is still watched.
        let kept = Path::new("/srv/build/reports");
        assert!(!dev.covers(Path::new("/srv/build/reports/q3.txt"), kept));
        assert!(dev.covers(Path::new("/srv/build/reports/dist/x"), kept));
        cfg.exclude_lists.push(crate::config::ExcludeList { name: "mine".into(), patterns: vec!["[".into()] });
        set.exclude_list = "mine".into();
        assert!(Excludes::new(&cfg, &set, home).is_err());
    }
}
