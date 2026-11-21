// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use std::{fs, path::PathBuf, process::Command};

const LABEL: &str = "org.superduperbackup.sdb";

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"))
}

fn unit_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        home_dir().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
    } else {
        home_dir().join(".config/systemd/user/sdb.service")
    }
}

pub fn install() -> Result<()> {
    let exe = std::env::current_exe()?;
    let path = unit_path();
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
                exe.display(),
                log = log.display()
            ),
        )?;
        let _ = Command::new("launchctl").args(["unload", "-w"]).arg(&path).output();
        let ok = Command::new("launchctl").args(["load", "-w"]).arg(&path).status()?.success();
        println!("installed {}{}", path.display(), if ok { " and started the daemon" } else { "; start it with `launchctl load -w` on that file" });
        println!("On macOS, grant {} Full Disk Access in System Settings if you back up Documents, Desktop or Downloads.", exe.display());
    } else {
        fs::write(
            &path,
            format!(
                "[Unit]\nDescription=Super Duper Backup\nAfter=network-online.target\n\n[Service]\nExecStart={} daemon\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n",
                exe.display()
            ),
        )?;
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
        let ok = Command::new("systemctl").args(["--user", "enable", "--now", "sdb.service"]).status().map(|s| s.success()).unwrap_or(false);
        println!("installed {}{}", path.display(), if ok { " and started the daemon" } else { "; start it with `systemctl --user enable --now sdb.service`" });
        println!("Run `loginctl enable-linger $USER` to keep it running while you are logged out.");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let path = unit_path();
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl").args(["unload", "-w"]).arg(&path).output();
    } else {
        let _ = Command::new("systemctl").args(["--user", "disable", "--now", "sdb.service"]).status();
    }
    let _ = fs::remove_file(&path);
    println!("removed {}", path.display());
    Ok(())
}
