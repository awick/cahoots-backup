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
    },
    Prune {
        dry_run: bool,
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
    SetEdit {
        name: String,
        to: Option<Vec<String>>,
        every: Option<u32>,
        hours: Option<String>,
        days: Option<u32>,
        versions: Option<u32>,
        last: Option<u32>,
        delete: bool,
    },
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
    Tune {
        upload_kbps: Option<u64>,
        hours: Option<String>,
        pause_hours: Option<i64>,
        warn_days: Option<u32>,
    },
    DriveAdd {
        name: String,
        path: PathBuf,
    },
    /// Add cloud storage as a destination, or give an existing one new details.
    CloudAdd {
        name: String,
        endpoint: String,
        bucket: String,
        region: Option<String>,
        prefix: Option<String>,
        access_key: String,
        secret_key: String,
        /// How the bucket is addressed; None tries `bucket.endpoint` and then `endpoint/bucket`.
        path_style: Option<bool>,
        create: bool,
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
        Ok(l.ok_or_else(|| anyhow!("no exclusion list named {name}; see `sdb exclude lists`"))?.name.clone())
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
    fn check_drive(&self, f: &Friend, dir: &Path) -> Result<()> {
        use std::io::Write;
        let mark = dir.join("sdb-drive-id");
        let key = self.cfg.lock().unwrap().friends.iter().find(|x| x.id == f.id).and_then(|x| x.drive_key.clone());
        let found = fs::symlink_metadata(&mark)
            .ok()
            .filter(|m| m.is_file() && m.len() <= 128)
            .and_then(|_| fs::read_to_string(&mark).ok());
        match (key, found) {
            (Some(k), Some(d)) if d.trim() == k => Ok(()),
            (Some(_), _) => bail!(
                "{}: the disk at {} is not the one that was added; to use this one run `sdb drive {} {}`",
                f.name,
                dir.display(),
                f.name,
                f.path.as_deref().unwrap_or(dir).display()
            ),
            (None, _) => {
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
        }
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
            let mut drives = self.drives.lock().unwrap();
            self.check_drive(f, &dir)?;
            let store = drives.entry(dir.clone()).or_insert_with(|| Arc::new(Store::new(dir, 0))).clone();
            drop(drives);
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

    /// The set of chunks a friend holds for us, refreshed from them at least daily.
    pub async fn known(&self, peer: &Peer, refresh: bool) -> Result<Arc<HashSet<Id>>> {
        if !refresh {
            if let Some((at, set)) = self.known.lock().await.get(&peer.friend.id) {
                if at.elapsed() < Duration::from_secs(86400) {
                    return Ok(set.clone());
                }
            }
        }
        let set = Arc::new(peer.list_chunks().await?);
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
                                    "{} has a missing or damaged piece of this file (run `sdb verify` to repair)",
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

    async fn invite(&self, out: &Out) -> Result<()> {
        let token: [u8; 16] = rand::random();
        self.update(|c| c.invites.push(Invite { token: hex::encode(token), expires: now() + 86400 }))?;
        if !net::offline() {
            let _ = tokio::time::timeout(Duration::from_secs(8), self.endpoint.online()).await;
        }
        let (relay, addrs) = net::my_addrs(&self.endpoint);
        let t = Ticket { id: *self.endpoint.id().as_bytes(), relay, addrs, token };
        let ticket = format!("sdb1{}", data_encoding::BASE32_NOPAD.encode(&postcard::to_stdvec(&t)?).to_lowercase());
        let waiter = self.paired.notified();
        tokio::pin!(waiter);
        waiter.as_mut().enable();
        out.say("Send this one-time invite to your friend (valid for 24 hours, treat it like a password):");
        out.say("");
        out.say(format!("  sdb join {ticket}"));
        out.say("");
        if self.daemon {
            out.say("The daemon will accept it whenever they join.");
        } else {
            out.say(
                "Waiting for your friend to join (Ctrl-C to stop; run `sdb daemon` to accept in the background)...",
            );
            if tokio::time::timeout(Duration::from_secs(86400), waiter).await.is_ok() {
                let c = self.cfg();
                out.say(format!("Paired with {}.", c.friends.last().map(|f| f.name.as_str()).unwrap_or("friend")));
            }
        }
        Ok(())
    }

    async fn join(&self, ticket: &str, out: &Out) -> Result<()> {
        let raw = ticket.trim().strip_prefix("sdb1").ok_or_else(|| anyhow!("that is not an sdb invite"))?;
        let bytes =
            data_encoding::BASE32_NOPAD.decode(raw.to_uppercase().as_bytes()).map_err(|_| anyhow!("damaged invite"))?;
        let t: Ticket = postcard::from_bytes(&bytes).map_err(|_| anyhow!("damaged invite"))?;
        let id = PublicKey::from_bytes(&t.id).map_err(|_| anyhow!("damaged invite"))?.to_string();
        if id == self.id() {
            bail!("that is your own invite");
        }
        let relay_hint = t.relay.filter(|r| net::valid_relay(r));
        let hints = dialable(&t.addrs);
        let conn = net::connect(&self.endpoint, net::endpoint_addr(&id, &relay_hint, &hints)?).await?;
        let placeholder = Friend {
            name: "friend".into(),
            id: id.clone(),
            quota_gb: 0,
            backup_to: true,
            relay: None,
            addrs: vec![],
            every_minutes: 0,
            max_in_kbps: 0,
            path: None,
            drive_key: None,
            cloud: None,
            retention: None,
        };
        let peer = Peer { friend: placeholder, link: Link::Remote(conn.clone()) };
        if !net::offline() {
            let _ = tokio::time::timeout(Duration::from_secs(8), self.endpoint.online()).await;
        }
        let (relay, addrs) = net::my_addrs(&self.endpoint);
        let theirs = peer.pair(Req::Pair { token: t.token, name: self.cfg().name, relay, addrs }).await?;
        let name = self.update(|c| add_friend(c, &id, &theirs, relay_hint, hints))?;
        self.store.allow(&id);
        self.conns.lock().unwrap().insert(id, conn);
        out.say(format!("Paired with {name}. You now back up to each other."));
        Ok(())
    }

    /// Deletes every chunk a friend holds for us that no remaining restore point uses.
    async fn gc(&self, peer: &Peer, snaps: &[&Snap], cache: &mut HashMap<String, Vec<Entry>>) -> Result<u64> {
        let mut live: HashSet<Id> = HashSet::new();
        for s in snaps {
            let Some((_, idx)) = s.peers.iter().find(|(f, _)| f.id == peer.friend.id) else {
                continue;
            };
            live.extend(idx.iter().copied());
            live.extend(s.m.index.iter().copied());
            if !cache.contains_key(&s.id) {
                cache.insert(s.id.clone(), self.load_index(s).await?);
            }
            for e in &cache[&s.id] {
                if let Kind::File { chunks, .. } = &e.kind {
                    live.extend(chunks.iter().copied());
                }
            }
        }
        let have = peer.list_chunks().await?;
        let dead: Vec<Id> = have.difference(&live).copied().collect();
        peer.delete_chunks(&dead).await?;
        let kept: HashSet<Id> = have.intersection(&live).copied().collect();
        self.known.lock().await.insert(peer.friend.id.clone(), (Instant::now(), Arc::new(kept)));
        Ok(dead.len() as u64)
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
        let mut cache = HashMap::new();
        let mut freed = false;
        while !mine.is_empty() {
            let victim = mine.remove(0);
            self.forget_ids([format!("{}|{}", friend.id, victim.id)]);
            peer.delete_snapshot(&victim.id).await?;
            out.say(format!("    removed restore point {} ({}) from {}", victim.id, when(victim.m.time), friend.name));
            let rest: Vec<&Snap> = mine.iter().copied().chain(newest.iter().copied()).collect();
            self.gc(&peer, &rest, &mut cache).await?;
            freed = true;
            let (used, quota) = peer.usage_exact().await?;
            if used <= quota / 10 * 9 {
                break;
            }
        }
        Ok(freed)
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
    pub async fn prune(&self, dry_run: bool, forget: Option<String>, out: &Out) -> Result<()> {
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
                "  {} restore point {} ({})",
                if dry_run { "would remove" } else { "removing" },
                s.id,
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

        let mut cache: HashMap<String, Vec<Entry>> = HashMap::new();
        let mut thinned = 0u64;
        let limited = cfg.retention.max_versions > 0
            || !cfg.version_rules.is_empty()
            || cfg.sets.iter().any(|b| b.retention.as_ref().is_some_and(|r| r.max_versions > 0));
        if limited && forget.is_none() {
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
                    cache.insert(s.id.clone(), entries);
                    continue;
                }
                let before = entries.len();
                let set_limit = cfg
                    .set(&s.m.set)
                    .and_then(|b| b.retention.as_ref())
                    .map(|r| r.max_versions)
                    .unwrap_or(cfg.retention.max_versions);
                let mut bytes = 0;
                let kept: Vec<Entry> = entries
                    .into_iter()
                    .filter(|e| {
                        let Kind::File { size, chunks } = &e.kind else {
                            return true;
                        };
                        let p = e.path();
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
                if gone > 0 {
                    thinned += gone;
                    if !dry_run {
                        self.rewrite(s, &kept, bytes, gone, &peers, &mut failed).await?;
                    }
                }
                cache.insert(s.id.clone(), kept);
            }
        }
        if dry_run {
            out.say(format!("dry run: {removed} restore points and {thinned} old file versions would be removed"));
            return Ok(());
        }
        let mut freed = 0;
        let all: Vec<&Snap> = snaps.iter().collect();
        for p in &peers {
            if failed.contains_key(&p.friend.id) {
                continue;
            }
            match self.gc(p, &all, &mut cache).await {
                Ok(n) => freed += n,
                Err(e) => drop(failed.insert(p.friend.id.clone(), format!("{e:#}"))),
            }
        }
        out.say(format!(
            "removed {removed} restore points and {thinned} old file versions, freed {freed} chunks; {} restore points kept",
            snaps.len()
        ));
        if !failed.is_empty() {
            for e in failed.values() {
                out.say(format!("  not finished: {e}"));
            }
            bail!("pruning could not finish at {} destination(s); the others are done", failed.len());
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
        for p in &peers {
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
                "{lost} chunks could not be repaired from a friend. Run `sdb backup`: anything still unchanged on this computer is uploaded again. Then run `sdb verify` once more."
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
        let have = self.known(p, true).await?;
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
        let mut bad: Vec<Id> = refs.iter().filter(|i| !have.contains(*i)).copied().collect();
        let mut ids: Vec<Id> = refs.into_iter().filter(|i| have.contains(i)).collect();
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
            have.len(),
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
                "{bad} of {checked} unmodified files no longer match their backup. Restore good copies with `sdb restore latest <file> --to <dir>` before the next backup replaces them."
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

    async fn status(self: &Arc<Self>, out: &Out) -> Result<()> {
        let c = self.cfg();
        for w in self.warnings() {
            out.say(format!("WARNING:  {w}"));
        }
        out.say(format!("name:     {}", c.name));
        out.say(format!("version:  {}", net::VERSION));
        out.say(format!("node id:  {}", self.id()));
        let turned_away = self.guard.rejected.load(std::sync::atomic::Ordering::Relaxed);
        if turned_away > 0 {
            out.say(format!(
                "security: {turned_away} connection attempts from unknown nodes turned away since this started"
            ));
        }
        out.say(format!(
            "daemon:   {}",
            if self.watching {
                "running (watching for changes)"
            } else {
                "not running (files are not watched; back up by hand or run `sdb service install`)"
            }
        ));
        out.say(format!(
            "at login: {}",
            if crate::service::installed() {
                "the daemon starts by itself (service installed)"
            } else {
                "the daemon is not started (run `sdb service install`)"
            }
        ));
        let (relay, addrs) = net::my_addrs(&self.endpoint);
        let addrs: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        out.say(format!(
            "reachable at: {}{}",
            addrs.join(", "),
            relay.map(|r| format!(" (relay {r})")).unwrap_or_default()
        ));
        if let Some(t) = self.last_backup.lock().unwrap().clone() {
            out.say(format!("last new restore point: {t}"));
        }
        if let Some(why) = c.held() {
            out.say(format!("automatic backups: held ({why})"));
        }
        if c.upload_kbps > 0 {
            out.say(format!("upload limit: {} KB/s", c.upload_kbps));
        }
        out.say("backing up:");
        for b in &c.sets {
            if c.sets.len() > 1 || !b.destinations.is_empty() || b.every_minutes > 0 || !b.hours.is_empty() {
                let to =
                    if b.destinations.is_empty() { "every destination".to_string() } else { b.destinations.join(", ") };
                let pace = if b.every_minutes > 0 { format!(", every {} min", b.every_minutes) } else { String::new() };
                let hours = if b.hours.is_empty() { String::new() } else { format!(", hours {}", b.hours) };
                out.say(format!("  set {} -> {to}{pace}{hours}", b.name));
            }
            for p in &b.paths {
                out.say(format!("    {}", p.display()));
            }
        }
        let r = &c.retention;
        out.say(format!(
            "keeping:  last {}, hourly {}, daily {}, weekly {}, monthly {}, max age {}, max versions per file {}",
            r.keep_last,
            r.keep_hourly,
            r.keep_daily,
            r.keep_weekly,
            r.keep_monthly,
            if r.keep_days == 0 { "unlimited".into() } else { format!("{} days", r.keep_days) },
            if r.max_versions == 0 { "unlimited".into() } else { r.max_versions.to_string() }
        ));
        for v in &c.version_rules {
            out.say(format!("          {} -> {} versions", v.pattern, v.max_versions));
        }
        out.say("destinations:");
        for f in &c.friends {
            let theirs = self.stored_here(&f.id).map_or("counting...".into(), human);
            let probe = async {
                let peer = self.peer(f).await?;
                let (used, quota) = peer.usage().await?;
                let link = match peer.version().await {
                    Some(v) => format!("{}, version {v}", peer.describe_link().await),
                    None => peer.describe_link().await,
                };
                let room =
                    if f.path.is_some() || quota == u64::MAX { String::new() } else { format!(" of {}", human(quota)) };
                Ok::<_, anyhow::Error>(format!("online {link}, you use {}{room} there", human(used)))
            };
            let mine = match tokio::time::timeout(Duration::from_secs(8), probe).await {
                Ok(Ok(s)) => s,
                _ => "offline".into(),
            };
            let mine = format!("{mine}, sent at most every {} min", f.interval_secs(&c) / 60);
            if f.path.is_some() {
                out.say(format!(
                    "  {:<16} {}",
                    f.name,
                    mine.replace("online local drive", "connected local drive")
                        .replace("offline", "local drive, not connected")
                ));
                continue;
            }
            if f.cloud.is_some() {
                let keep = match &f.retention {
                    Some(r) if r.keep_days > 0 => format!(", keeps {} days", r.keep_days),
                    Some(_) => ", own history rules".into(),
                    None => String::new(),
                };
                out.say(format!("  {:<16} {}{keep}", f.name, mine.replace("offline", "cloud storage, not reachable")));
                continue;
            }
            let last = match self.store.last_snapshot(&f.id) {
                Some(t) => format!(", last backup from them {}", when(t)),
                None => String::new(),
            };
            out.say(format!("  {:<16} {mine}; they use {theirs} of {} GB here{last}", f.name, f.quota_gb));
        }
        Ok(())
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
        let mut last: Option<Option<u64>> = None;
        let mut list = vec![];
        for s in &snaps {
            let cur =
                self.load_index(s).await?.into_iter().find(|e| e.path == path.as_bytes()).and_then(|e| match &e.kind {
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
                    Ok::<_, anyhow::Error>((used, quota, peer.describe_link().await, version))
                };
                let res = tokio::time::timeout(Duration::from_secs(25), check).await;
                let probe = match res {
                    Ok(Ok((used, quota, link, version))) => Probe { at: now(), ok: true, used, quota, link, version },
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
        json!({
            "name": c.name,
            "id": self.id(),
            "warnings": self.warnings(),
            "held": c.held(),
            "limit": c.upload_kbps,
            "service": crate::service::installed(),
            "version": net::VERSION,
            "turned_away": self.guard.rejected.load(std::sync::atomic::Ordering::Relaxed),
            "sets": sets,
            "dests": dests,
        })
    }

    fn state_path(&self) -> PathBuf {
        self.home.join("cache").join("last-ok.json")
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

    pub async fn run(self: &Arc<Self>, cmd: Cmd, out: &Out) -> Result<()> {
        match cmd {
            Cmd::Backup { tag, set } => backup::run(self, tag, set.as_deref(), None, out).await,
            Cmd::Snapshots => {
                let (snaps, _) = self.snapshots(out).await?;
                out.say(format!(
                    "{:<17} {:<20} {:<10} {:>9} {:>10} {:>10}  {:<12} {}",
                    "ID", "WHEN", "SET", "FILES", "SIZE", "UPLOADED", "TAG", "STORED WITH"
                ));
                for s in &snaps {
                    let names: Vec<_> = s.peers.iter().map(|(f, _)| f.name.clone()).collect();
                    out.say(format!(
                        "{:<17} {:<20} {:<10} {:>9} {:>10} {:>10}  {:<12} {}",
                        s.id,
                        when(s.m.time),
                        s.m.set,
                        s.m.files,
                        human(s.m.bytes),
                        human(s.m.new_bytes),
                        s.m.tag.clone().unwrap_or_default(),
                        names.join(", ")
                    ));
                }
                Ok(())
            }
            Cmd::Ls { snap, path } => {
                let (snaps, _) = self.snapshots(out).await?;
                let s = self.resolve(&snaps, &snap)?;
                let filters: Vec<String> = path.into_iter().collect();
                for e in self.load_index(s).await?.iter().filter(|e| under(&e.path(), &filters)) {
                    let t = match e.kind {
                        Kind::Dir => 'd',
                        Kind::Symlink(_) => 'l',
                        Kind::File { .. } => '-',
                    };
                    out.say(format!(
                        "{t}{:04o} {:>10} {} {}",
                        e.mode & 0o7777,
                        human(e.size()),
                        when(e.mtime / 1_000_000_000),
                        e.show()
                    ));
                }
                Ok(())
            }
            Cmd::Diff { a, b } => {
                let (snaps, _) = self.snapshots(out).await?;
                let (sa, sb) = (self.resolve(&snaps, &a)?, self.resolve(&snaps, &b)?);
                let ea: BTreeMap<Vec<u8>, Entry> =
                    self.load_index(sa).await?.into_iter().map(|e| (e.path.clone(), e)).collect();
                let eb: BTreeMap<Vec<u8>, Entry> =
                    self.load_index(sb).await?.into_iter().map(|e| (e.path.clone(), e)).collect();
                let mut n = 0;
                for (p, e) in &eb {
                    match ea.get(p) {
                        None => out.say(format!("+ {}", e.show())),
                        Some(o) if o.kind != e.kind => out.say(format!("M {}", e.show())),
                        Some(o) if o.mode != e.mode => out.say(format!("m {}", e.show())),
                        _ => continue,
                    }
                    n += 1;
                }
                for e in ea.values().filter(|e| !eb.contains_key(&e.path)) {
                    out.say(format!("- {}", e.show()));
                    n += 1;
                }
                out.say(format!("{n} differences between {} and {}", sa.id, sb.id));
                Ok(())
            }
            Cmd::History { path } => {
                let list = self.versions(&path, &None, out).await?;
                for (id, time, v, _) in &list {
                    match v {
                        Some((size, mtime)) => out.say(format!(
                            "{id}  {}  {:>10}  modified {}",
                            when(*time),
                            human(*size),
                            when(mtime / 1_000_000_000)
                        )),
                        None => out.say(format!("{id}  {}  (not present)", when(*time))),
                    }
                }
                let n = list.iter().filter(|v| v.2.is_some()).count();
                out.say(format!("{n} versions of {path}; restore one with `sdb restore <ID> {path} --to <dir>`"));
                Ok(())
            }
            Cmd::Versions { path, from } => {
                for (id, time, v, at) in self.versions(&path, &from, out).await? {
                    out.say(serde_json::json!({"id": id, "time": time, "size": v.map(|x| x.0), "at": at}).to_string());
                }
                Ok(())
            }
            Cmd::Restore { snap, paths, to, from } => {
                let snaps = self.snapshots_from(&from, out).await?;
                let s = self.resolve(&snaps, &snap)?;
                let entries: Vec<Entry> =
                    self.load_index(s).await?.into_iter().filter(|e| under(&e.path(), &paths)).collect();
                if entries.is_empty() {
                    bail!("nothing in restore point {} matches", s.id);
                }
                out.say(format!(
                    "restoring {} entries from restore point {} ({})",
                    entries.len(),
                    s.id,
                    when(s.m.time)
                ));
                restore::run(self, &s.friends(), entries, &to, &s.m.roots, out).await
            }
            Cmd::Prune { dry_run } => {
                let _op = self.op.lock().await;
                self.prune(dry_run, None, out).await
            }
            Cmd::Forget { snap } => {
                let _op = self.op.lock().await;
                self.prune(false, Some(snap), out).await
            }
            Cmd::Verify { set: Some(name), .. } if self.cfg().set(&name).is_none() => bail!("no set named {name}"),
            Cmd::Verify { local: true, set, .. } => self.check_local(set.as_deref(), out).await,
            Cmd::Verify { full, set, .. } => self.verify(full, set.as_deref(), out).await,
            Cmd::Scrub => self.scrub(out).await,
            Cmd::Status => self.status(out).await,
            Cmd::Invite => self.invite(out).await,
            Cmd::Join { ticket } => self.join(&ticket, out).await,
            Cmd::Friends => {
                for f in self.cfg().friends {
                    if let Some(p) = &f.path {
                        out.say(format!(
                            "{:<16} local drive at {}{}",
                            f.name,
                            p.display(),
                            if f.backup_to { "" } else { " (not in use)" }
                        ));
                        continue;
                    }
                    if let Some(c) = &f.cloud {
                        out.say(format!(
                            "{:<16} cloud storage, bucket {} at {}{}{}",
                            f.name,
                            c.bucket,
                            c.endpoint,
                            if f.quota_gb > 0 { format!(", capped at {} GB", f.quota_gb) } else { String::new() },
                            if f.backup_to { "" } else { " (not in use)" }
                        ));
                        continue;
                    }
                    out.say(format!(
                        "{:<16} quota {} GB, using {}{}  {}",
                        f.name,
                        f.quota_gb,
                        self.stored_here(&f.id).map_or("counting...".into(), human),
                        if f.backup_to { "" } else { ", not backing up to them" },
                        f.id
                    ));
                }
                Ok(())
            }
            Cmd::FriendRemove { name, purge } => {
                let mut stranded = vec![];
                let f = self.update(|c| {
                    let pos = c.friends.iter().position(|f| f.name == name)?;
                    for b in c.sets.iter_mut().filter(|b| b.destinations.contains(&name)) {
                        b.destinations.retain(|d| *d != name);
                        // An empty list would mean "everywhere", which nobody asked for.
                        if b.destinations.is_empty() {
                            b.destinations.push(NOWHERE.into());
                            stranded.push(b.name.clone());
                        }
                    }
                    Some(c.friends.remove(pos))
                })?;
                let f = f.ok_or_else(|| anyhow!("no friend named {name}"))?;
                if let Some(conn) = self.conns.lock().unwrap().remove(&f.id) {
                    conn.close(1u32.into(), b"removed");
                }
                self.drives.lock().unwrap().retain(|p, _| f.path.as_ref().is_none_or(|d| !p.starts_with(d)));
                self.clouds.lock().unwrap().remove(&f.id);
                self.drop_known(&f.id).await;
                for set in stranded {
                    out.say(format!(
                        "set {set} went only to {name} and now has no destination: it is not backed up until you run `sdb set {set} --to NAME` (or `--to all`)"
                    ));
                }
                if !f.is_friend() {
                    out.say(format!("removed {name} (what is stored there was left in place)"));
                    return Ok(());
                }
                if purge {
                    self.store.forget_owner(&f.id)?;
                }
                out.say(format!(
                    "removed {name}{}",
                    if purge {
                        " and deleted the data they stored here"
                    } else {
                        " (their stored data was left in place; use --purge to delete it)"
                    }
                ));
                Ok(())
            }
            Cmd::FriendLimit { name, kbps } => {
                if self.cfg().friends.iter().any(|f| f.name == name && !f.is_friend()) {
                    bail!("{name} is not a friend's computer: nothing arrives from it to limit");
                }
                self.update(|c| Self::friend_mut(c, &name).map(|f| f.max_in_kbps = kbps))??;
                out.say(if kbps == 0 {
                    format!("{name} may send backups here at any speed")
                } else {
                    format!("{name} may send backups here at up to {kbps} KB/s")
                });
                Ok(())
            }
            Cmd::FriendQuota { name, gb } => {
                let kind = self.cfg().friends.iter().find(|f| f.name == name).map(|f| f.kind());
                if kind == Some("drive") {
                    bail!("a drive has no quota");
                }
                self.update(|c| Self::friend_mut(c, &name).map(|f| f.quota_gb = gb))??;
                if kind == Some("cloud") {
                    out.say(if gb == 0 {
                        format!("{name} has no cap: everything your history rules keep is stored there")
                    } else {
                        format!("at most {gb} GB is kept on {name}; when that is reached your oldest restore points there are removed")
                    });
                    return Ok(());
                }
                out.say(format!("{name} may now store up to {gb} GB here; if they are over it, their next backup removes their oldest restore points"));
                Ok(())
            }
            Cmd::FriendBackupTo { name, on } => {
                self.update(|c| Self::friend_mut(c, &name).map(|f| f.backup_to = on))??;
                out.say(format!("{} backing up to {name}", if on { "now" } else { "no longer" }));
                Ok(())
            }
            Cmd::AddPaths { paths, set } => {
                let first = self.cfg().sets.first().map(|b| b.name.clone());
                let name = clean_name(set.as_deref().or(first.as_deref()).unwrap_or(DEFAULT_SET));
                if self.cfg().set(&name).is_none() {
                    refuse_reserved(&name)?;
                }
                self.update(|c| {
                    for b in c.sets.iter_mut() {
                        b.paths.retain(|p| !paths.contains(p));
                    }
                    if c.set(&name).is_none() {
                        c.sets.push(BackupSet { name: name.clone(), ..Default::default() });
                    }
                    let b = c.sets.iter_mut().find(|b| b.name == name).unwrap();
                    b.paths.extend(paths);
                })?;
                out.say(format!("ok (set {name})"));
                Ok(())
            }
            Cmd::RemovePath { path } => {
                self.update(|c| {
                    for b in c.sets.iter_mut() {
                        b.paths.retain(|p| *p != path);
                        let roots = b.paths.clone();
                        b.skip.retain(|s| roots.iter().any(|r| s.starts_with(r)));
                    }
                })?;
                out.say("ok (existing restore points still contain it)");
                Ok(())
            }
            Cmd::SetRename { name, to } => {
                let to = clean_name(&to);
                refuse_reserved(&to)?;
                let c = self.cfg();
                if c.set(&name).is_none() {
                    bail!("no set named {name}");
                }
                if to == name {
                    return Ok(());
                }
                if c.set(&to).is_some() || c.sets.iter().any(|b| b.name != name && b.former_names.contains(&to)) {
                    bail!("the name {to} is already used by a set");
                }
                let _op = self.op.lock().await;
                // Local records are filed under the set's name: move them along.
                let _ = fs::rename(self.last_index_path(&name), self.last_index_path(&to));
                let mut ok = self.last_ok();
                let prefix = format!("{name}|");
                for key in ok.keys().filter(|k| k.starts_with(&prefix)).cloned().collect::<Vec<_>>() {
                    if let Some(v) = ok.remove(&key) {
                        ok.insert(format!("{to}|{}", &key[prefix.len()..]), v);
                    }
                }
                let _ = fs::write(self.state_path(), serde_json::to_vec(&ok).unwrap_or_default());
                let stats_path = self.home.join("cache").join("set-stats.json");
                let mut stats: HashMap<String, (u64, u64, i64)> =
                    fs::read(&stats_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
                if let Some(v) = stats.remove(&name) {
                    stats.insert(to.clone(), v);
                    let _ = fs::write(stats_path, serde_json::to_vec(&stats).unwrap_or_default());
                }
                self.update(|c| {
                    if let Some(b) = c.sets.iter_mut().find(|b| b.name == name) {
                        b.former_names.retain(|n| *n != to);
                        b.former_names.push(name.clone());
                        b.name = to.clone();
                    }
                })?;
                out.say(format!("set {name} is now {to}; its existing restore points stay with it"));
                Ok(())
            }
            Cmd::Sets => {
                let c = self.cfg();
                for b in &c.sets {
                    let to = if b.destinations.is_empty() {
                        "every destination".to_string()
                    } else {
                        b.destinations.join(", ")
                    };
                    let pace = if b.every_minutes > 0 {
                        format!("every {} min", b.every_minutes)
                    } else {
                        "each destination's own pace".into()
                    };
                    let hours =
                        if b.hours.is_empty() { "general hours".to_string() } else { format!("hours {}", b.hours) };
                    let keep = match &b.retention {
                        Some(r) => format!(
                            "own history rules (last {}, max age {} days, max versions {})",
                            r.keep_last, r.keep_days, r.max_versions
                        ),
                        None => "general history rules".into(),
                    };
                    out.say(format!(
                        "{}: {} folders -> {to}; {pace}; {hours}; {keep}; exclusion list {}",
                        b.name,
                        b.paths.len(),
                        c.list_for(b).name
                    ));
                    for p in &b.paths {
                        out.say(format!("    {}", p.display()));
                    }
                    for p in &b.skip {
                        out.say(format!("    not {}", p.display()));
                    }
                }
                Ok(())
            }
            Cmd::SetEdit { name, to, every, hours, days, versions, last, delete } => {
                let name = clean_name(&name);
                let c = self.cfg();
                if let Some(h) = &hours {
                    if !h.is_empty() && crate::config::parse_hours(h).is_none() {
                        bail!("hours look like 22-7 (10pm to 7am); use 'always' to follow the general setting");
                    }
                }
                // An empty list is stored as "everywhere", so it is only written when asked for by name.
                let to = match to {
                    Some(t) if t.is_empty() => {
                        bail!("a set needs at least one destination (use `--to all` for every destination)")
                    }
                    Some(t) if t.iter().any(|d| d == EVERYWHERE) => {
                        if t.len() > 1 {
                            bail!("a set goes either to every destination or to the ones named, not both");
                        }
                        Some(vec![])
                    }
                    t => t,
                };
                if let Some(bad) = to.iter().flatten().find(|d| !c.friends.iter().any(|f| &f.name == *d)) {
                    bail!("no destination named {bad}");
                }
                if delete {
                    if c.sets.len() <= 1 {
                        bail!("the last set cannot be deleted; remove its folders instead");
                    }
                    self.update(|c| c.sets.retain(|b| b.name != name))?;
                    out.say(format!("set {name} deleted (its restore points stay until pruned or forgotten)"));
                    return Ok(());
                }
                self.update(|c| {
                    if c.set(&name).is_none() {
                        c.sets.push(BackupSet { name: name.clone(), ..Default::default() });
                    }
                    let general = c.retention.clone();
                    let b = c.sets.iter_mut().find(|b| b.name == name).unwrap();
                    if let Some(t) = to {
                        b.destinations = t;
                    }
                    if let Some(e) = every {
                        b.every_minutes = e;
                    }
                    if let Some(h) = hours {
                        b.hours = h;
                    }
                    if days.is_some() || versions.is_some() || last.is_some() {
                        let r = b.retention.get_or_insert(general);
                        r.keep_days = days.unwrap_or(r.keep_days);
                        r.max_versions = versions.unwrap_or(r.max_versions);
                        r.keep_last = last.unwrap_or(r.keep_last);
                    }
                })?;
                out.say(format!("set {name} saved"));
                Ok(())
            }
            Cmd::Skip { path, set, remove } => {
                let name = self.update(|c| {
                    let b = c.sets.iter_mut().find(|b| {
                        set.as_ref().is_none_or(|n| *n == b.name) && b.paths.iter().any(|r| path.starts_with(r))
                    })?;
                    b.skip.retain(|p| *p != path);
                    if !remove {
                        b.skip.push(path.clone());
                    }
                    Some(b.name.clone())
                })?;
                let name =
                    name.ok_or_else(|| anyhow!("{} is not inside a folder that is backed up", path.display()))?;
                out.say(if remove {
                    format!("{} is backed up again (set {name})", path.display())
                } else {
                    format!("{} is left out of set {name}; restore points made so far still contain it", path.display())
                });
                Ok(())
            }
            Cmd::SetExclude { name, list } => {
                let c = self.cfg();
                if c.set(&name).is_none() {
                    bail!("no set named {name}");
                }
                let found = c.lists().into_iter().find(|l| l.name.eq_ignore_ascii_case(list.trim()));
                let list =
                    found.ok_or_else(|| anyhow!("no exclusion list named {list}; see `sdb exclude lists`"))?.name;
                self.update(|c| {
                    if let Some(b) = c.sets.iter_mut().find(|b| b.name == name) {
                        b.exclude_list = if list == STANDARD { String::new() } else { list.clone() };
                    }
                })?;
                out.say(format!("set {name} now uses the exclusion list \"{list}\""));
                Ok(())
            }
            Cmd::ExcludeLists => {
                let c = self.cfg();
                for l in c.lists() {
                    let users: Vec<&str> =
                        c.sets.iter().filter(|b| c.list_for(b).name == l.name).map(|b| b.name.as_str()).collect();
                    out.say(format!(
                        "{}{}: {}",
                        l.name,
                        if is_builtin_list(&l.name) { " (built in)" } else { "" },
                        if users.is_empty() {
                            "not used by any set".into()
                        } else {
                            format!("used by {}", users.join(", "))
                        }
                    ));
                    for p in &l.patterns {
                        out.say(format!("    {p}"));
                    }
                }
                for b in c.sets.iter().filter(|b| !b.skip.is_empty()) {
                    out.say(format!("left out of set {} by hand:", b.name));
                    for p in &b.skip {
                        out.say(format!("    {}", p.display()));
                    }
                }
                Ok(())
            }
            Cmd::ExcludeNew { name, from } => {
                let name: String =
                    name.chars().filter(|c| c.is_alphanumeric() || "-_. ".contains(*c)).take(32).collect();
                let name = name.trim().to_string();
                let c = self.cfg();
                if name.is_empty() {
                    bail!("use letters and digits for the list's name");
                }
                if c.lists().iter().any(|l| l.name.eq_ignore_ascii_case(&name)) {
                    bail!("there is already a list called {name}");
                }
                let patterns = match &from {
                    Some(f) => {
                        let found = c.lists().into_iter().find(|l| l.name.eq_ignore_ascii_case(f.trim()));
                        found.ok_or_else(|| anyhow!("no exclusion list named {f}"))?.patterns
                    }
                    None => vec![],
                };
                let n = patterns.len();
                self.update(|c| c.exclude_lists.push(ExcludeList { name: name.clone(), patterns }))?;
                out.say(format!(
                    "list {name} created with {n} patterns; a set uses it after `sdb set NAME --exclude \"{name}\"`"
                ));
                Ok(())
            }
            Cmd::ExcludeDelete { name } => {
                if is_builtin_list(&name) {
                    bail!("{name} is built in and stays");
                }
                let name = self.own_list(&name)?;
                let users = self.update(|c| {
                    let before = c.exclude_lists.len();
                    c.exclude_lists.retain(|l| l.name != name);
                    if c.exclude_lists.len() == before {
                        return None;
                    }
                    let mut users = vec![];
                    for b in c.sets.iter_mut().filter(|b| b.exclude_list == name) {
                        b.exclude_list = String::new();
                        users.push(b.name.clone());
                    }
                    Some(users)
                })?;
                let users = users.ok_or_else(|| anyhow!("no exclusion list named {name}"))?;
                out.say(if users.is_empty() {
                    format!("list {name} deleted")
                } else {
                    format!(
                        "list {name} deleted; {} now use{} Standard",
                        users.join(", "),
                        if users.len() == 1 { "s" } else { "" }
                    )
                });
                Ok(())
            }
            Cmd::ExcludePattern { list, pattern, remove } => {
                if is_builtin_list(&list) {
                    bail!("{list} is built in: copy it to change it, with `sdb exclude new NAME --from {list}`");
                }
                let list = self.own_list(&list)?;
                // A bare name such as node_modules means that name anywhere, when adding and
                // when removing alike; removing also takes the pattern exactly as listed.
                let typed = pattern.trim().to_string();
                let pattern = if typed.starts_with('/') || typed.starts_with("**/") {
                    typed.clone()
                } else {
                    format!("**/{typed}")
                };
                if typed.is_empty() || pattern == "**/" {
                    bail!("the pattern is empty");
                }
                if !remove {
                    globset::Glob::new(&pattern).map_err(|e| anyhow!("bad pattern: {e}"))?;
                }
                let found = self.update(|c| {
                    let l = c.exclude_lists.iter_mut().find(|l| l.name == list)?;
                    let had = l.patterns.len();
                    l.patterns.retain(|p| *p != pattern && !(remove && *p == typed));
                    if !remove {
                        l.patterns.push(pattern.clone());
                    }
                    Some(l.patterns.len() != had)
                })?;
                match found {
                    None => bail!("no exclusion list named {list}"),
                    Some(false) if remove => bail!("{list} has no pattern {pattern}"),
                    Some(_) if remove => out.say(format!("{pattern} removed from {list}")),
                    Some(_) => out.say(format!(
                        "{pattern} added to {list}; restore points made so far still contain what it matches"
                    )),
                }
                Ok(())
            }
            Cmd::Tune { upload_kbps, hours, pause_hours, warn_days } => {
                if let Some(h) = &hours {
                    if !h.is_empty() && crate::config::parse_hours(h).is_none() {
                        bail!("hours look like 22-7 (10pm to 7am); use 'always' to remove the limit");
                    }
                }
                self.update(|c| {
                    if let Some(k) = upload_kbps {
                        c.upload_kbps = k;
                    }
                    if let Some(h) = hours {
                        c.backup_hours = h;
                    }
                    if let Some(p) = pause_hours {
                        c.paused_until = if p <= 0 { 0 } else { now() + p.min(24 * 3650) * 3600 };
                    }
                    if let Some(d) = warn_days {
                        c.warn_after_days = d;
                    }
                })?;
                out.say("saved");
                Ok(())
            }
            Cmd::Every { name: Some(name), minutes } => {
                self.update(|c| Self::friend_mut(c, &name).map(|f| f.every_minutes = minutes))??;
                out.say(if minutes == 0 {
                    format!("{name} is back on the default (every 60 minutes for a friend)")
                } else {
                    format!("automatic backups go to {name} at most every {minutes} minutes")
                });
                Ok(())
            }
            Cmd::Every { name: None, minutes } => {
                self.update(|c| c.min_interval_secs = minutes.max(1) as u64 * 60)?;
                out.say(format!("local drives get automatic backups at most every {} minutes unless set otherwise; friends default to 60", minutes.max(1)));
                Ok(())
            }
            Cmd::DriveAdd { name, path } => {
                let dir = path.join("sdb-store");
                fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
                if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                    bail!("{} is a link or a file; choose a real folder", dir.display());
                }
                let name = clean_name(&name);
                refuse_reserved(&name)?;
                if let Some(f) = self.cfg().friends.iter().find(|f| f.name == name && f.path.is_none()) {
                    let what = if f.cloud.is_some() { "cloud storage" } else { "a friend" };
                    bail!("{what} is already called {name}; give the drive another name");
                }
                // Adding a drive (again) is the moment its disk is trusted and marked.
                self.drives.lock().unwrap().remove(&dir);
                self.drop_known(&format!("drive-{name}")).await;
                self.update(|c| {
                    c.friends.retain(|f| f.name != name);
                    c.friends.push(Friend {
                        name: name.clone(),
                        id: format!("drive-{name}"),
                        quota_gb: 1_000_000,
                        backup_to: true,
                        relay: None,
                        addrs: vec![],
                        every_minutes: 15,
                        max_in_kbps: 0,
                        path: Some(path.clone()),
                        drive_key: None,
                        cloud: None,
                        retention: None,
                    });
                })?;
                out.say(format!("{name} added: backups also go to {} (encrypted like everything else). When the drive is unplugged it is skipped and catches up later.", dir.display()));
                Ok(())
            }
            Cmd::CloudAdd { name, endpoint, bucket, region, prefix, access_key, secret_key, path_style, create } => {
                let name = clean_name(&name);
                refuse_reserved(&name)?;
                if self.cfg().friends.iter().any(|f| f.name == name && f.cloud.is_none()) {
                    bail!("a friend or drive is already called {name}; give the cloud storage another name");
                }
                let (bucket, access_key) = (bucket.trim().to_string(), access_key.trim().to_string());
                if bucket.is_empty() || access_key.is_empty() || secret_key.trim().is_empty() {
                    bail!("cloud storage needs a bucket, an access key and a secret key");
                }
                let mut cloud = Cloud {
                    endpoint: endpoint.trim().trim_end_matches('/').to_string(),
                    bucket,
                    region: region
                        .map(|r| r.trim().to_string())
                        .filter(|r| !r.is_empty())
                        .unwrap_or("us-east-1".into()),
                    prefix: prefix.unwrap_or_default().trim_matches('/').to_string(),
                    access_key,
                    secret_key: secret_key.trim().to_string(),
                    path_style: path_style.unwrap_or(false),
                };
                // Nothing is saved until the storage has taken, listed and deleted an object for us.
                let try_it = async |c: &Cloud| {
                    let b = crate::cloud::Bucket::open(c, &self.id())?;
                    if create {
                        b.create().await?;
                    }
                    b.check().await?;
                    Ok::<_, anyhow::Error>(b)
                };
                let b = match try_it(&cloud).await {
                    Ok(b) => b,
                    Err(first) if path_style.is_none() => {
                        cloud.path_style = true;
                        // Whichever way of addressing the bucket fits this storage is the one whose
                        // complaint is worth showing: a bare address can only be reached the second way.
                        let bare = crate::cloud::check_endpoint(&cloud.endpoint)
                            .is_ok_and(|u| !matches!(u.host(), Some(url::Host::Domain(d)) if d != "localhost"));
                        match try_it(&cloud).await {
                            Ok(b) => b,
                            Err(second) if bare => return Err(second),
                            Err(_) => return Err(first),
                        }
                    }
                    Err(e) => return Err(e),
                };
                let id = format!("cloud-{name}");
                self.clouds.lock().unwrap().remove(&id);
                self.drop_known(&id).await;
                let place = b.place.clone();
                self.update(|c| match c.friends.iter_mut().find(|f| f.name == name) {
                    Some(f) => f.cloud = Some(cloud),
                    None => c.friends.push(Friend {
                        name: name.clone(),
                        id,
                        quota_gb: 0,
                        backup_to: true,
                        relay: None,
                        addrs: vec![],
                        every_minutes: 0,
                        max_in_kbps: 0,
                        path: None,
                        drive_key: None,
                        cloud: Some(cloud),
                        retention: None,
                    }),
                })?;
                out.say(format!(
                    "{name} added: backups also go to {place}, encrypted like everything else. \
                     Do not add expiry (lifecycle) rules to the bucket: sdb removes what is no longer needed itself."
                ));
                Ok(())
            }
            Cmd::DestKeep { name, set, clear } => {
                let general = self.cfg().retention;
                let r = self.update(|c| {
                    Self::friend_mut(c, &name).map(|f| {
                        if clear {
                            f.retention = None;
                        } else {
                            let r = f.retention.get_or_insert(Retention { max_versions: 0, ..general });
                            for (k, v) in set {
                                match k.as_str() {
                                    "last" => r.keep_last = v,
                                    "hourly" => r.keep_hourly = v,
                                    "daily" => r.keep_daily = v,
                                    "weekly" => r.keep_weekly = v,
                                    "monthly" => r.keep_monthly = v,
                                    _ => r.keep_days = v,
                                }
                            }
                        }
                        f.retention.clone()
                    })
                })??;
                out.say(match r {
                    Some(r) => format!(
                        "{name} keeps: last {}, hourly {}, daily {}, weekly {}, monthly {}, max age {}; applied at the next `sdb prune` (the daemon prunes daily)",
                        r.keep_last,
                        r.keep_hourly,
                        r.keep_daily,
                        r.keep_weekly,
                        r.keep_monthly,
                        if r.keep_days == 0 { "unlimited".into() } else { format!("{} days", r.keep_days) }
                    ),
                    None => format!("{name} keeps whatever each set's history rules keep"),
                });
                Ok(())
            }
            Cmd::Overview => {
                out.say(self.overview().to_string());
                Ok(())
            }
            Cmd::Log => {
                for l in self.log.lock().unwrap().iter() {
                    out.say(l.clone());
                }
                Ok(())
            }
            Cmd::Hello => {
                out.say(crate::control::started_fingerprint());
                Ok(())
            }
            Cmd::Stop => {
                out.say("stopping");
                self.quit.notify_waiters();
                Ok(())
            }
            Cmd::Config => {
                // Screens show the config; the cloud secrets have no business on them.
                let mut c = self.cfg();
                for f in c.friends.iter_mut() {
                    if let Some(cl) = f.cloud.as_mut() {
                        cl.secret_key = String::new();
                    }
                }
                out.say(serde_json::to_string(&c)?);
                Ok(())
            }
            Cmd::SnapList { from } => {
                for s in self.snapshots_from(&from, out).await? {
                    let at: Vec<&str> = s.peers.iter().map(|(f, _)| f.name.as_str()).collect();
                    out.say(serde_json::json!({"id": s.id, "time": s.m.time, "files": s.m.files, "bytes": s.m.bytes, "tag": s.m.tag, "at": at, "set": s.m.set}).to_string());
                }
                Ok(())
            }
            Cmd::Tree { snap, from } => {
                let snaps = self.snapshots_from(&from, out).await?;
                let s = self.resolve(&snaps, &snap)?;
                for e in self.load_index(s).await? {
                    let v = match &e.kind {
                        Kind::File { size, chunks } => sig(chunks, *size).to_string(),
                        Kind::Symlink(t) => String::from_utf8_lossy(t).into_owned(),
                        Kind::Dir => String::new(),
                    };
                    out.say(
                        serde_json::json!({"p": e.show(), "d": e.kind == Kind::Dir, "s": e.size(), "v": v}).to_string(),
                    );
                }
                Ok(())
            }
            Cmd::Keep { set, versions, pattern } => {
                if let Some(p) = &pattern {
                    globset::Glob::new(p).map_err(|e| anyhow!("bad pattern: {e}"))?;
                    if versions.is_none() {
                        bail!("a pattern needs --versions");
                    }
                }
                self.update(|c| {
                    for (k, v) in set {
                        match k.as_str() {
                            "last" => c.retention.keep_last = v,
                            "hourly" => c.retention.keep_hourly = v,
                            "daily" => c.retention.keep_daily = v,
                            "weekly" => c.retention.keep_weekly = v,
                            "monthly" => c.retention.keep_monthly = v,
                            _ => c.retention.keep_days = v,
                        }
                    }
                    match (pattern, versions) {
                        (Some(p), Some(n)) => {
                            c.version_rules.retain(|r| r.pattern != p);
                            if n > 0 {
                                c.version_rules.push(VersionRule { pattern: p, max_versions: n });
                            }
                        }
                        (None, Some(n)) => c.retention.max_versions = n,
                        _ => {}
                    }
                })?;
                out.say("saved; applied at the next `sdb prune` (the daemon prunes daily)");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_from_others_cannot_forge_lines_or_escapes() {
        assert_eq!(clean("ok\n\u{1}OK"), "ok??OK");
        assert_eq!(clean("\u{1b}]0;title\u{7}fine"), "?]0;title?fine");
        assert_eq!(clean("plain text, ünïcode"), "plain text, ünïcode");
    }
}
