// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("SDB_HOME") {
        return h.into();
    }
    PathBuf::from(std::env::var_os("HOME").expect("HOME is not set")).join(".super-duper-backup")
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Retention {
    pub keep_last: u32,
    pub keep_hourly: u32,
    pub keep_daily: u32,
    pub keep_weekly: u32,
    pub keep_monthly: u32,
    /// Restore points older than this many days are deleted (0 = no age limit).
    #[serde(default)]
    pub keep_days: u32,
    /// Most versions kept of any one file (0 = unlimited).
    #[serde(default)]
    pub max_versions: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VersionRule {
    pub pattern: String,
    pub max_versions: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Retention { keep_last: 24, keep_hourly: 48, keep_daily: 30, keep_weekly: 12, keep_monthly: 24, keep_days: 0, max_versions: 0 }
    }
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Friend {
    pub name: String,
    pub id: String,
    pub quota_gb: u64,
    #[serde(default = "yes")]
    pub backup_to: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default)]
    pub addrs: Vec<SocketAddr>,
    /// Minutes between automatic backups to this destination (0 = default: 60 for a friend, the general setting for a drive).
    #[serde(default)]
    pub every_minutes: u32,
    /// Set for a local destination (an external drive or folder) instead of a friend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Invite {
    pub token: String,
    pub expires: i64,
}

fn d_settle() -> u64 {
    30
}
fn d_min_interval() -> u64 {
    300
}
fn d_rescan() -> u64 {
    6
}
fn d_min_free() -> u64 {
    5
}
fn d_warn() -> u32 {
    3
}
fn d_quota() -> u64 {
    100
}

/// A named group of folders with its own destinations, pace and history rules.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BackupSet {
    pub name: String,
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Names of the friends or drives this set goes to; empty means every one that is switched on.
    #[serde(default)]
    pub destinations: Vec<String>,
    /// Minutes between automatic backups of this set (0 = each destination's own pace).
    #[serde(default)]
    pub every_minutes: u32,
    /// Hours this set may back up automatically, such as "22-7" (empty = the general setting).
    #[serde(default)]
    pub hours: String,
    /// History rules for this set (absent = the general rules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
}

impl BackupSet {
    pub fn sends_to(&self, f: &Friend) -> bool {
        f.backup_to && (self.destinations.is_empty() || self.destinations.contains(&f.name))
    }

    /// Seconds between automatic backups of this set to one destination.
    pub fn interval_secs(&self, f: &Friend, cfg: &Config) -> u64 {
        if self.every_minutes > 0 { self.every_minutes as u64 * 60 } else { f.interval_secs(cfg) }
    }
}

pub const DEFAULT_SET: &str = "default";

pub const CONFIG_FORMAT: u32 = 1;

fn d_format() -> u32 {
    CONFIG_FORMAT
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default = "d_format")]
    pub format: u32,
    #[serde(default)]
    pub created: String,
    pub name: String,
    #[serde(default = "d_settle")]
    pub settle_secs: u64,
    #[serde(default = "d_min_interval")]
    pub min_interval_secs: u64,
    #[serde(default = "d_rescan")]
    pub rescan_hours: u64,
    #[serde(default = "d_quota")]
    pub default_quota_gb: u64,
    /// Warn when a destination has had no successful backup for this many days (0 = never warn).
    #[serde(default = "d_warn")]
    pub warn_after_days: u32,
    /// Upload speed limit in KB/s across all friends (0 = unlimited).
    #[serde(default)]
    pub upload_kbps: u64,
    /// Hours of the day automatic backups may run, such as "22-7" (empty = any time).
    #[serde(default)]
    pub backup_hours: String,
    /// Automatic backups are paused until this unix time.
    #[serde(default)]
    pub paused_until: i64,
    /// Stop accepting friends' data when this computer's disk has less than this many GB free.
    #[serde(default = "d_min_free")]
    pub min_free_gb: u64,
    /// UDP port this node listens on (0 = pick one at random each start).
    #[serde(default)]
    pub port: u16,
    /// Ask the router (UPnP, NAT-PMP, PCP) to forward the port so friends can connect directly.
    #[serde(default = "yes")]
    pub upnp: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_dir: Option<PathBuf>,
    /// Older configs listed folders here; they move into the default set on load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<PathBuf>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub version_rules: Vec<VersionRule>,
    #[serde(default)]
    pub sets: Vec<BackupSet>,
    #[serde(default)]
    pub friends: Vec<Friend>,
    #[serde(default)]
    pub invites: Vec<Invite>,
}

impl Config {
    pub fn new(name: String) -> Self {
        Config {
            format: CONFIG_FORMAT,
            created: chrono::Utc::now().to_rfc3339(),
            name,
            settle_secs: d_settle(),
            min_interval_secs: d_min_interval(),
            rescan_hours: d_rescan(),
            default_quota_gb: d_quota(),
            warn_after_days: d_warn(),
            upload_kbps: 0,
            backup_hours: String::new(),
            paused_until: 0,
            min_free_gb: d_min_free(),
            port: 20000 + rand::random::<u16>() % 40000,
            upnp: true,
            store_dir: None,
            paths: vec![],
            exclude: vec!["**/.DS_Store".into(), "**/.Trash".into(), "**/*.sdb-tmp".into()],
            retention: Retention::default(),
            version_rules: vec![],
            sets: vec![BackupSet { name: DEFAULT_SET.into(), ..Default::default() }],
            friends: vec![],
            invites: vec![],
        }
    }

    pub fn load(home: &Path) -> Result<Self> {
        let p = home.join("config.toml");
        let s = fs::read_to_string(&p)
            .with_context(|| format!("cannot read {} (run `sdb init` first)", p.display()))?;
        let mut c: Config = toml::from_str(&s).with_context(|| format!("bad config {}", p.display()))?;
        if c.sets.is_empty() {
            c.sets.push(BackupSet { name: DEFAULT_SET.into(), ..Default::default() });
        }
        if !c.paths.is_empty() {
            let old = std::mem::take(&mut c.paths);
            c.sets[0].paths.extend(old);
        }
        if c.format > CONFIG_FORMAT {
            anyhow::bail!("{} is in format {} but this sdb only reads up to {CONFIG_FORMAT}; upgrade sdb", p.display(), c.format);
        }
        Ok(c)
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        write_private(&home.join("config.toml"), toml::to_string_pretty(self)?.as_bytes())
    }

    pub fn store_dir(&self, home: &Path) -> PathBuf {
        self.store_dir.clone().unwrap_or_else(|| home.join("store"))
    }
}

pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Identity {
    #[serde(default = "d_format")]
    pub format: u32,
    #[serde(default)]
    pub created: String,
    pub secret: String,
    pub master: String,
}

impl Identity {
    pub fn generate() -> Self {
        Identity {
            format: 1,
            created: chrono::Utc::now().to_rfc3339(),
            secret: hex::encode(rand::random::<[u8; 32]>()),
            master: hex::encode(rand::random::<[u8; 32]>()),
        }
    }

    pub fn load(home: &Path) -> Result<Self> {
        let p = home.join("identity.key");
        let s = fs::read_to_string(&p).with_context(|| format!("cannot read {}", p.display()))?;
        Ok(serde_json::from_str(&s)?)
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        write_private(&home.join("identity.key"), serde_json::to_string(self)?.as_bytes())
    }

    pub fn bytes(s: &str) -> Result<[u8; 32]> {
        let v = hex::decode(s)?;
        v.try_into().map_err(|_| anyhow::anyhow!("bad key length"))
    }
}

/// Parses "22-7" into (22, 7); the window may wrap past midnight.
pub fn parse_hours(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.trim().split_once('-')?;
    let (a, b): (u32, u32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (a < 24 && b <= 24 && a != b).then_some((a, b % 24))
}

impl Config {
    /// Why automatic backups are being held back right now, if they are.
    pub fn held(&self) -> Option<String> {
        self.held_with(&self.backup_hours)
    }

    /// The same for one set, whose own hours take the place of the general ones.
    pub fn held_for(&self, set: &BackupSet) -> Option<String> {
        self.held_with(if set.hours.is_empty() { &self.backup_hours } else { &set.hours })
    }

    fn held_with(&self, hours: &str) -> Option<String> {
        use chrono::Timelike;
        let now = chrono::Local::now();
        if self.paused_until > now.timestamp() {
            return Some("paused".into());
        }
        let (a, b) = parse_hours(hours)?;
        let h = now.hour();
        let inside = if a < b { h >= a && h < b } else { h >= a || h < b };
        (!inside).then(|| format!("outside the backup hours {hours}"))
    }

    pub fn all_paths(&self) -> Vec<PathBuf> {
        self.sets.iter().flat_map(|s| s.paths.iter().cloned()).collect()
    }

    pub fn set(&self, name: &str) -> Option<&BackupSet> {
        self.sets.iter().find(|s| s.name == name)
    }
}

impl Friend {
    /// Seconds between automatic backups to this destination.
    pub fn interval_secs(&self, cfg: &Config) -> u64 {
        match (self.every_minutes, &self.path) {
            (0, None) => 3600,
            (0, Some(_)) => cfg.min_interval_secs,
            (n, _) => n as u64 * 60,
        }
    }
}
