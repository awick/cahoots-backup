<!-- Copyright Andy Wick. SPDX-License-Identifier: Apache-2.0 -->

# Windows support plan

Goal: a `cahoots.exe` that does everything the Mac and Linux builds do (daemon, command
line, terminal UI, window), and can be a friend to Mac and Linux nodes in both directions.

## Status (2026-11-23)

Work is on the `windows` branch. Test machine: a Windows 11 Arm virtual machine; both the
Intel and the Arm build compile and pass the unit tests there.

- **Phase 0, done.** Everything system-specific is behind `src/os`. Mac and Linux pass the full
  checks unchanged.
- **Phase 1, working.** The daemon runs on Windows. A Mac node paired with it, backed up to it
  and restored from it. Left: decide the home folder, try disk and cloud destinations, the
  end-to-end scripts.
- **Phases 2 to 4, not started.** Backing up and restoring Windows files, the window, release.

What turned out different from the plan below:

- **Control channel.** The program forbids `unsafe` code, and giving a pipe its own access list
  needs it. So the pipe gets a new random name each start, kept with a key in the home
  folder, and each side shows the key before anything else is said. Nothing was loosened.
- **File identity** comes from the `winapi-util` library, already a dependency, for the same
  reason.
- **Main stack.** Windows gives 1 MB where the others give 8, and the program overflowed it on
  start; `build.rs` now asks for 8 MB.
- **Arm build** needs `clang` installed, for the `ring` library. The Intel build does not.
- **Daemon under SSH.** Windows ends a daemon started from an SSH session when that session
  closes. Starting it at login (phase 3) is not affected.

Still open from the lists below: restore does not yet check who owns a folder it just made
(`private_to_me` in `src/os/windows/files.rs` answers yes); paths in restore points are a
placeholder until phase 2.

## Summary

- **Nothing in the design blocks it.** The network library, the crypto, the cloud code, the
  file watcher, the window toolkit and the terminal toolkit all support Windows already.
- **The work is the file and process layer.** About 90 call sites in 10 files use Unix-only
  calls (`rustix`, `std::os::unix`, unix sockets, signals, launchd/systemd).
- **The wire protocol and the stores do not change.** A Windows node can pair with, store
  for, and restore from today's 0.2 nodes.
- **One format question:** how Windows paths are written into a restore point. The plan
  below needs no format change.
- **Size:** roughly 1,500 to 2,500 changed lines, most of it in `store.rs`, `backup.rs`,
  `restore.rs` and `control.rs`. This is an estimate from reading the code, not from a spike.

## What already works on Windows

| Area | Why |
| --- | --- |
| Networking (`net.rs`, most of `node.rs`) | iroh supports Windows, including the relay, hole punching and UPnP |
| Crypto, chunking, compression | Pure Rust or C that builds with MSVC (`blake3`, `ring`, `zstd`) |
| Cloud destinations (`cloud.rs`) | `reqwest` + `rusty-s3`, no OS calls |
| Watching folders (`watch.rs`) | `notify` has a Windows backend |
| Terminal UI | `ratatui` runs on `crossterm`, which supports Windows |
| Window | `eframe` and `rfd` support Windows (see the graphics risk below) |
| Home lock, free-space check | `fs2` supports Windows |
| Passphrase prompt | `rpassword` supports Windows |

## What has to be written

Sizes are relative: S is under a day of work, M a few days, L a week or more.

| Area | Today | On Windows | Size |
| --- | --- | --- | --- |
| Build | `rustix` is a plain dependency | Move it under `cfg(unix)`, add `windows-sys`, gate Unix code | S |
| Control channel (`control.rs`) | Unix socket in a private folder, peer checked by uid | Named pipe open only to the current user | M |
| Home folder (`config.rs` and 12 `HOME` reads) | `~/.cahoots`, mode 0700 | One `user_home()` helper; private by the profile's own access list | S |
| Store (`store.rs`) | `openat` with no-follow at every step | Same checks using reparse-point tests | L |
| Backup walk (`backup.rs`) | `openat`, device+inode checks, hole seeking | File IDs, reparse points, cloud placeholders, locked files | L |
| Restore (`restore.rs`) | `mkdirat`/`openat`, chmod, chown, times | Same shape; read-only flag and times only | M |
| Paths in restore points (`snapshot.rs`) | Raw Unix bytes | UTF-8 with `/`, plus rules for restoring across systems | M |
| Daemon start and stop (`main.rs`) | Process group, SIGTERM | Detached process with no console, console control events | S |
| Start at login (`service.rs`) | launchd agent, systemd user unit | Per-user Run key in the registry | S |
| Notifications (`watch.rs`) | `osascript`, `notify-send` | Toast, or leave out at first | S |
| Window polish (`gui.rs`) | | No console window, icon in the exe, `~` handling | S |
| Tests and CI | Shell scripts, Unix-only unit tests | Windows runner, gate Unix-only tests, port `e2e.sh` | M |
| Release | tar.gz for macOS and Linux | zip for Windows, website button | S |

## Design decisions

### 1. One platform module

Put every OS-specific call behind `src/os/unix.rs` and `src/os/windows.rs` with the same
functions, so the rest of the code has no `cfg`. The Unix side is today's code moved, with no
change in behaviour. The functions needed:

- private folder and private file creation
- control channel: listen, connect, check who is on the other end
- open a folder below another without passing through a link; list it; identify a file
- apply recorded times and permissions to a restored file
- host name, user home, start the daemon detached, wait for a stop signal
- install, remove and restart the login service
- desktop notification

### 2. Control channel: a named pipe

- **Name:** `\\.\pipe\cahoots-<user SID>-<hash of the home path>`.
- **Server:** created with an access list that names only the current user, remote clients
  refused, and first-instance set so another program cannot create the name first.
- **Client:** checks the pipe's owner is the current user before sending anything. This is
  the counterpart of today's uid check on the socket.
- **Window:** its blocking connect opens the pipe as a file.

tokio supports named pipes directly. Unix sockets do exist on Windows 10 and later, but tokio
does not expose them there, so the pipe is the supported route.

### 3. Paths in restore points

- **Stored form:** UTF-8, `/` between parts, drive kept as `C:/Users/me/Documents`. No
  change to the format: a Windows restore point is told apart by its drive-letter roots.
- **Names that are not valid Unicode:** skipped with a message. They are very rare.
- **Excludes:** matched without regard to case on Windows, `\` accepted in patterns.
- **Windows restore point restored on Mac or Linux:** `C:/Users/me` lands in `<to>/C/Users/me`.
- **Mac or Linux restore point restored on Windows:**
  - names Windows cannot hold (`:`, `?`, `*`, a trailing dot or space, `CON`, `NUL` and the
    like) are written with the bad characters escaped, and each one is reported
  - two names that differ only in case: the second gets a suffix, and is reported
  - links are created where Windows allows it, otherwise skipped and reported

### 4. What is recorded for a Windows file

| Field | Value on Windows |
| --- | --- |
| Modified time | Recorded and restored, to 100 ns |
| Mode | Made up: 0644, 0755 for folders, 0444 when read-only. The read-only flag is restored |
| Owner and group | 0. Not restored |
| Inode | The NTFS file index, used only to notice a changed file |
| Access lists, alternate data streams, hidden/system flags, create time | Not recorded |

### 5. The backup walk

- **Links and junctions:** never followed below a root. Links are recorded as links;
  junctions and other reparse points are skipped.
- **Swap check:** each folder and file is opened without following a reparse point and
  compared by volume serial and file index to what the listing saw, as device+inode is today.
- **OneDrive files that are online-only:** skipped and counted. Reading one would download
  it. Documents and Desktop are often redirected into OneDrive, so this matters on day one.
- **Files another program holds open exclusively** (Outlook data, some databases): reported
  as skipped. Reading them needs a volume shadow copy, which needs administrator rights;
  that is left for later.
- **Long paths:** opened with the `\\?\` prefix so the 260-character limit does not apply.
- **Sparse files:** at first handled by the existing "cannot find the holes" fallback; real
  hole detection later.
- **Default excludes:** add `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`, `$Recycle.Bin`,
  `System Volume Information`, `AppData/Local/Temp` and browser caches.

### 6. Files that are open

Windows refuses to delete or replace a file that is open, where Unix allows it. Every place
that writes a temporary file and renames it over the real one needs a look: chunk writes in
the store, restore's temp-then-rename, `config.toml`, the recovery kit. The fix is usually to
close the handle before the rename.

### 7. Home folder

- **Where:** `%LOCALAPPDATA%\Cahoots` (recommended) or `%USERPROFILE%\.cahoots`.
- **Privacy:** folders under the user's profile are already readable only by that user, the
  system and administrators, so the 0700 step becomes a no-op. If `CAHOOTS_HOME` points
  outside the profile, warn.

### 8. Daemon and start at login

- **Start:** detached, in its own process group, with no console window.
- **Stop:** already a control command; also handle Ctrl-C, console close and shutdown.
- **Start at login:** a value under `HKCU\...\CurrentVersion\Run`. No administrator rights
  needed, runs when the user logs in, the same as the launchd agent and systemd user unit.
  It does not restart the daemon after a crash; the next command already starts it.
- **No console flash:** a console program started at login or by double-click opens a
  console window. Ship a second small exe built for the window subsystem (`cahootsw.exe`)
  that opens the window and is what the Run key starts. This means moving the shared code
  from `main.rs` into a library crate that both programs use.

## Phases

Each phase ends with the Mac and Linux checks still green.

**Phase 0 - make it build.** Platform module, `rustix` under `cfg(unix)`, Unix-only tests
gated, a Windows job in `checks.yml` that builds and runs the unit tests. No behaviour change
on Mac or Linux.

**Phase 1 - a Windows node that can be a friend.** Home, named pipe, `start`/`stop`/`status`,
pairing, the store. At the end a Windows machine can hold a Mac's backup and a disk or cloud
destination works. This is useful on its own and exercises the network on Windows early.

**Phase 2 - back up and restore Windows files.** The walk, the recorded fields, path
encoding, restore, and the cross-system restore rules. `e2e.sh` and `damage.sh` running on the
Windows runner under Git Bash.

**Phase 3 - the desktop experience.** `cahootsw.exe`, icon, start at login, notifications,
terminal UI check, `~` and folder pickers in the window.

**Phase 4 - ship it.** `windows-x86_64` zip in `release.yml`, README section, Windows button
on the website.

**Later.** Shadow copies for locked files, hole detection, access lists and alternate
streams, an installer or winget entry, an arm64 build, code signing.

## Risks and things to check first

- **Graphics:** the window uses OpenGL. Some Windows machines, remote desktop sessions and
  virtual machines have poor OpenGL. If that shows up, switch the Windows build to the wgpu
  backend. Check on a real machine in phase 0.
- **Firewall prompt:** Windows asks to allow the daemon the first time it listens. If it is
  refused, links still form by hole punching but direct links may be less common.
- **SmartScreen:** an unsigned exe shows "Windows protected your PC". Signing needs a paid
  certificate and identity check.
- **Cross-compiling:** the C parts (`zstd`, `ring`) make building for Windows from the Mac
  awkward. Plan on building on the GitHub Windows runner or in a Windows machine.
- **Not yet verified:** that the Run key start works well with the detached daemon; how
  tokio's pipe security options map onto the access list above; `e2e.sh` under Git Bash
  (it uses `mkfifo` and the Perl S3 stand-in).

## Decisions needed

1. **A Windows machine to test on.** Without one, only the CI runner can run it and nobody
   sees the window. Windows 11 for ARM runs in a virtual machine on an Apple silicon Mac.
2. **Home location:** `%LOCALAPPDATA%\Cahoots` (recommended, the usual Windows place) or
   `%USERPROFILE%\.cahoots` (same as the README says for the others).
3. **Two programs or one:** `cahoots.exe` plus `cahootsw.exe` (recommended), or one console
   exe that flashes a console window when started from the desktop.
4. **Signing:** ship unsigned at first (recommended) or buy a certificate before release.
5. **Scope of the first Windows release:** all of phases 0 to 4 (recommended), or release
   after phase 1 as "Windows can be a friend but cannot back itself up yet".
