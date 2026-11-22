// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The interactive interface: a menu on the left, and on the right the backup
//! sets with their status, restore, destinations, settings, history and status.
//! It is only a client: everything it shows and does goes through the daemon.

use crate::{
    config::{BackupSet, Config, DEFAULT_SET, STANDARD, is_builtin_list},
    control,
    node::Cmd,
    snapshot::{human, parse_when, when},
};
use anyhow::Result;
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// Sends commands to the daemon over its local socket.
struct Exec(PathBuf);

impl Exec {
    async fn call(&self, cmd: Cmd) -> Result<Vec<String>> {
        let (tx, _rx) = mpsc::unbounded_channel();
        self.live(cmd, &tx).await
    }

    /// Runs a command, passing each output line to `progress` as it arrives.
    async fn live(&self, cmd: Cmd, progress: &mpsc::UnboundedSender<String>) -> Result<Vec<String>> {
        let mut lines = vec![];
        let mut seen = |l: String| {
            let _ = progress.send(l.clone());
            lines.push(l);
        };
        let stream = control::connect(&self.0)
            .await
            .ok_or_else(|| anyhow::anyhow!("the daemon is not running; quit and start sdb again"))?;
        if let Err(e) = control::request(stream, &cmd, &mut seen).await {
            lines.push(format!("{e:#}"));
            anyhow::bail!("{}", lines.join("\n"));
        }
        Ok(lines.into_iter().filter(|l| !l.trim_start().starts_with("...")).collect())
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
        format!("{e:#}").lines().filter(|l| !l.trim_start().starts_with("...")).map(String::from).collect()
    })
}

pub(crate) type Item = (String, bool, u64);
pub(crate) type Listing = Vec<(String, bool, u64, String)>;

pub(crate) fn parse_listing(lines: Vec<String>) -> Listing {
    lines
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| {
            let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
            (s("p"), v["d"].as_bool().unwrap_or(false), v["s"].as_u64().unwrap_or(0), s("v"))
        })
        .collect()
}

struct Row {
    path: String,
    depth: usize,
    dir: bool,
    open: bool,
    size: u64,
}

/// A collapsible tree shown as a flat list of rows.
#[derive(Default)]
struct Tree {
    rows: Vec<Row>,
    st: ListState,
}

impl Tree {
    fn reset(&mut self, items: Vec<Item>) {
        self.rows = items.into_iter().map(|(path, dir, size)| Row { path, depth: 0, dir, open: false, size }).collect();
        self.st.select(if self.rows.is_empty() { None } else { Some(0) });
    }

    fn cur(&self) -> Option<&Row> {
        self.rows.get(self.st.selected()?)
    }

    fn mv(&mut self, d: isize) {
        if !self.rows.is_empty() {
            let i = self.st.selected().unwrap_or(0) as isize + d;
            self.st.select(Some(i.clamp(0, self.rows.len() as isize - 1) as usize));
        }
    }

    fn open(&mut self, kids: Vec<Item>) {
        if let Some(i) = self.st.selected() {
            self.open_at(i, kids);
        }
    }

    fn open_at(&mut self, i: usize, kids: Vec<Item>) {
        let depth = self.rows[i].depth + 1;
        self.rows[i].open = true;
        let new = kids.into_iter().map(|(path, dir, size)| Row { path, depth, dir, open: false, size });
        self.rows.splice(i + 1..i + 1, new);
    }

    /// Closes the folder under the cursor, or moves to its parent.
    fn close(&mut self) {
        let Some(i) = self.st.selected() else { return };
        let depth = self.rows[i].depth;
        if self.rows[i].open {
            let end =
                self.rows[i + 1..].iter().position(|r| r.depth <= depth).map(|n| i + 1 + n).unwrap_or(self.rows.len());
            self.rows.drain(i + 1..end);
            self.rows[i].open = false;
        } else if let Some(p) = self.rows[..i].iter().rposition(|r| r.depth < depth) {
            self.st.select(Some(p));
        }
    }
}

pub(crate) fn list_dir(dir: &str, hidden: bool) -> Vec<Item> {
    let mut v: Vec<Item> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| hidden || !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            Some((e.path().to_string_lossy().into_owned(), md.is_dir(), md.len()))
        })
        .collect();
    v.sort_by_key(|a| (!a.1, a.0.to_lowercase()));
    v
}

pub(crate) fn name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

/// "5 min ago", "3 h ago", "2 days ago" or "never".
pub(crate) fn ago(ts: Option<i64>) -> String {
    let Some(t) = ts else { return "never".into() };
    let s = (chrono::Utc::now().timestamp() - t).max(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

/// "now", "in 5 min" or "in 3 h".
pub(crate) fn until(ts: i64) -> String {
    let s = ts - chrono::Utc::now().timestamp();
    match s {
        i64::MIN..=20 => "any moment".into(),
        21..=119 => "in a minute".into(),
        120..=5399 => format!("in {} min", s / 60),
        _ => format!("in {} h", (s + 1800) / 3600),
    }
}

/// Shortens a path by replacing the home folder with `~`.
pub(crate) fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && path.starts_with(&h) => format!("~{}", &path[h.len()..]),
        _ => path.to_string(),
    }
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

impl RestoreView {
    /// Shows only entries whose name contains the filter text, or the full tree when it is empty.
    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        if needle.is_empty() {
            self.tree.reset(self.roots.clone());
            return;
        }
        let hits: Vec<Item> =
            self.all.iter().filter(|i| name(&i.0).to_lowercase().contains(&needle)).take(5000).cloned().collect();
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
            Some(v) if !r.dir && self.sigs.get(&r.path) != Some(v) => Some(("different now", Color::Yellow)),
            Some(_) if r.dir && self.changed_dirs.contains(&r.path) => Some(("changes inside", Color::Yellow)),
            _ => None,
        }
    }
}

enum Prompt {
    FolderFilter,
    Setting(&'static str),
    Filter,
    NewSet,
    DeleteSet(String),
    RenameSet(String),
    Drive,
    /// Adding cloud storage, one answer at a time: the answers so far.
    Cloud(Vec<String>),
    /// Name for a new exclusion list that starts as a copy of this one.
    ExNew(String),
    ExAdd(String),
    ExDelete(String),
    /// A history rule ("days" or "last") for one destination.
    DestKeep(String, &'static str),
    Remove(String),
    Every(String),
    Quota(String),
    InLimit(String),
    Join,
    RestoreTo,
    Date,
}

/// Everything that can be chosen from a menu or done in a pane.
#[derive(Clone, Copy, PartialEq)]
enum Act {
    BackupSet,
    VerifySet,
    VerifySetFull,
    VerifySetLocal,
    BackupAll,
    NewSet,
    DeleteSet,
    RenameSet,
    Toggle,
    Hidden,
    UpFolder,
    Invite,
    Accept,
    AddDrive,
    AddCloud,
    SetExclude,
    ExNew,
    ExCopy,
    ExAdd,
    ExRemove(usize),
    ExDelete,
    DestDays,
    DestLast,
    DestDefault,
    Pace,
    Quota,
    InLimit,
    ServiceInfo,
    UseDest,
    RemoveDest,
    Pick,
    RestorePicked,
    Search,
    Timeline,
    Date,
    Versions,
    Source,
    Older,
    Newer,
    Refresh,
}

/// What adding cloud storage asks for, in order; the secret key comes last and is not shown.
const CLOUD_STEPS: [&str; 7] = [
    "a name for this destination",
    "endpoint, such as https://s3.us-west-004.backblazeb2.com",
    "bucket",
    "region",
    "folder inside the bucket (blank = none)",
    "access key (key id)",
    "secret key",
];

/// Where "Files and folders" and "Destinations" sit in a set's menu.
const SET_FILES: usize = 4;
const SET_DESTS: usize = 5;

/// The pages, in the order of the top-level menu.
const BACKUP: usize = 0;
const RESTORE: usize = 1;
const DESTS: usize = 2;
const SETTINGS: usize = 3;
const HISTORY: usize = 4;
const STATUS: usize = 5;

/// What the Backup page is showing.
#[derive(PartialEq)]
enum Sub {
    Sets,
    Files,
    Dests,
    Rules,
    Excludes,
    /// Restore, before a backup set is chosen.
    RestoreSets,
}

/// One level of the drill-down menu on the left.
#[derive(Clone, PartialEq)]
enum Kind {
    Root,
    Backup,
    Set(String),
    Rules(String),
    Restore,
    /// Restoring from one backup set.
    RestoreSet(String),
    Dests,
    Dest(String),
    Settings,
    Excludes,
    ExList(String),
}

/// What choosing a menu item does.
#[derive(Clone)]
enum Choice {
    /// Go one level down.
    Open(Kind),
    /// Move into the pane on the right.
    Focus,
    Do(Act),
    /// Type a new value for a setting.
    Value(&'static str),
}

const GENERAL: [(&str, &str); 12] = [
    ("hours", "Hours automatic backups may run, such as 22-7 ('always' for any time)"),
    ("pause", "Automatic backups (enter: pause for a number of hours; 0 resumes)"),
    ("every", "Minutes between automatic backups to local drives (friends default to 60)"),
    ("limit", "Upload speed limit in KB/s across friends (0 = unlimited)"),
    ("warn", "Warn when a backup is this many days old (0 = never)"),
    ("versions", "Max versions kept of each file (0 = unlimited)"),
    ("days", "Delete restore points older than this many days (0 = never)"),
    ("last", "Always keep this many most recent restore points"),
    ("hourly", "Keep one per hour for this many hours"),
    ("daily", "Keep one per day for this many days"),
    ("weekly", "Keep one per week for this many weeks"),
    ("monthly", "Keep one per month for this many months"),
];

const RULES: [(&str, &str); 4] = [
    ("set-every", "Minutes between automatic backups of this set (0 = each destination's own pace)"),
    ("set-hours", "Hours this set may back up, such as 22-7 ('always' = the general hours)"),
    ("set-versions", "Max versions kept of each file in this set (0 = unlimited)"),
    ("set-days", "Delete this set's restore points older than this many days (0 = never)"),
];

struct App {
    exec: Arc<Exec>,
    done_tx: mpsc::UnboundedSender<Done>,
    progress_tx: mpsc::UnboundedSender<String>,
    busy: Option<String>,
    progress: String,
    spin: usize,
    cfg: Config,
    /// The daemon's picture of sets, destinations and warnings.
    ov: Value,
    /// The menu levels entered so far, each with its cursor; the last one is on screen.
    stack: Vec<(Kind, usize)>,
    page: usize,
    in_menu: bool,
    sub: Sub,
    /// The cursor in Restore's list of backup sets.
    rsets: ListState,
    /// The exclusion list shown on the right of Settings > Exclusion lists.
    exlist: Option<String>,
    /// The backup set being looked at or edited.
    set: String,
    sets: ListState,
    root: String,
    hidden: bool,
    folders: Tree,
    /// Text the folder picker is searching for, and a counter that tells stale results apart.
    folder_filter: String,
    folder_search: u64,
    found_tx: mpsc::UnboundedSender<(u64, Vec<Item>)>,
    set_dests: ListState,
    rules: ListState,
    dests: ListState,
    settings: ListState,
    restore: RestoreView,
    /// Restore from this destination only; None means any of them.
    source: Option<String>,
    /// The backup set shown on the Restore page (None = whichever has the newest restore point).
    rset: Option<String>,
    text: Vec<String>,
    text_at: Instant,
    scroll: usize,
    input: Option<(Prompt, String)>,
    popup: Option<Vec<String>>,
    versions: Option<(Vec<(usize, String)>, ListState)>,
    status: String,
}

impl App {
    async fn reload(&mut self) {
        match self.exec.call(Cmd::Config).await {
            Ok(l) => match l.first().and_then(|s| serde_json::from_str(s).ok()) {
                Some(c) => self.cfg = c,
                None => self.status = "could not read the configuration".into(),
            },
            Err(e) => self.status = format!("{e:#}"),
        }
        if let Ok(l) = self.exec.call(Cmd::Overview).await {
            if let Some(v) = l.first().and_then(|s| serde_json::from_str(s).ok()) {
                self.ov = v;
            }
        }
        if self.cfg.set(&self.set).is_none() {
            self.set = self.cfg.sets.first().map(|b| b.name.clone()).unwrap_or_else(|| DEFAULT_SET.into());
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
        self.folder_filter.clear();
        // Backed up folders are stored by their real path, so browse by real path too.
        let root = std::fs::canonicalize(&root).map(|p| p.to_string_lossy().into_owned()).unwrap_or(root);
        self.folders.reset(list_dir(&root, self.hidden));
        self.root = root;
    }

    /// Searches below the folder being browsed for names containing the text, in the
    /// background, so typing stays smooth in a large home folder.
    fn search_folders(&mut self, text: String) {
        self.folder_filter = text.clone();
        self.folder_search += 1;
        if text.is_empty() {
            self.set_root(self.root.clone());
            return;
        }
        let (root, hidden, search, tx) = (self.root.clone(), self.hidden, self.folder_search, self.found_tx.clone());
        tokio::task::spawn_blocking(move || {
            // Level by level, so what is near the top is found first however large the
            // folders beside it are; results so far are shown after each level.
            let needle = text.to_lowercase();
            let began = Instant::now();
            let mut hits: Vec<Item> = vec![];
            let mut level = vec![PathBuf::from(&root)];
            let mut seen = 0u64;
            while !level.is_empty() && hits.len() < 2000 && seen < 3_000_000 && began.elapsed() < Duration::from_secs(8)
            {
                let mut next = vec![];
                for dir in &level {
                    let Ok(rd) = std::fs::read_dir(dir) else { continue };
                    for e in rd.flatten() {
                        seen += 1;
                        let fname = e.file_name().to_string_lossy().into_owned();
                        if !hidden && fname.starts_with('.') {
                            continue;
                        }
                        let Ok(ft) = e.file_type() else { continue };
                        if fname.to_lowercase().contains(&needle) && hits.len() < 2000 {
                            let size = if ft.is_dir() { 0 } else { e.metadata().map(|m| m.len()).unwrap_or(0) };
                            hits.push((e.path().to_string_lossy().into_owned(), ft.is_dir(), size));
                        }
                        if ft.is_dir() {
                            next.push(e.path());
                        }
                    }
                }
                let mut sorted = hits.clone();
                sorted.sort_by_key(|a| (!a.1, a.0.matches('/').count(), a.0.to_lowercase()));
                if tx.send((search, sorted)).is_err() {
                    return;
                }
                level = next;
            }
        });
    }

    fn cur_set(&self) -> BackupSet {
        self.cfg.set(&self.set).cloned().unwrap_or_else(|| BackupSet { name: self.set.clone(), ..Default::default() })
    }

    fn excluded(&self, path: &str) -> bool {
        self.cur_set().skip.iter().any(|p| p == Path::new(path))
    }

    /// The other set that already backs up this exact folder, if any.
    fn other_set(&self, path: &str) -> Option<&str> {
        let p = Path::new(path);
        self.cfg.sets.iter().find(|b| b.name != self.set && b.paths.iter().any(|x| x == p)).map(|b| b.name.as_str())
    }

    fn folder_mark(&self, path: &str) -> (&'static str, Color) {
        let p = Path::new(path);
        let BackupSet { paths, skip, .. } = self.cur_set();
        let inside = paths.iter().any(|x| p.starts_with(x));
        if paths.iter().any(|x| x == p) {
            ("[x]", Color::Green)
        } else if inside && skip.iter().any(|e| p.starts_with(e)) {
            ("[-]", Color::Red)
        } else if inside {
            ("[+]", Color::Green)
        } else if paths.iter().any(|x| x.starts_with(p)) {
            // Not backed up itself, but something inside it is.
            ("[o]", Color::Cyan)
        } else {
            ("[ ]", Color::Reset)
        }
    }

    /// A list of multi-line cards. The chosen card is marked by a bar down its left edge
    /// and bold text, which keeps the colours inside it readable.
    fn cards<'a>(items: Vec<ListItem<'a>>, block: Block<'a>, chosen: bool) -> List<'a> {
        let list = List::new(items).block(block).highlight_spacing(ratatui::widgets::HighlightSpacing::Always);
        if chosen {
            list.highlight_symbol("▌ ")
                .repeat_highlight_symbol(true)
                .highlight_style(Style::new().add_modifier(Modifier::BOLD))
        } else {
            list.highlight_symbol("  ")
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
            Cmd::Skip { path: p, set: Some(self.set.clone()), remove: self.excluded(&path) }
        } else {
            Cmd::AddPaths { paths: vec![p], set: Some(self.set.clone()) }
        };
        self.apply(cmd).await;
    }

    fn set_edit(&self, to: Option<Vec<String>>) -> Cmd {
        Cmd::SetEdit {
            name: self.set.clone(),
            to,
            every: None,
            hours: None,
            days: None,
            versions: None,
            last: None,
            delete: false,
        }
    }

    /// Switches whether the current set goes to a destination.
    async fn toggle_destination(&mut self, name: &str) {
        let b = self.cur_set();
        let Some(f) = self.cfg.friends.iter().find(|f| f.name == name).cloned() else {
            return;
        };
        let mut list: Vec<String> = self.cfg.friends.iter().filter(|x| b.sends_to(x)).map(|x| x.name.clone()).collect();
        if b.sends_to(&f) {
            list.retain(|n| n != name);
            if list.is_empty() {
                self.status = "a set needs at least one destination".into();
                return;
            }
        } else {
            if !f.backup_to {
                self.apply(Cmd::FriendBackupTo { name: name.to_string(), on: true }).await;
            }
            list.push(name.to_string());
        }
        self.apply(self.set_edit(Some(list))).await;
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
        let now = chrono::Utc::now().timestamp();
        match key {
            "set-hours" if self.cur_set().hours.is_empty() => "general".into(),
            "set-hours" => self.cur_set().hours,
            "hours" if self.cfg.backup_hours.is_empty() => "any time".into(),
            "hours" => self.cfg.backup_hours.clone(),
            "pause" if self.cfg.paused_until - now > 365 * 86400 => "paused".into(),
            "pause" if self.cfg.paused_until > now => {
                format!("paused to {}", &when(self.cfg.paused_until)[5..16])
            }
            "pause" => "running".into(),
            _ => self.setting(key).to_string(),
        }
    }

    /// Saves one setting from typed text; returns false when the text makes no sense for it.
    async fn set_setting(&mut self, key: &str, text: &str) -> bool {
        let text = text.trim();
        let num = text.parse::<u32>().ok();
        let always = ["always", "any time", "general"].iter().any(|w| text.eq_ignore_ascii_case(w));
        let hours = if always { String::new() } else { text.to_string() };
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
        let tune =
            |upload_kbps, hours, pause_hours, warn_days| Cmd::Tune { upload_kbps, hours, pause_hours, warn_days };
        let cmd = match (key, num) {
            ("set-every", Some(n)) => edit(Some(n), None, None, None),
            ("set-hours", _) => edit(None, Some(hours), None, None),
            ("set-versions", Some(n)) => edit(None, None, None, Some(n)),
            ("set-days", Some(n)) => edit(None, None, Some(n), None),
            ("hours", _) => tune(None, Some(hours), None, None),
            ("pause", _) if text.is_empty() => tune(None, None, Some(24 * 3650), None),
            ("pause", Some(n)) => tune(None, None, Some(n as i64), None),
            ("every", Some(n)) => Cmd::Every { name: None, minutes: n },
            ("limit", Some(n)) => tune(Some(n as u64), None, None, None),
            ("warn", Some(n)) => tune(None, None, None, Some(n)),
            ("versions", Some(n)) => Cmd::Keep { set: vec![], versions: Some(n), pattern: None },
            (k, Some(n)) if !["hours", "pause", "set-hours"].contains(&k) => {
                Cmd::Keep { set: vec![(k.to_string(), n)], versions: None, pattern: None }
            }
            _ => return false,
        };
        self.apply(cmd).await;
        true
    }

    /// Starts a background job unless one is already running.
    fn start<F>(&mut self, label: &str, make: impl FnOnce(Arc<Exec>, mpsc::UnboundedSender<String>) -> F) -> bool
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
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|v| v["id"].is_string())
                .map(|v| Snap {
                    set: v["set"].as_str().unwrap_or(DEFAULT_SET).to_string(),
                    at: v["at"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
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
            let snaps: Vec<Snap> = snaps.into_iter().filter(|s| Some(&s.set) == set.as_ref()).collect();
            let tree = match snaps.last() {
                Some(s) => ex.call(Cmd::Tree { snap: s.id.clone(), from }).await.ok().map(parse_listing),
                None => None,
            };
            let notes =
                lines.iter().filter(|l| !l.trim_start().starts_with('{')).map(|l| l.trim().to_string()).collect();
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

    /// Loads the text pages (History, Status) in the background.
    fn load_text(&mut self, tx: &mpsc::UnboundedSender<(usize, Vec<String>)>) {
        self.text_at = Instant::now();
        let (ex, tx, page) = (self.exec.clone(), tx.clone(), self.page);
        tokio::spawn(async move {
            let cmd = if page == HISTORY { Cmd::Log } else { Cmd::Status };
            let mut lines = lines_or_error(ex.call(cmd).await);
            if page == HISTORY && lines.is_empty() {
                lines.push("Nothing has happened since the daemon started.".into());
            }
            let _ = tx.send((page, lines));
        });
    }

    /// Loads whatever the current page still needs.
    fn kick(&mut self) {
        let chosen = matches!(self.stack.last(), Some((Kind::RestoreSet(_), _)));
        if self.busy.is_none() && chosen && !self.restore.loaded {
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
                self.reload().await;
            }
            Done::Paired(lines) => {
                self.popup = Some(lines);
                self.reload().await;
            }
            Done::Snaps(snaps, tree, notes, set) => {
                self.rset = set;
                if !notes.is_empty() {
                    self.status = format!("{} (showing what the others hold; u retries)", notes.join("; "));
                }
                self.restore = RestoreView { loaded: true, ..Default::default() };
                self.restore.cur = snaps.len().saturating_sub(1);
                self.restore.snaps = snaps;
                if let Some(items) = tree {
                    self.restore.latest = items.iter().map(|i| (i.0.clone(), i.3.clone())).collect();
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
                for v in lines.iter().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
                    let Some(idx) = self.restore.snaps.iter().position(|s| Some(s.id.as_str()) == v["id"].as_str())
                    else {
                        continue;
                    };
                    let what = match v["size"].as_u64() {
                        Some(n) => format!("{:>10}", human(n)),
                        None => "  (deleted)".into(),
                    };
                    let at: Vec<&str> =
                        v["at"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
                    list.push((
                        idx,
                        format!("{}  {what}   on {}", when(v["time"].as_i64().unwrap_or(0)), at.join(", ")),
                    ));
                }
                list.reverse();
                let mut st = ListState::default();
                st.select(list.iter().position(|(i, _)| *i <= self.restore.cur).or(Some(0)));
                self.versions = Some((list, st));
            }
            Done::Failed(e) => self.status = e,
        }
    }

    /// Shows a restore point's files, keeping the same folders open and the same entry selected.
    fn show_tree(&mut self, items: Listing) {
        let rv = &mut self.restore;
        let open: HashSet<String> = rv.tree.rows.iter().filter(|r| r.open).map(|r| r.path.clone()).collect();
        let at = rv.tree.cur().map(|r| r.path.clone());
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
            let parent = Path::new(p).parent().map(|x| x.to_string_lossy().into_owned());
            match parent.filter(|x| all.contains(x.as_str())) {
                Some(parent) => kids.entry(parent).or_default().push(it),
                None => roots.push(it),
            }
        }
        for v in kids.values_mut() {
            v.sort_by_key(|a| (!a.1, a.0.to_lowercase()));
        }
        rv.kids = kids;
        rv.all = items.iter().map(|(p, d, s, _)| (p.clone(), *d, *s)).collect();
        rv.all.sort_by(|a, b| a.0.cmp(&b.0));
        rv.roots = roots;
        rv.apply_filter();
        let mut i = 0;
        while i < rv.tree.rows.len() {
            if rv.tree.rows[i].dir && open.contains(&rv.tree.rows[i].path) {
                let k = rv.kids.get(&rv.tree.rows[i].path).cloned().unwrap_or_default();
                rv.tree.open_at(i, k);
            }
            i += 1;
        }
        if let Some(pos) = at.and_then(|p| rv.tree.rows.iter().position(|r| r.path == p)) {
            rv.tree.st.select(Some(pos));
        }
        let last = rv.snaps.len().saturating_sub(1);
        rv.tl.select(Some(last - rv.cur.min(last)));
    }

    /// The name of the set under the cursor on the Backup page, if the cursor is on one.
    fn picked_set(&self) -> Option<String> {
        self.cfg.sets.get(self.sets.selected().unwrap_or(0)).map(|b| b.name.clone())
    }

    fn picked_dest(&self) -> Option<crate::config::Friend> {
        self.cfg.friends.get(self.dests.selected().unwrap_or(0)).cloned()
    }

    /// The items of one menu level, with current values shown where there is one.
    fn items(&self, kind: &Kind) -> Vec<(String, Choice)> {
        let val = |label: &str, key: &'static str| (format!("{label}: {}", self.setting_text(key)), Choice::Value(key));
        let go = |label: &str, k: Kind| (label.to_string(), Choice::Open(k));
        let act = |label: String, a: Act| (label, Choice::Do(a));
        match kind {
            Kind::Root => vec![
                go("Backup", Kind::Backup),
                go("Restore", Kind::Restore),
                go("Destinations", Kind::Dests),
                go("Settings", Kind::Settings),
                ("History".into(), Choice::Focus),
                ("Status".into(), Choice::Focus),
            ],
            Kind::Backup => {
                let mut v: Vec<_> = self.cfg.sets.iter().map(|b| go(&b.name, Kind::Set(b.name.clone()))).collect();
                v.push(act("+ New backup set".into(), Act::NewSet));
                v.push(act("Back up all sets now".into(), Act::BackupAll));
                v
            }
            Kind::Set(n) => vec![
                act("Back up now".into(), Act::BackupSet),
                act("Verify (quick check)".into(), Act::VerifySet),
                act("Verify everything (slow)".into(), Act::VerifySetFull),
                act("Verify the files on this computer".into(), Act::VerifySetLocal),
                ("Files and folders".into(), Choice::Focus),
                ("Destinations".into(), Choice::Focus),
                go("Schedule and history", Kind::Rules(n.clone())),
                act(
                    format!(
                        "Leaves out: {} list",
                        self.cfg.set(n).map(|b| self.cfg.list_for(b).name).unwrap_or_default()
                    ),
                    Act::SetExclude,
                ),
                act(format!("Hidden files: {}", if self.hidden { "shown" } else { "not shown" }), Act::Hidden),
                act("Rename this set".into(), Act::RenameSet),
                act("Delete this set".into(), Act::DeleteSet),
            ],
            Kind::Rules(_) => vec![
                val("Minutes between backups", "set-every"),
                val("Backup hours", "set-hours"),
                val("Max versions of a file", "set-versions"),
                val("Max age in days", "set-days"),
            ],
            Kind::Restore => self.cfg.sets.iter().map(|b| go(&b.name, Kind::RestoreSet(b.name.clone()))).collect(),
            Kind::RestoreSet(_) => {
                let rv = &self.restore;
                let file = rv.tree.cur().filter(|r| !r.dir).map(|r| name(&r.path));
                let mut v = vec![
                    ("Browse and pick files".to_string(), Choice::Focus),
                    act(format!("Restore picked ({})", rv.picked.len()), Act::RestorePicked),
                    act(
                        if rv.filter.is_empty() {
                            "Search by name".into()
                        } else {
                            format!("Search: \"{}\"", rv.filter)
                        },
                        Act::Search,
                    ),
                    act("Go back in time".into(), Act::Timeline),
                    act("Jump to a date".into(), Act::Date),
                    act(format!("Versions of {}", file.as_deref().unwrap_or("a file")), Act::Versions),
                    act(format!("Read from: {}", self.source.as_deref().unwrap_or("all destinations")), Act::Source),
                ];
                v.push(act("Older restore point".into(), Act::Older));
                v.push(act("Newer restore point".into(), Act::Newer));
                v.push(act("Look again".into(), Act::Refresh));
                v
            }
            Kind::Dests => {
                let mut v: Vec<_> = self.cfg.friends.iter().map(|f| go(&f.name, Kind::Dest(f.name.clone()))).collect();
                v.push(act("+ Invite a friend".into(), Act::Invite));
                v.push(act("+ Accept an invite".into(), Act::Accept));
                v.push(act("+ Add a drive or folder".into(), Act::AddDrive));
                v.push(act("+ Add cloud storage (S3, B2, ...)".into(), Act::AddCloud));
                v
            }
            Kind::Dest(n) => {
                let Some(f) = self.cfg.friends.iter().find(|f| &f.name == n) else { return vec![] };
                let mut v = vec![
                    act(format!("In use: {}", if f.backup_to { "yes" } else { "no" }), Act::UseDest),
                    act(format!("Back up here every: {} min", f.interval_secs(&self.cfg) / 60), Act::Pace),
                ];
                if f.is_friend() {
                    v.push(act(format!("They may store here: {} GB", f.quota_gb), Act::Quota));
                    let speed = if f.max_in_kbps == 0 { "any".to_string() } else { format!("{} KB/s", f.max_in_kbps) };
                    v.push(act(format!("Their upload speed here: {speed}"), Act::InLimit));
                }
                if f.cloud.is_some() {
                    let cap = if f.quota_gb == 0 { "no cap".to_string() } else { format!("{} GB", f.quota_gb) };
                    v.push(act(format!("Store at most: {cap}"), Act::Quota));
                    v.push(act("New keys or address".into(), Act::AddCloud));
                }
                let rule = |n: Option<u32>| match n {
                    None => "each set's rule".to_string(),
                    Some(0) => "unlimited".to_string(),
                    Some(n) => n.to_string(),
                };
                v.push(act(
                    format!("Max age in days here: {}", rule(f.retention.as_ref().map(|r| r.keep_days))),
                    Act::DestDays,
                ));
                v.push(act(
                    format!("Keep most recent here: {}", rule(f.retention.as_ref().map(|r| r.keep_last))),
                    Act::DestLast,
                ));
                if f.retention.is_some() {
                    v.push(act("Use each set's history rules here".into(), Act::DestDefault));
                }
                v.push(act("Remove this destination".into(), Act::RemoveDest));
                v
            }
            Kind::Settings => vec![
                act(
                    format!(
                        "Start at login: {}",
                        if self.ov["service"].as_bool() == Some(true) { "yes" } else { "no" }
                    ),
                    Act::ServiceInfo,
                ),
                val("Backup hours", "hours"),
                val("Automatic backups", "pause"),
                val("Minutes between drive backups", "every"),
                val("Upload limit KB/s", "limit"),
                val("Warn after days", "warn"),
                val("Max versions of a file", "versions"),
                val("Max age in days", "days"),
                val("Keep most recent", "last"),
                val("Keep hourly", "hourly"),
                val("Keep daily", "daily"),
                val("Keep weekly", "weekly"),
                val("Keep monthly", "monthly"),
                go("Exclusion lists", Kind::Excludes),
            ],
            Kind::Excludes => {
                let mut v: Vec<_> = self
                    .cfg
                    .lists()
                    .iter()
                    .map(|l| {
                        let label =
                            if is_builtin_list(&l.name) { format!("{} (built in)", l.name) } else { l.name.clone() };
                        go(&label, Kind::ExList(l.name.clone()))
                    })
                    .collect();
                v.push(act("+ New list (starts as Standard)".into(), Act::ExNew));
                v
            }
            Kind::ExList(n) => {
                let mut v = vec![act("Copy to a new list".into(), Act::ExCopy)];
                if !is_builtin_list(n) {
                    v.push(act("+ Add a pattern".into(), Act::ExAdd));
                    let list = self.cfg.lists().into_iter().find(|l| &l.name == n);
                    for (i, p) in list.iter().flat_map(|l| l.patterns.iter()).enumerate() {
                        v.push(act(format!("remove {p}"), Act::ExRemove(i)));
                    }
                    v.push(act("Delete this list".into(), Act::ExDelete));
                }
                v
            }
        }
    }

    /// Makes the right-hand side follow the menu: which page, which set, which row.
    fn sync(&mut self) {
        // Leave levels whose subject no longer exists, such as a deleted set.
        loop {
            let alive = match &self.stack.last().expect("the root level is never removed").0 {
                Kind::Set(n) | Kind::Rules(n) | Kind::RestoreSet(n) => self.cfg.set(n).is_some(),
                Kind::Dest(n) => self.cfg.friends.iter().any(|f| &f.name == n),
                Kind::ExList(n) => self.cfg.lists().iter().any(|l| &l.name == n),
                _ => true,
            };
            if alive || self.stack.len() == 1 {
                break;
            }
            self.stack.pop();
            self.in_menu = true;
        }
        let (kind, sel) = self.stack.last().cloned().expect("the root level is never removed");
        let sel = sel.min(self.items(&kind).len().saturating_sub(1));
        self.stack.last_mut().expect("the root level is never removed").1 = sel;
        let before = self.page;
        self.sub = Sub::Sets;
        match &kind {
            Kind::Root => {
                self.page = sel;
                if sel == RESTORE {
                    self.sub = Sub::RestoreSets;
                }
            }
            Kind::Backup => {
                self.page = BACKUP;
                self.sets.select(Some(sel.min(self.cfg.sets.len())));
                if let Some(b) = self.cfg.sets.get(sel) {
                    self.set = b.name.clone();
                }
            }
            Kind::Set(n) | Kind::Rules(n) => {
                self.page = BACKUP;
                self.set = n.clone();
                self.sets.select(self.cfg.sets.iter().position(|b| &b.name == n));
                if matches!(kind, Kind::Rules(_)) {
                    self.sub = Sub::Rules;
                    self.rules.select(Some(sel));
                } else if sel == SET_FILES {
                    self.sub = Sub::Files;
                } else if sel == SET_DESTS {
                    self.sub = Sub::Dests;
                }
            }
            Kind::Restore => {
                self.page = RESTORE;
                self.sub = Sub::RestoreSets;
                self.rsets.select(Some(sel));
            }
            Kind::RestoreSet(n) => {
                self.page = RESTORE;
                if self.rset.as_ref() != Some(n) {
                    self.rset = Some(n.clone());
                    self.restore = RestoreView::default();
                }
            }
            Kind::Dests => {
                self.page = DESTS;
                self.dests.select(Some(sel.min(self.cfg.friends.len().saturating_sub(1))));
            }
            Kind::Dest(n) => {
                self.page = DESTS;
                self.dests.select(self.cfg.friends.iter().position(|f| &f.name == n));
            }
            Kind::Settings => {
                self.page = SETTINGS;
                // The first menu row is the start-at-login line, which has no row in the table.
                self.settings.select(sel.checked_sub(1));
                if sel > GENERAL.len() {
                    self.sub = Sub::Excludes;
                    self.exlist = None;
                }
            }
            Kind::Excludes => {
                self.page = SETTINGS;
                self.sub = Sub::Excludes;
                self.exlist = self.cfg.lists().get(sel).map(|l| l.name.clone());
            }
            Kind::ExList(n) => {
                self.page = SETTINGS;
                self.sub = Sub::Excludes;
                self.exlist = Some(n.clone());
            }
        }
        if self.page != before {
            self.scroll = 0;
            self.text_at = Instant::now() - Duration::from_secs(3600);
        }
    }

    async fn act(&mut self, a: Act) {
        match a {
            Act::BackupSet | Act::BackupAll => {
                if a == Act::BackupSet && self.picked_set().is_none() {
                    self.status = "choose a backup set first".into();
                    return;
                }
                let set = if a == Act::BackupSet { self.picked_set() } else { None };
                self.start("backing up", |ex, pr| async move {
                    Done::BackedUp(lines_or_error(ex.live(Cmd::Backup { tag: None, set }, &pr).await))
                });
            }
            Act::VerifySet | Act::VerifySetFull | Act::VerifySetLocal => {
                let Some(set) = self.picked_set() else { return };
                let cmd =
                    Cmd::Verify { full: a == Act::VerifySetFull, local: a == Act::VerifySetLocal, set: Some(set) };
                self.start("verifying", |ex, pr| async move { Done::Popup(lines_or_error(ex.live(cmd, &pr).await)) });
            }
            Act::NewSet => self.input = Some((Prompt::NewSet, String::new())),
            Act::RenameSet => {
                if let Some(n) = self.picked_set() {
                    self.input = Some((Prompt::RenameSet(n.clone()), n));
                }
            }
            Act::DeleteSet => match self.picked_set() {
                Some(_) if self.cfg.sets.len() <= 1 => self.status = "the last set cannot be deleted".into(),
                Some(n) => self.input = Some((Prompt::DeleteSet(n), String::new())),
                None => {}
            },
            Act::Toggle if self.sub == Sub::Files => self.toggle_folder().await,
            Act::Toggle => {
                let i = self.set_dests.selected().unwrap_or(0);
                if let Some(n) = self.cfg.friends.get(i).map(|f| f.name.clone()) {
                    self.toggle_destination(&n).await;
                }
            }
            Act::Hidden => {
                self.hidden = !self.hidden;
                self.set_root(self.root.clone());
            }
            Act::UpFolder => {
                if let Some(parent) = Path::new(&self.root).parent() {
                    self.set_root(parent.to_string_lossy().into_owned());
                }
            }
            Act::ServiceInfo => {
                let on = self.ov["service"].as_bool() == Some(true);
                self.popup = Some(if on {
                    vec![
                        "The background service is installed: the daemon starts by itself when you log in.".into(),
                        String::new(),
                        "To remove it, quit and run:  sdb service uninstall".into(),
                    ]
                } else {
                    vec![
                        "The background service is not installed.".into(),
                        "The daemon runs now because sdb started it, but after a restart of this computer".into(),
                        "nothing is backed up, and friends cannot back up to you, until you run sdb again.".into(),
                        String::new(),
                        "To have it start at login, quit and run:  sdb service install".into(),
                    ]
                });
            }
            Act::Invite => {
                self.start("creating an invite", |ex, _| async move {
                    Done::Popup(lines_or_error(ex.call(Cmd::Invite).await))
                });
            }
            Act::Accept => self.input = Some((Prompt::Join, String::new())),
            Act::AddDrive => self.input = Some((Prompt::Drive, "/Volumes/".into())),
            Act::SetExclude => {
                // Enter moves the set on to the next list.
                let lists = self.cfg.lists();
                let now = self.cfg.list_for(&self.cur_set()).name;
                let at = lists.iter().position(|l| l.name == now).unwrap_or(0);
                let list = lists[(at + 1) % lists.len()].name.clone();
                self.apply(Cmd::SetExclude { name: self.set.clone(), list }).await;
            }
            Act::ExNew => self.input = Some((Prompt::ExNew(STANDARD.into()), String::new())),
            Act::ExCopy | Act::ExAdd | Act::ExDelete | Act::ExRemove(_) => {
                let Some((Kind::ExList(list), _)) = self.stack.last().cloned() else { return };
                match a {
                    Act::ExCopy => self.input = Some((Prompt::ExNew(list), String::new())),
                    Act::ExAdd => self.input = Some((Prompt::ExAdd(list), String::new())),
                    Act::ExDelete => self.input = Some((Prompt::ExDelete(list), String::new())),
                    Act::ExRemove(i) => {
                        let pattern = self
                            .cfg
                            .lists()
                            .into_iter()
                            .find(|l| l.name == list)
                            .and_then(|l| l.patterns.get(i).cloned());
                        if let Some(pattern) = pattern {
                            self.apply(Cmd::ExcludePattern { list, pattern, remove: true }).await;
                        }
                    }
                    _ => {}
                }
            }
            Act::AddCloud => {
                // From a cloud destination's own menu this replaces its details, keeping its name.
                let at = matches!(self.stack.last(), Some((Kind::Dest(_), _)));
                let known = self.picked_dest().filter(|f| at && f.cloud.is_some());
                self.input = Some(match known {
                    Some(f) => {
                        let c = f.cloud.unwrap_or_else(|| unreachable!());
                        (Prompt::Cloud(vec![f.name]), c.endpoint)
                    }
                    None => (Prompt::Cloud(vec![]), String::new()),
                });
            }
            Act::Pace
            | Act::Quota
            | Act::InLimit
            | Act::RemoveDest
            | Act::UseDest
            | Act::DestDays
            | Act::DestLast
            | Act::DestDefault => {
                let Some(f) = self.picked_dest() else {
                    self.status = "there is no destination yet: invite a friend or add a drive".into();
                    return;
                };
                match a {
                    Act::Pace => self.input = Some((Prompt::Every(f.name), String::new())),
                    Act::DestDays => self.input = Some((Prompt::DestKeep(f.name, "days"), String::new())),
                    Act::DestLast => self.input = Some((Prompt::DestKeep(f.name, "last"), String::new())),
                    Act::DestDefault => self.apply(Cmd::DestKeep { name: f.name, set: vec![], clear: true }).await,
                    Act::Quota if f.path.is_some() => self.status = "a drive has no quota".into(),
                    Act::Quota => self.input = Some((Prompt::Quota(f.name), String::new())),
                    Act::InLimit if !f.is_friend() => self.status = "only a friend has a speed limit".into(),
                    Act::InLimit => self.input = Some((Prompt::InLimit(f.name), String::new())),
                    Act::RemoveDest => self.input = Some((Prompt::Remove(f.name), String::new())),
                    _ => self.apply(Cmd::FriendBackupTo { name: f.name, on: !f.backup_to }).await,
                }
            }
            Act::Pick if self.restore.timeline => {
                let n = self.restore.snaps.len();
                let i = self.restore.tl.selected().unwrap_or(0);
                if n > 0 {
                    self.goto(n - 1 - i.min(n - 1));
                }
            }
            Act::Pick => {
                let rv = &mut self.restore;
                if let Some(p) = rv.tree.cur().map(|r| r.path.clone()) {
                    if !rv.picked.remove(&p) {
                        rv.picked.insert(p);
                    }
                }
            }
            Act::RestorePicked => {
                if self.restore.picked.is_empty() {
                    self.status = "pick files or folders with space first".into();
                } else {
                    let home = std::env::var("HOME").unwrap_or_default();
                    self.input = Some((Prompt::RestoreTo, format!("{home}/sdb-restore")));
                }
            }
            Act::Search => self.input = Some((Prompt::Filter, self.restore.filter.clone())),
            Act::Timeline => {
                self.restore.timeline = true;
                self.in_menu = false;
            }
            Act::Date => self.input = Some((Prompt::Date, String::new())),
            Act::Versions => {
                let Some(path) = self.restore.tree.cur().filter(|r| !r.dir).map(|r| r.path.clone()) else {
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
            Act::Source => {
                // Cycle: all destinations, then each one by itself.
                let names: Vec<String> = self.cfg.friends.iter().map(|f| f.name.clone()).collect();
                self.source = match &self.source {
                    None => names.first().cloned(),
                    Some(cur) => names.iter().position(|n| n == cur).and_then(|i| names.get(i + 1)).cloned(),
                };
                self.load_snaps();
            }
            Act::Older if self.restore.cur > 0 => self.goto(self.restore.cur - 1),
            Act::Newer if self.restore.cur + 1 < self.restore.snaps.len() => self.goto(self.restore.cur + 1),
            Act::Older | Act::Newer => {}
            Act::Refresh if self.page == RESTORE => self.load_snaps(),
            Act::Refresh => self.text_at = Instant::now() - Duration::from_secs(3600),
        }
    }

    /// Cursor movement and folder opening for whichever list is on screen.
    /// Returns false when the key was not a movement key.
    fn navigate(&mut self, code: KeyCode) -> bool {
        let d: isize = match code {
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::PageUp => -15,
            KeyCode::PageDown => 15,
            _ => 0,
        };
        let step = |st: &mut ListState, len: usize| {
            let i = st.selected().unwrap_or(0) as isize + d;
            st.select(Some(i.clamp(0, len.max(1) as isize - 1) as usize));
        };
        let tree_key = matches!(code, KeyCode::Right | KeyCode::Left | KeyCode::Enter | KeyCode::Char('l' | 'h'));
        match (self.page, &self.sub) {
            (BACKUP, Sub::Files) if d != 0 => self.folders.mv(d),
            (BACKUP, Sub::Files) if tree_key => {
                if code == KeyCode::Left || code == KeyCode::Char('h') {
                    self.folders.close();
                } else if let Some(r) = self.folders.cur().filter(|r| r.dir && !r.open) {
                    let kids = list_dir(&r.path, self.hidden);
                    self.folders.open(kids);
                }
            }
            (RESTORE, _) if self.restore.timeline && d != 0 => step(&mut self.restore.tl, self.restore.snaps.len()),
            (RESTORE, _) if !self.restore.timeline && d != 0 => self.restore.tree.mv(d),
            (RESTORE, _) if !self.restore.timeline && tree_key => {
                let rv = &mut self.restore;
                if code == KeyCode::Left || code == KeyCode::Char('h') {
                    rv.tree.close();
                } else if let Some(r) = rv.tree.cur().filter(|r| r.dir && !r.open) {
                    let kids = rv.kids.get(&r.path).cloned().unwrap_or_default();
                    rv.tree.open(kids);
                }
            }
            (BACKUP, Sub::Sets) if d != 0 => step(&mut self.sets, self.cfg.sets.len() + 1),
            (BACKUP, Sub::Dests) if d != 0 => step(&mut self.set_dests, self.cfg.friends.len()),
            (BACKUP, Sub::Rules) if d != 0 => step(&mut self.rules, RULES.len()),
            (DESTS, _) if d != 0 => step(&mut self.dests, self.cfg.friends.len()),
            (SETTINGS, _) if d != 0 => step(&mut self.settings, GENERAL.len()),
            (HISTORY | STATUS, _) if d != 0 => {
                self.scroll = (self.scroll as isize - d).clamp(0, self.text.len() as isize) as usize
            }
            _ => return false,
        }
        true
    }

    /// Handles one key; returns false to quit.
    async fn key(&mut self, code: KeyCode) -> bool {
        if self.popup.take().is_some() {
            return true;
        }
        if let Some((list, mut st)) = self.versions.take() {
            let i = st.selected().unwrap_or(0);
            match code {
                KeyCode::Up | KeyCode::Char('k') => st.select(Some(i.saturating_sub(1))),
                KeyCode::Down | KeyCode::Char('j') => st.select(Some((i + 1).min(list.len().saturating_sub(1)))),
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
            let folder_filtering = matches!(prompt, Prompt::FolderFilter);
            let filtering = folder_filtering || matches!(prompt, Prompt::Filter);
            // Moving in the list ends typing and puts the cursor among the results.
            if filtering && matches!(code, KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown) {
                self.in_menu = false;
                self.restore.timeline = false;
                self.navigate(code);
                return true;
            }
            match code {
                KeyCode::Esc if filtering => text.clear(),
                KeyCode::Esc => {}
                KeyCode::Enter if filtering => {
                    self.in_menu = false;
                    self.restore.timeline = false;
                }
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
            if folder_filtering && self.folder_filter != text {
                self.search_folders(text);
            } else if filtering && !folder_filtering && self.restore.filter != text {
                self.restore.filter = text;
                self.restore.apply_filter();
            }
            self.sync();
            return true;
        }
        if code == KeyCode::Char('q') {
            return false;
        }
        if self.in_menu {
            let (kind, sel) = self.stack.last().cloned().expect("the root level is never removed");
            let items = self.items(&kind);
            let last = items.len().saturating_sub(1);
            let top = self.stack.len() - 1;
            match code {
                KeyCode::Up | KeyCode::Char('k') => self.stack[top].1 = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => self.stack[top].1 = (sel + 1).min(last),
                KeyCode::PageUp => self.stack[top].1 = sel.saturating_sub(10),
                KeyCode::PageDown => self.stack[top].1 = (sel + 10).min(last),
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match items.get(sel).map(|i| i.1.clone()) {
                    Some(Choice::Open(k)) => self.stack.push((k, 0)),
                    Some(Choice::Focus) => {
                        self.restore.timeline = false;
                        self.in_menu = false;
                    }
                    // Right only moves; doing something takes Enter.
                    Some(Choice::Do(a)) if code == KeyCode::Enter => self.act(a).await,
                    Some(Choice::Value(key)) if code == KeyCode::Enter => {
                        self.input = Some((Prompt::Setting(key), String::new()))
                    }
                    _ => {}
                },
                KeyCode::Esc | KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') if self.stack.len() > 1 => {
                    self.stack.pop();
                }
                _ => {}
            }
            self.sync();
            return true;
        }
        // The cursor is in the pane on the right.
        let top_of_tree = self.folders.cur().is_some_and(|r| r.depth == 0 && !r.open);
        match (code, self.page, &self.sub) {
            (KeyCode::Char('/'), BACKUP, Sub::Files) => {
                self.input = Some((Prompt::FolderFilter, self.folder_filter.clone()))
            }
            (KeyCode::Char('/'), RESTORE, _) if !self.restore.timeline => self.act(Act::Search).await,
            (KeyCode::Esc, BACKUP, Sub::Files) if !self.folder_filter.is_empty() => self.search_folders(String::new()),
            (KeyCode::Esc, RESTORE, _) if !self.restore.timeline && !self.restore.filter.is_empty() => {
                self.restore.filter.clear();
                self.restore.apply_filter();
            }
            (KeyCode::Esc | KeyCode::Tab, _, _) => {
                self.restore.timeline = false;
                self.in_menu = true;
            }
            (KeyCode::Char(' '), BACKUP, Sub::Files | Sub::Dests) => self.act(Act::Toggle).await,
            (KeyCode::Char(' '), RESTORE, _) if !self.restore.timeline => self.act(Act::Pick).await,
            (KeyCode::Enter, RESTORE, _) if self.restore.timeline => self.act(Act::Pick).await,
            (KeyCode::Left, BACKUP, Sub::Files) if top_of_tree => self.act(Act::UpFolder).await,
            _ => {
                self.navigate(code);
            }
        }
        self.sync();
        true
    }

    async fn submit(&mut self, prompt: Prompt, text: String) {
        // Blank is an answer for a pause, and for the optional folder inside a bucket.
        let optional =
            matches!(&prompt, Prompt::Setting("pause")) || matches!(&prompt, Prompt::Cloud(v) if v.len() == 4);
        if text.is_empty() && !optional {
            return;
        }
        match prompt {
            Prompt::Filter | Prompt::FolderFilter => {}
            Prompt::NewSet => {
                let name: String =
                    text.chars().filter(|c| c.is_alphanumeric() || "-_.".contains(*c)).take(32).collect();
                if name.is_empty() {
                    self.status = "use letters and digits for a set name".into();
                } else {
                    self.set = name.clone();
                    self.apply(self.set_edit(None)).await;
                    let idx = self.cfg.sets.iter().position(|b| b.name == name).unwrap_or(0);
                    self.stack = vec![(Kind::Root, BACKUP), (Kind::Backup, idx), (Kind::Set(name), SET_FILES)];
                    self.in_menu = false;
                    self.status = format!("set \"{}\" created: mark what it backs up with space, then esc", self.set);
                }
            }
            Prompt::RenameSet(name) => {
                let to: String = text.chars().filter(|c| c.is_alphanumeric() || "-_.".contains(*c)).take(32).collect();
                if to.is_empty() {
                    self.status = "use letters and digits for a set name".into();
                } else {
                    match self.exec.call(Cmd::SetRename { name: name.clone(), to: to.clone() }).await {
                        Ok(_) => {
                            // Follow the set to its new name so the menu stays where it was.
                            for level in self.stack.iter_mut() {
                                match &mut level.0 {
                                    Kind::Set(n) | Kind::Rules(n) | Kind::RestoreSet(n) if *n == name => {
                                        *n = to.clone()
                                    }
                                    _ => {}
                                }
                            }
                            if self.rset.as_deref() == Some(name.as_str()) {
                                self.rset = Some(to.clone());
                            }
                            self.set = to;
                            self.restore.loaded = false;
                            self.status = "renamed".into();
                        }
                        Err(e) => self.status = format!("{e:#}"),
                    }
                    self.reload().await;
                }
            }
            Prompt::DeleteSet(name) => {
                if text.eq_ignore_ascii_case("yes") {
                    self.apply(Cmd::SetEdit {
                        name,
                        to: None,
                        every: None,
                        hours: None,
                        days: None,
                        versions: None,
                        last: None,
                        delete: true,
                    })
                    .await;
                    self.sets.select(Some(0));
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
            Prompt::Cloud(mut got) => {
                got.push(text);
                if got.len() < CLOUD_STEPS.len() {
                    // The usual region is offered; the rest start empty.
                    let start = if got.len() == 3 { "us-east-1" } else { "" };
                    self.input = Some((Prompt::Cloud(got), start.into()));
                    return;
                }
                let mut it = got.into_iter();
                let mut next = || it.next().unwrap_or_default();
                let cmd = Cmd::CloudAdd {
                    name: next(),
                    endpoint: next(),
                    bucket: next(),
                    region: Some(next()),
                    prefix: Some(next()),
                    access_key: next(),
                    secret_key: next(),
                    path_style: None,
                    create: false,
                };
                self.start("checking the cloud storage", |ex, _| async move {
                    Done::Popup(lines_or_error(ex.call(cmd).await))
                });
            }
            Prompt::ExNew(from) => self.apply(Cmd::ExcludeNew { name: text, from: Some(from) }).await,
            Prompt::ExAdd(list) => self.apply(Cmd::ExcludePattern { list, pattern: text, remove: false }).await,
            Prompt::ExDelete(name) => {
                if text.eq_ignore_ascii_case("yes") {
                    self.apply(Cmd::ExcludeDelete { name }).await;
                }
            }
            Prompt::DestKeep(name, key) => match text.parse::<u32>() {
                Ok(n) => self.apply(Cmd::DestKeep { name, set: vec![(key.to_string(), n)], clear: false }).await,
                Err(_) => self.status = "type a number (0 = unlimited)".into(),
            },
            Prompt::Every(name) => match text.parse::<u32>() {
                Ok(minutes) => self.apply(Cmd::Every { name: Some(name), minutes }).await,
                Err(_) => self.status = "type a number of minutes".into(),
            },
            Prompt::InLimit(name) => match text.parse::<u64>() {
                Ok(kbps) => self.apply(Cmd::FriendLimit { name, kbps }).await,
                Err(_) => self.status = "type a number of KB/s (0 = unlimited)".into(),
            },
            Prompt::Quota(name) => match text.parse::<u64>() {
                Ok(gb) => self.apply(Cmd::FriendQuota { name, gb }).await,
                Err(_) => self.status = "type a number of GB".into(),
            },
            Prompt::Remove(name) => {
                if text.eq_ignore_ascii_case("yes") {
                    self.apply(Cmd::FriendRemove { name, purge: false }).await;
                    self.dests.select(Some(0));
                }
            }
            Prompt::Setting(key) => {
                if !self.set_setting(key, &text).await {
                    self.status = "that value does not fit this setting".into();
                }
            }
            Prompt::Date => {
                let found = parse_when(&text).and_then(|t| self.restore.snaps.iter().rposition(|s| s.time <= t));
                match found {
                    Some(i) => self.goto(i),
                    None => {
                        self.status = "no restore point at or before that (try 2026-11-20 or 2026-11-20 14:30)".into()
                    }
                }
            }
            Prompt::Join => {
                let ticket = text.trim_start_matches("sdb join ").to_string();
                self.start("pairing", |ex, _| async move {
                    Done::Paired(lines_or_error(ex.call(Cmd::Join { ticket }).await))
                });
            }
            Prompt::RestoreTo => {
                let Some(snap) = self.restore.snaps.get(self.restore.cur).map(|s| s.id.clone()) else {
                    return;
                };
                let paths = self.restore.picked.iter().cloned().collect();
                let cmd = Cmd::Restore { snap, paths, to: PathBuf::from(text), from: self.source.clone() };
                self.start("restoring", |ex, pr| async move { Done::Popup(lines_or_error(ex.live(cmd, &pr).await)) });
            }
        }
    }

    // ---------------------------------------------------------------- drawing

    fn dest_state(&self, name: &str) -> (String, Color) {
        let d = self.ov["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == name));
        match d.and_then(|d| d["state"].as_str()) {
            Some("online") | Some("connected") => ("●".into(), Color::Green),
            Some("checking") => ("◌".into(), Color::Yellow),
            _ => ("○".into(), Color::Red),
        }
    }

    fn draw_sets(&mut self, f: &mut Frame, area: Rect, focus: Style, hl: Style) {
        let warnings: Vec<String> = self.ov["warnings"]
            .as_array()
            .map(|a| a.iter().filter_map(|w| w.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let mut top: Vec<Line> =
            warnings.iter().map(|w| Line::styled(format!("! {w}"), Style::new().fg(Color::Red))).collect();
        if let Some(h) = self.ov["held"].as_str() {
            top.push(Line::styled(format!("Automatic backups are held: {h}"), Style::new().fg(Color::Yellow)));
        }
        let [warn, body] = Layout::vertical([Constraint::Length(top.len() as u16), Constraint::Min(3)]).areas(area);
        f.render_widget(Paragraph::new(top), warn);

        let dim = Style::new().fg(Color::DarkGray);
        let warn_secs = self.cfg.warn_after_days.max(1) as i64 * 86400;
        let now = chrono::Utc::now().timestamp();
        let mut items: Vec<ListItem> = vec![];
        for b in &self.cfg.sets {
            let o = self.ov["sets"].as_array().and_then(|a| a.iter().find(|s| s["name"] == b.name.as_str()));
            let size = match o.and_then(|o| o["files"].as_u64()) {
                Some(n) => format!("{n} files, {}", human(o.and_then(|o| o["bytes"].as_u64()).unwrap_or(0))),
                None => "not backed up yet".into(),
            };
            let running = o.and_then(|o| o["running"].as_str());
            let mut head = vec![
                Span::styled(b.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(format!("   {size}")),
            ];
            match running {
                Some(r) => head.push(Span::styled(format!("   backing up: {r}"), Style::new().fg(Color::Cyan))),
                None => {
                    head.push(Span::styled(format!("   last backup {}", ago(o.and_then(|o| o["last"].as_i64()))), dim))
                }
            }
            let files = if b.paths.is_empty() {
                "nothing chosen yet".to_string()
            } else {
                b.paths.iter().map(|p| tilde(&p.display().to_string())).collect::<Vec<_>>().join(", ")
            };
            let mut dests = vec![Span::styled("  Destinations  ", dim)];
            let list: Vec<_> = self.cfg.friends.iter().filter(|x| b.sends_to(x)).collect();
            if list.is_empty() {
                dests.push(Span::raw("none yet"));
            }
            for x in list {
                let d =
                    o.and_then(|o| o["dests"].as_array()).and_then(|a| a.iter().find(|d| d["name"] == x.name.as_str()));
                let last = d.and_then(|d| d["last"].as_i64());
                // When the daemon will next send to this destination, if anything is waiting.
                let next = match d {
                    Some(d) if d["held"].as_bool() == Some(true) => ", next: held".to_string(),
                    Some(d) => match d["next"].as_i64() {
                        Some(t) => format!(", next {}", until(t)),
                        None if last.is_some() => ", up to date".to_string(),
                        None => String::new(),
                    },
                    None => String::new(),
                };
                let (dot, color) = self.dest_state(&x.name);
                let stale = last.is_none_or(|t| now - t > warn_secs);
                dests.push(Span::styled(format!("{dot} "), Style::new().fg(color)));
                dests.push(Span::raw(x.name.clone()));
                dests.push(Span::styled(
                    format!(" {}{next}    ", ago(last)),
                    if stale && !b.paths.is_empty() { Style::new().fg(Color::Yellow) } else { dim },
                ));
            }
            let pace = if b.every_minutes > 0 {
                format!("every {} min", b.every_minutes)
            } else {
                "each destination's own pace".into()
            };
            let hours = if b.hours.is_empty() { "general hours".to_string() } else { format!("hours {}", b.hours) };
            let keep = match &b.retention {
                Some(r) if r.max_versions > 0 || r.keep_days > 0 => {
                    format!("own history rules (versions {}, days {})", r.max_versions, r.keep_days)
                }
                _ => "general history rules".into(),
            };
            items.push(ListItem::new(Text::from(vec![
                Line::from(head),
                Line::from(vec![Span::styled("  Files         ", dim), Span::raw(files)]),
                Line::from(dests),
                Line::from(vec![
                    Span::styled("  Schedule      ", dim),
                    Span::raw(format!("{pace} · {hours} · {keep}")),
                ]),
            ])));
        }
        items.push(ListItem::new(Line::styled("+ New backup set", Style::new().fg(Color::Green))));
        if self.sets.selected().is_none() {
            self.sets.select(Some(0));
        }
        let block = Block::bordered().title(" Backup sets ").border_style(focus);
        f.render_stateful_widget(Self::cards(items, block, hl != Style::new()), body, &mut self.sets);
    }

    fn draw_files(&mut self, f: &mut Frame, area: Rect, focus: Style, hl: Style) {
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
                let other = self.other_set(&r.path).map(|o| format!("   (in set {o})")).unwrap_or_default();
                // Search results lead with the name and say where it is.
                let label = if !self.folder_filter.is_empty() && r.depth == 0 {
                    let dir = Path::new(&r.path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                    format!("{}{}   in {}", name(&r.path), if r.dir { "/" } else { "" }, tilde(&dir))
                } else {
                    format!("{}{}", name(&r.path), if r.dir { "/" } else { "" })
                };
                ListItem::new(Line::from(vec![
                    Span::styled(mark, Style::new().fg(color)),
                    Span::raw(format!(" {}{arrow} {label}", "  ".repeat(r.depth))),
                    Span::styled(other, Style::new().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let mut title = format!(" Backup > {} > Files: {} ", self.set, tilde(&self.root));
        if !self.folder_filter.is_empty() {
            title += &format!("| {} matching \"{}\" ", self.folders.rows.len(), self.folder_filter);
        }
        let block = Block::bordered().title(title).border_style(focus);
        f.render_stateful_widget(List::new(items).block(block).highlight_style(hl), area, &mut self.folders.st);
    }

    fn draw_set_dests(&mut self, f: &mut Frame, area: Rect, focus: Style, hl: Style) {
        let b = self.cur_set();
        let items: Vec<ListItem> = self
            .cfg
            .friends
            .iter()
            .map(|x| {
                let mark =
                    if b.sends_to(x) { Span::styled("[x]", Style::new().fg(Color::Green)) } else { Span::raw("[ ]") };
                let kind = format!("{:<6}", x.kind());
                let (dot, color) = self.dest_state(&x.name);
                ListItem::new(Line::from(vec![
                    mark,
                    Span::styled(format!(" {dot} "), Style::new().fg(color)),
                    Span::raw(format!("{:<20} {kind}   every {} min", x.name, b.interval_secs(x, &self.cfg) / 60)),
                ]))
            })
            .collect();
        let title = format!(" Backup > {} > Destinations ", self.set);
        let block = Block::bordered().title(title).border_style(focus);
        if items.is_empty() {
            let text = "No destinations yet. Go to Destinations in the menu to invite a friend or add a drive.";
            f.render_widget(Paragraph::new(text).block(block), area);
        } else {
            if self.set_dests.selected().is_none() {
                self.set_dests.select(Some(0));
            }
            f.render_stateful_widget(List::new(items).block(block).highlight_style(hl), area, &mut self.set_dests);
        }
    }

    fn draw_values(&mut self, f: &mut Frame, area: Rect, focus: Style, hl: Style) {
        let (rows, title): (&[(&str, &str)], String) = if self.page == SETTINGS {
            (&GENERAL, " Defaults: backup sets can override ".into())
        } else {
            (&RULES, format!(" Backup > {} > Schedule and history rules ", self.set))
        };
        let items: Vec<ListItem> =
            rows.iter().map(|(k, label)| ListItem::new(format!("{:>18}   {label}", self.setting_text(k)))).collect();
        let st = if self.page == SETTINGS { &mut self.settings } else { &mut self.rules };
        if st.selected().is_none() {
            st.select(Some(0));
        }
        let block = Block::bordered().title(title).border_style(focus);
        f.render_stateful_widget(List::new(items).block(block).highlight_style(hl), area, st);
    }

    /// The exclusion lists: one in full when it is chosen, otherwise all of them in short.
    fn draw_excludes(&mut self, f: &mut Frame, area: Rect, focus: Style) {
        let dim = Style::new().fg(Color::DarkGray);
        let users = |name: &str| {
            let v: Vec<&str> =
                self.cfg.sets.iter().filter(|b| self.cfg.list_for(b).name == name).map(|b| b.name.as_str()).collect();
            if v.is_empty() { "not used by any set".to_string() } else { format!("used by {}", v.join(", ")) }
        };
        let kind = |name: &str| if is_builtin_list(name) { "built in" } else { "yours" };
        let mut lines: Vec<Line> = vec![];
        let chosen = self.exlist.as_ref().and_then(|n| self.cfg.lists().into_iter().find(|l| &l.name == n));
        match chosen {
            Some(l) => {
                lines.push(Line::from(vec![
                    Span::styled(l.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                    Span::raw(format!("   {}, {}", kind(&l.name), users(&l.name))),
                ]));
                lines.push(Line::styled(
                    if is_builtin_list(&l.name) {
                        "Comes with sdb and may gain patterns in a later release. Copy it to make changes."
                    } else {
                        "A bare name such as build means that name anywhere; a path starting with / means that one place."
                    },
                    dim,
                ));
                lines.push(Line::raw(""));
                lines.extend(l.patterns.iter().map(|p| Line::raw(format!("  {p}"))));
                if l.patterns.is_empty() {
                    lines.push(Line::raw("  (nothing: everything in the set's folders is backed up)"));
                }
            }
            None => {
                lines.push(Line::raw("Each backup set leaves out what one of these lists names."));
                lines.push(Line::styled(
                    "Choose a set's list under Backup > the set > Leaves out. Leaving a pattern out does not remove it from restore points made so far.",
                    dim,
                ));
                for l in self.cfg.lists() {
                    lines.push(Line::raw(""));
                    lines.push(Line::from(vec![
                        Span::styled(l.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                        Span::raw(format!("   {}, {} patterns, {}", kind(&l.name), l.patterns.len(), users(&l.name))),
                    ]));
                    let short: Vec<String> =
                        l.patterns.iter().map(|p| p.trim_start_matches("**/").to_string()).collect();
                    lines.push(Line::styled(format!("  {}", short.join("  ")), dim));
                }
            }
        }
        let block = Block::bordered().title(" Settings > Exclusion lists ").border_style(focus);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
    }

    fn draw_dests(&mut self, f: &mut Frame, area: Rect, focus: Style, hl: Style) {
        let dim = Style::new().fg(Color::DarkGray);
        let mut items: Vec<ListItem> = vec![];
        for x in &self.cfg.friends {
            let o = self.ov["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == x.name.as_str()));
            let get = |k: &str| o.map(|o| o[k].clone()).unwrap_or(Value::Null);
            let (dot, color) = self.dest_state(&x.name);
            let state = get("state").as_str().unwrap_or("checking").to_string();
            let link = get("link").as_str().map(|l| format!(", {l}")).unwrap_or_default();
            let link = match get("version").as_str() {
                Some(v) => format!("{link}, version {v}"),
                None if x.is_friend() && get("state") == "online" => format!("{link}, older version"),
                None => link,
            };
            let there = match (get("used_there").as_u64(), get("quota_there").as_u64()) {
                (Some(u), Some(q)) => format!("you use {} of {} there", human(u), human(q)),
                (Some(u), None) => format!("you use {} there", human(u)),
                _ => String::new(),
            };
            let off = if x.backup_to { "" } else { "   NOT IN USE" };
            let head = Line::from(vec![
                Span::styled(format!("{dot} "), Style::new().fg(color)),
                Span::styled(format!("{:<18}", x.name), Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(format!("{:<6}   {state}{link}", x.kind())),
                Span::styled(off, Style::new().fg(Color::Yellow)),
            ]);
            let second = match (&x.path, &x.cloud) {
                (Some(p), _) => format!("    {}   {there}", p.display()),
                (_, Some(c)) => format!(
                    "    bucket {} at {}   {there}{}",
                    c.bucket,
                    c.endpoint,
                    if x.quota_gb > 0 { "" } else { " (no cap)" }
                ),
                _ => format!(
                    "    {there}   they use {} of {} GB here{}, last backup from them {}",
                    human(get("used_here").as_u64().unwrap_or(0)),
                    x.quota_gb,
                    if x.max_in_kbps > 0 { format!(" at up to {} KB/s", x.max_in_kbps) } else { String::new() },
                    ago(get("last_in").as_i64())
                ),
            };
            let keeps = match &x.retention {
                Some(r) if r.keep_days > 0 => format!(", keeps {} days of history", r.keep_days),
                Some(r) => format!(", keeps its own history (last {})", r.keep_last),
                None => String::new(),
            };
            let third = format!(
                "    automatic backups every {} min, last sent {}{keeps}",
                x.interval_secs(&self.cfg) / 60,
                ago(get("last_out").as_i64())
            );
            items.push(ListItem::new(Text::from(vec![head, Line::raw(second), Line::styled(third, dim)])));
        }
        let block = Block::bordered()
            .title(" Destinations: friends' computers, local drives and cloud storage ")
            .border_style(focus);
        if items.is_empty() {
            let text = "No destinations yet.\n\n  i  invite a friend (you send them a one-time code)\n  a  accept an invite a friend sent you\n  d  add a local drive or folder\n\nCloud storage (S3, B2 and the like) is added from the menu on the left.";
            f.render_widget(Paragraph::new(text).block(block), area);
        } else {
            if self.dests.selected().is_none() {
                self.dests.select(Some(0));
            }
            f.render_stateful_widget(Self::cards(items, block, hl != Style::new()), area, &mut self.dests);
        }
    }

    /// The backup sets to restore from: the first thing Restore shows.
    fn draw_restore_sets(&mut self, f: &mut Frame, area: Rect, focus: Style, hl: Style) {
        let dim = Style::new().fg(Color::DarkGray);
        let mut items: Vec<ListItem> = vec![];
        for b in &self.cfg.sets {
            let o = self.ov["sets"].as_array().and_then(|a| a.iter().find(|s| s["name"] == b.name.as_str()));
            let size = match o.and_then(|o| o["files"].as_u64()) {
                Some(n) => format!("{n} files, {}", human(o.and_then(|o| o["bytes"].as_u64()).unwrap_or(0))),
                None => "not backed up yet".into(),
            };
            let head = Line::from(vec![
                Span::styled(format!("{:<18}", b.name), Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(size),
                Span::styled(format!("   last backup {}", ago(o.and_then(|o| o["last"].as_i64()))), dim),
            ]);
            let files = if b.paths.is_empty() {
                "no folders chosen".to_string()
            } else {
                b.paths.iter().map(|p| tilde(&p.to_string_lossy())).collect::<Vec<_>>().join(", ")
            };
            items.push(ListItem::new(Text::from(vec![head, Line::styled(format!("  {files}"), dim)])));
        }
        let block = Block::bordered().title(" Restore: choose a backup set ").border_style(focus);
        f.render_stateful_widget(Self::cards(items, block, hl != Style::new()), area, &mut self.rsets);
    }

    fn draw_restore(&mut self, f: &mut Frame, area: Rect, focus: Style, hl: Style) {
        let sets = self.cfg.sets.len();
        let rv = &mut self.restore;
        let mut title = match rv.snaps.get(rv.cur) {
            Some(s) => format!(
                " Files as of {}{} ({} files, {}){} ",
                when(s.time),
                s.tag.as_ref().map(|t| format!(" \"{t}\"")).unwrap_or_default(),
                s.files,
                human(s.bytes),
                if rv.at_latest() { ", latest" } else { "" }
            ),
            None => " Restore ".into(),
        };
        title += &match &self.source {
            Some(n) => format!("| {n} only "),
            None => "| all destinations ".into(),
        };
        if sets > 1 {
            title += &format!("| set {} ", self.rset.as_deref().unwrap_or("?"));
        }
        if !rv.filter.is_empty() {
            title += &format!("| {} matching \"{}\" ", rv.tree.rows.len(), rv.filter);
        }
        if rv.snaps.is_empty() {
            let text = if self.busy.is_some() {
                "Looking up restore points..."
            } else {
                "No restore points found yet (or no destination is reachable). Press u to look again."
            };
            f.render_widget(Paragraph::new(text).block(Block::bordered().title(title).border_style(focus)), area);
            return;
        }
        let [left, right] = Layout::horizontal([Constraint::Min(30), Constraint::Length(44)]).areas(area);
        let items: Vec<ListItem> = rv
            .tree
            .rows
            .iter()
            .map(|r| {
                let mark = if rv.picked.contains(&r.path) {
                    Span::styled("[x]", Style::new().fg(Color::Green))
                } else if rv.picked.iter().any(|p| Path::new(p).starts_with(&r.path)) {
                    Span::styled("[o]", Style::new().fg(Color::Cyan))
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
                let found = !rv.filter.is_empty() && r.depth == 0;
                let label = if r.depth > 0 {
                    name(&r.path)
                } else if found {
                    // Search results lead with the name so it is never cut off.
                    let dir = Path::new(&r.path).parent().map(|p| p.to_string_lossy().into_owned());
                    format!("{}   in {}", name(&r.path), tilde(&dir.unwrap_or_default()))
                } else {
                    tilde(&r.path)
                };
                let size = if r.dir || found { String::new() } else { format!("  {}", human(r.size)) };
                let slash = if r.dir && !found { "/" } else { "" };
                let mut spans = vec![mark, Span::raw(format!(" {}{arrow} {label}{slash}{size}", "  ".repeat(r.depth)))];
                if let Some((text, color)) = rv.marker(r) {
                    spans.push(Span::styled(format!("  {text}"), Style::new().fg(color)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let none = Style::new();
        let block = Block::bordered().title(title).border_style(if rv.timeline { none } else { focus });
        f.render_stateful_widget(List::new(items).block(block).highlight_style(hl), left, &mut rv.tree.st);
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
            .title(format!(" Timeline ({}), stored on ", rv.snaps.len()))
            .border_style(if rv.timeline { focus } else { none });
        let mut list = List::new(times).block(block);
        if rv.timeline {
            list = list.highlight_style(hl);
        }
        f.render_stateful_widget(list, right, &mut rv.tl);
    }

    fn draw_text(&mut self, f: &mut Frame, area: Rect, focus: Style) {
        let title = if self.page == HISTORY { " History: what the daemon has done, newest last " } else { " Status " };
        let inner = area.height.saturating_sub(2) as usize;
        let lines: Vec<Line> = if self.page == HISTORY {
            // Newest at the bottom; scrolling up goes back in time.
            let end = self.text.len().saturating_sub(self.scroll.min(self.text.len().saturating_sub(inner)));
            self.text[end.saturating_sub(inner)..end].iter().map(|l| Line::raw(l.clone())).collect()
        } else {
            self.text
                .iter()
                .map(|l| {
                    if l.starts_with("WARNING") {
                        Line::styled(l.clone(), Style::new().fg(Color::Red))
                    } else {
                        Line::raw(l.clone())
                    }
                })
                .collect()
        };
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::bordered().title(title).border_style(focus)),
            area,
        );
    }

    fn draw(&mut self, f: &mut Frame) {
        let [main, help] = Layout::vertical([Constraint::Min(5), Constraint::Length(4)]).areas(f.area());
        let [side, body] = Layout::horizontal([Constraint::Length(36), Constraint::Min(40)]).areas(main);
        let on = Style::new().fg(Color::Yellow);
        let (menu_focus, page_focus) = if self.in_menu { (on, Style::new()) } else { (Style::new(), on) };
        let hl = Style::new().add_modifier(Modifier::REVERSED);

        // The menu: the level we are in, with the path to it as the title.
        let (kind, sel) = self.stack.last().cloned().expect("the root level is never removed");
        let warn = self.ov["warnings"].as_array().is_some_and(|a| !a.is_empty());
        let rows: Vec<ListItem> = self
            .items(&kind)
            .into_iter()
            .map(|(label, choice)| {
                let more = if matches!(choice, Choice::Open(_)) { " >" } else { "" };
                let flag = if kind == Kind::Root && label == "Backup" && warn { " !" } else { "" };
                ListItem::new(format!(" {label}{flag}{more}"))
            })
            .collect();
        let crumbs: Vec<String> = self
            .stack
            .iter()
            .skip(1)
            .map(|(k, _)| match k {
                Kind::Root => String::new(),
                Kind::Backup => "Backup".into(),
                Kind::Set(n) | Kind::Dest(n) => n.clone(),
                Kind::Rules(_) => "Schedule".into(),
                Kind::Restore => "Restore".into(),
                Kind::RestoreSet(n) => n.clone(),
                Kind::Dests => "Destinations".into(),
                Kind::Settings => "Settings".into(),
                Kind::Excludes => "Exclusions".into(),
                Kind::ExList(n) => n.clone(),
            })
            .collect();
        let title = if crumbs.is_empty() {
            format!(
                " {} · sdb {} ",
                self.ov["name"].as_str().unwrap_or(&self.cfg.name),
                self.ov["version"].as_str().unwrap_or(crate::net::VERSION)
            )
        } else {
            format!(" {} ", crumbs.join(" > "))
        };
        let mut st = ListState::default();
        st.select(Some(sel));
        let dim = Style::new().add_modifier(Modifier::BOLD);
        let menu = List::new(rows)
            .block(Block::bordered().title(title).border_style(menu_focus))
            .highlight_style(if self.in_menu { hl } else { dim });
        f.render_stateful_widget(menu, side, &mut st);

        // At the top level the right side is a preview, so nothing in it is highlighted.
        let row_hl = if kind == Kind::Root { Style::new() } else { hl };
        match (self.page, &self.sub) {
            (BACKUP, Sub::Sets) => self.draw_sets(f, body, page_focus, row_hl),
            (BACKUP, Sub::Files) => self.draw_files(f, body, page_focus, if self.in_menu { Style::new() } else { hl }),
            (BACKUP, Sub::Dests) => {
                self.draw_set_dests(f, body, page_focus, if self.in_menu { Style::new() } else { hl })
            }
            (SETTINGS, Sub::Excludes) => self.draw_excludes(f, body, page_focus),
            (BACKUP, Sub::Rules) | (SETTINGS, _) => self.draw_values(f, body, page_focus, row_hl),
            (RESTORE, Sub::RestoreSets) => self.draw_restore_sets(f, body, page_focus, row_hl),
            (RESTORE, _) => self.draw_restore(f, body, page_focus, if self.in_menu { Style::new() } else { hl }),
            (DESTS, _) => self.draw_dests(f, body, page_focus, row_hl),
            _ => self.draw_text(f, body, page_focus),
        }

        let keys = if self.in_menu {
            "up/down: move   enter: choose   esc: back   q: quit"
        } else {
            match (self.page, &self.sub) {
                (BACKUP, Sub::Files) => {
                    "space: back up or stop   /: search   [x] chosen  [+] inside a chosen folder  [o] has chosen items inside  [-] left out   right/left: open/close   esc: done"
                }
                (BACKUP, Sub::Dests) => "space: send this set here or stop   esc: done",
                (RESTORE, _) if self.restore.timeline => {
                    "up/down: move through time   enter: show files as they were then   esc: back to the menu"
                }
                (RESTORE, _) => {
                    "space: pick for restore   /: search   [x] picked  [o] has picked items inside   right/left: open/close a folder   esc: back to the menu"
                }
                _ => "up/down: scroll   esc: back to the menu",
            }
        };
        let title = match &self.busy {
            Some(b) => format!(
                " {} {b}... {} ",
                ["|", "/", "-", "\\"][self.spin % 4],
                self.progress.trim().trim_start_matches("...").trim()
            ),
            None => format!(" {} ", self.status),
        };
        let style = if self.busy.is_some() { on } else { Style::new() };
        f.render_widget(
            Paragraph::new(keys).wrap(Wrap { trim: true }).block(Block::bordered().title(title).border_style(style)),
            help,
        );

        if let Some((prompt, text)) = &self.input {
            let title = match prompt {
                Prompt::NewSet => " Name for the new backup set ".to_string(),
                Prompt::RenameSet(n) => format!(" New name for set \"{n}\" "),
                Prompt::DeleteSet(n) => format!(" Type yes to delete set \"{n}\" (its restore points stay) "),
                Prompt::Filter | Prompt::FolderFilter => {
                    " Show only names containing (arrows or Enter: go to the list, Esc: clear) ".into()
                }
                Prompt::Drive => " Folder on the drive to back up into ".into(),
                Prompt::Cloud(got) => format!(
                    " Cloud storage {}/{}: {} ",
                    got.len() + 1,
                    CLOUD_STEPS.len(),
                    CLOUD_STEPS[got.len().min(CLOUD_STEPS.len() - 1)]
                ),
                Prompt::ExNew(from) => format!(" Name for the new list (a copy of {from}) "),
                Prompt::ExAdd(l) => format!(" Pattern to leave out with {l}, such as *.iso or build or /Users/me/tmp "),
                Prompt::ExDelete(l) => format!(" Type yes to delete the list {l} (sets using it go back to Standard) "),
                Prompt::DestKeep(n, "days") => format!(" Days of history kept on {n} (0 = unlimited) "),
                Prompt::DestKeep(n, _) => format!(" Most recent restore points always kept on {n} "),
                Prompt::Every(n) => format!(" Minutes between automatic backups to {n} (0 = default) "),
                Prompt::Quota(n) if self.cfg.friends.iter().any(|f| &f.name == n && f.cloud.is_some()) => {
                    format!(" Most GB to keep on {n} (0 = no cap); the oldest restore points there go first ")
                }
                Prompt::Quota(n) => format!(" GB that {n} may store on this computer "),
                Prompt::InLimit(n) => format!(" Fastest {n} may send backups here, in KB/s (0 = unlimited) "),
                Prompt::Remove(n) => format!(" Type yes to remove {n} (data already stored is left in place) "),
                Prompt::Setting("hours") | Prompt::Setting("set-hours") => {
                    " Backup hours, such as 22-7, or 'always' ".into()
                }
                Prompt::Setting("pause") => " Pause for how many hours? (blank = until resumed, 0 = resume) ".into(),
                Prompt::Setting(_) => " New value ".into(),
                Prompt::Join => " Paste the invite, then Enter ".into(),
                Prompt::RestoreTo => " Restore into which folder? ".into(),
                Prompt::Date => " Go back to which date? e.g. 2026-11-20 or 2026-11-20 14:30 ".into(),
            };
            let area = if matches!(prompt, Prompt::Filter | Prompt::FolderFilter) {
                Rect::new(body.x + 2, body.y + body.height.saturating_sub(4), body.width.saturating_sub(4).min(78), 3)
            } else {
                centered(f.area(), 84, 5)
            };
            f.render_widget(Clear, area);
            // A secret key is not echoed.
            let shown = match prompt {
                Prompt::Cloud(got) if got.len() + 1 == CLOUD_STEPS.len() => "*".repeat(text.chars().count()),
                _ => text.clone(),
            };
            f.render_widget(
                Paragraph::new(format!("{shown}_")).wrap(Wrap { trim: false }).block(Block::bordered().title(title)),
                area,
            );
        }
        if let Some((list, st)) = &mut self.versions {
            let area = centered(f.area(), 84, (list.len() as u16 + 2).clamp(4, 20));
            f.render_widget(Clear, area);
            let items: Vec<ListItem> = list.iter().map(|(_, l)| ListItem::new(l.as_str())).collect();
            let block = Block::bordered().title(" Versions: enter = go to that time, esc = close ");
            f.render_stateful_widget(List::new(items).block(block).highlight_style(hl), area, st);
        }
        if let Some(lines) = &self.popup {
            let area = centered(f.area(), 96, (lines.len() as u16 + 4).clamp(6, 20));
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
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

async fn event_loop(
    app: &mut App,
    term: &mut DefaultTerminal,
    mut done_rx: mpsc::UnboundedReceiver<Done>,
    mut progress_rx: mpsc::UnboundedReceiver<String>,
    mut found_rx: mpsc::UnboundedReceiver<(u64, Vec<Item>)>,
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
    let (text_tx, mut text_rx) = mpsc::unbounded_channel::<(usize, Vec<String>)>();
    let (ov_tx, mut ov_rx) = mpsc::unbounded_channel::<Value>();
    // The home screen stays current by asking the daemon for its overview every few seconds.
    let ex = app.exec.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Ok(l) = ex.call(Cmd::Overview).await {
                if let Some(v) = l.first().and_then(|s| serde_json::from_str(s).ok()) {
                    if ov_tx.send(v).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut tick = tokio::time::interval(Duration::from_millis(150));
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
                app.sync();
                app.kick();
            }
            Some(line) = progress_rx.recv() => app.progress = line,
            Some(v) = ov_rx.recv() => app.ov = v,
            Some((search, items)) = found_rx.recv() => {
                // Only the newest search may fill the list.
                if search == app.folder_search && !app.folder_filter.is_empty() {
                    app.folders.reset(items);
                }
            }
            Some((page, lines)) = text_rx.recv() => {
                if page == app.page {
                    app.text = lines;
                }
            }
            _ = tick.tick() => {
                app.spin += 1;
                // History and Status keep themselves current while on screen.
                if (app.page == HISTORY || app.page == STATUS) && app.text_at.elapsed() >= Duration::from_secs(5) {
                    app.load_text(&text_tx);
                }
            }
        }
    }
}

pub async fn run(home: PathBuf) -> Result<()> {
    let (found_tx, found_rx) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = mpsc::unbounded_channel();
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let mut app = App {
        exec: Arc::new(Exec(home)),
        done_tx,
        progress_tx,
        busy: None,
        progress: String::new(),
        spin: 0,
        cfg: Config::new(String::new()),
        ov: Value::Null,
        stack: vec![(Kind::Root, 0)],
        exlist: None,
        rsets: ListState::default(),
        page: BACKUP,
        in_menu: true,
        sub: Sub::Sets,
        set: DEFAULT_SET.into(),
        sets: ListState::default(),
        root: String::new(),
        hidden: false,
        folders: Tree::default(),
        folder_filter: String::new(),
        folder_search: 0,
        found_tx,
        set_dests: ListState::default(),
        rules: ListState::default(),
        dests: ListState::default(),
        settings: ListState::default(),
        restore: RestoreView::default(),
        source: None,
        rset: None,
        text: vec![],
        text_at: Instant::now() - Duration::from_secs(3600),
        scroll: 0,
        input: None,
        popup: None,
        versions: None,
        status: String::new(),
    };
    app.reload().await;
    app.sync();
    app.set_root(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
    let mut term = ratatui::init();
    let res = event_loop(&mut app, &mut term, done_rx, progress_rx, found_rx).await;
    ratatui::restore();
    res
}
