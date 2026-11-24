// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

// No unchecked code, with one exception, which says why: `os::windows::sys`, the calls into
// Windows that the standard library has no safe form of.
#![deny(unsafe_code)]
// Nested `if let` reads more clearly here than long let-chains.
#![allow(clippy::collapsible_if)]

mod backup;
mod cloud;
mod config;
mod control;
mod crypto;
mod dests;
#[cfg(feature = "gui")]
mod gui;
mod net;
mod node;
mod os;
mod proto;
mod recovery;
mod restore;
mod service;
mod snapshot;
mod store;
mod tui;
mod watch;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::{Config, Identity};
use node::{CloudAdd, Cmd, Node, Out, Secret, SetEdit, Tune};
use std::{fs, io::Write, path::PathBuf};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "cahoots", version, about = "Cahoots Backup: encrypted, deduplicated backups between friends")]
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
        /// UDP port to listen on (default: one picked at random, then kept)
        #[arg(long)]
        port: Option<u16>,
        /// Folder for what friends store on this computer (default: inside the node's own folder)
        #[arg(long)]
        store: Option<PathBuf>,
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
        /// The exclusion list this set uses, e.g. Standard or Developer (see `cahoots exclude lists`)
        #[arg(long)]
        exclude: Option<String>,
        /// Give the set a new name (its restore points stay with it)
        #[arg(long)]
        rename: Option<String>,
        /// Delete the set (its restore points stay until pruned)
        #[arg(long)]
        delete: bool,
    },
    /// Stop backing up a folder
    Rm { path: PathBuf },
    /// Exclusion lists: named lists of patterns for things not worth backing up
    Exclude {
        #[command(subcommand)]
        cmd: ExcludeCmd,
    },
    /// Leave one folder or file out of the set that backs it up (--remove puts it back)
    Skip {
        path: PathBuf,
        #[arg(long)]
        set: Option<String>,
        #[arg(long)]
        remove: bool,
    },
    /// Print a one-time invite for a friend
    Invite,
    /// Accept a friend's invite. Run it bare and paste the invite when asked (or pipe it in with
    /// `-`, or set CAHOOTS_TICKET): an invite on the command line can be seen by other users of
    /// this computer while it runs
    Join { ticket: Option<String> },
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
        /// Look only among this backup set's restore points, so `latest` is that set's newest
        #[arg(long)]
        set: Option<String>,
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
    /// Limit speed in MB/s, decimals allowed (0 = unlimited): your backups going out,
    /// and with --incoming what all friends together send here
    Limit {
        #[arg(value_name = "MBPS", value_parser = speed)]
        upload_kbps: Option<u64>,
        #[arg(long, value_name = "MBPS", value_parser = speed)]
        incoming: Option<u64>,
    },
    /// Give this computer another name
    Name { name: String },
    /// Hours automatic backups may run, e.g. 22-7, or 'always'
    Schedule { hours: String },
    /// Hold automatic backups, for a number of hours or until `cahoots resume`
    Pause { hours: Option<i64> },
    /// Let automatic backups run again
    Resume,
    /// Warn when a backup is this many days stale (0 = never)
    WarnAfter { days: u32 },
    /// Whether friends may back up to this computer: `cahoots accept off` makes it one that only
    /// backs up to its friends
    Accept {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// GB a friend you pair with from now on may store on this computer
    DefaultQuota { gb: u64 },
    /// Stop taking friends' backups when this computer's disk has less than this many GB free
    KeepFree { gb: u64 },
    /// How often automatic backups go out: `cahoots every 60` for all, `cahoots every 15 usb` for one destination
    Every { minutes: u32, name: Option<String> },
    /// Also back up to an external drive or folder
    Drive { name: String, path: PathBuf },
    /// Also back up to cloud storage: any S3-compatible bucket (Amazon S3, Backblaze B2,
    /// Cloudflare R2, Wasabi, MinIO). The secret key is asked for, never given here.
    Cloud {
        name: String,
        /// The storage's address, e.g. https://s3.us-west-004.backblazeb2.com
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        bucket: String,
        #[arg(long)]
        access_key: String,
        /// Default us-east-1; Backblaze and others name it in the endpoint, e.g. us-west-004
        #[arg(long)]
        region: Option<String>,
        /// Folder inside the bucket to keep everything under
        #[arg(long)]
        prefix: Option<String>,
        /// Always address the bucket as endpoint/bucket (normally found out by trying)
        #[arg(long)]
        path_style: bool,
        /// Create the bucket if it is not there yet
        #[arg(long)]
        create: bool,
        /// Trust the certificates in this PEM file for this storage, instead of the usual
        /// ones (a private server or an emulator with a certificate of its own)
        #[arg(long)]
        ca_file: Option<PathBuf>,
    },
    /// Apply the keep rules now and reclaim space at your friends
    Prune {
        #[arg(long)]
        dry_run: bool,
        /// Also take out of older restore points what the exclusion lists and skipped
        /// folders now leave out (named restore points are left as they are)
        #[arg(long)]
        apply_exclusions: bool,
        /// With --apply-exclusions: only this backup set
        #[arg(long)]
        set: Option<String>,
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
        /// Only this backup set
        #[arg(long)]
        set: Option<String>,
    },
    /// Check the data friends store on this computer for disk damage
    Scrub,
    /// Show configuration, destinations and usage
    Status,
    /// Stop the background daemon (the next command starts it again)
    Stop,
    /// The interface in the terminal: backup sets, restore, destinations, settings
    #[command(alias = "setup")]
    Tui,
    /// The same in a window
    Gui,
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
    /// Give a friend, drive or cloud storage another name on this computer
    Rename { name: String, to: String },
    /// What removed friends still have stored on this computer; with a name and --delete,
    /// delete it
    Removed {
        /// The removed friend's name, or the start of its id
        who: Option<String>,
        /// Delete what they stored here
        #[arg(long)]
        delete: bool,
    },
    /// Set how many GB a friend may store on this computer
    Quota { name: String, gb: u64 },
    /// Fastest a friend may send backups to this computer, in MB/s, decimals allowed (0 = unlimited)
    Limit {
        name: String,
        #[arg(value_name = "MBPS", value_parser = speed)]
        kbps: u64,
    },
    /// Start or stop sending your backups to a friend
    BackupTo {
        name: String,
        #[arg(action = clap::ArgAction::Set)]
        on: bool,
    },
    /// Remove from a destination the restore points of sets no longer sent there, and
    /// the chunks only they used
    Clean {
        name: String,
        /// Show what would be removed without removing it
        #[arg(long)]
        dry_run: bool,
    },
    /// How much history one destination keeps, instead of each set's own rules
    Keep {
        name: String,
        /// Delete restore points there older than this many days (0 = no limit)
        #[arg(long)]
        days: Option<u32>,
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
        /// Go back to each set's own rules
        #[arg(long)]
        default: bool,
    },
}

#[derive(Subcommand)]
enum ExcludeCmd {
    /// Show every list, its patterns and the sets that use it
    Lists,
    /// Make a list of your own, starting as a copy of Standard
    New {
        name: String,
        /// Copy this list instead
        #[arg(long)]
        from: Option<String>,
        /// Start with no patterns
        #[arg(long, conflicts_with = "from")]
        empty: bool,
    },
    /// Add a pattern to one of your lists, such as '*.iso' or '**/build'
    Add { list: String, pattern: String },
    /// Take a pattern out of one of your lists
    Rm { list: String, pattern: String },
    /// Delete one of your lists; sets that used it go back to Standard
    Delete { list: String },
}

#[derive(Subcommand)]
enum RecoveryCmd {
    /// Write the kit to a file (NAME.cahoots-recovery-kit here unless you name one); it asks for
    /// a passphrase
    Export { file: Option<PathBuf> },
    /// Set this computer up from a kit file; it asks for the kit's passphrase
    Import { file: PathBuf },
    /// Keep a copy of the kit in a folder, written again whenever it changes: one that a
    /// cloud drive syncs is the idea (on a Mac, Documents unless you name another). It asks
    /// for the kit's passphrase once
    CopyTo {
        folder: Option<PathBuf>,
        /// Stop keeping the copy; what is in the folder stays
        #[arg(long, conflicts_with_all = ["folder", "now"])]
        off: bool,
        /// Write the copy now
        #[arg(long, conflicts_with = "folder")]
        now: bool,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    Install,
    Uninstall,
}

/// Asks for the kit's passphrase. A new one has to be long enough and is typed twice; one
/// for a kit that exists is taken as it is, since older kits allowed shorter ones.
fn passphrase(new: bool) -> Result<String> {
    if let Some(p) = config::env_text("PASSPHRASE") {
        if new {
            recovery::check_passphrase(&p)?;
        }
        return Ok(p);
    }
    if new {
        eprintln!(
            "Choose a passphrase of at least {} characters. A sentence you will remember works well;",
            recovery::MIN_PASSPHRASE
        );
        eprintln!("any characters will do, and only its length is checked.");
    }
    let p = rpassword::prompt_password("Recovery passphrase: ")?;
    if new {
        recovery::check_passphrase(&p)?;
        if p != rpassword::prompt_password("Again: ")? {
            bail!("passphrases do not match");
        }
    }
    Ok(p)
}

/// The invite to join with: the one given, else CAHOOTS_TICKET, else a line read from what is
/// piped in, else one pasted when asked. `-` asks for what is piped in, whatever CAHOOTS_TICKET
/// holds. Only the first is seen by other users of this computer (in the list of what runs),
/// so the rest are what the help suggests.
fn invite(given: Option<String>) -> Result<String> {
    use std::io::{BufRead, IsTerminal};
    let piped = given.as_deref() == Some("-");
    let text = match given.filter(|t| t != "-") {
        Some(t) => t,
        None => match config::env_text("TICKET").filter(|_| !piped) {
            Some(t) => t,
            None if std::io::stdin().is_terminal() => rpassword::prompt_password("Invite (it is not shown): ")?,
            None => {
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                line
            }
        },
    };
    let ticket = text.trim().trim_start_matches("cahoots join ").trim_start_matches("sdb join ").trim();
    if ticket.is_empty() {
        bail!("no invite given: paste the one your friend sent (it starts with cahoots1)");
    }
    Ok(ticket.to_string())
}

/// A speed in MB/s from the command line, as KB/s.
fn speed(text: &str) -> Result<u64, String> {
    config::parse_mbps(text).ok_or_else(|| "a speed is a number of MB/s such as 0.5 or 20 (0 = unlimited)".to_string())
}

fn hostname() -> String {
    let name = os::host_name();
    let h = name.split('.').next().unwrap_or("").to_string();
    if h.is_empty() { "computer".into() } else { h }
}

fn abs(p: &PathBuf) -> Result<String> {
    Ok(std::path::absolute(p)?.to_string_lossy().into_owned())
}

fn lock(home: &std::path::Path) -> Result<fs::File> {
    let f = fs::File::create(home.join("lock"))?;
    if fs2::FileExt::try_lock_exclusive(&f).is_err() {
        bail!("another cahoots command is running on {}; wait for it to finish and try again", home.display());
    }
    Ok(f)
}

/// With CAHOOTS_TRACE set (say to `iroh=debug`), what the networking logs goes to trace.log.
fn trace(home: &std::path::Path) -> Result<()> {
    let Some(filter) = config::env_text("TRACE") else { return Ok(()) };
    let file = fs::OpenOptions::new().create(true).append(true).open(home.join("trace.log"))?;
    let filter = tracing_subscriber::EnvFilter::try_new(filter).context("CAHOOTS_TRACE")?;
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(file)).init();
    Ok(())
}

async fn daemon(home: PathBuf) -> Result<()> {
    let _lock = lock(&home)?;
    trace(&home)?;
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
    out.say(format!("Cahoots Backup daemon started as {} ({})", node.cfg().name, node.id()));
    tokio::spawn(control::serve(node.clone(), control::listen(&home)?));
    tokio::spawn(watch::run(node.clone(), out));
    let stop = os::stop_signal()?;
    tokio::select! {
        _ = stop => {}
        _ = node.quit.notified() => {
            // Out of reach first, so no new command arrives at a daemon on its way out; then
            // a moment for the answer to the one that stopped it.
            control::unlisten(&home);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    control::unlisten(&home);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), router.shutdown()).await;
    Ok(())
}

/// Prints a line; a reader that has stopped listening (`| head`) is not an error.
fn say(text: impl std::fmt::Display) {
    let _ = writeln!(std::io::stdout(), "{text}");
}

/// Starts the daemon in the background, detached from this terminal.
fn spawn_daemon(home: &std::path::Path) -> Result<()> {
    // Where the login service looks after this home, it is the one to start the daemon.
    if service::restart(home) {
        return Ok(());
    }
    start_daemon(home)
}

/// Starts the daemon apart from this program, with daemon.log to write to.
fn start_daemon(home: &std::path::Path) -> Result<()> {
    let log = fs::OpenOptions::new().create(true).append(true).open(home.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(os::program()?);
    cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    os::detached(&mut cmd).spawn().context("cannot start the cahoots daemon")?;
    Ok(())
}

/// Waits, a few seconds at most, until the daemon that was told to stop has really gone:
/// out of reach and no longer holding its home. The next command then starts a new one
/// rather than catching the old one leaving.
async fn gone(home: &std::path::Path) {
    for _ in 0..50 {
        if control::connect(home).await.is_none() && lock(home).is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Connects to the daemon, starting it if it is not running and replacing it
/// if it was started from an older build than this client.
async fn daemon_stream(home: &std::path::Path) -> Result<control::Stream> {
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
            gone(home).await;
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
        anyhow::anyhow!("the cahoots daemon could not be started; see {}", home.join("daemon.log").display())
    })
}

async fn run(cmd: Cmd) -> Result<()> {
    let home = config::home();
    if matches!(cmd, Cmd::Stop) {
        return match control::connect(&home).await {
            Some(s) => {
                let res = control::client(s, &cmd).await;
                gone(&home).await;
                res
            }
            None => {
                println!("the daemon is not running");
                Ok(())
            }
        };
    }
    control::client(daemon_stream(&home).await?, &cmd).await
}

/// Sets up a new node in `home` for the first start of a screen.
fn first_run(home: &std::path::Path) -> Result<()> {
    if !home.join("identity.key").exists() {
        config::check_new_home(home)?;
        fs::create_dir_all(home)?;
        os::private_dir(home)?;
        Identity::generate().save(home)?;
        Config::new(hostname()).save(home)?;
    }
    Ok(())
}

async fn tui_main(home: PathBuf) -> Result<()> {
    first_run(&home)?;
    drop(daemon_stream(&home).await?);
    tui::run(home).await
}

fn main_with(cli: Cli) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("cannot start the runtime")?;
    rt.block_on(real_main(cli))
}

/// The `cahoots` command.
pub fn cli() {
    if let Err(e) = main_with(Cli::parse()) {
        eprintln!("cahoots: {}", node::clean(&format!("{e:#}")));
        std::process::exit(1);
    }
}

/// The windowed program Windows has beside `cahoots`: the window when started bare, as from
/// the Start menu, and otherwise the same commands with no console to print to (`daemon`,
/// when starting at login).
#[cfg(feature = "windowed")]
pub fn windowed() {
    let mut args: Vec<_> = std::env::args_os().collect();
    if args.len() < 2 {
        args.push("gui".into());
    }
    let cli = Cli::parse_from(args);
    // Started at login it has nowhere to print, so the daemon would leave no log: it is
    // started again with daemon.log to write to, as any other command starts it.
    #[cfg(windows)]
    let done =
        if matches!(cli.cmd, C::Daemon) && os::no_output() { start_daemon(&config::home()) } else { main_with(cli) };
    #[cfg(not(windows))]
    let done = main_with(cli);
    if let Err(e) = done {
        gui::alert(&node::clean(&format!("{e:#}")));
        std::process::exit(1);
    }
}

async fn real_main(cli: Cli) -> Result<()> {
    let home = config::home();
    let cmd = match cli.cmd {
        C::Init { name, port, store } => {
            config::check_new_home(&home)?;
            let name = name.unwrap_or_else(hostname);
            if config::reserved(&name) {
                bail!("\"{name}\" has a special meaning in destination lists; choose another name");
            }
            fs::create_dir_all(&home)?;
            os::private_dir(&home)?;
            Identity::generate().save(&home)?;
            let mut cfg = Config::new(name);
            cfg.port = port.unwrap_or(cfg.port);
            cfg.store_dir = store.map(|s| std::path::absolute(&s)).transpose()?;
            // Made now, when it is chosen: the daemon only opens it (see `Node::start`).
            if let Some(dir) = &cfg.store_dir {
                fs::create_dir_all(dir).with_context(|| format!("cannot make {}", dir.display()))?;
            }
            cfg.save(&home)?;
            println!("Set up \"{}\" in {}", cfg.name, home.display());
            println!(
                "Next: `cahoots add <folder>`, `cahoots invite` (or `cahoots join`, then paste their invite), `cahoots service install`."
            );
            println!(
                "Then run `cahoots recovery export <file>` and keep that file somewhere other than this computer."
            );
            return Ok(());
        }
        C::Daemon => return daemon(home).await,
        C::Tui => return tui_main(home).await,
        #[cfg(feature = "gui")]
        C::Gui => {
            first_run(&home)?;
            drop(daemon_stream(&home).await?);
            return gui::run(home);
        }
        #[cfg(not(feature = "gui"))]
        C::Gui => bail!("this cahoots was built without the graphical interface; use `cahoots tui`"),
        C::Service { cmd } => {
            return match cmd {
                ServiceCmd::Install if service::in_container() => {
                    bail!(
                        "cahoots runs in a container here: the container starts the daemon, and its restart policy brings it back"
                    )
                }
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
            let file = match file {
                Some(f) => f,
                None => PathBuf::from(recovery::file_name(&Config::load(&home)?.name)),
            };
            recovery::export(&home, &passphrase(true)?, &file)?;
            say(format!(
                "Wrote {}. With it and the passphrase you can restore everything on a new computer.",
                file.display()
            ));
            say("Export again after adding friends so the kit knows how to reach them.");
            return Ok(());
        }
        C::Recovery { cmd: RecoveryCmd::CopyTo { off: true, .. } } => Cmd::KitCopyOff,
        C::Recovery { cmd: RecoveryCmd::CopyTo { now: true, .. } } => Cmd::KitCopySync,
        C::Recovery { cmd: RecoveryCmd::CopyTo { folder, .. } } => {
            let Some(folder) = folder.or_else(recovery::default_folder) else {
                bail!("name the folder to keep the copy in, or --now to write it again, or --off to stop")
            };
            let folder = os::real_path(&folder).with_context(|| folder.display().to_string())?;
            say(format!(
                "The kit will be kept as {}",
                folder.join(recovery::file_name(&Config::load(&home)?.name)).display()
            ));
            Cmd::KitCopyOn { passphrase: Secret(passphrase(true)?), folder }
        }
        C::Recovery { cmd: RecoveryCmd::Import { file } } => {
            let config = recovery::import(&home, &passphrase(false)?, &file)?;
            let sets: Vec<&str> = config.sets.iter().map(|b| b.name.as_str()).collect();
            say(format!(
                "Recovered \"{}\" with {} friends and {} backup sets ({}).",
                config.name,
                config.friends.len(),
                sets.len(),
                sets.join(", ")
            ));
            say(
                "`cahoots status` shows whether your destinations are reachable, `cahoots snapshots` the restore points.",
            );
            say("To put a set's files back where they were: cahoots restore latest --set NAME --in-place");
            say("(or --to FOLDER to look first; `cahoots gui` and `cahoots tui` do the same from their Restore page).");
            return Ok(());
        }
        C::Add { paths, set } => {
            if paths.is_empty() {
                bail!("give at least one path");
            }
            let paths = paths
                .iter()
                .map(|p| os::real_path(p).with_context(|| p.display().to_string()))
                .collect::<Result<_>>()?;
            Cmd::AddPaths { paths, set }
        }
        C::Rm { path } => Cmd::RemovePath { path: os::real_path(&path).unwrap_or(std::path::absolute(&path)?) },
        C::Exclude { cmd } => match cmd {
            ExcludeCmd::Lists => Cmd::ExcludeLists,
            ExcludeCmd::New { name, from, empty } => {
                Cmd::ExcludeNew { name, from: if empty { None } else { Some(from.unwrap_or("Standard".into())) } }
            }
            ExcludeCmd::Add { list, pattern } => Cmd::ExcludePattern { list, pattern, remove: false },
            ExcludeCmd::Rm { list, pattern } => Cmd::ExcludePattern { list, pattern, remove: true },
            ExcludeCmd::Delete { list } => Cmd::ExcludeDelete { name: list },
        },
        C::Skip { path, set, remove } => {
            Cmd::Skip { path: os::real_path(&path).unwrap_or(std::path::absolute(&path)?), set, remove }
        }
        C::Invite => Cmd::Invite,
        C::Join { ticket } => Cmd::Join { ticket: invite(ticket)? },
        C::Destinations => Cmd::Friends,
        C::Destination { cmd } => match cmd {
            FriendCmd::Rm { name, purge } => Cmd::FriendRemove { name, purge },
            FriendCmd::Rename { name, to } => Cmd::FriendRename { name, to },
            FriendCmd::Removed { who: None, delete: false } => Cmd::Removed,
            FriendCmd::Removed { who: Some(who), delete: true } => Cmd::RemovedDelete { who },
            FriendCmd::Removed { who: None, delete: true } => {
                bail!("say whose: cahoots destination removed NAME --delete")
            }
            FriendCmd::Removed { who: Some(_), delete: false } => {
                bail!(
                    "to delete what they stored here add --delete; without a name, `cahoots destination removed` lists them"
                )
            }
            FriendCmd::Quota { name, gb } => Cmd::FriendQuota { name, gb },
            FriendCmd::Limit { name, kbps } => Cmd::FriendLimit { name, kbps },
            FriendCmd::BackupTo { name, on } => Cmd::FriendBackupTo { name, on },
            FriendCmd::Clean { name, dry_run } => Cmd::DestClean { name, dry_run },
            FriendCmd::Keep { name, days, last, hourly, daily, weekly, monthly, default } => {
                let set: Vec<(String, u32)> = [
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
                if default != set.is_empty() {
                    bail!("give either the rules to keep by (--days, --last, ...) or --default");
                }
                Cmd::DestKeep { name, set, clear: default }
            }
        },
        C::Backup { tag, set } => Cmd::Backup { tag, set },
        C::Sets => Cmd::Sets,
        C::Set { name, rename: Some(to), .. } => Cmd::SetRename { name, to },
        C::Set { name, exclude: Some(list), to, every, hours, days, versions, last, delete, .. } => {
            if to.is_some()
                || every.is_some()
                || hours.is_some()
                || days.is_some()
                || versions.is_some()
                || last.is_some()
                || delete
            {
                bail!("give --exclude on its own");
            }
            Cmd::SetExclude { name, list }
        }
        C::Set { name, to, every, hours, days, versions, last, delete, .. } => Cmd::SetEdit(SetEdit {
            name,
            to: to
                .map(|t| -> Result<Vec<String>> {
                    // Every destination is asked for by name; the daemon refuses an empty list.
                    if t.trim() == "all" {
                        return Ok(vec![config::EVERYWHERE.into()]);
                    }
                    let to: Vec<String> =
                        t.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
                    if to.is_empty() {
                        bail!("--to needs destination names, or `all` for every destination");
                    }
                    Ok(to)
                })
                .transpose()?,
            every,
            hours: hours.map(|h| if h == "always" { String::new() } else { h }),
            days,
            versions,
            last,
            delete,
        }),
        C::Snapshots => Cmd::Snapshots,
        C::Ls { snap, path } => Cmd::Ls { snap, path: path.as_ref().map(abs).transpose()? },
        C::Diff { a, b } => Cmd::Diff { a, b },
        C::History { path } => Cmd::History { path: abs(&path)? },
        C::Restore { snap, paths, to, in_place, from, set } => Cmd::Restore {
            from,
            set,
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
        C::Limit { upload_kbps: None, incoming: None } => {
            bail!(
                "give a speed in MB/s for your backups going out, --incoming MBPS for what friends send here, or both"
            )
        }
        C::Limit { upload_kbps, incoming } => {
            Cmd::Tune(Tune { upload_kbps, incoming_kbps: incoming, ..Default::default() })
        }
        C::Name { name } => Cmd::Tune(Tune { name: Some(name), ..Default::default() }),
        C::Schedule { hours } => {
            let hours = Some(if hours == "always" { String::new() } else { hours });
            Cmd::Tune(Tune { hours, ..Default::default() })
        }
        C::Pause { hours } => Cmd::Tune(Tune { pause_hours: Some(hours.unwrap_or(24 * 3650)), ..Default::default() }),
        C::Resume => Cmd::Tune(Tune { pause_hours: Some(0), ..Default::default() }),
        C::WarnAfter { days } => Cmd::Tune(Tune { warn_days: Some(days), ..Default::default() }),
        C::Accept { state } => Cmd::Tune(Tune { accept_backups: Some(state == "on"), ..Default::default() }),
        C::DefaultQuota { gb } => Cmd::Tune(Tune { default_quota_gb: Some(gb), ..Default::default() }),
        C::KeepFree { gb } => Cmd::Tune(Tune { min_free_gb: Some(gb), ..Default::default() }),
        C::Every { minutes, name } => Cmd::Every { name, minutes },
        C::Drive { name, path } => {
            Cmd::DriveAdd { name, path: os::real_path(&path).with_context(|| path.display().to_string())? }
        }
        C::Cloud { name, endpoint, bucket, access_key, region, prefix, path_style, create, ca_file } => {
            let ca_pem = match &ca_file {
                Some(p) => Some(fs::read_to_string(p).with_context(|| p.display().to_string())?),
                None => None,
            };
            let secret_key = match config::env_text("CLOUD_SECRET") {
                Some(s) => s,
                None => rpassword::prompt_password("Secret key: ")?,
            };
            let path_style = path_style.then_some(true);
            Cmd::CloudAdd(CloudAdd {
                name,
                endpoint,
                bucket,
                region,
                prefix,
                access_key,
                secret_key,
                path_style,
                create,
                ca_pem,
            })
        }
        C::Prune { dry_run, apply_exclusions, set } => Cmd::Prune { dry_run, apply_exclusions, set },
        C::Forget { snap } => Cmd::Forget { snap },
        C::Verify { full, local, set } => Cmd::Verify { full, local, set },
        C::Scrub => Cmd::Scrub,
        C::Status => Cmd::Status,
        C::Stop => Cmd::Stop,
    };
    run(cmd).await
}
