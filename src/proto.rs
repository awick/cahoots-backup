// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::crypto::Id;
use anyhow::{bail, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"super-duper-backup/1";
pub const MAX_BLOB: usize = 6 << 20;
pub const MAX_MSG: usize = 64 << 20;
pub const MISSING: u32 = u32::MAX;

#[derive(Serialize, Deserialize, Debug)]
pub enum Req {
    Pair { token: [u8; 16], name: String, relay: Option<String>, addrs: Vec<SocketAddr> },
    ListChunks,
    PutChunks { count: u32 },
    GetChunks { ids: Vec<Id> },
    DeleteChunks { ids: Vec<Id> },
    PutSnapshot { id: String, data: Vec<u8> },
    ListSnapshots,
    DeleteSnapshot { id: String },
    Usage,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Resp {
    Ok,
    Err(String),
    Paired { name: String },
    Count(u64),
    Snapshots(Vec<(String, Vec<u8>)>),
    Usage { used: u64, quota: u64 },
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
    w.write_u32(data.len() as u32).await?;
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
