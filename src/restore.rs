// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::Friend,
    crypto::Id,
    node::{Node, Out},
    snapshot::{Entry, Kind, human},
};
use anyhow::{Context, Result};
use filetime::FileTime;
use std::{
    ffi::OsStr,
    fs,
    io::Write,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
};

/// Where an entry lands: under the name of the folder that was backed up
/// (`~/Documents/a.txt` into `out` gives `out/Documents/a.txt`), or at its full
/// original path when restoring in place or when two backed up folders share a name.
fn dest(to: &Path, e: &Entry, roots: &[PathBuf]) -> PathBuf {
    let p = e.path();
    let base = roots.iter().find(|r| p.starts_with(r)).and_then(|r| r.parent());
    to.join(p.strip_prefix(base.unwrap_or(Path::new("/"))).unwrap_or(&p))
}

fn finish(path: &Path, e: &Entry, root: bool) {
    if !matches!(e.kind, Kind::Symlink(_)) {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(e.mode & 0o7777));
    }
    if root {
        let _ = std::os::unix::fs::lchown(path, Some(e.uid), Some(e.gid));
    }
    let t = FileTime::from_unix_time(e.mtime.div_euclid(1_000_000_000), e.mtime.rem_euclid(1_000_000_000) as u32);
    #[allow(deprecated)]
    let _ = filetime::set_symlink_file_times(path, t, t);
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
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut told = std::time::Instant::now();
    for e in &entries {
        let path = dest(to, e, roots);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match &e.kind {
            Kind::Dir => fs::create_dir_all(&path)?,
            Kind::Symlink(target) => {
                let _ = fs::remove_file(&path);
                std::os::unix::fs::symlink(OsStr::from_bytes(target), &path)?;
                finish(&path, e, root);
            }
            Kind::File { chunks, size } => {
                let mut written = 0u64;
                let mut name = path.clone().into_os_string();
                name.push(".sdb-tmp");
                let tmp = PathBuf::from(name);
                let mut f = fs::File::create(&tmp).with_context(|| tmp.display().to_string())?;
                for batch in chunks.chunks(8) {
                    let batch: &[Id] = batch;
                    let got = node.fetch(friends, batch).await;
                    if got.is_err() {
                        let _ = fs::remove_file(&tmp);
                    }
                    for data in got.with_context(|| e.show())? {
                        bytes += data.len() as u64;
                        written += data.len() as u64;
                        if told.elapsed().as_secs() >= 2 {
                            told = std::time::Instant::now();
                            out.say(format!("  ... {files} files, {} restored so far", human(bytes)));
                        }
                        f.write_all(&data)?;
                    }
                }
                drop(f);
                if written != *size {
                    let _ = fs::remove_file(&tmp);
                    anyhow::bail!("{}: restored {written} bytes but the backup recorded {size}", e.show());
                }
                fs::rename(&tmp, &path)?;
                finish(&path, e, root);
                files += 1;
            }
        }
    }
    for e in entries.iter().rev().filter(|e| e.kind == Kind::Dir) {
        finish(&dest(to, e, roots), e, root);
    }
    out.say(format!("restored {files} files ({}) into {}", human(bytes), to.display()));
    Ok(())
}
