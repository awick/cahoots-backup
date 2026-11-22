// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::SocketAddr,
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
        Retention {
            keep_last: 24,
            keep_hourly: 48,
            keep_daily: 30,
            keep_weekly: 12,
            keep_monthly: 24,
            keep_days: 0,
            max_versions: 0,
        }
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
    /// Fastest this friend may send backups to this computer, in KB/s (0 = unlimited).
    #[serde(default)]
    pub max_in_kbps: u64,
    /// Set for a local destination (an external drive or folder) instead of a friend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// For a drive: the mark written on it when it was added, so another disk mounted
    /// at the same place is not mistaken for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_key: Option<String>,
    /// Set for a cloud destination: an S3-compatible bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<Cloud>,
    /// History rules for what is kept at this destination (absent = each set's own rules).
    /// The per-file version limit does not apply here; that stays with the set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<Retention>,
}

fn d_region() -> String {
    "us-east-1".into()
}

/// Where a cloud destination lives. The keys are stored here, in the private config.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Cloud {
    pub endpoint: String,
    pub bucket: String,
    #[serde(default = "d_region")]
    pub region: String,
    /// Folder inside the bucket; this node's data goes under `<prefix>/<node id>/`.
    #[serde(default)]
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
    /// Address the bucket as `endpoint/bucket` instead of `bucket.endpoint` (MinIO and some others).
    #[serde(default)]
    pub path_style: bool,
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
    /// That is how "everywhere" has always been stored (the default set starts that way), so an
    /// empty list must only ever be written on purpose: see `EVERYWHERE` and `NOWHERE`.
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
    /// Names this set had before; restore points made under them still belong to it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_names: Vec<String>,
    /// The exclusion list this set uses (empty = Standard).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exclude_list: String,
    /// Folders and files inside this set's folders that were left out by hand.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<PathBuf>,
}

/// A named list of patterns for things not worth backing up. Each set uses one.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExcludeList {
    pub name: String,
    pub patterns: Vec<String>,
}

pub const STANDARD: &str = "Standard";
pub const DEVELOPER: &str = "Developer";

/// Left out everywhere, whatever list a set uses: our own half-written restore files.
pub const ALWAYS_EXCLUDED: &str = "**/*.sdb-tmp";

/// The lists that come with sdb. They cannot be edited, only copied, so a later release
/// can improve them. Nothing here is irreplaceable: caches, trash, and things a tool
/// downloads or builds again. `.git` is in neither, since it can hold unpushed work.
pub fn builtin_lists() -> Vec<ExcludeList> {
    let standard = [
        "**/.DS_Store",
        "**/.Trash",
        "**/.Trashes",
        "**/.Trash-*",
        "**/.Spotlight-V100",
        "**/.fseventsd",
        "**/.TemporaryItems",
        "**/.DocumentRevisions-V100",
        "**/Thumbs.db",
        "**/Library/Caches",
        "**/.cache",
        "**/node_modules",
        "**/__pycache__",
        "**/*.pyc",
    ];
    let developer =
        ["**/target", "**/build", "**/dist", "**/.venv", "**/.tox", "**/.gradle", "**/.mypy_cache", "**/.pytest_cache"];
    let own = |v: &[&str]| v.iter().map(|p| p.to_string()).collect::<Vec<_>>();
    vec![
        ExcludeList { name: STANDARD.into(), patterns: own(&standard) },
        ExcludeList { name: DEVELOPER.into(), patterns: [own(&standard), own(&developer)].concat() },
    ]
}

pub fn is_builtin_list(name: &str) -> bool {
    builtin_lists().iter().any(|l| l.name.eq_ignore_ascii_case(name))
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

/// Stands in a set's destination list when its last destination was removed, so the set
/// goes nowhere instead of everywhere. No friend or drive can carry this name.
pub const NOWHERE: &str = "(none)";

/// What a request to change a set's destinations sends to mean every destination; it is
/// stored as the empty list. An empty request is refused, so a list emptied by mistake never
/// widens a set. No friend or drive can carry this name.
pub const EVERYWHERE: &str = "(all)";

/// Words that mean something where destinations are named (`--to all`), so no friend, drive,
/// set or node may be called by one of them, in any case.
const RESERVED: &[&str] = &["all", "none", "everywhere", "nowhere", EVERYWHERE, NOWHERE];

pub fn reserved(name: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(name.trim()))
}

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
    /// Older configs kept one list of patterns for everything; it becomes lists on load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Exclusion lists made here, next to the built-in ones.
    #[serde(default)]
    pub exclude_lists: Vec<ExcludeList>,
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
            exclude: vec![],
            exclude_lists: vec![],
            retention: Retention::default(),
            version_rules: vec![],
            sets: vec![BackupSet { name: DEFAULT_SET.into(), ..Default::default() }],
            friends: vec![],
            invites: vec![],
        }
    }

    pub fn load(home: &Path) -> Result<Self> {
        let p = home.join("config.toml");
        let s =
            fs::read_to_string(&p).with_context(|| format!("cannot read {} (run `sdb init` first)", p.display()))?;
        let mut c: Config = toml::from_str(&s).with_context(|| format!("bad config {}", p.display()))?;
        if c.sets.is_empty() {
            c.sets.push(BackupSet { name: DEFAULT_SET.into(), ..Default::default() });
        }
        if !c.paths.is_empty() {
            let old = std::mem::take(&mut c.paths);
            c.sets[0].paths.extend(old);
        }
        c.adopt_lists();
        if c.format > CONFIG_FORMAT {
            anyhow::bail!(
                "{} is in format {} but this sdb only reads up to {CONFIG_FORMAT}; upgrade sdb",
                p.display(),
                c.format
            );
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

/// Writes a file only its owner can read. The temporary file is new, private from the
/// start and never opened through a link someone else left in the folder.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let tmp = path.with_extension(format!("tmp{:016x}", rand::random::<u64>()));
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    let done = f.write_all(data).and_then(|_| fs::rename(&tmp, path));
    if done.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(done?)
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

    /// The current name of the set a restore point was made under.
    pub fn set_name_now(&self, recorded: &str) -> String {
        if self.set(recorded).is_some() {
            return recorded.to_string();
        }
        self.sets
            .iter()
            .find(|b| b.former_names.iter().any(|n| n == recorded))
            .map(|b| b.name.clone())
            .unwrap_or_else(|| recorded.to_string())
    }

    pub fn set(&self, name: &str) -> Option<&BackupSet> {
        self.sets.iter().find(|s| s.name == name)
    }

    /// Every exclusion list: the built-in ones, then this node's own.
    pub fn lists(&self) -> Vec<ExcludeList> {
        let mut v = builtin_lists();
        v.extend(self.exclude_lists.iter().filter(|l| !is_builtin_list(&l.name)).cloned());
        v
    }

    /// The list a set uses; one that no longer exists falls back to Standard.
    pub fn list_for(&self, set: &BackupSet) -> ExcludeList {
        let mut all = self.lists();
        let at = all.iter().position(|l| l.name == set.exclude_list).unwrap_or(0);
        all.swap_remove(at)
    }

    /// Turns the single pattern list of older configs into what replaced it: every set
    /// uses Standard, folders left out by hand stay left out of the sets that hold them,
    /// and patterns typed in by hand are kept in a list of their own.
    fn adopt_lists(&mut self) {
        let old = std::mem::take(&mut self.exclude);
        let known = builtin_lists();
        let mut custom = vec![];
        for p in old {
            let plain = p.replace('\\', "");
            if p == ALWAYS_EXCLUDED || known.iter().any(|l| l.patterns.contains(&p)) {
                continue;
            }
            if plain.starts_with('/') && !plain.contains(['*', '?', '[', '{']) {
                let path = PathBuf::from(plain);
                for b in self.sets.iter_mut().filter(|b| b.paths.iter().any(|r| path.starts_with(r))) {
                    if !b.skip.contains(&path) {
                        b.skip.push(path.clone());
                    }
                }
            } else {
                custom.push(p);
            }
        }
        if !custom.is_empty() && !self.exclude_lists.iter().any(|l| l.name == "Custom") {
            let mut patterns = known[0].patterns.clone();
            patterns.extend(custom);
            self.exclude_lists.push(ExcludeList { name: "Custom".into(), patterns });
            for b in self.sets.iter_mut().filter(|b| b.exclude_list.is_empty()) {
                b.exclude_list = "Custom".into();
            }
        }
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

    /// Another person's node, as opposed to a drive or cloud storage of our own.
    pub fn is_friend(&self) -> bool {
        self.path.is_none() && self.cloud.is_none()
    }

    pub fn kind(&self) -> &'static str {
        match (&self.path, &self.cloud) {
            (Some(_), _) => "drive",
            (_, Some(_)) => "cloud",
            _ => "friend",
        }
    }

    /// The most this node may store there, in bytes. A cloud destination with no cap has no limit.
    pub fn quota_bytes(&self) -> u64 {
        if self.cloud.is_some() && self.quota_gb == 0 { u64::MAX } else { self.quota_gb.saturating_mul(1 << 30) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn friend(name: &str, drive: bool) -> Friend {
        Friend {
            name: name.into(),
            id: name.into(),
            quota_gb: 1,
            backup_to: true,
            relay: None,
            addrs: vec![],
            every_minutes: 0,
            max_in_kbps: 0,
            drive_key: None,
            cloud: None,
            retention: None,
            path: drive.then(|| PathBuf::from("/tmp")),
        }
    }

    #[test]
    fn an_old_pattern_list_becomes_lists() {
        let mut c = Config::new("x".into());
        c.sets[0].paths.push("/home/me/docs".into());
        c.sets.push(BackupSet { name: "other".into(), paths: vec!["/srv".into()], ..Default::default() });
        c.exclude =
            vec!["**/.DS_Store".into(), "**/*.sdb-tmp".into(), "/home/me/docs/big\\ stuff".into(), "**/*.iso".into()];
        c.adopt_lists();
        assert!(c.exclude.is_empty());
        assert_eq!(c.sets[0].skip, vec![PathBuf::from("/home/me/docs/big stuff")]);
        assert!(c.sets[1].skip.is_empty());
        let mine = c.list_for(&c.sets[1]);
        assert_eq!(mine.name, "Custom");
        assert!(
            mine.patterns.contains(&"**/*.iso".to_string()) && mine.patterns.contains(&"**/node_modules".to_string())
        );
        // A set pointing at a list that is gone uses Standard; built-in names cannot be taken over.
        c.sets[1].exclude_list = "gone".into();
        assert_eq!(c.list_for(&c.sets[1]).name, STANDARD);
        c.exclude_lists.push(ExcludeList { name: "standard".into(), patterns: vec![] });
        assert!(!c.list_for(&BackupSet::default()).patterns.is_empty());
        assert!(!builtin_lists().iter().any(|l| l.patterns.iter().any(|p| p.contains(".git"))));
    }

    #[test]
    fn hours() {
        assert_eq!(parse_hours("22-7"), Some((22, 7)));
        assert_eq!(parse_hours(" 9 - 17 "), Some((9, 17)));
        assert_eq!(parse_hours("7-7"), None);
        assert_eq!(parse_hours("25-3"), None);
        assert_eq!(parse_hours("always"), None);
    }

    #[test]
    fn pacing_defaults() {
        let cfg = Config::new("n".into());
        assert_eq!(friend("bob", false).interval_secs(&cfg), 3600);
        assert_eq!(friend("usb", true).interval_secs(&cfg), cfg.min_interval_secs);
        let mut f = friend("bob", false);
        f.every_minutes = 10;
        assert_eq!(f.interval_secs(&cfg), 600);
        let set = BackupSet { name: "s".into(), every_minutes: 5, ..Default::default() };
        assert_eq!(set.interval_secs(&f, &cfg), 300);
    }

    #[test]
    fn renamed_sets_keep_their_restore_points() {
        let mut cfg = Config::new("n".into());
        cfg.sets[0].name = "docs".into();
        cfg.sets[0].former_names = vec![DEFAULT_SET.into(), "documents".into()];
        cfg.sets.push(BackupSet { name: "photos".into(), ..Default::default() });
        assert_eq!(cfg.set_name_now("default"), "docs");
        assert_eq!(cfg.set_name_now("documents"), "docs");
        assert_eq!(cfg.set_name_now("photos"), "photos");
        assert_eq!(cfg.set_name_now("gone"), "gone");
        // A new set may reuse an old name; its own restore points are then its own.
        cfg.sets.push(BackupSet { name: "default".into(), ..Default::default() });
        assert_eq!(cfg.set_name_now("default"), "default");
    }

    #[test]
    fn set_destinations() {
        let all = BackupSet { name: "a".into(), ..Default::default() };
        let one = BackupSet { name: "b".into(), destinations: vec!["usb".into()], ..Default::default() };
        let mut off = friend("bob", false);
        assert!(all.sends_to(&off) && !one.sends_to(&off) && one.sends_to(&friend("usb", true)));
        off.backup_to = false;
        assert!(!all.sends_to(&off));
    }

    #[test]
    fn old_config_moves_folders_into_the_default_set() {
        let dir = std::env::temp_dir().join(format!("sdb-test-cfg-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), "name = \"old\"\npaths = [\"/home/me/Documents\"]\n").unwrap();
        let c = Config::load(&dir).unwrap();
        assert!(c.paths.is_empty());
        assert_eq!(c.sets.len(), 1);
        assert_eq!(c.sets[0].name, DEFAULT_SET);
        assert_eq!(c.all_paths(), vec![PathBuf::from("/home/me/Documents")]);
        c.save(&dir).unwrap();
        let again = Config::load(&dir).unwrap();
        assert_eq!(again.all_paths(), c.all_paths());
        assert_eq!(fs::metadata(dir.join("config.toml")).unwrap().permissions().mode() & 0o777, 0o600);

        // A link planted where the old fixed temporary name was is not written through.
        fs::write(dir.join("victim"), "untouched").unwrap();
        std::os::unix::fs::symlink(dir.join("victim"), dir.join("config.tmp")).unwrap();
        c.save(&dir).unwrap();
        assert_eq!(fs::read_to_string(dir.join("victim")).unwrap(), "untouched");

        fs::write(dir.join("config.toml"), "name = \"new\"\nformat = 99\n").unwrap();
        assert!(Config::load(&dir).is_err(), "a newer config format must be refused");
        fs::remove_dir_all(&dir).unwrap();
    }
}
