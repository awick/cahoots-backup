// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The recovery kit: this computer's identity, key and settings in one passphrase-protected file.

use crate::{
    config::{self, Config, Identity},
    crypto,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::Path};

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

pub fn check_passphrase(p: &str) -> Result<()> {
    if p.len() < 8 {
        bail!("use at least 8 characters");
    }
    Ok(())
}

pub(crate) fn mark(home: &Path, cfg: &Config) -> Result<()> {
    let m = Marker { at: chrono::Utc::now().timestamp(), friends: cfg.friends.iter().map(|f| f.id.clone()).collect() };
    config::write_private(&home.join(MARKER), &serde_json::to_vec(&m)?)
}

pub fn export(home: &Path, passphrase: &str, file: &Path) -> Result<()> {
    check_passphrase(passphrase)?;
    let mut config = Config::load(home)?;
    config.invites.clear();
    let forgotten = match fs::read(home.join("forgotten.json")) {
        Ok(b) => serde_json::from_slice(&b).context("forgotten.json")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(e) => return Err(e).context("forgotten.json"),
    };
    let data = KitData { identity: Identity::load(home)?, config, forgotten };
    let kit = crypto::kit_seal(passphrase, &serde_json::to_vec(&data)?)?;
    config::write_private(file, kit.as_bytes()).with_context(|| file.display().to_string())?;
    mark(home, &data.config)
}

/// Sets up an empty folder from a kit; returns the settings it held.
pub fn import(home: &Path, passphrase: &str, file: &Path) -> Result<Config> {
    if home.join("identity.key").exists() {
        bail!("{} is already set up; move it away first", home.display());
    }
    let kit = fs::read_to_string(file).with_context(|| file.display().to_string())?;
    let data: KitData = serde_json::from_slice(&crypto::kit_open(passphrase, &kit)?)?;
    fs::create_dir_all(home)?;
    fs::set_permissions(home, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
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
