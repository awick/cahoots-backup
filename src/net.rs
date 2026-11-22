// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Friend-to-friend protocol: one request per QUIC stream.

use crate::{
    config::Friend,
    crypto::Id,
    node::Node,
    proto::{self, ALPN, ID_BATCH, MAX_BLOB, MAX_DELETE, MAX_IDS, MAX_SNAPSHOT, PIECE, Req, Resp},
    store::{Store, check_snap, cost},
};
use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, PublicKey, RelayMode, RelayUrl, SecretKey, TransportAddr,
    address_lookup::{
        AddressLookup, AddressLookupBuilder, AddressLookupBuilderError, DnsAddressLookup, EndpointData, EndpointInfo,
        Item, PkarrPublisher, PkarrResolver,
    },
    dns::{BoxIter, DNS_TIMEOUT, DnsError, DnsResolver, Resolver, TxtRecordData},
    endpoint::{Connection, RecvStream, SendStream, presets},
    protocol::{AcceptError, ProtocolHandler},
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
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
        // What N0 sets up, except that what a lookup says about a friend (which they sign and so
        // choose) passes the same filter as their hints, and a name iroh resolves, a relay's
        // included, only ever yields public addresses, so the address checked is the one dialed.
        Endpoint::builder(presets::Minimal)
            .address_lookup(PkarrPublisher::n0_dns())
            .address_lookup(Public(PkarrResolver::n0_dns()))
            .address_lookup(Public(DnsAddressLookup::n0_dns()))
            .relay_mode(iroh::endpoint::default_relay_mode())
            .dns_resolver(DnsResolver::custom(PublicDns(DnsResolver::new())))
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
/// a link-local, carrier-NAT, unspecified, broadcast, multicast, reserved, documentation
/// or benchmarking address, nor an IPv6 form that carries one of those inside it.
pub(crate) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => public_v4(a),
        IpAddr::V6(a) => public_v6(a),
    }
}

fn public_v4(a: Ipv4Addr) -> bool {
    let [o0, o1, o2, _] = a.octets();
    !(a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_unspecified()
        || a.is_broadcast()
        || a.is_multicast()
        || a.is_documentation()
        || o0 == 0
        || o0 >= 240
        || (o0 == 100 && o1 & 0xc0 == 64)
        || (o0 == 192 && o1 == 0 && o2 == 0)
        || (o0 == 192 && o1 == 88 && o2 == 99)
        || (o0 == 198 && o1 & 0xfe == 18))
}

fn public_v6(a: Ipv6Addr) -> bool {
    let s = a.segments();
    let v4 = |hi: u16, lo: u16| Ipv4Addr::from((hi as u32) << 16 | lo as u32);
    if let Some(v4) = a.to_ipv4_mapped() {
        return public_v4(v4);
    }
    match s {
        // NAT64 and 6to4 reach the IPv4 address inside them, so that address decides.
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] | [0x2002, hi, lo, ..] => public_v4(v4(hi, lo)),
        // Otherwise only global unicast, less its protocol (Teredo, benchmarking, ORCHID) and
        // documentation ranges. Everything else (IPv4-compatible, local NAT64, discard,
        // unique-local, link-local, multicast) is not the open internet.
        _ => {
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && s[1] < 0x200)
                && !(s[0] == 0x2001 && s[1] == 0xdb8)
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

/// Whether an address a lookup returned for a friend may be dialed: what their hints may be.
fn dialable_addr(a: &TransportAddr) -> bool {
    match a {
        TransportAddr::Ip(s) => s.port() != 0 && public_ip(s.ip()),
        TransportAddr::Relay(u) => valid_relay(u.as_str()),
        _ => false,
    }
}

fn public_only(item: Item) -> Item {
    let info = item.endpoint_info();
    let mut data = EndpointData::new(info.addrs().filter(|a| dialable_addr(a)).cloned().collect());
    data.set_user_data(info.user_data().cloned());
    Item::new(EndpointInfo::from_parts(item.endpoint_id(), data), item.provenance(), item.last_updated())
}

/// An address lookup whose answers keep only what `dialable_addr` allows.
#[derive(Debug)]
struct Public<B>(B);

#[derive(Debug)]
struct PublicLookup<L>(L);

impl<B: AddressLookupBuilder> AddressLookupBuilder for Public<B> {
    fn into_address_lookup(self, ep: &Endpoint) -> Result<impl AddressLookup, AddressLookupBuilderError> {
        Ok(PublicLookup(self.0.into_address_lookup(ep)?))
    }
}

type Found = Result<Item, iroh::address_lookup::Error>;

impl<L: AddressLookup> AddressLookup for PublicLookup<L> {
    fn resolve(&self, id: EndpointId) -> Option<Pin<Box<dyn futures::Stream<Item = Found> + Send + 'static>>> {
        Some(Box::pin(self.0.resolve(id)?.map(|r| r.map(public_only))))
    }
}

/// A DNS resolver that drops every answer that is not a public address. iroh dials a relay
/// at the addresses it resolved, so a name cannot pass a check and then rebind inward.
#[derive(Debug, Clone)]
struct PublicDns(DnsResolver);

impl Resolver for PublicDns {
    fn lookup_ipv4(&self, host: String) -> Pin<Box<dyn Future<Output = Result<BoxIter<Ipv4Addr>, DnsError>> + Send>> {
        let dns = self.0.clone();
        Box::pin(async move {
            let ips: Vec<_> = dns
                .lookup_ipv4(host, DNS_TIMEOUT)
                .await?
                .filter_map(|ip| match ip {
                    IpAddr::V4(a) if public_v4(a) => Some(a),
                    _ => None,
                })
                .collect();
            Ok(Box::new(ips.into_iter()) as BoxIter<_>)
        })
    }

    fn lookup_ipv6(&self, host: String) -> Pin<Box<dyn Future<Output = Result<BoxIter<Ipv6Addr>, DnsError>> + Send>> {
        let dns = self.0.clone();
        Box::pin(async move {
            let ips: Vec<_> = dns
                .lookup_ipv6(host, DNS_TIMEOUT)
                .await?
                .filter_map(|ip| match ip {
                    IpAddr::V6(a) if public_v6(a) => Some(a),
                    _ => None,
                })
                .collect();
            Ok(Box::new(ips.into_iter()) as BoxIter<_>)
        })
    }

    fn lookup_txt(
        &self,
        host: String,
    ) -> Pin<Box<dyn Future<Output = Result<BoxIter<TxtRecordData>, DnsError>> + Send>> {
        let dns = self.0.clone();
        Box::pin(async move {
            let txt: Vec<_> = dns.lookup_txt(host, DNS_TIMEOUT).await?.collect();
            Ok(Box::new(txt.into_iter()) as BoxIter<_>)
        })
    }

    fn clear_cache(&self) {
        self.0.clear_cache();
    }

    fn reset(&self) -> Box<dyn Resolver> {
        self.0.reset();
        Box::new(self.clone())
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

/// A destination's chunk listing, sorted against what the caller wanted (see `Peer::list_chunks`).
pub struct Listing {
    wanted: Arc<HashSet<Id>>,
    others: usize,
    /// Listed chunks that were wanted.
    pub have: HashSet<Id>,
    /// Listed chunks that were not, at most `others` of them.
    pub other: Vec<Id>,
    /// Every id listed, repeats included.
    pub count: u64,
}

impl Listing {
    fn take(&mut self, ids: impl IntoIterator<Item = Id>) {
        for id in ids {
            self.count += 1;
            if self.wanted.contains(&id) {
                self.have.insert(id);
            } else if self.other.len() < self.others {
                self.other.push(id);
            }
        }
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
/// Besides never going quiet, a request must average at least this much per IDLE, so a friend
/// that keeps one alive with a step every IDLE cannot hold it for days. Our upload goes at the
/// friend's speed limit, which may be very low, so it need only make a piece (about 220 B/s).
const SLOW_UP: u64 = PIECE as u64;
/// Their reply has no speed limit behind it: about 7 KB/s, slower than any link worth using.
const SLOW_DOWN: u64 = 2 << 20;

/// How long a request may take in all, having moved `sent` and `got` bytes so far.
fn allowed(sent: u64, got: u64) -> Duration {
    IDLE.mul_f64(1.0 + sent as f64 / SLOW_UP as f64 + got as f64 / SLOW_DOWN as f64)
}

/// When a request last moved forward, and how many progress reports the friend may still send.
struct Moved {
    at: Instant,
    bytes: usize,
    start: Instant,
    sent: u64,
    got: u64,
    reports: u64,
    /// Bumped on each report, for a caller that watches a long upload move.
    tick: Option<Arc<AtomicU64>>,
}

#[derive(Clone)]
struct Progress(Arc<Mutex<Moved>>);

impl Progress {
    fn new() -> Self {
        let now = Instant::now();
        Progress(Arc::new(Mutex::new(Moved { at: now, bytes: 0, start: now, sent: 0, got: 0, reports: 0, tick: None })))
    }

    fn moved(&self, n: usize, sent: bool) {
        let mut m = self.0.lock().unwrap();
        m.bytes += n;
        if sent {
            m.sent += n as u64;
        } else {
            m.got += n as u64;
        }
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

    /// How long the request has been quiet, and how much longer its pace lets it run.
    fn idle(&self) -> (Duration, Duration) {
        let m = self.0.lock().unwrap();
        (m.at.elapsed(), allowed(m.sent, m.got).saturating_sub(m.start.elapsed()))
    }
}

/// One side of a request's stream; every byte through it counts as progress.
struct Watch<S>(S, Progress);

impl<S: AsyncRead + Unpin> AsyncRead for Watch<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let before = buf.filled().len();
        let r = Pin::new(&mut me.0).poll_read(cx, buf);
        me.1.moved(buf.filled().len() - before, false);
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watch<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        let r = Pin::new(&mut me.0).poll_write(cx, data);
        if let Poll::Ready(Ok(n)) = r {
            me.1.moved(n, true);
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

/// Most chunk ids a place that holds `bytes` of ours can list: no chunk there costs less than a
/// disk block. A batch more allows for uploads that land while it is listed.
fn plausible(bytes: u64) -> u64 {
    (bytes / cost(0)).saturating_add(ID_BATCH as u64).min(MAX_IDS)
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

    /// Gives up on a friend once a request stops moving forward, or falls behind the slowest
    /// pace it may keep, so neither a silent nor a trickling one can hold everything else up,
    /// while a slow but steady one is waited for.
    async fn within<T>(&self, p: &Progress, f: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::pin!(f);
        loop {
            let (idle, left) = p.idle();
            if idle >= IDLE {
                bail!("{}: no answer in time", self.friend.name);
            }
            if left.is_zero() {
                bail!("{}: answering too slowly", self.friend.name);
            }
            tokio::select! {
                r = &mut f => return r,
                _ = tokio::time::sleep((IDLE - idle).min(left)) => {}
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

    /// The chunks the destination holds for us, sorted as they arrive: those in `wanted` are kept,
    /// up to `others` of the rest are handed back (to delete), and anything beyond is only counted.
    /// Memory so follows what this node itself knows, never what the other side claims to hold: a
    /// listing is the one reply whose size it picks, and names on a drive or in a bucket can be
    /// planted. Past what it could plausibly hold, the listing is refused.
    pub async fn list_chunks(&self, wanted: Arc<HashSet<Id>>, others: usize) -> Result<Listing> {
        let implausible = || anyhow!("{}: implausible chunk count", self.friend.name);
        let mut l = Listing { wanted, others, have: HashSet::new(), other: vec![], count: 0 };
        if let Some((st, me)) = self.store() {
            // By the cap we set, not by what is on the drive, which planted files would swell.
            let most = plausible(self.quota());
            let l = blocking(move || {
                st.list_chunks(&me, |ids| {
                    l.take(ids);
                    l.count <= most
                });
                l
            })
            .await?;
            return if l.count > most { Err(implausible()) } else { Ok(l) };
        }
        if let Some(b) = self.cloud() {
            self.named(b.list_chunks(|id| l.take([id])).await)?;
            return Ok(l);
        }
        let (used, quota) = self.usage().await?;
        let most = plausible(used.max(quota));
        let p = Progress::new();
        self.within(&p, async {
            let (mut s, mut r) = self.open(&Req::ListChunks, &p).await?;
            let _ = s.0.finish();
            let Resp::Ok = self.reply(&mut r).await? else { bail!("unexpected reply") };
            // Counted as they arrive, repeats included: time grows only with what is sent.
            loop {
                let ids = proto::read_ids(&mut r).await?;
                if ids.is_empty() {
                    return Ok(l);
                }
                l.take(ids);
                if l.count > most {
                    return Err(implausible());
                }
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
    let (mut kbps, mut all_kbps, mut checked) = (friend.max_in_kbps, node.cfg().incoming_kbps, Instant::now());
    loop {
        // A speed limit changed while a large piece of data comes in applies from the next piece.
        if checked.elapsed() >= RECHECK {
            kbps = node.friend(&friend.id).map_or(kbps, |f| f.max_in_kbps);
            (all_kbps, checked) = (node.cfg().incoming_kbps, Instant::now());
        }
        let n = (buf.len() - at).min(PIECE);
        tokio::time::sleep(node.guard.owe(&friend.id, n + extra, kbps, all_kbps)).await;
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
            let (mut friend, mut quota) = (friend.clone(), quota);
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
                // The owner's current terms hold for a long upload too: a friend removed while it
                // runs stops here (the store refuses a purged one at once), and a closed door, a
                // lower quota or a new speed limit apply from the next chunk.
                if checked.elapsed() >= RECHECK {
                    friend = node.friend(remote).ok_or_else(|| anyhow!("you are not on this node's friend list"))?;
                    quota = friend.quota_gb.saturating_mul(1 << 30);
                    if !node.accepting() {
                        err.get_or_insert_with(|| anyhow!(CLOSED));
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

    #[test]
    fn forms_that_reach_inward_are_not_public() {
        for ok in ["8.8.8.8", "1.1.1.1", "2606:4700::1111", "64:ff9b::808:808", "2002:808:808::1", "::ffff:8.8.8.8"] {
            assert!(public_ip(ok.parse().unwrap()), "{ok}");
        }
        for bad in [
            "198.18.0.1",
            "198.19.255.255",
            "192.0.0.170",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "240.0.0.1",
            "255.255.255.255",
            "192.88.99.1",
            "64:ff9b::a00:1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::a00:1",
            "2002:a00:1::1",
            "2002:7f00:1::",
            "::a00:1",
            "::7f00:1",
            "2001::1",
            "2001:2::1",
            "2001:db8::1",
            "3fff::1",
            "100::1",
            "fec0::1",
            "5f00::1",
        ] {
            assert!(!public_ip(bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn lookups_keep_only_what_a_hint_could_name() {
        let id = SecretKey::from_bytes(&[7; 32]).public();
        let (public, inward): (SocketAddr, SocketAddr) =
            ("8.8.8.8:4433".parse().unwrap(), "10.0.0.1:161".parse().unwrap());
        let info = EndpointInfo::new(id)
            .with_relay_url("http://127.0.0.1:8080/".parse().unwrap())
            .with_relay_url("https://relay.example.com/".parse().unwrap())
            .with_ip_addrs(vec![public, inward]);
        let kept = public_only(Item::new(info, "test", None)).into_endpoint_addr();
        assert_eq!(kept.ip_addrs().collect::<Vec<_>>(), [&public]);
        assert_eq!(kept.relay_urls().map(|u| u.to_string()).collect::<Vec<_>>(), ["https://relay.example.com/"]);
    }

    #[test]
    fn a_request_must_keep_a_pace() {
        assert_eq!(allowed(0, 0), IDLE);
        // A trickle of one step per IDLE earns almost nothing, where real data earns its time.
        assert!(allowed(0, 100 * STEP as u64) < IDLE * 2);
        assert!(allowed(8 << 20, 0) >= IDLE * 128, "an upload at a very low speed limit still fits");
        assert_eq!(plausible(0), ID_BATCH as u64);
        assert_eq!(plausible(1 << 30), (1 << 18) + ID_BATCH as u64);
        assert_eq!(plausible(u64::MAX), MAX_IDS);
    }
}
