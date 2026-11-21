// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use crate::crypto::Id;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::PathBuf};

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum Kind {
    File { size: u64, chunks: Vec<Id> },
    Dir,
    Symlink(Vec<u8>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub path: Vec<u8>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub ino: u64,
    pub kind: Kind,
}

impl Entry {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(OsStr::from_bytes(&self.path))
    }

    pub fn show(&self) -> String {
        String::from_utf8_lossy(&self.path).into_owned()
    }

    pub fn size(&self) -> u64 {
        match &self.kind {
            Kind::File { size, .. } => *size,
            _ => 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub version: u8,
    pub time: i64,
    pub nanos: u32,
    pub host: String,
    pub roots: Vec<String>,
    pub index: Vec<Id>,
    pub files: u64,
    pub bytes: u64,
    pub new_bytes: u64,
    pub tag: Option<String>,
    pub thinned: u64,
    /// The sdb release that wrote this restore point.
    pub writer: String,
    /// The backup set this restore point belongs to.
    pub set: String,
}

/// Manifest layout before backup sets existed.
#[derive(Deserialize)]
struct ManifestV1 {
    version: u8,
    time: i64,
    nanos: u32,
    host: String,
    roots: Vec<String>,
    index: Vec<Id>,
    files: u64,
    bytes: u64,
    new_bytes: u64,
    tag: Option<String>,
    thinned: u64,
    writer: String,
}

pub const MANIFEST_FORMAT: u8 = 2;
const INDEX_MAGIC: &[u8; 5] = b"SDBI\x01";

pub fn encode_manifest(m: &Manifest) -> Result<Vec<u8>> {
    let mut out = vec![MANIFEST_FORMAT];
    out.extend(postcard::to_stdvec(m)?);
    Ok(out)
}

pub fn decode_manifest(buf: &[u8]) -> Result<Manifest> {
    match buf.first() {
        Some(&MANIFEST_FORMAT) => Ok(postcard::from_bytes(&buf[1..])?),
        Some(1) => {
            let m: ManifestV1 = postcard::from_bytes(&buf[1..])?;
            Ok(Manifest {
                version: m.version,
                time: m.time,
                nanos: m.nanos,
                host: m.host,
                roots: m.roots,
                index: m.index,
                files: m.files,
                bytes: m.bytes,
                new_bytes: m.new_bytes,
                tag: m.tag,
                thinned: m.thinned,
                writer: m.writer,
                set: crate::config::DEFAULT_SET.into(),
            })
        }
        Some(v) => bail!(
            "restore point is in format {v} but this sdb only reads up to format {MANIFEST_FORMAT}; upgrade sdb"
        ),
        None => bail!("empty restore point"),
    }
}

pub fn encode_index(entries: &[Entry]) -> Result<Vec<u8>> {
    let mut out = INDEX_MAGIC.to_vec();
    for e in entries {
        let b = postcard::to_stdvec(e)?;
        out.extend((b.len() as u32).to_le_bytes());
        out.extend(b);
    }
    Ok(out)
}

pub fn decode_index(mut buf: &[u8]) -> Result<Vec<Entry>> {
    if buf.len() < 5 || buf[..4] != INDEX_MAGIC[..4] {
        bail!("not a snapshot index");
    }
    if buf[4] != INDEX_MAGIC[4] {
        bail!(
            "snapshot index is in format {} but this sdb only reads format {}; upgrade sdb",
            buf[4],
            INDEX_MAGIC[4]
        );
    }
    buf = &buf[5..];
    let mut out = Vec::new();
    while !buf.is_empty() {
        if buf.len() < 4 {
            bail!("truncated snapshot index");
        }
        let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        if buf.len() < 4 + len {
            bail!("truncated snapshot index");
        }
        out.push(postcard::from_bytes(&buf[4..4 + len])?);
        buf = &buf[4 + len..];
    }
    Ok(out)
}

pub fn human(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn when(t: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(t, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| t.to_string())
}

/// Parses "2026-11-20", "2026-11-20 14:30" or "2026-11-20 14:30:00" as local time.
pub fn parse_when(spec: &str) -> Option<i64> {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone};
    let spec = spec.trim();
    let dt = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(spec, f).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(spec, "%Y-%m-%d")
            .ok()?
            .and_hms_opt(23, 59, 59)
    })?;
    Some(
        chrono::Local
            .from_local_datetime(&dt)
            .earliest()?
            .timestamp(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: Kind) -> Entry {
        Entry {
            path: path.as_bytes().to_vec(),
            mode: 0o644,
            uid: 1,
            gid: 2,
            mtime: 3,
            ino: 4,
            kind,
        }
    }

    #[test]
    fn index_round_trip() {
        let entries = vec![
            entry("/a", Kind::Dir),
            entry(
                "/a/f",
                Kind::File {
                    size: 5,
                    chunks: vec![[9; 32], [8; 32]],
                },
            ),
            entry("/a/l", Kind::Symlink(b"f".to_vec())),
        ];
        let buf = encode_index(&entries).unwrap();
        let back = decode_index(&buf).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[1].kind, entries[1].kind);
        assert_eq!(back[2].show(), "/a/l");
    }

    #[test]
    fn index_rejects_garbage_and_newer_formats() {
        assert!(decode_index(b"nope").is_err());
        let mut buf = encode_index(&[]).unwrap();
        buf[4] += 1;
        assert!(
            decode_index(&buf)
                .unwrap_err()
                .to_string()
                .contains("upgrade")
        );
        let mut cut = encode_index(&[entry("/a", Kind::Dir)]).unwrap();
        cut.pop();
        assert!(decode_index(&cut).is_err());
    }

    #[test]
    fn manifest_round_trip_and_old_format() {
        let m = Manifest {
            version: 1,
            time: 100,
            nanos: 5,
            host: "h".into(),
            roots: vec!["/a".into()],
            index: vec![[1; 32]],
            files: 2,
            bytes: 3,
            new_bytes: 4,
            tag: Some("t".into()),
            thinned: 0,
            writer: "0.1.0".into(),
            set: "photos".into(),
        };
        let back = decode_manifest(&encode_manifest(&m).unwrap()).unwrap();
        assert_eq!(back.set, "photos");
        assert_eq!(back.tag.as_deref(), Some("t"));

        // A restore point written before backup sets existed belongs to the default set.
        let v1 = (
            1u8,
            100i64,
            5u32,
            "h",
            vec!["/a"],
            vec![[1u8; 32]],
            2u64,
            3u64,
            4u64,
            None::<String>,
            0u64,
            "0.1.0",
        );
        let mut old = vec![1u8];
        old.extend(postcard::to_stdvec(&v1).unwrap());
        let m1 = decode_manifest(&old).unwrap();
        assert_eq!((m1.set.as_str(), m1.files, m1.time), ("default", 2, 100));

        assert!(
            decode_manifest(&[99, 0, 0])
                .unwrap_err()
                .to_string()
                .contains("upgrade")
        );
        assert!(decode_manifest(&[]).is_err());
    }

    #[test]
    fn dates_and_sizes() {
        assert!(parse_when("2026-11-20").is_some());
        assert!(parse_when("2026-11-20 14:30").unwrap() < parse_when("2026-11-20").unwrap());
        assert!(parse_when("yesterday").is_none());
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KB");
    }
}
