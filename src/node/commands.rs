// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! What each command from the CLI, TUI or GUI does: one method per command, called from
//! `Node::run`.

use super::*;
use crate::config::mbps;

impl Node {
    pub(super) async fn backup(self: &Arc<Self>, tag: Option<String>, set: Option<String>, out: &Out) -> Result<()> {
        backup::run(self, tag, set.as_deref(), None, out).await
    }

    pub(super) async fn show_versions(&self, path: String, from: Option<String>, out: &Out) -> Result<()> {
        for (id, time, v, at) in self.versions(&path, &from, out).await? {
            out.say(serde_json::json!({"id": id, "time": time, "size": v.map(|x| x.0), "at": at}).to_string());
        }
        Ok(())
    }

    pub(super) async fn run_verify(&self, full: bool, local: bool, set: Option<String>, out: &Out) -> Result<()> {
        if let Some(name) = set.as_ref().filter(|n| self.cfg().set(n).is_none()) {
            bail!("no set named {name}");
        }
        if local { self.check_local(set.as_deref(), out).await } else { self.verify(full, set.as_deref(), out).await }
    }

    pub(super) async fn run_prune(
        &self,
        dry_run: bool,
        exclusions: bool,
        set: Option<String>,
        out: &Out,
    ) -> Result<()> {
        if let Some(name) = set.as_ref().filter(|n| self.cfg().set(n).is_none()) {
            bail!("no set named {name}");
        }
        if set.is_some() && !exclusions {
            bail!("--set goes with --apply-exclusions; the history rules always cover every set");
        }
        self.prune(dry_run, None, exclusions, set.as_deref(), out).await
    }

    pub(super) async fn use_destination(&self, name: String, on: bool, out: &Out) -> Result<()> {
        self.update(|c| Self::friend_mut(c, &name).map(|f| f.backup_to = on))??;
        out.say(format!("{} backing up to {name}", if on { "now" } else { "no longer" }));
        Ok(())
    }

    /// How often automatic backups go to one destination, or to local drives in general.
    pub(super) async fn set_pace(&self, name: Option<String>, minutes: u32, out: &Out) -> Result<()> {
        match name {
            Some(name) => {
                self.update(|c| Self::friend_mut(c, &name).map(|f| f.every_minutes = minutes))??;
                out.say(if minutes == 0 {
                    format!("{name} is back on the default (every 60 minutes for a friend)")
                } else {
                    format!("automatic backups go to {name} at most every {minutes} minutes")
                });
            }
            None => {
                let minutes = minutes.max(1);
                self.update(|c| c.min_interval_secs = u64::from(minutes) * 60)?;
                out.say(format!(
                    "local drives get automatic backups at most every {minutes} minutes unless set otherwise; friends default to 60"
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn show_overview(self: &Arc<Self>, out: &Out) -> Result<()> {
        out.say(self.overview().to_string());
        Ok(())
    }

    pub(super) async fn show_log(&self, out: &Out) -> Result<()> {
        for l in self.log.lock().unwrap().iter() {
            out.say(l.clone());
        }
        Ok(())
    }

    /// Which build the running daemon is, so a newer `sdb` can tell it to make way.
    pub(super) async fn hello(&self, out: &Out) -> Result<()> {
        out.say(crate::control::started_fingerprint());
        Ok(())
    }

    pub(super) async fn stop(&self, out: &Out) -> Result<()> {
        out.say("stopping");
        self.quit.notify_waiters();
        Ok(())
    }

    pub(super) async fn show_snap_list(&self, from: Option<String>, out: &Out) -> Result<()> {
        for s in self.snapshots_from(&from, out).await? {
            let at: Vec<&str> = s.peers.iter().map(|(f, _)| f.name.as_str()).collect();
            out.say(
                serde_json::json!({"id": s.id, "time": s.m.time, "files": s.m.files, "bytes": s.m.bytes, "tag": s.m.tag, "at": at, "set": s.m.set})
                    .to_string(),
            );
        }
        Ok(())
    }

    pub(super) async fn status(self: &Arc<Self>, out: &Out) -> Result<()> {
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
            out.say(format!("max outgoing: {} MB/s", mbps(c.upload_kbps)));
        }
        if c.incoming_kbps > 0 {
            out.say(format!("max incoming: {} MB/s", mbps(c.incoming_kbps)));
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

    pub(super) async fn invite(&self, out: &Out) -> Result<()> {
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

    pub(super) async fn join(&self, ticket: &str, out: &Out) -> Result<()> {
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

    pub(super) async fn show_snapshots(&self, out: &Out) -> Result<()> {
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

    pub(super) async fn ls(&self, snap: String, path: Option<String>, out: &Out) -> Result<()> {
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

    pub(super) async fn diff(&self, a: String, b: String, out: &Out) -> Result<()> {
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

    pub(super) async fn history(&self, path: String, out: &Out) -> Result<()> {
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

    pub(super) async fn restore(
        &self,
        snap: String,
        paths: Vec<String>,
        to: PathBuf,
        from: Option<String>,
        out: &Out,
    ) -> Result<()> {
        let snaps = self.snapshots_from(&from, out).await?;
        let s = self.resolve(&snaps, &snap)?;
        let entries: Vec<Entry> = self.load_index(s).await?.into_iter().filter(|e| under(&e.path(), &paths)).collect();
        if entries.is_empty() {
            bail!("nothing in restore point {} matches", s.id);
        }
        out.say(format!("restoring {} entries from restore point {} ({})", entries.len(), s.id, when(s.m.time)));
        restore::run(self, &s.friends(), entries, &to, &s.m.roots, out).await
    }

    pub(super) async fn show_destinations(self: &Arc<Self>, out: &Out) -> Result<()> {
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

    pub(super) async fn remove_destination(&self, name: String, purge: bool, out: &Out) -> Result<()> {
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

    pub(super) async fn limit_friend(&self, name: String, kbps: u64, out: &Out) -> Result<()> {
        if self.cfg().friends.iter().any(|f| f.name == name && !f.is_friend()) {
            bail!("{name} is not a friend's computer: nothing arrives from it to limit");
        }
        self.update(|c| Self::friend_mut(c, &name).map(|f| f.max_in_kbps = kbps))??;
        out.say(if kbps == 0 {
            format!("{name} may send backups here at any speed")
        } else {
            format!("{name} may send backups here at up to {} MB/s", mbps(kbps))
        });
        Ok(())
    }

    pub(super) async fn set_quota(&self, name: String, gb: u64, out: &Out) -> Result<()> {
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

    pub(super) async fn add_paths(&self, paths: Vec<PathBuf>, set: Option<String>, out: &Out) -> Result<()> {
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

    pub(super) async fn remove_path(&self, path: PathBuf, out: &Out) -> Result<()> {
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

    pub(super) async fn rename_set(&self, name: String, to: String, out: &Out) -> Result<()> {
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

    pub(super) async fn show_sets(&self, out: &Out) -> Result<()> {
        let c = self.cfg();
        for b in &c.sets {
            let to =
                if b.destinations.is_empty() { "every destination".to_string() } else { b.destinations.join(", ") };
            let pace = if b.every_minutes > 0 {
                format!("every {} min", b.every_minutes)
            } else {
                "each destination's own pace".into()
            };
            let hours = if b.hours.is_empty() { "general hours".to_string() } else { format!("hours {}", b.hours) };
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

    pub(super) async fn edit_set(&self, edit: SetEdit, out: &Out) -> Result<()> {
        let SetEdit { name, to, every, hours, days, versions, last, delete } = edit;
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

    pub(super) async fn skip_path(&self, path: PathBuf, set: Option<String>, remove: bool, out: &Out) -> Result<()> {
        let name = self.update(|c| {
            let b = c
                .sets
                .iter_mut()
                .find(|b| set.as_ref().is_none_or(|n| *n == b.name) && b.paths.iter().any(|r| path.starts_with(r)))?;
            b.skip.retain(|p| *p != path);
            if !remove {
                b.skip.push(path.clone());
            }
            Some(b.name.clone())
        })?;
        let name = name.ok_or_else(|| anyhow!("{} is not inside a folder that is backed up", path.display()))?;
        out.say(if remove {
            format!("{} is backed up again (set {name})", path.display())
        } else {
            format!("{} is left out of set {name}; restore points made so far still contain it", path.display())
        });
        Ok(())
    }

    pub(super) async fn choose_exclude_list(&self, name: String, list: String, out: &Out) -> Result<()> {
        let c = self.cfg();
        if c.set(&name).is_none() {
            bail!("no set named {name}");
        }
        let found = c.lists().into_iter().find(|l| l.name.eq_ignore_ascii_case(list.trim()));
        let list = found.ok_or_else(|| anyhow!("no exclusion list named {list}; see `sdb exclude lists`"))?.name;
        self.update(|c| {
            if let Some(b) = c.sets.iter_mut().find(|b| b.name == name) {
                b.exclude_list = if list == STANDARD { String::new() } else { list.clone() };
            }
        })?;
        out.say(format!("set {name} now uses the exclusion list \"{list}\""));
        Ok(())
    }

    pub(super) async fn show_exclude_lists(&self, out: &Out) -> Result<()> {
        let c = self.cfg();
        for l in c.lists() {
            let users: Vec<&str> =
                c.sets.iter().filter(|b| c.list_for(b).name == l.name).map(|b| b.name.as_str()).collect();
            out.say(format!(
                "{}{}: {}",
                l.name,
                if is_builtin_list(&l.name) { " (built in)" } else { "" },
                if users.is_empty() { "not used by any set".into() } else { format!("used by {}", users.join(", ")) }
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

    pub(super) async fn new_exclude_list(&self, name: String, from: Option<String>, out: &Out) -> Result<()> {
        let name: String = name.chars().filter(|c| c.is_alphanumeric() || "-_. ".contains(*c)).take(32).collect();
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

    pub(super) async fn delete_exclude_list(&self, name: String, out: &Out) -> Result<()> {
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

    pub(super) async fn edit_exclude_pattern(
        &self,
        list: String,
        pattern: String,
        remove: bool,
        out: &Out,
    ) -> Result<()> {
        if is_builtin_list(&list) {
            bail!("{list} is built in: copy it to change it, with `sdb exclude new NAME --from {list}`");
        }
        let list = self.own_list(&list)?;
        // A bare name such as node_modules means that name anywhere, when adding and
        // when removing alike; removing also takes the pattern exactly as listed.
        let typed = pattern.trim().to_string();
        let pattern =
            if typed.starts_with('/') || typed.starts_with("**/") { typed.clone() } else { format!("**/{typed}") };
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
            Some(_) => {
                out.say(format!("{pattern} added to {list}; restore points made so far still contain what it matches"))
            }
        }
        Ok(())
    }

    pub(super) async fn tune(&self, t: Tune, out: &Out) -> Result<()> {
        let Tune { name, upload_kbps, incoming_kbps, hours, pause_hours, warn_days } = t;
        if let Some(h) = &hours {
            if !h.is_empty() && crate::config::parse_hours(h).is_none() {
                bail!("hours look like 22-7 (10pm to 7am); use 'always' to remove the limit");
            }
        }
        let name = match name {
            Some(n) if !n.chars().any(char::is_alphanumeric) => bail!("a name needs letters or digits"),
            n => n.map(|n| clean_name(&n)),
        };
        self.update(|c| {
            if let Some(n) = &name {
                c.name = n.clone();
            }
            if let Some(k) = upload_kbps {
                c.upload_kbps = k;
            }
            if let Some(k) = incoming_kbps {
                c.incoming_kbps = k;
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
        match name {
            Some(n) => {
                out.say(format!("this computer is now called {n}; friends you already have keep the name they know"))
            }
            None => out.say("saved"),
        }
        Ok(())
    }

    pub(super) async fn add_drive(&self, name: String, path: PathBuf, out: &Out) -> Result<()> {
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

    pub(super) async fn add_cloud(&self, add: CloudAdd, out: &Out) -> Result<()> {
        let CloudAdd { name, endpoint, bucket, region, prefix, access_key, secret_key, path_style, create, ca_pem } =
            add;
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
            region: region.map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).unwrap_or("us-east-1".into()),
            prefix: prefix.unwrap_or_default().trim_matches('/').to_string(),
            access_key,
            secret_key: secret_key.trim().to_string(),
            // New keys for storage already added keep the certificate it was given.
            ca_pem: ca_pem.filter(|p| !p.trim().is_empty()).or_else(|| {
                let known = self.cfg().friends.into_iter().find(|f| f.name == name);
                known.and_then(|f| f.cloud).and_then(|c| c.ca_pem)
            }),
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

    pub(super) async fn keep_at_destination(
        &self,
        name: String,
        set: Vec<(String, u32)>,
        clear: bool,
        out: &Out,
    ) -> Result<()> {
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

    pub(super) async fn show_config(&self, out: &Out) -> Result<()> {
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

    pub(super) async fn show_tree(&self, snap: String, from: Option<String>, out: &Out) -> Result<()> {
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

    pub(super) async fn keep(
        &self,
        set: Vec<(String, u32)>,
        versions: Option<u32>,
        pattern: Option<String>,
        out: &Out,
    ) -> Result<()> {
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
