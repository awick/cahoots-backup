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
    let docs = match docs {
        Some(d) => d,
        None => crate::os::user_home()?.join("Documents"),
    };
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
    config::write_carried(file, kit.as_bytes()).with_context(|| file.display().to_string())?;
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
    config::check_new_home(home)?;
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
#[derive(Serialize, Deserialize, Clone)]
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
    /// Proof this node wrote the record: a MAC of the rest under a key made from the master
    /// key. Where the kit goes and the key it is sealed with are only taken from a record
    /// that carries it, so one planted or rewritten by someone else is ignored.
    #[serde(default)]
    mac: String,
}

impl Copy {
    /// A record from before the MAC that `taken` has not yet found this node wrote: no kit is
    /// written from it until then.
    pub fn unchecked(&self) -> bool {
        self.mac.is_empty()
    }
}

const COPY: &str = "recovery-copy.json";

/// Held while the record is read and written, so a copy that is being written cannot put
/// back a record that `copy_off` removed or `copy_on` replaced meanwhile.
static RECORD: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Held while a copy is written, so two never write the folder at once.
static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn record_lock() -> std::sync::MutexGuard<'static, ()> {
    RECORD.lock().unwrap_or_else(|e| e.into_inner())
}

/// The MAC of a record, under a key only this node's master key gives. It covers what says
/// where the kit goes and how it is sealed (the folder, the salt and key), and what is done
/// to that folder after (the file written, and later removed, and the fingerprint that
/// decides whether it is written again), each with its length in front so no two records
/// read alike. When it was last written, and how that went, are only shown, and are left
/// out, as is anything added later, so a record does not stop being taken when it grows.
fn copy_mac(home: &Path, c: &Copy) -> Result<String> {
    let master = Identity::bytes(&Identity::load(home)?.master)?;
    let key = blake3::derive_key("cahoots v1 recovery-copy record", &master);
    let mut h = blake3::Hasher::new_keyed(&key);
    let folder = c.folder.as_os_str().as_encoded_bytes();
    for part in [folder, c.salt.as_bytes(), c.key.as_bytes(), c.file.as_bytes(), c.hash.as_bytes()] {
        h.update(&(part.len() as u64).to_le_bytes()).update(part);
    }
    Ok(h.finalize().to_hex().to_string())
}

/// Whether two records say to write the same kit, sealed the same way, to the same place.
fn same_copy(a: &Copy, b: &Copy) -> bool {
    (&a.folder, &a.salt, &a.key) == (&b.folder, &b.salt, &b.key)
}

pub fn copy_load(home: &Path) -> Option<Copy> {
    let _record = record_lock();
    load(home)
}

/// The record, if this node wrote it, or one from before the MAC, which it may have: what is
/// shown of the copy. Only `taken` lets a kit be written from it. Called with RECORD held.
fn load(home: &Path) -> Option<Copy> {
    let c: Copy = fs::read(home.join(COPY)).ok().and_then(|b| serde_json::from_slice(&b).ok())?;
    if c.mac.is_empty() {
        return Some(c);
    }
    let mac = copy_mac(home, &c).ok()?;
    (crypto_eq(&c.mac, &mac)).then_some(c)
}

/// The record a kit may be written from. Releases before the MAC wrote none. Such a record is
/// taken on proof that this node wrote it (see `wrote_kit`), and saved again with a MAC, so a
/// kit copy kept since then goes on being written. Leaving the MAC out of a planted record so
/// gains nothing. The proof is read from the folder, which may be slow or away, so RECORD is
/// not held meanwhile and what only shows the copy goes on. A record whose kit cannot be read
/// is left as it is, and tried again next time.
fn taken(home: &Path) -> Result<Option<Copy>> {
    let Some(c) = copy_load(home) else { return Ok(None) };
    if !c.mac.is_empty() {
        return Ok(Some(c));
    }
    if !wrote_kit(home, &c) {
        // With the folder away the kit may yet be read. With the folder there, it may only be
        // held up (a sync program holding the file, a file not yet downloaded), so it is tried
        // again; but a kit that is gone or not this node's never will be: then the copy has to
        // be set up again.
        let why = if c.folder.is_dir() {
            format!(
                "it was set up by an older release and the kit there is missing or could not be read to check it, \
                 so nothing is written there for now. It is tried again; if this keeps up, set it up again with \
                 `cahoots recovery copy-to \"{}\"`",
                c.folder.display()
            )
        } else {
            "it was set up by an older release, and is not written until its folder is back and the kit there can \
             be read to check it"
                .to_string()
        };
        // Said in the record too, still without a MAC, so `status` stops saying a copy is kept.
        let _record = record_lock();
        if let Some(now) = load(home).filter(|now| now.mac.is_empty() && same_copy(now, &c)) {
            let said = Copy { error: Some(why.clone()), ..now };
            config::write_private(&home.join(COPY), &serde_json::to_vec(&said)?)?;
        }
        bail!("the copy in {}: {why}", c.folder.display());
    }
    let _record = record_lock();
    match load(home) {
        // Unless it was replaced or removed meanwhile.
        Some(now) if now.mac.is_empty() && same_copy(&now, &c) && (&now.file, &now.hash) == (&c.file, &c.hash) => {
            save(home, &now)?;
            Ok(load(home))
        }
        Some(now) if !now.mac.is_empty() => Ok(Some(now)),
        _ => Ok(None),
    }
}

/// Whether the kit a record says it keeps is there, opens with the record's key and holds
/// this node's master key: only one who had that key could have sealed it.
fn wrote_kit(home: &Path, c: &Copy) -> bool {
    let proved = || {
        let key: [u8; 32] = hex::decode(&c.key).ok()?.try_into().ok()?;
        // One plain name, as this node writes: never a path out of the folder.
        let mut parts = Path::new(&c.file).components();
        let (Some(std::path::Component::Normal(_)), None) = (parts.next(), parts.next()) else { return None };
        let kit = read_kit(&c.folder.join(&c.file))?;
        let data: KitData = serde_json::from_slice(&crypto::kit_open_key(&key, &kit).ok()?).ok()?;
        Some(crypto_eq(&data.identity.master, &Identity::load(home).ok()?.master))
    };
    proved().unwrap_or(false)
}

fn crypto_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Writes the record with its MAC. Called with RECORD held.
fn save(home: &Path, c: &Copy) -> Result<()> {
    let c = Copy { mac: copy_mac(home, c)?, ..c.clone() };
    config::write_private(&home.join(COPY), &serde_json::to_vec(&c)?)
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
        mac: String::new(),
    };
    {
        let _record = record_lock();
        save(home, &copy)?;
    }
    copy_sync(home, true).map(|_| ())
}

/// Stops keeping the copy. What is in the folder stays there.
pub fn copy_off(home: &Path) -> Result<bool> {
    let _record = record_lock();
    match fs::remove_file(home.join(COPY)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Writes the kit to the folder if it changed since the last copy. Returns whether it did.
pub fn copy_sync(home: &Path, force: bool) -> Result<bool> {
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut copy) = taken(home)? else { return Ok(false) };
    // The folder may be slow: the record is not held while the kit is written there.
    let res = copy_write(home, force, &mut copy);
    copy.error = res.as_ref().err().map(|e| format!("{e:#}"));
    finish(home, &copy)?;
    res
}

/// Saves what writing a copy found, only if it is still the copy being kept: `copy_off` or a
/// new folder or passphrase meanwhile wins over it, and so does a record with no MAC: only
/// `taken` gives one.
fn finish(home: &Path, copy: &Copy) -> Result<()> {
    let _record = record_lock();
    if load(home).is_some_and(|now| !now.mac.is_empty() && same_copy(&now, copy)) {
        save(home, copy)?;
    }
    Ok(())
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
    config::write_carried(&path, kit.as_bytes()).with_context(|| path.display().to_string())?;
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

#[cfg(test)]
mod copy_tests {
    use super::*;

    fn home() -> PathBuf {
        let home = std::env::temp_dir().join(format!("sdb-test-copy-{}", rand::random::<u64>()));
        fs::create_dir_all(&home).unwrap();
        Identity::generate().save(&home).unwrap();
        home
    }

    fn record(folder: &str) -> Copy {
        Copy {
            folder: folder.into(),
            salt: "00".repeat(16),
            key: "11".repeat(32),
            file: String::new(),
            hash: String::new(),
            at: 0,
            error: None,
            mac: String::new(),
        }
    }

    #[test]
    fn a_record_this_node_did_not_write_is_ignored() {
        let home = home();
        {
            let _r = record_lock();
            save(&home, &record("/mine")).unwrap();
        }
        assert_eq!(copy_load(&home).unwrap().folder, PathBuf::from("/mine"));
        // Someone rewrites where the kit goes, or plants a record of their own with a MAC
        // under another key: neither is taken.
        let text = fs::read_to_string(home.join(COPY)).unwrap();
        fs::write(home.join(COPY), text.replace("/mine", "/theirs")).unwrap();
        assert!(copy_load(&home).is_none());
        let other = home.join("other");
        fs::create_dir_all(&other).unwrap();
        Identity::generate().save(&other).unwrap();
        {
            let _r = record_lock();
            save(&other, &record("/theirs")).unwrap();
        }
        fs::copy(other.join(COPY), home.join(COPY)).unwrap();
        assert!(copy_load(&home).is_none());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn what_is_only_shown_or_added_later_is_left_out_of_the_mac() {
        let home = home();
        {
            let _r = record_lock();
            save(&home, &record("/mine")).unwrap();
        }
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(home.join(COPY)).unwrap()).unwrap();
        v["at"] = 7.into();
        v["error"] = "the folder was not there".into();
        v["added_later"] = true.into();
        fs::write(home.join(COPY), serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(copy_load(&home).unwrap().at, 7);
        // What it does with the folder is held to it.
        for (field, to) in [("file", "../elsewhere"), ("hash", "00"), ("salt", "22"), ("key", "33")] {
            let mut bad = v.clone();
            bad[field] = to.into();
            fs::write(home.join(COPY), serde_json::to_vec(&bad).unwrap()).unwrap();
            assert!(copy_load(&home).is_none(), "{field}");
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_record_from_before_the_mac_is_kept_and_given_one() {
        let home = home();
        Config::new("me".into()).save(&home).unwrap();
        let kept = home.join("kept");
        fs::create_dir_all(&kept).unwrap();
        let old = Copy { folder: kept.clone(), file: "me.kit".into(), ..record("") };
        let write = |c: &Copy| fs::write(home.join(COPY), serde_json::to_vec(c).unwrap()).unwrap();
        // Without the kit it says it keeps, a record with no MAC proves nothing: it waits. It is
        // shown as it is all the same, and showing it does not read the folder.
        write(&old);
        assert!(taken(&home).is_err());
        assert_eq!(copy_load(&home).unwrap().folder, kept);
        // Nor does a kit sealed with its key by someone who has not this node's master key.
        let (plain, _, node) = plain(&home).unwrap();
        let seal = |plain: &[u8]| crypto::kit_seal_key(&[0x11; 32], &[0; 16], plain, &node).unwrap();
        let mut theirs: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        theirs["identity"]["master"] = serde_json::to_value(Identity::generate().master).unwrap();
        fs::write(kept.join("me.kit"), seal(&serde_json::to_vec(&theirs).unwrap())).unwrap();
        assert!(taken(&home).is_err());
        // Nor one with a file name that leads out of its folder.
        fs::write(kept.join("me.kit"), seal(&plain)).unwrap();
        write(&Copy { folder: home.clone(), file: "kept/me.kit".into(), ..record("") });
        assert!(taken(&home).is_err());
        // Nor is one given a MAC by a copy that finishes meanwhile.
        finish(&home, &Copy { folder: home.clone(), file: "kept/me.kit".into(), ..record("") }).unwrap();
        assert!(copy_load(&home).unwrap().mac.is_empty());
        // The kit this node sealed with that key is the proof.
        write(&old);
        assert_eq!(taken(&home).unwrap().unwrap().folder, kept);
        let saved: Copy = serde_json::from_slice(&fs::read(home.join(COPY)).unwrap()).unwrap();
        assert!(!saved.mac.is_empty(), "saved again with a MAC");
        // From then on it is held to it.
        let text = fs::read_to_string(home.join(COPY)).unwrap();
        fs::write(home.join(COPY), text.replace("me.kit", "it.kit")).unwrap();
        assert!(copy_load(&home).is_none());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_copy_being_written_does_not_undo_copy_off_or_a_new_folder() {
        let home = home();
        let save_now = |c: &Copy| {
            let _r = record_lock();
            save(&home, c).unwrap();
        };
        save_now(&record("/old"));
        let mut writing = copy_load(&home).unwrap();
        writing.at = 1;
        // Turned off while that copy was being written.
        assert!(copy_off(&home).unwrap());
        finish(&home, &writing).unwrap();
        assert!(!home.join(COPY).exists(), "copy_off was undone");
        // Moved to another folder while it was being written.
        save_now(&record("/old"));
        let writing = copy_load(&home).unwrap();
        save_now(&record("/new"));
        finish(&home, &writing).unwrap();
        assert_eq!(copy_load(&home).unwrap().folder, PathBuf::from("/new"));
        // Nothing changed meanwhile: what it found is kept.
        let mut writing = copy_load(&home).unwrap();
        writing.at = 42;
        finish(&home, &writing).unwrap();
        assert_eq!(copy_load(&home).unwrap().at, 42);
        fs::remove_dir_all(home).unwrap();
    }
}
