// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Interactive setup: choose folders and friends, tune history, browse and restore.

use crate::{
    config::{BackupSet, Config, DEFAULT_SET},
    control,
    node::{Cmd, Node, Out},
    snapshot::{human, parse_when, when},
};
use anyhow::Result;
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;

pub enum Exec {
    Daemon(PathBuf),
    Local(Arc<Node>),
}

impl Exec {
    async fn call(&self, cmd: Cmd) -> Result<Vec<String>> {
        let (tx, _rx) = mpsc::unbounded_channel();
        self.live(cmd, &tx).await
    }

    /// Runs a command, passing each output line to `progress` as it arrives.
    async fn live(
        &self,
        cmd: Cmd,
        progress: &mpsc::UnboundedSender<String>,
    ) -> Result<Vec<String>> {
        let mut lines = vec![];
        let mut seen = |l: String| {
            let _ = progress.send(l.clone());
            lines.push(l);
        };
        match self {
            Exec::Daemon(sock) => {
                let stream = control::connect(sock)
                    .await
                    .ok_or_else(|| anyhow::anyhow!("the daemon is no longer running"))?;
                let res = control::request(stream, &cmd, &mut seen).await;
                if let Err(e) = res {
                    lines.push(format!("{e:#}"));
                    anyhow::bail!("{}", lines.join("\n"));
                }
            }
            Exec::Local(node) => {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let out = Out(tx);
                let res = {
                    let job = node.run(cmd, &out);
                    tokio::pin!(job);
                    loop {
                        tokio::select! {
                            r = &mut job => break r,
                            Some(l) = rx.recv() => seen(l),
                        }
                    }
                };
                while let Ok(l) = rx.try_recv() {
                    seen(l);
                }
                if let Err(e) = res {
                    lines.push(format!("{e:#}"));
                    anyhow::bail!("{}", lines.join("\n"));
                }
            }
        }
        Ok(lines
            .into_iter()
            .filter(|l| !l.trim_start().starts_with("..."))
            .collect())
    }
}

/// What a background job hands back to the screen.
enum Done {
    Popup(Vec<String>),
    BackedUp(Vec<String>),
    Paired(Vec<String>),
    Snaps(Vec<Snap>, Option<Listing>, Vec<String>, Option<String>),
    Tree(usize, Listing),
    Versions(Vec<String>),
    Failed(String),
}

fn lines_or_error(r: Result<Vec<String>>) -> Vec<String> {
    r.unwrap_or_else(|e| {
        format!("{e:#}")
            .lines()
            .filter(|l| !l.trim_start().starts_with("..."))
            .map(String::from)
            .collect()
    })
}

fn parse_listing(lines: Vec<String>) -> Listing {
    lines
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| {
            let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
            (
                s("p"),
                v["d"].as_bool().unwrap_or(false),
                v["s"].as_u64().unwrap_or(0),
                s("v"),
            )
        })
        .collect()
}

type Item = (String, bool, u64);

struct Row {
    path: String,
    depth: usize,
    dir: bool,
    open: bool,
    size: u64,
}

#[derive(Default)]
struct Tree {
    rows: Vec<Row>,
    st: ListState,
}

impl Tree {
    fn reset(&mut self, items: Vec<Item>) {
        self.rows = items
            .into_iter()
            .map(|(path, dir, size)| Row {
                path,
                depth: 0,
                dir,
                open: false,
                size,
            })
            .collect();
        self.st
            .select(if self.rows.is_empty() { None } else { Some(0) });
    }

    fn cur(&self) -> Option<&Row> {
        self.rows.get(self.st.selected()?)
    }

    fn mv(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        let i = self.st.selected().unwrap_or(0) as isize + d;
        self.st
            .select(Some(i.clamp(0, self.rows.len() as isize - 1) as usize));
    }

    fn open(&mut self, kids: Vec<Item>) {
        if let Some(i) = self.st.selected() {
            self.open_at(i, kids);
        }
    }

    fn open_at(&mut self, i: usize, kids: Vec<Item>) {
        let depth = self.rows[i].depth + 1;
        self.rows[i].open = true;
        let new = kids.into_iter().map(|(path, dir, size)| Row {
            path,
            depth,
            dir,
            open: false,
            size,
        });
        self.rows.splice(i + 1..i + 1, new);
    }

    fn close(&mut self) {
        let Some(i) = self.st.selected() else { return };
        let depth = self.rows[i].depth;
        if self.rows[i].open {
            let end = self.rows[i + 1..]
                .iter()
                .position(|r| r.depth <= depth)
                .map(|n| i + 1 + n)
                .unwrap_or(self.rows.len());
            self.rows.drain(i + 1..end);
            self.rows[i].open = false;
        } else if let Some(p) = self.rows[..i].iter().rposition(|r| r.depth < depth) {
            self.st.select(Some(p));
        }
    }
}

fn list_dir(dir: &str, hidden: bool) -> Vec<Item> {
    let mut v: Vec<Item> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| hidden || !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            Some((
                e.path().to_string_lossy().into_owned(),
                md.is_dir(),
                md.len(),
            ))
        })
        .collect();
    v.sort_by_key(|a| (!a.1, a.0.to_lowercase()));
    v
}

fn name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

struct Snap {
    set: String,
    at: Vec<String>,
    id: String,
    time: i64,
    files: u64,
    bytes: u64,
    tag: Option<String>,
}

#[derive(Default)]
struct RestoreView {
    snaps: Vec<Snap>,
    cur: usize,
    kids: HashMap<String, Vec<Item>>,
    tree: Tree,
    picked: BTreeSet<String>,
    loaded: bool,
    timeline: bool,
    tl: ListState,
    sigs: HashMap<String, String>,
    latest: HashMap<String, String>,
    changed_dirs: HashSet<String>,
    roots: Vec<Item>,
    all: Vec<Item>,
    filter: String,
}

type Listing = Vec<(String, bool, u64, String)>;

impl RestoreView {
    /// Shows only entries whose name contains the filter text, or the full tree when it is empty.
    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        if needle.is_empty() {
            self.tree.reset(self.roots.clone());
            return;
        }
        let hits: Vec<Item> = self
            .all
            .iter()
            .filter(|i| name(&i.0).to_lowercase().contains(&needle))
            .take(5000)
            .cloned()
            .collect();
        self.tree.reset(hits);
    }

    fn at_latest(&self) -> bool {
        self.cur + 1 >= self.snaps.len()
    }

    /// How an entry in the viewed restore point differs from the latest one.
    fn marker(&self, r: &Row) -> Option<(&'static str, Color)> {
        if self.at_latest() {
            return None;
        }
        match self.latest.get(&r.path) {
            None => Some(("deleted since", Color::Red)),
            Some(v) if !r.dir && self.sigs.get(&r.path) != Some(v) => {
                Some(("different now", Color::Yellow))
            }
            Some(_) if r.dir && self.changed_dirs.contains(&r.path) => {
                Some(("changes inside", Color::Yellow))
            }
            _ => None,
        }
    }
}

enum Prompt {
    Setting(&'static str),
    Filter,
    NewSet,
    Drive,
    Remove(String),
    Every(String),
    Join,
    RestoreTo,
    Date,
}

struct App {
    exec: Arc<Exec>,
    done_tx: mpsc::UnboundedSender<Done>,
    progress_tx: mpsc::UnboundedSender<String>,
    info_tx: mpsc::UnboundedSender<Vec<String>>,
    busy: Option<String>,
    progress: String,
    spin: usize,
    cfg: Config,
    tab: usize,
    root: String,
    hidden: bool,
    folders: Tree,
    friends: ListState,
    settings: ListState,
    restore: RestoreView,
    input: Option<(Prompt, String)>,
    popup: Option<Vec<String>>,
    versions: Option<(Vec<(usize, String)>, ListState)>,
    /// Restore from this destination only; None means any of them.
    source: Option<String>,
    /// The backup set the Folders, Friends and Settings pages are editing.
    set: String,
    /// The backup set shown on the Restore page (None = whichever has the newest restore point).
    rset: Option<String>,
    info: Vec<String>,
    info_loaded: bool,
    info_at: std::time::Instant,
    status: String,
}

const TABS: [&str; 5] = [
    "1 Folders",
    "2 Friends",
    "3 Settings",
    "4 Restore",
    "5 Status",
];
const SETTINGS: [(&str, &str); 16] = [
    (
        "set-every",
        "This set: minutes between automatic backups (0 = each destination's own pace)",
    ),
    (
        "set-hours",
        "This set: hours it may back up, such as 22-7 ('always' = the general hours below)",
    ),
    (
        "set-versions",
        "This set: max versions kept of each file (0 = unlimited)",
    ),
    (
        "set-days",
        "This set: delete restore points older than this many days (0 = never)",
    ),
    (
        "hours",
        "Hours automatic backups may run, such as 22-7 (enter to change; 'always' for any time)",
    ),
    (
        "pause",
        "Automatic backups (enter: pause for a number of hours; 0 resumes)",
    ),
    (
        "every",
        "Minutes between automatic backups to local drives (friends default to 60; e on the Friends page)",
    ),
    (
        "limit",
        "Upload speed limit in KB/s across friends (0 = unlimited)",
    ),
    (
        "warn",
        "Warn when a backup is this many days old (0 = never)",
    ),
    ("versions", "Max versions kept of each file (0 = unlimited)"),
    (
        "days",
        "Delete restore points older than this many days (0 = never)",
    ),
    ("last", "Always keep this many most recent restore points"),
    ("hourly", "Keep one per hour for this many hours"),
    ("daily", "Keep one per day for this many days"),
    ("weekly", "Keep one per week for this many weeks"),
    ("monthly", "Keep one per month for this many months"),
];

impl App {
    async fn reload(&mut self) {
        match self.exec.call(Cmd::Config).await {
            Ok(l) => match l.first().and_then(|s| serde_json::from_str(s).ok()) {
                Some(c) => self.cfg = c,
                None => self.status = "could not read the configuration".into(),
            },
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    async fn apply(&mut self, cmd: Cmd) {
        match self.exec.call(cmd).await {
            Ok(_) => self.status = "saved".into(),
            Err(e) => self.status = format!("{e:#}"),
        }
        self.reload().await;
    }

    fn set_root(&mut self, root: String) {
        self.folders.reset(list_dir(&root, self.hidden));
        self.root = root;
    }

    fn excluded(&self, path: &str) -> bool {
        self.cfg.exclude.contains(&globset::escape(path))
    }

    fn cur_set(&self) -> BackupSet {
        self.cfg
            .set(&self.set)
            .cloned()
            .unwrap_or_else(|| BackupSet {
                name: self.set.clone(),
                ..Default::default()
            })
    }

    /// The other set that already backs up this exact folder, if any.
    fn other_set(&self, path: &str) -> Option<&str> {
        let p = Path::new(path);
        self.cfg
            .sets
            .iter()
            .find(|b| b.name != self.set && b.paths.iter().any(|x| x == p))
            .map(|b| b.name.as_str())
    }

    fn folder_mark(&self, path: &str) -> (&'static str, Color) {
        let p = Path::new(path);
        let paths = self.cur_set().paths;
        if paths.iter().any(|x| x == p) {
            ("[x]", Color::Green)
        } else if self
            .cfg
            .exclude
            .iter()
            .any(|e| p.starts_with(e.replace('\\', "")))
            && paths.iter().any(|x| p.starts_with(x))
        {
            ("[-]", Color::Red)
        } else if paths.iter().any(|x| p.starts_with(x)) {
            ("[+]", Color::Green)
        } else {
            ("[ ]", Color::Reset)
        }
    }

    async fn toggle_folder(&mut self) {
        let Some(path) = self.folders.cur().map(|r| r.path.clone()) else {
            return;
        };
        let p = PathBuf::from(&path);
        let paths = self.cur_set().paths;
        let cmd = if paths.contains(&p) {
            Cmd::RemovePath { path: p }
        } else if paths.iter().any(|x| p.starts_with(x)) {
            Cmd::Exclude {
                pattern: globset::escape(&path),
                remove: self.excluded(&path),
            }
        } else {
            Cmd::AddPaths {
                paths: vec![p],
                set: Some(self.set.clone()),
            }
        };
        self.apply(cmd).await;
    }

    /// Switches which destinations the current set goes to.
    async fn toggle_destination(&mut self, name: &str) {
        let b = self.cur_set();
        let Some(f) = self.cfg.friends.iter().find(|f| f.name == name).cloned() else {
            return;
        };
        let mut list: Vec<String> = self
            .cfg
            .friends
            .iter()
            .filter(|x| b.sends_to(x))
            .map(|x| x.name.clone())
            .collect();
        if b.sends_to(&f) {
            list.retain(|n| n != name);
            if list.is_empty() {
                self.status = "a set needs at least one destination".into();
                return;
            }
        } else {
            if !f.backup_to {
                self.apply(Cmd::FriendBackupTo {
                    name: name.to_string(),
                    on: true,
                })
                .await;
            }
            list.push(name.to_string());
        }
        self.apply(Cmd::SetEdit {
            name: self.set.clone(),
            to: Some(list),
            every: None,
            hours: None,
            days: None,
            versions: None,
            last: None,
            delete: false,
        })
        .await;
    }

    fn setting(&self, key: &str) -> u32 {
        let r = &self.cfg.retention;
        let b = self.cur_set();
        let own = b.retention.clone().unwrap_or_else(|| r.clone());
        match key {
            "set-every" => b.every_minutes,
            "set-versions" => own.max_versions,
            "set-days" => own.keep_days,
            "every" => (self.cfg.min_interval_secs / 60).max(1) as u32,
            "limit" => self.cfg.upload_kbps as u32,
            "warn" => self.cfg.warn_after_days,
            "versions" => r.max_versions,
            "days" => r.keep_days,
            "last" => r.keep_last,
            "hourly" => r.keep_hourly,
            "daily" => r.keep_daily,
            "weekly" => r.keep_weekly,
            _ => r.keep_monthly,
        }
    }

    fn setting_text(&self, key: &str) -> String {
        match key {
            "set-hours" if self.cur_set().hours.is_empty() => "general".into(),
            "set-hours" => self.cur_set().hours,
            "hours" if self.cfg.backup_hours.is_empty() => "any time".into(),
            "hours" => self.cfg.backup_hours.clone(),
            "pause" if self.cfg.paused_until > chrono::Utc::now().timestamp() => {
                if self.cfg.paused_until - chrono::Utc::now().timestamp() > 365 * 86400 {
                    "paused".into()
                } else {
                    format!("paused to {}", &when(self.cfg.paused_until)[5..16])
                }
            }
            "pause" => "running".into(),
            _ => self.setting(key).to_string(),
        }
    }

    /// Saves one setting from typed text; returns false when the text makes no sense for it.
    async fn set_setting(&mut self, key: &str, text: &str) -> bool {
        let text = text.trim();
        let num = text.parse::<u32>().ok();
        let edit = |every, hours, days, versions| Cmd::SetEdit {
            name: self.set.clone(),
            to: None,
            every,
            hours,
            days,
            versions,
            last: None,
            delete: false,
        };
        let cmd = match (key, num) {
            ("set-every", Some(n)) => edit(Some(n), None, None, None),
            ("set-hours", _) => edit(
                None,
                Some(
                    if text.eq_ignore_ascii_case("always") || text.eq_ignore_ascii_case("general") {
                        String::new()
                    } else {
                        text.to_string()
                    },
                ),
                None,
                None,
            ),
            ("set-versions", Some(n)) => edit(None, None, None, Some(n)),
            ("set-days", Some(n)) => edit(None, None, Some(n), None),
            ("hours", _) => {
                let h = if text.eq_ignore_ascii_case("always")
                    || text.eq_ignore_ascii_case("any time")
                {
                    String::new()
                } else {
                    text.to_string()
                };
                Cmd::Tune {
                    hours: Some(h),
                    upload_kbps: None,
                    pause_hours: None,
                    warn_days: None,
                }
            }
            ("pause", _) if text.is_empty() => Cmd::Tune {
                pause_hours: Some(24 * 3650),
                upload_kbps: None,
                hours: None,
                warn_days: None,
            },
            ("pause", Some(n)) => Cmd::Tune {
                pause_hours: Some(n as i64),
                upload_kbps: None,
                hours: None,
                warn_days: None,
            },
            ("every", Some(n)) => Cmd::Every {
                name: None,
                minutes: n,
            },
            ("limit", Some(n)) => Cmd::Tune {
                upload_kbps: Some(n as u64),
                hours: None,
                pause_hours: None,
                warn_days: None,
            },
            ("warn", Some(n)) => Cmd::Tune {
                warn_days: Some(n),
                upload_kbps: None,
                hours: None,
                pause_hours: None,
            },
            ("versions", Some(n)) => Cmd::Keep {
                set: vec![],
                versions: Some(n),
                pattern: None,
            },
            (k, Some(n)) if !["hours", "pause", "set-hours"].contains(&k) => Cmd::Keep {
                set: vec![(k.to_string(), n)],
                versions: None,
                pattern: None,
            },
            _ => return false,
        };
        self.apply(cmd).await;
        self.info_loaded = false;
        true
    }

    async fn bump(&mut self, d: i64) {
        let key = SETTINGS[self.settings.selected().unwrap_or(0)].0;
        match key {
            "hours" | "set-hours" => {
                self.status = "press enter to type the hours, such as 22-7".into()
            }
            "pause" if d < 0 => {
                self.set_setting("pause", "0").await;
            }
            "pause" => {
                let left = (self.cfg.paused_until - chrono::Utc::now().timestamp()).max(0) / 3600;
                self.set_setting("pause", &(left + 1).to_string()).await;
            }
            _ => {
                let step = if key == "limit" { 100 } else { 1 };
                let v = (self.setting(key) as i64 + d * step).max(0);
                self.set_setting(key, &v.to_string()).await;
            }
        }
    }

    /// Starts a background job unless one is already running.
    fn start<F>(
        &mut self,
        label: &str,
        make: impl FnOnce(Arc<Exec>, mpsc::UnboundedSender<String>) -> F,
    ) -> bool
    where
        F: std::future::Future<Output = Done> + Send + 'static,
    {
        if let Some(b) = &self.busy {
            self.status = format!("still {b}, one moment");
            return false;
        }
        self.busy = Some(label.into());
        self.progress.clear();
        self.status.clear();
        let job = make(self.exec.clone(), self.progress_tx.clone());
        let tx = self.done_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(job.await);
        });
        true
    }

    fn load_snaps(&mut self) {
        let from = self.source.clone();
        let want = self.rset.clone();
        let started = self.start("looking up restore points", |ex, _| async move {
            let lines = match ex.call(Cmd::SnapList { from: from.clone() }).await {
                Ok(l) => l,
                Err(e) => return Done::Failed(format!("{e:#}")),
            };
            let snaps: Vec<Snap> = lines
                .iter()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|v| v["id"].is_string())
                .map(|v| Snap {
                    set: v["set"].as_str().unwrap_or(DEFAULT_SET).to_string(),
                    at: v["at"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    id: v["id"].as_str().unwrap_or_default().into(),
                    time: v["time"].as_i64().unwrap_or(0),
                    files: v["files"].as_u64().unwrap_or(0),
                    bytes: v["bytes"].as_u64().unwrap_or(0),
                    tag: v["tag"].as_str().map(String::from),
                })
                .collect();
            // One set at a time: the one asked for, or the one with the newest restore point.
            let set = want.or_else(|| snaps.last().map(|s| s.set.clone()));
            let snaps: Vec<Snap> = snaps
                .into_iter()
                .filter(|s| Some(&s.set) == set.as_ref())
                .collect();
            let tree = match snaps.last() {
                Some(s) => ex
                    .call(Cmd::Tree {
                        snap: s.id.clone(),
                        from,
                    })
                    .await
                    .ok()
                    .map(parse_listing),
                None => None,
            };
            let notes = lines
                .iter()
                .filter(|l| !l.trim_start().starts_with('{'))
                .map(|l| l.trim().to_string())
                .collect();
            Done::Snaps(snaps, tree, notes, set)
        });
        if started {
            self.restore.loaded = true;
        }
    }

    /// Switches the Restore page to another restore point.
    fn goto(&mut self, idx: usize) {
        let Some(id) = self.restore.snaps.get(idx).map(|s| s.id.clone()) else {
            return;
        };
        let from = self.source.clone();
        self.start("loading that point in time", |ex, _| async move {
            match ex.call(Cmd::Tree { snap: id, from }).await {
                Ok(l) => Done::Tree(idx, parse_listing(l)),
                Err(e) => Done::Failed(format!("{e:#}")),
            }
        });
    }

    /// Status is quick and read-only, so it loads even while a backup or restore is running.
    fn load_info(&mut self) {
        self.info_loaded = true;
        self.info_at = std::time::Instant::now();
        if self.info.is_empty() {
            self.info = vec!["checking...".into()];
        }
        let (ex, tx) = (self.exec.clone(), self.info_tx.clone());
        tokio::spawn(async move {
            let mut lines = lines_or_error(ex.call(Cmd::Status).await);
            let snaps = ex
                .call(Cmd::SnapList { from: None })
                .await
                .unwrap_or_default();
            let last = snaps.iter().rev().find_map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .filter(|v| v["id"].is_string())
            });
            lines.push(String::new());
            lines.push(match last {
                Some(v) => format!(
                    "restore points: {}, latest {} ({} files, {})",
                    snaps.len(),
                    when(v["time"].as_i64().unwrap_or(0)),
                    v["files"].as_u64().unwrap_or(0),
                    human(v["bytes"].as_u64().unwrap_or(0))
                ),
                None => "restore points: none yet (the first one appears when a backup finishes; press b to start one)".into(),
            });
            let _ = tx.send(lines);
        });
    }

    /// Loads whatever the current page still needs, once nothing else is running.
    fn kick(&mut self) {
        if self.tab == 4 && !self.info_loaded {
            self.load_info();
        }
        if self.busy.is_none() && self.tab == 3 && !self.restore.loaded {
            self.load_snaps();
        }
    }

    async fn finish(&mut self, done: Done) {
        self.busy = None;
        self.progress.clear();
        match done {
            Done::Popup(lines) => self.popup = Some(lines),
            Done::BackedUp(lines) => {
                self.popup = Some(lines);
                self.restore.loaded = false;
                self.info_loaded = false;
            }
            Done::Paired(lines) => {
                self.popup = Some(lines);
                self.info_loaded = false;
                self.reload().await;
            }
            Done::Snaps(snaps, tree, notes, set) => {
                self.rset = set;
                if !notes.is_empty() {
                    self.status = format!(
                        "{} (showing what the others hold; u retries)",
                        notes.join("; ")
                    );
                }
                self.restore = RestoreView {
                    loaded: true,
                    ..Default::default()
                };
                self.restore.cur = snaps.len().saturating_sub(1);
                self.restore.snaps = snaps;
                if let Some(items) = tree {
                    self.restore.latest =
                        items.iter().map(|i| (i.0.clone(), i.3.clone())).collect();
                    self.show_tree(items);
                }
            }
            Done::Tree(idx, items) => {
                self.restore.cur = idx;
                self.restore.timeline = false;
                self.show_tree(items);
            }
            Done::Versions(lines) => {
                let mut list = vec![];
                for v in lines
                    .iter()
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                {
                    let Some(idx) = self
                        .restore
                        .snaps
                        .iter()
                        .position(|s| Some(s.id.as_str()) == v["id"].as_str())
                    else {
                        continue;
                    };
                    let what = match v["size"].as_u64() {
                        Some(n) => format!("{:>10}", human(n)),
                        None => "  (deleted)".into(),
                    };
                    let at: Vec<&str> = v["at"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                        .unwrap_or_default();
                    list.push((
                        idx,
                        format!(
                            "{}  {what}   on {}",
                            when(v["time"].as_i64().unwrap_or(0)),
                            at.join(", ")
                        ),
                    ));
                }
                list.reverse();
                let mut st = ListState::default();
                st.select(
                    list.iter()
                        .position(|(i, _)| *i <= self.restore.cur)
                        .or(Some(0)),
                );
                self.versions = Some((list, st));
            }
            Done::Failed(e) => self.status = e,
        }
    }

    /// Shows a restore point's files, keeping the same folders open and the same entry selected.
    fn show_tree(&mut self, items: Listing) {
        let rv = &mut self.restore;
        let open: HashSet<String> = rv
            .tree
            .rows
            .iter()
            .filter(|r| r.open)
            .map(|r| r.path.clone())
            .collect();
        let at = rv.tree.cur().map(|r| r.path.clone());
        rv.kids.clear();
        rv.picked.clear();
        rv.sigs.clear();
        rv.changed_dirs.clear();
        let all: HashSet<&str> = items.iter().map(|i| i.0.as_str()).collect();
        let mut roots = vec![];
        let mut kids: HashMap<String, Vec<Item>> = HashMap::new();
        for (p, dir, size, v) in &items {
            if !dir && rv.latest.get(p) != Some(v) {
                for anc in Path::new(p).ancestors().skip(1) {
                    if !rv.changed_dirs.insert(anc.to_string_lossy().into_owned()) {
                        break;
                    }
                }
            }
            rv.sigs.insert(p.clone(), v.clone());
            let it = (p.clone(), *dir, *size);
            match Path::new(p)
                .parent()
                .map(|x| x.to_string_lossy().into_owned())
                .filter(|x| all.contains(x.as_str()))
            {
                Some(parent) => kids.entry(parent).or_default().push(it),
                None => roots.push(it),
            }
        }
        for v in kids.values_mut() {
            v.sort_by_key(|a| (!a.1, a.0.to_lowercase()));
        }
        rv.kids = kids;
        rv.all = items
            .iter()
            .map(|(p, d, s, _)| (p.clone(), *d, *s))
            .collect();
        rv.all.sort_by(|a, b| a.0.cmp(&b.0));
        rv.roots = roots;
        rv.apply_filter();
        let mut i = 0;
        while i < rv.tree.rows.len() {
            if rv.tree.rows[i].dir && open.contains(&rv.tree.rows[i].path) {
                let k = rv
                    .kids
                    .get(&rv.tree.rows[i].path)
                    .cloned()
                    .unwrap_or_default();
                rv.tree.open_at(i, k);
            }
            i += 1;
        }
        if let Some(pos) = at.and_then(|p| rv.tree.rows.iter().position(|r| r.path == p)) {
            rv.tree.st.select(Some(pos));
        }
        rv.tl.select(Some(
            rv.snaps.len().saturating_sub(1) - rv.cur.min(rv.snaps.len().saturating_sub(1)),
        ));
    }

    fn show_versions(&mut self) {
        let Some(path) = self
            .restore
            .tree
            .cur()
            .filter(|r| !r.dir)
            .map(|r| r.path.clone())
        else {
            self.status = "move to a file first".into();
            return;
        };
        let from = self.source.clone();
        self.start("finding versions", |ex, _| async move {
            match ex.call(Cmd::Versions { path, from }).await {
                Ok(l) => Done::Versions(l),
                Err(e) => Done::Failed(format!("{e:#}")),
            }
        });
    }

    async fn key(&mut self, code: KeyCode) -> bool {
        if self.popup.take().is_some() {
            return true;
        }
        if let Some((list, mut st)) = self.versions.take() {
            let i = st.selected().unwrap_or(0);
            match code {
                KeyCode::Up | KeyCode::Char('k') => st.select(Some(i.saturating_sub(1))),
                KeyCode::Down | KeyCode::Char('j') => {
                    st.select(Some((i + 1).min(list.len().saturating_sub(1))))
                }
                KeyCode::Enter => {
                    if let Some((idx, _)) = list.get(i) {
                        self.goto(*idx);
                    }
                    return true;
                }
                _ => return true,
            }
            self.versions = Some((list, st));
            return true;
        }
        if let Some((prompt, mut text)) = self.input.take() {
            let filtering = matches!(prompt, Prompt::Filter);
            // Moving in the list ends typing, so the next keys (space, v, r) act on the results.
            if filtering
                && matches!(
                    code,
                    KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::Left
                        | KeyCode::Right
                )
            {
                self.key_restore(code).await;
                return true;
            }
            match code {
                KeyCode::Esc if filtering => text.clear(),
                KeyCode::Esc => {}
                KeyCode::Enter if filtering => {}
                KeyCode::Enter => self.submit(prompt, text.trim().to_string()).await,
                KeyCode::Backspace => {
                    text.pop();
                    self.input = Some((prompt, text.clone()));
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    self.input = Some((prompt, text.clone()));
                }
                _ => self.input = Some((prompt, text.clone())),
            }
            if filtering && self.restore.filter != text {
                self.restore.filter = text;
                self.restore.apply_filter();
            }
            return true;
        }
        match code {
            KeyCode::Esc if self.tab == 3 && !self.restore.filter.is_empty() => {
                self.key_restore(code).await
            }
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Tab => self.tab = (self.tab + 1) % TABS.len(),
            KeyCode::BackTab => self.tab = (self.tab + TABS.len() - 1) % TABS.len(),
            KeyCode::Char(c @ '1'..='5') => self.tab = c as usize - '1' as usize,
            KeyCode::Char('w') if self.tab != 3 => {
                let names: Vec<String> = self.cfg.sets.iter().map(|b| b.name.clone()).collect();
                let i = names
                    .iter()
                    .position(|n| *n == self.set)
                    .map(|i| (i + 1) % names.len())
                    .unwrap_or(0);
                self.set = names.get(i).cloned().unwrap_or_else(|| DEFAULT_SET.into());
                self.status = format!("now editing set \"{}\"", self.set);
            }
            KeyCode::Char('n') if self.tab != 3 => {
                self.input = Some((Prompt::NewSet, String::new()))
            }
            KeyCode::Char('b') => {
                self.start("backing up", |ex, pr| async move {
                    Done::BackedUp(lines_or_error(
                        ex.live(
                            Cmd::Backup {
                                tag: None,
                                set: None,
                            },
                            &pr,
                        )
                        .await,
                    ))
                });
            }
            _ => match self.tab {
                0 => self.key_folders(code).await,
                1 => self.key_friends(code).await,
                2 => self.key_settings(code).await,
                3 => self.key_restore(code).await,
                _ if code == KeyCode::Char('u') => self.info_loaded = false,
                _ => {}
            },
        }
        true
    }

    async fn key_folders(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.folders.mv(-1),
            KeyCode::Down | KeyCode::Char('j') => self.folders.mv(1),
            KeyCode::PageUp => self.folders.mv(-15),
            KeyCode::PageDown => self.folders.mv(15),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(r) = self.folders.cur().filter(|r| r.dir && !r.open) {
                    let kids = list_dir(&r.path, self.hidden);
                    self.folders.open(kids);
                }
            }
            KeyCode::Left | KeyCode::Char('h') => self.folders.close(),
            KeyCode::Backspace => {
                if let Some(parent) = Path::new(&self.root).parent() {
                    self.set_root(parent.to_string_lossy().into_owned());
                }
            }
            KeyCode::Char('.') => {
                self.hidden = !self.hidden;
                self.set_root(self.root.clone());
            }
            KeyCode::Char(' ') => self.toggle_folder().await,
            _ => {}
        }
    }

    async fn key_friends(&mut self, code: KeyCode) {
        let n = self.cfg.friends.len();
        let i = self
            .friends
            .selected()
            .unwrap_or(0)
            .min(n.saturating_sub(1));
        let cur = self.cfg.friends.get(i).cloned();
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.friends.select(Some(i.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') => {
                self.friends.select(Some((i + 1).min(n.saturating_sub(1))))
            }
            KeyCode::Char(' ') => {
                if let Some(f) = cur {
                    self.toggle_destination(&f.name).await;
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Right => {
                if let Some(f) = cur {
                    self.apply(Cmd::FriendQuota {
                        name: f.name,
                        gb: f.quota_gb + 10,
                    })
                    .await;
                }
            }
            KeyCode::Char('-') | KeyCode::Left => {
                if let Some(f) = cur {
                    self.apply(Cmd::FriendQuota {
                        name: f.name,
                        gb: f.quota_gb.saturating_sub(10),
                    })
                    .await;
                }
            }
            KeyCode::Char('i') => {
                self.start("creating an invite", |ex, _| async move {
                    Done::Popup(lines_or_error(ex.call(Cmd::Invite).await))
                });
            }
            KeyCode::Char('a') => self.input = Some((Prompt::Join, String::new())),
            KeyCode::Char('e') => {
                if let Some(f) = cur {
                    self.input = Some((Prompt::Every(f.name), String::new()));
                }
            }
            KeyCode::Char('d') => self.input = Some((Prompt::Drive, "/Volumes/".into())),
            KeyCode::Char('x') => {
                if let Some(f) = cur {
                    self.input = Some((Prompt::Remove(f.name), String::new()));
                }
            }
            _ => {}
        }
    }

    async fn key_settings(&mut self, code: KeyCode) {
        let i = self.settings.selected().unwrap_or(0);
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.settings.select(Some(i.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings.select(Some((i + 1).min(SETTINGS.len() - 1)))
            }
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => self.bump(1).await,
            KeyCode::Left | KeyCode::Char('-') => self.bump(-1).await,
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.input = Some((Prompt::Setting(SETTINGS[i].0), String::new()))
            }
            _ => {}
        }
    }

    async fn key_restore(&mut self, code: KeyCode) {
        let rv = &mut self.restore;
        let n = rv.snaps.len();
        if rv.timeline {
            let i = rv.tl.selected().unwrap_or(0);
            match code {
                KeyCode::Up | KeyCode::Char('k') => rv.tl.select(Some(i.saturating_sub(1))),
                KeyCode::Down | KeyCode::Char('j') => {
                    rv.tl.select(Some((i + 1).min(n.saturating_sub(1))))
                }
                KeyCode::PageUp => rv.tl.select(Some(i.saturating_sub(15))),
                KeyCode::PageDown => rv.tl.select(Some((i + 15).min(n.saturating_sub(1)))),
                KeyCode::Enter | KeyCode::Char(' ') if n > 0 => self.goto(n - 1 - i),
                KeyCode::Char('t') | KeyCode::Left => rv.timeline = false,
                KeyCode::Char('g') => self.input = Some((Prompt::Date, String::new())),
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Char('t') => rv.timeline = true,
            KeyCode::Char('g') => self.input = Some((Prompt::Date, String::new())),
            KeyCode::Char('v') => self.show_versions(),
            KeyCode::Char('w') => {
                let names: Vec<String> = self.cfg.sets.iter().map(|b| b.name.clone()).collect();
                let i = self
                    .rset
                    .as_ref()
                    .and_then(|c| names.iter().position(|n| n == c))
                    .map(|i| (i + 1) % names.len().max(1))
                    .unwrap_or(0);
                self.rset = names.get(i).cloned();
                self.load_snaps();
            }
            KeyCode::Char('s') => {
                // Cycle: all destinations, then each one by itself.
                let names: Vec<String> = self.cfg.friends.iter().map(|f| f.name.clone()).collect();
                self.source = match &self.source {
                    None => names.first().cloned(),
                    Some(cur) => names
                        .iter()
                        .position(|n| n == cur)
                        .and_then(|i| names.get(i + 1))
                        .cloned(),
                };
                self.load_snaps();
            }
            KeyCode::Char('/') => self.input = Some((Prompt::Filter, rv.filter.clone())),
            KeyCode::Esc if !rv.filter.is_empty() => {
                rv.filter.clear();
                rv.apply_filter();
            }
            KeyCode::Up | KeyCode::Char('k') => rv.tree.mv(-1),
            KeyCode::Down | KeyCode::Char('j') => rv.tree.mv(1),
            KeyCode::PageUp => rv.tree.mv(-15),
            KeyCode::PageDown => rv.tree.mv(15),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => {
                if let Some(r) = rv.tree.cur().filter(|r| r.dir && !r.open) {
                    let kids = rv.kids.get(&r.path).cloned().unwrap_or_default();
                    rv.tree.open(kids);
                }
            }
            KeyCode::Left | KeyCode::Char('h') => rv.tree.close(),
            KeyCode::Char(' ') => {
                if let Some(p) = rv.tree.cur().map(|r| r.path.clone()) {
                    if !rv.picked.remove(&p) {
                        rv.picked.insert(p);
                    }
                }
            }
            KeyCode::Char('[') if rv.cur > 0 => {
                let i = rv.cur - 1;
                self.goto(i);
            }
            KeyCode::Char(']') if rv.cur + 1 < rv.snaps.len() => {
                let i = rv.cur + 1;
                self.goto(i);
            }
            KeyCode::Char('u') => self.load_snaps(),
            KeyCode::Char('r') => {
                if rv.picked.is_empty() {
                    self.status = "pick files or folders with space first".into();
                } else {
                    let home = std::env::var("HOME").unwrap_or_default();
                    self.input = Some((Prompt::RestoreTo, format!("{home}/sdb-restore")));
                }
            }
            _ => {}
        }
    }

    async fn submit(&mut self, prompt: Prompt, text: String) {
        if text.is_empty() && !matches!(prompt, Prompt::Setting("pause")) {
            return;
        }
        match prompt {
            Prompt::Filter => {}
            Prompt::NewSet => {
                let name: String = text
                    .chars()
                    .filter(|c| c.is_alphanumeric() || "-_.".contains(*c))
                    .take(32)
                    .collect();
                if name.is_empty() {
                    self.status = "use letters and digits for a set name".into();
                } else {
                    self.apply(Cmd::SetEdit {
                        name: name.clone(),
                        to: None,
                        every: None,
                        hours: None,
                        days: None,
                        versions: None,
                        last: None,
                        delete: false,
                    })
                    .await;
                    self.set = name;
                    self.status = format!(
                        "set \"{}\" created: pick its folders on page 1 and its destinations on page 2",
                        self.set
                    );
                }
            }
            Prompt::Drive => {
                let home = std::env::var("HOME").unwrap_or_default();
                let typed = text.replacen('~', &home, 1);
                match std::fs::canonicalize(&typed) {
                    Ok(path) if path.is_dir() => {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "drive".into());
                        match self.exec.call(Cmd::DriveAdd { name, path }).await {
                            Ok(l) => self.popup = Some(l),
                            Err(e) => self.status = format!("{e:#}"),
                        }
                        self.reload().await;
                    }
                    _ => self.status = format!("{typed} is not a folder that exists"),
                }
            }
            Prompt::Every(name) => match text.parse::<u32>() {
                Ok(minutes) => {
                    self.apply(Cmd::Every {
                        name: Some(name),
                        minutes,
                    })
                    .await
                }
                Err(_) => self.status = "type a number of minutes".into(),
            },
            Prompt::Remove(name) => {
                if text.eq_ignore_ascii_case("yes") {
                    self.apply(Cmd::FriendRemove { name, purge: false }).await;
                    self.friends.select(Some(0));
                }
            }
            Prompt::Setting(key) => {
                if !self.set_setting(key, &text).await {
                    self.status = "that value does not fit this setting".into();
                }
            }
            Prompt::Date => {
                match parse_when(&text)
                    .and_then(|t| self.restore.snaps.iter().rposition(|s| s.time <= t))
                {
                    Some(i) => self.goto(i),
                    None => self.status =
                        "no restore point at or before that (try 2026-11-20 or 2026-11-20 14:30)"
                            .into(),
                }
            }
            Prompt::Join => {
                let ticket = text.trim_start_matches("sdb join ").to_string();
                self.start("pairing", |ex, _| async move {
                    Done::Paired(lines_or_error(ex.call(Cmd::Join { ticket }).await))
                });
            }
            Prompt::RestoreTo => {
                let Some(snap) = self
                    .restore
                    .snaps
                    .get(self.restore.cur)
                    .map(|s| s.id.clone())
                else {
                    return;
                };
                let paths = self.restore.picked.iter().cloned().collect();
                let cmd = Cmd::Restore {
                    snap,
                    paths,
                    to: PathBuf::from(text),
                    from: self.source.clone(),
                };
                self.start("restoring", |ex, pr| async move {
                    Done::Popup(lines_or_error(ex.live(cmd, &pr).await))
                });
            }
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [top, body, help] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .areas(f.area());
        let tabs = Tabs::new(TABS.to_vec())
            .select(self.tab)
            .highlight_style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            .block(Block::bordered().title(format!(
                " Super Duper Backup: {} | backup set: {} of {} (w: switch set, n: new set) ",
                self.cfg.name,
                self.set,
                self.cfg.sets.len()
            )));
        f.render_widget(tabs, top);
        let hl = Style::new().add_modifier(Modifier::REVERSED);
        let keys = match self.tab {
            0 => {
                let items: Vec<ListItem> = self
                    .folders
                    .rows
                    .iter()
                    .map(|r| {
                        let (mark, color) = self.folder_mark(&r.path);
                        let arrow = if !r.dir {
                            " "
                        } else if r.open {
                            "v"
                        } else {
                            ">"
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(mark, Style::new().fg(color)),
                            Span::raw(format!(
                                " {}{arrow} {}{}",
                                "  ".repeat(r.depth),
                                name(&r.path),
                                if r.dir { "/" } else { "" }
                            )),
                            Span::styled(
                                self.other_set(&r.path)
                                    .map(|o| format!("   (in set {o})"))
                                    .unwrap_or_default(),
                                Style::new().fg(Color::DarkGray),
                            ),
                        ]))
                    })
                    .collect();
                let title = format!(" What set \"{}\" backs up: {} ", self.set, self.root);
                f.render_stateful_widget(
                    List::new(items)
                        .block(Block::bordered().title(title))
                        .highlight_style(hl),
                    body,
                    &mut self.folders.st,
                );
                "space: back up / stop ([x] chosen, [+] inside a chosen folder, [-] left out)   arrows: move, open, close\nshift+up/down: page   backspace: go up a folder   .: show hidden files"
            }
            1 => {
                let cur_set = self.cur_set();
                let items: Vec<ListItem> = self
                    .cfg
                    .friends
                    .iter()
                    .map(|fr| {
                        let mark = if cur_set.sends_to(fr) {
                            Span::styled("[x]", Style::new().fg(Color::Green))
                        } else {
                            Span::raw("[ ]")
                        };
                        let what = match &fr.path {
                            Some(p) => format!("local drive at {}", p.display()),
                            None => {
                                format!("they may store up to {} GB on this computer", fr.quota_gb)
                            }
                        };
                        let every = fr.interval_secs(&self.cfg) / 60;
                        ListItem::new(Line::from(vec![
                            mark,
                            Span::raw(format!(" {:<20} every {every:>3} min   {what}", fr.name)),
                        ]))
                    })
                    .collect();
                if self.friends.selected().is_none() && !self.cfg.friends.is_empty() {
                    self.friends.select(Some(0));
                }
                let title = format!(
                    " Friends and drives ([x] = set \"{}\" is sent there) ",
                    self.set
                );
                if items.is_empty() {
                    f.render_widget(Paragraph::new("No destinations yet. Press i to create an invite to send, a to accept one you were sent, or d to add a local drive.").block(Block::bordered().title(title.as_str())), body);
                } else {
                    f.render_stateful_widget(
                        List::new(items)
                            .block(Block::bordered().title(title.as_str()))
                            .highlight_style(hl),
                        body,
                        &mut self.friends,
                    );
                }
                "space: send / stop sending my backups here   +/-: change a friend's quota by 10 GB   x: remove\ne: how often to send here   i: invite a friend   a: accept an invite   d: add a local drive or folder"
            }
            2 => {
                let items: Vec<ListItem> = SETTINGS
                    .iter()
                    .map(|(k, label)| {
                        ListItem::new(format!("{:>18}   {label}", self.setting_text(k)))
                    })
                    .collect();
                if self.settings.selected().is_none() {
                    self.settings.select(Some(0));
                }
                f.render_stateful_widget(
                    List::new(items)
                        .block(Block::bordered().title(format!(
                            " Settings (rows starting \"This set\" apply to set \"{}\") ",
                            self.set
                        )))
                        .highlight_style(hl),
                    body,
                    &mut self.settings,
                );
                "left/right: change   enter: type a value (pause: hours, blank = until resumed)\nper-pattern version limits: sdb keep --versions 5 --pattern '**/*.iso'"
            }
            3 => {
                let rv = &mut self.restore;
                let title = match rv.snaps.get(rv.cur) {
                    Some(s) => format!(
                        " Your files as of {}{} ({} files, {}){} ",
                        when(s.time),
                        s.tag
                            .as_ref()
                            .map(|t| format!(" \"{t}\""))
                            .unwrap_or_default(),
                        s.files,
                        human(s.bytes),
                        if rv.at_latest() { ", latest" } else { "" }
                    ),
                    None => " Restore ".into(),
                };
                let title = match &self.source {
                    Some(n) => format!("{title}| from {n} only "),
                    None => format!("{title}| from all destinations "),
                };
                let title = if self.cfg.sets.len() > 1 {
                    format!("{title}| set {} ", self.rset.as_deref().unwrap_or("?"))
                } else {
                    title
                };
                let title = if rv.filter.is_empty() {
                    title
                } else {
                    format!(
                        "{title}| {} matching \"{}\" ",
                        rv.tree.rows.len(),
                        rv.filter
                    )
                };
                let focus = |on: bool| {
                    if on {
                        Style::new().fg(Color::Yellow)
                    } else {
                        Style::new()
                    }
                };
                if rv.snaps.is_empty() {
                    f.render_widget(Paragraph::new("No restore points found yet (or no friend is reachable). Press u to look again.").block(Block::bordered().title(title)), body);
                } else {
                    let [left, right] =
                        Layout::horizontal([Constraint::Min(30), Constraint::Length(48)])
                            .areas(body);
                    let items: Vec<ListItem> = rv
                        .tree
                        .rows
                        .iter()
                        .map(|r| {
                            let mark = if rv.picked.contains(&r.path) {
                                Span::styled("[x]", Style::new().fg(Color::Green))
                            } else if rv.picked.iter().any(|p| Path::new(&r.path).starts_with(p)) {
                                Span::styled("[+]", Style::new().fg(Color::Green))
                            } else {
                                Span::raw("[ ]")
                            };
                            let arrow = if !r.dir {
                                " "
                            } else if r.open {
                                "v"
                            } else {
                                ">"
                            };
                            let label = if r.depth > 0 {
                                name(&r.path)
                            } else if rv.filter.is_empty() {
                                r.path.clone()
                            } else {
                                // Search results lead with the name so it is never cut off.
                                let dir = Path::new(&r.path)
                                    .parent()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                format!("{}   in {dir}", name(&r.path))
                            };
                            let size = if r.dir || !rv.filter.is_empty() && r.depth == 0 {
                                String::new()
                            } else {
                                format!("  {}", human(r.size))
                            };
                            let slash = if r.dir && (rv.filter.is_empty() || r.depth > 0) {
                                "/"
                            } else {
                                ""
                            };
                            let mut spans = vec![
                                mark,
                                Span::raw(format!(
                                    " {}{arrow} {label}{slash}{size}",
                                    "  ".repeat(r.depth)
                                )),
                            ];
                            if let Some((text, color)) = rv.marker(r) {
                                spans.push(Span::styled(
                                    format!("  {text}"),
                                    Style::new().fg(color),
                                ));
                            }
                            ListItem::new(Line::from(spans))
                        })
                        .collect();
                    let block = Block::bordered()
                        .title(title)
                        .border_style(focus(!rv.timeline));
                    f.render_stateful_widget(
                        List::new(items).block(block).highlight_style(hl),
                        left,
                        &mut rv.tree.st,
                    );
                    let times: Vec<ListItem> = rv
                        .snaps
                        .iter()
                        .enumerate()
                        .rev()
                        .map(|(i, s)| {
                            let here = if i == rv.cur { ">" } else { " " };
                            let tag = s.tag.as_ref().map(|t| format!(" {t}")).unwrap_or_default();
                            let line = format!("{here} {}{tag}  {}", when(s.time), s.at.join(", "));
                            ListItem::new(if i == rv.cur {
                                Line::styled(line, Style::new().fg(Color::Green))
                            } else {
                                Line::raw(line)
                            })
                        })
                        .collect();
                    let block = Block::bordered()
                        .title(format!(
                            " Timeline ({}) and where each is stored ",
                            rv.snaps.len()
                        ))
                        .border_style(focus(rv.timeline));
                    let mut list = List::new(times).block(block);
                    if rv.timeline {
                        list = list.highlight_style(hl);
                    }
                    f.render_stateful_widget(list, right, &mut rv.tl);
                }
                if rv.timeline {
                    "up/down: move through time   enter: show my files as they were then   g: jump to a date\nt: back to the files"
                } else {
                    "space: pick   r: restore picked   /: search names   t: timeline   g: jump to a date   v: versions of this file\ns: all / one destination   w: which set   [ and ]: older / newer   shift+up/down: page   u: refresh"
                }
            }
            _ => "",
        };
        let keys = if self.tab == 4 {
            f.render_widget(
                Paragraph::new(self.info.join("\n"))
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(" Status ")),
                body,
            );
            "u: refresh"
        } else {
            keys
        };
        let footer = format!("{keys}   b: back up now   tab: next page   q: quit");
        let title = match &self.busy {
            Some(b) => format!(
                " {} {b}... {} ",
                ["|", "/", "-", "\\"][self.spin % 4],
                self.progress.trim().trim_start_matches("...").trim()
            ),
            None => format!(" {} ", self.status),
        };
        let style = if self.busy.is_some() {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new()
        };
        f.render_widget(
            Paragraph::new(footer).block(Block::bordered().title(title).border_style(style)),
            help,
        );

        if let Some((prompt, text)) = &self.input {
            let title = match prompt {
                Prompt::NewSet => " Name for the new backup set ",
                Prompt::Filter => {
                    " Show only names containing (arrows or Enter: use the list, Esc: clear) "
                }
                Prompt::Drive => {
                    " Folder on the drive to back up into (Enter adds it, Esc cancels) "
                }
                Prompt::Every(_) => {
                    " Send automatic backups here at most every how many minutes? (0 = default) "
                }
                Prompt::Remove(_) => {
                    " Type yes to remove this destination (data already stored is left in place) "
                }
                Prompt::Setting("hours") | Prompt::Setting("set-hours") => {
                    " Backup hours, such as 22-7, or 'always' "
                }
                Prompt::Setting("pause") => {
                    " Pause for how many hours? (blank = until resumed, 0 = resume) "
                }
                Prompt::Setting(_) => " New value ",
                Prompt::Join => " Paste the invite, then Enter (Esc cancels) ",
                Prompt::RestoreTo => " Restore into which folder? Enter to start (Esc cancels) ",
                Prompt::Date => " Go back to which date? e.g. 2026-11-20 or 2026-11-20 14:30 ",
            };
            let area = if matches!(prompt, Prompt::Filter) {
                Rect::new(
                    body.x + 2,
                    body.y + body.height.saturating_sub(4),
                    body.width.saturating_sub(40).max(20),
                    3,
                )
            } else {
                centered(f.area(), 80, 5)
            };
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!("{text}_"))
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(title)),
                area,
            );
        }
        if let Some((list, st)) = &mut self.versions {
            let area = centered(f.area(), 84, (list.len() as u16 + 2).clamp(4, 20));
            f.render_widget(Clear, area);
            let items: Vec<ListItem> = list
                .iter()
                .map(|(_, l)| ListItem::new(l.as_str()))
                .collect();
            let block = Block::bordered().title(" Versions: enter = go to that time, esc = close ");
            f.render_stateful_widget(List::new(items).block(block).highlight_style(hl), area, st);
        }
        if let Some(lines) = &self.popup {
            let area = centered(f.area(), 90, (lines.len() as u16 + 4).clamp(6, 20));
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(lines.join("\n"))
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(" Press any key ")),
                area,
            );
        }
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

async fn event_loop(
    app: &mut App,
    term: &mut DefaultTerminal,
    mut done_rx: mpsc::UnboundedReceiver<Done>,
    mut progress_rx: mpsc::UnboundedReceiver<String>,
    mut info_rx: mpsc::UnboundedReceiver<Vec<String>>,
) -> Result<()> {
    // Keys are read on their own thread so the screen keeps updating while work runs.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if key_tx.send(ev).is_err() {
                break;
            }
        }
    });
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(150));
    loop {
        term.draw(|f| app.draw(f))?;
        tokio::select! {
            Some(ev) = key_rx.recv() => {
                let Event::Key(k) = ev else { continue };
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                    return Ok(());
                }
                let code = match k.code {
                    KeyCode::Up if k.modifiers.contains(KeyModifiers::SHIFT) => KeyCode::PageUp,
                    KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => KeyCode::PageDown,
                    c => c,
                };
                if !app.key(code).await {
                    return Ok(());
                }
                app.kick();
            }
            Some(done) = done_rx.recv() => {
                app.finish(done).await;
                app.kick();
            }
            Some(line) = progress_rx.recv() => app.progress = line,
            Some(lines) = info_rx.recv() => app.info = lines,
            _ = tick.tick() => {
                app.spin += 1;
                // The Status page keeps itself current while it is on screen.
                if app.tab == 4 && app.info_loaded && app.info_at.elapsed().as_secs() >= 10 {
                    app.load_info();
                }
            }
        }
    }
}

pub async fn run(exec: Exec) -> Result<()> {
    let (done_tx, done_rx) = mpsc::unbounded_channel();
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let (info_tx, info_rx) = mpsc::unbounded_channel();
    let mut app = App {
        exec: Arc::new(exec),
        done_tx,
        progress_tx,
        info_tx,
        busy: None,
        progress: String::new(),
        spin: 0,
        cfg: Config::new(String::new()),
        tab: 0,
        root: String::new(),
        hidden: false,
        folders: Tree::default(),
        friends: ListState::default(),
        settings: ListState::default(),
        restore: RestoreView::default(),
        input: None,
        popup: None,
        versions: None,
        source: None,
        set: DEFAULT_SET.into(),
        rset: None,
        info: vec![],
        info_loaded: false,
        info_at: std::time::Instant::now(),
        status: String::new(),
    };
    app.reload().await;
    app.set_root(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
    let mut term = ratatui::init();
    let res = event_loop(&mut app, &mut term, done_rx, progress_rx, info_rx).await;
    ratatui::restore();
    res
}
