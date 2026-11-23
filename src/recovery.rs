// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The recovery kit: this computer's identity, key and settings in one passphrase-protected file.

use crate::{
    config::{self, Config, Identity},
    crypto,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct KitData {
    identity: Identity,
    config: Config,
    /// Restore points deleted on purpose, so a destination cannot bring them back after recovery.
    #[serde(default)]
    forgotten: HashSet<String>,
}

/// When the kit was last written and which destinations it knew, kept beside the config.
#[derive(Serialize, Deserialize)]
struct Marker {
    at: i64,
    friends: Vec<String>,
}

const MARKER: &str = "recovery-kit.json";

/// Whether a kit was written from this folder, and whether it still knows every destination.
#[derive(Debug, PartialEq)]
pub enum Kit {
    Never,
    Current(i64),
    Stale(i64),
}

/// Fewest characters the passphrase of a new kit may have. Length is the only rule.
pub const MIN_PASSPHRASE: usize = 16;

pub fn check_passphrase(p: &str) -> Result<()> {
    let n = p.chars().count();
    if n < MIN_PASSPHRASE {
        bail!("that is {n} characters; use at least {MIN_PASSPHRASE}. A sentence you will remember works well");
    }
    Ok(())
}

pub(crate) fn mark(home: &Path, cfg: &Config) -> Result<()> {
    let m = Marker { at: chrono::Utc::now().timestamp(), friends: cfg.friends.iter().map(|f| f.id.clone()).collect() };
    config::write_private(&home.join(MARKER), &serde_json::to_vec(&m)?)
}

/// The file a computer's kit goes in unless another is named: NAME.cahoots-recovery-kit.
pub fn file_name(name: &str) -> String {
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    format!("{}.{EXT}", if safe.is_empty() { "cahoots" } else { &safe })
}

pub const EXT: &str = "cahoots-recovery-kit";

/// Where a kit is offered to be kept: the Documents folder on a Mac or on Windows, which
/// iCloud Drive or OneDrive carries off the computer when it is set to.
pub fn default_folder() -> Option<PathBuf> {
    // On Windows, Documents is usually the one OneDrive keeps, when OneDrive is set up.
    let synced = std::env::var_os("OneDrive").filter(|_| cfg!(windows)).map(PathBuf::from);
    let docs = synced.map(|d| d.join("Documents")).filter(|d| d.is_dir());
    let docs = docs.unwrap_or(crate::os::user_home()?.join("Documents"));
    ((cfg!(target_os = "macos") || cfg!(windows)) && docs.is_dir()).then_some(docs)
}

/// Everything a kit holds, before it is sealed, the settings among it, and whose it is.
pub fn plain(home: &Path) -> Result<(Vec<u8>, Config, String)> {
    let mut config = Config::load(home)?;
    config.invites.clear();
    let forgotten = match fs::read(home.join("forgotten.json")) {
        Ok(b) => serde_json::from_slice(&b).context("forgotten.json")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e).context("forgotten.json"),
    };
    let data = KitData { identity: Identity::load(home)?, config, forgotten };
    let node = iroh::SecretKey::from_bytes(&Identity::bytes(&data.identity.secret)?).public().to_string();
    Ok((serde_json::to_vec(&data)?, data.config, node))
}

pub fn export(home: &Path, passphrase: &str, file: &Path) -> Result<()> {
    check_passphrase(passphrase)?;
    let (plain, config, node) = plain(home)?;
    let kit = crypto::kit_seal(passphrase, &plain, &node)?;
    config::write_private(file, kit.as_bytes()).with_context(|| file.display().to_string())?;
    mark(home, &config)
}

/// Sets up an empty folder from a kit file; returns the settings it held.
pub fn import(home: &Path, passphrase: &str, file: &Path) -> Result<Config> {
    let kit = fs::read_to_string(file).with_context(|| file.display().to_string())?;
    import_kit(home, passphrase, &kit)
}

pub fn import_kit(home: &Path, passphrase: &str, kit: &str) -> Result<Config> {
    if home.join("identity.key").exists() {
        bail!("{} is already set up; move it away first", home.display());
    }
    let data: KitData = serde_json::from_slice(&crypto::kit_open(passphrase, kit)?)?;
    fs::create_dir_all(home)?;
    crate::os::private_dir(home)?;
    data.identity.save(home)?;
    data.config.save(home)?;
    if !data.forgotten.is_empty() {
        config::write_private(&home.join("forgotten.json"), &serde_json::to_vec(&data.forgotten)?)?;
    }
    mark(home, &data.config)?;
    Ok(data.config)
}

pub fn state(home: &Path, cfg: &Config) -> Kit {
    let m: Option<Marker> = fs::read(home.join(MARKER)).ok().and_then(|b| serde_json::from_slice(&b).ok());
    match m {
        None => Kit::Never,
        Some(m) if cfg.friends.iter().all(|f| m.friends.contains(&f.id)) => Kit::Current(m.at),
        Some(m) => Kit::Stale(m.at),
    }
}

impl Kit {
    /// One line for the screens, and whether it is something to act on.
    pub fn describe(&self) -> (String, bool) {
        let day = |t: &i64| chrono::DateTime::from_timestamp(*t, 0).map(|d| d.format("%Y-%m-%d").to_string());
        match self {
            Kit::Never => ("never exported: without it, losing this computer loses your backups".into(), true),
            Kit::Stale(t) => (
                format!("exported {}, before your newest friend or drive: export it again", day(t).unwrap_or_default()),
                true,
            ),
            Kit::Current(t) => (format!("exported {}, up to date", day(t).unwrap_or_default()), false),
        }
    }
}

/// A copy of the kit kept in a folder of the user's choosing, such as one a cloud drive
/// syncs, and written again whenever the kit changes. Held beside the config.
#[derive(Serialize, Deserialize)]
pub struct Copy {
    pub folder: PathBuf,
    /// The key the passphrase made, so later kits can be sealed without asking for it again.
    /// It sits beside the master key it protects, so it gives away nothing more than that file.
    salt: String,
    key: String,
    /// Name of the kit's file in the folder.
    #[serde(default)]
    file: String,
    /// Fingerprint of the kit last written.
    #[serde(default)]
    hash: String,
    #[serde(default)]
    pub at: i64,
    #[serde(default)]
    pub error: Option<String>,
}

const COPY: &str = "recovery-copy.json";

pub fn copy_load(home: &Path) -> Option<Copy> {
    fs::read(home.join(COPY)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn copy_save(home: &Path, c: &Copy) -> Result<()> {
    config::write_private(&home.join(COPY), &serde_json::to_vec(c)?)
}

/// Starts keeping a copy in `folder` and writes the first one. Slow on purpose: it makes
/// the key from the passphrase.
pub fn copy_on(home: &Path, passphrase: &str, folder: &Path) -> Result<()> {
    check_passphrase(passphrase)?;
    if !folder.is_dir() {
        bail!("{} is not a folder", folder.display());
    }
    let salt: [u8; 16] = rand::random();
    let key = crypto::kit_key(passphrase, &salt)?;
    let copy = Copy {
        folder: folder.to_path_buf(),
        salt: hex::encode(salt),
        key: hex::encode(key),
        file: String::new(),
        hash: String::new(),
        at: 0,
        error: None,
    };
    copy_save(home, &copy)?;
    copy_sync(home, true).map(|_| ())
}

/// Stops keeping the copy. What is in the folder stays there.
pub fn copy_off(home: &Path) -> Result<bool> {
    match fs::remove_file(home.join(COPY)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Writes the kit to the folder if it changed since the last copy. Returns whether it did.
pub fn copy_sync(home: &Path, force: bool) -> Result<bool> {
    let Some(mut copy) = copy_load(home) else { return Ok(false) };
    let res = copy_write(home, force, &mut copy);
    copy.error = res.as_ref().err().map(|e| format!("{e:#}"));
    copy_save(home, &copy)?;
    res
}

/// The kit this one replaced.
fn before(file: &str) -> String {
    format!("{file}.before")
}

/// A kit is a few KB; anything far larger in the folder is not one.
const MAX_KIT: u64 = 4 << 20;

/// Reads a kit from a folder others may write: never through a link, never waiting on a pipe
/// or device, and never more than a kit's size.
fn read_kit(path: &Path) -> Option<String> {
    use std::io::Read;
    let f = crate::os::open_file(path).ok()?;
    let md = f.metadata().ok()?;
    if !md.is_file() || md.len() > MAX_KIT {
        return None;
    }
    let mut s = String::new();
    f.take(MAX_KIT).read_to_string(&mut s).ok()?;
    Some(s)
}

fn copy_write(home: &Path, force: bool, copy: &mut Copy) -> Result<bool> {
    let (plain, cfg, node) = plain(home)?;
    let hash = blake3::hash(&plain).to_hex().to_string();
    // Named for the computer. Where another computer of the same name already keeps its kit
    // in this folder, the start of this one's id goes in the name rather than writing over it.
    let mut file = file_name(&cfg.name);
    let theirs = read_kit(&copy.folder.join(&file)).and_then(|k| crypto::kit_node(&k));
    if theirs.is_some_and(|n| n != node) {
        file = file_name(&format!("{}-{}", cfg.name, &node[..node.len().min(8)]));
    }
    let path = copy.folder.join(&file);
    if !force && hash == copy.hash && file == copy.file && path.exists() {
        return Ok(false);
    }
    if !copy.folder.is_dir() {
        bail!("{} is not there", copy.folder.display());
    }
    let unhex = |s: &str| hex::decode(s).ok();
    let key: [u8; 32] = unhex(&copy.key).and_then(|k| k.try_into().ok()).context(COPY)?;
    let salt: [u8; 16] = unhex(&copy.salt).and_then(|k| k.try_into().ok()).context(COPY)?;
    let kit = crypto::kit_seal_key(&key, &salt, &plain, &node)?;
    // The one before stays, in case this copy is the bad one.
    if path.exists() {
        fs::rename(&path, copy.folder.join(before(&file))).with_context(|| path.display().to_string())?;
    }
    config::write_private(&path, kit.as_bytes()).with_context(|| path.display().to_string())?;
    if !copy.file.is_empty() && copy.file != file {
        // The computer was renamed: its kit under the old name would only mislead.
        let _ = fs::remove_file(copy.folder.join(&copy.file));
        let _ = fs::remove_file(copy.folder.join(before(&copy.file)));
    }
    (copy.file, copy.hash, copy.at) = (file, hash, chrono::Utc::now().timestamp());
    mark(home, &cfg)?;
    Ok(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_kit_is_read_only_from_a_small_regular_file() {
        let dir = std::env::temp_dir().join(format!("sdb-test-kit-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("kit"), "sealed").unwrap();
        assert_eq!(read_kit(&dir.join("kit")).as_deref(), Some("sealed"));
        std::os::unix::fs::symlink(dir.join("kit"), dir.join("link")).unwrap();
        assert!(read_kit(&dir.join("link")).is_none());
        let fifo = dir.join("fifo");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        assert!(read_kit(&fifo).is_none());
        fs::File::create(dir.join("big")).unwrap().set_len(MAX_KIT + 1).unwrap();
        assert!(read_kit(&dir.join("big")).is_none());
        fs::remove_dir_all(dir).unwrap();
    }
}
