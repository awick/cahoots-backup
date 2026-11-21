// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]
// Nested `if let` reads more clearly here than long let-chains.
#![allow(clippy::collapsible_if)]

mod backup;
mod config;
mod control;
mod crypto;
mod net;
mod node;
mod proto;
mod restore;
mod service;
mod snapshot;
mod store;
mod tui;
mod watch;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::{Config, Identity};
use node::{Cmd, Node, Out};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "sdb", version, about = "Super Duper Backup: encrypted, deduplicated backups between friends")]
struct Cli {
    #[command(subcommand)]
    cmd: C,
}

#[derive(Subcommand)]
enum C {
    /// Create this computer's identity and encryption key
    Init {
        #[arg(long)]
        name: Option<String>,
    },
    /// Run in the background: store friends' backups, watch your files, back up changes
    Daemon,
    /// Add folders or files to back up
    Add {
        paths: Vec<PathBuf>,
        /// Put them in this backup set (created if new; default: the default set)
        #[arg(long)]
        set: Option<String>,
    },
    /// List backup sets
    Sets,
    /// Create or change a backup set: where it goes, how often, when, and its own history rules
    Set {
        name: String,
        /// Destinations for this set, comma separated (`all` = every destination)
        #[arg(long)]
        to: Option<String>,
        /// Minutes between automatic backups of this set (0 = each destination's own pace)
        #[arg(long)]
        every: Option<u32>,
        /// Hours this set may back up, e.g. 22-7, or `always`
        #[arg(long)]
        hours: Option<String>,
        /// Delete this set's restore points older than this many days
        #[arg(long)]
        days: Option<u32>,
        /// Most versions kept of each file in this set
        #[arg(long)]
        versions: Option<u32>,
        /// Always keep this many most recent restore points of this set
        #[arg(long)]
        last: Option<u32>,
        /// Give the set a new name (its restore points stay with it)
        #[arg(long)]
        rename: Option<String>,
        /// Delete the set (its restore points stay until pruned)
        #[arg(long)]
        delete: bool,
    },
    /// Stop backing up a folder
    Rm { path: PathBuf },
    /// Add (or with --remove, drop) an exclude pattern such as '**/node_modules'
    Exclude {
        pattern: String,
        #[arg(long)]
        remove: bool,
    },
    /// Print a one-time invite for a friend
    Invite,
    /// Accept a friend's invite
    Join { ticket: String },
    /// List destinations: friends and drives
    #[command(alias = "friends")]
    Destinations,
    /// Manage one destination
    #[command(alias = "friend")]
    Destination {
        #[command(subcommand)]
        cmd: FriendCmd,
    },
    /// Create a restore point now
    Backup {
        /// Name it; named restore points are kept until you `forget` them
        #[arg(long)]
        tag: Option<String>,
        /// Only this backup set
        #[arg(long)]
        set: Option<String>,
    },
    /// List restore points
    Snapshots,
    /// List files in a restore point (id, tag, 'latest' or a date)
    Ls { snap: String, path: Option<PathBuf> },
    /// Show what changed between two restore points
    Diff { a: String, b: String },
    /// Show every version of one file
    History { path: PathBuf },
    /// Restore files from a restore point (id, tag, 'latest' or a date)
    Restore {
        snap: String,
        /// Only these files or folders (default: everything)
        paths: Vec<PathBuf>,
        /// Folder to restore into
        #[arg(long, required_unless_present = "in_place")]
        to: Option<PathBuf>,
        /// Overwrite the original locations
        #[arg(long)]
        in_place: bool,
        /// Use only this friend or drive (default: any that has it, local drives first)
        #[arg(long)]
        from: Option<String>,
    },
    /// Set how much history to keep
    Keep {
        /// Delete restore points older than this many days (0 = no limit)
        #[arg(long)]
        days: Option<u32>,
        /// Keep at most this many versions of each file (0 = no limit)
        #[arg(long)]
        versions: Option<u32>,
        /// Apply --versions only to files matching this pattern, e.g. '**/*.iso'
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long)]
        last: Option<u32>,
        #[arg(long)]
        hourly: Option<u32>,
        #[arg(long)]
        daily: Option<u32>,
        #[arg(long)]
        weekly: Option<u32>,
        #[arg(long)]
        monthly: Option<u32>,
    },
    /// Limit upload speed in KB/s across all friends (0 = unlimited)
    Limit { upload_kbps: u64 },
    /// Hours automatic backups may run, e.g. 22-7, or 'always'
    Schedule { hours: String },
    /// Hold automatic backups, for a number of hours or until `sdb resume`
    Pause { hours: Option<i64> },
    /// Let automatic backups run again
    Resume,
    /// Warn when a backup is this many days stale (0 = never)
    WarnAfter { days: u32 },
    /// How often automatic backups go out: `sdb every 60` for all, `sdb every 15 usb` for one destination
    Every { minutes: u32, name: Option<String> },
    /// Also back up to an external drive or folder
    Drive { name: String, path: PathBuf },
    /// Apply the keep rules now and reclaim space at your friends
    Prune {
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete one restore point
    Forget { snap: String },
    /// Check that friends still hold your data and that it decrypts
    Verify {
        /// Download and check every chunk instead of a sample
        #[arg(long)]
        full: bool,
        /// Instead, fingerprint this computer's unmodified files against the last backup
        #[arg(long)]
        local: bool,
    },
    /// Check the data friends store on this computer for disk damage
    Scrub,
    /// Show configuration, destinations and usage
    Status,
    /// Stop the background daemon (the next command starts it again)
    Stop,
    /// Interactive setup: pick folders and friends, browse and restore files
    #[command(alias = "setup")]
    Tui,
    /// Export or import the recovery kit you need after losing this computer
    Recovery {
        #[command(subcommand)]
        cmd: RecoveryCmd,
    },
    /// Install or remove the background service (launchd / systemd)
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
}

#[derive(Subcommand)]
enum FriendCmd {
    /// Remove a friend
    Rm {
        name: String,
        /// Also delete the data they stored here
        #[arg(long)]
        purge: bool,
    },
    /// Set how many GB a friend may store on this computer
    Quota { name: String, gb: u64 },
    /// Fastest a friend may send backups to this computer, in KB/s (0 = unlimited)
    Limit { name: String, kbps: u64 },
    /// Start or stop sending your backups to a friend
    BackupTo {
        name: String,
        #[arg(action = clap::ArgAction::Set)]
        on: bool,
    },
}

#[derive(Subcommand)]
enum RecoveryCmd {
    Export { file: PathBuf },
    Import { file: PathBuf },
}

#[derive(Subcommand)]
enum ServiceCmd {
    Install,
    Uninstall,
}

#[derive(Serialize, Deserialize)]
struct KitData {
    identity: Identity,
    config: Config,
}

fn passphrase(confirm: bool) -> Result<String> {
    if let Ok(p) = std::env::var("SDB_PASSPHRASE") {
        return Ok(p);
    }
    let p = rpassword::prompt_password("Recovery passphrase: ")?;
    if confirm && p != rpassword::prompt_password("Again: ")? {
        bail!("passphrases do not match");
    }
    if p.len() < 8 {
        bail!("use at least 8 characters");
    }
    Ok(p)
}

fn hostname() -> String {
    let name = rustix::system::uname().nodename().to_string_lossy().into_owned();
    let h = name.split('.').next().unwrap_or("").to_string();
    if h.is_empty() { "computer".into() } else { h }
}

fn abs(p: &PathBuf) -> Result<String> {
    Ok(std::path::absolute(p)?.to_string_lossy().into_owned())
}

fn lock(home: &std::path::Path) -> Result<fs::File> {
    let f = fs::File::create(home.join("lock"))?;
    if fs2::FileExt::try_lock_exclusive(&f).is_err() {
        bail!("another sdb command is running on {}; wait for it to finish and try again", home.display());
    }
    Ok(f)
}

async fn daemon(home: PathBuf) -> Result<()> {
    let _lock = lock(&home)?;
    control::started_fingerprint();
    let (node, router) = Node::start(home.clone(), true, true).await?;
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let history = node.clone();
    tokio::spawn(async move {
        while let Some(l) = rx.recv().await {
            println!("{} {l}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
            // Backup results reach the history from the engine itself.
            if !l.contains("restore point ") && !l.starts_with("watching ") && !l.trim_start().starts_with("...") {
                history.note(l);
            }
        }
    });
    let out = Out(tx);
    out.say(format!("Super Duper Backup daemon started as {} ({})", node.cfg().name, node.id()));
    tokio::spawn(control::serve(node.clone(), control::listen(&home)?));
    tokio::spawn(watch::run(node.clone(), out));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = node.quit.notified() => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
    }
    let _ = fs::remove_file(control::sock_path(&home));
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), router.shutdown()).await;
    Ok(())
}

/// Starts the daemon in the background, detached from this terminal.
fn spawn_daemon(home: &std::path::Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let log = fs::OpenOptions::new().create(true).append(true).open(home.join("daemon.log"))?;
    std::process::Command::new(std::env::current_exe()?)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("cannot start the sdb daemon")?;
    Ok(())
}

/// Connects to the daemon, starting it if it is not running and replacing it
/// if it was started from an older build than this client.
async fn daemon_stream(home: &std::path::Path) -> Result<tokio::net::UnixStream> {
    Config::load(home)?;
    for _ in 0..2 {
        if let Some(s) = control::connect(home).await {
            let mut theirs = String::new();
            let known = control::request(s, &Cmd::Hello, &mut |l| theirs = l).await.is_ok();
            if known && theirs == control::fingerprint() {
                if let Some(s) = control::connect(home).await {
                    return Ok(s);
                }
            }
            if let Some(s) = control::connect(home).await {
                let _ = control::request(s, &Cmd::Stop, &mut |_| {}).await;
            }
            for _ in 0..50 {
                if control::connect(home).await.is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        if control::connect(home).await.is_none() {
            spawn_daemon(home)?;
        }
        for _ in 0..100 {
            if control::connect(home).await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    control::connect(home).await.ok_or_else(|| {
        anyhow::anyhow!("the sdb daemon could not be started; see {}", home.join("daemon.log").display())
    })
}

async fn run(cmd: Cmd) -> Result<()> {
    let home = config::home();
    if matches!(cmd, Cmd::Stop) {
        return match control::connect(&home).await {
            Some(s) => control::client(s, &cmd).await,
            None => {
                println!("the daemon is not running");
                Ok(())
            }
        };
    }
    control::client(daemon_stream(&home).await?, &cmd).await
}

async fn tui_main(home: PathBuf) -> Result<()> {
    if !home.join("identity.key").exists() {
        fs::create_dir_all(&home)?;
        fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        Identity::generate().save(&home)?;
        Config::new(hostname()).save(&home)?;
    }
    drop(daemon_stream(&home).await?);
    tui::run(home).await
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("sdb: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let home = config::home();
    let cmd = match Cli::parse().cmd {
        C::Init { name } => {
            if home.join("identity.key").exists() {
                bail!("{} is already set up", home.display());
            }
            fs::create_dir_all(&home)?;
            fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
            Identity::generate().save(&home)?;
            let cfg = Config::new(name.unwrap_or_else(hostname));
            cfg.save(&home)?;
            println!("Set up \"{}\" in {}", cfg.name, home.display());
            println!("Next: `sdb add <folder>`, `sdb invite` (or `sdb join <invite>`), `sdb service install`.");
            println!("Then run `sdb recovery export <file>` and keep that file somewhere other than this computer.");
            return Ok(());
        }
        C::Daemon => return daemon(home).await,
        C::Tui => return tui_main(home).await,
        C::Service { cmd } => {
            return match cmd {
                ServiceCmd::Install => {
                    // Hand over: a daemon this user started by hand would block the service's one.
                    if let Some(s) = control::connect(&home).await {
                        let _ = control::request(s, &Cmd::Stop, &mut |_| {}).await;
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    service::install()
                }
                ServiceCmd::Uninstall => service::uninstall(),
            };
        }
        C::Recovery { cmd: RecoveryCmd::Export { file } } => {
            let mut config = Config::load(&home)?;
            config.invites.clear();
            let data = KitData { identity: Identity::load(&home)?, config };
            let kit = crypto::kit_seal(&passphrase(true)?, &serde_json::to_vec(&data)?)?;
            config::write_private(&file, kit.as_bytes())?;
            println!(
                "Wrote {}. With it and the passphrase you can restore everything on a new computer.",
                file.display()
            );
            println!("Export again after adding friends so the kit knows how to reach them.");
            return Ok(());
        }
        C::Recovery { cmd: RecoveryCmd::Import { file } } => {
            if home.join("identity.key").exists() {
                bail!("{} is already set up; move it away first", home.display());
            }
            let kit = fs::read_to_string(&file).with_context(|| file.display().to_string())?;
            let data: KitData = serde_json::from_slice(&crypto::kit_open(&passphrase(false)?, &kit)?)?;
            fs::create_dir_all(&home)?;
            fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
            data.identity.save(&home)?;
            data.config.save(&home)?;
            println!("Recovered \"{}\" with {} friends.", data.config.name, data.config.friends.len());
            println!("Run `sdb snapshots`, then `sdb restore latest --to <folder>` (or --in-place).");
            return Ok(());
        }
        C::Add { paths, set } => {
            if paths.is_empty() {
                bail!("give at least one path");
            }
            let paths = paths
                .iter()
                .map(|p| fs::canonicalize(p).with_context(|| p.display().to_string()))
                .collect::<Result<_>>()?;
            Cmd::AddPaths { paths, set }
        }
        C::Rm { path } => Cmd::RemovePath { path: fs::canonicalize(&path).unwrap_or(std::path::absolute(&path)?) },
        C::Exclude { pattern, remove } => Cmd::Exclude { pattern, remove },
        C::Invite => Cmd::Invite,
        C::Join { ticket } => Cmd::Join { ticket },
        C::Destinations => Cmd::Friends,
        C::Destination { cmd } => match cmd {
            FriendCmd::Rm { name, purge } => Cmd::FriendRemove { name, purge },
            FriendCmd::Quota { name, gb } => Cmd::FriendQuota { name, gb },
            FriendCmd::Limit { name, kbps } => Cmd::FriendLimit { name, kbps },
            FriendCmd::BackupTo { name, on } => Cmd::FriendBackupTo { name, on },
        },
        C::Backup { tag, set } => Cmd::Backup { tag, set },
        C::Sets => Cmd::Sets,
        C::Set { name, rename: Some(to), .. } => Cmd::SetRename { name, to },
        C::Set { name, to, every, hours, days, versions, last, delete, .. } => Cmd::SetEdit {
            name,
            to: to.map(|t| {
                if t == "all" {
                    vec![]
                } else {
                    t.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
                }
            }),
            every,
            hours: hours.map(|h| if h == "always" { String::new() } else { h }),
            days,
            versions,
            last,
            delete,
        },
        C::Snapshots => Cmd::Snapshots,
        C::Ls { snap, path } => Cmd::Ls { snap, path: path.as_ref().map(abs).transpose()? },
        C::Diff { a, b } => Cmd::Diff { a, b },
        C::History { path } => Cmd::History { path: abs(&path)? },
        C::Restore { snap, paths, to, in_place, from } => Cmd::Restore {
            from,
            snap,
            paths: paths.iter().map(abs).collect::<Result<_>>()?,
            to: if in_place { PathBuf::from("/") } else { std::path::absolute(to.unwrap())? },
        },
        C::Keep { days, versions, pattern, last, hourly, daily, weekly, monthly } => {
            let set = [
                ("days", days),
                ("last", last),
                ("hourly", hourly),
                ("daily", daily),
                ("weekly", weekly),
                ("monthly", monthly),
            ]
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_string(), v?)))
            .collect();
            Cmd::Keep { set, versions, pattern }
        }
        C::Limit { upload_kbps } => {
            Cmd::Tune { upload_kbps: Some(upload_kbps), hours: None, pause_hours: None, warn_days: None }
        }
        C::Schedule { hours } => Cmd::Tune {
            upload_kbps: None,
            hours: Some(if hours == "always" { String::new() } else { hours }),
            pause_hours: None,
            warn_days: None,
        },
        C::Pause { hours } => {
            Cmd::Tune { upload_kbps: None, hours: None, pause_hours: Some(hours.unwrap_or(24 * 3650)), warn_days: None }
        }
        C::Resume => Cmd::Tune { upload_kbps: None, hours: None, pause_hours: Some(0), warn_days: None },
        C::WarnAfter { days } => Cmd::Tune { upload_kbps: None, hours: None, pause_hours: None, warn_days: Some(days) },
        C::Every { minutes, name } => Cmd::Every { name, minutes },
        C::Drive { name, path } => {
            Cmd::DriveAdd { name, path: fs::canonicalize(&path).with_context(|| path.display().to_string())? }
        }
        C::Prune { dry_run } => Cmd::Prune { dry_run },
        C::Forget { snap } => Cmd::Forget { snap },
        C::Verify { full, local } => Cmd::Verify { full, local },
        C::Scrub => Cmd::Scrub,
        C::Status => Cmd::Status,
        C::Stop => Cmd::Stop,
    };
    run(cmd).await
}
