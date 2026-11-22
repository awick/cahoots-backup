// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::crypto::Id;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped whenever the messages change in a way an older node would misread (version 2
/// streams chunk listings and reports upload progress), so mismatched nodes refuse each
/// other at connect time instead of failing halfway through a request.
/// It keeps the program's first name (as do the key and kit labels in crypto.rs and the
/// `sdb-` file names on drives): changing any of them would cut off every node and backup
/// made before the rename.
pub const ALPN: &[u8] = b"super-duper-backup/2";
pub const MAX_BLOB: usize = 6 << 20;
pub const MAX_MSG: usize = 64 << 20;
pub const MISSING: u32 = u32::MAX;
/// Largest restore point manifest a node stores or accepts.
pub const MAX_SNAPSHOT: usize = 256 << 10;
/// Most chunk ids one listing may carry.
pub const MAX_IDS: u64 = 64 << 20;
/// Most chunk ids in one piece of a listing.
pub const ID_BATCH: usize = 4096;
/// Most chunk ids one delete request may name, so one request is a bounded amount of disk work.
pub const MAX_DELETE: usize = 10_000;
/// Uploads are read, and paced, this many bytes at a time; the receiver reports progress after
/// each piece, so a slow speed limit looks like steady progress rather than silence.
pub const PIECE: usize = 64 << 10;

#[derive(Serialize, Deserialize, Debug)]
pub enum Req {
    Pair {
        token: [u8; 16],
        name: String,
        relay: Option<String>,
        addrs: Vec<SocketAddr>,
    },
    ListChunks,
    PutChunks {
        count: u32,
    },
    GetChunks {
        ids: Vec<Id>,
    },
    DeleteChunks {
        ids: Vec<Id>,
    },
    PutSnapshot {
        id: String,
        data: Vec<u8>,
    },
    ListSnapshots,
    DeleteSnapshot {
        id: String,
    },
    Usage,
    /// Which cahoots release the other side runs. Added after the first release, so it stays
    /// last: an older node answers it with an error, which reads as "unknown".
    Version,
    /// Whether this node takes the asker's backups at all. Newer still, so it stays last:
    /// a node too old to know it answers with an error, which reads as "yes".
    Accepts,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Resp {
    Ok,
    Err(String),
    Paired {
        name: String,
    },
    /// No longer sent (listings stream in pieces, see `write_ids`); kept so the variants
    /// after it keep their numbers on the wire.
    Count(u64),
    Snapshots(Vec<(String, Vec<u8>)>),
    Usage {
        used: u64,
        quota: u64,
    },
    Version(String),
    /// Sent ahead of the reply, once per piece of a large request read so far.
    Progress,
    Accepts(bool),
}

pub async fn send<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    write_blob(w, &postcard::to_stdvec(msg)?).await
}

pub async fn recv<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    recv_max(r, MAX_MSG).await
}

pub async fn recv_max<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R, max: usize) -> Result<T> {
    Ok(postcard::from_bytes(&read_blob(r, max).await?.unwrap_or_default())?)
}

pub async fn write_blob<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    w.write_u32(u32::try_from(data.len()).map_err(|_| anyhow::anyhow!("message too large"))?).await?;
    w.write_all(data).await?;
    Ok(())
}

pub async fn write_missing<W: AsyncWrite + Unpin>(w: &mut W) -> Result<()> {
    w.write_u32(MISSING).await?;
    Ok(())
}

pub async fn read_blob<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> Result<Option<Vec<u8>>> {
    let len = r.read_u32().await?;
    if len == MISSING {
        return Ok(None);
    }
    if len as usize > max {
        bail!("message too large ({len} bytes)");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

pub async fn read_id<R: AsyncRead + Unpin>(r: &mut R) -> Result<Id> {
    let mut id = [0u8; 32];
    r.read_exact(&mut id).await?;
    Ok(id)
}

/// One piece of a chunk listing; an empty piece ends the listing.
pub async fn write_ids<W: AsyncWrite + Unpin>(w: &mut W, ids: &[Id]) -> Result<()> {
    w.write_u32(ids.len() as u32).await?;
    w.write_all(ids.as_flattened()).await?;
    Ok(())
}

pub async fn read_ids<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<Id>> {
    let n = r.read_u32().await? as usize;
    if n > ID_BATCH {
        bail!("listing piece too large ({n} ids)");
    }
    let mut buf = vec![0u8; n * 32];
    r.read_exact(&mut buf).await?;
    Ok(buf.as_chunks::<32>().0.to_vec())
}
