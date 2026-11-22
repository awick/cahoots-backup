// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Cloud destinations: any S3-compatible bucket (Amazon S3, Backblaze B2,
//! Cloudflare R2, Wasabi, MinIO). Only ciphertext is ever uploaded, laid out as
//! `<prefix>/<node id>/chunks/<chunk id>` and `<prefix>/<node id>/snapshots/<id>`.
//! What the storage answers is trusted no more than a friend's answers are.

use crate::{
    config::Cloud,
    crypto::Id,
    proto::{MAX_BLOB, MAX_IDS, MAX_MSG, MAX_SNAPSHOT},
    store::check_snap,
};
use anyhow::{Result, anyhow, bail};
use futures::{StreamExt, TryStreamExt, future::BoxFuture, stream};
use rusty_s3::{Credentials, S3Action, UrlStyle, actions::ListObjectsV2};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const SIGNED_FOR: Duration = Duration::from_secs(3600);
const PARALLEL: usize = 8;
const TRIES: u32 = 4;
/// A listing page holds a thousand names; anything much larger is not a listing.
const MAX_PAGE: usize = 8 << 20;
/// Most restore point records one listing takes in.
const MAX_SNAPS: usize = 50_000;
/// Most bytes of restore point records one listing holds, as for a friend or a drive.
const MAX_SNAPS_BYTES: u64 = (MAX_MSG - (1 << 20)) as u64;
/// Most pages one listing takes: enough for the most objects a node keeps, a thousand a page.
const MAX_PAGES: u64 = (MAX_IDS + MAX_SNAPS as u64) / 1000 + 1000;
/// A sealed chunk is a format byte, a nonce and a tag around at most this much.
const MIN_SEALED: u64 = 1 + 24 + 16;
const MAX_SEALED: usize = MAX_BLOB + 64;

/// A restore point record's id, version tag and body, if it is still there.
type Fetched = (String, String, Option<Vec<u8>>);

pub struct Bucket {
    http: reqwest::Client,
    bucket: rusty_s3::Bucket,
    creds: Credentials,
    /// Everything this node stores lives under this key prefix.
    base: String,
    /// "bucket at host", for status lines.
    pub place: String,
    /// Storage on this computer, which has no other owner to check for.
    local: bool,
    /// Bytes stored, learned from a listing and kept current as we add and remove.
    used: Mutex<Option<u64>>,
    /// One count of the bucket at a time; the others wait for its answer.
    counting: tokio::sync::Mutex<()>,
    /// Restore point records already downloaded, by id, with the version tag the storage gave.
    snaps: Mutex<HashMap<String, (String, Vec<u8>)>>,
}

/// Plain http would let anyone on the path replay our signed requests, so it is only
/// accepted for storage running on this computer.
pub fn check_endpoint(endpoint: &str) -> Result<reqwest::Url> {
    let url: reqwest::Url = endpoint.parse().map_err(|_| anyhow!("{endpoint} is not a valid endpoint URL"))?;
    if url.host().is_none() {
        bail!("{endpoint} is not a valid endpoint URL");
    }
    match url.scheme() {
        "https" => {}
        "http" if is_local(&url) => {}
        "http" => bail!("the endpoint must start with https:// (http is only accepted for this computer itself)"),
        _ => bail!("the endpoint must start with https://"),
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        bail!("the endpoint is only an address: give the keys separately");
    }
    Ok(url)
}

fn is_local(url: &reqwest::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(a)) => a.is_loopback(),
        Some(url::Host::Ipv6(a)) => a.is_loopback(),
        None => false,
    }
}

/// The `<Code>` of an S3 error answer.
fn error_code(body: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(body).ok()?;
    let start = text.find("<Code>")? + "<Code>".len();
    Some(text[start..][..text[start..].find("</Code>")?].trim())
}

impl Bucket {
    pub fn open(c: &Cloud, owner: &str) -> Result<Self> {
        let endpoint = check_endpoint(&c.endpoint)?;
        let host = endpoint.host_str().unwrap_or_default().to_string();
        let local = is_local(&endpoint);
        let style = if c.path_style { UrlStyle::Path } else { UrlStyle::VirtualHost };
        let bucket = rusty_s3::Bucket::new(endpoint, style, c.bucket.clone(), c.region.clone())
            .map_err(|e| anyhow!("bad endpoint or bucket: {e}"))?;
        let prefix = c.prefix.trim_matches('/');
        // Fine if one is installed already.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = &c.ca_pem {
            // Only what was named is trusted for this destination, not the usual authorities too.
            let certs = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
                .ok()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| anyhow!("the CA certificate file holds no certificate in PEM form"))?;
            http = http.tls_certs_only(certs);
        }
        Ok(Bucket {
            http: http.build()?,
            bucket,
            creds: Credentials::new(c.access_key.clone(), c.secret_key.clone()),
            base: if prefix.is_empty() { format!("{owner}/") } else { format!("{prefix}/{owner}/") },
            place: format!("{} at {host}", c.bucket),
            local,
            used: Mutex::new(None),
            counting: Default::default(),
            snaps: Mutex::new(HashMap::new()),
        })
    }

    /// Sends a request, trying again when the storage is busy or the network hiccups.
    async fn send(&self, what: &str, make: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let mut last = String::new();
        for attempt in 0..TRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(3u64.pow(attempt - 1))).await;
            }
            match make().send().await {
                Ok(r) if r.status().is_server_error() || r.status().as_u16() == 429 => {
                    last = format!("the storage answered {}", r.status());
                }
                Ok(r) => return Ok(r),
                // The address carries our signature: keep it out of messages and logs.
                Err(e) => last = e.without_url().to_string(),
            }
        }
        bail!("cannot {what}: {last}")
    }

    async fn fail(what: &str, r: reqwest::Response) -> anyhow::Error {
        let status = r.status();
        let hint = match status.as_u16() {
            403 => " (check the access key, the secret key and what the key is allowed to do)",
            404 => " (check the bucket name)",
            301 | 307 | 308 => " (check the region and the endpoint)",
            _ => "",
        };
        let body = Self::body(r, 2000).await.unwrap_or_default();
        let text: String = String::from_utf8_lossy(&body).chars().filter(|c| !c.is_control()).take(300).collect();
        anyhow!("cloud storage refused to {what}: {status}{hint} {text}")
    }

    /// Reads an answer, but no more of it than the thing asked for could be.
    async fn body(mut r: reqwest::Response, max: usize) -> Result<Vec<u8>> {
        let mut out = vec![];
        while let Some(piece) = r.chunk().await.map_err(|e| anyhow!("{}", e.without_url()))? {
            if out.len() + piece.len() > max {
                bail!("the storage sent more than was asked for");
            }
            out.extend_from_slice(&piece);
        }
        Ok(out)
    }

    /// Creates the bucket; fine if it already exists and is ours, but not if it is someone else's.
    pub async fn create(&self) -> Result<()> {
        let url = self.bucket.create_bucket(&self.creds).sign(SIGNED_FOR);
        let r = self.send("create the bucket", || self.http.put(url.as_str())).await?;
        if r.status().is_success() {
            return Ok(());
        }
        if r.status().as_u16() != 409 {
            return Err(Self::fail("create the bucket", r).await);
        }
        let body = Self::body(r, 2000).await.unwrap_or_default();
        match error_code(&body) {
            Some("BucketAlreadyOwnedByYou") => Ok(()),
            Some("BucketAlreadyExists") => bail!(
                "the bucket name {} is taken by another account: choose another name (if the bucket is yours, add it without creating it)",
                self.bucket.name()
            ),
            code => bail!("cloud storage refused to create the bucket: 409 Conflict {}", code.unwrap_or_default()),
        }
    }

    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let url = self.bucket.put_object(Some(&self.creds), key).sign(SIGNED_FOR);
        let r = self.send("store an object", || self.http.put(url.as_str()).body(data.to_vec())).await?;
        if r.status().is_success() { Ok(()) } else { Err(Self::fail("store an object", r).await) }
    }

    async fn get(&self, key: &str, max: usize) -> Result<Option<Vec<u8>>> {
        let url = self.bucket.get_object(Some(&self.creds), key).sign(SIGNED_FOR);
        let r = self.send("read an object", || self.http.get(url.as_str())).await?;
        match r.status().as_u16() {
            404 => Ok(None),
            s if (200..300).contains(&s) => Ok(Some(Self::body(r, max).await?)),
            _ => Err(Self::fail("read an object", r).await),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let url = self.bucket.delete_object(Some(&self.creds), key).sign(SIGNED_FOR);
        let r = self.send("delete an object", || self.http.delete(url.as_str())).await?;
        if r.status().is_success() || r.status().as_u16() == 404 {
            Ok(())
        } else {
            Err(Self::fail("delete an object", r).await)
        }
    }

    /// Every object under a prefix: its name, size and version tag. Listings come a thousand
    /// at a time and are handed over as they arrive; `each` returns false to stop early.
    async fn list(&self, prefix: &str, mut each: impl FnMut(&str, u64, &str) -> bool) -> Result<()> {
        let mut token: Option<String> = None;
        // A storage that hands back any token it gave before, or pages without end, is never done.
        let mut tokens = HashSet::new();
        for _ in 0..MAX_PAGES {
            let mut action = self.bucket.list_objects_v2(Some(&self.creds));
            action.with_prefix(prefix.to_string());
            if let Some(t) = &token {
                action.with_continuation_token(t.clone());
            }
            let url = action.sign(SIGNED_FOR);
            let r = self.send("list the bucket", || self.http.get(url.as_str())).await?;
            if !r.status().is_success() {
                return Err(Self::fail("list the bucket", r).await);
            }
            let text = String::from_utf8(Self::body(r, MAX_PAGE).await?).map_err(|_| anyhow!("unreadable listing"))?;
            let page = ListObjectsV2::parse_response(&text).map_err(|e| anyhow!("unreadable listing: {e}"))?;
            for o in &page.contents {
                // A name outside what was asked for is not ours to act on.
                if o.key.starts_with(prefix) && !each(&o.key, o.size.unwrap_or(0), o.etag.as_deref().unwrap_or("")) {
                    return Ok(());
                }
            }
            match page.next_continuation_token {
                Some(t) if tokens.insert(*blake3::hash(t.as_bytes()).as_bytes()) => token = Some(t),
                Some(_) => bail!("the storage's listing does not end"),
                None => return Ok(()),
            }
        }
        bail!("the storage's listing does not end")
    }

    fn chunk_key(&self, id: &Id) -> String {
        format!("{}chunks/{}", self.base, hex::encode(id))
    }

    fn snap_key(&self, id: &str) -> Result<String> {
        check_snap(id)?;
        Ok(format!("{}snapshots/{id}", self.base))
    }

    fn adjust(&self, delta: i64) {
        if let Some(u) = self.used.lock().unwrap().as_mut() {
            *u = u.saturating_add_signed(delta);
        }
    }

    /// Proves the endpoint, bucket and keys work for reading, writing and deleting, and that
    /// the bucket is not open to anyone (as one squatted under a name we picked may be).
    pub async fn check(&self) -> Result<()> {
        self.list(&self.base, |_, _, _| false).await?;
        if !self.local {
            let mut action = self.bucket.list_objects_v2(None);
            action.with_prefix(self.base.clone());
            action.with_max_keys(1);
            let url = action.sign(SIGNED_FOR);
            if self.send("list the bucket", || self.http.get(url.as_str())).await?.status().is_success() {
                bail!("anyone can list {}: use a private bucket of your own", self.place);
            }
        }
        let key = format!("{}cahoots-check", self.base);
        self.put(&key, b"cahoots").await?;
        self.delete(&key).await
    }

    pub async fn usage(&self) -> Result<u64> {
        let _one = self.counting.lock().await;
        if let Some(u) = *self.used.lock().unwrap() {
            return Ok(u);
        }
        let mut total = 0u64;
        self.list(&self.base, |_, size, _| {
            total = total.saturating_add(size);
            true
        })
        .await?;
        *self.used.lock().unwrap() = Some(total);
        Ok(total)
    }

    /// Counts the bucket again: the running total runs high after chunks are deleted.
    pub async fn recount(&self) -> Result<u64> {
        *self.used.lock().unwrap() = None;
        self.usage().await
    }

    /// Hands each chunk stored here to `each` as the listing arrives; the caller decides what to
    /// keep, so storage that lists ids it never stored cannot make this node hold them.
    pub async fn list_chunks(&self, mut each: impl FnMut(Id)) -> Result<()> {
        let chunks = format!("{}chunks/", self.base);
        let (mut total, mut seen) = (0u64, 0u64);
        self.list(&self.base, |key, size, _| {
            total = total.saturating_add(size);
            seen += 1;
            // Only what could be a sealed chunk counts as one stored.
            let sealed = (MIN_SEALED..=MAX_SEALED as u64).contains(&size);
            if let Some(id) =
                key.strip_prefix(&chunks).filter(|_| sealed).and_then(|h| hex::decode(h).ok()?.try_into().ok())
            {
                each(id);
            }
            seen <= MAX_IDS
        })
        .await?;
        if seen > MAX_IDS {
            bail!("implausible chunk count");
        }
        *self.used.lock().unwrap() = Some(total);
        Ok(())
    }

    /// Refuses what would go past the cap. The running total may run high (an object stored
    /// twice, or space freed by deletes not yet subtracted), so it is recounted before refusing.
    async fn room(&self, adding: u64, quota: u64) -> Result<()> {
        if quota == u64::MAX || self.usage().await?.saturating_add(adding) <= quota {
            return Ok(());
        }
        if self.recount().await?.saturating_add(adding) > quota {
            bail!("quota exceeded (the {} GB cap you set for this cloud storage)", quota >> 30);
        }
        Ok(())
    }

    pub async fn put_chunks(&self, items: &[&(Id, Vec<u8>)], quota: u64, tick: Option<Arc<AtomicU64>>) -> Result<()> {
        self.room(items.iter().map(|(_, d)| d.len() as u64).sum(), quota).await?;
        let tick = &tick;
        let puts: Vec<BoxFuture<'_, Result<()>>> = items
            .iter()
            .map(|(id, data)| {
                Box::pin(async move {
                    self.put(&self.chunk_key(id), data).await?;
                    // Counted as each one lands, so a batch that fails part way is still counted right.
                    self.adjust(data.len() as i64);
                    if let Some(t) = tick {
                        t.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                }) as BoxFuture<'_, Result<()>>
            })
            .collect();
        stream::iter(puts).buffer_unordered(PARALLEL).try_collect::<()>().await
    }

    pub async fn get_chunks(&self, ids: &[Id]) -> Result<Vec<Option<Vec<u8>>>> {
        let gets: Vec<BoxFuture<'_, Result<Option<Vec<u8>>>>> = ids
            .iter()
            .map(|id| Box::pin(async move { self.get(&self.chunk_key(id), MAX_SEALED).await }) as BoxFuture<'_, _>)
            .collect();
        stream::iter(gets).buffered(PARALLEL).try_collect().await
    }

    pub async fn delete_chunks(&self, ids: &[Id]) -> Result<()> {
        // Sizes of what goes are not known here. The total is left high rather than recounted
        // (a count lists the whole bucket): the next listing corrects it, and a cap recounts first.
        let dels: Vec<BoxFuture<'_, Result<()>>> = ids
            .iter()
            .map(|id| Box::pin(async move { self.delete(&self.chunk_key(id)).await }) as BoxFuture<'_, _>)
            .collect();
        stream::iter(dels).buffer_unordered(PARALLEL * 2).try_collect::<()>().await
    }

    pub async fn put_snapshot(&self, id: &str, data: Vec<u8>, quota: u64) -> Result<()> {
        if data.len() > MAX_SNAPSHOT {
            bail!("restore point record too large");
        }
        let key = self.snap_key(id)?;
        self.room(data.len() as u64, quota).await?;
        self.snaps.lock().unwrap().remove(id);
        self.put(&key, &data).await?;
        self.adjust(data.len() as i64);
        Ok(())
    }

    /// Records are small and rarely change, so one already downloaded is not fetched again
    /// while the storage reports the same version of it.
    pub async fn list_snapshots(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let prefix = format!("{}snapshots/", self.base);
        let (mut found, mut listed): (Vec<(String, String)>, u64) = (vec![], 0);
        self.list(&prefix, |key, size, etag| {
            let id = &key[prefix.len()..];
            if check_snap(id).is_ok() && size <= MAX_SNAPSHOT as u64 {
                found.push((id.to_string(), etag.to_string()));
                listed += size;
            }
            found.len() <= MAX_SNAPS && listed <= MAX_SNAPS_BYTES
        })
        .await?;
        // A partial list must never be taken for the whole one: cleaning up would delete the
        // chunks of every restore point left off it.
        if found.len() > MAX_SNAPS {
            bail!("more than {MAX_SNAPS} restore points are stored there; too many to list");
        }
        let too_big = || anyhow!("the restore point records stored there are too large to list");
        if listed > MAX_SNAPS_BYTES {
            return Err(too_big());
        }
        let prefix = &prefix;
        let gets: Vec<BoxFuture<'_, Result<Fetched>>> = {
            let cached = self.snaps.lock().unwrap();
            found
                .into_iter()
                .map(|(id, etag)| {
                    let hit = cached.get(&id).filter(|(e, _)| !e.is_empty() && *e == etag).map(|(_, b)| b.clone());
                    Box::pin(async move {
                        let blob = match hit {
                            Some(b) => Some(b),
                            None => self.get(&format!("{prefix}{id}"), MAX_SNAPSHOT).await?,
                        };
                        Ok((id, etag, blob))
                    }) as BoxFuture<'_, _>
                })
                .collect()
        };
        // The listing's sizes are only the storage's word: what arrives is counted too.
        let mut blobs = stream::iter(gets).buffered(PARALLEL);
        let (mut out, mut keep, mut held) = (vec![], HashMap::new(), 0u64);
        while let Some((id, etag, blob)) = blobs.try_next().await? {
            if let Some(b) = blob {
                held += b.len() as u64;
                if held > MAX_SNAPS_BYTES {
                    return Err(too_big());
                }
                keep.insert(id.clone(), (etag, b.clone()));
                out.push((id, b));
            }
        }
        *self.snaps.lock().unwrap() = keep;
        Ok(out)
    }

    pub async fn delete_snapshot(&self, id: &str) -> Result<()> {
        let key = self.snap_key(id)?;
        let size = self.snaps.lock().unwrap().remove(id).map(|(_, b)| b.len());
        self.delete(&key).await?;
        if let Some(n) = size {
            self.adjust(-(n as i64));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_only_for_this_computer() {
        assert!(check_endpoint("https://s3.us-west-2.amazonaws.com").is_ok());
        assert!(check_endpoint("http://127.0.0.1:9000").is_ok());
        assert!(check_endpoint("http://localhost:9000").is_ok());
        assert!(check_endpoint("http://192.168.1.10:9000").is_err());
        assert!(check_endpoint("http://s3.example.com").is_err());
        assert!(check_endpoint("ftp://s3.example.com").is_err());
        assert!(check_endpoint("https://key:secret@s3.example.com").is_err());
        assert!(check_endpoint("not a url").is_err());
    }

    #[test]
    fn only_our_own_bucket_is_taken_as_created() {
        let err = |c: &str| format!("<?xml version=\"1.0\"?><Error><Code>{c}</Code><Message>x</Message></Error>");
        assert_eq!(error_code(err("BucketAlreadyOwnedByYou").as_bytes()), Some("BucketAlreadyOwnedByYou"));
        assert_eq!(error_code(err("BucketAlreadyExists").as_bytes()), Some("BucketAlreadyExists"));
        assert_eq!(error_code(b"<Error><Code>BucketAlready"), None);
        assert_eq!(error_code(b""), None);
    }

    #[test]
    fn keys_stay_under_this_nodes_folder() {
        let c = Cloud {
            endpoint: "https://s3.example.com".into(),
            bucket: "b".into(),
            region: "us-east-1".into(),
            prefix: "/backups/".into(),
            access_key: "a".into(),
            secret_key: "s".into(),
            path_style: true,
            ca_pem: None,
        };
        let b = Bucket::open(&c, "me").unwrap();
        assert_eq!(b.chunk_key(&[0xab; 32]), format!("backups/me/chunks/{}", "ab".repeat(32)));
        assert_eq!(b.snap_key("00ff").unwrap(), "backups/me/snapshots/00ff");
        assert!(b.snap_key("../x").is_err());
        let junk = Cloud { ca_pem: Some("not a certificate".into()), ..c };
        assert!(Bucket::open(&junk, "me").is_err());
    }
}
