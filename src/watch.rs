// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! Watches the backed up folders and creates a restore point once changes settle.

use crate::{
    backup::{self, Excludes},
    node::{Node, Out},
};
use notify::{EventKind, RecursiveMode, Watcher};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// A set's name, its folders and what it leaves out.
type SetFilter = (String, Vec<PathBuf>, Option<Excludes>);

/// How long after starting the daemon leaves its regular jobs alone.
const SETTLE: Duration = Duration::from_secs(600);

pub async fn run(node: Arc<Node>, out: Out) {
    let mut dirty_since = Some(Instant::now());
    let mut last_event = Instant::now() - Duration::from_secs(3600);
    // Destinations still owed a backup since the last change, and when each last got one.
    let mut pending: HashSet<(String, String)> = HashSet::new();
    let mut sent_at: HashMap<(String, String), Instant> = HashMap::new();
    // Sets with changes not yet handed to `pending`; None means all of them.
    let mut touched: Option<HashSet<String>> = None;
    let mut refill = true;
    let mut last_scan = Instant::now();
    let (started, mut last_jobs, mut last_kit) = (Instant::now(), Instant::now(), Instant::now());
    // A copy of the recovery kit still under way; a hung folder holds one thread, not one per try.
    let mut kit_sync: Option<tokio::task::JoinHandle<anyhow::Result<bool>>> = None;
    let mut last_alert = Instant::now() - Duration::from_secs(3000);
    let mut last_notice = Instant::now();
    let mut last_touch = Instant::now() - Duration::from_secs(280);
    let mut alerted: Vec<String> = vec![];
    loop {
        let cfg = node.cfg();
        // A burst of changes larger than this is remembered only as "something changed everywhere".
        let (tx, mut rx) = mpsc::channel(4096);
        let flood = Arc::new(AtomicBool::new(false));
        let flooded = flood.clone();
        // Each set leaves out different things, so a change counts for the sets that back it up.
        let filters: Arc<Vec<SetFilter>> = Arc::new(
            cfg.sets
                .iter()
                .map(|b| (b.name.clone(), b.paths.clone(), Excludes::new(&cfg, b, &node.home).ok()))
                .collect(),
        );
        let wanted = |filters: &[SetFilter], p: &Path| -> Vec<String> {
            filters
                .iter()
                .filter(|(_, roots, ex)| {
                    roots.iter().any(|r| p.starts_with(r) && ex.as_ref().is_none_or(|x| !x.covers(p, r)))
                })
                .map(|(name, _, _)| name.clone())
                .collect()
        };
        let seen = filters.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            for p in ev.paths.into_iter().filter(|p| !wanted(&seen, p).is_empty()) {
                if tx.try_send(p).is_err() {
                    flooded.store(true, Ordering::Relaxed);
                }
            }
        });
        let mut watcher = match watcher {
            Ok(w) => Some(w),
            Err(e) => {
                out.say(format!("file watching unavailable ({e}), falling back to periodic scans"));
                None
            }
        };
        if let Some(w) = watcher.as_mut() {
            for p in &cfg.all_paths() {
                if let Err(e) = w.watch(p, RecursiveMode::Recursive) {
                    out.say(format!("cannot watch {}: {e}", p.display()));
                }
            }
        }
        if !cfg.all_paths().is_empty() {
            out.say(format!("watching {} folders", cfg.all_paths().len()));
        }
        let changed = node.cfg_changed.notified();
        tokio::pin!(changed);
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = &mut changed => {
                    dirty_since.get_or_insert_with(Instant::now);
                    refill = true;
                    touched = None;
                    break;
                }
                Some(path) = rx.recv() => {
                    last_event = Instant::now();
                    dirty_since.get_or_insert(last_event);
                    refill = true;
                    if let Some(t) = touched.as_mut() {
                        t.extend(wanted(&filters, &path));
                    }
                }
                _ = tick.tick() => {}
            }
            if flood.swap(false, Ordering::Relaxed) {
                last_event = Instant::now();
                dirty_since.get_or_insert(last_event);
                refill = true;
                touched = None;
            }
            let cfg = node.cfg();
            let settle = Duration::from_secs(cfg.settle_secs);
            if last_scan.elapsed() > Duration::from_secs(cfg.rescan_hours.max(1) * 3600) {
                dirty_since.get_or_insert_with(Instant::now);
                refill = true;
                touched = None;
            }
            if refill {
                refill = false;
                for b in cfg
                    .sets
                    .iter()
                    .filter(|b| !b.paths.is_empty() && touched.as_ref().is_none_or(|t| t.contains(&b.name)))
                {
                    pending
                        .extend(cfg.friends.iter().filter(|f| b.sends_to(f)).map(|f| (b.name.clone(), f.id.clone())));
                }
                touched = Some(HashSet::new());
            }
            pending.retain(|(set, id)| {
                cfg.set(set).is_some_and(|b| cfg.friends.iter().any(|f| &f.id == id && b.sends_to(f)))
            });
            if pending.is_empty() {
                dirty_since = None;
            }
            let settled =
                dirty_since.is_some_and(|since| last_event.elapsed() >= settle || since.elapsed() >= settle * 10);
            // Each set and destination has its own pace, such as a drive every 15 minutes and a friend every hour.
            let due: Vec<(String, String)> = pending
                .iter()
                .filter(|key| {
                    let (Some(b), Some(f)) = (cfg.set(&key.0), cfg.friends.iter().find(|f| f.id == key.1)) else {
                        return false;
                    };
                    cfg.held_for(b).is_none()
                        && sent_at
                            .get(*key)
                            .is_none_or(|t| t.elapsed() >= Duration::from_secs(b.interval_secs(f, &cfg)))
                })
                .cloned()
                .collect();
            if last_touch.elapsed() > Duration::from_secs(300) {
                // Staying in touch keeps a working path open in both directions.
                last_touch = Instant::now();
                for f in cfg.friends.iter().filter(|f| f.is_friend()) {
                    let (node, f) = (node.clone(), f.clone());
                    tokio::spawn(async move {
                        let _ = node.peer(&f).await;
                    });
                }
            }
            if last_alert.elapsed() > Duration::from_secs(3600) {
                last_alert = Instant::now();
                node.recheck_store().await;
                let w = node.warnings();
                if !w.is_empty() && (w != alerted || last_notice.elapsed() > Duration::from_secs(86400)) {
                    last_notice = Instant::now();
                    for line in &w {
                        out.say(format!("WARNING: {line}"));
                    }
                    notify_desktop(&w.join("; "));
                }
                alerted = w;
            }
            // Tell the home screen when each waiting backup is expected.
            {
                let now = chrono::Utc::now().timestamp();
                let wait_to_settle = if settled { 0 } else { settle.saturating_sub(last_event.elapsed()).as_secs() };
                let mut next = node.next.lock().unwrap();
                next.clear();
                for key in &pending {
                    let (Some(b), Some(f)) = (cfg.set(&key.0), cfg.friends.iter().find(|f| f.id == key.1)) else {
                        continue;
                    };
                    let pace = Duration::from_secs(b.interval_secs(f, &cfg));
                    let wait = sent_at.get(key).map(|t| pace.saturating_sub(t.elapsed()).as_secs()).unwrap_or(0);
                    let at = if cfg.held_for(b).is_some() { i64::MAX } else { now + wait.max(wait_to_settle) as i64 };
                    next.insert(format!("{}|{}", key.0, key.1), at);
                }
            }
            // These come before the backups, and whether or not one is due: an idle or paused node
            // still has history to prune and data to check. The weekly check and the daily prune go by when they last ran, which is kept on
            // disk, so restarting the daemon does not start the wait over. Neither runs in the
            // first minutes after a start, while the network and the clock may still be settling.
            let wall = chrono::Utc::now().timestamp();
            if last_kit.elapsed() > Duration::from_secs(600) && kit_sync.as_ref().is_none_or(|h| h.is_finished()) {
                last_kit = Instant::now();
                // Only says so when it fails; the warnings carry it from then on. The copy may sit on a
                // slow or hung disk, so it runs off the async workers and this loop waits only so long;
                // one that is still stuck at the next turn is left to finish rather than joined by another.
                let home = node.home.clone();
                let sync =
                    kit_sync.insert(tokio::task::spawn_blocking(move || crate::recovery::copy_sync(&home, false)));
                match tokio::time::timeout(Duration::from_secs(60), sync).await {
                    Ok(Ok(Ok(_))) => {}
                    Ok(Ok(Err(e))) => out.say(format!("recovery kit: {e:#}")),
                    Ok(Err(e)) => out.say(format!("recovery kit: {e}")),
                    Err(_) => out.say("recovery kit: its folder did not answer within a minute"),
                }
            }
            if started.elapsed() > SETTLE && last_jobs.elapsed() > Duration::from_secs(60) {
                last_jobs = Instant::now();
                if node.job_last("check").is_none() {
                    // A node that has never checked starts its week now, not with a check.
                    node.job_mark("check");
                } else if Node::job_due(node.job_last("check"), wall, 7 * 86400) {
                    node.job_mark("check");
                    let _ = node.scrub(&out).await;
                    if let Err(e) = node.verify(false, None, &out).await {
                        out.say(format!("verify: {e:#}"));
                    }
                }
                if Node::job_due(node.job_last("prune"), wall, 86400) {
                    // Marked whether or not it finishes: a destination that is away is tried again tomorrow.
                    node.job_mark("prune");
                    let _op = node.op.lock().await;
                    if let Err(e) = node.prune(false, None, false, None, &out).await {
                        out.say(format!("prune failed: {e:#}"));
                    }
                }
            }
            if !settled || due.is_empty() {
                continue;
            }
            last_scan = Instant::now();
            // Sets whose run ran out of time go on at the next tick, between other work,
            // rather than waiting for their pace or the next change.
            let mut unfinished = HashSet::new();
            for b in &cfg.sets {
                let ids: HashSet<String> =
                    due.iter().filter(|(set, _)| *set == b.name).map(|(_, id)| id.clone()).collect();
                if ids.is_empty() {
                    continue;
                }
                match backup::run(&node, None, Some(&b.name), Some(&ids), &out).await {
                    Ok(()) if backup::deferred(&b.name) => {
                        unfinished.insert(b.name.clone());
                    }
                    Ok(()) => {}
                    Err(e) => out.say(format!("backup failed: {e:#}")),
                }
            }
            for key in due {
                if unfinished.contains(&key.0) {
                    continue;
                }
                pending.remove(&key);
                sent_at.insert(key, Instant::now());
            }
            if pending.is_empty() {
                dirty_since = None;
            }
        }
    }
}

/// Best-effort desktop notification (Notification Center on macOS, notify-send on Linux,
/// none yet on Windows).
fn notify_desktop(text: &str) {
    if cfg!(windows) {
        return;
    }
    let text = text.replace(['"', '\\'], "'");
    let _ = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript")
            .args(["-e", &format!("display notification \"{text}\" with title \"Cahoots Backup\"")])
            .output()
    } else {
        std::process::Command::new("notify-send").args(["--", "Cahoots Backup", &text]).output()
    };
}
