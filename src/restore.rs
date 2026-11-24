// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::{
    backup::MAX_CHUNK,
    config::Friend,
    crypto::Id,
    node::{Node, Out},
    os::{self, Dir},
    snapshot::{Entry, Kind, human},
};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    io::{self, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

/// Where an entry lands.
struct Place {
    /// The drive it goes back to, when restoring a Windows file in place on Windows.
    drive: Option<u8>,
    /// Its path below the restore folder, or below that drive.
    path: PathBuf,
    /// Whether its own name had to be changed for this system to keep it.
    renamed: bool,
}

/// The drive a stored name stands for: a Windows path is recorded starting with `C:`.
fn drive(name: &[u8]) -> Option<u8> {
    (name.len() == 2 && name[1] == b':' && name[0].is_ascii_alphabetic()).then(|| name[0].to_ascii_uppercase())
}

/// Where an entry lands below the restore folder: under the name of the folder that was
/// backed up (`~/Documents/a.txt` gives `Documents/a.txt`), or at its full original path
/// when restoring in place or when two backed up folders share a name. A recorded path is
/// names with `/` between them whatever system wrote it, so it is taken apart here and never
/// by this system's own rules. None for one that tries to step out of the restore folder.
fn rel(e: &Entry, roots: &[&[u8]], to_drives: bool) -> Option<Place> {
    let p = &e.path[..];
    let below = |r: &&[u8]| p == *r || (p.len() > r.len() && p.starts_with(r) && p[r.len()] == b'/');
    let base = roots.iter().find(|r| below(r)).map_or(0, |r| r.iter().rposition(|b| *b == b'/').unwrap_or(0));
    let mut place = Place { drive: None, path: PathBuf::new(), renamed: false };
    let names = p[base..].split(|b| *b == b'/').filter(|n| !n.is_empty() && *n != b".");
    for (i, name) in names.enumerate() {
        if name == b".." {
            return None;
        }
        place.renamed = false;
        match drive(name).filter(|_| i == 0 && base == 0) {
            Some(d) if to_drives => place.drive = Some(d),
            // Anywhere else a drive becomes a folder named by its letter.
            Some(d) => place.path.push((d as char).to_string()),
            None => {
                let (name, changed) = os::name(name);
                place.path.push(name);
                place.renamed = changed;
            }
        }
    }
    Some(place)
}

/// A name as a disk where case does not count compares it: each letter as its capital, one
/// for one, as NTFS does by its table of capitals. A disk may count more names as one (APFS
/// counts accents written two ways as one); `free_name` asks the disk itself too.
fn fold(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let one = |c: char| {
        let mut up = c.to_uppercase();
        match (up.next(), up.next()) {
            (Some(u), None) => u,
            _ => c,
        }
    };
    name.nfc().map(one).collect()
}

/// Whether `dir` counts names that differ only in case as one name, found by making a file
/// there and looking it up in capitals. It differs from disk to disk (APFS and NTFS do, most
/// Linux disks do not, and a share, exFAT, or a Linux or NTFS folder set either way can be
/// either), even from folder to folder, so each folder is asked. `probe` is named as a
/// restore's half-written file, so one a restore dies leaving is cleared like the others.
/// A folder that cannot be asked is taken to: a second file of a name is then numbered,
/// never lost.
fn caseless(dir: &Dir, probe: &OsStr) -> bool {
    let Ok(f) = dir.create_file(probe) else { return true };
    let upper = OsString::from(probe.to_string_lossy().to_uppercase());
    let same = match (os::file_meta(&f), dir.stat(&upper, false)) {
        (Ok(a), Ok(b)) => (a.dev, a.ino) == (b.dev, b.ino),
        (_, Err(_)) => false,
        (Err(_), Ok(_)) => true,
    };
    drop(f);
    let _ = dir.remove_file(probe);
    same
}

/// A folder this restore writes files into, and what it knows of it.
struct Folder {
    dir: Dir,
    /// The folder itself, by device and inode.
    id: (u64, u64),
    caseless: bool,
    /// The names that were in it, as compared here, read the first time one is in the way.
    names: Option<HashSet<String>>,
}

impl Folder {
    fn new(dir: Dir, probe: &OsStr) -> Folder {
        Folder { id: id(&dir).unwrap_or_default(), caseless: caseless(&dir, probe), dir, names: None }
    }

    /// A name as this folder compares it. Where case counts a name is its bytes, each kept as
    /// it is: two names that are not text and differ only there stay two.
    fn key(&self, name: &OsStr) -> String {
        if self.caseless {
            fold(&name.to_string_lossy())
        } else {
            name.as_encoded_bytes().iter().map(|b| *b as char).collect()
        }
    }

    /// Whether `name`, which the folder finds, is the file of that very name, and not another
    /// the disk also finds by it: by its short name on Windows (`REPORT~1.TXT` for `Report
    /// for May.txt`), or by a spelling this restore does not count as the same name.
    fn holds(&mut self, name: &OsStr) -> bool {
        // A Unix disk where case counts finds a name by itself alone.
        if cfg!(unix) && !self.caseless {
            return true;
        }
        if self.names.is_none() {
            let names: HashSet<String> = self.dir.names().unwrap_or_default().iter().map(|n| self.key(n)).collect();
            self.names = Some(names);
        }
        self.names.as_ref().is_some_and(|names| names.contains(&self.key(name)))
    }
}

/// What this restore found of each folder it has written into, by device and inode: whether
/// case counts there. Files come in the order of their paths, so a folder is opened again
/// after each folder inside it; it is asked, and swept, only the first time.
type Seen = HashMap<(u64, u64), bool>;

/// Opens a folder to write files into, asking of it what `Seen` does not yet know.
fn folder(dir: Dir, probe: &OsStr, made: &Made, seen: &mut Seen) -> Folder {
    let Some(at) = id(&dir) else { return Folder::new(dir, probe) };
    let caseless = *seen.entry(at).or_insert_with(|| {
        // A folder that was already there may hold what a restore that died left.
        if !made.contains(&at) {
            sweep(&dir);
        }
        caseless(&dir, probe)
    });
    Folder { dir, id: at, caseless, names: None }
}

/// Names given out in each folder, by the folder and the name as it compares it.
type Taken = HashSet<((u64, u64), String)>;

/// A name for an entry in `folder` that lands neither on what this restore has already put
/// there (`ours`, by device and inode) nor on a different file the disk finds by that name
/// too: where `A` and `a` are one name, or a file has a short name as well, the second of a
/// name gets a number instead. A file of that very name that was there before is replaced,
/// as a restore does, so it says whether the name replaces what is there, as well as
/// whether it was numbered.
fn free_name(
    taken: &mut Taken,
    ours: impl Fn((u64, u64)) -> bool,
    folder: &mut Folder,
    name: &OsStr,
) -> (OsString, bool, bool) {
    let (stem, ext) = (Path::new(name).file_stem().unwrap_or(name), Path::new(name).extension());
    let mut free = name.to_os_string();
    let mut n = 1;
    loop {
        if taken.insert((folder.id, folder.key(&free))) {
            match folder.dir.stat(&free, false) {
                Err(_) => return (free, n > 1, false),
                Ok(m) if !ours((m.dev, m.ino)) && folder.holds(&free) => return (free, n > 1, true),
                Ok(_) => {}
            }
        }
        n += 1;
        free = stem.to_os_string();
        free.push(format!(" ({n})"));
        if let Some(ext) = ext {
            free.push(".");
            free.push(ext);
        }
    }
}

/// Moves a finished file or link into place: over what is there only when that is meant.
fn put(from_dir: &Dir, from: &OsStr, dir: &Dir, name: &OsStr, replace: bool) -> Result<()> {
    if replace {
        return Ok(from_dir.rename(from, dir, name)?);
    }
    match from_dir.rename_new(from, dir, name) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => bail!("another file took its name while it was restored"),
        done => Ok(done?),
    }
}

/// Folders this restore made, by device and inode, so only those get the recorded owner and
/// mode, never one someone else had waiting under the same name.
type Made = HashSet<(u64, u64)>;

fn id(fd: &Dir) -> Option<(u64, u64)> {
    fd.meta().ok().map(|m| (m.dev, m.ino))
}

/// Opens the folder `rel` below `top` one name at a time, never passing through a symbolic
/// link, so nothing planted in the restore folder can steer a write somewhere else. Missing
/// folders are made when `made` is given, which then records them; with `private` the last
/// one is made open to this user alone (as a folder that was closed to everyone else stays).
/// On failure it gives the part of `rel` that is in the way, so what lies below it can be
/// skipped.
fn open_dir(
    top: &Dir,
    rel: &Path,
    mut made: Option<&mut Made>,
    private: bool,
) -> Result<Dir, (PathBuf, anyhow::Error)> {
    let mut fd = top.try_clone().map_err(|e| (rel.to_path_buf(), e.into()))?;
    let mut at = PathBuf::new();
    let last = rel.components().filter(|c| matches!(c, Component::Normal(_))).count();
    for (i, c) in rel.components().filter(|c| !matches!(c, Component::RootDir | Component::CurDir)).enumerate() {
        let name = match c {
            Component::Normal(n) => n,
            _ => return Err((rel.to_path_buf(), anyhow!("unsafe path"))),
        };
        at.push(name);
        let make = |fd: &Dir| if private && i + 1 == last { fd.make_private(name) } else { fd.make(name) };
        let new = match made {
            Some(_) => match make(&fd) {
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err((at, e.into())),
                r => r.is_ok(),
            },
            None => false,
        };
        fd = fd
            .sub(name)
            .map_err(|e| (at.clone(), anyhow!("a link or not a folder, refusing to restore through it ({e})")))?;
        // Ours only if it is still the private one just made: anyone who can write the parent
        // may have swapped theirs in between.
        if new && let Some(m) = made.as_deref_mut() {
            let st = fd.meta().map_err(|e| (at.clone(), e.into()))?;
            if os::private_to_me(&st) {
                m.insert((st.dev, st.ino));
            }
        }
    }
    Ok(fd)
}

/// What the restore point recorded about an entry, to be put back on it.
fn recorded(e: &Entry) -> os::Recorded {
    os::Recorded { mode: e.mode, uid: e.uid, gid: e.gid, mtime: e.mtime }
}

/// A half-written file untouched for this long belongs to a restore that died: a live one
/// writes to its file far more often.
const STALE: i64 = 3600;

/// Whether `name` is a restore's half-written file: `.<8 hex>-<count>.sdb-tmp`.
fn is_tmp(name: &[u8]) -> bool {
    let Some(mid) = name.strip_prefix(b".").and_then(|n| n.strip_suffix(b".sdb-tmp")) else { return false };
    let mut parts = mid.splitn(2, |b| *b == b'-');
    let (Some(tag), Some(count)) = (parts.next(), parts.next()) else { return false };
    tag.len() == 8 && tag.iter().all(u8::is_ascii_hexdigit) && !count.is_empty() && count.iter().all(u8::is_ascii_digit)
}

/// Clears away the half-written files that restores which died (the daemon killed, the power
/// gone) left in a folder. One a running restore is still writing is too fresh to be taken.
fn sweep(dir: &Dir) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    dir.each_name(|name, _| {
        let stale = is_tmp(name.as_encoded_bytes())
            && dir
                .stat(name, false)
                .is_ok_and(|m| m.kind == os::Kind::File && now - m.mtime.div_euclid(1_000_000_000) > STALE);
        if stale {
            let _ = dir.remove_file(name);
        }
        true
    });
}

/// A file being written, removed again if the restore ends or is stopped (its client gone, say)
/// before the file is moved into place.
struct Unfinished {
    dir: Option<Dir>,
    name: OsString,
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = dir.remove_file(&self.name);
        }
    }
}

/// Makes the link in a new folder only this process can write, gives it its recorded owner
/// and time there, then moves it into place. Done next to the destination, anyone who can
/// write the restore folder could swap in a hard link to another file between the steps and
/// have that file chowned instead. On Windows, where nothing is put on the link there, the
/// folder takes `dir`'s permissions, and the link with it keeps them once moved.
fn place_link(dir: &Dir, name: &OsStr, target: &[u8], e: &Entry, root: bool, replace: bool) -> Result<()> {
    let tmp: OsString = format!(".sdb-tmp-{:016x}", rand::random::<u64>()).into();
    dir.make(&tmp)?;
    let made = dir.sub(&tmp).map_err(anyhow::Error::from).and_then(|t| {
        // Someone who can write `dir` may have renamed ours away and put their own in its place.
        if !os::private_to_me(&t.meta()?) {
            bail!("the temporary folder was replaced");
        }
        let link = OsStr::new("l");
        t.symlink(link, target, dir)?;
        t.finish_link(link, &recorded(e), root);
        let moved = put(&t, link, dir, name, replace);
        if moved.is_err() {
            let _ = t.remove_file(link);
        }
        moved
    });
    let _ = dir.remove_dir(&tmp);
    made
}

/// Where a restore's pieces come from.
trait Pieces {
    async fn get(&self, ids: &[Id]) -> Result<Vec<Vec<u8>>>;
}

/// The destinations the restore point is kept on.
struct Destinations<'a>(&'a Node, &'a [Friend]);

impl Pieces for Destinations<'_> {
    async fn get(&self, ids: &[Id]) -> Result<Vec<Vec<u8>>> {
        self.0.fetch(self.1, ids).await
    }
}

/// Writes a file's pieces into `f`, leaving a hole wherever the backup found one (`zero`),
/// and gives how much it wrote. `wrote` hears how much as each piece is written.
async fn write_pieces(
    f: &mut fs::File,
    chunks: &[Id],
    zero: &Id,
    pieces: &impl Pieces,
    wrote: &mut impl FnMut(u64),
) -> Result<u64> {
    if chunks.contains(zero) {
        // For a disk that leaves holes only when asked (Windows). One that cannot has them
        // written out instead.
        let _ = os::set_sparse(f);
    }
    let mut written = 0u64;
    for run in chunks.chunk_by(|a, b| (a == zero) == (b == zero)) {
        if run[0] == *zero {
            let n = (run.len() * MAX_CHUNK) as u64;
            f.seek(SeekFrom::Current(n as i64))?;
            written += n;
            wrote(n);
            continue;
        }
        for batch in run.chunks(8) {
            for data in pieces.get(batch).await.map_err(Unfetched)? {
                f.write_all(&data)?;
                written += data.len() as u64;
                wrote(data.len() as u64);
            }
        }
    }
    // A file that ends in a hole gets its length here.
    f.set_len(written)?;
    Ok(written)
}

/// Restores one file into `dir` as `name`, written under the half-written name `tmp` and
/// moved into place once whole, over what is there only with `replace`. Gives the device and
/// inode it now has. A file that fails (the disk full, a piece no destination has) leaves
/// nothing behind and is the caller's to report, and to stop on (see `Failing`).
#[allow(clippy::too_many_arguments)]
async fn restore_file(
    dir: &Dir,
    tmp: &OsStr,
    name: &OsStr,
    replace: bool,
    (chunks, size, zero): (&[Id], u64, &Id),
    pieces: &impl Pieces,
    wrote: &mut impl FnMut(u64),
    (r, root): (&os::Recorded, bool),
) -> Result<Option<(u64, u64)>> {
    // A brand new file, never one (or a link) that was waiting under this name, open to this
    // user alone until it is finished.
    let mut f = dir.create_file(tmp)?;
    let mut unfinished = Unfinished { dir: dir.try_clone().ok(), name: tmp.to_owned() };
    let written = write_pieces(&mut f, chunks, zero, pieces, wrote).await?;
    if written != size {
        bail!("restored {written} bytes but the backup recorded {size}");
    }
    os::finish_file(&f, r, root);
    let at = os::file_meta(&f).ok().map(|m| (m.dev, m.ino));
    drop(f);
    put(dir, tmp, dir, name, replace)?;
    unfinished.dir = None;
    Ok(at)
}

/// Files in a row whose pieces could not be fetched, after which a restore stops: past a few,
/// every file left would most likely fail the same way (no destination answers), each after
/// fetching what it could.
const FAILING: u32 = 20;

/// Why a file's pieces could not be fetched, told apart from a file that could not be written.
#[derive(Debug)]
struct Unfetched(anyhow::Error);

impl std::fmt::Display for Unfetched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for Unfetched {}

/// Why nothing more can be written where an error came from, if that is what it says: the
/// disk is full, past its quota, or only to be read.
fn disk_stops(err: &anyhow::Error) -> Option<String> {
    use io::ErrorKind::{QuotaExceeded, ReadOnlyFilesystem, StorageFull};
    let e = err.chain().filter_map(|e| e.downcast_ref::<io::Error>()).next()?;
    match e.kind() {
        StorageFull => Some("the disk is full".into()),
        QuotaExceeded => Some("the disk quota is used up".into()),
        ReadOnlyFilesystem => Some("the disk can only be read".into()),
        _ => None,
    }
}

/// Counts files that could not be fetched one after another.
#[derive(Default)]
struct Failing(u32);

impl Failing {
    fn ok(&mut self) {
        self.0 = 0;
    }

    /// Notes a file that failed with `err`; gives why the restore stops, if it should. One
    /// that could not be written (a link Windows refuses, a folder closed to us) says nothing
    /// of the next, so it is not counted.
    fn failed(&mut self, err: &anyhow::Error) -> Option<String> {
        if let Some(why) = disk_stops(err) {
            return Some(why);
        }
        err.downcast_ref::<Unfetched>()?;
        self.0 += 1;
        (self.0 >= FAILING).then(|| format!("the last {FAILING} files could not be fetched, the last with: {err:#}"))
    }
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
    // How the caller asks for every entry to go back to its full original path.
    let in_place = to == Path::new("/");
    let mut roots: Vec<&[u8]> = roots.iter().map(|r| r.trim_end_matches('/').as_bytes()).collect();
    let names: HashSet<&[u8]> = roots.iter().map(|r| r.rsplit(|b| *b == b'/').next().unwrap_or(r)).collect();
    if in_place || names.len() != roots.len() {
        roots.clear();
    }
    // In place, a Windows file goes back to its own drive, where this system has drives.
    let to_drives = in_place && os::drive_root(b'C').is_some();
    let places: Vec<Option<Place>> = entries.iter().map(|e| rel(e, &roots, to_drives)).collect();
    let root = os::is_root();
    // `to` is the folder the user chose, so links in it and its parents are followed on purpose.
    fs::create_dir_all(to).with_context(|| to.display().to_string())?;
    let (all_to, all_top) = (to, Dir::open(to, true).with_context(|| to.display().to_string())?);
    let mut drives: HashMap<u8, Option<(PathBuf, Dir)>> = HashMap::new();
    for d in places.iter().flatten().filter_map(|p| p.drive) {
        drives.entry(d).or_insert_with(|| {
            let root = os::drive_root(d)?;
            Dir::open(&root, true).ok().map(|dir| (root, dir))
        });
    }
    let base = |drive: Option<u8>| -> Option<(&Path, &Dir)> {
        match drive {
            None => Some((all_to, &all_top)),
            Some(d) => drives.get(&d)?.as_ref().map(|(root, dir)| (root.as_path(), dir)),
        }
    };
    let (mut files, mut bytes, mut skipped) = (0u64, 0u64, 0u64);
    let mut told = std::time::Instant::now();
    // A hole the backup skipped is stored as this chunk; it is skipped again here rather
    // than fetched and written, so a sparse file comes back sparse.
    let zero = node.keys.chunk_id(&vec![0; MAX_CHUNK]);
    // Names this restore's half-written files apart from any other restore's.
    let (run_tag, mut tmp_count) = (format!("{:08x}", rand::random::<u32>()), 0u64);
    // Folders that could not be opened; everything below them is skipped.
    let mut blocked: HashMap<Option<u8>, Vec<PathBuf>> = HashMap::new();
    // The folder most recently written into, kept open for the files that follow in it.
    let mut held: Option<(Option<u8>, PathBuf, Folder)> = None;
    let (mut made, mut seen) = (Made::new(), Seen::new());
    // Why the restore stopped before the end, if it did.
    let (mut stopped, mut failing) = (None, Failing::default());
    // What this restore has put where: names given out, and the files and links themselves.
    let (mut taken, mut placed) = (Taken::new(), HashSet::<(u64, u64)>::new());
    let probe: OsString = format!(".{run_tag}-0.sdb-tmp").into();
    let pieces = Destinations(node, friends);
    for (e, place) in entries.iter().zip(&places) {
        let Some(place) = place else {
            out.say(format!("  skipped {}: unsafe path", e.show()));
            skipped += 1;
            continue;
        };
        let Some((to, top)) = base(place.drive) else {
            out.say(format!("  skipped {}: its drive cannot be opened", e.show()));
            skipped += 1;
            continue;
        };
        let blocked = blocked.entry(place.drive).or_default();
        if blocked.iter().any(|b| place.path.starts_with(b)) {
            skipped += 1;
            continue;
        }
        if e.kind == Kind::Dir {
            match open_dir(top, &place.path, Some(&mut made), recorded(e).private()) {
                Ok(_) if place.renamed => {
                    out.say(format!("  {} was restored as {}", e.show(), to.join(&place.path).display()))
                }
                Ok(_) => {}
                Err((at, err)) => {
                    skip(out, to, &at, &err, Some(blocked));
                    skipped += 1;
                    if let Some(why) = disk_stops(&err) {
                        stopped = Some(why);
                        break;
                    }
                }
            }
            continue;
        }
        let (parent, name) = match (place.path.parent(), place.path.file_name()) {
            (Some(p), Some(n)) => (p, n),
            _ => {
                skip(out, to, &place.path, &anyhow!("unsafe path"), None);
                skipped += 1;
                continue;
            }
        };
        if held.as_ref().is_none_or(|(d, p, _)| *d != place.drive || p != parent) {
            held = None;
            match open_dir(top, parent, Some(&mut made), false) {
                Ok(fd) => held = Some((place.drive, parent.to_path_buf(), folder(fd, &probe, &made, &mut seen))),
                Err((at, err)) => {
                    skip(out, to, &at, &err, Some(blocked));
                    skipped += 1;
                    if let Some(why) = disk_stops(&err) {
                        stopped = Some(why);
                        break;
                    }
                    continue;
                }
            }
        }
        let folder = &mut held.as_mut().unwrap().2;
        let ours = |i| placed.contains(&i) || made.contains(&i);
        let (name, numbered, replace) = free_name(&mut taken, ours, folder, name);
        let (path, name, dir) = (parent.join(&name), name.as_os_str(), &folder.dir);
        let renamed =
            (place.renamed || numbered).then(|| format!("  {} was restored as {}", e.show(), to.join(&path).display()));
        let done = match &e.kind {
            Kind::Dir => continue,
            Kind::Symlink(target) => place_link(dir, name, target, e, root, replace)
                .map(|()| dir.stat(name, false).ok().map(|m| (m.dev, m.ino))),
            Kind::File { chunks, size } => {
                // Each restore writes under a name of its own: one still running in the daemon after
                // its client was stopped must not have its half-written file taken, or renamed away,
                // by the next restore of the same file. Short, so it fits beside the longest name the
                // file may have; the ending keeps it out of backups.
                let mut old: OsString = name.to_owned();
                old.push(".sdb-tmp");
                let _ = dir.remove_file(&old);
                tmp_count += 1;
                let tmp: OsString = format!(".{run_tag}-{tmp_count}.sdb-tmp").into();
                let mut wrote = |n: u64| {
                    bytes += n;
                    if told.elapsed().as_secs() >= 2 {
                        told = std::time::Instant::now();
                        out.say(format!("  ... {files} files, {} restored so far", human(bytes)));
                    }
                };
                let file = (&chunks[..], *size, &zero);
                restore_file(dir, &tmp, name, replace, file, &pieces, &mut wrote, (&recorded(e), root)).await
            }
        };
        match done {
            Ok(at) => {
                // Only a file whose pieces were fetched shows a destination answering: a link,
                // an empty file or one that is all holes comes back with none answering.
                if matches!(&e.kind, Kind::File { chunks, .. } if chunks.iter().any(|c| *c != zero)) {
                    failing.ok();
                }
                placed.extend(at);
                if matches!(e.kind, Kind::File { .. }) {
                    files += 1;
                }
                renamed.into_iter().for_each(|line| out.say(line));
            }
            // Reported, and the restore goes on with the next: one file that cannot be
            // written, or fetched, does not stop the others coming back. A full disk, or a
            // run of files that cannot be fetched, does: every file left would fail the same way.
            Err(err) => {
                skip(out, to, &path, &err, None);
                skipped += 1;
                if let Some(why) = failing.failed(&err) {
                    stopped = Some(why);
                    break;
                }
            }
        }
    }
    drop(held);
    for (e, place) in entries.iter().zip(&places).rev().filter(|(e, _)| e.kind == Kind::Dir) {
        let Some((place, (_, top))) = place.as_ref().and_then(|p| Some((p, base(p.drive)?))) else { continue };
        // Re-opened by name, so checked to be the very folder made earlier.
        match open_dir(top, &place.path, None, false) {
            Ok(fd) if id(&fd).is_some_and(|i| made.contains(&i)) => fd.finish(&recorded(e), root),
            Ok(fd) => fd.finish_existing(&recorded(e), root),
            Err(_) => {}
        }
    }
    out.say(format!("restored {files} files ({}) into {}", human(bytes), all_to.display()));
    if let Some(why) = stopped {
        bail!("the restore stopped before the end: {why}. Put that right and restore again");
    }
    if skipped > 0 {
        bail!("{skipped} entries could not be restored (listed above), so the restore is incomplete");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn entry(path: &str) -> Entry {
        Entry { path: path.as_bytes().to_vec(), mode: 0o100644, uid: 0, gid: 0, mtime: 0, ino: 0, kind: Kind::Dir }
    }

    #[test]
    fn a_recorded_path_lands_the_same_way_on_every_system() {
        let at = |path: &str, roots: &[&str], to_drives: bool| {
            let roots: Vec<&[u8]> = roots.iter().map(|r| r.as_bytes()).collect();
            rel(&entry(path), &roots, to_drives).map(|p| (p.drive, p.path.to_string_lossy().replace('\\', "/")))
        };
        let plain = |path: &str| Some((None, path.to_string()));
        assert_eq!(at("/Users/me/Documents/a.txt", &["/Users/me/Documents"], false), plain("Documents/a.txt"));
        assert_eq!(at("/Users/me/Documents", &["/Users/me/Documents"], false), plain("Documents"));
        assert_eq!(at("/Users/me/Documents2/a", &["/Users/me/Documents"], false), plain("Users/me/Documents2/a"));
        assert_eq!(at("/Users/me//Documents/./a.txt", &[], false), plain("Users/me/Documents/a.txt"));
        assert_eq!(at("/etc/hosts", &[""], false), plain("etc/hosts"));
        assert_eq!(at("/a/../b", &[], false), None);
        // A Windows path: under its folder's name, or with its drive as a folder or as the drive.
        assert_eq!(at("C:/Users/me/Documents/a.txt", &["C:/Users/me/Documents"], false), plain("Documents/a.txt"));
        assert_eq!(at("D:/photos/a.jpg", &["D:"], false), plain("D/photos/a.jpg"));
        assert_eq!(at("C:/Users/me/a.txt", &[], false), plain("C/Users/me/a.txt"));
        assert_eq!(at("c:/Users/me/a.txt", &[], true), Some((Some(b'C'), "Users/me/a.txt".to_string())));
    }

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!("sdb-test-restore-{}", rand::random::<u64>()))
    }

    /// Whether the disk itself finds `name` by `other`, as the probe in `caseless` asks.
    fn same_file(dir: &Path, name: &str, other: &str) -> bool {
        fs::write(dir.join(name), "x").unwrap();
        let same = fs::symlink_metadata(dir.join(other)).is_ok();
        fs::remove_file(dir.join(name)).unwrap();
        same
    }

    #[test]
    fn whether_case_counts_is_asked_of_the_folder() {
        let dir = scratch();
        let folder = Folder::new(top(&dir), ".0a1b2c3d-0.sdb-tmp".as_ref());
        assert_eq!(folder.caseless, same_file(&dir, "Probe", "PROBE"));
        // As APFS, where a Mac keeps its temporary folder, is by default.
        #[cfg(any(target_os = "macos", windows))]
        assert!(folder.caseless);
        assert!(fs::read_dir(&dir).unwrap().next().is_none(), "the probe is taken away");
        drop(folder);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_folder_opened_again_is_not_asked_again() {
        let dir = scratch();
        let (made, mut seen) = (Made::new(), Seen::new());
        let first = folder(top(&dir), ".0a1b2c3d-0.sdb-tmp".as_ref(), &made, &mut seen);
        assert_eq!(seen.get(&first.id), Some(&first.caseless));
        // What it found is taken as it stands: here the opposite, to tell it from asking again.
        seen.insert(first.id, !first.caseless);
        // Nor is it swept again: a stale half-written file put there since stays.
        let stale = dir.join(".deadbeef-1.sdb-tmp");
        fs::write(&stale, "x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * STALE as u64);
        fs::File::options().write(true).open(&stale).unwrap().set_modified(old).unwrap();
        let again = folder(top(&dir), ".0a1b2c3d-0.sdb-tmp".as_ref(), &made, &mut seen);
        assert_eq!(again.caseless, !first.caseless);
        assert!(stale.exists());
        // A folder new to the restore is swept, and asked.
        let fresh = folder(top(&dir), ".0a1b2c3d-0.sdb-tmp".as_ref(), &made, &mut Seen::new());
        assert_eq!(fresh.caseless, first.caseless);
        assert!(!stale.exists());
        drop((first, again, fresh));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_restore_stops_on_a_full_disk_or_a_run_of_failures() {
        let lost = anyhow::Error::from(Unfetched(anyhow!("no friend holds this data")));
        let mut f = Failing::default();
        // Files that cannot be written, however many, do not stop it: each fails for itself.
        let refused = anyhow::Error::from(io::Error::from(io::ErrorKind::PermissionDenied)).context("making the link");
        for _ in 0..FAILING * 2 {
            assert_eq!(f.failed(&refused), None);
        }
        for _ in 1..FAILING {
            assert_eq!(f.failed(&lost), None);
        }
        // One that comes back starts the count again.
        f.ok();
        for _ in 1..FAILING {
            assert_eq!(f.failed(&lost), None);
        }
        assert!(f.failed(&lost).is_some_and(|why| why.contains("no friend")));
        // A full disk, or one only to be read, stops it at once, however it is wrapped.
        for kind in [io::ErrorKind::StorageFull, io::ErrorKind::QuotaExceeded, io::ErrorKind::ReadOnlyFilesystem] {
            let err = anyhow::Error::from(io::Error::from(kind)).context("writing it");
            assert!(Failing::default().failed(&err).is_some(), "{kind:?}");
        }
        // As the system says it.
        #[cfg(unix)]
        let full =
            [rustix::io::Errno::NOSPC, rustix::io::Errno::DQUOT, rustix::io::Errno::ROFS].map(|e| e.raw_os_error());
        // ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL, ERROR_WRITE_PROTECT, ERROR_DISK_QUOTA_EXCEEDED
        #[cfg(windows)]
        let full = [112, 39, 19, 1295];
        for code in full {
            assert!(disk_stops(&io::Error::from_raw_os_error(code).into()).is_some(), "{code}");
        }
        assert!(disk_stops(&io::Error::from(io::ErrorKind::PermissionDenied).into()).is_none());
    }

    #[test]
    fn a_second_file_of_a_name_gets_a_number_where_the_disk_counts_them_as_one() {
        let dir = scratch();
        let mut folder = Folder::new(top(&dir), ".0a1b2c3d-0.sdb-tmp".as_ref());
        let (mut taken, mut placed) = (Taken::new(), HashSet::new());
        // Puts a file where free_name says, as the restore does, and gives what it was named.
        let mut put = |folder: &mut Folder, n: &str| {
            let (name, numbered, replace) = free_name(&mut taken, |i| placed.contains(&i), folder, n.as_ref());
            assert!(!replace, "{n} replaced what is there");
            let name = name.into_string().unwrap();
            fs::write(dir.join(&name), n).unwrap();
            let m = folder.dir.stat(name.as_ref(), false).unwrap();
            placed.insert((m.dev, m.ino));
            assert_eq!(numbered, name != n);
            name
        };
        assert_eq!(put(&mut folder, "Readme.txt"), "Readme.txt");
        assert_eq!(put(&mut folder, "other"), "other");
        if folder.caseless {
            assert_eq!(put(&mut folder, "README.TXT"), "README (2).TXT");
            assert_eq!(put(&mut folder, "readme.txt"), "readme (3).txt");
            assert_eq!(put(&mut folder, "OTHER"), "OTHER (2)");
        } else {
            assert_eq!(put(&mut folder, "README.TXT"), "README.TXT");
        }
        // Whatever else the disk counts as the same name is caught by asking it: APFS takes an
        // accent written as one letter and as two for the same.
        let (one, two) = ("caf\u{e9}", "cafe\u{301}");
        put(&mut folder, one);
        if same_file(&dir, "x\u{e9}", "xe\u{301}") {
            assert_eq!(put(&mut folder, two), format!("{two} (2)"));
        }
        // None was written over another.
        assert_eq!(fs::read_to_string(dir.join("Readme.txt")).unwrap(), "Readme.txt");
        assert_eq!(fs::read_to_string(dir.join(one)).unwrap(), one);
        drop(folder);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn names_that_are_not_text_stay_apart_where_case_counts() {
        use std::os::unix::ffi::OsStrExt;
        let dir = scratch();
        let mut folder = Folder::new(top(&dir), ".0a1b2c3d-0.sdb-tmp".as_ref());
        let (one, other) = (OsStr::from_bytes(b"caf\xe9"), OsStr::from_bytes(b"caf\xe8"));
        folder.caseless = false;
        assert_ne!(folder.key(one), folder.key(other));
        assert_ne!(folder.key("Readme".as_ref()), folder.key("README".as_ref()));
        folder.caseless = true;
        assert_eq!(folder.key("Readme".as_ref()), folder.key("README".as_ref()));
        // An accent written as one letter or as two is one name: a disk that lists names its
        // own way (HFS+) must still find the file a restore put there.
        assert_eq!(folder.key("R\u{e9}sum\u{e9}.pdf".as_ref()), folder.key("Re\u{301}sume\u{301}.pdf".as_ref()));
        drop(folder);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_that_was_there_is_replaced_only_by_its_own_name() {
        let dir = scratch();
        let top = top(&dir);
        fs::write(dir.join("notes.txt"), "old").unwrap();
        let mut folder = Folder::new(top.try_clone().unwrap(), ".0a1b2c3d-0.sdb-tmp".as_ref());
        let mut taken = Taken::new();
        let mut name = |n: &str| {
            let (name, numbered, replace) = free_name(&mut taken, |_| false, &mut folder, n.as_ref());
            (name.into_string().unwrap(), numbered, replace)
        };
        // In place, the file comes back over itself.
        assert_eq!(name("notes.txt"), ("notes.txt".into(), false, true));
        assert_eq!(name("new.txt"), ("new.txt".into(), false, false));
        // Moving into place never replaces what is there when that is not meant.
        fs::write(dir.join("tmp"), "new").unwrap();
        let err = put(&top, "tmp".as_ref(), &top, "notes.txt".as_ref(), false).unwrap_err();
        assert!(err.to_string().contains("took its name"), "{err:#}");
        if same_file(&dir, "Case", "CASE") {
            put(&top, "tmp".as_ref(), &top, "NOTES.TXT".as_ref(), false).unwrap_err();
        }
        assert_eq!(fs::read_to_string(dir.join("notes.txt")).unwrap(), "old");
        put(&top, "tmp".as_ref(), &top, "notes.txt".as_ref(), true).unwrap();
        assert_eq!(fs::read_to_string(dir.join("notes.txt")).unwrap(), "new");
        drop((folder, top));
        fs::remove_dir_all(dir).unwrap();
    }

    /// A Windows file's short name finds it too: a restored file of that name gets a number
    /// rather than replacing it.
    #[cfg(windows)]
    #[test]
    fn a_short_name_does_not_replace_the_file_it_stands_for() {
        let dir = scratch();
        let top = top(&dir);
        fs::write(dir.join("Report for May.txt"), "theirs").unwrap();
        if fs::metadata(dir.join("REPORT~1.TXT")).is_err() {
            println!("skipped: this disk makes no short names");
            drop(top);
            return fs::remove_dir_all(dir).unwrap();
        }
        let mut folder = Folder::new(top.try_clone().unwrap(), ".0a1b2c3d-0.sdb-tmp".as_ref());
        let (name, numbered, replace) = free_name(&mut Taken::new(), |_| false, &mut folder, "REPORT~1.TXT".as_ref());
        assert_eq!((name.to_str().unwrap(), numbered, replace), ("REPORT~1 (2).TXT", true, false));
        // In place, the file of its own name still comes back over itself, whatever its case.
        let (name, _, replace) = free_name(&mut Taken::new(), |_| false, &mut folder, "REPORT FOR MAY.TXT".as_ref());
        assert!(replace && name == "REPORT FOR MAY.TXT");
        fs::write(dir.join("tmp"), "new").unwrap();
        assert!(put(&top, "tmp".as_ref(), &top, "REPORT~1.TXT".as_ref(), false).is_err());
        assert_eq!(fs::read_to_string(dir.join("Report for May.txt")).unwrap(), "theirs");
        drop((folder, top));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Hands out made-up pieces, failing for those it is told to.
    struct Fake(HashMap<Id, Vec<u8>>);

    impl Pieces for Fake {
        async fn get(&self, ids: &[Id]) -> Result<Vec<Vec<u8>>> {
            ids.iter().map(|id| self.0.get(id).cloned().ok_or_else(|| anyhow!("no friend holds this data"))).collect()
        }
    }

    #[tokio::test]
    async fn a_file_that_fails_is_its_own_and_holes_stay_holes() {
        let dir = scratch();
        let top = top(&dir);
        let (data, zero, gone) = ([1; 32], [0; 32], [9; 32]);
        let pieces = Fake(HashMap::from([(data, vec![7; 4096])]));
        let r = os::Recorded { mode: 0o100644, uid: 0, gid: 0, mtime: 0 };
        let mut wrote = |_| {};
        let mut restore = async |name: &str, chunks: &[Id], size: u64| {
            let file = (chunks, size, &zero);
            let tmp = format!(".0a1b2c3d-{name}.sdb-tmp");
            restore_file(&top, tmp.as_ref(), name.as_ref(), false, file, &pieces, &mut wrote, (&r, false)).await
        };
        // A piece no destination has: the file is not restored, and leaves nothing behind.
        let lost = restore("lost", &[data, gone], 8192).await;
        assert!(lost.unwrap_err().to_string().contains("no friend"));
        // Nor is one that does not come out the size it was.
        assert!(restore("short", &[data], 5000).await.is_err());
        assert!(fs::read_dir(&dir).unwrap().next().is_none(), "nothing is left of either");
        // The next one comes back, holes and all: 64 MiB with 8 KiB of data.
        let holes = [vec![data], vec![zero; 8], vec![data], vec![zero; 8]].concat();
        let len = 4096 * 2 + 16 * MAX_CHUNK as u64;
        assert!(restore("sparse", &holes, len).await.unwrap().is_some());
        let f = os::open_file(&dir.join("sparse")).unwrap();
        assert_eq!(f.metadata().unwrap().len(), len);
        assert!(os::allocated(&f).unwrap() < 4 << 20, "{} bytes taken", os::allocated(&f).unwrap());
        let back = fs::read(dir.join("sparse")).unwrap();
        assert!(back[..4096].iter().all(|b| *b == 7) && back[4096..4096 + 8 * MAX_CHUNK].iter().all(|b| *b == 0));
        drop((f, top));
        fs::remove_dir_all(dir).unwrap();
    }

    fn top(dir: &Path) -> Dir {
        fs::create_dir_all(dir).unwrap();
        Dir::open(dir, true).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn folders_are_never_opened_through_a_link() {
        let dir = std::env::temp_dir().join(format!("sdb-test-restore-{}", rand::random::<u64>()));
        let top = top(&dir);
        fs::create_dir_all(dir.join("outside")).unwrap();
        std::os::unix::fs::symlink(dir.join("outside"), dir.join("link")).unwrap();
        assert!(open_dir(&top, Path::new("real/deeper"), Some(&mut Made::new()), false).is_ok());
        assert!(open_dir(&top, Path::new("link"), Some(&mut Made::new()), false).is_err());
        assert!(open_dir(&top, Path::new("link/sub"), Some(&mut Made::new()), false).is_err());
        assert!(open_dir(&top, Path::new("real/../outside"), Some(&mut Made::new()), false).is_err());
        assert!(!dir.join("outside/sub").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn half_written_files_do_not_outlive_their_restore() {
        let dir = std::env::temp_dir().join(format!("sdb-test-restore-{}", rand::random::<u64>()));
        let top = top(&dir);
        assert!(is_tmp(b".0a1b2c3d-17.sdb-tmp"));
        for other in
            [".0a1b2c3d-.sdb-tmp", ".0a1b2c3-17.sdb-tmp", "0a1b2c3d-17.sdb-tmp", ".0a1b2c3d-17.txt", "note.sdb-tmp"]
        {
            assert!(!is_tmp(other.as_bytes()), "{other}");
        }
        // A restore that is stopped takes its file with it; one that finished does not.
        fs::write(dir.join(".0a1b2c3d-1.sdb-tmp"), "half").unwrap();
        drop(Unfinished { dir: top.try_clone().ok(), name: ".0a1b2c3d-1.sdb-tmp".into() });
        assert!(!dir.join(".0a1b2c3d-1.sdb-tmp").exists());
        fs::write(dir.join("done"), "whole").unwrap();
        drop(Unfinished { dir: None, name: "done".into() });
        assert!(dir.join("done").exists());
        // What a restore that died left is cleared once it is old; a fresh one may still be in use.
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * STALE as u64);
        for (name, old) in [(".aaaaaaaa-1.sdb-tmp", true), (".bbbbbbbb-2.sdb-tmp", false), ("kept.txt", true)] {
            let f = fs::File::create(dir.join(name)).unwrap();
            if old {
                f.set_modified(long_ago).unwrap();
            }
        }
        sweep(&top);
        assert!(!dir.join(".aaaaaaaa-1.sdb-tmp").exists(), "a stale half-written file is cleared");
        assert!(dir.join(".bbbbbbbb-2.sdb-tmp").exists(), "a fresh one is left alone");
        assert!(dir.join("kept.txt").exists(), "nothing else is touched");
        // Windows keeps a folder that is open where it is.
        drop(top);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn only_folders_this_restore_made_are_recorded() {
        let dir = std::env::temp_dir().join(format!("sdb-test-restore-{}", rand::random::<u64>()));
        let top = top(&dir);
        fs::create_dir_all(dir.join("planted")).unwrap();
        fs::set_permissions(dir.join("planted"), fs::Permissions::from_mode(0o777)).unwrap();
        let mut made = Made::new();
        let new = open_dir(&top, Path::new("planted/new"), Some(&mut made), false).unwrap();
        assert_eq!(made.len(), 1);
        assert!(made.contains(&id(&new).unwrap()));
        let fd = open_dir(&top, Path::new("planted"), None, false).unwrap();
        assert!(!made.contains(&id(&fd).unwrap()));
        let e = Entry { path: vec![], mode: 0o42755, uid: 0, gid: 0, mtime: 0, ino: 0, kind: Kind::Dir };
        fd.finish_existing(&recorded(&e), false);
        assert_eq!(fs::metadata(dir.join("planted")).unwrap().permissions().mode() & 0o7777, 0o755);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
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
        os::finish_file(&f, &recorded(&e), false);
        assert_eq!(f.metadata().unwrap().permissions().mode() & 0o7777, 0o755);
        fs::remove_dir_all(dir).unwrap();
    }
}
