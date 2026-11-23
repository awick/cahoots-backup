// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::{
    backup,
    config::{
        BackupSet, Cloud, Config, DEFAULT_SET, EVERYWHERE, ExcludeList, Friend, Identity, Invite, NOWHERE, Retention,
        STANDARD, VersionRule, is_builtin_list, write_private,
    },
    crypto::{Id, Keys},
    net::{self, Handler, Link, Peer},
    proto::{ALPN, Req},
    restore,
    snapshot::{self, Entry, Kind, Manifest, human, when},
    store::Store,
};
use anyhow::{Context, Result, anyhow, bail};
use iroh::{Endpoint, PublicKey, SecretKey, endpoint::Connection, protocol::Router};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc};

mod commands;

/// Most missing chunks a sampled verify copies back to one destination by itself.
const MAX_SAMPLED_REPAIR: usize = 256;
/// Most unneeded chunk ids one clean-up takes in from a destination (32 MiB of ids).
const MAX_DEAD: usize = 1 << 20;
/// Most listed chunks outside this node's latest indexes it remembers as already stored.
const KNOWN_OTHERS: usize = 1 << 20;

#[derive(Clone)]
pub struct Out(pub mpsc::UnboundedSender<String>);

impl Out {
    pub fn say(&self, s: impl Into<String>) {
        let _ = self.0.send(clean(&s.into()));
    }
}

/// Text for a terminal or the one-line-per-message control socket: names of files and
/// words from other nodes may hold line breaks or escape codes, which become `?`.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// A change to the general settings; what is left unset stays as it is.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Tune {
    pub name: Option<String>,
    pub upload_kbps: Option<u64>,
    pub incoming_kbps: Option<u64>,
    pub hours: Option<String>,
    pub pause_hours: Option<i64>,
    pub warn_days: Option<u32>,
    pub accept_backups: Option<bool>,
    pub default_quota_gb: Option<u64>,
    pub min_free_gb: Option<u64>,
}

/// A change to one backup set; what is left unset stays as it is.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SetEdit {
    pub name: String,
    pub to: Option<Vec<String>>,
    pub every: Option<u32>,
    pub hours: Option<String>,
    pub days: Option<u32>,
    pub versions: Option<u32>,
    pub last: Option<u32>,
    pub delete: bool,
}

/// Cloud storage to add as a destination, or new details for one already added.
#[derive(Serialize, Deserialize, Clone)]
pub struct CloudAdd {
    pub name: String,
    pub endpoint: String,
    pub bucket: String,
    pub region: Option<String>,
    pub prefix: Option<String>,
    pub access_key: String,
    pub secret_key: String,
    /// How the bucket is addressed; None tries `bucket.endpoint` and then `endpoint/bucket`.
    pub path_style: Option<bool>,
    pub create: bool,
    /// Certificates to trust for this storage instead of the usual ones (PEM text).
    #[serde(default)]
    pub ca_pem: Option<String>,
}

/// Something typed in secret, such as a passphrase, on its way to the daemon.
#[derive(Serialize, Deserialize, Clone)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl std::fmt::Debug for CloudAdd {
    /// The secret key stays out of anything that prints a command.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CloudAdd {{ name: {:?}, endpoint: {:?}, bucket: {:?}, .. }}", self.name, self.endpoint, self.bucket)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Cmd {
    Backup {
        tag: Option<String>,
        set: Option<String>,
    },
    Snapshots,
    Ls {
        snap: String,
        path: Option<String>,
    },
    Diff {
        a: String,
        b: String,
    },
    History {
        path: String,
    },
    Restore {
        snap: String,
        paths: Vec<String>,
        to: PathBuf,
        from: Option<String>,
        /// Look only among this set's restore points, so `latest` is that set's newest.
        #[serde(default)]
        set: Option<String>,
    },
    Prune {
        dry_run: bool,
        /// Also take what the exclusion lists now leave out from older restore points.
        #[serde(default)]
        apply_exclusions: bool,
        /// ... of this set only.
        #[serde(default)]
        set: Option<String>,
    },
    Forget {
        snap: String,
    },
    Verify {
        full: bool,
        local: bool,
        /// Only this backup set (None = all of them).
        set: Option<String>,
    },
    Scrub,
    Status,
    Invite,
    Join {
        ticket: String,
    },
    Friends,
    FriendRemove {
        name: String,
        purge: bool,
    },
    FriendLimit {
        name: String,
        kbps: u64,
    },
    FriendQuota {
        name: String,
        gb: u64,
    },
    FriendBackupTo {
        name: String,
        on: bool,
    },
    AddPaths {
        paths: Vec<PathBuf>,
        set: Option<String>,
    },
    Sets,
    SetRename {
        name: String,
        to: String,
    },
    SetEdit(SetEdit),
    RemovePath {
        path: PathBuf,
    },
    /// Leave a folder or file out of the set that holds it, or put it back.
    Skip {
        path: PathBuf,
        set: Option<String>,
        remove: bool,
    },
    /// Which exclusion list a set uses.
    SetExclude {
        name: String,
        list: String,
    },
    ExcludeLists,
    /// A new exclusion list: a copy of `from`, or empty.
    ExcludeNew {
        name: String,
        from: Option<String>,
    },
    ExcludeDelete {
        name: String,
    },
    ExcludePattern {
        list: String,
        pattern: String,
        remove: bool,
    },
    Keep {
        set: Vec<(String, u32)>,
        versions: Option<u32>,
        pattern: Option<String>,
    },
    Config,
    /// Everything the home screen shows, as one JSON line; never waits on the network.
    Overview,
    /// Recent activity lines.
    Log,
    /// The running daemon's build fingerprint.
    Hello,
    /// Ask the daemon to exit.
    Stop,
    Tune(Tune),
    /// Keep a copy of the recovery kit in a folder, written again whenever it changes.
    KitCopyOn {
        passphrase: Secret,
        folder: PathBuf,
    },
    KitCopyOff,
    /// Write the copy now.
    KitCopySync,
    DriveAdd {
        name: String,
        path: PathBuf,
    },
    /// Add cloud storage as a destination, or give an existing one new details.
    CloudAdd(CloudAdd),
    /// Remove from one destination what the sets no longer sent there left behind.
    DestClean {
        name: String,
        dry_run: bool,
    },
    /// History rules for what is kept at one destination.
    DestKeep {
        name: String,
        set: Vec<(String, u32)>,
        /// Back to each set's own rules.
        clear: bool,
    },
    Every {
        name: Option<String>,
        minutes: u32,
    },
    SnapList {
        from: Option<String>,
    },
    Tree {
        snap: String,
        from: Option<String>,
    },
    Versions {
        path: String,
        from: Option<String>,
    },
}

impl Cmd {
    /// Carries on to the end even if the client that asked for it goes away.
    pub fn detached(&self) -> bool {
        matches!(
            self,
            Cmd::Backup { .. } | Cmd::Prune { .. } | Cmd::Forget { .. } | Cmd::DestClean { dry_run: false, .. }
        )
    }

    /// Changes what destinations hold or how records are filed, so it waits its turn behind any
    /// other such job. A backup is one too, but takes its turn inside `backup::run`, which the
    /// daemon's own automatic backups also go through.
    fn exclusive(&self) -> bool {
        matches!(self, Cmd::Prune { .. } | Cmd::Forget { .. } | Cmd::DestClean { .. } | Cmd::SetRename { .. })
    }
}

#[derive(Serialize, Deserialize)]
struct Ticket {
    id: [u8; 32],
    relay: Option<String>,
    addrs: Vec<SocketAddr>,
    token: [u8; 16],
}

pub struct Snap {
    pub id: String,
    pub m: Manifest,
    pub peers: Vec<(Friend, Vec<Id>)>,
}

impl Snap {
    pub fn friends(&self) -> Vec<Friend> {
        // Whoever holds exactly this version of the index goes first; local drives before friends,
        // and cloud storage last, since reading from it usually costs money.
        let mut v: Vec<_> = self.peers.iter().collect();
        v.sort_by_key(|(f, idx)| (*idx != self.m.index, f.path.is_none(), f.cloud.is_some()));
        v.into_iter().map(|(f, _)| f.clone()).collect()
    }
}

/// For each destination: when its chunk list was last fetched, and the list.
type KnownChunks = HashMap<String, (Instant, Arc<HashSet<Id>>)>;

/// Which chunks each restore point's files use, for working out what a destination still
/// needs. Restore points share most of their chunks, so each is kept as a list of small
/// numbers into one table of ids rather than as its whole file list: pruning a long history
/// of a large folder then takes megabytes, not gigabytes.
#[derive(Default)]
struct Uses {
    ids: Vec<Id>,
    number: HashMap<Id, u32>,
    by_snap: HashMap<String, Vec<u32>>,
}

impl Uses {
    fn knows(&self, snap: &str) -> bool {
        self.by_snap.contains_key(snap)
    }

    /// Records the chunks of a restore point's files, replacing what was recorded before.
    fn set(&mut self, snap: &str, entries: &[Entry]) {
        let mut mine: Vec<u32> = vec![];
        for e in entries {
            let Kind::File { chunks, .. } = &e.kind else { continue };
            for id in chunks {
                let next = self.ids.len() as u32;
                let n = *self.number.entry(*id).or_insert(next);
                if n == next {
                    self.ids.push(*id);
                }
                mine.push(n);
            }
        }
        mine.sort_unstable();
        mine.dedup();
        mine.shrink_to_fit();
        self.by_snap.insert(snap.to_string(), mine);
    }

    fn add_to(&self, snap: &str, live: &mut HashSet<Id>) {
        for n in self.by_snap.get(snap).into_iter().flatten() {
            live.insert(self.ids[*n as usize]);
        }
    }
}

/// Result of checking on one destination in the background.
#[derive(Clone)]
struct Probe {
    at: i64,
    ok: bool,
    used: u64,
    quota: u64,
    link: String,
    version: Option<String>,
}

pub struct Node {
    me: std::sync::Weak<Node>,
    pub home: PathBuf,
    cfg: Mutex<Config>,
    pub keys: Arc<Keys>,
    pub endpoint: Endpoint,
    pub store: Store,
    known: tokio::sync::Mutex<KnownChunks>,
    conns: Mutex<HashMap<String, Connection>>,
    drives: Mutex<HashMap<PathBuf, Arc<Store>>>,
    clouds: Mutex<HashMap<String, Arc<crate::cloud::Bucket>>>,
    down: Mutex<HashMap<String, Instant>>,
    /// Defences for the listening side: counters and limits for unknown and known callers.
    pub guard: net::Guard,
    /// What each backup set is doing right now, for the home screen.
    running: Mutex<HashMap<String, String>>,
    /// Recent activity, newest last.
    log: Mutex<std::collections::VecDeque<String>>,
    /// The last reachability check of each destination.
    probes: Mutex<HashMap<String, Probe>>,
    /// Friends last found to take no backups from this node.
    closed: Mutex<HashSet<String>>,
    last_probe: Mutex<Option<Instant>>,
    pub quit: Notify,
    /// When the daemon next intends to back up each set to each destination, keyed "set|id".
    /// Absent means nothing is waiting to be sent; `i64::MAX` means held (paused or outside its hours).
    pub next: Mutex<HashMap<String, i64>>,
    pub op: tokio::sync::Mutex<()>,
    /// "destination|id" of deleted restore points a destination still offered and was asked
    /// again to drop during this run; it is not asked twice.
    asked_gone: Mutex<HashSet<String>>,
    /// Destinations with such a request still going.
    deleting: Mutex<HashSet<String>>,
    paired: Notify,
    pub cfg_changed: Notify,
    pub daemon: bool,
    pub watching: bool,
    pub last_backup: Mutex<Option<String>>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn clean_name(name: &str) -> String {
    let s: String = name.chars().filter(|c| c.is_alphanumeric() || "-_.".contains(*c)).take(32).collect();
    if s.is_empty() { "friend".into() } else { s }
}

impl Node {
    /// One of this node's own exclusion lists, by its name in any case.
    fn own_list(&self, name: &str) -> Result<String> {
        let c = self.cfg();
        let l = c.exclude_lists.iter().find(|l| l.name.eq_ignore_ascii_case(name.trim()));
        Ok(l.ok_or_else(|| anyhow!("no exclusion list named {name}; see `cahoots exclude lists`"))?.name.clone())
    }
}

fn refuse_reserved(name: &str) -> Result<()> {
    if crate::config::reserved(name) {
        bail!("\"{name}\" has a special meaning in destination lists; choose another name");
    }
    Ok(())
}

fn add_friend(c: &mut Config, id: &str, name: &str, relay: Option<String>, addrs: Vec<SocketAddr>) -> String {
    if let Some(f) = c.friends.iter_mut().find(|f| f.id == id) {
        f.relay = relay;
        f.addrs = addrs;
        return f.name.clone();
    }
    // The name is the friend's choice, so a reserved word quietly becomes a plain one.
    let base = Some(clean_name(name)).filter(|b| !crate::config::reserved(b)).unwrap_or_else(|| "friend".into());
    let mut name = base.clone();
    let mut n = 2;
    while c.friends.iter().any(|f| f.name == name) {
        name = format!("{base}{n}");
        n += 1;
    }
    c.friends.push(Friend {
        name: name.clone(),
        id: id.into(),
        quota_gb: c.default_quota_gb,
        backup_to: true,
        relay,
        addrs,
        every_minutes: 0,
        max_in_kbps: 0,
        path: None,
        drive_key: None,
        cloud: None,
        retention: None,
    });
    name
}

/// Direct addresses from an invite or a friend, at most a few, and outside offline tests only
/// public ones: a code from someone else must not aim our packets into our own network. The
/// relay still finds a friend on the same network.
fn dialable(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let offline = net::offline();
    addrs.iter().filter(|a| a.port() != 0 && (offline || net::public_ip(a.ip()))).take(8).copied().collect()
}

/// A drive's marker. Neither the folder nor the marker is opened through a link, a pipe cannot
/// block the open, and type and size come from the open file, so only a small regular file is read.
fn read_mark(dir: &Path) -> Option<String> {
    use std::io::Read;
    let f = crate::os::Dir::open(dir, false).ok()?.open_file("sdb-drive-id".as_ref()).ok()?;
    let md = f.metadata().ok()?;
    if !md.is_file() || md.len() > 128 {
        return None;
    }
    let mut s = String::new();
    f.take(128).read_to_string(&mut s).ok()?;
    Some(s)
}

fn sig(chunks: &[Id], size: u64) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    h.update(chunks.as_flattened());
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap())
}

fn under(path: &Path, filters: &[String]) -> bool {
    filters.is_empty() || filters.iter().any(|f| path.starts_with(f))
}

impl Node {
    pub async fn start(home: PathBuf, daemon: bool, watching: bool) -> Result<(Arc<Node>, Router)> {
        let cfg = Config::load(&home)?;
        let ident = Identity::load(&home)?;
        let secret = SecretKey::from_bytes(&Identity::bytes(&ident.secret)?);
        let endpoint = net::bind(secret, cfg.port, cfg.upnp).await?;
        let master = Identity::bytes(&ident.master)?;
        let node = Arc::new_cyclic(|me| Node {
            me: me.clone(),
            store: Store::new(cfg.store_dir(&home), cfg.min_free_gb.saturating_mul(1 << 30)),
            keys: Arc::new(Keys::new(&master)),
            home,
            cfg: Mutex::new(cfg),
            endpoint: endpoint.clone(),
            known: Default::default(),
            conns: Default::default(),
            drives: Default::default(),
            clouds: Default::default(),
            down: Default::default(),
            guard: Default::default(),
            running: Default::default(),
            log: Default::default(),
            probes: Default::default(),
            closed: Default::default(),
            last_probe: Mutex::new(None),
            quit: Notify::new(),
            next: Default::default(),
            op: Default::default(),
            asked_gone: Default::default(),
            deleting: Default::default(),
            paired: Notify::new(),
            cfg_changed: Notify::new(),
            daemon,
            watching,
            last_backup: Mutex::new(None),
        });
        let router = Router::builder(endpoint).accept(ALPN, Handler(node.clone())).spawn();
        Ok((node, router))
    }

    pub fn cfg(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }

    fn update<R>(&self, f: impl FnOnce(&mut Config) -> R) -> Result<R> {
        let mut c = self.cfg.lock().unwrap();
        let r = f(&mut c);
        c.save(&self.home)?;
        self.cfg_changed.notify_waiters();
        Ok(r)
    }

    pub fn id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// The friend with this node id, if they are (still) one.
    /// Whether friends may store backups on this node.
    pub fn accepting(&self) -> bool {
        self.cfg.lock().unwrap().accept_backups
    }

    /// Remembers whether a friend takes this node's backups.
    pub fn set_closed(&self, id: &str, closed: bool) {
        let mut c = self.closed.lock().unwrap();
        if closed {
            c.insert(id.to_string());
        } else {
            c.remove(id);
        }
    }

    pub fn is_closed(&self, id: &str) -> bool {
        self.closed.lock().unwrap().contains(id)
    }

    pub fn friend(&self, id: &str) -> Option<Friend> {
        self.cfg.lock().unwrap().friends.iter().find(|f| f.id == id && f.is_friend()).cloned()
    }

    fn forgotten_path(&self) -> PathBuf {
        self.home.join("forgotten.json")
    }

    /// Restore points deleted on purpose: an id everywhere, or "destination|id" at one place.
    /// A destination that still offers one is not believed.
    fn forgotten(&self) -> HashSet<String> {
        fs::read(self.forgotten_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn forget_ids(&self, keys: impl IntoIterator<Item = String>) {
        let mut all = self.forgotten();
        all.extend(keys);
        let _ = write_private(&self.forgotten_path(), &serde_json::to_vec(&all).unwrap_or_default());
    }

    /// Makes sure the disk at a drive's path is the one that was added, marking it on first use.
    /// The marker is read without holding the drives lock, so a slow or hostile disk holds up only its own probe.
    fn check_drive(&self, f: &Friend, dir: &Path) -> Result<()> {
        let key = || self.cfg.lock().unwrap().friends.iter().find(|x| x.id == f.id).and_then(|x| x.drive_key.clone());
        let k = match key() {
            Some(k) => k,
            None => {
                // Marking happens once: a second probe waits, then checks the key the first one wrote.
                let _one = self.drives.lock().unwrap();
                match key() {
                    Some(k) => k,
                    None => return self.mark_drive(f, dir),
                }
            }
        };
        if read_mark(dir).is_some_and(|d| d.trim() == k) {
            return Ok(());
        }
        bail!(
            "{}: the disk at {} is not the one that was added; to use this one run `cahoots drive {} {}`",
            f.name,
            dir.display(),
            f.name,
            f.path.as_deref().unwrap_or(dir).display()
        )
    }

    fn mark_drive(&self, f: &Friend, dir: &Path) -> Result<()> {
        use std::io::Write;
        let mark = dir.join("sdb-drive-id");
        let key = hex::encode(rand::random::<[u8; 16]>());
        let _ = fs::remove_file(&mark);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&mark)
            .and_then(|mut m| m.write_all(key.as_bytes()))
            .with_context(|| mark.display().to_string())?;
        self.update(|c| {
            if let Some(x) = c.friends.iter_mut().find(|x| x.id == f.id) {
                x.drive_key = Some(key);
            }
        })
    }

    /// Closes every open invite; used when someone keeps guessing at them.
    pub fn revoke_invites(&self) {
        let _ = self.update(|c| c.invites.clear());
    }

    /// Keeps a connection a friend opened to us so our own requests can use it too.
    pub fn adopt(&self, id: &str, conn: &Connection) {
        self.down.lock().unwrap().remove(id);
        let mut conns = self.conns.lock().unwrap();
        if !conns.get(id).is_some_and(|c| c.close_reason().is_none()) {
            conns.insert(id.to_string(), conn.clone());
        }
    }

    /// Like `peer`, but tries a second time: a node that just restarted can need a moment.
    pub async fn peer_patient(&self, f: &Friend) -> Result<Peer> {
        match self.peer(f).await {
            Ok(p) => Ok(p),
            Err(_) if f.is_friend() => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                self.down.lock().unwrap().remove(&f.id);
                self.peer(f).await
            }
            Err(e) => Err(e),
        }
    }

    pub async fn peer(&self, f: &Friend) -> Result<Peer> {
        if let Some(path) = &f.path {
            let dir = path.join("sdb-store");
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => bail!("{}: {} is a link or a file, refusing to use it", f.name, dir.display()),
                Err(_) => bail!("{}: drive is not connected ({} not found)", f.name, dir.display()),
            }
            self.check_drive(f, &dir)?;
            let store =
                self.drives.lock().unwrap().entry(dir.clone()).or_insert_with(|| Arc::new(Store::new(dir, 0))).clone();
            return Ok(Peer { friend: f.clone(), link: Link::Drive(store, self.id()) });
        }
        if let Some(c) = &f.cloud {
            let mut clouds = self.clouds.lock().unwrap();
            let bucket = match clouds.get(&f.id) {
                Some(b) => b.clone(),
                None => {
                    let b = Arc::new(crate::cloud::Bucket::open(c, &self.id()).with_context(|| f.name.clone())?);
                    clouds.insert(f.id.clone(), b.clone());
                    b
                }
            };
            return Ok(Peer { friend: f.clone(), link: Link::Cloud(bucket) });
        }
        if let Some(c) = self.conns.lock().unwrap().get(&f.id) {
            if c.close_reason().is_none() {
                return Ok(Peer { friend: f.clone(), link: Link::Remote(c.clone()) });
            }
        }
        let addr = net::endpoint_addr(&f.id, &f.relay, &dialable(&f.addrs))?;
        // An unreachable friend is not retried for a while, so one offline friend does not slow everything down.
        if self.down.lock().unwrap().get(&f.id).is_some_and(|t| t.elapsed() < Duration::from_secs(20)) {
            bail!("{}: offline", f.name);
        }
        let conn = match net::connect(&self.endpoint, addr).await {
            Ok(c) => c,
            Err(e) => {
                self.down.lock().unwrap().insert(f.id.clone(), Instant::now());
                return Err(e).with_context(|| f.name.clone());
            }
        };
        self.down.lock().unwrap().remove(&f.id);
        // TLS has proven who answered; refuse anything but the friend we meant to reach.
        if conn.remote_id().to_string() != f.id {
            conn.close(1u32.into(), b"wrong node");
            bail!("{}: a different node answered", f.name);
        }
        if let Some(me) = self.me.upgrade() {
            tokio::spawn(net::serve_conn(me, conn.clone(), f.id.clone()));
        }
        self.conns.lock().unwrap().insert(f.id.clone(), conn.clone());
        Ok(Peer { friend: f.clone(), link: Link::Remote(conn) })
    }

    /// The chunks a destination holds for us that this node's latest indexes use, and a bounded
    /// number of others, refreshed at least daily. A destination so cannot make this node hold
    /// more by listing ids it never stored, and a chunk left out is at worst sent again.
    pub async fn known(&self, peer: &Peer, refresh: bool) -> Result<Arc<HashSet<Id>>> {
        if !refresh {
            if let Some((at, set)) = self.known.lock().await.get(&peer.friend.id) {
                if at.elapsed() < Duration::from_secs(86400) {
                    return Ok(set.clone());
                }
            }
        }
        let mut ours = HashSet::new();
        for b in self.cfg().sets {
            for e in self.load_last_index(&b.name) {
                if let Kind::File { chunks, .. } = e.kind {
                    ours.extend(chunks);
                }
            }
        }
        // Also some others, so the chunks an interrupted backup already sent are not sent again.
        let l = peer.list_chunks(Arc::new(ours), KNOWN_OTHERS).await?;
        let mut set = l.have;
        set.extend(l.other);
        let set = Arc::new(set);
        self.known.lock().await.insert(peer.friend.id.clone(), (Instant::now(), set.clone()));
        Ok(set)
    }

    pub async fn set_known(&self, id: &str, set: Arc<HashSet<Id>>) {
        let mut k = self.known.lock().await;
        let at = k.get(id).map(|(at, _)| *at).unwrap_or_else(Instant::now);
        k.insert(id.into(), (at, set));
    }

    pub async fn drop_known(&self, id: &str) {
        self.known.lock().await.remove(id);
    }

    pub fn open_manifest(&self, id: &str, blob: &[u8]) -> Result<Manifest> {
        snapshot::decode_manifest(&self.keys.open_raw(id.as_bytes(), blob)?)
    }

    fn index_path(&self, id: &str, index: &[Id]) -> PathBuf {
        let h = blake3::hash(index.as_flattened()).to_hex();
        self.home.join("cache").join(format!("index-{id}-{}", &h[..8]))
    }

    fn last_index_path(&self, set: &str) -> PathBuf {
        let name =
            if set == DEFAULT_SET { "last-index".to_string() } else { format!("last-index-{}", clean_name(set)) };
        self.home.join("cache").join(name)
    }

    /// What the previous backup of a set recorded, used to skip unchanged files.
    pub fn load_last_index(&self, set: &str) -> Vec<Entry> {
        fs::read(self.last_index_path(set))
            .ok()
            .and_then(|b| zstd::decode_all(&b[..]).ok())
            .and_then(|b| snapshot::decode_index(&b).ok())
            .unwrap_or_default()
    }

    pub fn save_index(&self, set: &str, entries: &[Entry]) -> Result<()> {
        fs::create_dir_all(self.home.join("cache"))?;
        fs::write(self.last_index_path(set), zstd::encode_all(&snapshot::encode_index(entries)?[..], 3)?)?;
        Ok(())
    }

    pub async fn load_index(&self, snap: &Snap) -> Result<Vec<Entry>> {
        let path = self.index_path(&snap.id, &snap.m.index);
        if let Some(b) = fs::read(&path).ok().and_then(|b| zstd::decode_all(&b[..]).ok()) {
            if let Ok(e) = snapshot::decode_index(&b) {
                return Ok(e);
            }
        }
        let buf = self.fetch(&snap.friends(), &snap.m.index).await?.concat();
        let entries = snapshot::decode_index(&buf)?;
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, zstd::encode_all(&buf[..], 3)?)?;
        Ok(entries)
    }

    /// Fetches and decrypts chunks, falling back to the next friend when one fails.
    pub async fn fetch(&self, friends: &[Friend], ids: &[Id]) -> Result<Vec<Vec<u8>>> {
        let mut last = anyhow!("no friend holds this data");
        for f in friends {
            let got = async {
                let blobs = self.peer(f).await?.get_chunks(ids).await?;
                ids.iter()
                    .zip(blobs)
                    .map(|(id, b)| {
                        self.keys.open(
                            id,
                            &b.ok_or_else(|| {
                                anyhow!(
                                    "{} has a missing or damaged piece of this file (run `cahoots verify` to repair)",
                                    f.name
                                )
                            })?,
                        )
                    })
                    .collect::<Result<Vec<_>>>()
            };
            match got.await {
                Ok(v) => return Ok(v),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Connects to every destination in use, or to just the named one.
    pub async fn peers(&self, only: Option<&str>, out: &Out) -> Vec<Peer> {
        let friends: Vec<_> =
            self.cfg().friends.into_iter().filter(|f| f.backup_to && only.is_none_or(|n| n == f.name)).collect();
        let res = futures::future::join_all(friends.iter().map(|f| self.peer(f))).await;
        let mut peers = vec![];
        for (f, r) in friends.iter().zip(res) {
            match r {
                Ok(p) => peers.push(p),
                Err(e) => out.say(format!("  {} is unreachable: {e:#}", f.name)),
            }
        }
        peers
    }

    pub async fn snapshots(&self, out: &Out) -> Result<(Vec<Snap>, Vec<Peer>)> {
        self.snapshots_of(None, out).await
    }

    /// Restore points held by every destination, or by just the named one,
    /// which is then the only one contacted.
    async fn snapshots_of(&self, only: Option<&str>, out: &Out) -> Result<(Vec<Snap>, Vec<Peer>)> {
        let names = self.cfg();
        let peers = self.peers(only, out).await;
        let mut map: BTreeMap<String, Snap> = BTreeMap::new();
        let mut live = vec![];
        let gone = self.forgotten();
        for p in peers {
            let list = match p.list_snapshots().await {
                Ok(l) => l,
                Err(e) => {
                    out.say(format!("  {e:#}"));
                    continue;
                }
            };
            let mut stale = vec![];
            for (id, blob) in list {
                if gone.contains(&id) || gone.contains(&format!("{}|{id}", p.friend.id)) {
                    // Deleted on purpose earlier: do not let it come back, and ask again.
                    stale.push(id);
                    continue;
                }
                let Ok(mut m) = self.open_manifest(&id, &blob) else {
                    out.say(format!("  {}: restore point {id} is damaged", p.friend.name));
                    continue;
                };
                m.set = names.set_name_now(&m.set);
                let s = map.entry(id.clone()).or_insert_with(|| Snap { id, m: m.clone(), peers: vec![] });
                if m.thinned > s.m.thinned {
                    s.m = m.clone();
                }
                s.peers.push((p.friend.clone(), m.index));
            }
            if p.friend.path.is_some() {
                // Our own drive answers at once and cannot stall us.
                for id in &stale {
                    let _ = p.delete_snapshot(id).await;
                }
            } else {
                self.redelete(&p, stale);
            }
            live.push(p);
        }
        let mut snaps: Vec<Snap> = map.into_values().collect();
        snaps.sort_by_key(|s| (s.m.time, s.m.nanos));
        Ok((snaps, live))
    }

    /// Asks a destination again, in the background, to drop restore points it still offers
    /// after they were deleted. One that ignores this must not hold up listings (and with
    /// them prune and verify): requests are short, one at a time per destination, and an
    /// id is asked about once per run.
    fn redelete(&self, p: &Peer, ids: Vec<String>) {
        let Some(me) = self.me.upgrade() else { return };
        if ids.is_empty() || !self.deleting.lock().unwrap().insert(p.friend.id.clone()) {
            return;
        }
        let ids: Vec<String> = {
            let mut asked = self.asked_gone.lock().unwrap();
            ids.into_iter().filter(|id| asked.insert(format!("{}|{id}", p.friend.id))).collect()
        };
        let p = p.clone();
        tokio::spawn(async move {
            let mut left = ids.into_iter();
            while let Some(id) = left.next() {
                let done = tokio::time::timeout(Duration::from_secs(10), p.delete_snapshot(&id)).await;
                if !matches!(done, Ok(Ok(()))) {
                    // Not answering: stop here, and leave this one and the rest to a later listing.
                    let mut asked = me.asked_gone.lock().unwrap();
                    for id in std::iter::once(id).chain(left) {
                        asked.remove(&format!("{}|{id}", p.friend.id));
                    }
                    break;
                }
            }
            me.deleting.lock().unwrap().remove(&p.friend.id);
        });
    }

    /// Restore points as seen through one destination, or through all of them.
    pub async fn snapshots_from(&self, from: &Option<String>, out: &Out) -> Result<Vec<Snap>> {
        if let Some(name) = from {
            if !self.cfg().friends.iter().any(|f| &f.name == name) {
                bail!("no destination named {name}");
            }
        }
        let (mut snaps, _) = self.snapshots_of(from.as_deref(), out).await?;
        if let Some(name) = from {
            for s in snaps.iter_mut() {
                s.peers.retain(|(f, _)| &f.name == name);
                if let Some((_, idx)) = s.peers.first() {
                    if *idx != s.m.index {
                        s.m.index = idx.clone();
                    }
                }
            }
            snaps.retain(|s| !s.peers.is_empty());
        }
        Ok(snaps)
    }

    fn resolve<'a>(&self, snaps: &'a [Snap], spec: &str) -> Result<&'a Snap> {
        if snaps.is_empty() {
            bail!("there are no restore points yet");
        }
        if spec == "latest" {
            return Ok(snaps.last().unwrap());
        }
        if let Some(s) = snaps.iter().rev().find(|s| s.m.tag.as_deref() == Some(spec)) {
            return Ok(s);
        }
        let m: Vec<_> = snaps.iter().filter(|s| s.id.starts_with(spec)).collect();
        match m.len() {
            1 => return Ok(m[0]),
            0 => {}
            _ => bail!("'{spec}' matches more than one restore point"),
        }
        let Some(t) = snapshot::parse_when(spec) else {
            bail!(
                "no restore point matches '{spec}' (use an id, a tag, 'latest' or a date like 2026-11-20 or '2026-11-20 14:30')"
            );
        };
        snaps.iter().rev().find(|s| s.m.time <= t).ok_or_else(|| anyhow!("no restore point exists at or before {spec}"))
    }

    pub fn accept_pair(
        &self,
        remote: &str,
        token: &[u8; 16],
        name: &str,
        relay: Option<String>,
        addrs: Vec<SocketAddr>,
    ) -> Result<String> {
        let tok = blake3::hash(hex::encode(token).as_bytes());
        // A wrong code changes nothing and writes nothing.
        let open = self
            .cfg
            .lock()
            .unwrap()
            .invites
            .iter()
            .any(|i| i.expires > now() && blake3::hash(i.token.as_bytes()) == tok);
        if !open {
            bail!("that invite is invalid, already used or expired");
        }
        let mine = self.update(|c| {
            c.invites.retain(|i| i.expires > now());
            let pos = c.invites.iter().position(|i| blake3::hash(i.token.as_bytes()) == tok)?;
            c.invites.remove(pos);
            // Only keep address hints that are well formed and few.
            let relay = relay.filter(|r| net::valid_relay(r));
            let addrs = dialable(&addrs);
            add_friend(c, remote, name, relay, addrs);
            Some(c.name.clone())
        })?;
        let mine = mine.ok_or_else(|| anyhow!("that invite is invalid, already used or expired"))?;
        self.store.allow(remote);
        self.paired.notify_waiters();
        Ok(mine)
    }

    /// Deletes every chunk a friend holds for us that no remaining restore point uses.
    /// The chunks a destination holds, and those among them that none of `snaps` needs.
    /// Also returns how many ids the destination listed. At most MAX_DEAD unneeded ones come back
    /// at a time, so a destination listing ids it never stored cannot swell this node's memory;
    /// the rest go on a later run.
    async fn unneeded(&self, peer: &Peer, snaps: &[&Snap], uses: &mut Uses) -> Result<(HashSet<Id>, Vec<Id>, u64)> {
        let mut live: HashSet<Id> = HashSet::new();
        for s in snaps {
            let Some((_, idx)) = s.peers.iter().find(|(f, _)| f.id == peer.friend.id) else {
                continue;
            };
            live.extend(idx.iter().copied());
            live.extend(s.m.index.iter().copied());
            if !uses.knows(&s.id) {
                uses.set(&s.id, &self.load_index(s).await?);
            }
            uses.add_to(&s.id, &mut live);
        }
        let l = peer.list_chunks(Arc::new(live), MAX_DEAD).await?;
        Ok((l.have, l.other, l.count))
    }

    /// Deletes the chunks a destination holds that none of `snaps` needs. Returns how many
    /// went and how many bytes that freed there, by the destination's own count.
    async fn gc(&self, peer: &Peer, snaps: &[&Snap], uses: &mut Uses) -> Result<(u64, u64)> {
        let (have, dead, _) = self.unneeded(peer, snaps, uses).await?;
        if dead.is_empty() {
            self.known.lock().await.insert(peer.friend.id.clone(), (Instant::now(), Arc::new(have)));
            return Ok((0, 0));
        }
        let before = peer.usage().await.map(|u| u.0).ok();
        peer.delete_chunks(&dead).await?;
        let gone: HashSet<Id> = dead.iter().copied().collect();
        let kept: HashSet<Id> = have.difference(&gone).copied().collect();
        self.known.lock().await.insert(peer.friend.id.clone(), (Instant::now(), Arc::new(kept)));
        let after = peer.usage_exact().await.map(|u| u.0).ok();
        let freed = before.zip(after).map_or(0, |(b, a)| b.saturating_sub(a));
        Ok((dead.len() as u64, freed))
    }

    /// Frees room at a friend by dropping our oldest restore points there. Never drops the newest of any set.
    pub async fn evict(&self, friend: &Friend, out: &Out) -> Result<bool> {
        let peer = self.peer(friend).await?;
        let (snaps, _) = self.snapshots(out).await?;
        let mut mine: Vec<&Snap> = snaps.iter().filter(|s| s.peers.iter().any(|(f, _)| f.id == friend.id)).collect();
        let mut newest: Vec<&Snap> = vec![];
        for s in mine.iter().rev() {
            if !newest.iter().any(|n| n.m.set == s.m.set) {
                newest.push(s);
            }
        }
        mine.retain(|s| !newest.iter().any(|n| n.id == s.id));
        mine.sort_by_key(|s| (s.m.tag.is_some(), s.m.time, s.m.nanos));
        let mut uses = Uses::default();
        let mut freed = false;
        while !mine.is_empty() {
            let victim = mine.remove(0);
            self.forget_ids([format!("{}|{}", friend.id, victim.id)]);
            peer.delete_snapshot(&victim.id).await?;
            out.say(format!("    removed restore point {} ({}) from {}", victim.id, when(victim.m.time), friend.name));
            let rest: Vec<&Snap> = mine.iter().copied().chain(newest.iter().copied()).collect();
            self.gc(&peer, &rest, &mut uses).await?;
            freed = true;
            let (used, quota) = peer.usage_exact().await?;
            if used <= quota / 10 * 9 {
                break;
            }
        }
        Ok(freed)
    }

    /// Removes from one destination the restore points of sets that are no longer sent
    /// there, then the chunks nothing left there needs. Named restore points stay, as do
    /// those of sets that no longer exist: `cahoots forget` removes either.
    async fn clean_dest(&self, name: &str, dry_run: bool, out: &Out) -> Result<()> {
        let cfg = self.cfg();
        let f = cfg.friends.iter().find(|f| f.name == name).cloned();
        let f = f.ok_or_else(|| anyhow!("no destination named {name}"))?;
        let peer = self.peer_patient(&f).await?;
        let gone = self.forgotten();
        let mut held: Vec<Snap> = vec![];
        for (id, blob) in peer.list_snapshots().await? {
            if gone.contains(&id) || gone.contains(&format!("{}|{id}", f.id)) {
                continue;
            }
            match self.open_manifest(&id, &blob) {
                Ok(mut m) => {
                    m.set = cfg.set_name_now(&m.set);
                    let index = m.index.clone();
                    held.push(Snap { id, m, peers: vec![(f.clone(), index)] });
                }
                // What cannot be read cannot be judged: nothing is deleted on a guess.
                Err(_) => bail!("{name}: restore point {id} is damaged; run `cahoots verify` first"),
            }
        }
        held.sort_by_key(|s| (s.m.time, s.m.nanos));
        // "In use: no" is a pause, not a decision, so only the sets' own lists count here.
        let sent =
            |s: &Snap| cfg.set(&s.m.set).is_none_or(|b| b.destinations.is_empty() || b.destinations.contains(&f.name));
        let (stay, go): (Vec<&Snap>, Vec<&Snap>) = held.iter().partition(|s| s.m.tag.is_some() || sent(s));
        let mut names: Vec<&str> = go.iter().map(|s| s.m.set.as_str()).collect();
        names.dedup();
        names.sort();
        names.dedup();
        for n in &names {
            let of: Vec<&&Snap> = go.iter().filter(|s| s.m.set == *n).collect();
            out.say(format!(
                "  set {n} is no longer sent to {name}: {} {} restore points made {} to {}",
                if dry_run { "would remove" } else { "removing" },
                of.len(),
                when(of[0].m.time),
                when(of[of.len() - 1].m.time)
            ));
        }
        let named = stay.iter().filter(|s| s.m.tag.is_some() && !sent(s)).count();
        if named > 0 {
            out.say(format!(
                "  {named} named restore points of those sets stay on {name}; `cahoots forget` removes one"
            ));
        }
        if go.is_empty() {
            out.say(format!(
                "nothing to clean on {name}: every restore point there belongs to a set that is sent there"
            ));
            return Ok(());
        }
        if dry_run {
            out.say(format!(
                "dry run: {} restore points would be removed from {name}, and the chunks only they use",
                go.len()
            ));
            return Ok(());
        }
        self.forget_ids(go.iter().map(|s| format!("{}|{}", f.id, s.id)));
        for s in &go {
            peer.delete_snapshot(&s.id).await?;
        }
        let (chunks, freed) = self.gc(&peer, &stay, &mut Uses::default()).await?;
        let (used, _) = peer.usage_exact().await.unwrap_or((0, 0));
        out.say(format!(
            "removed {} restore points from {name} and freed {} there ({chunks} chunks); {} restore points and {} remain",
            go.len(),
            human(freed),
            stay.len(),
            human(used)
        ));
        Ok(())
    }

    fn keep_set(snaps: &[&Snap], r: &Retention) -> HashSet<String> {
        use chrono::TimeZone;
        let mut keep = HashSet::new();
        let newest_first: Vec<&Snap> = snaps.iter().rev().copied().collect();
        for s in newest_first.iter().take(r.keep_last as usize) {
            keep.insert(s.id.clone());
        }
        for (fmt, n) in [
            ("%Y-%m-%d %H", r.keep_hourly),
            ("%Y-%m-%d", r.keep_daily),
            ("%G-%V", r.keep_weekly),
            ("%Y-%m", r.keep_monthly),
        ] {
            let mut seen = HashSet::new();
            for s in &newest_first {
                let Some(d) = chrono::Local.timestamp_opt(s.m.time, 0).single() else {
                    continue;
                };
                if seen.len() >= n as usize {
                    break;
                }
                if seen.insert(d.format(fmt).to_string()) {
                    keep.insert(s.id.clone());
                }
            }
        }
        if r.keep_days > 0 {
            let cutoff = now() - r.keep_days as i64 * 86400;
            keep.retain(|id| snaps.iter().any(|s| &s.id == id && s.m.time >= cutoff));
        }
        for s in snaps.iter().filter(|s| s.m.tag.is_some()) {
            keep.insert(s.id.clone());
        }
        if let Some(s) = snaps.last() {
            keep.insert(s.id.clone());
        }
        keep
    }

    /// Applies the retention policy: drops expired restore points, trims files to
    /// their maximum number of versions, then reclaims the space at each friend.
    /// With `exclusions`, what a set's exclusion list and skipped folders now leave out is
    /// also taken out of the restore points made before (of every set, or of `only_set`).
    pub async fn prune(
        &self,
        dry_run: bool,
        forget: Option<String>,
        exclusions: bool,
        only_set: Option<&str>,
        out: &Out,
    ) -> Result<()> {
        let cfg = self.cfg();
        let (mut snaps, peers) = self.snapshots(out).await?;
        if snaps.is_empty() {
            out.say("there are no restore points yet");
            return Ok(());
        }
        let keep = match &forget {
            Some(spec) => {
                let id = self.resolve(&snaps, spec)?.id.clone();
                snaps.iter().map(|s| s.id.clone()).filter(|i| *i != id).collect()
            }
            None => {
                // Each set is judged by its own history rules.
                let mut names: Vec<String> = snaps.iter().map(|s| s.m.set.clone()).collect();
                names.sort();
                names.dedup();
                let mut keep = HashSet::new();
                for n in names {
                    let group: Vec<&Snap> = snaps.iter().filter(|s| s.m.set == n).collect();
                    let rules = cfg.set(&n).and_then(|s| s.retention.clone()).unwrap_or_else(|| cfg.retention.clone());
                    keep.extend(Self::keep_set(&group, &rules));
                }
                keep
            }
        };
        let mut removed = 0;
        // Destinations that failed part way: they are left alone, the others are still finished.
        let mut failed: HashMap<String, String> = HashMap::new();
        // A destination with history rules of its own drops what those rules do not keep,
        // whatever the others still hold.
        if forget.is_none() {
            for p in peers.iter().filter(|p| p.friend.retention.is_some()) {
                let (id, rules) = (&p.friend.id, p.friend.retention.as_ref().unwrap());
                let held = |s: &Snap| s.peers.iter().any(|(f, _)| &f.id == id);
                let mut names: Vec<String> = snaps.iter().filter(|s| held(s)).map(|s| s.m.set.clone()).collect();
                names.sort();
                names.dedup();
                let mut stay = HashSet::new();
                for n in names {
                    let group: Vec<&Snap> = snaps.iter().filter(|s| s.m.set == n && held(s)).collect();
                    stay.extend(Self::keep_set(&group, rules));
                }
                for s in snaps.iter_mut().filter(|s| keep.contains(&s.id)) {
                    if !held(s) || stay.contains(&s.id) {
                        continue;
                    }
                    out.say(format!(
                        "  {} restore point {} ({}) from {}",
                        if dry_run { "would remove" } else { "removing" },
                        s.id,
                        when(s.m.time),
                        p.friend.name
                    ));
                    removed += 1;
                    if dry_run || failed.contains_key(id) {
                        continue;
                    }
                    self.forget_ids([format!("{id}|{}", s.id)]);
                    match p.delete_snapshot(&s.id).await {
                        Ok(()) => s.peers.retain(|(f, _)| &f.id != id),
                        Err(e) => drop(failed.insert(id.clone(), format!("{e:#}"))),
                    }
                }
            }
            if !dry_run {
                snaps.retain(|s| !s.peers.is_empty());
            }
        }
        if !dry_run {
            self.forget_ids(snaps.iter().filter(|s| !keep.contains(&s.id)).map(|s| s.id.clone()));
        }
        for s in snaps.iter().filter(|s| !keep.contains(&s.id)) {
            out.say(format!(
                "  {} restore point {} of set {} ({})",
                if dry_run { "would remove" } else { "removing" },
                s.id,
                s.m.set,
                when(s.m.time)
            ));
            removed += 1;
            if !dry_run {
                for p in peers.iter().filter(|p| s.peers.iter().any(|(f, _)| f.id == p.friend.id)) {
                    if failed.contains_key(&p.friend.id) {
                        continue;
                    }
                    if let Err(e) = p.delete_snapshot(&s.id).await {
                        failed.insert(p.friend.id.clone(), format!("{e:#}"));
                    }
                }
            }
        }
        snaps.retain(|s| keep.contains(&s.id));

        let mut uses = Uses::default();
        let mut thinned = 0u64;
        let limited = cfg.retention.max_versions > 0
            || !cfg.version_rules.is_empty()
            || cfg.sets.iter().any(|b| b.retention.as_ref().is_some_and(|r| r.max_versions > 0));
        // What each set leaves out today, and how much of it older restore points held.
        let mut leaves_out: HashMap<String, Option<backup::Excludes>> = HashMap::new();
        let mut left_out: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        // About how large each chunk of a file taken out is, for the dry run's estimate.
        let mut sizes: HashMap<Id, u64> = HashMap::new();
        if (limited || exclusions) && forget.is_none() {
            let rules: Vec<(globset::GlobMatcher, u32)> = cfg
                .version_rules
                .iter()
                .filter_map(|r| Some((globset::Glob::new(&r.pattern).ok()?.compile_matcher(), r.max_versions)))
                .collect();
            let mut seen: HashMap<Vec<u8>, (u32, u64)> = HashMap::new();
            // Sets whose history could not be read in full: their older restore points are left
            // whole, since versions cannot be counted past the gap.
            let mut unread: HashSet<String> = HashSet::new();
            for s in snaps.iter_mut().rev() {
                if unread.contains(&s.m.set) {
                    continue;
                }
                let entries = match self.load_index(s).await {
                    Ok(e) => e,
                    Err(e) => {
                        out.say(format!(
                            "  set {}: restore point {} could not be read, not thinned: {e:#}",
                            s.m.set, s.id
                        ));
                        // Its holders cannot be cleaned up without this file list either.
                        for (f, _) in &s.peers {
                            failed.entry(f.id.clone()).or_insert_with(|| format!("{}: {e:#}", f.name));
                        }
                        unread.insert(s.m.set.clone());
                        continue;
                    }
                };
                if s.m.tag.is_some() {
                    uses.set(&s.id, &entries);
                    continue;
                }
                let before = entries.len();
                let set_limit = cfg
                    .set(&s.m.set)
                    .and_then(|b| b.retention.as_ref())
                    .map(|r| r.max_versions)
                    .unwrap_or(cfg.retention.max_versions);
                let ex = match exclusions && only_set.is_none_or(|n| n == s.m.set) {
                    true => leaves_out
                        .entry(s.m.set.clone())
                        .or_insert_with(|| backup::Excludes::new(&cfg, cfg.set(&s.m.set)?, &self.home).ok())
                        .as_ref(),
                    false => None,
                };
                let roots: Vec<PathBuf> = s.m.roots.iter().map(PathBuf::from).collect();
                let (mut bytes, mut excluded) = (0, 0u64);
                let kept: Vec<Entry> = entries
                    .into_iter()
                    .filter(|e| {
                        let p = e.path();
                        if ex.is_some_and(|ex| roots.iter().any(|r| p.starts_with(r) && ex.covers(&p, r))) {
                            excluded += 1;
                            if let (true, Kind::File { size, chunks }) = (dry_run, &e.kind) {
                                let each = size / chunks.len().max(1) as u64;
                                sizes.extend(chunks.iter().map(|id| (*id, each)));
                            }
                            return false;
                        }
                        let Kind::File { size, chunks } = &e.kind else {
                            return true;
                        };
                        if !limited {
                            bytes += size;
                            return true;
                        }
                        let limit = rules.iter().find(|(g, _)| g.is_match(&p)).map(|(_, n)| *n).unwrap_or(set_limit);
                        let v = sig(chunks, *size);
                        let st = seen.entry([s.m.set.as_bytes(), b"\0", &e.path[..]].concat()).or_insert((0, !v));
                        if st.1 != v {
                            *st = (st.0 + 1, v);
                        }
                        let ok = limit == 0 || st.0 <= limit;
                        if ok {
                            bytes += size;
                        }
                        ok
                    })
                    .collect();
                let gone = (before - kept.len()) as u64;
                if excluded > 0 {
                    let n = left_out.entry(s.m.set.clone()).or_default();
                    *n = (n.0 + excluded, n.1 + 1);
                }
                if gone > 0 {
                    thinned += gone - excluded;
                    if !dry_run {
                        self.rewrite(s, &kept, bytes, gone, &peers, &mut failed).await?;
                    }
                }
                uses.set(&s.id, &kept);
            }
        }
        for (set, (entries, points)) in &left_out {
            out.say(format!(
                "  set {set}: {points} older restore points still hold what the set now leaves out \
                 ({entries} files and folders in all); {}",
                if dry_run { "it would be taken out of them" } else { "it was taken out of them" }
            ));
        }
        if exclusions && left_out.is_empty() {
            out.say("  no older restore point holds anything the exclusion lists now leave out");
        }
        let all: Vec<&Snap> = snaps.iter().collect();
        if dry_run {
            // Nothing is deleted, but each destination can say how much it holds that would go.
            for p in &peers {
                let Ok((_, dead, listed)) = self.unneeded(p, &all, &mut uses).await else {
                    out.say(format!("  {}: could not work out what would be freed there", p.friend.name));
                    continue;
                };
                // A chunk of a file being taken out is about that file's size divided by its
                // chunks; for any other, the destination's average has to do.
                let used = p.usage().await.map_or(0, |u| u.0);
                let average = used / listed.max(1);
                let space: u64 = dead.iter().map(|id| sizes.get(id).copied().unwrap_or(average)).sum();
                out.say(format!(
                    "  {}: would free roughly {} ({} of the {} chunks it holds)",
                    p.friend.name,
                    human(space.min(used)),
                    dead.len(),
                    listed
                ));
            }
            out.say(format!("dry run: {removed} restore points and {thinned} old file versions would be removed"));
            return Ok(());
        }
        let (mut chunks, mut bytes) = (0, 0);
        for p in &peers {
            if failed.contains_key(&p.friend.id) {
                continue;
            }
            match self.gc(p, &all, &mut uses).await {
                Ok((n, freed)) => {
                    if n > 0 {
                        out.say(format!("  {}: freed {} ({n} chunks)", p.friend.name, human(freed)));
                    }
                    (chunks, bytes) = (chunks + n, bytes + freed);
                }
                Err(e) => drop(failed.insert(p.friend.id.clone(), format!("{e:#}"))),
            }
        }
        out.say(format!(
            "removed {removed} restore points and {thinned} old file versions, freed {} ({chunks} chunks) across destinations; {} restore points kept",
            human(bytes),
            snaps.len()
        ));
        if !failed.is_empty() {
            for e in failed.values() {
                out.say(format!("  not finished: {e}"));
            }
            bail!("pruning could not finish at {} destination(s); the others are done", failed.len());
        }
        if forget.is_none() {
            self.job_mark("prune");
        }
        Ok(())
    }

    /// Replaces a restore point's file list with a trimmed one on every reachable friend.
    async fn rewrite(
        &self,
        s: &mut Snap,
        entries: &[Entry],
        bytes: u64,
        gone: u64,
        peers: &[Peer],
        failed: &mut HashMap<String, String>,
    ) -> Result<()> {
        let buf = snapshot::encode_index(entries)?;
        let pieces: Vec<(Id, Vec<u8>)> = backup::split_index(&buf)
            .into_iter()
            .map(|p| {
                let id = self.keys.chunk_id(p);
                Ok((id, self.keys.seal(&id, p)?))
            })
            .collect::<Result<_>>()?;
        s.m.index = pieces.iter().map(|(id, _)| *id).collect();
        s.m.files = entries.iter().filter(|e| matches!(e.kind, Kind::File { .. })).count() as u64;
        s.m.bytes = bytes;
        s.m.thinned += gone;
        let sealed = self.keys.seal_raw(s.id.as_bytes(), &snapshot::encode_manifest(&s.m)?)?;
        for p in peers {
            let Some(slot) = s.peers.iter_mut().find(|(f, _)| f.id == p.friend.id) else {
                continue;
            };
            if failed.contains_key(&p.friend.id) {
                continue;
            }
            let put = async {
                p.put_chunks(&pieces.iter().collect::<Vec<_>>()).await?;
                p.put_snapshot(&s.id, sealed.clone()).await
            };
            match put.await {
                Ok(()) => slot.1 = s.m.index.clone(),
                Err(e) => drop(failed.insert(p.friend.id.clone(), format!("{e:#}"))),
            }
        }
        let path = self.index_path(&s.id, &s.m.index);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, zstd::encode_all(&buf[..], 3)?)?;
        Ok(())
    }

    /// Checks what friends hold for us and repairs damaged or missing chunks from another friend.
    pub async fn verify(&self, full: bool, set: Option<&str>, out: &Out) -> Result<()> {
        let (snaps, peers) = self.snapshots(out).await?;
        let (mut repaired, mut lost) = (0u64, 0u64);
        let mut unchecked = vec![];
        let chosen = set.and_then(|n| self.cfg().set(n).cloned());
        for p in &peers {
            // A set is checked where it is sent, and wherever older restore points of it still sit.
            if let Some(b) = &chosen {
                let held =
                    snaps.iter().filter(|s| s.m.set == b.name && s.peers.iter().any(|(f, _)| f.id == p.friend.id));
                match (b.sends_to(&p.friend), held.count()) {
                    (true, _) => {}
                    (false, 0) => continue,
                    (false, n) => out.say(format!(
                        "{}: set {} is no longer sent here, but {n} older restore points of it still are; checking those",
                        p.friend.name, b.name
                    )),
                }
            }
            // One destination that misbehaves does not stop the others from being checked.
            match self.verify_one(p, &snaps, &peers, full, set, out).await {
                Ok((fixed, gone)) => {
                    repaired += fixed;
                    lost += gone;
                }
                Err(e) => {
                    out.say(format!("  {} could not be checked: {e:#}", p.friend.name));
                    unchecked.push(p.friend.name.clone());
                }
            }
        }
        if lost > 0 {
            bail!(
                "{lost} chunks could not be repaired from a friend. Run `cahoots backup`: anything still unchanged on this computer is uploaded again. Then run `cahoots verify` once more."
            );
        }
        if !unchecked.is_empty() {
            bail!("could not check {}; every other destination was checked", unchecked.join(", "));
        }
        out.say(if repaired > 0 { format!("all good after repairing {repaired} chunks") } else { "all good".into() });
        Ok(())
    }

    /// Checks one destination; returns how many chunks were repaired and how many are lost.
    async fn verify_one(
        &self,
        p: &Peer,
        snaps: &[Snap],
        peers: &[Peer],
        full: bool,
        set: Option<&str>,
        out: &Out,
    ) -> Result<(u64, u64)> {
        let mut refs: HashSet<Id> = HashSet::new();
        let mut count = 0;
        let mine = |s: &&Snap| set.is_none_or(|n| s.m.set == n) && s.peers.iter().any(|(f, _)| f.id == p.friend.id);
        for s in snaps.iter().filter(mine) {
            count += 1;
            refs.extend(s.m.index.iter().copied());
            for e in self.load_index(s).await? {
                if let Kind::File { chunks, .. } = e.kind {
                    refs.extend(chunks);
                }
            }
        }
        // Only what these restore points use is kept from the listing.
        let refs = Arc::new(refs);
        let listing = p.list_chunks(refs.clone(), 0).await?;
        let (have, listed) = (Arc::new(listing.have), listing.count);
        // A check of one set saw only that set's chunks: it adds to what is known rather than
        // replacing it, or the other sets' next backups would send everything again.
        let mut known = self.known.lock().await;
        match (set, known.get(&p.friend.id)) {
            (None, _) => drop(known.insert(p.friend.id.clone(), (Instant::now(), have.clone()))),
            (Some(_), Some((at, old))) => {
                let (at, mut all) = (*at, (**old).clone());
                all.extend(have.iter().copied());
                known.insert(p.friend.id.clone(), (at, Arc::new(all)));
            }
            (Some(_), None) => {}
        }
        drop(known);
        let mut bad: Vec<Id> = refs.iter().filter(|i| !have.contains(*i)).copied().collect();
        // A destination that suddenly lists far less than it should is more likely broken or
        // misbehaving than damaged; sending it all again, perhaps out of paid storage, waits for the user.
        if !full && bad.len() > MAX_SAMPLED_REPAIR {
            bail!(
                "lists {} chunks, but {} of the {} it should hold are missing; nothing was copied. If it really lost them, `cahoots verify --full` copies them back from another destination",
                listed,
                bad.len(),
                refs.len()
            );
        }
        let mut ids: Vec<Id> = refs.iter().filter(|i| have.contains(*i)).copied().collect();
        if !full {
            use rand::seq::SliceRandom;
            ids.shuffle(&mut rand::rng());
            ids.truncate(32);
        }
        let mut read = 0u64;
        let (mut done, mut told) = (0usize, Instant::now());
        for batch in ids.chunks(16) {
            done += batch.len();
            if told.elapsed() >= Duration::from_secs(2) {
                told = Instant::now();
                out.say(format!("  ... {}: checked {done} of {} chunks", p.friend.name, ids.len()));
            }
            for (id, blob) in batch.iter().zip(p.get_chunks(batch).await?) {
                match blob.ok_or_else(|| anyhow!("missing")).and_then(|b| self.keys.open(id, &b)) {
                    Ok(d) => read += d.len() as u64,
                    Err(_) => bad.push(*id),
                }
            }
        }
        let others: Vec<&Peer> = peers.iter().filter(|o| o.friend.id != p.friend.id).collect();
        let (mut fixed, mut gone) = (0u64, 0u64);
        for id in &bad {
            let mut good = None;
            for o in &others {
                if let Ok(mut v) = o.get_chunks(&[*id]).await {
                    if let Some(blob) = v.pop().flatten().filter(|b| self.keys.open(id, b).is_ok()) {
                        good = Some(blob);
                        break;
                    }
                }
            }
            p.delete_chunks(&[*id]).await?;
            match good {
                Some(blob) => {
                    p.put_chunks(&[&(*id, blob)]).await?;
                    fixed += 1;
                }
                None => gone += 1,
            }
        }
        if !bad.is_empty() {
            self.known(p, true).await?;
        }
        out.say(format!(
            "{}: {count} restore points, {} chunks listed, {} chunks ({}) downloaded and checked{}",
            p.friend.name,
            listed,
            ids.len(),
            human(read),
            if full { "" } else { " (sample; use --full for everything)" }
        ));
        if !bad.is_empty() {
            out.say(format!(
                "  {} damaged or missing chunks: {fixed} repaired from another friend, {gone} not available elsewhere",
                bad.len()
            ));
        }
        Ok((fixed, gone))
    }

    async fn check_local(&self, set: Option<&str>, out: &Out) -> Result<()> {
        let sets = self.cfg().sets;
        let last: Vec<Entry> = sets
            .iter()
            .filter(|s| set.is_none_or(|n| s.name == n))
            .flat_map(|s| self.load_last_index(&s.name))
            .collect();
        let (keys, out2) = (self.keys.clone(), out.clone());
        let (checked, bad) = tokio::task::spawn_blocking(move || backup::check_local(&keys, last, &out2)).await?;
        if bad > 0 {
            bail!(
                "{bad} of {checked} unmodified files no longer match their backup. Restore good copies with `cahoots restore latest <file> --to <dir>` before the next backup replaces them."
            );
        }
        out.say(format!("{checked} unmodified files re-read and fingerprinted, all match their backup"));
        Ok(())
    }

    pub async fn scrub(self: &Arc<Self>, out: &Out) -> Result<()> {
        let node = self.clone();
        let (checked, bad) = tokio::task::spawn_blocking(move || node.store.scrub()).await?;
        out.say(format!("checked {checked} chunks stored here for friends, removed {bad} damaged ones (their owners re-send or repair them)"));
        Ok(())
    }

    /// What a friend stores here, for views. A first count walks their folder, which must not
    /// hold up the async workers, so it runs in the background and the view shows it next time.
    fn stored_here(self: &Arc<Self>, id: &str) -> Option<u64> {
        let n = self.store.usage_known(id);
        if n.is_none() {
            let (node, id) = (self.clone(), id.to_string());
            tokio::task::spawn_blocking(move || node.store.usage(&id));
        }
        n
    }

    /// The restore points at which a file's content changed, oldest first.
    #[allow(clippy::type_complexity)]
    async fn versions(
        &self,
        path: &str,
        from: &Option<String>,
        out: &Out,
    ) -> Result<Vec<(String, i64, Option<(u64, i64)>, Vec<String>)>> {
        let snaps = self.snapshots_from(from, out).await?;
        let want = crate::os::path_bytes(Path::new(path));
        let mut last: Option<Option<u64>> = None;
        let mut list = vec![];
        for s in &snaps {
            let cur = self.load_index(s).await?.into_iter().find(|e| e.path == want).and_then(|e| match &e.kind {
                Kind::File { size, chunks } => Some((sig(chunks, *size), *size, e.mtime)),
                _ => None,
            });
            let key = cur.map(|c| c.0);
            if last == Some(key) || (last.is_none() && key.is_none()) {
                continue;
            }
            list.push((
                s.id.clone(),
                s.m.time,
                cur.map(|c| (c.1, c.2)),
                s.peers.iter().map(|(f, _)| f.name.clone()).collect(),
            ));
            last = Some(key);
        }
        Ok(list)
    }

    pub fn set_running(&self, set: &str, what: Option<String>) {
        let mut r = self.running.lock().unwrap();
        match what {
            Some(w) => r.insert(set.to_string(), w),
            None => r.remove(set),
        };
    }

    /// Adds a line to the activity history.
    pub fn note(&self, line: impl Into<String>) {
        let mut log = self.log.lock().unwrap();
        log.push_back(format!("{} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), line.into()));
        while log.len() > 500 {
            log.pop_front();
        }
    }

    /// Records a finished backup of one set: when each destination got it, and its size.
    pub fn mark_set(&self, set: &str, ids: &[String], files: u64, bytes: u64) {
        let mut m = self.last_ok();
        for id in ids {
            m.insert(id.clone(), now());
            m.insert(format!("{set}|{id}"), now());
        }
        let _ = fs::create_dir_all(self.home.join("cache"));
        let _ = fs::write(self.state_path(), serde_json::to_vec(&m).unwrap_or_default());
        let path = self.home.join("cache").join("set-stats.json");
        let mut stats: HashMap<String, (u64, u64, i64)> =
            fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        stats.insert(set.to_string(), (files, bytes, now()));
        let _ = fs::write(path, serde_json::to_vec(&stats).unwrap_or_default());
    }

    /// Checks on every destination in the background so the home screen never waits.
    fn probe_all(self: &Arc<Self>) {
        {
            let mut last = self.last_probe.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(20)) {
                return;
            }
            *last = Some(Instant::now());
        }
        for f in self.cfg().friends {
            let node = self.clone();
            tokio::spawn(async move {
                let check = async {
                    let peer = node.peer(&f).await?;
                    let (used, quota) = peer.usage().await?;
                    let version = peer.version().await;
                    let closed = !peer.accepts().await;
                    Ok::<_, anyhow::Error>((used, quota, peer.describe_link().await, version, closed))
                };
                let res = tokio::time::timeout(Duration::from_secs(25), check).await;
                let probe = match res {
                    Ok(Ok((used, quota, link, version, closed))) => {
                        node.set_closed(&f.id, closed);
                        Probe { at: now(), ok: true, used, quota, link, version }
                    }
                    _ => Probe { at: now(), ok: false, used: 0, quota: 0, link: String::new(), version: None },
                };
                node.probes.lock().unwrap().insert(f.id.clone(), probe);
            });
        }
    }

    /// One JSON document describing sets, destinations and warnings.
    fn overview(self: &Arc<Self>) -> serde_json::Value {
        use serde_json::json;
        self.probe_all();
        let c = self.cfg();
        let ok = self.last_ok();
        let stats: HashMap<String, (u64, u64, i64)> = fs::read(self.home.join("cache").join("set-stats.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let running = self.running.lock().unwrap().clone();
        let probes = self.probes.lock().unwrap().clone();
        let next = self.next.lock().unwrap().clone();
        let sets: Vec<_> = c
            .sets
            .iter()
            .map(|b| {
                let dests: Vec<_> = c
                    .friends
                    .iter()
                    .filter(|f| b.sends_to(f))
                    .map(|f| {
                        let key = format!("{}|{}", b.name, f.id);
                        let due = next.get(&key).copied();
                        json!({
                            "name": f.name,
                            "last": ok.get(&key),
                            "every": b.interval_secs(f, &c) / 60,
                            "next": due.filter(|t| *t != i64::MAX),
                            "held": due == Some(i64::MAX),
                        })
                    })
                    .collect();
                let st = stats.get(&b.name);
                json!({
                    "name": b.name,
                    "paths": b.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    "hours": b.hours,
                    "held": c.held_for(b),
                    "files": st.map(|s| s.0),
                    "bytes": st.map(|s| s.1),
                    "last": st.map(|s| s.2),
                    "running": running.get(&b.name),
                    "dests": dests,
                })
            })
            .collect();
        let dests: Vec<_> = c
            .friends
            .iter()
            .map(|f| {
                let p = probes.get(&f.id);
                let state = match (&f.path, p) {
                    (Some(path), _) if path.join("sdb-store").is_dir() => "connected",
                    (Some(_), _) => "not connected",
                    (None, Some(p)) if p.ok => "online",
                    (None, Some(_)) => "offline",
                    (None, None) => "checking",
                };
                json!({
                    "name": f.name,
                    "drive": f.path.is_some(),
                    "kind": f.kind(),
                    "cloud": f.cloud.as_ref().map(|c| format!("{} at {}", c.bucket, c.endpoint)),
                    "keep_days": f.retention.as_ref().map(|r| r.keep_days),
                    "keep_last": f.retention.as_ref().map(|r| r.keep_last),
                    "path": f.path.as_ref().map(|p| p.display().to_string()),
                    "state": state,
                    "closed": self.is_closed(&f.id),
                    "link": p.filter(|p| p.ok).map(|p| p.link.clone()),
                    "version": p.filter(|p| p.ok).and_then(|p| p.version.clone()),
                    "checked": p.map(|p| p.at),
                    "used_there": p.filter(|p| p.ok).map(|p| p.used),
                    "quota_there": p.filter(|p| p.ok && f.path.is_none() && p.quota != u64::MAX).map(|p| p.quota),
                    "every": f.interval_secs(&c) / 60,
                    "max_in_kbps": f.max_in_kbps,
                    "in_use": f.backup_to,
                    "quota_gb": f.quota_gb,
                    "used_here": if f.is_friend() { self.stored_here(&f.id) } else { None },
                    "last_in": if f.is_friend() { self.store.last_snapshot(&f.id) } else { None },
                    "last_out": ok.get(&f.id),
                })
            })
            .collect();
        let kit = crate::recovery::state(&self.home, &c).describe();
        let (relay, addrs) = net::my_addrs(&self.endpoint);
        let copy = crate::recovery::copy_load(&self.home);
        json!({
            "name": c.name,
            "id": self.id(),
            "warnings": self.warnings(),
            "held": c.held(),
            "limit": c.upload_kbps,
            "kit": { "text": kit.0, "act": kit.1 },
            "kit_copy": {
                "on": copy.is_some(),
                "folder": copy.as_ref().map(|c| c.folder.display().to_string()),
                "at": copy.as_ref().map(|c| c.at),
                "error": copy.as_ref().and_then(|c| c.error.clone()),
            },
            "service": crate::service::installed(),
            "container": crate::service::in_container(),
            "addrs": addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "relay": relay,
            "last_new": self.last_backup.lock().unwrap().clone(),
            "version": net::VERSION,
            "turned_away": self.guard.rejected.load(std::sync::atomic::Ordering::Relaxed),
            "sets": sets,
            "dests": dests,
        })
    }

    fn state_path(&self) -> PathBuf {
        self.home.join("cache").join("last-ok.json")
    }

    fn jobs_path(&self) -> PathBuf {
        self.home.join("cache").join("jobs.json")
    }

    /// When one of the daemon's regular jobs ("prune", "check") last ran. It is kept on disk,
    /// so a daemon that is restarted more often than the job comes round still gets to it.
    pub fn job_last(&self, job: &str) -> Option<i64> {
        let all: HashMap<String, i64> =
            fs::read(self.jobs_path()).ok().and_then(|b| serde_json::from_slice(&b).ok())?;
        all.get(job).copied()
    }

    pub fn job_mark(&self, job: &str) {
        let mut all: HashMap<String, i64> =
            fs::read(self.jobs_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        all.insert(job.to_string(), now());
        let _ = fs::create_dir_all(self.home.join("cache"));
        let _ = fs::write(self.jobs_path(), serde_json::to_vec(&all).unwrap_or_default());
    }

    /// Whether a job last run at `last` and wanted every `every` seconds is due at `now`. A
    /// time still ahead of us means the clock has been set back since: nothing is due then,
    /// least of all a job that deletes by age.
    pub fn job_due(last: Option<i64>, now: i64, every: i64) -> bool {
        last.is_none_or(|t| now >= t && now - t >= every)
    }

    /// When each destination last completed a backup run, by friend id.
    pub fn last_ok(&self) -> HashMap<String, i64> {
        fs::read(self.state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Things the owner should know about: backups that have gone stale in either direction.
    pub fn warnings(&self) -> Vec<String> {
        let c = self.cfg();
        let mut w = vec![];
        for b in c.sets.iter().filter(|b| !b.paths.is_empty() && b.destinations.iter().all(|d| d == NOWHERE)) {
            if !b.destinations.is_empty() {
                w.push(format!("set {} has no destination and is not being backed up", b.name));
            }
        }
        if !c.all_paths().is_empty() && c.friends.iter().any(|f| f.backup_to) {
            match crate::recovery::state(&self.home, &c) {
                crate::recovery::Kit::Never => w.push(
                    "no recovery kit yet: without one, losing this computer loses your backups (cahoots recovery export)"
                        .into(),
                ),
                crate::recovery::Kit::Stale(_) => w.push(
                    "the recovery kit is older than your newest friend or drive: export it again (cahoots recovery export)"
                        .into(),
                ),
                crate::recovery::Kit::Current(_) => {}
            }
        }
        if let Some(c) = crate::recovery::copy_load(&self.home) {
            if let Some(e) = c.error {
                w.push(format!("the recovery kit could not be copied to {}: {e}", c.folder.display()));
            }
        }
        if c.warn_after_days == 0 {
            return w;
        }
        let limit = c.warn_after_days as i64 * 86400;
        let ok = self.last_ok();
        let created = chrono::DateTime::parse_from_rfc3339(&c.created).map(|d| d.timestamp()).unwrap_or(0);
        if !c.all_paths().is_empty() {
            for f in c.friends.iter().filter(|f| c.sets.iter().any(|b| !b.paths.is_empty() && b.sends_to(f))) {
                let last = ok.get(&f.id).copied();
                if now() - last.unwrap_or(created) > limit {
                    w.push(match last {
                        _ if self.is_closed(&f.id) => format!(
                            "{} does not accept backups from this computer, so nothing new of yours is kept there",
                            f.name
                        ),
                        Some(t) => format!(
                            "no backup has reached {} since {} ({} days ago)",
                            f.name,
                            when(t),
                            (now() - t) / 86400
                        ),
                        None => format!("no backup has ever reached {}", f.name),
                    });
                }
            }
            if c.friends.iter().all(|f| !f.backup_to) {
                w.push("you are not backing up to anyone".into());
            }
        }
        for f in c.friends.iter().filter(|f| f.is_friend()) {
            if let Some(t) = self.store.last_snapshot(&f.id) {
                if now() - t > limit {
                    w.push(format!("{} has not backed up to this computer for {} days", f.name, (now() - t) / 86400));
                }
            }
        }
        w
    }

    fn friend_mut<'a>(c: &'a mut Config, name: &str) -> Result<&'a mut Friend> {
        c.friends.iter_mut().find(|f| f.name == name).ok_or_else(|| anyhow!("no friend named {name}"))
    }

    /// Does what one command asks. Each command is one method, most of them in `commands.rs`.
    pub async fn run(self: &Arc<Self>, cmd: Cmd, out: &Out) -> Result<()> {
        let _turn = if cmd.exclusive() { Some(self.op.lock().await) } else { None };
        match cmd {
            Cmd::Backup { tag, set } => self.backup(tag, set, out).await,
            Cmd::Snapshots => self.show_snapshots(out).await,
            Cmd::Ls { snap, path } => self.ls(snap, path, out).await,
            Cmd::Diff { a, b } => self.diff(a, b, out).await,
            Cmd::History { path } => self.history(path, out).await,
            Cmd::Versions { path, from } => self.show_versions(path, from, out).await,
            Cmd::Restore { snap, paths, to, from, set } => self.restore(snap, paths, to, from, set, out).await,
            Cmd::Prune { dry_run, apply_exclusions, set } => self.run_prune(dry_run, apply_exclusions, set, out).await,
            Cmd::Forget { snap } => self.prune(false, Some(snap), false, None, out).await,
            Cmd::Verify { full, local, set } => self.run_verify(full, local, set, out).await,
            Cmd::Scrub => self.scrub(out).await,
            Cmd::Status => self.status(out).await,
            Cmd::Invite => self.invite(out).await,
            Cmd::Join { ticket } => self.join(&ticket, out).await,
            Cmd::Friends => self.show_destinations(out).await,
            Cmd::FriendRemove { name, purge } => self.remove_destination(name, purge, out).await,
            Cmd::FriendLimit { name, kbps } => self.limit_friend(name, kbps, out).await,
            Cmd::FriendQuota { name, gb } => self.set_quota(name, gb, out).await,
            Cmd::FriendBackupTo { name, on } => self.use_destination(name, on, out).await,
            Cmd::AddPaths { paths, set } => self.add_paths(paths, set, out).await,
            Cmd::RemovePath { path } => self.remove_path(path, out).await,
            Cmd::SetRename { name, to } => self.rename_set(name, to, out).await,
            Cmd::Sets => self.show_sets(out).await,
            Cmd::SetEdit(e) => self.edit_set(e, out).await,
            Cmd::Skip { path, set, remove } => self.skip_path(path, set, remove, out).await,
            Cmd::SetExclude { name, list } => self.choose_exclude_list(name, list, out).await,
            Cmd::ExcludeLists => self.show_exclude_lists(out).await,
            Cmd::ExcludeNew { name, from } => self.new_exclude_list(name, from, out).await,
            Cmd::ExcludeDelete { name } => self.delete_exclude_list(name, out).await,
            Cmd::ExcludePattern { list, pattern, remove } => {
                self.edit_exclude_pattern(list, pattern, remove, out).await
            }
            Cmd::Tune(t) => self.tune(t, out).await,
            Cmd::KitCopyOn { passphrase, folder } => self.kit_copy_on(passphrase, folder, out).await,
            Cmd::KitCopyOff => self.kit_copy_off(out).await,
            Cmd::KitCopySync => self.kit_copy_sync(out).await,
            Cmd::Every { name, minutes } => self.set_pace(name, minutes, out).await,
            Cmd::DriveAdd { name, path } => self.add_drive(name, path, out).await,
            Cmd::CloudAdd(c) => self.add_cloud(c, out).await,
            Cmd::DestClean { name, dry_run } => self.clean_dest(&name, dry_run, out).await,
            Cmd::DestKeep { name, set, clear } => self.keep_at_destination(name, set, clear, out).await,
            Cmd::Overview => self.show_overview(out).await,
            Cmd::Log => self.show_log(out).await,
            Cmd::Hello => self.hello(out).await,
            Cmd::Stop => self.stop(out).await,
            Cmd::Config => self.show_config(out).await,
            Cmd::SnapList { from } => self.show_snap_list(from, out).await,
            Cmd::Tree { snap, from } => self.show_tree(snap, from, out).await,
            Cmd::Keep { set, versions, pattern } => self.keep(set, versions, pattern, out).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_jobs_come_due_by_the_clock_on_the_wall() {
        let day = 86400;
        assert!(Node::job_due(None, 1_000_000, day), "never run: due");
        assert!(!Node::job_due(Some(1_000_000), 1_000_000 + day - 1, day));
        assert!(Node::job_due(Some(1_000_000), 1_000_000 + day, day));
        // The clock was set back since the job last ran: wait, do not delete by a wrong date.
        assert!(!Node::job_due(Some(2_000_000), 1_000_000, day));
    }

    #[test]
    fn chunk_use_is_kept_per_restore_point() {
        let file = |name: &str, chunks: Vec<Id>| Entry {
            path: name.as_bytes().to_vec(),
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
            ino: 0,
            kind: Kind::File { size: 1, chunks },
        };
        let dir = Entry { kind: Kind::Dir, ..file("d", vec![]) };
        let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let mut uses = Uses::default();
        uses.set("one", &[dir.clone(), file("x", vec![a, b]), file("y", vec![b])]);
        uses.set("two", &[file("x", vec![b, c])]);
        assert!(uses.knows("one") && !uses.knows("three"));
        assert_eq!(uses.ids.len(), 3, "a chunk shared by restore points is held once");
        let mut live = HashSet::new();
        uses.add_to("two", &mut live);
        assert_eq!(live, HashSet::from([b, c]));
        uses.add_to("three", &mut live);
        assert_eq!(live.len(), 2);
        // A thinned file list replaces the one recorded before.
        uses.set("one", &[file("y", vec![b])]);
        let mut live = HashSet::new();
        uses.add_to("one", &mut live);
        assert_eq!(live, HashSet::from([b]));
    }

    #[test]
    fn text_from_others_cannot_forge_lines_or_escapes() {
        assert_eq!(clean("ok\n\u{1}OK"), "ok??OK");
        assert_eq!(clean("\u{1b}]0;title\u{7}fine"), "?]0;title?fine");
        assert_eq!(clean("plain text, ünïcode"), "plain text, ünïcode");
    }

    #[cfg(unix)]
    #[test]
    fn a_drive_marker_is_only_a_small_regular_file() {
        let dir = std::env::temp_dir().join(format!("sdb-mark-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mark = dir.join("sdb-drive-id");
        assert_eq!(read_mark(&dir), None);
        fs::write(&mark, "abc").unwrap();
        assert_eq!(read_mark(&dir).as_deref(), Some("abc"));
        fs::write(&mark, [b'a'; 129]).unwrap();
        assert_eq!(read_mark(&dir), None, "too big");
        fs::remove_file(&mark).unwrap();
        fs::write(dir.join("real"), "abc").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), &mark).unwrap();
        assert_eq!(read_mark(&dir), None, "a link");
        fs::remove_file(&mark).unwrap();
        // A pipe with no writer would block a plain open forever.
        assert!(std::process::Command::new("mkfifo").arg(&mark).status().unwrap().success());
        assert_eq!(read_mark(&dir), None, "a pipe");
        fs::remove_dir_all(&dir).unwrap();
    }
}
