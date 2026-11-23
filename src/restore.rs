// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::{
    backup::MAX_CHUNK,
    config::Friend,
    node::{Node, Out},
    os::{self, Dir},
    snapshot::{Entry, Kind, human},
};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    io::{Seek, SeekFrom, Write},
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

/// On a system where `A` and `a` are one name, a second file of a name would replace the
/// first: it gets a number instead. `taken` holds what this restore has put where.
fn free_name(
    taken: &mut HashSet<(Option<u8>, String)>,
    drive: Option<u8>,
    parent: &Path,
    name: &OsStr,
) -> (OsString, bool) {
    let mut free = name.to_os_string();
    if !os::CASELESS {
        return (free, false);
    }
    let (stem, ext) = (Path::new(name).file_stem().unwrap_or(name), Path::new(name).extension());
    let mut n = 1;
    while !taken.insert((drive, parent.join(&free).to_string_lossy().to_lowercase())) {
        n += 1;
        free = stem.to_os_string();
        free.push(format!(" ({n})"));
        if let Some(ext) = ext {
            free.push(".");
            free.push(ext);
        }
    }
    (free, n > 1)
}

/// Folders this restore made, by device and inode, so only those get the recorded owner and
/// mode, never one someone else had waiting under the same name.
type Made = HashSet<(u64, u64)>;

fn id(fd: &Dir) -> Option<(u64, u64)> {
    fd.meta().ok().map(|m| (m.dev, m.ino))
}

/// Opens the folder `rel` below `top` one name at a time, never passing through a symbolic
/// link, so nothing planted in the restore folder can steer a write somewhere else. Missing
/// folders are made when `made` is given, which then records them. On failure it gives the
/// part of `rel` that is in the way, so what lies below it can be skipped.
fn open_dir(top: &Dir, rel: &Path, mut made: Option<&mut Made>) -> Result<Dir, (PathBuf, anyhow::Error)> {
    let mut fd = top.try_clone().map_err(|e| (rel.to_path_buf(), e.into()))?;
    let mut at = PathBuf::new();
    for c in rel.components() {
        let name = match c {
            Component::Normal(n) => n,
            Component::RootDir | Component::CurDir => continue,
            _ => return Err((rel.to_path_buf(), anyhow!("unsafe path"))),
        };
        at.push(name);
        let new = match made {
            Some(_) => match fd.make(name) {
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
/// have that file chowned instead.
fn place_link(dir: &Dir, name: &OsStr, target: &[u8], e: &Entry, root: bool) -> Result<()> {
    let tmp: OsString = format!(".sdb-tmp-{:016x}", rand::random::<u64>()).into();
    dir.make(&tmp)?;
    let made = dir.sub(&tmp).map_err(anyhow::Error::from).and_then(|t| {
        // Someone who can write `dir` may have renamed ours away and put their own in its place.
        if !os::private_to_me(&t.meta()?) {
            bail!("the temporary folder was replaced");
        }
        let link = OsStr::new("l");
        t.symlink(link, target)?;
        t.finish_link(link, &recorded(e), root);
        let moved = t.rename(link, dir, name);
        if moved.is_err() {
            let _ = t.remove_file(link);
        }
        Ok(moved?)
    });
    let _ = dir.remove_dir(&tmp);
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
    let mut held: Option<(Option<u8>, PathBuf, Dir)> = None;
    let mut made = Made::new();
    let mut taken = HashSet::new();
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
            match open_dir(top, &place.path, Some(&mut made)) {
                Ok(_) if place.renamed => {
                    out.say(format!("  {} was restored as {}", e.show(), to.join(&place.path).display()))
                }
                Ok(_) => {}
                Err((at, err)) => {
                    skip(out, to, &at, &err, Some(blocked));
                    skipped += 1;
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
        let (name, numbered) = free_name(&mut taken, place.drive, parent, name);
        let (path, name) = (parent.join(&name), name.as_os_str());
        let renamed =
            (place.renamed || numbered).then(|| format!("  {} was restored as {}", e.show(), to.join(&path).display()));
        if held.as_ref().is_none_or(|(d, p, _)| *d != place.drive || p != parent) {
            held = None;
            match open_dir(top, parent, Some(&mut made)) {
                Ok(fd) => {
                    // A folder that was already there may hold what a restore that died left.
                    if !id(&fd).is_some_and(|i| made.contains(&i)) {
                        sweep(&fd);
                    }
                    held = Some((place.drive, parent.to_path_buf(), fd));
                }
                Err((at, err)) => {
                    skip(out, to, &at, &err, Some(blocked));
                    skipped += 1;
                    continue;
                }
            }
        }
        let dir = &held.as_ref().unwrap().2;
        match &e.kind {
            Kind::Dir => {}
            Kind::Symlink(target) => match place_link(dir, name, target, e, root) {
                Ok(()) => renamed.into_iter().for_each(|line| out.say(line)),
                Err(err) => {
                    skip(out, to, &path, &err, None);
                    skipped += 1;
                }
            },
            Kind::File { chunks, size } => {
                let mut written = 0u64;
                // Each restore writes under a name of its own: one still running in the daemon after
                // its client was stopped must not have its half-written file taken, or renamed away,
                // by the next restore of the same file. Short, so it fits beside the longest name the
                // file may have; the ending keeps it out of backups.
                let mut old: OsString = name.to_owned();
                old.push(".sdb-tmp");
                let _ = dir.remove_file(&old);
                tmp_count += 1;
                let tmp: OsString = format!(".{run_tag}-{tmp_count}.sdb-tmp").into();
                let tmp = &tmp;
                // A brand new file, never one (or a link) that was waiting under this name.
                let mut f = match dir.create_file(tmp) {
                    Ok(f) => f,
                    Err(err) => {
                        skip(out, to, &path, &err.into(), None);
                        skipped += 1;
                        continue;
                    }
                };
                let mut unfinished = Unfinished { dir: dir.try_clone().ok(), name: tmp.clone() };
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
                    let _ = dir.remove_file(tmp);
                    return Err(err);
                }
                if written != *size {
                    let _ = dir.remove_file(tmp);
                    skip(out, to, &path, &anyhow!("restored {written} bytes but the backup recorded {size}"), None);
                    skipped += 1;
                    continue;
                }
                os::finish_file(&f, &recorded(e), root);
                drop(f);
                if let Err(err) = dir.rename(tmp, dir, name) {
                    let _ = dir.remove_file(tmp);
                    skip(out, to, &path, &err.into(), None);
                    skipped += 1;
                    continue;
                }
                unfinished.dir = None;
                files += 1;
                renamed.into_iter().for_each(|line| out.say(line));
            }
        }
    }
    drop(held);
    for (e, place) in entries.iter().zip(&places).rev().filter(|(e, _)| e.kind == Kind::Dir) {
        let Some((place, (_, top))) = place.as_ref().and_then(|p| Some((p, base(p.drive)?))) else { continue };
        // Re-opened by name, so checked to be the very folder made earlier.
        match open_dir(top, &place.path, None) {
            Ok(fd) if id(&fd).is_some_and(|i| made.contains(&i)) => fd.finish(&recorded(e), root),
            Ok(fd) => fd.finish_existing(&recorded(e), root),
            Err(_) => {}
        }
    }
    out.say(format!("restored {files} files ({}) into {}", human(bytes), all_to.display()));
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

    #[test]
    fn a_second_file_of_a_name_gets_a_number_where_case_does_not_count() {
        let mut taken = HashSet::new();
        let mut name =
            |n: &str| free_name(&mut taken, None, Path::new("d"), n.as_ref()).0.to_string_lossy().into_owned();
        assert_eq!(name("Readme.txt"), "Readme.txt");
        assert_eq!(name("other"), "other");
        if os::CASELESS {
            assert_eq!(name("README.TXT"), "README (2).TXT");
            assert_eq!(name("readme.txt"), "readme (3).txt");
            assert_eq!(name("OTHER"), "OTHER (2)");
        } else {
            assert_eq!(name("README.TXT"), "README.TXT");
        }
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
        assert!(open_dir(&top, Path::new("real/deeper"), Some(&mut Made::new())).is_ok());
        assert!(open_dir(&top, Path::new("link"), Some(&mut Made::new())).is_err());
        assert!(open_dir(&top, Path::new("link/sub"), Some(&mut Made::new())).is_err());
        assert!(open_dir(&top, Path::new("real/../outside"), Some(&mut Made::new())).is_err());
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
        let new = open_dir(&top, Path::new("planted/new"), Some(&mut made)).unwrap();
        assert_eq!(made.len(), 1);
        assert!(made.contains(&id(&new).unwrap()));
        let fd = open_dir(&top, Path::new("planted"), None).unwrap();
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
