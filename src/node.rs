// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::{
    backup,
    config::{BackupSet, Config, Friend, Identity, Invite, Retention, VersionRule, DEFAULT_SET},
    crypto::{Id, Keys},
    net::{self, Handler, Link, Peer},
    proto::{Req, ALPN},
    restore,
    snapshot::{self, human, when, Entry, Kind, Manifest},
    store::Store,
};
use anyhow::{anyhow, bail, Context, Result};
use iroh::{endpoint::Connection, protocol::Router, Endpoint, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, Notify};

#[derive(Clone)]
pub struct Out(pub mpsc::UnboundedSender<String>);

impl Out {
    pub fn say(&self, s: impl Into<String>) {
        let _ = self.0.send(s.into());
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Cmd {
    Backup { tag: Option<String>, set: Option<String> },
    Snapshots,
    Ls { snap: String, path: Option<String> },
    Diff { a: String, b: String },
    History { path: String },
    Restore { snap: String, paths: Vec<String>, to: PathBuf, from: Option<String> },
    Prune { dry_run: bool },
    Forget { snap: String },
    Verify { full: bool, local: bool },
    Scrub,
    Status,
    Invite,
    Join { ticket: String },
    Friends,
    FriendRemove { name: String, purge: bool },
    FriendQuota { name: String, gb: u64 },
    FriendBackupTo { name: String, on: bool },
    AddPaths { paths: Vec<PathBuf>, set: Option<String> },
    Sets,
    SetEdit { name: String, to: Option<Vec<String>>, every: Option<u32>, hours: Option<String>, days: Option<u32>, versions: Option<u32>, last: Option<u32>, delete: bool },
    RemovePath { path: PathBuf },
    Exclude { pattern: String, remove: bool },
    Keep { set: Vec<(String, u32)>, versions: Option<u32>, pattern: Option<String> },
    Config,
    Tune { upload_kbps: Option<u64>, hours: Option<String>, pause_hours: Option<i64>, warn_days: Option<u32> },
    DriveAdd { name: String, path: PathBuf },
    Every { name: Option<String>, minutes: u32 },
    SnapList { from: Option<String> },
    Tree { snap: String, from: Option<String> },
    Versions { path: String, from: Option<String> },
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
        // Whoever holds exactly this version of the index goes first, local drives before friends.
        let mut v: Vec<_> = self.peers.iter().collect();
        v.sort_by_key(|(f, idx)| (*idx != self.m.index, f.path.is_none()));
        v.into_iter().map(|(f, _)| f.clone()).collect()
    }
}

pub struct Node {
    me: std::sync::Weak<Node>,
    pub home: PathBuf,
    cfg: Mutex<Config>,
    pub keys: Arc<Keys>,
    pub endpoint: Endpoint,
    pub store: Store,
    known: tokio::sync::Mutex<HashMap<String, (Instant, Arc<HashSet<Id>>)>>,
    conns: Mutex<HashMap<String, Connection>>,
    drives: Mutex<HashMap<PathBuf, Arc<Store>>>,
    down: Mutex<HashMap<String, Instant>>,
    /// Defences for the listening side: counters and limits for unknown and known callers.
    pub guard: net::Guard,
    pub op: tokio::sync::Mutex<()>,
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

fn add_friend(c: &mut Config, id: &str, name: &str, relay: Option<String>, addrs: Vec<SocketAddr>) -> String {
    if let Some(f) = c.friends.iter_mut().find(|f| f.id == id) {
        f.relay = relay;
        f.addrs = addrs;
        return f.name.clone();
    }
    let base = clean_name(name);
    let mut name = base.clone();
    let mut n = 2;
    while c.friends.iter().any(|f| f.name == name) {
        name = format!("{base}{n}");
        n += 1;
    }
    c.friends.push(Friend { name: name.clone(), id: id.into(), quota_gb: c.default_quota_gb, backup_to: true, relay, addrs, every_minutes: 0, path: None });
    name
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
            store: Store::new(cfg.store_dir(&home), cfg.min_free_gb << 30),
            keys: Arc::new(Keys::new(&master)),
            home,
            cfg: Mutex::new(cfg),
            endpoint: endpoint.clone(),
            known: Default::default(),
            conns: Default::default(),
            drives: Default::default(),
            down: Default::default(),
            guard: Default::default(),
            op: Default::default(),
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
            Err(_) if f.path.is_none() => {
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
            if !dir.is_dir() {
                bail!("{}: drive is not connected ({} not found)", f.name, dir.display());
            }
            let store = self.drives.lock().unwrap().entry(dir.clone()).or_insert_with(|| Arc::new(Store::new(dir, 0))).clone();
            return Ok(Peer { friend: f.clone(), link: Link::Drive(store, self.id()) });
        }
        if let Some(c) = self.conns.lock().unwrap().get(&f.id) {
            if c.close_reason().is_none() {
                return Ok(Peer { friend: f.clone(), link: Link::Remote(c.clone()) });
            }
        }
        let addr = net::endpoint_addr(&f.id, &f.relay, &f.addrs)?;
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
        let name = if set == DEFAULT_SET { "last-index".to_string() } else { format!("last-index-{}", clean_name(set)) };
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
                    .map(|(id, b)| self.keys.open(id, &b.ok_or_else(|| anyhow!("{} has a missing or damaged piece of this file (run `sdb verify` to repair)", f.name))?))
                    .collect::<Result<Vec<_>>>()
            };
            match got.await {
                Ok(v) => return Ok(v),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    pub async fn peers(&self, out: &Out) -> Vec<Peer> {
        let friends: Vec<_> = self.cfg().friends.into_iter().filter(|f| f.backup_to).collect();
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
        let peers = self.peers(out).await;
        let mut map: BTreeMap<String, Snap> = BTreeMap::new();
        let mut live = vec![];
        for p in peers {
            let list = match p.list_snapshots().await {
                Ok(l) => l,
                Err(e) => {
                    out.say(format!("  {e:#}"));
                    continue;
                }
            };
            for (id, blob) in list {
                let Ok(m) = self.open_manifest(&id, &blob) else {
                    out.say(format!("  {}: restore point {id} is damaged", p.friend.name));
                    continue;
                };
                let s = map.entry(id.clone()).or_insert_with(|| Snap { id, m: m.clone(), peers: vec![] });
                if m.thinned > s.m.thinned {
                    s.m = m.clone();
                }
                s.peers.push((p.friend.clone(), m.index));
            }
            live.push(p);
        }
        let mut snaps: Vec<Snap> = map.into_values().collect();
        snaps.sort_by_key(|s| (s.m.time, s.m.nanos));
        Ok((snaps, live))
    }

    /// Restore points as seen through one destination, or through all of them.
    pub async fn snapshots_from(&self, from: &Option<String>, out: &Out) -> Result<Vec<Snap>> {
        let (mut snaps, _) = self.snapshots(out).await?;
        if let Some(name) = from {
            if !self.cfg().friends.iter().any(|f| &f.name == name) {
                bail!("no destination named {name}");
            }
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
            bail!("no restore point matches '{spec}' (use an id, a tag, 'latest' or a date like 2026-11-20 or '2026-11-20 14:30')");
        };
        snaps
            .iter()
            .rev()
            .find(|s| s.m.time <= t)
            .ok_or_else(|| anyhow!("no restore point exists at or before {spec}"))
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
        let mine = self.update(|c| {
            c.invites.retain(|i| i.expires > now());
            let pos = c.invites.iter().position(|i| blake3::hash(i.token.as_bytes()) == tok)?;
            c.invites.remove(pos);
            // Only keep address hints that are well formed and few.
            let relay = relay.filter(|r| r.len() <= 200 && r.starts_with("https://"));
            let addrs: Vec<SocketAddr> = addrs.into_iter().take(8).collect();
            add_friend(c, remote, name, relay, addrs);
            Some(c.name.clone())
        })?;
        let mine = mine.ok_or_else(|| anyhow!("that invite is invalid, already used or expired"))?;
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
            out.say("Waiting for your friend to join (Ctrl-C to stop; run `sdb daemon` to accept in the background)...");
            if tokio::time::timeout(Duration::from_secs(86400), waiter).await.is_ok() {
                let c = self.cfg();
                out.say(format!("Paired with {}.", c.friends.last().map(|f| f.name.as_str()).unwrap_or("friend")));
            }
        }
        Ok(())
    }

    async fn join(&self, ticket: &str, out: &Out) -> Result<()> {
        let raw = ticket.trim().strip_prefix("sdb1").ok_or_else(|| anyhow!("that is not an sdb invite"))?;
        let bytes = data_encoding::BASE32_NOPAD.decode(raw.to_uppercase().as_bytes()).map_err(|_| anyhow!("damaged invite"))?;
        let t: Ticket = postcard::from_bytes(&bytes).map_err(|_| anyhow!("damaged invite"))?;
        let id = PublicKey::from_bytes(&t.id).map_err(|_| anyhow!("damaged invite"))?.to_string();
        if id == self.id() {
            bail!("that is your own invite");
        }
        let conn = net::connect(&self.endpoint, net::endpoint_addr(&id, &t.relay, &t.addrs)?).await?;
        let placeholder = Friend { name: "friend".into(), id: id.clone(), quota_gb: 0, backup_to: true, relay: None, addrs: vec![], every_minutes: 0, path: None };
        let peer = Peer { friend: placeholder, link: Link::Remote(conn.clone()) };
        if !net::offline() {
            let _ = tokio::time::timeout(Duration::from_secs(8), self.endpoint.online()).await;
        }
        let (relay, addrs) = net::my_addrs(&self.endpoint);
        let theirs = peer.pair(Req::Pair { token: t.token, name: self.cfg().name, relay, addrs }).await?;
        let name = self.update(|c| add_friend(c, &id, &theirs, t.relay, t.addrs))?;
        self.conns.lock().unwrap().insert(id, conn);
        out.say(format!("Paired with {name}. You now back up to each other."));
        Ok(())
    }

    /// Deletes every chunk a friend holds for us that no remaining restore point uses.
    async fn gc(&self, peer: &Peer, snaps: &[&Snap], cache: &mut HashMap<String, Vec<Entry>>) -> Result<u64> {
        let mut live: HashSet<Id> = HashSet::new();
        for s in snaps {
            let Some((_, idx)) = s.peers.iter().find(|(f, _)| f.id == peer.friend.id) else { continue };
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
            peer.delete_snapshot(&victim.id).await?;
            out.say(format!("    removed restore point {} ({}) from {}", victim.id, when(victim.m.time), friend.name));
            let rest: Vec<&Snap> = mine.iter().copied().chain(newest.iter().copied()).collect();
            self.gc(&peer, &rest, &mut cache).await?;
            freed = true;
            let (used, quota) = peer.usage().await?;
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
        for (fmt, n) in [("%Y-%m-%d %H", r.keep_hourly), ("%Y-%m-%d", r.keep_daily), ("%G-%V", r.keep_weekly), ("%Y-%m", r.keep_monthly)] {
            let mut seen = HashSet::new();
            for s in &newest_first {
                let Some(d) = chrono::Local.timestamp_opt(s.m.time, 0).single() else { continue };
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
        for s in snaps.iter().filter(|s| !keep.contains(&s.id)) {
            out.say(format!("  {} restore point {} ({})", if dry_run { "would remove" } else { "removing" }, s.id, when(s.m.time)));
            removed += 1;
            if !dry_run {
                for p in peers.iter().filter(|p| s.peers.iter().any(|(f, _)| f.id == p.friend.id)) {
                    p.delete_snapshot(&s.id).await?;
                }
            }
        }
        snaps.retain(|s| keep.contains(&s.id));

        let mut cache: HashMap<String, Vec<Entry>> = HashMap::new();
        let mut thinned = 0u64;
        let limited = cfg.retention.max_versions > 0 || !cfg.version_rules.is_empty() || cfg.sets.iter().any(|b| b.retention.as_ref().is_some_and(|r| r.max_versions > 0));
        if limited && forget.is_none() {
            let rules: Vec<(globset::GlobMatcher, u32)> = cfg
                .version_rules
                .iter()
                .filter_map(|r| Some((globset::Glob::new(&r.pattern).ok()?.compile_matcher(), r.max_versions)))
                .collect();
            let mut seen: HashMap<Vec<u8>, (u32, u64)> = HashMap::new();
            for s in snaps.iter_mut().rev() {
                let entries = self.load_index(s).await?;
                if s.m.tag.is_some() {
                    cache.insert(s.id.clone(), entries);
                    continue;
                }
                let before = entries.len();
                let set_limit = cfg.set(&s.m.set).and_then(|b| b.retention.as_ref()).map(|r| r.max_versions).unwrap_or(cfg.retention.max_versions);
                let mut bytes = 0;
                let kept: Vec<Entry> = entries
                    .into_iter()
                    .filter(|e| {
                        let Kind::File { size, chunks } = &e.kind else { return true };
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
                        self.rewrite(s, &kept, bytes, gone, &peers).await?;
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
            freed += self.gc(p, &all, &mut cache).await?;
        }
        out.say(format!(
            "removed {removed} restore points and {thinned} old file versions, freed {freed} chunks; {} restore points kept",
            snaps.len()
        ));
        Ok(())
    }

    /// Replaces a restore point's file list with a trimmed one on every reachable friend.
    async fn rewrite(&self, s: &mut Snap, entries: &[Entry], bytes: u64, gone: u64, peers: &[Peer]) -> Result<()> {
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
            let Some(slot) = s.peers.iter_mut().find(|(f, _)| f.id == p.friend.id) else { continue };
            p.put_chunks(&pieces.iter().collect::<Vec<_>>()).await?;
            p.put_snapshot(&s.id, sealed.clone()).await?;
            slot.1 = s.m.index.clone();
        }
        let path = self.index_path(&s.id, &s.m.index);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, zstd::encode_all(&buf[..], 3)?)?;
        Ok(())
    }

    /// Checks what friends hold for us and repairs damaged or missing chunks from another friend.
    pub async fn verify(&self, full: bool, out: &Out) -> Result<()> {
        let (snaps, peers) = self.snapshots(out).await?;
        let (mut repaired, mut lost) = (0u64, 0u64);
        for p in &peers {
            let have = self.known(p, true).await?;
            let mut refs: HashSet<Id> = HashSet::new();
            let mut count = 0;
            for s in snaps.iter().filter(|s| s.peers.iter().any(|(f, _)| f.id == p.friend.id)) {
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
            for batch in ids.chunks(16) {
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
                out.say(format!("  {} damaged or missing chunks: {fixed} repaired from another friend, {gone} not available elsewhere", bad.len()));
            }
            repaired += fixed;
            lost += gone;
        }
        if lost > 0 {
            bail!("{lost} chunks could not be repaired from a friend. Run `sdb backup`: anything still unchanged on this computer is uploaded again. Then run `sdb verify` once more.");
        }
        out.say(if repaired > 0 { format!("all good after repairing {repaired} chunks") } else { "all good".into() });
        Ok(())
    }

    async fn check_local(&self, out: &Out) -> Result<()> {
        let last: Vec<Entry> = self.cfg().sets.iter().flat_map(|s| self.load_last_index(&s.name)).collect();
        let (keys, out2) = (self.keys.clone(), out.clone());
        let (checked, bad) = tokio::task::spawn_blocking(move || backup::check_local(&keys, last, &out2)).await?;
        if bad > 0 {
            bail!("{bad} of {checked} unmodified files no longer match their backup. Restore good copies with `sdb restore latest <file> --to <dir>` before the next backup replaces them.");
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

    async fn status(&self, out: &Out) -> Result<()> {
        let c = self.cfg();
        for w in self.warnings() {
            out.say(format!("WARNING:  {w}"));
        }
        out.say(format!("name:     {}", c.name));
        out.say(format!("node id:  {}", self.id()));
        let turned_away = self.guard.rejected.load(std::sync::atomic::Ordering::Relaxed);
        if turned_away > 0 {
            out.say(format!("security: {turned_away} connection attempts from unknown nodes turned away since this started"));
        }
        out.say(format!("daemon:   {}", if self.watching { "running (watching for changes)" } else { "not running (files are not watched; back up by hand or run `sdb service install`)" }));
        let (relay, addrs) = net::my_addrs(&self.endpoint);
        let addrs: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        out.say(format!("reachable at: {}{}", addrs.join(", "), relay.map(|r| format!(" (relay {r})")).unwrap_or_default()));
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
                let to = if b.destinations.is_empty() { "every destination".to_string() } else { b.destinations.join(", ") };
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
        out.say("friends:");
        for f in &c.friends {
            let theirs = human(self.store.usage(&f.id).unwrap_or(0));
            let probe = async {
                let peer = self.peer(f).await?;
                let (used, quota) = peer.usage().await?;
                let link = peer.describe_link().await;
                let room = if f.path.is_some() { String::new() } else { format!(" of {}", human(quota)) };
                Ok::<_, anyhow::Error>(format!("online {link}, you use {}{room} there", human(used)))
            };
            let mine = match tokio::time::timeout(Duration::from_secs(8), probe).await {
                Ok(Ok(s)) => s,
                _ => "offline".into(),
            };
            let mine = format!("{mine}, sent at most every {} min", f.interval_secs(&c) / 60);
            if f.path.is_some() {
                out.say(format!("  {:<16} {}", f.name, mine.replace("online local drive", "connected local drive").replace("offline", "local drive, not connected")));
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
    async fn versions(&self, path: &str, from: &Option<String>, out: &Out) -> Result<Vec<(String, i64, Option<(u64, i64)>, Vec<String>)>> {
        let snaps = self.snapshots_from(from, out).await?;
        let mut last: Option<Option<u64>> = None;
        let mut list = vec![];
        for s in &snaps {
            let cur = self.load_index(s).await?.into_iter().find(|e| e.path == path.as_bytes()).and_then(|e| match &e.kind {
                Kind::File { size, chunks } => Some((sig(chunks, *size), *size, e.mtime)),
                _ => None,
            });
            let key = cur.map(|c| c.0);
            if last == Some(key) || (last.is_none() && key.is_none()) {
                continue;
            }
            list.push((s.id.clone(), s.m.time, cur.map(|c| (c.1, c.2)), s.peers.iter().map(|(f, _)| f.name.clone()).collect()));
            last = Some(key);
        }
        Ok(list)
    }

    fn state_path(&self) -> PathBuf {
        self.home.join("cache").join("last-ok.json")
    }

    /// When each destination last completed a backup run, by friend id.
    pub fn last_ok(&self) -> HashMap<String, i64> {
        fs::read(self.state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn mark_ok(&self, ids: &[String]) {
        let mut m = self.last_ok();
        for id in ids {
            m.insert(id.clone(), now());
        }
        let _ = fs::create_dir_all(self.home.join("cache"));
        let _ = fs::write(self.state_path(), serde_json::to_vec(&m).unwrap_or_default());
    }

    /// Things the owner should know about: backups that have gone stale in either direction.
    pub fn warnings(&self) -> Vec<String> {
        let c = self.cfg();
        let mut w = vec![];
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
                        Some(t) => format!("no backup has reached {} since {} ({} days ago)", f.name, when(t), (now() - t) / 86400),
                        None => format!("no backup has ever reached {}", f.name),
                    });
                }
            }
            if c.friends.iter().all(|f| !f.backup_to) {
                w.push("you are not backing up to anyone".into());
            }
        }
        for f in c.friends.iter().filter(|f| f.path.is_none()) {
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
                out.say(format!("{:<17} {:<20} {:<10} {:>9} {:>10} {:>10}  {:<12} {}", "ID", "WHEN", "SET", "FILES", "SIZE", "UPLOADED", "TAG", "STORED WITH"));
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
                    out.say(format!("{t}{:04o} {:>10} {} {}", e.mode & 0o7777, human(e.size()), when(e.mtime / 1_000_000_000), e.show()));
                }
                Ok(())
            }
            Cmd::Diff { a, b } => {
                let (snaps, _) = self.snapshots(out).await?;
                let (sa, sb) = (self.resolve(&snaps, &a)?, self.resolve(&snaps, &b)?);
                let ea: BTreeMap<Vec<u8>, Entry> = self.load_index(sa).await?.into_iter().map(|e| (e.path.clone(), e)).collect();
                let eb: BTreeMap<Vec<u8>, Entry> = self.load_index(sb).await?.into_iter().map(|e| (e.path.clone(), e)).collect();
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
                        Some((size, mtime)) => out.say(format!("{id}  {}  {:>10}  modified {}", when(*time), human(*size), when(mtime / 1_000_000_000))),
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
                let entries: Vec<Entry> = self.load_index(s).await?.into_iter().filter(|e| under(&e.path(), &paths)).collect();
                if entries.is_empty() {
                    bail!("nothing in restore point {} matches", s.id);
                }
                out.say(format!("restoring {} entries from restore point {} ({})", entries.len(), s.id, when(s.m.time)));
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
            Cmd::Verify { local: true, .. } => self.check_local(out).await,
            Cmd::Verify { full, .. } => self.verify(full, out).await,
            Cmd::Scrub => self.scrub(out).await,
            Cmd::Status => self.status(out).await,
            Cmd::Invite => self.invite(out).await,
            Cmd::Join { ticket } => self.join(&ticket, out).await,
            Cmd::Friends => {
                for f in self.cfg().friends {
                    if let Some(p) = &f.path {
                        out.say(format!("{:<16} local drive at {}{}", f.name, p.display(), if f.backup_to { "" } else { " (not in use)" }));
                        continue;
                    }
                    out.say(format!(
                        "{:<16} quota {} GB, using {}{}  {}",
                        f.name,
                        f.quota_gb,
                        human(self.store.usage(&f.id).unwrap_or(0)),
                        if f.backup_to { "" } else { ", not backing up to them" },
                        f.id
                    ));
                }
                Ok(())
            }
            Cmd::FriendRemove { name, purge } => {
                let f = self.update(|c| {
                    let pos = c.friends.iter().position(|f| f.name == name)?;
                    c.sets.iter_mut().for_each(|b| b.destinations.retain(|d| *d != name));
                    Some(c.friends.remove(pos))
                })?;
                let f = f.ok_or_else(|| anyhow!("no friend named {name}"))?;
                self.conns.lock().unwrap().remove(&f.id);
                if purge {
                    self.store.forget_owner(&f.id)?;
                }
                out.say(format!("removed {name}{}", if purge { " and deleted the data they stored here" } else { " (their stored data was left in place; use --purge to delete it)" }));
                Ok(())
            }
            Cmd::FriendQuota { name, gb } => {
                self.update(|c| Self::friend_mut(c, &name).map(|f| f.quota_gb = gb))??;
                out.say(format!("{name} may now store up to {gb} GB here; if they are over it, their next backup removes their oldest restore points"));
                Ok(())
            }
            Cmd::FriendBackupTo { name, on } => {
                self.update(|c| Self::friend_mut(c, &name).map(|f| f.backup_to = on))??;
                out.say(format!("{} backing up to {name}", if on { "now" } else { "no longer" }));
                Ok(())
            }
            Cmd::AddPaths { paths, set } => {
                let name = clean_name(set.as_deref().unwrap_or(DEFAULT_SET));
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
                self.update(|c| c.sets.iter_mut().for_each(|b| b.paths.retain(|p| *p != path)))?;
                out.say("ok (existing restore points still contain it)");
                Ok(())
            }
            Cmd::Sets => {
                let c = self.cfg();
                for b in &c.sets {
                    let to = if b.destinations.is_empty() { "every destination".to_string() } else { b.destinations.join(", ") };
                    let pace = if b.every_minutes > 0 { format!("every {} min", b.every_minutes) } else { "each destination's own pace".into() };
                    let hours = if b.hours.is_empty() { "general hours".to_string() } else { format!("hours {}", b.hours) };
                    let keep = match &b.retention {
                        Some(r) => format!("own history rules (last {}, max age {} days, max versions {})", r.keep_last, r.keep_days, r.max_versions),
                        None => "general history rules".into(),
                    };
                    out.say(format!("{}: {} folders -> {to}; {pace}; {hours}; {keep}", b.name, b.paths.len()));
                    for p in &b.paths {
                        out.say(format!("    {}", p.display()));
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
                if let Some(bad) = to.iter().flatten().find(|d| !c.friends.iter().any(|f| &f.name == *d)) {
                    bail!("no destination named {bad}");
                }
                if delete {
                    if name == DEFAULT_SET {
                        bail!("the default set cannot be deleted; remove its folders instead");
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
            Cmd::Exclude { pattern, remove } => {
                globset::Glob::new(&pattern).map_err(|e| anyhow!("bad pattern: {e}"))?;
                self.update(|c| {
                    c.exclude.retain(|p| *p != pattern);
                    if !remove {
                        c.exclude.push(pattern);
                    }
                })?;
                out.say("ok");
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
                        c.paused_until = if p == 0 { 0 } else { now() + p * 3600 };
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
                out.say(if minutes == 0 { format!("{name} is back on the default (every 60 minutes for a friend)") } else { format!("automatic backups go to {name} at most every {minutes} minutes") });
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
                let name = clean_name(&name);
                self.update(|c| {
                    c.friends.retain(|f| f.name != name);
                    c.friends.push(Friend { name: name.clone(), id: format!("drive-{name}"), quota_gb: 1_000_000, backup_to: true, relay: None, addrs: vec![], every_minutes: 15, path: Some(path.clone()) });
                })?;
                out.say(format!("{name} added: backups also go to {} (encrypted like everything else). When the drive is unplugged it is skipped and catches up later.", dir.display()));
                Ok(())
            }
            Cmd::Config => {
                out.say(serde_json::to_string(&self.cfg())?);
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
                    out.say(serde_json::json!({"p": e.show(), "d": e.kind == Kind::Dir, "s": e.size(), "v": v}).to_string());
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
