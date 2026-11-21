// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Friend-to-friend protocol: one request per QUIC stream.

use crate::{
    config::Friend,
    crypto::Id,
    node::Node,
    proto::{self, ALPN, MAX_BLOB, Req, Resp},
    store::Store,
};
use anyhow::{Result, anyhow, bail};
use iroh::{
    Endpoint, EndpointAddr, PublicKey, RelayMode, SecretKey,
    endpoint::{Connection, RecvStream, SendStream, presets},
    protocol::{AcceptError, ProtocolHandler},
};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

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
    if let Some(r) = relay {
        if let Ok(url) = r.parse() {
            addr = addr.with_relay_url(url);
        }
    }
    for a in addrs {
        addr = addr.with_ip_addr(*a);
    }
    Ok(addr)
}

pub async fn connect(ep: &Endpoint, addr: EndpointAddr) -> Result<Connection> {
    match tokio::time::timeout(Duration::from_secs(15), ep.connect(addr, ALPN)).await {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => bail!("connect failed: {e}"),
        Err(_) => bail!("connect timed out"),
    }
}

/// Where a destination's data lives: a friend over the network or a local drive.
#[derive(Clone)]
pub enum Link {
    Remote(Connection),
    Drive(Arc<Store>, String),
}

#[derive(Clone)]
pub struct Peer {
    pub friend: Friend,
    pub link: Link,
}

impl Peer {
    fn quota(&self) -> u64 {
        self.friend.quota_gb << 30
    }

    /// "direct to 1.2.3.4:5", "through the relay" or "local drive".
    pub async fn describe_link(&self) -> String {
        let Link::Remote(conn) = &self.link else {
            return "local drive".into();
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

    pub fn is_remote(&self) -> bool {
        matches!(self.link, Link::Remote(_))
    }

    async fn open(&self, req: &Req) -> Result<(SendStream, RecvStream)> {
        let Link::Remote(conn) = &self.link else { bail!("not a network destination") };
        let (mut s, r) = conn.open_bi().await.map_err(|e| anyhow!("{}: {e}", self.friend.name))?;
        proto::send(&mut s, req).await?;
        Ok((s, r))
    }

    async fn reply(&self, r: &mut RecvStream) -> Result<Resp> {
        match proto::recv(r).await.map_err(|e| anyhow!("{}: {e}", self.friend.name))? {
            Resp::Err(e) => bail!("{}: {e}", self.friend.name),
            resp => Ok(resp),
        }
    }

    async fn simple(&self, req: Req) -> Result<Resp> {
        let (mut s, mut r) = self.open(&req).await?;
        let _ = s.finish();
        self.reply(&mut r).await
    }

    pub async fn pair(&self, req: Req) -> Result<String> {
        match self.simple(req).await? {
            Resp::Paired { name } => Ok(name),
            _ => bail!("unexpected reply"),
        }
    }

    pub async fn list_chunks(&self) -> Result<HashSet<Id>> {
        if let Link::Drive(st, me) = &self.link {
            return Ok(st.list_chunks(me).into_iter().collect());
        }
        let (mut s, mut r) = self.open(&Req::ListChunks).await?;
        let _ = s.finish();
        let Resp::Count(n) = self.reply(&mut r).await? else { bail!("unexpected reply") };
        let mut set = HashSet::with_capacity(n as usize);
        for _ in 0..n {
            set.insert(proto::read_id(&mut r).await?);
        }
        Ok(set)
    }

    pub async fn put_chunks(&self, items: &[&(Id, Vec<u8>)]) -> Result<()> {
        if let Link::Drive(st, me) = &self.link {
            for (id, data) in items {
                st.put_chunk(me, id, data, self.quota()).map_err(|e| anyhow!("{}: {e}", self.friend.name))?;
            }
            return Ok(());
        }
        let (mut s, mut r) = self.open(&Req::PutChunks { count: items.len() as u32 }).await?;
        for (id, data) in items {
            AsyncWriteExt::write_all(&mut s, id).await?;
            proto::write_blob(&mut s, data).await?;
        }
        let _ = s.finish();
        self.reply(&mut r).await.map(|_| ())
    }

    pub async fn get_chunks(&self, ids: &[Id]) -> Result<Vec<Option<Vec<u8>>>> {
        if let Link::Drive(st, me) = &self.link {
            return Ok(ids.iter().map(|id| st.get_chunk(me, id)).collect());
        }
        let (mut s, mut r) = self.open(&Req::GetChunks { ids: ids.to_vec() }).await?;
        let _ = s.finish();
        self.reply(&mut r).await?;
        let mut out = Vec::with_capacity(ids.len());
        for _ in ids {
            out.push(proto::read_blob(&mut r, MAX_BLOB).await?);
        }
        Ok(out)
    }

    pub async fn delete_chunks(&self, ids: &[Id]) -> Result<()> {
        if let Link::Drive(st, me) = &self.link {
            st.delete_chunks(me, ids);
            return Ok(());
        }
        for batch in ids.chunks(10_000) {
            self.simple(Req::DeleteChunks { ids: batch.to_vec() }).await?;
        }
        Ok(())
    }

    pub async fn put_snapshot(&self, id: &str, data: Vec<u8>) -> Result<()> {
        if let Link::Drive(st, me) = &self.link {
            return st.put_snapshot(me, id, &data, self.quota());
        }
        self.simple(Req::PutSnapshot { id: id.into(), data }).await.map(|_| ())
    }

    pub async fn list_snapshots(&self) -> Result<Vec<(String, Vec<u8>)>> {
        if let Link::Drive(st, me) = &self.link {
            return Ok(st.list_snapshots(me));
        }
        match self.simple(Req::ListSnapshots).await? {
            Resp::Snapshots(v) => Ok(v),
            _ => bail!("unexpected reply"),
        }
    }

    pub async fn delete_snapshot(&self, id: &str) -> Result<()> {
        if let Link::Drive(st, me) = &self.link {
            return st.delete_snapshot(me, id);
        }
        self.simple(Req::DeleteSnapshot { id: id.into() }).await.map(|_| ())
    }

    /// The sdb release the destination runs; None for a drive or a node too old to say.
    pub async fn version(&self) -> Option<String> {
        if !self.is_remote() {
            return None;
        }
        match self.simple(Req::Version).await {
            Ok(Resp::Version(v)) => Some(v.chars().filter(|c| c.is_ascii_graphic()).take(24).collect()),
            _ => None,
        }
    }

    pub async fn usage(&self) -> Result<(u64, u64)> {
        if let Link::Drive(st, me) = &self.link {
            return Ok((st.usage(me)?, self.quota()));
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
}

impl Guard {
    /// Accounts for `bytes` just received from a friend and returns how long to wait
    /// before reading more, so their uploads average at most `kbps`.
    fn owe(&self, friend: &str, bytes: usize, kbps: u64) -> Duration {
        if kbps == 0 {
            return Duration::ZERO;
        }
        let now = tokio::time::Instant::now();
        let cost = Duration::from_millis(bytes as u64 * 1000 / (kbps * 1024));
        let mut pace = self.pace.lock().unwrap();
        let slot = pace.entry(friend.to_string()).or_insert(now);
        *slot = (*slot).max(now) + cost;
        slot.saturating_duration_since(now)
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
        }
    }
}

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
        let is_friend = |n: &Node| n.cfg().friends.iter().any(|f| f.id == remote && f.path.is_none());
        if !is_friend(node) {
            // An unknown node gets nothing, except one small, short pairing attempt while an invite is open.
            node.guard.rejected.fetch_add(1, Ordering::Relaxed);
            let now = chrono::Utc::now().timestamp();
            let invite_open = node.cfg().invites.iter().any(|i| i.expires > now);
            if let (true, Ok(_slot)) = (invite_open, node.guard.strangers.try_acquire()) {
                let attempt = async {
                    let (mut s, mut r) = conn.accept_bi().await.ok()?;
                    if let Err(e) = serve(node, &remote, &mut s, &mut r, PAIR_REQUEST).await {
                        let _ = proto::send(&mut s, &Resp::Err(format!("{e:#}"))).await;
                    }
                    let _ = s.finish();
                    let _ = s.stopped().await;
                    Some(())
                };
                let _ = tokio::time::timeout(Duration::from_secs(15), attempt).await;
                if is_friend(node) {
                    node.guard.bad_invites.store(0, Ordering::Relaxed);
                    node.guard.rejected.fetch_sub(1, Ordering::Relaxed);
                } else if node.guard.bad_invites.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_BAD_INVITES {
                    // Someone is guessing: close the door until the owner makes a new invite.
                    node.revoke_invites();
                    node.guard.bad_invites.store(0, Ordering::Relaxed);
                    eprintln!("too many wrong invite codes: all open invites were cancelled");
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

async fn serve(node: &Arc<Node>, remote: &str, s: &mut SendStream, r: &mut RecvStream, max: usize) -> Result<()> {
    let req: Req = proto::recv_max(r, max).await?;
    if let Req::Pair { token, name, relay, addrs } = req {
        let name = node.accept_pair(remote, &token, &name, relay, addrs)?;
        return proto::send(s, &Resp::Paired { name }).await;
    }
    let Some(friend) = node.cfg().friends.into_iter().find(|f| f.id == remote && f.path.is_none()) else {
        bail!("you are not on this node's friend list");
    };
    crate::store::check_owner(remote)?;
    let quota = friend.quota_gb << 30;
    let store = &node.store;
    match req {
        Req::Pair { .. } => unreachable!(),
        Req::ListChunks => {
            let ids = store.list_chunks(remote);
            proto::send(s, &Resp::Count(ids.len() as u64)).await?;
            for chunk in ids.chunks(4096) {
                AsyncWriteExt::write_all(s, chunk.as_flattened()).await?;
            }
        }
        Req::PutChunks { count } => {
            let mut err = None;
            for _ in 0..count {
                let id = proto::read_id(r).await?;
                let data = proto::read_blob(r, MAX_BLOB).await?.ok_or_else(|| anyhow!("bad chunk"))?;
                if err.is_none() {
                    err = store.put_chunk(remote, &id, &data, quota).err();
                }
                // Not reading the next chunk yet is what slows the sender down.
                let wait = node.guard.owe(remote, data.len(), friend.max_in_kbps);
                tokio::time::sleep(wait).await;
            }
            if let Some(e) = err {
                return Err(e);
            }
            proto::send(s, &Resp::Ok).await?;
        }
        Req::GetChunks { ids } => {
            proto::send(s, &Resp::Ok).await?;
            for id in ids {
                match store.get_chunk(remote, &id) {
                    Some(d) => proto::write_blob(s, &d).await?,
                    None => proto::write_missing(s).await?,
                }
            }
        }
        Req::DeleteChunks { ids } => {
            store.delete_chunks(remote, &ids);
            proto::send(s, &Resp::Ok).await?;
        }
        Req::PutSnapshot { id, data } => {
            store.put_snapshot(remote, &id, &data, quota)?;
            proto::send(s, &Resp::Ok).await?;
        }
        Req::ListSnapshots => proto::send(s, &Resp::Snapshots(store.list_snapshots(remote))).await?,
        Req::DeleteSnapshot { id } => {
            store.delete_snapshot(remote, &id)?;
            proto::send(s, &Resp::Ok).await?;
        }
        Req::Usage => proto::send(s, &Resp::Usage { used: store.usage(remote)?, quota }).await?,
        Req::Version => proto::send(s, &Resp::Version(VERSION.into())).await?,
    }
    Ok(())
}
