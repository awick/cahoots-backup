// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The graphical interface (`cahoots gui`). Like the TUI it is only a client of the
//! daemon: every button sends a command over the local socket.

use crate::{
    config::{BackupSet, Config, Friend, is_builtin_list, mbps, parse_mbps},
    control,
    node::{CloudAdd, Cmd, Secret, SetEdit, Tune},
    snapshot::{human, when},
    tui::{Item, Listing, ago, list_dir, name, parse_listing, tilde, until},
};
use eframe::egui::{self, Color32, RichText};
use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

const GREEN: Color32 = Color32::from_rgb(60, 170, 80);
const YELLOW: Color32 = Color32::from_rgb(200, 150, 30);
const RED: Color32 = Color32::from_rgb(210, 70, 70);
const BLUE: Color32 = Color32::from_rgb(70, 140, 210);
/// A friend who is there but takes no backups from this computer.
const PURPLE: Color32 = Color32::from_rgb(150, 105, 215);
/// Width of the label column in a backup set's rows.
const LABEL_WIDTH: f32 = 104.0;

/// Sends one command to the daemon and waits for it to finish. Output lines go to
/// `line` as they arrive; progress lines (starting "...") are passed on but not kept.
fn call(home: &Path, cmd: &Cmd, mut line: impl FnMut(&str)) -> Result<Vec<String>, String> {
    let gone = || "the daemon is not running; quit and start `cahoots gui` again".to_string();
    let mut s = control::connect_blocking(home).ok_or_else(gone)?;
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
    (Page::Destinations, "Friends"),
    (Page::Settings, "Settings"),
    (Page::History, "History"),
    (Page::Status, "Status"),
];

/// What a finished background command was for.
/// The window title of adding cloud storage, which also says which job finished.
const CLOUD_JOB: &str = "Cloud storage";

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
    /// The confirm button that was clicked once and is waiting for its second click.
    armed: Option<(String, Instant)>,

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
            armed: None,
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
        // For development: CAHOOTS_GUI_SHOW=restore|destinations|settings|kit|history|status|files|schedule
        // opens on that page with its data already loaded, so a screenshot shows it.
        let show = crate::config::env_text("GUI_SHOW").unwrap_or_default();
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
            "result" => {
                self.popup = Some(("Status".into(), call(&self.home, &Cmd::Status, |_| {}).unwrap_or_default()));
            }
            "destinations" => self.page = Page::Destinations,
            "settings" => self.page = Page::Settings,
            "kit" | "kit-copy" => {
                self.page = Page::Settings;
                let key = if show == "kit" { "kit" } else { "kc" };
                self.fields.insert(format!("{key}-pass"), "correct horse".into());
                self.fields.insert(format!("{key}-again"), String::new());
            }
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
            "set-destinations" => self.edit = Some((first_set, Tab::Destinations)),
            "restore-sets" => self.page = Page::Restore,
            _ => {}
        }
    }

    /// For development: CAHOOTS_GUI_SHOT=file.ppm saves a picture of the window after it has
    /// settled and exits, so the layout can be checked without a person looking.
    fn maybe_screenshot(&mut self, ctx: &egui::Context) {
        if !cfg!(debug_assertions) {
            return;
        }
        let Some(path) = crate::config::env_text("GUI_SHOT") else { return };
        self.frames += 1;
        ctx.request_repaint();
        if self.frames == crate::config::env_text("GUI_SHOT_FRAME").and_then(|f| f.parse().ok()).unwrap_or(20) {
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
                            // The cloud form empties only once the storage was accepted, so a
                            // mistake is fixed in one field rather than typed all over again.
                            if title == CLOUD_JOB && res.is_ok() {
                                self.fields.retain(|k, _| !k.starts_with("cloud-"));
                            }
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
    fn dot(ui: &mut egui::Ui, (kind, color, meaning): (&'static str, Color32, &'static str)) {
        let (rect, over) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
        over.on_hover_text(meaning);
        if kind == "full" {
            ui.painter().circle_filled(rect.center(), 5.0, color);
        } else {
            ui.painter().circle_stroke(rect.center(), 4.5, egui::Stroke::new(1.5, color));
        }
    }

    /// A button for something that cannot be undone. The first click only arms it: it turns
    /// red and asks to be clicked again, and goes back by itself after a few seconds or when
    /// another one is armed. Returns true on that second click.
    fn confirm(&mut self, ui: &mut egui::Ui, key: &str, label: &str, hint: &str) -> bool {
        const ARMED_FOR: Duration = Duration::from_secs(5);
        let armed = self.armed.as_ref().is_some_and(|(k, at)| k == key && at.elapsed() < ARMED_FOR);
        if armed {
            let text = RichText::new(format!("Confirm: {}", label.to_lowercase())).color(Color32::WHITE);
            if ui.add(egui::Button::new(text).fill(RED)).on_hover_text("Click again to go ahead").clicked() {
                self.armed = None;
                return true;
            }
            return false;
        }
        let mut button = ui.button(label);
        if !hint.is_empty() {
            button = button.on_hover_text(hint);
        }
        if button.clicked() {
            self.armed = Some((key.to_string(), Instant::now()));
            // So it is drawn going back, even if nothing else happens meanwhile.
            ui.ctx().request_repaint_after(ARMED_FOR);
        }
        false
    }

    /// A set's rows are a grid whose first column holds the dim labels: the same width
    /// everywhere, so cards and tabs line up with each other.
    fn rows(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, add: impl FnOnce(&mut egui::Ui)) {
        egui::Grid::new(id).num_columns(2).min_col_width(LABEL_WIDTH).spacing([14.0, 7.0]).show(ui, add);
    }

    /// "Backup › default": the page's name leads back to its list of sets. Returns whether
    /// it was clicked.
    fn crumbs(ui: &mut egui::Ui, page: &str, set: &str) -> bool {
        let back = ui
            .add(egui::Label::new(RichText::new(page).heading().color(BLUE)).sense(egui::Sense::click()))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Back to the backup sets");
        ui.label(RichText::new("›").heading().weak());
        ui.heading(set);
        back.clicked()
    }

    /// The small triangle that opens and closes a folder in a tree. Files get the same space
    /// left empty, so names line up whatever they are.
    fn expander(ui: &mut egui::Ui, folder: bool, open: bool) -> bool {
        let sense = if folder { egui::Sense::click() } else { egui::Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(16.0, 18.0), sense);
        if !folder {
            return false;
        }
        let (c, r) = (rect.center(), 4.5);
        let points = if open {
            vec![egui::pos2(c.x - r, c.y - r * 0.6), egui::pos2(c.x + r, c.y - r * 0.6), egui::pos2(c.x, c.y + r * 0.8)]
        } else {
            vec![egui::pos2(c.x - r * 0.6, c.y - r), egui::pos2(c.x + r * 0.8, c.y), egui::pos2(c.x - r * 0.6, c.y + r)]
        };
        let color = if resp.hovered() { ui.visuals().strong_text_color() } else { ui.visuals().weak_text_color() };
        ui.painter().add(egui::Shape::convex_polygon(points, color, egui::Stroke::NONE));
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
    }

    /// What the daemon reports about one set.
    fn set_overview(&self, name: &str) -> Value {
        let o = self.ov["sets"].as_array().and_then(|a| a.iter().find(|s| s["name"] == name)).cloned();
        o.unwrap_or(Value::Null)
    }

    /// "4 files, 8 B · last backup 2 h ago", or what the set is doing right now.
    fn set_status(ui: &mut egui::Ui, o: &Value) {
        if let Some(r) = o["running"].as_str() {
            ui.colored_label(BLUE, format!("backing up: {r}"));
            ui.spinner();
            return;
        }
        ui.weak(match o["files"].as_u64() {
            Some(n) => {
                format!(
                    "{n} files, {} · last backup {}",
                    human(o["bytes"].as_u64().unwrap_or(0)),
                    ago(o["last"].as_i64())
                )
            }
            None => "not backed up yet".to_string(),
        });
    }

    /// A card's top line: the set's name on the left, how it stands on the right.
    fn card_header(ui: &mut egui::Ui, name: &str, o: &Value) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).strong().size(17.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| Self::set_status(ui, o));
        });
        ui.add_space(2.0);
    }

    /// The folders of a set, one to a line.
    fn folders(ui: &mut egui::Ui, set: &BackupSet) {
        ui.vertical(|ui| {
            if set.paths.is_empty() {
                ui.weak("nothing chosen yet");
            }
            for p in &set.paths {
                ui.label(tilde(&p.display().to_string()));
            }
        });
    }

    /// The circle for a destination: its shape, its colour, and what the two mean, which
    /// shows when the pointer rests on it.
    fn dest_dot(&self, name: &str) -> (&'static str, Color32, &'static str) {
        let d = self.ov["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == name));
        match d.and_then(|d| d["state"].as_str()) {
            Some("online") if d.is_some_and(|d| d["closed"] == true) => {
                ("full", PURPLE, "Purple: online, but it takes no backups from this computer")
            }
            Some("online") => ("full", GREEN, "Green: online, backups can go there now"),
            Some("connected") => ("full", GREEN, "Green: the drive is connected, backups can go there now"),
            Some("checking") => ("ring", YELLOW, "Yellow ring: not heard from yet, still checking"),
            Some("not connected") => {
                ("ring", RED, "Red ring: the drive is not connected; it catches up when it is plugged in again")
            }
            _ => ("ring", RED, "Red ring: cannot be reached right now; it catches up when it is back"),
        }
    }

    /// What needs attention, each with the place to deal with it where there is one.
    /// Returns how many there were.
    fn warnings(&mut self, ui: &mut egui::Ui) -> usize {
        let warnings: Vec<String> =
            self.ov["warnings"].as_array().into_iter().flatten().filter_map(|w| w.as_str()).map(String::from).collect();
        for w in &warnings {
            // What a terminal would be told to type is, in a window, a place to go.
            let text = w.split_once(" (cahoots ").map_or(w.as_str(), |(text, _)| text);
            ui.horizontal(|ui| {
                ui.colored_label(RED, format!("! {text}"));
                if w.contains("recovery kit") && ui.link("Fix it in Settings").clicked() {
                    self.page = Page::Settings;
                }
            });
        }
        warnings.len()
    }

    fn set_edit(name: &str) -> Cmd {
        Cmd::SetEdit(SetEdit { name: name.to_string(), ..Default::default() })
    }

    fn backup_page(&mut self, ui: &mut egui::Ui) {
        // A set that was opened fills the page; its name in the heading leads back here.
        if let Some((name, tab)) = self.edit.clone() {
            if self.cfg.set(&name).is_some() {
                return self.set_page(ui, &name, tab);
            }
            self.edit = None;
        }
        let ctx = ui.ctx().clone();
        self.warnings(ui);
        if let Some(h) = self.ov["held"].as_str() {
            ui.colored_label(YELLOW, format!("Automatic backups are held: {h}"));
        }
        ui.horizontal(|ui| {
            ui.heading("Backup");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Back up all now").clicked() {
                    self.spawn(&ctx, Job::Show("Backup"), Cmd::Backup { tag: None, set: None }, Some("Backing up"));
                }
            });
        });
        ui.add_space(4.0);
        let warn_secs = self.cfg.warn_after_days.max(1) as i64 * 86400;
        let now = chrono::Utc::now().timestamp();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for b in self.cfg.sets.clone() {
                let o = self.set_overview(&b.name);
                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());
                    Self::card_header(ui, &b.name, &o);
                    Self::rows(ui, ("set", &b.name), |ui| {
                        ui.weak("Files");
                        Self::folders(ui, &b);
                        ui.end_row();
                        ui.weak("Destinations");
                        let dests: Vec<Friend> = self.cfg.friends.iter().filter(|f| b.sends_to(f)).cloned().collect();
                        if dests.is_empty() {
                            ui.weak("none yet");
                        }
                        // One destination to a line, in columns: where, when it last got a backup, what is next.
                        egui::Grid::new(("set-dests", &b.name)).num_columns(3).spacing([18.0, 4.0]).show(ui, |ui| {
                            for f in dests {
                                let d =
                                    o["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == f.name.as_str()));
                                let last = d.and_then(|d| d["last"].as_i64());
                                let next = match d {
                                    Some(d) if d["held"].as_bool() == Some(true) => "next: held".to_string(),
                                    Some(d) => match d["next"].as_i64() {
                                        Some(t) => format!("next {}", until(t)),
                                        None if last.is_some() => "up to date".to_string(),
                                        None => String::new(),
                                    },
                                    None => String::new(),
                                };
                                ui.horizontal(|ui| {
                                    Self::dot(ui, self.dest_dot(&f.name));
                                    ui.label(&f.name);
                                });
                                let stale = !b.paths.is_empty() && last.is_none_or(|t| now - t > warn_secs);
                                let when = RichText::new(ago(last));
                                ui.label(if stale { when.color(YELLOW) } else { when.weak() });
                                ui.weak(next);
                                ui.end_row();
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
                        ui.weak("Leaves out");
                        let skipped = match b.skip.len() {
                            0 => String::new(),
                            n => format!(" · {n} left out by hand"),
                        };
                        ui.label(format!("{} list{skipped}", self.cfg.list_for(&b).name));
                        ui.end_row();
                    });
                    ui.add_space(4.0);
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
                        // The ways into the set sit together on the right.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            for (tab, label) in [
                                (Tab::Schedule, "Schedule"),
                                (Tab::Destinations, "Destinations"),
                                (Tab::Files, "Files"),
                            ] {
                                if ui.button(label).clicked() {
                                    self.open_set(&b.name, tab);
                                }
                            }
                            ui.weak("Change:");
                        });
                    });
                });
                ui.add_space(4.0);
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

    fn open_set(&mut self, name: &str, tab: Tab) {
        self.edit = Some((name.to_string(), tab));
        self.fields.retain(|k, _| !k.starts_with("rule-") && k != "rename");
    }

    /// One backup set, filling the Backup page: what it is doing, and its files, destinations
    /// and schedule on tabs.
    fn set_page(&mut self, ui: &mut egui::Ui, set_name: &str, mut tab: Tab) {
        let ctx = ui.ctx().clone();
        let o = self.set_overview(set_name);
        let mut back = false;
        ui.horizontal(|ui| {
            back = Self::crumbs(ui, "Backup", set_name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let verify = ui.button("Verify").on_hover_text(
                    "Checks that every destination still holds this set and that a sample of it decrypts.",
                );
                if verify.clicked() {
                    let cmd = Cmd::Verify { full: false, local: false, set: Some(set_name.to_string()) };
                    self.spawn(&ctx, Job::Show("Verify"), cmd, Some("Verifying"));
                }
                if ui.button("Back up now").clicked() {
                    let cmd = Cmd::Backup { tag: None, set: Some(set_name.to_string()) };
                    self.spawn(&ctx, Job::Show("Backup"), cmd, Some("Backing up"));
                }
                Self::set_status(ui, &o);
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut tab, Tab::Files, "Files and folders");
            ui.selectable_value(&mut tab, Tab::Destinations, "Destinations");
            ui.selectable_value(&mut tab, Tab::Schedule, "Schedule and history");
        });
        ui.separator();
        match tab {
            Tab::Files => self.files_tab(ui, set_name),
            Tab::Destinations => self.dests_tab(ui, set_name),
            Tab::Schedule => self.schedule_tab(ui, set_name),
        }
        // A tab may have renamed or deleted the set; only then is `edit` already different.
        if back {
            self.edit = None;
        } else if self.edit.as_ref().is_some_and(|(n, _)| n == set_name) {
            self.edit = Some((set_name.to_string(), tab));
        }
    }

    /// "x" chosen, "+" inside a chosen folder, "o" has chosen items inside, "-" left out.
    fn mark(&self, set: &BackupSet, path: &str) -> char {
        let p = Path::new(path);
        let inside = set.paths.iter().any(|x| p.starts_with(x));
        if set.paths.iter().any(|x| x == p) {
            'x'
        } else if inside && set.skip.iter().any(|e| p.starts_with(e)) {
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
            Cmd::Skip { remove: set.skip.contains(&p), path: p, set: Some(set.name.clone()) }
        } else {
            Cmd::AddPaths { paths: vec![p], set: Some(set.name.clone()) }
        };
        self.save(ctx, cmd);
    }

    fn file_row(&mut self, ui: &mut egui::Ui, set: &BackupSet, item: &Item, depth: usize, label: String) {
        let ctx = ui.ctx().clone();
        let (path, dir, size) = item;
        ui.horizontal(|ui| {
            // A long name is cut short rather than pushing the size column out of sight.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            ui.add_space(depth as f32 * 18.0);
            if Self::expander(ui, *dir, self.open.contains(path)) && !self.open.remove(path) {
                self.open.insert(path.clone());
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
            // Sizes sit in a column of their own at the right edge.
            if !dir {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.weak(human(*size)));
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
        Self::rows(ui, "files-rows", |ui| {
            ui.weak("Folder");
            ui.horizontal(|ui| {
                if ui.button("Up").on_hover_text("Show the folder this one is in").clicked() {
                    if let Some(parent) = Path::new(&self.root).parent() {
                        self.root = parent.to_string_lossy().into_owned();
                        self.found = None;
                    }
                }
                ui.label(tilde(&self.root));
                ui.add_space(12.0);
                if ui.checkbox(&mut self.hidden, "show hidden files").changed() {
                    self.dirs.clear();
                }
            });
            ui.end_row();
            ui.weak("Search");
            ui.horizontal(|ui| {
                let text = self.fields.entry("filesearch".into()).or_default();
                let changed = ui
                    .add(egui::TextEdit::singleline(text).desired_width(280.0).hint_text("name contains..."))
                    .changed();
                let text = text.clone();
                if changed {
                    self.search_files(&ctx, text);
                }
                if self.found.is_some() && ui.button("Clear").clicked() {
                    self.fields.remove("filesearch");
                    self.found = None;
                }
            });
            ui.end_row();
            ui.weak("Leaves out");
            ui.horizontal(|ui| {
                let now = self.cfg.list_for(&set).name;
                let mut picked = now.clone();
                egui::ComboBox::from_id_salt("exclude-list").selected_text(format!("{now} list")).show_ui(ui, |ui| {
                    for l in self.cfg.lists() {
                        ui.selectable_value(
                            &mut picked,
                            l.name.clone(),
                            format!("{} ({} patterns)", l.name, l.patterns.len()),
                        );
                    }
                });
                if picked != now {
                    self.save(&ctx, Cmd::SetExclude { name: set.name.clone(), list: picked });
                }
                ui.weak("and whatever is unticked inside a chosen folder");
            });
            ui.end_row();
            // What is left out now stays in older restore points until it is taken out of them.
            ui.weak("Older backups");
            ui.horizontal(|ui| {
                let prune = |dry_run| Cmd::Prune { dry_run, apply_exclusions: true, set: Some(set.name.clone()) };
                let look = ui.button("What do they still hold?").on_hover_text(
                    "Older restore points keep what was left out later. This shows how much, without changing anything.",
                );
                if look.clicked() {
                    let label = Some("Looking through older restore points");
                    self.spawn(&ctx, Job::Show("Left out now"), prune(true), label);
                }
                if self.confirm(ui, "apply-excl", "Take it out of them", "") {
                    self.spawn(&ctx, Job::Show("Prune"), prune(false), Some("Pruning"));
                }
            });
            ui.end_row();
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
        let o = self.set_overview(set_name);
        if self.cfg.friends.is_empty() {
            ui.label("No destinations yet. Add a friend, a drive or cloud storage on the Friends page.");
            return;
        }
        ui.weak("Tick where this set's backups go.");
        ui.add_space(4.0);
        egui::Grid::new("set-dest-rows").num_columns(5).spacing([22.0, 8.0]).show(ui, |ui| {
            for head in ["", "Destination", "Kind", "Sent", "Last backup"] {
                ui.weak(head);
            }
            ui.end_row();
            for f in self.cfg.friends.clone() {
                let mut on = set.sends_to(&f);
                let changed = ui.checkbox(&mut on, "").changed();
                ui.horizontal(|ui| {
                    Self::dot(ui, self.dest_dot(&f.name));
                    ui.label(&f.name);
                });
                ui.label(f.kind());
                ui.label(format!("every {} min", set.interval_secs(&f, &self.cfg) / 60));
                let d = o["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == f.name.as_str()));
                ui.weak(match d {
                    Some(d) => ago(d["last"].as_i64()),
                    None if !f.backup_to => "not in use".to_string(),
                    None => String::new(),
                });
                ui.end_row();
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
                        if let Cmd::SetEdit(e) = &mut cmd {
                            e.to = Some(list);
                        }
                        self.save(&ctx, cmd);
                    }
                }
            }
        });
    }

    /// A row of the schedule: what it sets, the box to type in, and a word on what goes there.
    fn rule(&mut self, ui: &mut egui::Ui, key: &str, label: &str, current: String, unit: &str, hint: &str) -> String {
        ui.weak(label);
        let text = self.fields.entry(key.to_string()).or_insert(current);
        let typed = ui
            .horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(text).desired_width(70.0));
                // The unit takes the same room on every row, so the notes after it line up.
                let slot = egui::vec2(64.0, ui.spacing().interact_size.y);
                ui.allocate_ui_with_layout(slot, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(unit);
                    ui.allocate_space(ui.available_size());
                });
                ui.weak(hint);
                text.trim().to_string()
            })
            .inner;
        ui.end_row();
        typed
    }

    fn schedule_tab(&mut self, ui: &mut egui::Ui, set_name: &str) {
        let ctx = ui.ctx().clone();
        let set = self.set(set_name);
        let own = set.retention.clone().unwrap_or_else(|| self.cfg.retention.clone());
        let (mut every, mut hours, mut versions, mut days) = Default::default();
        Self::rows(ui, "rules", |ui| {
            let cur = set.every_minutes.to_string();
            every = self.rule(ui, "rule-every", "Back up every", cur, "min", "0 = each destination's own pace");
            hours = self.rule(
                ui,
                "rule-hours",
                "Only between",
                set.hours.clone(),
                "",
                "hours such as 22-7; blank = the default hours",
            );
            let cur = own.max_versions.to_string();
            versions = self.rule(ui, "rule-versions", "Keep", cur, "versions", "of each file; 0 = all of them");
            let cur = own.keep_days.to_string();
            days = self.rule(ui, "rule-days", "Keep for", cur, "days", "then restore points are deleted; 0 = for ever");
            ui.label("");
            if ui.button("Save").clicked() {
                let own_rules = set.retention.is_some();
                let v = versions.parse::<u32>().ok().filter(|v| own_rules || *v != own.max_versions);
                let d = days.parse::<u32>().ok().filter(|d| own_rules || *d != own.keep_days);
                self.save(
                    &ctx,
                    Cmd::SetEdit(SetEdit {
                        name: set.name.clone(),
                        every: every.parse().ok(),
                        hours: Some(hours.clone()),
                        days: d,
                        versions: v,
                        ..Default::default()
                    }),
                );
            }
            ui.end_row();
        });
        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);
        Self::rows(ui, "set-more", |ui| {
            ui.weak("Verify");
            ui.horizontal(|ui| {
                if ui.button("Everything").on_hover_text("Downloads and checks every chunk of this set.").clicked() {
                    let cmd = Cmd::Verify { full: true, local: false, set: Some(set.name.clone()) };
                    self.spawn(&ctx, Job::Show("Verify"), cmd, Some("Verifying"));
                }
                let local = ui
                    .button("My files against the backup")
                    .on_hover_text("Re-reads unchanged files on this computer and compares them with the last backup.");
                if local.clicked() {
                    let cmd = Cmd::Verify { full: false, local: true, set: Some(set.name.clone()) };
                    self.spawn(&ctx, Job::Show("Verify"), cmd, Some("Verifying"));
                }
            });
            ui.end_row();
            ui.weak("Rename");
            ui.horizontal(|ui| {
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
            ui.end_row();
            if self.cfg.sets.len() > 1 {
                ui.weak("Delete");
                ui.horizontal(|ui| {
                    if self.confirm(ui, "delete-set", "Delete this set", "") {
                        let mut cmd = Self::set_edit(&set.name);
                        if let Cmd::SetEdit(e) = &mut cmd {
                            e.delete = true;
                        }
                        self.save(&ctx, cmd);
                        self.edit = None;
                    }
                    ui.weak("its restore points stay until they are pruned");
                });
                ui.end_row();
            }
        });
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
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            ui.add_space(depth as f32 * 18.0);
            if Self::expander(ui, *dir, self.ropen.contains(path)) && !self.ropen.remove(path) {
                self.ropen.insert(path.clone());
            }
            let inside = self.picked.iter().any(|p| p != path && Path::new(path).starts_with(p));
            let mut on = self.picked.contains(path) || inside;
            if ui.add_enabled(!inside, egui::Checkbox::new(&mut on, label)).changed() && !self.picked.remove(path) {
                self.picked.insert(path.clone());
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
            // Sizes sit in a column of their own at the right edge.
            if !dir {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.weak(human(*size)));
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
        // Restore starts with the choice of backup set, as Backup does.
        let Some(chosen) = self.rset.clone() else {
            ui.heading("Restore");
            ui.weak("Choose the backup set to restore from.");
            ui.add_space(4.0);
            for b in self.cfg.sets.clone() {
                let o = self.set_overview(&b.name);
                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());
                    Self::card_header(ui, &b.name, &o);
                    Self::rows(ui, ("restore-set", &b.name), |ui| {
                        ui.weak("Files");
                        Self::folders(ui, &b);
                        ui.end_row();
                        ui.weak("Kept on");
                        // Each with the dot that says whether it can be reached right now.
                        ui.horizontal(|ui| {
                            let on: Vec<Friend> = self.cfg.friends.iter().filter(|f| b.sends_to(f)).cloned().collect();
                            if on.is_empty() {
                                ui.weak("no destination yet");
                            }
                            for f in on {
                                Self::dot(ui, self.dest_dot(&f.name));
                                ui.label(&f.name);
                                ui.add_space(10.0);
                            }
                        });
                        ui.end_row();
                    });
                    ui.add_space(4.0);
                    if ui.button("Browse its restore points").clicked() {
                        self.rset = Some(b.name.clone());
                        self.snaps.clear();
                        self.load_snaps(&ctx);
                    }
                });
                ui.add_space(4.0);
            }
            return;
        };
        if !self.snaps_loaded && self.busy.is_none() {
            self.load_snaps(&ctx);
        }
        let mut leave = false;
        ui.horizontal(|ui| {
            leave = Self::crumbs(ui, "Restore", &chosen);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut reload = ui.button("Look again").clicked();
                let before = self.source.clone();
                egui::ComboBox::from_id_salt("read-from")
                    .selected_text(self.source.clone().unwrap_or_else(|| "all destinations".into()))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.source, None, "all destinations");
                        for f in self.cfg.friends.clone() {
                            ui.selectable_value(&mut self.source, Some(f.name.clone()), f.name);
                        }
                    });
                ui.weak("Read from");
                reload |= before != self.source;
                if reload {
                    self.load_snaps(&ctx);
                }
            });
        });
        ui.add_space(4.0);
        if leave {
            self.rset = None;
            self.snaps.clear();
            self.snaps_loaded = false;
            return;
        }
        if self.snaps.is_empty() {
            ui.weak("No restore points found yet, or no destination is reachable.");
            return;
        }
        egui::Panel::right("timeline").default_size(300.0).size_range(200.0..=700.0).show(ui, |ui| {
            ui.take_available_width();
            ui.strong(format!("Timeline ({})", self.snaps.len()));
            egui::ScrollArea::vertical().id_salt("timeline").auto_shrink(false).show(ui, |ui| {
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
        egui::Panel::bottom("restore-bar").resizable(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("{} picked. Restore into:", self.picked.len()));
                let home = std::env::var("HOME").unwrap_or_default();
                let to = self.fields.entry("restore-to".into()).or_insert_with(|| format!("{home}/cahoots-restore"));
                ui.add(egui::TextEdit::singleline(to).desired_width(300.0));
                let to = to.replacen('~', &home, 1);
                if ui.add_enabled(!self.picked.is_empty() && !to.is_empty(), egui::Button::new("Restore")).clicked() {
                    let cmd = Cmd::Restore {
                        snap: self.snaps[self.cur].id.clone(),
                        paths: self.picked.iter().cloned().collect(),
                        to: PathBuf::from(to),
                        from: self.source.clone(),
                        set: None,
                    };
                    self.spawn(&ctx, Job::Show("Restore"), cmd, Some("Restoring"));
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            let s = &self.snaps[self.cur];
            let latest = if self.cur + 1 >= self.snaps.len() { " (latest)" } else { "" };
            let title = format!("Files as of {}{latest}", when(s.time));
            ui.horizontal(|ui| {
                ui.strong(title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let text = self.fields.entry("restore-search".into()).or_default();
                    if !text.is_empty() && ui.button("Clear").clicked() {
                        text.clear();
                    }
                    ui.add(egui::TextEdit::singleline(text).desired_width(240.0).hint_text("search by name..."));
                });
            });
            ui.separator();
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

    // ----------------------------------------------------------- friends page

    fn dests_page(&mut self, ui: &mut egui::Ui) {
        ui.heading("Friends");
        ui.weak("Friends' computers that your backups go to, and drives and cloud storage of your own.");
        ui.add_space(4.0);
        egui::ScrollArea::vertical().show(ui, |ui| {
            let all = self.cfg.friends.clone();
            // Friends come first. A drive or a bucket is kept on this page too, but under its
            // own heading: it is nobody's friend.
            let groups = [
                ("Friends", "friend", "No friends yet. One invite pairs the two of you both ways."),
                ("Your own drives", "drive", "No drive yet."),
                ("Cloud storage", "cloud", "No cloud storage yet."),
            ];
            for (title, kind, none) in groups {
                if kind != "friend" {
                    ui.add_space(14.0);
                }
                ui.label(RichText::new(title).strong().size(19.0));
                ui.add_space(2.0);
                let mine: Vec<&Friend> = all.iter().filter(|f| f.kind() == kind).collect();
                if mine.is_empty() {
                    ui.weak(none);
                    ui.add_space(4.0);
                }
                for f in mine {
                    self.dest_card(ui, f);
                    ui.add_space(4.0);
                }
                match kind {
                    "friend" => self.add_friend_box(ui),
                    "drive" => self.add_drive_box(ui),
                    _ => self.add_cloud_box(ui),
                }
            }
        });
    }

    /// One friend, drive or bucket: how it stands, and its settings in rows.
    fn dest_card(&mut self, ui: &mut egui::Ui, f: &Friend) {
        let ctx = ui.ctx().clone();
        let o = self.ov["dests"].as_array().and_then(|a| a.iter().find(|d| d["name"] == f.name.as_str())).cloned();
        let o = o.unwrap_or(Value::Null);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            // The name on the left, what it is and how it is reached on the right.
            ui.horizontal(|ui| {
                Self::dot(ui, self.dest_dot(&f.name));
                ui.label(RichText::new(&f.name).strong().size(17.0));
                if !f.backup_to {
                    ui.colored_label(YELLOW, "not in use");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut about = vec![f.kind().to_string(), o["state"].as_str().unwrap_or("checking").to_string()];
                    if o["closed"] == true {
                        about.push("takes no backups from you".into());
                    }
                    about.extend(o["link"].as_str().map(String::from));
                    match o["version"].as_str() {
                        Some(v) => about.push(format!("version {v}")),
                        None if f.is_friend() && o["state"] == "online" => about.push("older version".into()),
                        None => {}
                    }
                    ui.weak(about.join(" · "));
                });
            });
            ui.add_space(2.0);
            let key = |k: &str| format!("{k}-{}", f.name);
            let (mut every, mut quota, mut limit) = (String::new(), None, None);
            let (mut days, mut last) = (None, None);
            let shown = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
            let (had_days, had_last) =
                (shown(f.retention.as_ref().map(|r| r.keep_days)), shown(f.retention.as_ref().map(|r| r.keep_last)));
            Self::rows(ui, ("dest", &f.name), |ui| {
                ui.weak("You use");
                let there = match (o["used_there"].as_u64(), o["quota_there"].as_u64()) {
                    (Some(u), Some(q)) => format!("{} of {} there", human(u), human(q)),
                    (Some(u), None) => format!("{} there", human(u)),
                    _ => "not known yet".to_string(),
                };
                ui.horizontal(|ui| {
                    ui.label(format!("{there} · last sent {}", ago(o["last_out"].as_i64())));
                    if o["closed"] == true {
                        ui.colored_label(PURPLE, "they do not accept backups from this computer");
                    }
                });
                ui.end_row();
                match (&f.path, &f.cloud) {
                    (Some(p), _) => {
                        ui.weak("Folder");
                        ui.label(p.display().to_string());
                    }
                    (_, Some(c)) => {
                        ui.weak("Bucket");
                        ui.label(format!("{} at {}", c.bucket, c.endpoint));
                    }
                    _ => {
                        ui.weak("They use");
                        ui.horizontal(|ui| {
                            ui.label(format!(
                                "{} of {} GB here · last backup from them {}",
                                human(o["used_here"].as_u64().unwrap_or(0)),
                                f.quota_gb,
                                ago(o["last_in"].as_i64())
                            ));
                            if !self.cfg.accept_backups {
                                ui.colored_label(PURPLE, "this computer accepts no new backups (see Settings)");
                            }
                        });
                    }
                }
                ui.end_row();
                ui.weak("In use");
                let mut on = f.backup_to;
                if ui.checkbox(&mut on, "send backups here").changed() {
                    self.save(&ctx, Cmd::FriendBackupTo { name: f.name.clone(), on });
                }
                ui.end_row();
                let cur = (f.interval_secs(&self.cfg) / 60).to_string();
                every = self.rule(ui, &key("every"), "Back up every", cur, "min", "at most this often");
                if f.is_friend() {
                    let hint = "what they may store on this computer";
                    quota = Some(self.rule(ui, &key("quota"), "Their quota", f.quota_gb.to_string(), "GB", hint));
                    let hint = "fastest they may send here, such as 0.5; 0 = any speed";
                    limit = Some(self.rule(ui, &key("limit"), "Their speed", mbps(f.max_in_kbps), "MB/s", hint));
                } else if f.cloud.is_some() {
                    let hint = "most to keep there, oldest restore points go first; 0 = no cap";
                    quota = Some(self.rule(ui, &key("quota"), "Cap", f.quota_gb.to_string(), "GB", hint));
                }
                // History kept here, on one row; blank follows each set's own rules.
                ui.weak("Keep");
                ui.horizontal(|ui| {
                    let mut boxed = |ui: &mut egui::Ui, k: &str, cur: &str| {
                        let text = self.fields.entry(key(k)).or_insert_with(|| cur.to_string());
                        ui.add(egui::TextEdit::singleline(text).desired_width(70.0));
                        text.trim().to_string()
                    };
                    days = Some(boxed(ui, "kdays", &had_days));
                    ui.label("days of history, and always the last");
                    last = Some(boxed(ui, "klast", &had_last));
                    ui.add_space(8.0);
                    ui.weak("blank = each set's rules");
                });
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let num = |t: &str, cur: u64| t.parse::<u64>().ok().filter(|v| *v != cur);
                let every = num(&every, f.interval_secs(&self.cfg) / 60);
                let quota = quota.as_deref().and_then(|t| num(t, f.quota_gb));
                let limit = limit.filter(|t| *t != mbps(f.max_in_kbps)).and_then(|t| parse_mbps(&t));
                let days = days.filter(|t| *t != had_days);
                let last = last.filter(|t| *t != had_last);
                let dirty = every.is_some() || quota.is_some() || limit.is_some() || days.is_some() || last.is_some();
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
                    let typed = [("days", &days), ("last", &last)];
                    if typed.iter().any(|(_, t)| t.is_some()) {
                        let set: Vec<(String, u32)> = typed
                            .iter()
                            .filter_map(|(k, t)| Some((k.to_string(), t.as_ref()?.parse().ok()?)))
                            .collect();
                        // Both blanked: back to each set's rules.
                        let blank = |t: &Option<String>, cur: bool| t.as_ref().map_or(!cur, |t| t.is_empty());
                        let clear = set.is_empty()
                            && blank(&days, f.retention.is_some())
                            && blank(&last, f.retention.is_some());
                        if clear || !set.is_empty() {
                            self.save(&ctx, Cmd::DestKeep { name: f.name.clone(), set, clear });
                        }
                    }
                    self.fields.retain(|k, _| !k.ends_with(&format!("-{}", f.name)));
                }
                // What removes things sits together on the right; each asks to be clicked twice.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let clean = |dry_run| Cmd::DestClean { name: f.name.clone(), dry_run };
                    let hint = "Stops using this destination. What is stored there is left in place.";
                    if self.confirm(ui, &format!("remove-{}", f.name), "Remove", hint) {
                        self.save(&ctx, Cmd::FriendRemove { name: f.name.clone(), purge: false });
                    }
                    let hint = "Deletes from here the restore points of sets that are no longer sent here.";
                    if self.confirm(ui, &format!("clean-{}", f.name), "Clean up", hint) {
                        self.spawn(&ctx, Job::Show("Clean up"), clean(false), Some("Cleaning up"));
                    }
                    let look = ui
                        .button("Leftovers?")
                        .on_hover_text("Shows what sets no longer sent here left behind, without changing anything.");
                    if look.clicked() {
                        self.spawn(&ctx, Job::Show("Leftovers"), clean(true), Some("Looking for leftovers"));
                    }
                });
            });
        });
    }

    fn add_friend_box(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Add a friend").strong().size(15.0));
            ui.weak("One of you creates an invite and sends it; the other pastes it in. That pairs both ways.");
            ui.add_space(2.0);
            Self::rows(ui, "add-friend", |ui| {
                ui.weak("Your invite");
                ui.horizontal(|ui| {
                    if ui.button("Create an invite").clicked() {
                        self.spawn(&ctx, Job::Show("Invite"), Cmd::Invite, Some("Creating an invite"));
                    }
                    ui.weak("a one-time code to send them, good for a day");
                });
                ui.end_row();
                ui.weak("Their invite");
                ui.horizontal(|ui| {
                    let text = self.fields.entry("join".into()).or_default();
                    ui.add(egui::TextEdit::singleline(text).desired_width(360.0).hint_text("paste cahoots1... here"));
                    let ticket =
                        text.trim().trim_start_matches("cahoots join ").trim_start_matches("sdb join ").to_string();
                    let invite = ticket.starts_with("cahoots1") || ticket.starts_with("sdb1");
                    if ui.add_enabled(invite, egui::Button::new("Join")).clicked() {
                        self.fields.remove("join");
                        self.spawn(&ctx, Job::Show("Pairing"), Cmd::Join { ticket }, Some("Pairing"));
                    }
                });
                ui.end_row();
            });
        });
    }

    fn add_drive_box(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Add a drive").strong().size(15.0));
            ui.weak(
                "An external drive or any folder. It is encrypted like everything else, and skipped while unplugged.",
            );
            ui.add_space(2.0);
            Self::rows(ui, "add-drive", |ui| {
                ui.weak("Folder");
                ui.horizontal(|ui| {
                    let text = self.fields.entry("drive".into()).or_insert_with(|| "/Volumes/".into());
                    ui.add(egui::TextEdit::singleline(text).desired_width(360.0));
                    let home = std::env::var("HOME").unwrap_or_default();
                    // The system's own folder chooser, opened where the box points if that exists.
                    if ui.button("Browse...").on_hover_text("Choose the folder in a dialog").clicked() {
                        let typed = PathBuf::from(text.trim().replacen('~', &home, 1));
                        let start = [typed, PathBuf::from("/Volumes"), PathBuf::from(&home)]
                            .into_iter()
                            .find(|p| p.is_dir())
                            .unwrap_or_else(|| PathBuf::from("/"));
                        let picked = rfd::FileDialog::new()
                            .set_title("Choose a drive or folder to back up to")
                            .set_directory(start)
                            .pick_folder();
                        if let Some(dir) = picked {
                            *text = dir.display().to_string();
                        }
                    }
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
                ui.end_row();
            });
        });
    }

    fn add_cloud_box(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Add cloud storage").strong().size(15.0));
            ui.weak("Any S3-compatible bucket: Amazon S3, Backblaze B2, Cloudflare R2, Wasabi, MinIO. Using the name of one you already have gives it new keys.");
            let mut got: Vec<String> = vec![];
            ui.add_space(2.0);
            Self::rows(ui, "cloud", |ui| {
                let rows = [
                    ("cloud-name", "Name", "what to call this destination", false),
                    ("cloud-endpoint", "Endpoint", "https://s3.us-west-004.backblazeb2.com", false),
                    ("cloud-bucket", "Bucket", "", false),
                    ("cloud-region", "Region", "us-east-1", false),
                    ("cloud-prefix", "Folder", "optional: a folder inside the bucket", false),
                    ("cloud-ca", "CA certificate", "optional: a file, only for storage with a certificate of its own", false),
                    ("cloud-access", "Access key", "the key id", false),
                    ("cloud-secret", "Secret key", "", true),
                ];
                for (key, label, hint, secret) in rows {
                    ui.weak(label);
                    let text = self.fields.entry(key.into()).or_default();
                    ui.add(egui::TextEdit::singleline(text).desired_width(420.0).hint_text(hint).password(secret));
                    ui.end_row();
                    got.push(text.trim().to_string());
                }
            });
            // Region and folder may stay blank.
            let ready = [0, 1, 2, 6, 7].iter().all(|i| !got[*i].is_empty());
            if ui.add_enabled(ready, egui::Button::new("Check and add")).clicked() {
                let mut it = got.into_iter();
                let mut next = || it.next().unwrap_or_default();
                let (name, endpoint, bucket, region, prefix, ca) = (next(), next(), next(), next(), next(), next());
                let home = std::env::var("HOME").unwrap_or_default();
                let ca_pem = if ca.is_empty() { Ok(None) } else { std::fs::read_to_string(ca.replacen('~', &home, 1)).map(Some) };
                match ca_pem {
                    Ok(ca_pem) => {
                        let cmd = Cmd::CloudAdd(CloudAdd {
                            name,
                            endpoint,
                            bucket,
                            region: Some(region),
                            prefix: Some(prefix),
                            access_key: next(),
                            secret_key: next(),
                            path_style: None,
                            create: false,
                            ca_pem,
                        });
                        self.spawn(&ctx, Job::Show(CLOUD_JOB), cmd, Some("Checking the cloud storage"));
                    }
                    Err(e) => self.status = format!("Cannot read the CA certificate file {ca}: {e}"),
                }
            }
        });
    }

    // ---------------------------------------------------------- settings page

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading("Settings");
        ui.weak("This computer, and what applies to every backup set that has no setting of its own.");
        ui.add_space(4.0);
        let c = self.cfg.clone();
        let r = c.retention.clone();
        let mut v: HashMap<&str, String> = HashMap::new();
        let mut save = false;
        fn note(v: &mut HashMap<&'static str, String>, key: &'static str, typed: String, cur: &str) {
            if typed != cur {
                v.insert(key, typed);
            }
        }
        let save_button = |ui: &mut egui::Ui, v: &HashMap<&str, String>, keys: &[&str]| {
            ui.add_space(2.0);
            ui.add_enabled(keys.iter().any(|k| v.contains_key(k)), egui::Button::new("Save")).clicked()
        };
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Essentials").strong().size(15.0));
            ui.add_space(2.0);
            Self::rows(ui, "essentials", |ui| {
                ui.weak("Name");
                let text = self.fields.entry("def-name".into()).or_insert_with(|| c.name.clone());
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(text).desired_width(142.0));
                    ui.weak("what this computer is called; friends you already have keep the name they know");
                });
                note(&mut v, "name", text.trim().to_string(), &c.name);
                ui.end_row();
                let cur = mbps(c.upload_kbps);
                let hint = "your backups to friends and cloud storage, such as 0.5 or 20; 0 = unlimited";
                note(&mut v, "limit", self.rule(ui, "def-limit", "Max outgoing", cur.clone(), "MB/s", hint), &cur);
                let cur = mbps(c.incoming_kbps);
                let hint = "all your friends' backups to this computer together; 0 = unlimited";
                note(
                    &mut v,
                    "incoming",
                    self.rule(ui, "def-incoming", "Max incoming", cur.clone(), "MB/s", hint),
                    &cur,
                );
                ui.weak("Recovery kit");
                let open = self.fields.contains_key("kit-pass");
                ui.horizontal(|ui| {
                    let text = self.ov["kit"]["text"].as_str().unwrap_or("not known yet").to_string();
                    let color = if self.ov["kit"]["act"] == true { YELLOW } else { GREEN };
                    ui.colored_label(color, text);
                    if ui.button(if open { "Cancel" } else { "Export..." }).clicked() {
                        for k in ["kit-pass", "kit-again"] {
                            if open {
                                self.fields.remove(k);
                            } else {
                                self.fields.insert(k.into(), String::new());
                            }
                        }
                    }
                });
                ui.end_row();
                if open {
                    self.kit_rows(ui);
                }
                self.kit_copy_rows(ui);
            });
            save |= save_button(ui, &v, &["name", "limit", "incoming"]);
        });
        ui.add_space(4.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Friends' backups to this computer").strong().size(15.0));
            ui.add_space(2.0);
            Self::rows(ui, "friends-in", |ui| {
                ui.weak("Accept");
                ui.horizontal(|ui| {
                    let mut on = c.accept_backups;
                    if ui.checkbox(&mut on, "friends may back up to this computer").changed() {
                        self.save(&ctx, Cmd::Tune(Tune { accept_backups: Some(on), ..Default::default() }));
                    }
                    if c.accept_backups {
                        ui.weak("untick to only back up to your friends and take nothing from them");
                    } else {
                        let note = "off: this computer only backs up to its friends; what they already keep here stays";
                        ui.colored_label(PURPLE, note);
                    }
                });
                ui.end_row();
                let cur = c.default_quota_gb.to_string();
                let hint = "what a friend you pair with from now on may store here; each friend's own is on Friends";
                note(&mut v, "quota", self.rule(ui, "def-quota", "New friends get", cur.clone(), "GB", hint), &cur);
                let cur = c.min_free_gb.to_string();
                let hint = "friends' backups are refused while this disk has less free than this; 0 = no check";
                note(&mut v, "free", self.rule(ui, "def-free", "Keep free", cur.clone(), "GB", hint), &cur);
            });
            save |= save_button(ui, &v, &["quota", "free"]);
        });
        ui.add_space(4.0);
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Automatic backups").strong().size(15.0));
            ui.add_space(2.0);
            Self::rows(ui, "auto", |ui| {
                ui.weak("At login");
                if self.ov["service"].as_bool() == Some(true) {
                    ui.colored_label(GREEN, "the daemon starts by itself (the background service is installed)");
                } else {
                    ui.horizontal(|ui| {
                        ui.colored_label(YELLOW, "not started; to have it start by itself, run this in a terminal");
                        Self::command(ui, "cahoots service install");
                    });
                }
                ui.end_row();
                ui.weak("Right now");
                ui.horizontal(|ui| {
                    let paused = c.paused_until > chrono::Utc::now().timestamp();
                    ui.label(if paused { "paused" } else { "running" });
                    let (label, hours) = if paused { ("Resume", 0) } else { ("Pause", 24 * 3650) };
                    if ui.button(label).clicked() {
                        self.save(&ctx, Cmd::Tune(Tune { pause_hours: Some(hours), ..Default::default() }));
                    }
                });
                ui.end_row();
                let cur = c.backup_hours.clone();
                let hint = "hours such as 22-7; blank = any time";
                note(&mut v, "hours", self.rule(ui, "def-hours", "Only between", cur.clone(), "", hint), &cur);
                let cur = (c.min_interval_secs / 60).max(1).to_string();
                let hint = "how often a local drive gets a backup; friends and cloud storage default to 60";
                note(&mut v, "every", self.rule(ui, "def-every", "Drives every", cur.clone(), "min", hint), &cur);
                let cur = c.warn_after_days.to_string();
                let hint = "without a backup, yours or a friend's to you; 0 = never";
                note(&mut v, "warn", self.rule(ui, "def-warn", "Warn after", cur.clone(), "days", hint), &cur);
            });
            save |= save_button(ui, &v, &["hours", "every", "warn"]);
        });
        ui.add_space(4.0);
        const HISTORY: [&str; 7] = ["versions", "days", "last", "hourly", "daily", "weekly", "monthly"];
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("History kept").strong().size(15.0));
            ui.add_space(2.0);
            Self::rows(ui, "history", |ui| {
                let rows: [(&str, String, &str, &str); 7] = [
                    ("Keep", r.max_versions.to_string(), "versions", "of each file; 0 = all of them"),
                    ("Keep for", r.keep_days.to_string(), "days", "then restore points are deleted; 0 = for ever"),
                    ("Always the last", r.keep_last.to_string(), "", "restore points, whatever their age"),
                    ("Hourly", r.keep_hourly.to_string(), "hours", "one restore point an hour is kept this long"),
                    ("Daily", r.keep_daily.to_string(), "days", "one a day is kept this long"),
                    ("Weekly", r.keep_weekly.to_string(), "weeks", "one a week is kept this long"),
                    ("Monthly", r.keep_monthly.to_string(), "months", "one a month is kept this long"),
                ];
                for (key, (label, cur, unit, hint)) in HISTORY.into_iter().zip(rows) {
                    note(&mut v, key, self.rule(ui, &format!("def-{key}"), label, cur.clone(), unit, hint), &cur);
                }
            });
            save |= save_button(ui, &v, &HISTORY);
        });
        if save {
            let num = |k: &str| v.get(k).and_then(|t| t.parse::<u32>().ok());
            let whole = |k: &str| v.get(k).and_then(|t| t.parse::<u64>().ok());
            if ["name", "limit", "incoming", "hours", "warn", "quota", "free"].iter().any(|k| v.contains_key(k)) {
                self.save(
                    &ctx,
                    Cmd::Tune(Tune {
                        name: v.get("name").cloned(),
                        upload_kbps: v.get("limit").and_then(|t| parse_mbps(t)),
                        incoming_kbps: v.get("incoming").and_then(|t| parse_mbps(t)),
                        hours: v.get("hours").cloned(),
                        warn_days: num("warn"),
                        default_quota_gb: whole("quota"),
                        min_free_gb: whole("free"),
                        ..Default::default()
                    }),
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
        ui.add_space(8.0);
        self.exclude_lists(ui);
    }

    /// A new passphrase, typed twice, with a count of what has been typed, and the button that
    /// goes ahead with it. Returns the passphrase when that button is clicked.
    fn passphrase_rows(&mut self, ui: &mut egui::Ui, key: &str, button: &str, note: &str) -> Option<String> {
        let (first, second) = (format!("{key}-pass"), format!("{key}-again"));
        let mut pass = self.fields.get(&first).cloned().unwrap_or_default();
        let mut again = self.fields.get(&second).cloned().unwrap_or_default();
        ui.weak("Passphrase");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut pass).password(true).desired_width(200.0));
            let (n, min) = (pass.chars().count(), crate::recovery::MIN_PASSPHRASE);
            if n < min {
                ui.colored_label(YELLOW, format!("{n} of at least {min} characters"));
            } else {
                ui.colored_label(GREEN, format!("{n} characters"));
            }
            ui.weak("a sentence you will remember works well; only its length is checked");
        });
        ui.end_row();
        ui.weak("Again");
        let mut go = false;
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut again).password(true).desired_width(200.0));
            let ok = crate::recovery::check_passphrase(&pass).is_ok() && pass == again;
            go = ui.add_enabled(ok, egui::Button::new(button)).clicked();
            if !again.is_empty() && pass != again {
                ui.colored_label(YELLOW, "the two do not match");
            } else {
                ui.weak(note);
            }
        });
        ui.end_row();
        self.fields.insert(first, pass.clone());
        self.fields.insert(second, again);
        go.then_some(pass)
    }

    /// Asks for a passphrase and where to save, and writes the recovery kit there.
    fn kit_rows(&mut self, ui: &mut egui::Ui) {
        let note = "keep the file somewhere other than this computer";
        let Some(pass) = self.passphrase_rows(ui, "kit", "Save the kit as...", note) else { return };
        let folder = crate::recovery::default_folder().or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let picked = rfd::FileDialog::new()
            .set_title("Save the recovery kit")
            .set_directory(folder.unwrap_or_else(|| PathBuf::from("/")))
            .set_file_name(crate::recovery::file_name(&self.cfg.name))
            .save_file();
        if let Some(file) = picked {
            self.status = match crate::recovery::export(&self.home, &pass, &file) {
                Ok(()) => format!(
                    "Recovery kit written to {}. Keep it and its passphrase somewhere other than this computer.",
                    file.display()
                ),
                Err(e) => format!("{e:#}"),
            };
            self.fields.remove("kit-pass");
            self.fields.remove("kit-again");
            self.last_poll = Instant::now() - Duration::from_secs(10);
        }
    }

    /// The optional copy of the recovery kit in a folder, such as one a cloud drive syncs:
    /// where it is, and the buttons to start it, write it again or stop it.
    fn kit_copy_rows(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let c = self.ov["kit_copy"].clone();
        let open = self.fields.contains_key("kc-pass");
        ui.weak("Kept in");
        ui.horizontal(|ui| {
            if let Some(folder) = c["folder"].as_str() {
                // Short enough to leave room for the buttons; the whole path shows on hover.
                let home = std::env::var("HOME").unwrap_or_default();
                let mut short = match folder.strip_prefix(&home) {
                    Some(rest) if !home.is_empty() => format!("~{rest}"),
                    _ => folder.to_string(),
                };
                if short.chars().count() > 56 {
                    let tail: String = short.chars().rev().take(52).collect::<Vec<_>>().into_iter().rev().collect();
                    short = format!("...{tail}");
                }
                let said = match c["error"].as_str() {
                    Some(e) => ui.colored_label(YELLOW, format!("{short}, but the copy is failing: {e}")),
                    None => ui.colored_label(GREEN, format!("{short} · last written {}", ago(c["at"].as_i64()))),
                };
                said.on_hover_text(folder);
                if ui.button("Write now").clicked() {
                    self.save(&ctx, Cmd::KitCopySync);
                }
                if self.confirm(ui, "kit-copy-off", "Stop", "What is in the folder stays there") {
                    self.save(&ctx, Cmd::KitCopyOff);
                }
            } else {
                ui.weak(if cfg!(target_os = "macos") {
                    "optional: a folder that syncs, such as Documents with iCloud Drive; cahoots keeps the kit there up to date"
                } else {
                    "optional: a folder that a cloud drive syncs; cahoots keeps the kit there up to date"
                });
                if ui.button(if open { "Cancel" } else { "Keep a copy..." }).clicked() {
                    for k in ["kc-pass", "kc-again"] {
                        if open {
                            self.fields.remove(k);
                        } else {
                            self.fields.insert(k.into(), String::new());
                        }
                    }
                }
            }
        });
        ui.end_row();
        if open && c["folder"].is_null() {
            let note = "recovering takes the file in that folder and this passphrase";
            let Some(pass) = self.passphrase_rows(ui, "kc", "Choose the folder...", note) else { return };
            let mut dialog = rfd::FileDialog::new().set_title("Keep a copy of the recovery kit in");
            if let Some(docs) = crate::recovery::default_folder() {
                dialog = dialog.set_directory(docs);
            }
            let picked = dialog.pick_folder();
            if let Some(folder) = picked {
                self.fields.remove("kc-pass");
                self.fields.remove("kc-again");
                self.save(&ctx, Cmd::KitCopyOn { passphrase: Secret(pass), folder });
            }
        }
    }

    /// The exclusion lists: the built-in ones to read and copy, your own to change.
    fn exclude_lists(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new("Exclusion lists").strong().size(15.0));
            ui.weak(
                "Each backup set leaves out what one of these lists names; choose it on the set's Files tab. \
                 Leaving something out does not remove it from restore points made so far.",
            );
            ui.add_space(2.0);
            for l in self.cfg.lists() {
                let builtin = is_builtin_list(&l.name);
                let users: Vec<&str> = self
                    .cfg
                    .sets
                    .iter()
                    .filter(|b| self.cfg.list_for(b).name == l.name)
                    .map(|b| b.name.as_str())
                    .collect();
                let title = format!(
                    "{}   ({}, {} patterns, {})",
                    l.name,
                    if builtin { "built in" } else { "yours" },
                    l.patterns.len(),
                    if users.is_empty() { "not used".to_string() } else { format!("used by {}", users.join(", ")) }
                );
                egui::CollapsingHeader::new(title).id_salt(format!("exlist-{}", l.name)).show(ui, |ui| {
                    for p in &l.patterns {
                        ui.horizontal(|ui| {
                            if !builtin && ui.small_button("remove").clicked() {
                                let (list, pattern) = (l.name.clone(), p.clone());
                                self.save(&ctx, Cmd::ExcludePattern { list, pattern, remove: true });
                            }
                            ui.monospace(p);
                        });
                    }
                    ui.horizontal(|ui| {
                        if !builtin {
                            let text = self.fields.entry(format!("expat-{}", l.name)).or_default();
                            ui.add(
                                egui::TextEdit::singleline(text)
                                    .desired_width(220.0)
                                    .hint_text("*.iso, build, /a/path"),
                            );
                            let pattern = text.trim().to_string();
                            if ui.add_enabled(!pattern.is_empty(), egui::Button::new("Add pattern")).clicked() {
                                self.fields.remove(&format!("expat-{}", l.name));
                                self.save(&ctx, Cmd::ExcludePattern { list: l.name.clone(), pattern, remove: false });
                            }
                            ui.separator();
                        }
                        let text = self.fields.entry(format!("excopy-{}", l.name)).or_default();
                        ui.add(egui::TextEdit::singleline(text).desired_width(140.0).hint_text("name for a copy"));
                        let name = text.trim().to_string();
                        if ui.add_enabled(!name.is_empty(), egui::Button::new("Copy to a new list")).clicked() {
                            self.fields.remove(&format!("excopy-{}", l.name));
                            self.save(&ctx, Cmd::ExcludeNew { name, from: Some(l.name.clone()) });
                        }
                        if !builtin {
                            ui.separator();
                            if self.confirm(ui, &format!("exdel-{}", l.name), "Delete list", "") {
                                self.save(&ctx, Cmd::ExcludeDelete { name: l.name.clone() });
                            }
                        }
                    });
                });
            }
            ui.add_space(4.0);
            // A list of your own from nothing, or from one of the lists above.
            Self::rows(ui, "new-exlist", |ui| {
                ui.weak("New list");
                ui.horizontal(|ui| {
                    let text = self.fields.entry("exnew".into()).or_default();
                    ui.add(egui::TextEdit::singleline(text).desired_width(160.0).hint_text("name"));
                    let name = text.trim().to_string();
                    ui.label("starting");
                    let from = self.fields.entry("exnew-from".into()).or_default().clone();
                    let mut picked = from.clone();
                    let shown = if from.is_empty() { "empty".to_string() } else { format!("as a copy of {from}") };
                    egui::ComboBox::from_id_salt("exnew-from").selected_text(shown).show_ui(ui, |ui| {
                        ui.selectable_value(&mut picked, String::new(), "empty");
                        for l in self.cfg.lists() {
                            ui.selectable_value(&mut picked, l.name.clone(), format!("as a copy of {}", l.name));
                        }
                    });
                    if picked != from {
                        self.fields.insert("exnew-from".into(), picked.clone());
                    }
                    if ui.add_enabled(!name.is_empty(), egui::Button::new("Create")).clicked() {
                        self.fields.remove("exnew");
                        self.fields.remove("exnew-from");
                        self.save(&ctx, Cmd::ExcludeNew { name, from: Some(picked).filter(|f| !f.is_empty()) });
                    }
                });
                ui.end_row();
            });
        });
    }

    /// A command to run in a terminal, with a button that copies it.
    fn command(ui: &mut egui::Ui, text: &str) {
        ui.label(RichText::new(text).monospace());
        if ui.small_button("Copy").on_hover_text("Copy the command").clicked() {
            ui.ctx().copy_text(text.to_string());
        }
    }

    /// A card on the Status page: a title and what goes under it.
    fn status_card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
        ui.group(|ui| {
            ui.set_min_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(15.0));
            ui.add_space(2.0);
            add(ui);
        });
        ui.add_space(4.0);
    }

    /// A table on the Status page: weak column titles, then the rows `add` draws.
    fn status_table(ui: &mut egui::Ui, id: &str, titles: &[&str], add: impl FnOnce(&mut egui::Ui)) {
        egui::Grid::new(id).num_columns(titles.len()).spacing([22.0, 7.0]).show(ui, |ui| {
            for t in titles {
                ui.weak(*t);
            }
            ui.end_row();
            add(ui);
        });
    }

    /// How this computer and its backups stand: what needs attention, the computer itself,
    /// each backup set, each place backups go and what friends keep here.
    fn status_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if self.text_at.elapsed() > Duration::from_secs(5) {
            self.text_at = Instant::now();
            self.spawn(&ctx, Job::Text(Page::Status), Cmd::Status, None);
        }
        ui.heading("Status");
        ui.weak("How this computer and its backups stand right now.");
        ui.add_space(4.0);
        let (ov, c) = (self.ov.clone(), self.cfg.clone());
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            Self::status_card(ui, "Needs attention", |ui| {
                let mut any = self.warnings(ui) > 0;
                if let Some(h) = ov["held"].as_str() {
                    ui.colored_label(YELLOW, format!("Automatic backups are held: {h}"));
                    any = true;
                }
                if !any {
                    ui.colored_label(GREEN, "Nothing. Backups are running as they should.");
                }
            });
            Self::status_card(ui, "This computer", |ui| {
                Self::rows(ui, "st-this", |ui| {
                    ui.weak("Name");
                    ui.label(format!("{} · cahoots {}", c.name, ov["version"].as_str().unwrap_or(crate::net::VERSION)));
                    ui.end_row();
                    ui.weak("Id");
                    ui.horizontal(|ui| {
                        let id = ov["id"].as_str().unwrap_or_default();
                        let short: String = id.chars().take(16).collect();
                        ui.label(RichText::new(format!("{short}...")).monospace()).on_hover_text(id);
                        if ui.small_button("Copy").clicked() {
                            ctx.copy_text(id.to_string());
                        }
                        ui.weak("what friends know this computer by");
                    });
                    ui.end_row();
                    ui.weak("Daemon");
                    ui.horizontal(|ui| {
                        ui.colored_label(GREEN, "running");
                        if ov["container"] == true {
                            ui.weak("· the container starts it");
                        } else if ov["service"] == true {
                            ui.weak("· starts by itself at login");
                        } else {
                            ui.colored_label(YELLOW, "· not started at login: run this in a terminal");
                            Self::command(ui, "cahoots service install");
                        }
                    });
                    ui.end_row();
                    ui.weak("Reachable at");
                    ui.horizontal(|ui| {
                        let addrs: Vec<&str> =
                            ov["addrs"].as_array().into_iter().flatten().filter_map(|a| a.as_str()).collect();
                        ui.label(if addrs.is_empty() { "not known yet".to_string() } else { addrs.join(", ") });
                        if let Some(r) = ov["relay"].as_str() {
                            let host = r.trim_start_matches("https://").trim_end_matches('/').trim_end_matches('.');
                            ui.weak(format!("· relay {host}"));
                        }
                    });
                    ui.end_row();
                    ui.weak("Recovery kit");
                    ui.horizontal(|ui| {
                        let color = if ov["kit"]["act"] == true { YELLOW } else { GREEN };
                        ui.colored_label(color, ov["kit"]["text"].as_str().unwrap_or("not known yet"));
                        if let Some(f) = ov["kit_copy"]["folder"].as_str() {
                            ui.weak(format!("· kept in {}", tilde(f)));
                        }
                    });
                    ui.end_row();
                    ui.weak("Friends' backups");
                    ui.horizontal(|ui| {
                        if c.accept_backups {
                            ui.label("accepted");
                        } else {
                            ui.colored_label(PURPLE, "not accepted: this computer only backs up to its friends");
                        }
                        match ov["turned_away"].as_u64() {
                            Some(n) if n > 0 => ui.weak(format!("· {n} strangers turned away")),
                            _ => ui.weak("· no strangers turned away"),
                        };
                    });
                    ui.end_row();
                    ui.weak("Speed");
                    let limit =
                        |kbps: u64| if kbps == 0 { "unlimited".to_string() } else { format!("{} MB/s", mbps(kbps)) };
                    ui.label(format!("outgoing {} · incoming {}", limit(c.upload_kbps), limit(c.incoming_kbps)));
                    ui.end_row();
                    ui.weak("History kept");
                    let r = &c.retention;
                    ui.label(format!(
                        "last {} · hourly {} · daily {} · weekly {} · monthly {} · {}",
                        r.keep_last,
                        r.keep_hourly,
                        r.keep_daily,
                        r.keep_weekly,
                        r.keep_monthly,
                        if r.keep_days == 0 {
                            "no age limit".to_string()
                        } else {
                            format!("{} days at most", r.keep_days)
                        }
                    ));
                    ui.end_row();
                });
            });
            Self::status_card(ui, "Backup sets", |ui| {
                let sets: Vec<Value> = ov["sets"].as_array().cloned().unwrap_or_default();
                if sets.is_empty() {
                    ui.weak("No backup sets yet. Make one on the Backup page.");
                    return;
                }
                Self::status_table(ui, "st-sets", &["Set", "Size", "Last backup", "Goes to"], |ui| {
                    for o in &sets {
                        ui.label(RichText::new(o["name"].as_str().unwrap_or_default()).strong());
                        ui.label(match o["files"].as_u64() {
                            Some(n) => format!("{n} files, {}", human(o["bytes"].as_u64().unwrap_or(0))),
                            None => "not backed up yet".to_string(),
                        });
                        match o["running"].as_str() {
                            Some(r) => ui.colored_label(BLUE, format!("backing up: {r}")),
                            None => ui.label(ago(o["last"].as_i64())),
                        };
                        ui.horizontal(|ui| {
                            let to: Vec<&str> = o["dests"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|d| d["name"].as_str())
                                .collect();
                            if to.is_empty() {
                                ui.colored_label(YELLOW, "nowhere");
                            }
                            for name in to {
                                Self::dot(ui, self.dest_dot(name));
                                ui.label(name);
                                ui.add_space(6.0);
                            }
                        });
                        ui.end_row();
                    }
                });
            });
            let dests: Vec<Value> = ov["dests"].as_array().cloned().unwrap_or_default();
            Self::status_card(ui, "Where your backups go", |ui| {
                if dests.is_empty() {
                    ui.weak("Nowhere yet. Invite a friend or add a drive on the Friends page.");
                    return;
                }
                Self::status_table(ui, "st-dests", &["", "How it is reached", "You use", "Last sent"], |ui| {
                    for d in &dests {
                        let name = d["name"].as_str().unwrap_or_default();
                        ui.horizontal(|ui| {
                            Self::dot(ui, self.dest_dot(name));
                            ui.label(RichText::new(name).strong());
                            if d["in_use"] == false {
                                ui.colored_label(YELLOW, "not in use");
                            }
                        });
                        let mut how = vec![d["kind"].as_str().unwrap_or_default().to_string()];
                        how.push(d["state"].as_str().unwrap_or("checking").to_string());
                        how.extend(d["link"].as_str().map(String::from));
                        if d["closed"] == true {
                            how.push("takes no backups from you".into());
                        }
                        ui.label(how.join(" · "));
                        ui.label(match (d["used_there"].as_u64(), d["quota_there"].as_u64()) {
                            (Some(u), Some(q)) => format!("{} of {}", human(u), human(q)),
                            (Some(u), None) => human(u),
                            _ => "not known yet".to_string(),
                        });
                        ui.label(ago(d["last_out"].as_i64()));
                        ui.end_row();
                    }
                });
            });
            let friends: Vec<&Value> = dests.iter().filter(|d| d["kind"] == "friend").collect();
            if !friends.is_empty() {
                Self::status_card(ui, "What friends keep on this computer", |ui| {
                    Self::status_table(ui, "st-here", &["Friend", "They use", "Last backup from them"], |ui| {
                        for d in &friends {
                            ui.label(RichText::new(d["name"].as_str().unwrap_or_default()).strong());
                            ui.label(format!(
                                "{} of {} GB",
                                human(d["used_here"].as_u64().unwrap_or(0)),
                                d["quota_gb"].as_u64().unwrap_or(0)
                            ));
                            ui.label(ago(d["last_in"].as_i64()));
                            ui.end_row();
                        }
                    });
                });
            }
            egui::CollapsingHeader::new("The full report, as `cahoots status` prints it").id_salt("st-full").show(
                ui,
                |ui| {
                    if ui.button("Copy").clicked() {
                        ctx.copy_text(self.text.join("\n"));
                    }
                    for l in &self.text {
                        ui.monospace(l);
                    }
                },
            );
        });
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
    /// For development: CAHOOTS_GUI_DRAG=x1,y1,x2,y2 drags the pointer from one point to the other
    /// before the picture CAHOOTS_GUI_SHOT takes, to try things that only a drag shows.
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if !cfg!(debug_assertions) {
            return;
        }
        let Some(spec) = crate::config::env_text("GUI_DRAG") else { return };
        let n: Vec<f32> = spec.split(',').filter_map(|v| v.trim().parse().ok()).collect();
        let [x1, y1, x2, y2] = n[..] else { return };
        let (from, to) = (egui::pos2(x1, y1), egui::pos2(x2, y2));
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        match self.frames {
            4 => raw.events.push(egui::Event::PointerMoved(from)),
            5 => raw.events.push(button(from, true)),
            6..=12 => raw.events.push(egui::Event::PointerMoved(from + (to - from) * ((self.frames - 5) as f32 / 7.0))),
            13 => raw.events.push(button(to, false)),
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.receive(&ctx);
        self.poll(&ctx);
        self.maybe_screenshot(&ctx);
        ctx.request_repaint_after(Duration::from_secs(1));

        egui::Panel::left("menu").default_size(150.0).size_range(120.0..=360.0).show(ui, |ui| {
            // A panel only keeps the width it is dragged to if what is in it uses that width.
            ui.take_available_width();
            ui.add_space(6.0);
            ui.label(RichText::new("Cahoots Backup").strong());
            ui.weak(format!(
                "{} · cahoots {}",
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
        egui::Panel::bottom("status").resizable(false).show(ui, |ui| {
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
        // What a command answered sits under the page it was started from, where it can be
        // read, selected and copied without covering anything.
        if let Some((title, lines)) = self.popup.clone() {
            // Bottom panels have no drag handle unless asked for one.
            egui::Panel::bottom("result").resizable(true).default_size(230.0).size_range(90.0..=900.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(&title);
                    if ui.button("Copy").clicked() {
                        ctx.copy_text(lines.join("\n"));
                        self.status = "Copied.".into();
                    }
                    if ui.button("Close").clicked() {
                        self.popup = None;
                    }
                });
                // Filling the panel, whatever the length of the text, is what lets its edge be dragged.
                egui::ScrollArea::vertical().id_salt("result").auto_shrink(false).show(ui, |ui| {
                    // A text box so any part can be selected; what is typed into it is not kept.
                    let mut text = lines.join("\n");
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                });
            });
        }
        egui::CentralPanel::default().show(ui, |ui| match self.page {
            Page::Backup => self.backup_page(ui),
            Page::Restore => self.restore_page(ui),
            Page::Destinations => self.dests_page(ui),
            Page::Settings => {
                egui::ScrollArea::vertical().show(ui, |ui| self.settings_page(ui));
            }
            Page::History => self.text_page(ui, "History"),
            Page::Status => self.status_page(ui),
        });
    }
}

pub fn run(home: PathBuf) -> anyhow::Result<()> {
    // For development: CAHOOTS_GUI_TALL makes the window tall enough to picture a long page whole.
    let height = if cfg!(debug_assertions) && crate::config::env("GUI_TALL").is_some() { 1500.0 } else { 700.0 };
    let mut viewport = egui::ViewportBuilder::default().with_inner_size([1080.0, height]).with_title("Cahoots Backup");
    // The window's icon, which on a Mac is also the one in the Dock.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let mut gui = Gui::new(home);
    gui.first_load();
    eframe::run_native("Cahoots Backup", options, Box::new(|_cc| Ok(Box::new(gui)))).map_err(|e| {
        anyhow::anyhow!(
            "the window could not be opened (is there a desktop session? `cahoots tui` works without one): {e}"
        )
    })
}
