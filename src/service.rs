// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const LABEL: &str = "com.cahootsbackup.cahoots";
const UNIT: &str = "cahoots.service";
/// What the service was called while the program was Super Duper Backup (`sdb`).
const OLD_LABEL: &str = "org.superduperbackup.sdb";
const OLD_UNIT: &str = "sdb.service";

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"))
}

fn unit_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        home_dir().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
    } else {
        home_dir().join(".config/systemd/user").join(UNIT)
    }
}

fn old_unit_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        home_dir().join("Library/LaunchAgents").join(format!("{OLD_LABEL}.plist"))
    } else {
        home_dir().join(".config/systemd/user").join(OLD_UNIT)
    }
}

/// Whether a service from before the rename is still in place: it starts the old program,
/// so it is replaced by installing the service again.
pub fn outdated() -> bool {
    old_unit_path().exists()
}

/// Takes the service from before the rename out of the way.
fn remove_old() {
    let path = old_unit_path();
    if !path.exists() {
        return;
    }
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl").args(["unload", "-w"]).arg(&path).output();
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", OLD_UNIT]).status();
    }
    let _ = fs::remove_file(&path);
    println!("removed {} (the service from before the rename)", path.display());
}

/// Whether the start-at-login service file is in place.
pub fn installed() -> bool {
    unit_path().exists()
}

/// Whether this cahoots runs inside a container, where the container is what starts the
/// daemon and a login service has no meaning. The project's image says so itself; the
/// marker files cover an image built some other way.
pub fn in_container() -> bool {
    crate::config::env("IN_CONTAINER").is_some()
        || Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
}

/// Has the login service start the daemon, or start it afresh, so that the service stays the
/// one running it: a daemon started beside it would leave the service failing to start its
/// own, over and over. False when there is no service or it looks after another home (it
/// always runs the default one); the caller then starts the daemon itself.
pub fn restart(home: &Path) -> bool {
    if !installed() || home != crate::config::default_home() {
        return false;
    }
    let done = if cfg!(target_os = "macos") {
        let target = format!("gui/{}/{LABEL}", rustix::process::getuid().as_raw());
        Command::new("launchctl").args(["kickstart", "-k", &target]).output()
    } else {
        Command::new("systemctl").args(["--user", "restart", UNIT]).output()
    };
    done.is_ok_and(|o| o.status.success())
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn install() -> Result<()> {
    let exe = std::env::current_exe()?;
    let path = unit_path();
    remove_old();
    fs::create_dir_all(path.parent().unwrap())?;
    let log = crate::config::home().join("daemon.log");
    if cfg!(target_os = "macos") {
        fs::write(
            &path,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#,
                xml(&exe.display().to_string()),
                log = xml(&log.display().to_string())
            ),
        )?;
        let _ = Command::new("launchctl").args(["unload", "-w"]).arg(&path).output();
        let ok = Command::new("launchctl").args(["load", "-w"]).arg(&path).status()?.success();
        println!(
            "installed {}{}",
            path.display(),
            if ok { " and started the daemon" } else { "; start it with `launchctl load -w` on that file" }
        );
        println!(
            "On macOS, grant {} Full Disk Access in System Settings if you back up Documents, Desktop or Downloads.",
            exe.display()
        );
    } else {
        fs::write(
            &path,
            format!(
                "[Unit]\nDescription=Cahoots Backup\nAfter=network-online.target\n\n[Service]\nExecStart=\"{}\" daemon\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n",
                exe.display()
            ),
        )?;
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
        let ok = Command::new("systemctl")
            .args(["--user", "enable", "--now", UNIT])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        println!(
            "installed {}{}",
            path.display(),
            if ok {
                " and started the daemon"
            } else {
                "; start it with `systemctl --user enable --now cahoots.service`"
            }
        );
        println!("Run `loginctl enable-linger $USER` to keep it running while you are logged out.");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    remove_old();
    let path = unit_path();
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl").args(["unload", "-w"]).arg(&path).output();
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", UNIT]).status();
    }
    let _ = fs::remove_file(&path);
    println!("removed {}", path.display());
    Ok(())
}
