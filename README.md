<p align="center">
  <img src="assets/logo.svg" width="150" alt="Super Duper Backup: a backup drive wearing a cape">
</p>

# Super Duper Backup

Encrypted, deduplicated backups between friends. Each person runs `sdb` on their
Mac or Linux computer; you add each other with one-time invites and your
computers back up to one another, and optionally to a local drive or a cloud
bucket as well. Friends, and the cloud, only ever hold ciphertext.

## Why this exists

We loved CrashPlan for Home. Its best trick had nothing to do with its cloud:
you could back up to a friend's computer, or to a drive of your own, for free.
Everything was encrypted before it left your machine, it ran quietly in the
background, and restoring a file was boring, which is the highest praise a
backup program can earn.

Then, in 2017, its maker announced it was leaving the home market, and the
following year the service was switched off. The friend-to-friend backup went
with it, and we were devastated. Nothing since has quite filled the gap. The
cloud services want a monthly fee for every computer, forever. The good
open-source tools assume you already have a server or a bucket, and that you
are happy living on the command line.

I meant to write a replacement for years and never had the months it would
take. What changed is AI. With an AI coding assistant doing most of the typing,
and me doing the deciding, the reviewing and the breaking, a project that never
fitted into the evenings suddenly did. Super Duper Backup is the result: the
part of CrashPlan Home we missed most, rebuilt for today. Invite a friend, swap
some disk space, and let your computers look after each other.

Most important: it is open source, it has CLI, TUI and GUI interfaces, and it
is written in Rust instead of Java!!!

It is new software, so treat it that way for now: run it next to whatever
protects your files today. It is not affiliated with CrashPlan or its makers.

## Quick start

Download `sdb` for macOS or Linux from the
[releases page](https://github.com/awick/super-duper-backup/releases), or build
it yourself with `cargo build --release` (the binary is `target/release/sdb`).
The downloads are not signed, so a Mac asks before running one the first time;
`xattr -d com.apple.quarantine sdb` tells it you trust it.

```bash
sdb tui                            # interactive: backup sets, restore, destinations, settings
sdb gui                            # the same in a window
```

Or from the command line:

```bash
sdb init --name andy
sdb add ~/Documents ~/Pictures
sdb invite                         # prints `sdb join sdb1...` to send to a friend
sdb join sdb1...                   # what the friend runs (or you, with their invite)
sdb service install                # run the daemon at login (launchd / systemd)
sdb recovery export ~/sdb-kit.json # keep this file OFF this computer
```

One invite pairs both directions. The daemon watches your folders and creates a
restore point shortly after files stop changing.

## Everyday commands

| Command | What it does |
| --- | --- |
| `sdb status` | folders, destinations, usage, how you are reachable |
| `sdb backup [--tag NAME]` | restore point now; tagged ones are kept until `sdb forget` |
| `sdb snapshots` | list restore points |
| `sdb history FILE` | every version of one file |
| `sdb diff A B` / `sdb ls SNAP [PATH]` | what changed / what is inside |
| `sdb restore SNAP [PATHS] --to DIR` | SNAP is an id, a tag, `latest`, or a date such as `2026-11-20` or `"2026-11-20 14:30"`; `~/Documents/a.txt` lands at `DIR/Documents/a.txt`; `--from NAME` uses one friend or drive only |
| `sdb keep --days 365 --versions 10` | maximum age and versions per file |
| `sdb keep --versions 5 --pattern '**/*.iso'` | version limit for matching files only |
| `sdb prune [--dry-run]` | apply the keep rules now (the daemon does it daily) |
| `sdb add PATH --set NAME` | put a folder in a backup set (created if new) |
| `sdb set NAME --to usb,barney --every 15 --hours 22-7 --versions 5 --days 90` | where a set goes, how often, when, and its own history rules |
| `sdb sets` / `sdb backup --set NAME` | list sets / back up one set now |
| `sdb drive NAME PATH` | also back up to an external drive or folder (encrypted the same way; skipped while unplugged) |
| `sdb cloud NAME --endpoint URL --bucket B --access-key K` | also back up to cloud storage: any S3-compatible bucket (Amazon S3, Backblaze B2, Cloudflare R2, Wasabi, MinIO); the secret key is asked for, never typed on the command line |
| `sdb destination keep NAME --days 30 --last 5` | how much history one destination keeps, instead of each set's rules (`--default` goes back) |
| `sdb every MINUTES [NAME]` | how often automatic backups go to one destination (friends default to 60 minutes, a new drive to 15) |
| `sdb limit KBPS` | cap upload speed across friends (0 = unlimited) |
| `sdb schedule 22-7` / `sdb schedule always` | hours of the day automatic backups may run |
| `sdb pause [HOURS]` / `sdb resume` | hold automatic backups |
| `sdb warn-after DAYS` | warn when a backup, yours or a friend's to you, is this stale (default 3) |
| `sdb destinations` | list friends and drives |
| `sdb destination quota NAME GB` | how much a friend may store on your computer; for cloud storage, the most you keep there (0 = no cap) |
| `sdb destination limit NAME KBPS` | fastest a friend may send backups to your computer (0 = unlimited) |
| `sdb exclude lists` | the exclusion lists, their patterns and which sets use them |
| `sdb set NAME --exclude Developer` | the exclusion list a set uses (`Standard` unless you choose) |
| `sdb exclude new NAME [--from LIST]` / `sdb exclude add LIST PATTERN` / `sdb exclude rm LIST PATTERN` / `sdb exclude delete LIST` | make and change lists of your own |
| `sdb skip PATH [--remove]` | leave one folder or file out of the set that backs it up |
| `sdb set NAME --rename NEW` | rename a backup set; its restore points stay with it |
| `sdb destination backup-to NAME false` | stop using a destination |
| `sdb verify --set NAME` | the same checks for one backup set (also on each set in the TUI and GUI) |
| `sdb verify [--full]` | check friends still hold your data and it decrypts; repairs damage from another friend |
| `sdb verify --local` | fingerprint your unmodified files against the last backup to catch silent disk corruption |
| `sdb scrub` | check the data friends store on your computer for disk damage |

## The TUI

`sdb tui` has a menu on the left and, on the right, whatever the highlighted
item is about. Enter goes into an item and the menu changes to that item's own
choices (Backup, then a backup set, then its files, destinations or schedule);
Esc goes back up. Up and down move, and that is all there is to learn. When the
cursor moves into the right-hand side (a folder tree, a list to tick), space
marks things and Esc returns to the menu.

The first screen shows every backup set with its files, its destinations (and
when each last got a backup), its schedule, and what it is doing right now.

| Page | What is there |
| --- | --- |
| Backup | the backup sets and their status; create a set, choose its files, its destinations and its schedule and history rules, back up now |
| Restore | browse files as of any restore point, search by name, step through time, list a file's versions, choose which destination to read from, restore |
| Destinations | friends' computers, local drives and cloud storage: whether each is reachable and how, what you store there, what they store here; invite, accept an invite, add a drive or cloud storage, set pace, quota or cap, and how much history each keeps |
| Settings | hours, pause, pace, upload limit, warnings and history rules that apply to every set; the exclusion lists |
| History | what the daemon has done since it started |
| Status | the full `sdb status` report |

## The GUI

`sdb gui` opens the same thing in a window: the backup sets with their status,
restore with a timeline, destinations, defaults, history and status. It is
another client of the daemon, built with egui into the same binary. It needs a
desktop session only when you run it; everything else works without one. Build
with `cargo build --release --no-default-features` to leave it out.

## The daemon

One background process per computer does all the work: it serves your friends,
watches your files and runs backups. Every `sdb` command and the TUI are clients
that talk to it over a private local socket. The first command starts it if it
is not running, and replaces it if it was started by an older build;
`sdb stop` stops it and `sdb service install` starts it at login. Its log is
`~/.super-duper-backup/daemon.log`.

## How it works

**Identity and pairing.** Every node has an Ed25519 key; its public key is the
node id. Connections are QUIC with TLS 1.3, mutually authenticated by those keys
([iroh](https://iroh.computer)), so a node always knows cryptographically who is
on the other end. An invite carries the inviter's node id, addresses and a
random 128-bit single-use token valid for 24 hours. Joining proves knowledge of
the token over a connection already authenticated to the inviter's key, and both
sides then record each other's node id. A node that is not on the friend list is
disconnected before it can make any request; pairing attempts are only read
while an invite is open.

**The listening side.** The only thing exposed to the internet is one UDP port
speaking QUIC/TLS 1.3 (iroh, rustls). After the handshake proves a caller's key:
an unknown node is disconnected at once, unless an invite is open, in which case
it gets a single 4 KB pairing request with a 15 second limit (at most four such
attempts at a time, and 50 wrong codes cancel every open invite). A friend may
hold four connections with eight requests in flight each; requests are capped at
8 MB and chunks at 6 MB. A friend can only touch its own folder, whose name is
its verified key; every file counts at least 4 KB against its quota; restore
points are capped at 50,000; and nothing is accepted once the disk has less than
`min_free_gb` (default 5) free. `sdb status` reports how many unknown callers
were turned away. The local control socket is private to your user.

**Reaching each other.** Nodes dial by node id. iroh finds the current address
through DNS discovery, punches through NAT, asks the router to forward the port
(UPnP, NAT-PMP, PCP; `upnp = false` in the config turns that off) and falls back
to an encrypted relay that cannot read the traffic. The listening UDP port is
fixed per node (`port` in the config). A connection opened by either side is
used in both directions, and daemons contact each friend every five minutes, so
it is enough for one of two nodes to be able to reach the other. The default
discovery and relay servers are the public ones run by n0; iroh lets you host
your own.

**Encryption.** A random 256-bit master key never leaves your computer (except
inside the passphrase-protected recovery kit). Files are split into chunks; each
chunk is compressed (zstd level 5) and sealed with XChaCha20-Poly1305, all on
your machine before anything is sent. A chunk's name is
a *keyed* BLAKE3 hash, so a friend cannot test whether you have a known file.
File names, sizes, permissions and the directory tree live in the snapshot
index, which is chunked and encrypted the same way. Friends can see only how
many chunks you store, their sizes and when they arrive.

**Only changed pieces are sent.** Chunk boundaries come from content-defined
chunking (FastCDC, 256 KiB to 4 MiB, 1 MiB average), so inserting or editing
bytes in a large file changes only the chunks around the edit. Unchanged files
are recognised by size, mtime and inode and not re-read. Identical chunks are
stored once per friend no matter how many files or restore points use them.

**Restore points.** Each backup is a complete, independent view of your folders
that shares chunks with the others. Every destination a set is sent to holds a
full copy. One that was offline or unplugged catches up on the next backup.
Files put back exactly as they were earlier still make a new restore point, so
`latest` is always the files as they are now.
Destinations are backed up at their own pace (friends hourly, a new drive every
15 minutes, unless you change it), so the same state can carry different ids on
different destinations.

**Backup sets.** A set is a named group of folders with its own destinations,
pace, hours and history rules; folders start in the `default` set, which goes to
every destination. Each restore point belongs to one set. Sets share storage, so
identical data in two sets is stored once per destination. In the TUI, `w`
switches the set being edited and `n` creates one.

**Retention.** `keep_last / hourly / daily / weekly / monthly` choose which
restore points survive, `keep_days` caps their age, and `max_versions` (global
or per pattern) removes older versions of a file from older restore points.
Tagged restore points are exempt. Pruning then deletes unreferenced chunks at
each destination. A destination can have history rules of its own
(`sdb destination keep`), so a drive can keep two years while a cloud bucket
keeps thirty days; the per-file version limit stays with the set.

**Cloud storage.** A cloud destination is an S3-compatible bucket. It gets
exactly what a friend gets, encrypted chunks and restore point records, under
`<folder>/<your node id>/`, so the provider sees object sizes and timing and
nothing else. Adding one first proves the keys can write, list and delete. The
keys live in `config.toml` (private to your user) and travel in the recovery
kit. The bucket runs none of our code, so your own node prunes it: it deletes
the restore points that the rules no longer keep, lists the bucket, and deletes
every chunk nothing refers to. Do not add expiry (lifecycle) rules to the
bucket; the provider cannot tell which chunks newer restore points still share.
A cap in GB (`sdb destination quota`) works like a friend's quota: when it is
reached the oldest restore points there go first, never the newest of a set.
Restores read from a local drive first, then friends, then the cloud, since
cloud downloads usually cost money. Plain `http://` endpoints are only accepted
on this computer itself.

**What is left out.** Each set uses one exclusion list. `Standard` (the
default) names things nobody wants back: trash, caches, `node_modules`,
`__pycache__` and the like. `Developer` adds build output such as `target`,
`build`, `dist` and `.venv`. Both come with sdb and can gain patterns in a later
release; to change one, copy it into a list of your own. `.git` is in neither.
A folder you untick in the file picker is left out of that set only. Leaving
something out does not remove it from restore points made so far; it ages out
under the history rules.

**Quotas.** You set how many GB each friend may store on your computer; the
store refuses anything beyond it. Because only the owner can tell which chunks
belong to which restore point, the owner's node does the trimming: when it hits
(or is over) a quota it removes its oldest restore points at that friend,
never the newest of any set, and retries.

**Going back in time.** The Restore page of `sdb tui` shows your files as of
any restore point: `t` opens the timeline, `g` jumps to a date, `v` lists the
versions of the highlighted file, and entries are marked *different now*,
*deleted since* or *changes inside* relative to the latest restore point.

**Corruption.** Every chunk is authenticated (AEAD) and its plaintext is
re-hashed on restore, and the restored size must match, so damaged data is
never written back silently; restore falls through to another destination when
one copy is bad. A friend's node keeps a
BLAKE3 checksum of each stored chunk, so it can detect disk rot without any key
(`sdb scrub`, and on every read) and drops damaged chunks so they show up as
missing. `sdb verify` finds missing or damaged chunks and repairs them from
another friend; what no friend has is uploaded again by the next backup if the
file is still unchanged locally. `sdb verify --local` re-reads files whose size
and mtime are unchanged and compares fingerprints with the last backup. The
daemon runs a scrub and a sampled verify weekly.

**Format versions.** Every stored thing names its format so a later release can
tell old from new: `format` and `created` in `config.toml`, `identity.key` and
the recovery kit; a version byte at the start of every encrypted blob and
restore point manifest (which also records its time and the sdb release that
wrote it); `SDBI`/`SDBC` magic plus version on snapshot indexes and stored
chunk files; `sdb1` on invites; and a versioned protocol name on the wire. A
release that meets a newer format stops with an "upgrade sdb" message instead
of guessing.

**Losing your computer.** `sdb recovery export` writes your identity, master key
and friend list encrypted with a passphrase (Argon2id). On a new machine:
`sdb recovery import kit.json`, then `sdb restore latest --in-place`. Without
the kit the backups cannot be decrypted by anyone.

## Files

Everything lives in `~/.super-duper-backup` (override with `SDB_HOME`):
`config.toml` (also holds any cloud storage keys), `identity.key` (secret),
`store/` (friends' encrypted data, move it with `store_dir`), `cache/`.

## Limits worth knowing

- Each chunk is one file on the friend's disk; very large numbers of tiny files
  work but are not packed together.
- Extended attributes, ACLs and hard-link relationships are not recorded.
- Discovery and relaying use n0's free public servers, which are rate-limited
  and carry no uptime guarantee; `sdb` has no setting for your own servers yet.
- A friend can delete or withhold what they hold for you; back up to more than
  one friend and run `sdb verify` occasionally.
- On macOS the binary needs Full Disk Access to read Documents, Desktop and
  Downloads when run by launchd.

## Development

```bash
git config --local core.hooksPath .githooks/   # once per clone: quick checks before every commit
./check.sh quick                               # what that hook runs: formatting, lints, unit tests
./check.sh                                     # those plus the end-to-end and damage tests
```

GitHub runs the full `./check.sh` on Linux and macOS for every push and pull
request, and the cloud test as well, so a commit only waits for the quick part.

`tests/e2e.sh` starts three local nodes and a drive (no internet needed) and
exercises pairing, backup, restore, sets, a refused stranger, corruption
detection and repair, pruning and exclusion lists.

`tests/damage.sh` ruins things on purpose and checks what happens: truncated,
emptied, swapped and missing chunks, damaged restore point records, caches and
config, odd file names, a file that changes mid-backup, a daemon killed
mid-backup, a restore stopped half way, a friend with no room, and pruning
while a friend is away.

`tests/cloud.sh target/release/sdb` tests cloud destinations against a local S3
emulator. It needs the `minio` program.

To publish a release, raise `version` in `Cargo.toml`, push, and run the
`release` workflow on GitHub (`gh workflow run release`). It names the release
after that version and attaches binaries for macOS and Linux.

The app icon is drawn in `assets/icon.svg`; `icon.png` (built into the window)
and `icon.icns` (for a Mac app bundle) are made from it with `rsvg-convert` and
`iconutil`.

## License

Copyright Andy Wick. Licensed under the [Apache License, Version 2.0](LICENSE).
