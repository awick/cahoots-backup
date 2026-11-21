// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The graphical interface (`sdb gui`). Like the TUI it is only a client of the
//! daemon: every button sends a command over the local socket.

use crate::{
    config::{BackupSet, Config, Friend},
    control,
    node::Cmd,
    snapshot::{human, when},
    tui::{Item, Listing, ago, list_dir, name, parse_listing, tilde, until},
};
use eframe::egui::{self, Color32, RichText};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

const GREEN: Color32 = Color32::from_rgb(60, 170, 80);
const YELLOW: Color32 = Color32::from_rgb(200, 150, 30);
const RED: Color32 = Color32::from_rgb(210, 70, 70);
const BLUE: Color32 = Color32::from_rgb(70, 140, 210);

/// Sends one command to the daemon and waits for it to finish. Output lines go to
/// `line` as they arrive; progress lines (starting "...") are passed on but not kept.
fn call(home: &Path, cmd: &Cmd, mut line: impl FnMut(&str)) -> Result<Vec<String>, String> {
    let gone = || "the daemon is not running; quit and start `sdb gui` again".to_string();
    let path = control::own_socket(home).ok_or_else(gone)?;
    let mut s = UnixStream::connect(path).map_err(|_| gone())?;
    let text = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
    s.write_all(format!("{text}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut out = vec![];
    for l in BufReader::new(s).lines() {
        let l = l.map_err(|e| e.to_string())?;
        if let Some(end) = l.strip_prefix('\u{1}') {
            if end == "OK" {
                return Ok(out);
            }
            out.push(end.to_string());
            return Err(out.join("\n"));
        }
        line(l.trim().trim_start_matches("...").trim());
        if !l.trim_start().starts_with("...") {
            out.push(l);
        }
    }
    Err("the daemon closed the connection".into())
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Backup,
    Restore,
    Destinations,
    Settings,
    History,
    Status,
}

const PAGES: [(Page, &str); 6] = [
    (Page::Backup, "Backup"),
    (Page::Restore, "Restore"),
    (Page::Destinations, "Destinations"),
    (Page::Settings, "Settings"),
    (Page::History, "History"),
    (Page::Status, "Status"),
];

/// What a finished background command was for.
#[derive(Clone)]
enum Job {
    /// A setting was saved; nothing to show but errors.
    Quiet,
    /// Show the output in a window with this title.
    Show(&'static str),
    Snaps,
    Tree(usize),
    Text(Page),
}

enum Msg {
    Overview(Value),
    Config(Box<Config>),
    Progress(String),
    Done(Job, Result<Vec<String>, String>),
    Found(u64, Vec<Item>),
}

/// Which part of a backup set is being edited in the side window.
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Files,
    Destinations,
    Schedule,
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

struct Gui {
    home: PathBuf,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    cfg: Config,
    ov: Value,
    page: Page,
    last_poll: Instant,
    busy: Option<String>,
    progress: String,
    status: String,
    popup: Option<(String, Vec<String>)>,
    /// Text being typed into fields, by field name.
    fields: HashMap<String, String>,

    // Backup page: the set being edited and the folder picker.
    edit: Option<(String, Tab)>,
    root: String,
    hidden: bool,
    open: HashSet<String>,
    dirs: HashMap<String, Vec<Item>>,
    found: Option<Vec<Item>>,
    search: u64,

    // Restore page.
    snaps: Vec<Snap>,
    snaps_loaded: bool,
    cur: usize,
    kids: HashMap<String, Vec<Item>>,
    roots: Vec<Item>,
    all: Vec<Item>,
    sigs: HashMap<String, String>,
    latest: HashMap<String, String>,
    ropen: HashSet<String>,
    picked: BTreeSet<String>,
    source: Option<String>,
    rset: Option<String>,

    text: Vec<String>,
    text_at: Instant,
    frames: u32,
}

impl Gui {
    fn new(home: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let long_ago = Instant::now() - Duration::from_secs(3600);
        let root = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let root = std::fs::canonicalize(&root).map(|p| p.to_string_lossy().into_owned()).unwrap_or(root);
        Gui {
            home,
            tx,
            rx,
            cfg: Config::new(String::new()),
            ov: Value::Null,
            page: Page::Backup,
            last_poll: long_ago,
            busy: None,
            progress: String::new(),
            status: String::new(),
            popup: None,
            fields: HashMap::new(),
            edit: None,
            root,
            hidden: false,
            open: HashSet::new(),
            dirs: HashMap::new(),
            found: None,
            search: 0,
            snaps: vec![],
            snaps_loaded: false,
            cur: 0,
            kids: HashMap::new(),
            roots: vec![],
            all: vec![],
            sigs: HashMap::new(),
            latest: HashMap::new(),
            ropen: HashSet::new(),
            picked: BTreeSet::new(),
            source: None,
            rset: None,
            text: vec![],
            text_at: long_ago,
            frames: 0,
        }
    }

    /// Fills the window before it first appears, so it never opens empty.
    fn first_load(&mut self) {
        let first = |cmd: Cmd| call(&self.home, &cmd, |_| {}).ok().and_then(|l| l.into_iter().next());
        if let Some(v) = first(Cmd::Overview).and_then(|l| serde_json::from_str(&l).ok()) {
            self.ov = v;
        }
        if let Some(c) = first(Cmd::Config).and_then(|l| serde_json::from_str(&l).ok()) {
            self.cfg = c;
        }
        // For development: SDB_GUI_SHOW=restore|destinations|settings|history|status|files|schedule
        // opens on that page with its data already loaded, so a screenshot shows it.
        let show = std::env::var("SDB_GUI_SHOW").unwrap_or_default();
        let first_set = self.cfg.sets.first().map(|b| b.name.clone()).unwrap_or_default();
        match show.as_str() {
            "restore" | "restore-old" => {
                self.page = Page::Restore;
                self.snaps_loaded = true;
                let lines = call(&self.home, &Cmd::SnapList { from: None }, |_| {}).unwrap_or_default();
                if self.take_snaps(lines) {
                    let tree = |me: &Self, i: usize| {
                        let cmd = Cmd::Tree { snap: me.snaps[i].id.clone(), from: None };
                        parse_listing(call(&me.home, &cmd, |_| {}).unwrap_or_default())
                    };
                    let items = tree(self, self.cur);
                    self.show_tree(items);
                    if show == "restore-old" && self.cur > 0 {
                        self.cur -= 1;
                        let items = tree(self, self.cur);
                        self.show_tree(items);
                    }
                    for k in self.kids.keys().cloned().collect::<Vec<_>>() {
                        self.ropen.insert(k);
                    }
                }
            }
            "destinations" => self.page = Page::Destinations,
            "settings" => self.page = Page::Settings,
            "history" | "status" => {
                self.page = if show == "history" { Page::History } else { Page::Status };
                let cmd = if show == "history" { Cmd::Log } else { Cmd::Status };
                self.text = call(&self.home, &cmd, |_| {}).unwrap_or_default();
                self.text_at = Instant::now();
            }
            "files" => {
                self.open.insert(format!("{}/Documents", self.root));
                self.edit = Some((first_set, Tab::Files));
            }
            "schedule" => self.edit = Some((first_set, Tab::Schedule)),
            _ => {}
        }
    }

    /// For development: SDB_GUI_SHOT=file.ppm saves a picture of the window after it has
    /// settled and exits, so the layout can be checked without a person looking.
    fn maybe_screenshot(&mut self, ctx: &egui::Context) {
        if !cfg!(debug_assertions) {
            return;
        }
        let Ok(path) = std::env::var("SDB_GUI_SHOT") else { return };
        self.frames += 1;
        ctx.request_repaint();
        if self.frames == 20 {
            let done = egui::ScreenshotCallback::new(move |img: std::sync::Arc<egui::ColorImage>| {
                let mut out = format!("P6\n{} {}\n255\n", img.size[0], img.size[1]).into_bytes();
                for p in &img.pixels {
                    out.extend([p.r(), p.g(), p.b()]);
                }
                let _ = std::fs::write(&path, out);
                std::process::exit(0);
            });
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(done));
        }
    }

    /// Runs a command on a background thread; the result comes back as a message.
    fn spawn(&mut self, ctx: &egui::Context, job: Job, cmd: Cmd, label: Option<&str>) {
        if let Some(l) = label {
            self.busy = Some(l.to_string());
            self.progress.clear();
        }
        let (home, tx, ctx, live) = (self.home.clone(), self.tx.clone(), ctx.clone(), label.is_some());
        std::thread::spawn(move || {
            let res = call(&home, &cmd, |l| {
                // Only long jobs report progress; a file listing would flood the screen.
                if live && matches!(cmd, Cmd::Backup { .. } | Cmd::Restore { .. } | Cmd::Verify { .. }) {
                    let _ = tx.send(Msg::Progress(l.to_string()));
                    ctx.request_repaint();
                }
            });
            let _ = tx.send(Msg::Done(job, res));
            ctx.request_repaint();
        });
    }

    fn save(&mut self, ctx: &egui::Context, cmd: Cmd) {
        self.spawn(ctx, Job::Quiet, cmd, None);
    }

    /// Asks the daemon for its current picture every couple of seconds.
    fn poll(&mut self, ctx: &egui::Context) {
        if self.last_poll.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_poll = Instant::now();
        let (home, tx, ctx) = (self.home.clone(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let first = |cmd: Cmd| call(&home, &cmd, |_| {}).ok().and_then(|l| l.into_iter().next());
            if let Some(v) = first(Cmd::Overview).and_then(|l| serde_json::from_str(&l).ok()) {
                let _ = tx.send(Msg::Overview(v));
            }
            if let Some(c) = first(Cmd::Config).and_then(|l| serde_json::from_str(&l).ok()) {
                let _ = tx.send(Msg::Config(Box::new(c)));
            }
            ctx.request_repaint();
        });
    }

    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Overview(v) => self.ov = v,
                Msg::Config(c) => self.cfg = *c,
                Msg::Progress(p) => self.progress = p,
                Msg::Found(search, items) if search == self.search => self.found = Some(items),
                Msg::Found(..) => {}
                Msg::Done(job, res) => {
                    self.last_poll = Instant::now() - Duration::from_secs(3600);
                    match (job, res) {
                        (Job::Quiet, Ok(_)) => self.status = "Saved.".into(),
                        (Job::Quiet, Err(e)) => self.status = e,
                        (Job::Show(title), res) => {
                            self.busy = None;
                            self.snaps_loaded = false;
                            self.popup = Some((
                                title.to_string(),
                                res.unwrap_or_else(|e| e.lines().map(String::from).collect()),
                            ));
                        }
                        (Job::Snaps, Ok(lines)) => {
                            if self.take_snaps(lines) {
                                self.goto(ctx, self.cur);
                            }
                        }
                        (Job::Tree(idx), Ok(lines)) => {
                            self.busy = None;
                            self.cur = idx;
                            self.show_tree(parse_listing(lines));
                        }
                        (Job::Text(page), res) => {
                            if page == self.page {
                                self.text = res.unwrap_or_else(|e| vec![e]);
                            }
                        }
                        (_, Err(e)) => {
                            self.busy = None;
                            self.status = e;
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ backup page

    fn set(&self, name: &str) -> BackupSet {
        self.cfg.set(name).cloned().unwrap_or_else(|| BackupSet { name: name.to_string(), ..Default::default() })
    }

    /// Draws a small status circle: filled when reachable, a ring when not.
    fn dot(ui: &mut egui::Ui, (kind, color): (&'static str, Color32)) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
        if kind == "full" {
            ui.painter().circle_filled(rect.center(), 5.0, color);
        } else {
            ui.painter().circle_stroke(rect.center(), 4.5, egui::Stroke::new(1.5, color));
        }
    }

    fn dest_dot(&self, name: &str) -> (&'static str, Color32) {
        let d = self.ov["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == name));
        match d.and_then(|d| d["state"].as_str()) {
            Some("online") | Some("connected") => ("full", GREEN),
            Some("checking") => ("ring", YELLOW),
            _ => ("ring", RED),
        }
    }

    fn set_edit(name: &str) -> Cmd {
        Cmd::SetEdit {
            name: name.to_string(),
            to: None,
            every: None,
            hours: None,
            days: None,
            versions: None,
            last: None,
            delete: false,
        }
    }

    fn backup_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        for w in self.ov["warnings"].as_array().into_iter().flatten().filter_map(|w| w.as_str()) {
            ui.colored_label(RED, format!("! {w}"));
        }
        if let Some(h) = self.ov["held"].as_str() {
            ui.colored_label(YELLOW, format!("Automatic backups are held: {h}"));
        }
        ui.horizontal(|ui| {
            ui.heading("Backup sets");
            if ui.button("Back up all now").clicked() {
                self.spawn(&ctx, Job::Show("Backup"), Cmd::Backup { tag: None, set: None }, Some("Backing up"));
            }
        });
        let warn_secs = self.cfg.warn_after_days.max(1) as i64 * 86400;
        let now = chrono::Utc::now().timestamp();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for b in self.cfg.sets.clone() {
                let o =
                    self.ov["sets"].as_array().and_then(|a| a.iter().find(|s| s["name"] == b.name.as_str())).cloned();
                let o = o.unwrap_or(Value::Null);
                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&b.name).strong().size(17.0));
                        match o["files"].as_u64() {
                            Some(n) => ui.label(format!("{n} files, {}", human(o["bytes"].as_u64().unwrap_or(0)))),
                            None => ui.weak("not backed up yet"),
                        };
                        match o["running"].as_str() {
                            Some(r) => {
                                ui.spinner();
                                ui.colored_label(BLUE, format!("backing up: {r}"));
                            }
                            None => {
                                ui.weak(format!("last backup {}", ago(o["last"].as_i64())));
                            }
                        }
                    });
                    egui::Grid::new(("set", &b.name)).num_columns(2).show(ui, |ui| {
                        ui.weak("Files");
                        if b.paths.is_empty() {
                            ui.weak("nothing chosen yet");
                        } else {
                            let list: Vec<String> = b.paths.iter().map(|p| tilde(&p.display().to_string())).collect();
                            ui.label(list.join(", "));
                        }
                        ui.end_row();
                        ui.weak("Destinations");
                        ui.horizontal_wrapped(|ui| {
                            let dests: Vec<Friend> =
                                self.cfg.friends.iter().filter(|f| b.sends_to(f)).cloned().collect();
                            if dests.is_empty() {
                                ui.weak("none yet");
                            }
                            for f in dests {
                                let d =
                                    o["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == f.name.as_str()));
                                let last = d.and_then(|d| d["last"].as_i64());
                                let next = match d {
                                    Some(d) if d["held"].as_bool() == Some(true) => ", next: held".to_string(),
                                    Some(d) => match d["next"].as_i64() {
                                        Some(t) => format!(", next {}", until(t)),
                                        None if last.is_some() => ", up to date".to_string(),
                                        None => String::new(),
                                    },
                                    None => String::new(),
                                };
                                let (dot, color) = self.dest_dot(&f.name);
                                Self::dot(ui, (dot, color));
                                ui.label(&f.name);
                                let stale = !b.paths.is_empty() && last.is_none_or(|t| now - t > warn_secs);
                                let when = format!("{}{next}   ", ago(last));
                                if stale {
                                    ui.colored_label(YELLOW, when);
                                } else {
                                    ui.weak(when);
                                }
                            }
                        });
                        ui.end_row();
                        ui.weak("Schedule");
                        let pace = if b.every_minutes > 0 {
                            format!("every {} min", b.every_minutes)
                        } else {
                            "each destination's own pace".into()
                        };
                        let hours =
                            if b.hours.is_empty() { "default hours".to_string() } else { format!("hours {}", b.hours) };
                        ui.label(format!("{pace} · {hours}"));
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Back up now").clicked() {
                            let cmd = Cmd::Backup { tag: None, set: Some(b.name.clone()) };
                            self.spawn(&ctx, Job::Show("Backup"), cmd, Some("Backing up"));
                        }
                        let verify = ui.button("Verify").on_hover_text(
                            "Checks that every destination still holds this set and that a sample of it decrypts.",
                        );
                        if verify.clicked() {
                            let cmd = Cmd::Verify { full: false, local: false, set: Some(b.name.clone()) };
                            self.spawn(&ctx, Job::Show("Verify"), cmd, Some("Verifying"));
                        }
                        for (tab, label) in [
                            (Tab::Files, "Files..."),
                            (Tab::Destinations, "Destinations..."),
                            (Tab::Schedule, "Schedule..."),
                        ] {
                            if ui.button(label).clicked() {
                                self.edit = Some((b.name.clone(), tab));
                                self.fields.retain(|k, _| !k.starts_with("rule-") && k != "rename");
                            }
                        }
                    });
                });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("New backup set:");
                let name = self.fields.entry("newset".into()).or_default();
                ui.add(egui::TextEdit::singleline(name).desired_width(160.0).hint_text("name"));
                let clean: String =
                    name.chars().filter(|c| c.is_alphanumeric() || "-_.".contains(*c)).take(32).collect();
                if ui.add_enabled(!clean.is_empty(), egui::Button::new("Create")).clicked() {
                    self.fields.remove("newset");
                    self.save(&ctx, Self::set_edit(&clean));
                    self.edit = Some((clean, Tab::Files));
                }
            });
        });
    }

    /// The window for one set's files, destinations and schedule.
    fn edit_window(&mut self, ctx: &egui::Context) {
        let Some((set_name, mut tab)) = self.edit.clone() else { return };
        let mut open = true;
        egui::Window::new(format!("Backup set: {set_name}"))
            .id(egui::Id::unique("edit-set"))
            .open(&mut open)
            .default_size([640.0, 480.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut tab, Tab::Files, "Files and folders");
                    ui.selectable_value(&mut tab, Tab::Destinations, "Destinations");
                    ui.selectable_value(&mut tab, Tab::Schedule, "Schedule and history");
                });
                ui.separator();
                match tab {
                    Tab::Files => self.files_tab(ui, &set_name),
                    Tab::Destinations => self.dests_tab(ui, &set_name),
                    Tab::Schedule => self.schedule_tab(ui, &set_name),
                }
            });
        // A tab may have renamed or deleted the set; only then is `edit` already different.
        if !open {
            self.edit = None;
        } else if self.edit.as_ref().is_some_and(|(n, _)| *n == set_name) {
            self.edit = Some((set_name, tab));
        }
    }

    /// "x" chosen, "+" inside a chosen folder, "o" has chosen items inside, "-" left out.
    fn mark(&self, set: &BackupSet, path: &str) -> char {
        let p = Path::new(path);
        let inside = set.paths.iter().any(|x| p.starts_with(x));
        if set.paths.iter().any(|x| x == p) {
            'x'
        } else if inside && self.cfg.exclude.iter().any(|e| p.starts_with(e.replace('\\', ""))) {
            '-'
        } else if inside {
            '+'
        } else if set.paths.iter().any(|x| x.starts_with(p)) {
            'o'
        } else {
            ' '
        }
    }

    fn toggle_path(&mut self, ctx: &egui::Context, set: &BackupSet, path: &str) {
        let p = PathBuf::from(path);
        let cmd = if set.paths.contains(&p) {
            Cmd::RemovePath { path: p }
        } else if set.paths.iter().any(|x| p.starts_with(x)) {
            let pattern = globset::escape(path);
            Cmd::Exclude { remove: self.cfg.exclude.contains(&pattern), pattern }
        } else {
            Cmd::AddPaths { paths: vec![p], set: Some(set.name.clone()) }
        };
        self.save(ctx, cmd);
    }

    fn file_row(&mut self, ui: &mut egui::Ui, set: &BackupSet, item: &Item, depth: usize, label: String) {
        let ctx = ui.ctx().clone();
        let (path, dir, _) = item;
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 18.0);
            if *dir {
                let is_open = self.open.contains(path);
                if ui.small_button(if is_open { "-" } else { "+" }).clicked() {
                    if !self.open.remove(path) {
                        self.open.insert(path.clone());
                    }
                }
            } else {
                ui.add_space(22.0);
            }
            let mark = self.mark(set, path);
            let mut on = matches!(mark, 'x' | '+');
            if ui.checkbox(&mut on, label).changed() {
                self.toggle_path(&ctx, set, path);
            }
            match mark {
                '+' => ui.weak("inside a chosen folder"),
                'o' => ui.colored_label(BLUE, "has chosen items inside"),
                '-' => ui.colored_label(RED, "left out"),
                _ => ui.label(""),
            };
            if let Some(other) =
                self.cfg.sets.iter().find(|b| b.name != set.name && b.paths.iter().any(|x| x == Path::new(path)))
            {
                ui.weak(format!("(in set {})", other.name));
            }
        });
    }

    fn file_tree(&mut self, ui: &mut egui::Ui, set: &BackupSet, dir: &str, depth: usize) {
        if !self.dirs.contains_key(dir) {
            self.dirs.insert(dir.to_string(), list_dir(dir, self.hidden));
        }
        for item in self.dirs[dir].clone() {
            let label = format!("{}{}", name(&item.0), if item.1 { "/" } else { "" });
            self.file_row(ui, set, &item, depth, label);
            if item.1 && self.open.contains(&item.0) && depth < 24 {
                self.file_tree(ui, set, &item.0, depth + 1);
            }
        }
    }

    /// Looks for names below the folder being browsed, level by level, on another thread.
    fn search_files(&mut self, ctx: &egui::Context, text: String) {
        self.search += 1;
        if text.is_empty() {
            self.found = None;
            return;
        }
        let (root, hidden, search, tx, ctx) =
            (self.root.clone(), self.hidden, self.search, self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let needle = text.to_lowercase();
            let began = Instant::now();
            let (mut hits, mut level, mut seen) = (Vec::<Item>::new(), vec![PathBuf::from(&root)], 0u64);
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
                            hits.push((e.path().to_string_lossy().into_owned(), ft.is_dir(), 0));
                        }
                        if ft.is_dir() {
                            next.push(e.path());
                        }
                    }
                }
                let mut sorted = hits.clone();
                sorted.sort_by_key(|a| (!a.1, a.0.matches('/').count(), a.0.to_lowercase()));
                if tx.send(Msg::Found(search, sorted)).is_err() {
                    return;
                }
                ctx.request_repaint();
                level = next;
            }
        });
    }

    fn files_tab(&mut self, ui: &mut egui::Ui, set_name: &str) {
        let ctx = ui.ctx().clone();
        let set = self.set(set_name);
        ui.horizontal(|ui| {
            if ui.button("Up a folder").clicked() {
                if let Some(parent) = Path::new(&self.root).parent() {
                    self.root = parent.to_string_lossy().into_owned();
                    self.found = None;
                }
            }
            ui.label(tilde(&self.root));
            if ui.checkbox(&mut self.hidden, "hidden files").changed() {
                self.dirs.clear();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Search:");
            let text = self.fields.entry("filesearch".into()).or_default();
            let changed =
                ui.add(egui::TextEdit::singleline(text).desired_width(260.0).hint_text("name contains...")).changed();
            let text = text.clone();
            if changed {
                self.search_files(&ctx, text);
            }
            if self.found.is_some() && ui.button("Clear").clicked() {
                self.fields.remove("filesearch");
                self.found = None;
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.found.clone() {
            Some(found) => {
                if found.is_empty() {
                    ui.weak("No names match yet.");
                }
                for item in found {
                    let parent =
                        Path::new(&item.0).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                    let label = format!("{}{}   in {}", name(&item.0), if item.1 { "/" } else { "" }, tilde(&parent));
                    self.file_row(ui, &set, &item, 0, label);
                    if item.1 && self.open.contains(&item.0) {
                        self.file_tree(ui, &set, &item.0, 1);
                    }
                }
            }
            None => {
                let root = self.root.clone();
                self.file_tree(ui, &set, &root, 0);
            }
        });
    }

    fn dests_tab(&mut self, ui: &mut egui::Ui, set_name: &str) {
        let ctx = ui.ctx().clone();
        let set = self.set(set_name);
        if self.cfg.friends.is_empty() {
            ui.label("No destinations yet. Add a friend or a drive on the Destinations page.");
        }
        for f in self.cfg.friends.clone() {
            let mut on = set.sends_to(&f);
            let (dot, color) = self.dest_dot(&f.name);
            ui.horizontal(|ui| {
                let changed = ui.checkbox(&mut on, "").changed();
                Self::dot(ui, (dot, color));
                ui.label(&f.name);
                ui.weak(format!(
                    "{} · every {} min",
                    if f.path.is_some() { "drive" } else { "friend" },
                    set.interval_secs(&f, &self.cfg) / 60
                ));
                if changed {
                    let mut list: Vec<String> =
                        self.cfg.friends.iter().filter(|x| set.sends_to(x)).map(|x| x.name.clone()).collect();
                    if on {
                        if !f.backup_to {
                            self.save(&ctx, Cmd::FriendBackupTo { name: f.name.clone(), on: true });
                        }
                        list.push(f.name.clone());
                    } else {
                        list.retain(|n| *n != f.name);
                    }
                    if list.is_empty() {
                        self.status = "A set needs at least one destination.".into();
                    } else {
                        let mut cmd = Self::set_edit(&set.name);
                        if let Cmd::SetEdit { to, .. } = &mut cmd {
                            *to = Some(list);
                        }
                        self.save(&ctx, cmd);
                    }
                }
            });
        }
    }

    /// A labelled text field that starts out holding the current value.
    fn field(&mut self, ui: &mut egui::Ui, key: &str, label: &str, current: String) -> String {
        ui.label(label);
        let text = self.fields.entry(key.to_string()).or_insert(current);
        ui.add(egui::TextEdit::singleline(text).desired_width(110.0));
        ui.end_row();
        text.trim().to_string()
    }

    fn schedule_tab(&mut self, ui: &mut egui::Ui, set_name: &str) {
        let ctx = ui.ctx().clone();
        let set = self.set(set_name);
        let own = set.retention.clone().unwrap_or_else(|| self.cfg.retention.clone());
        let (mut every, mut hours, mut versions, mut days) = Default::default();
        egui::Grid::new("rules").num_columns(2).show(ui, |ui| {
            every = self.field(
                ui,
                "rule-every",
                "Minutes between backups (0 = each destination's own pace)",
                set.every_minutes.to_string(),
            );
            hours = self.field(
                ui,
                "rule-hours",
                "Backup hours, such as 22-7 (blank = the default hours)",
                set.hours.clone(),
            );
            versions = self.field(
                ui,
                "rule-versions",
                "Most versions kept of each file (0 = unlimited)",
                own.max_versions.to_string(),
            );
            days = self.field(
                ui,
                "rule-days",
                "Delete restore points older than this many days (0 = never)",
                own.keep_days.to_string(),
            );
        });
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                let own_rules = set.retention.is_some();
                let v = versions.parse::<u32>().ok().filter(|v| own_rules || *v != own.max_versions);
                let d = days.parse::<u32>().ok().filter(|d| own_rules || *d != own.keep_days);
                self.save(
                    &ctx,
                    Cmd::SetEdit {
                        name: set.name.clone(),
                        to: None,
                        every: every.parse().ok(),
                        hours: Some(hours.clone()),
                        days: d,
                        versions: v,
                        last: None,
                        delete: false,
                    },
                );
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Verify:");
            if ui.button("Everything (downloads it all)").clicked() {
                let cmd = Cmd::Verify { full: true, local: false, set: Some(set.name.clone()) };
                self.spawn(&ctx, Job::Show("Verify"), cmd, Some("Verifying"));
            }
            if ui.button("My files against the backup").clicked() {
                let cmd = Cmd::Verify { full: false, local: true, set: Some(set.name.clone()) };
                self.spawn(&ctx, Job::Show("Verify"), cmd, Some("Verifying"));
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Rename to:");
            let text = self.fields.entry("rename".into()).or_insert_with(|| set.name.clone());
            ui.add(egui::TextEdit::singleline(text).desired_width(160.0));
            let to: String = text.chars().filter(|c| c.is_alphanumeric() || "-_.".contains(*c)).take(32).collect();
            if ui.add_enabled(!to.is_empty() && to != set.name, egui::Button::new("Rename")).clicked() {
                self.save(&ctx, Cmd::SetRename { name: set.name.clone(), to: to.clone() });
                self.fields.remove("rename");
                self.edit = Some((to, Tab::Schedule));
                self.snaps_loaded = false;
            }
        });
        if self.cfg.sets.len() > 1 {
            ui.horizontal(|ui| {
                let sure = self.fields.entry("delete-sure".into()).or_default();
                ui.label("Delete this set (its restore points stay until pruned). Type yes:");
                ui.add(egui::TextEdit::singleline(sure).desired_width(50.0));
                let ok = sure.trim().eq_ignore_ascii_case("yes");
                if ui.add_enabled(ok, egui::Button::new("Delete")).clicked() {
                    let mut cmd = Self::set_edit(&set.name);
                    if let Cmd::SetEdit { delete, .. } = &mut cmd {
                        *delete = true;
                    }
                    self.save(&ctx, cmd);
                    self.fields.remove("delete-sure");
                    self.edit = None;
                }
            });
        }
    }

    // ----------------------------------------------------------- restore page

    fn load_snaps(&mut self, ctx: &egui::Context) {
        self.snaps_loaded = true;
        self.spawn(ctx, Job::Snaps, Cmd::SnapList { from: self.source.clone() }, Some("Looking up restore points"));
    }

    /// Takes in the list of restore points; returns whether there is one to show.
    fn take_snaps(&mut self, lines: Vec<String>) -> bool {
        let all: Vec<Snap> = lines
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["id"].is_string())
            .map(|v| Snap {
                set: v["set"].as_str().unwrap_or_default().to_string(),
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
        let notes: Vec<&str> = lines.iter().filter(|l| !l.trim_start().starts_with('{')).map(|l| l.trim()).collect();
        if !notes.is_empty() {
            self.status = format!("{} (showing what the others hold)", notes.join("; "));
        }
        // One set at a time: the chosen one, or the one with the newest restore point.
        let set = self.rset.clone().or_else(|| all.last().map(|s| s.set.clone()));
        self.snaps = all.into_iter().filter(|s| Some(&s.set) == set.as_ref()).collect();
        self.rset = set;
        self.cur = self.snaps.len().saturating_sub(1);
        self.kids.clear();
        self.roots.clear();
        self.all.clear();
        self.latest.clear();
        self.picked.clear();
        self.busy = None;
        !self.snaps.is_empty()
    }

    fn goto(&mut self, ctx: &egui::Context, idx: usize) {
        let Some(id) = self.snaps.get(idx).map(|s| s.id.clone()) else { return };
        let cmd = Cmd::Tree { snap: id, from: self.source.clone() };
        self.spawn(ctx, Job::Tree(idx), cmd, Some("Loading that point in time"));
    }

    fn show_tree(&mut self, items: Listing) {
        if self.cur + 1 >= self.snaps.len() {
            self.latest = items.iter().map(|i| (i.0.clone(), i.3.clone())).collect();
        }
        let present: HashSet<&str> = items.iter().map(|i| i.0.as_str()).collect();
        self.kids.clear();
        self.roots.clear();
        self.sigs.clear();
        self.picked.clear();
        for (p, dir, size, v) in &items {
            self.sigs.insert(p.clone(), v.clone());
            let it = (p.clone(), *dir, *size);
            let parent = Path::new(p).parent().map(|x| x.to_string_lossy().into_owned());
            match parent.filter(|x| present.contains(x.as_str())) {
                Some(parent) => self.kids.entry(parent).or_default().push(it),
                None => self.roots.push(it),
            }
        }
        for v in self.kids.values_mut() {
            v.sort_by_key(|a| (!a.1, a.0.to_lowercase()));
        }
        self.all = items.into_iter().map(|(p, d, s, _)| (p, d, s)).collect();
        self.all.sort_by(|a, b| a.0.cmp(&b.0));
        // The top folders start open so there is something to look at.
        for r in self.roots.clone() {
            self.ropen.insert(r.0);
        }
    }

    fn restore_row(&mut self, ui: &mut egui::Ui, item: &Item, depth: usize, label: String) {
        let (path, dir, size) = item;
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 18.0);
            if *dir {
                let is_open = self.ropen.contains(path);
                if ui.small_button(if is_open { "-" } else { "+" }).clicked() && !self.ropen.remove(path) {
                    self.ropen.insert(path.clone());
                }
            } else {
                ui.add_space(22.0);
            }
            let inside = self.picked.iter().any(|p| p != path && Path::new(path).starts_with(p));
            let mut on = self.picked.contains(path) || inside;
            if ui.add_enabled(!inside, egui::Checkbox::new(&mut on, label)).changed() && !self.picked.remove(path) {
                self.picked.insert(path.clone());
            }
            if !dir {
                ui.weak(human(*size));
            }
            if self.picked.iter().any(|p| p != path && Path::new(p).starts_with(path)) {
                ui.colored_label(BLUE, "has picked items inside");
            }
            // How this entry differs from the newest restore point.
            if self.cur + 1 < self.snaps.len() {
                match self.latest.get(path) {
                    None => ui.colored_label(RED, "deleted since"),
                    Some(v) if !dir && self.sigs.get(path) != Some(v) => ui.colored_label(YELLOW, "different now"),
                    _ => ui.label(""),
                };
            }
        });
    }

    fn restore_tree(&mut self, ui: &mut egui::Ui, items: Vec<Item>, depth: usize) {
        for item in items {
            let label = if depth == 0 { tilde(&item.0) } else { name(&item.0) };
            self.restore_row(ui, &item, depth, format!("{label}{}", if item.1 { "/" } else { "" }));
            if item.1 && self.ropen.contains(&item.0) && depth < 64 {
                let kids = self.kids.get(&item.0).cloned().unwrap_or_default();
                self.restore_tree(ui, kids, depth + 1);
            }
        }
    }

    fn restore_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if !self.snaps_loaded && self.busy.is_none() {
            self.load_snaps(&ctx);
        }
        ui.horizontal(|ui| {
            ui.heading("Restore");
            let mut reload = ui.button("Look again").clicked();
            let before = (self.source.clone(), self.rset.clone());
            ui.label("Read from");
            egui::ComboBox::from_id_salt("read-from")
                .selected_text(self.source.clone().unwrap_or_else(|| "all destinations".into()))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.source, None, "all destinations");
                    for f in self.cfg.friends.clone() {
                        ui.selectable_value(&mut self.source, Some(f.name.clone()), f.name);
                    }
                });
            if self.cfg.sets.len() > 1 {
                ui.label("Backup set");
                egui::ComboBox::from_id_salt("which-set")
                    .selected_text(self.rset.clone().unwrap_or_else(|| "newest".into()))
                    .show_ui(ui, |ui| {
                        for b in self.cfg.sets.clone() {
                            ui.selectable_value(&mut self.rset, Some(b.name.clone()), b.name);
                        }
                    });
            }
            reload |= before != (self.source.clone(), self.rset.clone());
            if reload {
                self.load_snaps(&ctx);
            }
        });
        if self.snaps.is_empty() {
            ui.weak("No restore points found yet, or no destination is reachable.");
            return;
        }
        egui::Panel::right("timeline").default_size(300.0).show(ui, |ui| {
            ui.strong(format!("Timeline ({})", self.snaps.len()));
            egui::ScrollArea::vertical().id_salt("timeline").show(ui, |ui| {
                for i in (0..self.snaps.len()).rev() {
                    let s = &self.snaps[i];
                    let tag = s.tag.as_ref().map(|t| format!("  \"{t}\"")).unwrap_or_default();
                    let label = format!(
                        "{}{tag}\n{} files, {} · on {}",
                        when(s.time),
                        s.files,
                        human(s.bytes),
                        s.at.join(", ")
                    );
                    if ui.selectable_label(i == self.cur, label).clicked() && i != self.cur {
                        self.goto(&ctx, i);
                    }
                }
            });
        });
        egui::Panel::bottom("restore-bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("{} picked. Restore into:", self.picked.len()));
                let home = std::env::var("HOME").unwrap_or_default();
                let to = self.fields.entry("restore-to".into()).or_insert_with(|| format!("{home}/sdb-restore"));
                ui.add(egui::TextEdit::singleline(to).desired_width(300.0));
                let to = to.replacen('~', &home, 1);
                if ui.add_enabled(!self.picked.is_empty() && !to.is_empty(), egui::Button::new("Restore")).clicked() {
                    let cmd = Cmd::Restore {
                        snap: self.snaps[self.cur].id.clone(),
                        paths: self.picked.iter().cloned().collect(),
                        to: PathBuf::from(to),
                        from: self.source.clone(),
                    };
                    self.spawn(&ctx, Job::Show("Restore"), cmd, Some("Restoring"));
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            let s = &self.snaps[self.cur];
            let latest = if self.cur + 1 >= self.snaps.len() { " (latest)" } else { "" };
            ui.strong(format!("Files as of {}{latest}", when(s.time)));
            ui.horizontal(|ui| {
                ui.label("Search:");
                let text = self.fields.entry("restore-search".into()).or_default();
                ui.add(egui::TextEdit::singleline(text).desired_width(260.0).hint_text("name contains..."));
                if !text.is_empty() && ui.button("Clear").clicked() {
                    text.clear();
                }
            });
            let needle = self.fields.get("restore-search").cloned().unwrap_or_default().to_lowercase();
            egui::ScrollArea::vertical().id_salt("restore-tree").auto_shrink(false).show(ui, |ui| {
                if needle.is_empty() {
                    let roots = self.roots.clone();
                    self.restore_tree(ui, roots, 0);
                } else {
                    let hits: Vec<Item> = self
                        .all
                        .iter()
                        .filter(|i| name(&i.0).to_lowercase().contains(&needle))
                        .take(500)
                        .cloned()
                        .collect();
                    for item in hits {
                        let parent =
                            Path::new(&item.0).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                        let label =
                            format!("{}{}   in {}", name(&item.0), if item.1 { "/" } else { "" }, tilde(&parent));
                        self.restore_row(ui, &item, 0, label);
                    }
                }
            });
        });
    }

    // ------------------------------------------------------ destinations page

    fn dests_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading("Destinations");
        ui.weak("Friends' computers and local drives that your backups go to.");
        egui::ScrollArea::vertical().show(ui, |ui| {
            for f in self.cfg.friends.clone() {
                let o =
                    self.ov["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == f.name.as_str())).cloned();
                let o = o.unwrap_or(Value::Null);
                let (dot, color) = self.dest_dot(&f.name);
                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        Self::dot(ui, (dot, color));
                        ui.label(RichText::new(&f.name).strong().size(16.0));
                        ui.weak(if f.path.is_some() { "drive" } else { "friend" });
                        ui.label(o["state"].as_str().unwrap_or("checking"));
                        if let Some(l) = o["link"].as_str() {
                            ui.weak(l);
                        }
                        match o["version"].as_str() {
                            Some(v) => ui.weak(format!("version {v}")),
                            None if f.path.is_none() && o["state"] == "online" => ui.weak("older version"),
                            None => ui.label(""),
                        };
                    });
                    let there = match (o["used_there"].as_u64(), o["quota_there"].as_u64()) {
                        (Some(u), Some(q)) => format!("You use {} of {} there. ", human(u), human(q)),
                        (Some(u), None) => format!("You use {} there. ", human(u)),
                        _ => String::new(),
                    };
                    match &f.path {
                        Some(p) => ui.label(format!("{there}{}", p.display())),
                        None => ui.label(format!(
                            "{there}They use {} of {} GB here; last backup from them {}.",
                            human(o["used_here"].as_u64().unwrap_or(0)),
                            f.quota_gb,
                            ago(o["last_in"].as_i64())
                        )),
                    };
                    ui.horizontal(|ui| {
                        let mut on = f.backup_to;
                        if ui.checkbox(&mut on, "in use").changed() {
                            self.save(&ctx, Cmd::FriendBackupTo { name: f.name.clone(), on });
                        }
                        let num = |ui: &mut egui::Ui, me: &mut Self, key: &str, label: &str, cur: u64| -> Option<u64> {
                            ui.label(label);
                            let text = me.fields.entry(format!("{key}-{}", f.name)).or_insert_with(|| cur.to_string());
                            ui.add(egui::TextEdit::singleline(text).desired_width(55.0));
                            text.trim().parse::<u64>().ok().filter(|v| *v != cur)
                        };
                        let every = num(ui, self, "every", "back up every (min)", f.interval_secs(&self.cfg) / 60);
                        let (mut quota, mut limit) = (None, None);
                        if f.path.is_none() {
                            quota = num(ui, self, "quota", "they may store (GB)", f.quota_gb);
                            limit = num(ui, self, "limit", "their upload speed (KB/s, 0 = any)", f.max_in_kbps);
                        }
                        let dirty = every.is_some() || quota.is_some() || limit.is_some();
                        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
                            if let Some(m) = every {
                                self.save(&ctx, Cmd::Every { name: Some(f.name.clone()), minutes: m as u32 });
                            }
                            if let Some(gb) = quota {
                                self.save(&ctx, Cmd::FriendQuota { name: f.name.clone(), gb });
                            }
                            if let Some(kbps) = limit {
                                self.save(&ctx, Cmd::FriendLimit { name: f.name.clone(), kbps });
                            }
                            self.fields.retain(|k, _| !k.ends_with(&format!("-{}", f.name)));
                        }
                        let sure = self.fields.entry(format!("remove-{}", f.name)).or_default();
                        ui.separator();
                        ui.add(egui::TextEdit::singleline(sure).desired_width(40.0).hint_text("yes"));
                        if ui
                            .add_enabled(sure.trim().eq_ignore_ascii_case("yes"), egui::Button::new("Remove"))
                            .clicked()
                        {
                            self.save(&ctx, Cmd::FriendRemove { name: f.name.clone(), purge: false });
                        }
                    });
                });
            }
            ui.add_space(8.0);
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());
                ui.strong("Add a destination");
                ui.horizontal(|ui| {
                    if ui.button("Invite a friend").clicked() {
                        self.spawn(&ctx, Job::Show("Invite"), Cmd::Invite, Some("Creating an invite"));
                    }
                    ui.weak("creates a one-time code to send them");
                });
                ui.horizontal(|ui| {
                    ui.label("Accept an invite:");
                    let text = self.fields.entry("join".into()).or_default();
                    ui.add(egui::TextEdit::singleline(text).desired_width(360.0).hint_text("paste sdb1... here"));
                    let ticket = text.trim().trim_start_matches("sdb join ").to_string();
                    if ui.add_enabled(ticket.starts_with("sdb1"), egui::Button::new("Join")).clicked() {
                        self.fields.remove("join");
                        self.spawn(&ctx, Job::Show("Pairing"), Cmd::Join { ticket }, Some("Pairing"));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Add a drive or folder:");
                    let text = self.fields.entry("drive".into()).or_insert_with(|| "/Volumes/".into());
                    ui.add(egui::TextEdit::singleline(text).desired_width(360.0));
                    let home = std::env::var("HOME").unwrap_or_default();
                    let path = std::fs::canonicalize(text.trim().replacen('~', &home, 1)).ok().filter(|p| p.is_dir());
                    if ui.add_enabled(path.is_some(), egui::Button::new("Add")).clicked() {
                        if let Some(path) = path {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "drive".into());
                            self.fields.remove("drive");
                            self.spawn(&ctx, Job::Show("Drive added"), Cmd::DriveAdd { name, path }, None);
                        }
                    }
                });
            });
        });
    }

    // ---------------------------------------------------------- settings page

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading("Defaults");
        ui.weak("Backup sets can override these.");
        let service = self.ov["service"].as_bool() == Some(true);
        ui.horizontal(|ui| {
            ui.label("Start at login:");
            if service {
                ui.colored_label(GREEN, "yes, the background service is installed");
            } else {
                ui.colored_label(YELLOW, "no. Run `sdb service install` in a terminal to have it start by itself.");
            }
        });
        let c = self.cfg.clone();
        let r = c.retention.clone();
        let paused = c.paused_until > chrono::Utc::now().timestamp();
        ui.horizontal(|ui| {
            ui.label(if paused { "Automatic backups are paused." } else { "Automatic backups are running." });
            let (label, hours) = if paused { ("Resume", 0) } else { ("Pause", 24 * 3650) };
            if ui.button(label).clicked() {
                self.save(
                    &ctx,
                    Cmd::Tune { upload_kbps: None, hours: None, pause_hours: Some(hours), warn_days: None },
                );
            }
        });
        ui.separator();
        let mut v: HashMap<&str, String> = HashMap::new();
        egui::Grid::new("defaults").num_columns(2).show(ui, |ui| {
            let rows: [(&str, &str, String); 11] = [
                ("hours", "Hours automatic backups may run, such as 22-7 (blank = any time)", c.backup_hours.clone()),
                (
                    "every",
                    "Minutes between automatic backups to local drives (friends default to 60)",
                    (c.min_interval_secs / 60).max(1).to_string(),
                ),
                ("limit", "Upload speed limit in KB/s across friends (0 = unlimited)", c.upload_kbps.to_string()),
                ("warn", "Warn when a backup is this many days old (0 = never)", c.warn_after_days.to_string()),
                ("versions", "Most versions kept of each file (0 = unlimited)", r.max_versions.to_string()),
                ("days", "Delete restore points older than this many days (0 = never)", r.keep_days.to_string()),
                ("last", "Always keep this many most recent restore points", r.keep_last.to_string()),
                ("hourly", "Keep one per hour for this many hours", r.keep_hourly.to_string()),
                ("daily", "Keep one per day for this many days", r.keep_daily.to_string()),
                ("weekly", "Keep one per week for this many weeks", r.keep_weekly.to_string()),
                ("monthly", "Keep one per month for this many months", r.keep_monthly.to_string()),
            ];
            for (key, label, cur) in rows {
                let typed = self.field(ui, &format!("def-{key}"), label, cur.clone());
                if typed != cur {
                    v.insert(key, typed);
                }
            }
        });
        if ui.add_enabled(!v.is_empty(), egui::Button::new("Save")).clicked() {
            let num = |k: &str| v.get(k).and_then(|t| t.parse::<u32>().ok());
            if v.contains_key("hours") || v.contains_key("limit") || v.contains_key("warn") {
                self.save(
                    &ctx,
                    Cmd::Tune {
                        upload_kbps: num("limit").map(u64::from),
                        hours: v.get("hours").cloned(),
                        pause_hours: None,
                        warn_days: num("warn"),
                    },
                );
            }
            if let Some(m) = num("every") {
                self.save(&ctx, Cmd::Every { name: None, minutes: m });
            }
            let set: Vec<(String, u32)> = ["days", "last", "hourly", "daily", "weekly", "monthly"]
                .iter()
                .filter_map(|k| Some((k.to_string(), num(k)?)))
                .collect();
            if !set.is_empty() || num("versions").is_some() {
                self.save(&ctx, Cmd::Keep { set, versions: num("versions"), pattern: None });
            }
            self.fields.retain(|k, _| !k.starts_with("def-"));
        }
    }

    fn text_page(&mut self, ui: &mut egui::Ui, title: &str) {
        let ctx = ui.ctx().clone();
        if self.text_at.elapsed() > Duration::from_secs(5) {
            self.text_at = Instant::now();
            let cmd = if self.page == Page::History { Cmd::Log } else { Cmd::Status };
            self.spawn(&ctx, Job::Text(self.page), cmd, None);
        }
        ui.heading(title);
        egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(self.page == Page::History).show(ui, |ui| {
            if self.text.is_empty() {
                ui.weak(if self.page == Page::History {
                    "Nothing has happened since the daemon started."
                } else {
                    "Checking..."
                });
            }
            for l in &self.text {
                if l.starts_with("WARNING") {
                    ui.colored_label(RED, l);
                } else {
                    ui.monospace(l);
                }
            }
        });
    }
}

impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.receive(&ctx);
        self.poll(&ctx);
        self.maybe_screenshot(&ctx);
        ctx.request_repaint_after(Duration::from_secs(1));

        egui::Panel::left("menu").exact_size(150.0).show(ui, |ui| {
            ui.add_space(6.0);
            ui.label(RichText::new("Super Duper Backup").strong());
            ui.weak(format!(
                "{} · sdb {}",
                self.ov["name"].as_str().unwrap_or(&self.cfg.name),
                self.ov["version"].as_str().unwrap_or(crate::net::VERSION)
            ));
            ui.separator();
            let warn = self.ov["warnings"].as_array().is_some_and(|a| !a.is_empty());
            for (page, label) in PAGES {
                let text = if page == Page::Backup && warn { format!("{label}  (!)") } else { label.to_string() };
                if ui.selectable_label(self.page == page, RichText::new(text).size(15.0)).clicked() && self.page != page
                {
                    self.page = page;
                    self.text.clear();
                    self.text_at = Instant::now() - Duration::from_secs(3600);
                }
            }
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| match &self.busy {
                Some(b) => {
                    ui.spinner();
                    ui.label(format!("{b}... {}", self.progress));
                }
                None => {
                    ui.label(&self.status);
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| match self.page {
            Page::Backup => self.backup_page(ui),
            Page::Restore => self.restore_page(ui),
            Page::Destinations => self.dests_page(ui),
            Page::Settings => self.settings_page(ui),
            Page::History => self.text_page(ui, "History"),
            Page::Status => self.text_page(ui, "Status"),
        });
        self.edit_window(&ctx);

        if let Some((title, lines)) = self.popup.clone() {
            let mut open = true;
            egui::Window::new(title).collapsible(false).open(&mut open).show(&ctx, |ui| {
                for l in &lines {
                    ui.label(l);
                }
                ui.horizontal(|ui| {
                    if ui.button("Copy").clicked() {
                        ctx.copy_text(lines.join("\n"));
                    }
                    if ui.button("Close").clicked() {
                        self.popup = None;
                    }
                });
            });
            if !open {
                self.popup = None;
            }
        }
    }
}

pub fn run(home: PathBuf) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1080.0, 700.0]).with_title("Super Duper Backup"),
        ..Default::default()
    };
    let mut gui = Gui::new(home);
    gui.first_load();
    eframe::run_native("Super Duper Backup", options, Box::new(|_cc| Ok(Box::new(gui)))).map_err(|e| {
        anyhow::anyhow!("the window could not be opened (is there a desktop session? `sdb tui` works without one): {e}")
    })
}
