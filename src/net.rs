// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Friend-to-friend protocol: one request per QUIC stream.

use crate::{
    config::Friend,
    crypto::Id,
    node::Node,
    proto::{self, ALPN, MAX_BLOB, MAX_DELETE, MAX_IDS, MAX_SNAPSHOT, PIECE, Req, Resp},
    store::{Store, check_snap, cost},
};
use anyhow::{Result, anyhow, bail};
use iroh::{
    Endpoint, EndpointAddr, PublicKey, RelayMode, RelayUrl, SecretKey, TransportAddr,
    endpoint::{Connection, RecvStream, SendStream, presets},
    protocol::{AcceptError, ProtocolHandler},
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::Semaphore;
use tokio::time::Instant;

/// This build's release number, from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn offline() -> bool {
    std::env::var_os("SDB_OFFLINE").is_some()
}

fn builder(secret: &SecretKey, upnp: bool) -> iroh::endpoint::Builder {
    let mut b = if offline() {
        Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
    } else {
        Endpoint::builder(presets::N0)
    };
    if !upnp {
        b = b.portmapper_config(iroh::endpoint::PortmapperConfig::Disabled);
    }
    b.secret_key(secret.clone())
}

pub async fn bind(secret: SecretKey, port: u16, upnp: bool) -> Result<Endpoint> {
    if port != 0 {
        if let Ok(b) = builder(&secret, upnp).bind_addr(SocketAddr::from(([0, 0, 0, 0], port))) {
            if let Ok(ep) = b.bind().await {
                return Ok(ep);
            }
        }
    }
    builder(&secret, upnp).bind().await.map_err(|e| anyhow!("cannot start networking: {e}"))
}

pub fn my_addrs(ep: &Endpoint) -> (Option<String>, Vec<SocketAddr>) {
    let addr = ep.addr();
    let relay = addr.relay_urls().next().map(|u| u.to_string());
    let mut addrs: Vec<SocketAddr> = addr.ip_addrs().copied().collect();
    if addrs.is_empty() {
        addrs = ep
            .bound_sockets()
            .into_iter()
            .filter(|a| a.is_ipv4())
            .map(|a| SocketAddr::from(([127, 0, 0, 1], a.port())))
            .collect();
    }
    (relay, addrs)
}

pub fn endpoint_addr(id: &str, relay: &Option<String>, addrs: &[SocketAddr]) -> Result<EndpointAddr> {
    let pk: PublicKey = id.parse().map_err(|_| anyhow!("bad node id {id}"))?;
    let mut addr = EndpointAddr::new(pk);
    if let Some(r) = relay.as_ref().filter(|r| valid_relay(r)) {
        if let Ok(url) = r.parse() {
            addr = addr.with_relay_url(url);
        }
    }
    for a in addrs {
        addr = addr.with_ip_addr(*a);
    }
    Ok(addr)
}

/// A relay address from an invite or a friend is only dialed if it names a public https server.
pub fn valid_relay(r: &str) -> bool {
    let Ok(url) = r.parse::<iroh::RelayUrl>() else {
        return false;
    };
    let host = url.host_str().unwrap_or("").trim_matches(['[', ']']).trim_end_matches('.').to_ascii_lowercase();
    if r.len() > 200 || url.scheme() != "https" || !matches!(url.path(), "" | "/") {
        return false;
    }
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => public_ip(ip),
        // A name is resolved, and what it resolves to checked, each time it is dialed (see `connect`).
        Err(_) => true,
    }
}

/// True for an address on the open internet: not this computer, its local network,
/// a link-local, carrier-NAT, unspecified, broadcast or multicast address.
pub(crate) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let [o0, o1, ..] = a.octets();
            !(a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_multicast()
                || o0 == 0
                || (o0 == 100 && o1 & 0xc0 == 64))
        }
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => public_ip(IpAddr::V4(v4)),
            None => {
                !(a.is_loopback()
                    || a.is_unspecified()
                    || a.is_multicast()
                    || matches!(a.segments()[0] & 0xfe00, 0xfc00 | 0xfe00))
            }
        },
    }
}

/// A relay named by a friend is dialed only while its name resolves to public addresses alone,
/// so a hint cannot point this node at a service on its own network.
async fn public_relay(url: &RelayUrl) -> bool {
    if !valid_relay(url.as_str()) {
        return false;
    }
    let Some(host) = url.host_str() else { return false };
    if host.starts_with('[') || host.parse::<IpAddr>().is_ok() {
        return true;
    }
    let port = url.port_or_known_default().unwrap_or(443);
    match tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, port))).await {
        Ok(Ok(found)) => {
            let ips: Vec<IpAddr> = found.map(|a| a.ip()).collect();
            !ips.is_empty() && ips.into_iter().all(public_ip)
        }
        _ => false,
    }
}

pub async fn connect(ep: &Endpoint, mut addr: EndpointAddr) -> Result<Connection> {
    // Offline (local tests) no relay is used at all, so there is nothing to resolve.
    if !offline() {
        let mut keep = BTreeSet::new();
        for a in std::mem::take(&mut addr.addrs) {
            if let TransportAddr::Relay(url) = &a {
                if !public_relay(url).await {
                    continue;
                }
            }
            keep.insert(a);
        }
        addr.addrs = keep;
    }
    match tokio::time::timeout(Duration::from_secs(15), ep.connect(addr, ALPN)).await {
        Ok(Ok(c)) => Ok(c),
        // A node that does not speak our protocol version refuses the handshake with TLS alert
        // 120 (no_application_protocol), which QUIC reports as "handshake failed: error 120".
        Ok(Err(e))
            if ["error 120", "application protocol", "alpn"]
                .iter()
                .any(|w| format!("{e:#}").to_lowercase().contains(w)) =>
        {
            bail!(
                "connect failed: the other side runs an incompatible version of sdb; both need the same release ({e})"
            )
        }
        Ok(Err(e)) => bail!("connect failed: {e}"),
        Err(_) => bail!("connect timed out"),
    }
}

/// Where a destination's data lives: a friend over the network or a local drive.
#[derive(Clone)]
pub enum Link {
    Remote(Connection),
    Drive(Arc<Store>, String),
    Cloud(Arc<crate::cloud::Bucket>),
}

#[derive(Clone)]
pub struct Peer {
    pub friend: Friend,
    pub link: Link,
}

/// How long a friend may go without moving a request forward. Generous, because a friend may
/// hold us to a slow incoming speed limit; it is progress, not total time, that counts.
const IDLE: Duration = Duration::from_secs(300);
/// Bytes that must move to count as progress, so a trickle of single bytes does not.
const STEP: usize = 16 << 10;

/// When a request last moved forward, and how many progress reports the friend may still send.
struct Moved {
    at: Instant,
    bytes: usize,
    reports: u64,
    /// Bumped on each report, for a caller that watches a long upload move.
    tick: Option<Arc<AtomicU64>>,
}

#[derive(Clone)]
struct Progress(Arc<Mutex<Moved>>);

impl Progress {
    fn new() -> Self {
        Progress(Arc::new(Mutex::new(Moved { at: Instant::now(), bytes: 0, reports: 0, tick: None })))
    }

    fn moved(&self, n: usize) {
        let mut m = self.0.lock().unwrap();
        m.bytes += n;
        if m.bytes >= STEP {
            (m.at, m.bytes) = (Instant::now(), 0);
        }
    }

    /// Lets the friend report progress on `bytes` more of our upload: once per piece, at most.
    fn expect(&self, bytes: usize) {
        self.0.lock().unwrap().reports += (bytes / PIECE + 1) as u64;
    }

    /// The friend says it read another piece. Only as many reports as our upload can earn
    /// count, so a friend cannot keep a request alive by chattering.
    fn report(&self) -> Result<()> {
        let mut m = self.0.lock().unwrap();
        if m.reports == 0 {
            bail!("more progress reports than the request allows");
        }
        (m.reports, m.at) = (m.reports - 1, Instant::now());
        if let Some(t) = &m.tick {
            t.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn idle(&self) -> Duration {
        self.0.lock().unwrap().at.elapsed()
    }
}

/// One side of a request's stream; every byte through it counts as progress.
struct Watch<S>(S, Progress);

impl<S: AsyncRead + Unpin> AsyncRead for Watch<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let before = buf.filled().len();
        let r = Pin::new(&mut me.0).poll_read(cx, buf);
        me.1.moved(buf.filled().len() - before);
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watch<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        let r = Pin::new(&mut me.0).poll_write(cx, data);
        if let Poll::Ready(Ok(n)) = r {
            me.1.moved(n);
        }
        r
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// Runs blocking store work on a thread of its own: disk access, and waiting for an owner's
/// lock, must not hold up the async workers that serve everyone else.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| anyhow!("store task failed: {e}"))
}

impl Peer {
    fn quota(&self) -> u64 {
        self.friend.quota_bytes()
    }

    fn cloud(&self) -> Option<Arc<crate::cloud::Bucket>> {
        match &self.link {
            Link::Cloud(b) => Some(b.clone()),
            _ => None,
        }
    }

    /// Names the destination in front of whatever went wrong there.
    fn named<T>(&self, r: Result<T>) -> Result<T> {
        r.map_err(|e| anyhow!("{}: {e:#}", self.friend.name))
    }

    fn store(&self) -> Option<(Arc<Store>, String)> {
        match &self.link {
            Link::Drive(st, me) => Some((st.clone(), me.clone())),
            _ => None,
        }
    }

    /// Gives up on a friend once a request stops moving forward, so a silent one cannot hold
    /// everything else up, while a slow but steady one is waited for.
    async fn within<T>(&self, p: &Progress, f: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::pin!(f);
        loop {
            let idle = p.idle();
            if idle >= IDLE {
                bail!("{}: no answer in time", self.friend.name);
            }
            tokio::select! {
                r = &mut f => return r,
                _ = tokio::time::sleep(IDLE - idle) => {}
            }
        }
    }

    /// "direct to 1.2.3.4:5", "through the relay" or "local drive".
    pub async fn describe_link(&self) -> String {
        let conn = match &self.link {
            Link::Remote(conn) => conn,
            Link::Drive(..) => return "local drive".into(),
            Link::Cloud(b) => return format!("cloud storage ({})", b.place),
        };
        // A new connection starts on the relay; give it a moment to find a direct path.
        for _ in 0..12 {
            if conn.paths().iter().any(|p| p.is_selected() && !p.is_relay()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        match conn.paths().iter().find(|p| p.is_selected()) {
            Some(p) if p.is_relay() => "through the relay (still looking for a direct path)".to_string(),
            Some(p) => match p.remote_addr() {
                iroh::TransportAddr::Ip(a) => format!("direct to {a}"),
                other => format!("direct ({other:?})"),
            },
            None => "connecting".to_string(),
        }
    }

    /// Reached over the network, so the upload speed limit applies.
    pub fn is_remote(&self) -> bool {
        !matches!(self.link, Link::Drive(..))
    }

    async fn open(&self, req: &Req, p: &Progress) -> Result<(Watch<SendStream>, Watch<RecvStream>)> {
        let Link::Remote(conn) = &self.link else { bail!("not a network destination") };
        let (s, r) = conn.open_bi().await.map_err(|e| anyhow!("{}: {e}", self.friend.name))?;
        let (mut s, r) = (Watch(s, p.clone()), Watch(r, p.clone()));
        let msg = postcard::to_stdvec(req)?;
        p.expect(msg.len() + 4);
        proto::write_blob(&mut s, &msg).await?;
        Ok((s, r))
    }

    /// The friend's answer, after any progress reports that come ahead of it.
    async fn reply(&self, r: &mut Watch<RecvStream>) -> Result<Resp> {
        loop {
            match proto::recv(r).await.map_err(|e| anyhow!("{}: {e}", self.friend.name))? {
                Resp::Progress => r.1.report().map_err(|e| anyhow!("{}: {e}", self.friend.name))?,
                Resp::Err(e) => bail!("{}: {}", self.friend.name, e.chars().take(300).collect::<String>()),
                resp => return Ok(resp),
            }
        }
    }

    async fn simple(&self, req: Req) -> Result<Resp> {
        let p = Progress::new();
        self.within(&p, async {
            let (mut s, mut r) = self.open(&req, &p).await?;
            let _ = s.0.finish();
            self.reply(&mut r).await
        })
        .await
    }

    pub async fn pair(&self, req: Req) -> Result<String> {
        match self.simple(req).await? {
            Resp::Paired { name } => Ok(name),
            _ => bail!("unexpected reply"),
        }
    }

    pub async fn list_chunks(&self) -> Result<HashSet<Id>> {
        let implausible = || anyhow!("{}: implausible chunk count", self.friend.name);
        if let Some((st, me)) = self.store() {
            let set = blocking(move || {
                let mut set = HashSet::new();
                st.list_chunks(&me, |ids| {
                    set.extend(ids);
                    set.len() as u64 <= MAX_IDS
                });
                set
            })
            .await?;
            return if set.len() as u64 > MAX_IDS { Err(implausible()) } else { Ok(set) };
        }
        if let Some(b) = self.cloud() {
            return self.named(b.list_chunks().await);
        }
        let p = Progress::new();
        self.within(&p, async {
            let (mut s, mut r) = self.open(&Req::ListChunks, &p).await?;
            let _ = s.0.finish();
            let Resp::Ok = self.reply(&mut r).await? else { bail!("unexpected reply") };
            // Counted as they arrive, repeats included: memory and time grow only with what is sent.
            let (mut set, mut total) = (HashSet::new(), 0u64);
            loop {
                let ids = proto::read_ids(&mut r).await?;
                if ids.is_empty() {
                    return Ok(set);
                }
                total += ids.len() as u64;
                if total > MAX_IDS {
                    return Err(implausible());
                }
                set.extend(ids);
            }
        })
        .await
    }

    pub async fn put_chunks(&self, items: &[&(Id, Vec<u8>)]) -> Result<()> {
        self.put_chunks_ticking(items, None).await
    }

    /// Uploads chunks, bumping `tick` as the friend reports each piece it has read.
    pub async fn put_chunks_ticking(&self, items: &[&(Id, Vec<u8>)], tick: Option<Arc<AtomicU64>>) -> Result<()> {
        if let Some(b) = self.cloud() {
            return self.named(b.put_chunks(items, self.quota(), tick).await);
        }
        if let Some((st, me)) = self.store() {
            let (items, quota): (Vec<(Id, Vec<u8>)>, _) = (items.iter().map(|&i| i.clone()).collect(), self.quota());
            let name = self.friend.name.clone();
            return blocking(move || {
                for (id, data) in &items {
                    st.put_chunk(&me, id, data, quota).map_err(|e| anyhow!("{name}: {e}"))?;
                }
                Ok(())
            })
            .await?;
        }
        let p = Progress::new();
        p.0.lock().unwrap().tick = tick;
        self.within(&p, async {
            let (mut s, mut r) = self.open(&Req::PutChunks { count: items.len() as u32 }, &p).await?;
            for (_, data) in items {
                p.expect(data.len());
            }
            // The friend reports each piece it reads while we are still sending, so read as we write.
            let send = async {
                for (id, data) in items {
                    AsyncWriteExt::write_all(&mut s, id).await?;
                    proto::write_blob(&mut s, data).await?;
                }
                let _ = s.0.finish();
                Ok::<_, anyhow::Error>(())
            };
            match tokio::try_join!(send, self.reply(&mut r))? {
                ((), Resp::Ok) => Ok(()),
                _ => bail!("unexpected reply"),
            }
        })
        .await
    }

    pub async fn get_chunks(&self, ids: &[Id]) -> Result<Vec<Option<Vec<u8>>>> {
        if let Some((st, me)) = self.store() {
            let ids = ids.to_vec();
            return blocking(move || ids.iter().map(|id| st.get_chunk(&me, id)).collect()).await;
        }
        if let Some(b) = self.cloud() {
            return self.named(b.get_chunks(ids).await);
        }
        let p = Progress::new();
        self.within(&p, async {
            let (mut s, mut r) = self.open(&Req::GetChunks { ids: ids.to_vec() }, &p).await?;
            let _ = s.0.finish();
            self.reply(&mut r).await?;
            let mut out = Vec::with_capacity(ids.len());
            for _ in ids {
                out.push(proto::read_blob(&mut r, MAX_BLOB).await?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn delete_chunks(&self, ids: &[Id]) -> Result<()> {
        if let Some((st, me)) = self.store() {
            let ids = ids.to_vec();
            return blocking(move || st.delete_chunks(&me, &ids)).await;
        }
        if let Some(b) = self.cloud() {
            return self.named(b.delete_chunks(ids).await);
        }
        for batch in ids.chunks(MAX_DELETE) {
            self.simple(Req::DeleteChunks { ids: batch.to_vec() }).await?;
        }
        Ok(())
    }

    pub async fn put_snapshot(&self, id: &str, data: Vec<u8>) -> Result<()> {
        if let Some((st, me)) = self.store() {
            let (id, quota) = (id.to_string(), self.quota());
            return blocking(move || st.put_snapshot(&me, &id, &data, quota)).await?;
        }
        if let Some(b) = self.cloud() {
            return self.named(b.put_snapshot(id, data, self.quota()).await);
        }
        self.simple(Req::PutSnapshot { id: id.into(), data }).await.map(|_| ())
    }

    pub async fn list_snapshots(&self) -> Result<Vec<(String, Vec<u8>)>> {
        if let Some((st, me)) = self.store() {
            return blocking(move || st.list_snapshots(&me)).await?;
        }
        if let Some(b) = self.cloud() {
            return self.named(b.list_snapshots().await);
        }
        match self.simple(Req::ListSnapshots).await? {
            // Ids are the friend's word until the record decrypts: keep only well formed ones.
            Resp::Snapshots(v) => {
                Ok(v.into_iter().filter(|(id, b)| check_snap(id).is_ok() && b.len() <= MAX_SNAPSHOT).collect())
            }
            _ => bail!("unexpected reply"),
        }
    }

    pub async fn delete_snapshot(&self, id: &str) -> Result<()> {
        if let Some((st, me)) = self.store() {
            let id = id.to_string();
            return blocking(move || st.delete_snapshot(&me, &id)).await?;
        }
        if let Some(b) = self.cloud() {
            return self.named(b.delete_snapshot(id).await);
        }
        self.simple(Req::DeleteSnapshot { id: id.into() }).await.map(|_| ())
    }

    /// The sdb release the destination runs; None for a drive or a node too old to say.
    pub async fn version(&self) -> Option<String> {
        if !matches!(self.link, Link::Remote(_)) {
            return None;
        }
        match self.simple(Req::Version).await {
            Ok(Resp::Version(v)) => Some(v.chars().filter(|c| c.is_ascii_graphic()).take(24).collect()),
            _ => None,
        }
    }

    /// Whether the destination takes backups from this node. Only a friend's computer can say
    /// no; one too old to answer takes them.
    pub async fn accepts(&self) -> bool {
        !(matches!(self.link, Link::Remote(_)) && matches!(self.simple(Req::Accepts).await, Ok(Resp::Accepts(false))))
    }

    /// Usage counted afresh, for deciding by it right after deleting: cloud storage's running
    /// figure does not drop until its next listing.
    pub async fn usage_exact(&self) -> Result<(u64, u64)> {
        match self.cloud() {
            Some(b) => Ok((self.named(b.recount().await)?, self.quota())),
            None => self.usage().await,
        }
    }

    pub async fn usage(&self) -> Result<(u64, u64)> {
        if let Some((st, me)) = self.store() {
            return Ok((blocking(move || st.usage(&me)).await??, self.quota()));
        }
        if let Some(b) = self.cloud() {
            // Counting a large bucket takes a while: it runs to the end even if this caller gives up.
            let used = tokio::spawn(async move { b.usage().await }).await.map_err(|e| anyhow!("{e}"))?;
            return Ok((self.named(used)?, self.quota()));
        }
        match self.simple(Req::Usage).await? {
            Resp::Usage { used, quota } => Ok((used, quota)),
            _ => bail!("unexpected reply"),
        }
    }
}

#[derive(Clone)]
pub struct Handler(pub Arc<Node>);

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Handler")
    }
}

/// Limits that keep the listening side from being worn down.
pub struct Guard {
    /// Unknown nodes turned away since start.
    pub rejected: AtomicU64,
    /// Wrong invite codes presented since the last successful pairing.
    bad_invites: AtomicU32,
    /// How many unknown nodes may be mid-pairing at once.
    strangers: Semaphore,
    /// Open connections per friend.
    conns: Mutex<HashMap<String, u32>>,
    /// Per friend, the earliest moment more of their upload may be read; this is what
    /// holds a friend to the incoming speed limit set for them.
    pace: Mutex<HashMap<String, tokio::time::Instant>>,
    /// One listing at a time per friend: a listing is the largest reply this node builds.
    lists: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl Guard {
    /// Accounts for `bytes` just received from a friend and returns how long to wait
    /// before reading more, so their uploads average at most `kbps` and all friends'
    /// together at most `all_kbps`.
    fn owe(&self, friend: &str, bytes: usize, kbps: u64, all_kbps: u64) -> Duration {
        let now = tokio::time::Instant::now();
        let mut pace = self.pace.lock().unwrap();
        let mut wait = Duration::ZERO;
        for (who, kbps) in [(friend, kbps), (EVERYONE, all_kbps)] {
            if kbps == 0 {
                continue;
            }
            let cost = Duration::from_nanos((bytes as u128 * 1_000_000_000 / (kbps as u128 * 1024)) as u64);
            let slot = pace.entry(who.to_string()).or_insert(now);
            *slot = (*slot).max(now) + cost;
            wait = wait.max(slot.saturating_duration_since(now));
        }
        wait
    }

    fn listing(&self, friend: &str) -> Arc<Semaphore> {
        self.lists.lock().unwrap().entry(friend.to_string()).or_insert_with(|| Arc::new(Semaphore::new(1))).clone()
    }

    /// Counts a wrong invite code; too many of them close every open invite.
    fn wrong_invite(&self, node: &Node) {
        if self.bad_invites.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_BAD_INVITES {
            node.revoke_invites();
            self.bad_invites.store(0, Ordering::Relaxed);
            eprintln!("too many wrong invite codes: all open invites were cancelled");
        }
    }
}

impl Default for Guard {
    fn default() -> Self {
        Guard {
            rejected: AtomicU64::new(0),
            bad_invites: AtomicU32::new(0),
            strangers: Semaphore::new(MAX_STRANGERS),
            conns: Mutex::new(HashMap::new()),
            pace: Mutex::new(HashMap::new()),
            lists: Mutex::new(HashMap::new()),
        }
    }
}

/// What a friend is told when this node takes no backups.
const CLOSED: &str = "this computer does not accept backups from friends";
/// The pace entry shared by all friends; no friend has an empty id.
const EVERYONE: &str = "";
const MAX_STRANGERS: usize = 4;
const MAX_BAD_INVITES: u32 = 50;
const MAX_CONNS_PER_FRIEND: u32 = 4;
const MAX_REQUESTS_PER_CONN: usize = 8;
/// Largest request a friend may send; chunk data is streamed separately and capped by MAX_BLOB.
const MAX_REQUEST: usize = 8 << 20;
const PAIR_REQUEST: usize = 4096;

impl ProtocolHandler for Handler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let node = &self.0;
        let remote = conn.remote_id().to_string();
        let is_friend = |n: &Node| n.cfg().friends.iter().any(|f| f.id == remote && f.is_friend());
        if !is_friend(node) {
            // An unknown node gets nothing, except one small, short pairing attempt while an invite is open.
            node.guard.rejected.fetch_add(1, Ordering::Relaxed);
            let now = chrono::Utc::now().timestamp();
            let invite_open = node.cfg().invites.iter().any(|i| i.expires > now);
            if let (true, Ok(_slot)) = (invite_open, node.guard.strangers.try_acquire()) {
                let attempt = async {
                    let (mut s, mut r) = conn.accept_bi().await.ok()?;
                    if serve(node, &remote, &mut s, &mut r, PAIR_REQUEST).await.is_err() {
                        // A stranger learns nothing about this computer from the failure.
                        let msg = "that invite is invalid, already used or expired";
                        let _ = proto::send(&mut s, &Resp::Err(msg.into())).await;
                    }
                    let _ = s.finish();
                    let _ = s.stopped().await;
                    Some(())
                };
                let _ = tokio::time::timeout(Duration::from_secs(15), attempt).await;
                if is_friend(node) {
                    node.guard.bad_invites.store(0, Ordering::Relaxed);
                    node.guard.rejected.fetch_sub(1, Ordering::Relaxed);
                } else {
                    // Someone may be guessing: enough of these close the door until a new invite is made.
                    node.guard.wrong_invite(node);
                }
            }
            if !is_friend(node) {
                conn.close(1u32.into(), b"not a friend");
                return Ok(());
            }
        }
        let open = {
            let mut c = node.guard.conns.lock().unwrap();
            let n = c.entry(remote.clone()).or_insert(0);
            *n += 1;
            *n
        };
        if open <= MAX_CONNS_PER_FRIEND {
            // A friend who reached us gives us a working path back to them, whichever side could dial.
            node.adopt(&remote, &conn);
            serve_conn(node.clone(), conn, remote.clone()).await;
        } else {
            conn.close(1u32.into(), b"too many connections");
        }
        if let Some(n) = node.guard.conns.lock().unwrap().get_mut(&remote) {
            *n = n.saturating_sub(1);
        }
        Ok(())
    }
}

/// Answers a friend's requests on a connection, whichever side opened it.
/// Only a few requests run at once per connection, which bounds memory.
pub async fn serve_conn(node: Arc<Node>, conn: Connection, remote: String) {
    let slots = Arc::new(Semaphore::new(MAX_REQUESTS_PER_CONN));
    loop {
        let Ok(slot) = slots.clone().acquire_owned().await else {
            break;
        };
        let Ok((mut s, mut r)) = conn.accept_bi().await else {
            break;
        };
        let node = node.clone();
        let remote = remote.clone();
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(e) = serve(&node, &remote, &mut s, &mut r, MAX_REQUEST).await {
                let _ = proto::send(&mut s, &Resp::Err(format!("{e:#}"))).await;
            }
            let _ = s.finish();
        });
    }
}

/// Runs store work for a friend off the async workers.
async fn on_store<T: Send + 'static>(
    node: &Arc<Node>,
    owner: &str,
    f: impl FnOnce(&Store, &str) -> T + Send + 'static,
) -> Result<T> {
    let (node, owner) = (node.clone(), owner.to_string());
    blocking(move || f(&node.store, &owner)).await
}

/// How often a friend's long upload checks they are still a friend.
const RECHECK: Duration = Duration::from_secs(1);

/// Reads `buf` from a friend a piece at a time, each only once their speed limit allows it,
/// reporting progress after each piece of a request longer than one. `extra` framing bytes are
/// charged with the first piece.
async fn read_paced(
    node: &Node,
    friend: &Friend,
    s: &mut SendStream,
    r: &mut RecvStream,
    buf: &mut [u8],
    mut extra: usize,
    report: bool,
) -> Result<()> {
    let mut at = 0;
    let all_kbps = node.cfg().incoming_kbps;
    loop {
        let n = (buf.len() - at).min(PIECE);
        tokio::time::sleep(node.guard.owe(&friend.id, n + extra, friend.max_in_kbps, all_kbps)).await;
        r.read_exact(&mut buf[at..at + n]).await?;
        // The sender hears its data is moving however slow the limit, so only a silent node is given up on.
        if report {
            proto::send(s, &Resp::Progress).await?;
        }
        (at, extra) = (at + n, 0);
        if at == buf.len() {
            return Ok(());
        }
    }
}

async fn serve(node: &Arc<Node>, remote: &str, s: &mut SendStream, r: &mut RecvStream, max: usize) -> Result<()> {
    let friend = node.friend(remote);
    let len = r.read_u32().await? as usize;
    if len > max {
        bail!("message too large ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    match &friend {
        // Every byte a friend sends counts toward their speed limit, whatever the request.
        Some(f) => read_paced(node, f, s, r, &mut buf, 4, len > PIECE).await?,
        None => r.read_exact(&mut buf).await?,
    }
    let req: Req = postcard::from_bytes(&buf)?;
    drop(buf);
    if let Req::Pair { token, name, relay, addrs } = req {
        let res = node.accept_pair(remote, &token, &name, relay, addrs);
        if res.is_err() && friend.is_some() {
            node.guard.wrong_invite(node);
        }
        return proto::send(s, &Resp::Paired { name: res? }).await;
    }
    let Some(friend) = friend else {
        bail!("you are not on this node's friend list");
    };
    crate::store::check_owner(remote)?;
    let quota = friend.quota_gb.saturating_mul(1 << 30);
    let accepting = node.accepting();
    match req {
        Req::Pair { .. } => unreachable!(),
        Req::ListChunks => {
            let _one = node.guard.listing(remote).acquire_owned().await?;
            proto::send(s, &Resp::Ok).await?;
            // Walked on a blocking thread and sent as it goes: only a couple of pieces are ever in memory,
            // and the walk stops as soon as the friend stops reading.
            let (tx, mut rx) = tokio::sync::mpsc::channel(2);
            let (n, owner) = (node.clone(), remote.to_string());
            let walk =
                tokio::task::spawn_blocking(move || n.store.list_chunks(&owner, |ids| tx.blocking_send(ids).is_ok()));
            while let Some(ids) = rx.recv().await {
                proto::write_ids(s, &ids).await?;
            }
            walk.await?;
            proto::write_ids(s, &[]).await?;
        }
        Req::PutChunks { count } => {
            // Read to the end even when refused, so the sender gets the reason rather than a broken stream.
            let (mut err, mut checked) = ((!accepting).then(|| anyhow!(CLOSED)), Instant::now());
            for _ in 0..count {
                let id = proto::read_id(r).await?;
                let len = r.read_u32().await? as usize;
                if len > MAX_BLOB {
                    bail!("bad chunk");
                }
                // The id and length count too, and no chunk costs less than a disk block, so a stream
                // of empty chunks is paced like real data.
                let extra = cost(len as u64 + 36) as usize - len;
                let mut data = vec![0u8; len];
                read_paced(node, &friend, s, r, &mut data, extra, true).await?;
                // A friend removed while their upload runs stops here; the store refuses a purged one at once.
                if checked.elapsed() >= RECHECK {
                    if node.friend(remote).is_none() {
                        bail!("you are not on this node's friend list");
                    }
                    checked = Instant::now();
                }
                if err.is_none() {
                    err = on_store(node, remote, move |st, o| st.put_chunk(o, &id, &data, quota)).await?.err();
                }
            }
            if let Some(e) = err {
                return Err(e);
            }
            proto::send(s, &Resp::Ok).await?;
        }
        Req::GetChunks { ids } => {
            proto::send(s, &Resp::Ok).await?;
            for id in ids {
                match on_store(node, remote, move |st, o| st.get_chunk(o, &id)).await? {
                    Some(d) => proto::write_blob(s, &d).await?,
                    None => proto::write_missing(s).await?,
                }
            }
        }
        Req::DeleteChunks { ids } => {
            if ids.len() > MAX_DELETE {
                bail!("too many chunks in one delete request");
            }
            on_store(node, remote, move |st, o| st.delete_chunks(o, &ids)).await?;
            proto::send(s, &Resp::Ok).await?;
        }
        Req::PutSnapshot { .. } if !accepting => bail!(CLOSED),
        Req::PutSnapshot { id, data } => {
            on_store(node, remote, move |st, o| st.put_snapshot(o, &id, &data, quota)).await??;
            proto::send(s, &Resp::Ok).await?;
        }
        Req::ListSnapshots => {
            let _one = node.guard.listing(remote).acquire_owned().await?;
            let snaps = on_store(node, remote, |st, o| st.list_snapshots(o)).await??;
            proto::send(s, &Resp::Snapshots(snaps)).await?
        }
        Req::DeleteSnapshot { id } => {
            on_store(node, remote, move |st, o| st.delete_snapshot(o, &id)).await??;
            proto::send(s, &Resp::Ok).await?;
        }
        Req::Usage => {
            let used = on_store(node, remote, |st, o| st.usage(o)).await??;
            proto::send(s, &Resp::Usage { used, quota }).await?
        }
        Req::Version => proto::send(s, &Resp::Version(VERSION.into())).await?,
        Req::Accepts => proto::send(s, &Resp::Accepts(accepting)).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_uploads_are_paced_too() {
        let g = Guard::default();
        // 1000 chunks of 1000 bytes at 1000 KB/s owe about a second, not nothing.
        let mut owed = Duration::ZERO;
        for _ in 0..1000 {
            owed = g.owe("f", 1000, 1000, 0);
        }
        assert!(owed > Duration::from_millis(900) && owed < Duration::from_millis(1100), "{owed:?}");
        assert_eq!(g.owe("f", 1 << 20, 0, 0), Duration::ZERO, "no limit means no waiting");
    }

    #[test]
    fn friends_share_the_limit_for_everyone() {
        let g = Guard::default();
        // Two friends sending 500 KB each against 1000 KB/s for all owe a second between them.
        let mut owed = Duration::ZERO;
        for friend in ["a", "b"] {
            for _ in 0..500 {
                owed = g.owe(friend, 1024, 0, 1000);
            }
        }
        assert!(owed > Duration::from_millis(900) && owed < Duration::from_millis(1100), "{owed:?}");
        // A friend's own lower limit still holds them back further.
        assert!(g.owe("c", 1024 * 100, 10, 1000) > Duration::from_secs(9));
    }

    #[test]
    fn only_public_https_relays_are_dialed() {
        for ok in ["https://use1-1.relay.n0.iroh.link./", "https://relay.example.com", "https://8.8.8.8/"] {
            assert!(valid_relay(ok), "{ok}");
        }
        for bad in [
            "http://relay.example.com/",
            "https://127.0.0.1:8080/",
            "https://localhost/",
            "https://10.1.2.3/",
            "https://192.168.1.1/",
            "https://169.254.169.254/",
            "https://[::1]/",
            "https://[fd00::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://100.64.1.1/",
            "https://224.0.0.1/",
            "https://0.1.2.3/",
            "https://[ff02::1]/",
            "https://relay.example.com/internal-admin/",
            "not a url",
        ] {
            assert!(!valid_relay(bad), "{bad}");
        }
    }
}
