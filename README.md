<p align="center">
  <img src="assets/logo.svg" width="150" alt="Cahoots Backup: two backup drives leaning on each other, the front one with a padlock">
</p>

# Cahoots Backup

Encrypted, deduplicated backups between friends. Each person runs `cahoots` on their
Mac, Linux or Windows computer; you add each other with one-time invites and your
computers back up to one another, and optionally to a local drive or a cloud
bucket as well. Friends, and the cloud, only ever hold ciphertext.

**[cahootsbackup.com](https://cahootsbackup.com)** has the short tour, with pictures.

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
fitted into the evenings suddenly did. Cahoots Backup is the result: the
part of CrashPlan Home we missed most, rebuilt for today. Invite a friend, swap
some disk space, and let your computers look after each other.

Most important: it is open source, it has CLI, TUI and GUI interfaces, and it
is written in Rust instead of Java!!!

It is new software, so treat it that way for now: run it next to whatever
protects your files today. It is not affiliated with CrashPlan or its makers.

## Quick start

Download `cahoots` for macOS, Linux or Windows from the
[releases page](https://github.com/awick/cahoots-backup/releases), or build
it yourself with `cargo build --release` (the binary is `target/release/cahoots`).
The downloads are not signed, so a Mac asks before running one the first time;
`xattr -d com.apple.quarantine cahoots` tells it you trust it.

```bash
cahoots tui                            # interactive: backup sets, restore, friends, settings
cahoots gui                            # the same in a window
```

Or from the command line:

```bash
cahoots init --name andy
cahoots add ~/Documents ~/Pictures
cahoots invite                         # prints `cahoots join cahoots1...` to send to a friend
cahoots join cahoots1...                   # what the friend runs (or you, with their invite)
cahoots service install                # run the daemon at login (launchd / systemd)
cahoots recovery export                # writes NAME.cahoots-recovery-kit; keep it OFF this computer
```

One invite pairs both directions. The daemon watches your folders and creates a
restore point shortly after files stop changing. Every command is listed under
[Commands](#commands), further down.

## Recovering on a new computer

Your computer is lost, stolen or dead. Two things get your files back: the
**recovery kit** with its passphrase, and any one destination that holds your
backups. Nothing has to survive on the old computer: the list of files in every
restore point is stored, encrypted, at the destinations along with the data.

**Before it happens**, export the kit and keep it, and its passphrase, somewhere
other than that computer (a password manager, a USB stick, a friend):

```bash
cahoots recovery export
```

That writes `NAME.cahoots-recovery-kit` in the folder you are in, NAME being this
computer's name; `cahoots recovery export FILE` puts it elsewhere.

In the GUI it is Settings, Essentials, Recovery kit, Export. Export it again
after adding a friend, a drive or cloud storage, since a kit only knows the
destinations that existed when it was written; `cahoots status` and Settings say
when it is out of date.

The passphrase has to be at least 16 characters, and length is the only rule: a
sentence you will remember works well.

**A copy that keeps itself up to date (optional).** cahoots can keep the kit in a
folder for you and write it again whenever your settings change. Point it at a
folder that a cloud drive syncs (iCloud Drive, Google Drive, Dropbox, OneDrive)
and the kit is always off this computer and never out of date:

```bash
cahoots recovery copy-to ~/Dropbox
```

It asks for the kit's passphrase once. On a Mac the folder can be left out, and
it is then Documents, which iCloud Drive carries off the computer when "Desktop
& Documents Folders" is switched on in its settings. In the GUI it is Settings,
Essentials, Kept in. The file is `NAME.cahoots-recovery-kit`, and the kit it
replaced is kept next to it with `.before` on the end. Two computers of the
same name can share a folder: the second adds the start of its id to the name
rather than writing over the first. `cahoots recovery copy-to --now` writes it again
and `--off` stops. To recover, fetch that file and import it like any other
kit. Whoever can read the folder gets the sealed kit, so the passphrase is what
protects it; cahoots needs no account or sign-in for this, and nothing passes
through a server of this project's.

**When it happens**, on the new computer:

```bash
cahoots recovery import andy.cahoots-recovery-kit    # asks for the passphrase
cahoots status                                   # are your destinations reachable?
cahoots snapshots                                # the restore points, and the set each belongs to
cahoots restore latest --set default --in-place  # put that set's files back where they were
cahoots service install                          # carry on backing up from here
```

- The import makes the new computer the same one as far as your friends can
  tell: same identity, same key, same friends, drives, cloud storage and backup
  sets. Your friends do nothing.
- Restore each backup set you have (`--set NAME`); `--to FOLDER` instead of
  `--in-place` puts the files in a folder to look at first. `cahoots gui` and
  `cahoots tui` do the same from their Restore page.
- A drive has to be plugged in where it was before; a friend's computer has to
  be on.
- You get back what had been backed up. Changes made after the last backup are
  gone.
- Do not run the old computer and the new one at the same time.
- Without the kit, or without its passphrase, the backups cannot be decrypted
  by anyone.

## The TUI

`cahoots tui` has a menu on the left and, on the right, whatever the highlighted
item is about. Enter goes into an item and the menu changes to that item's own
choices (Backup, then a backup set, then its files, destinations or schedule);
Esc goes back up. Up and down move, and that is all there is to learn. When the
cursor moves into the right-hand side (a folder tree, a list to tick), space
marks things and Esc returns to the menu.

The first screen shows every backup set with its files, its destinations (and
when each last got a backup), its schedule, and what it is doing right now.

| Page | What is there |
| --- | --- |
| Backup | the backup sets and their status; create a set, choose its files, its destinations, its exclusion list and its schedule and history rules; back up now, verify, and take what the set now leaves out from its older restore points |
| Restore | choose a backup set, then browse its files as of any restore point, search by name, step through time, list a file's versions, choose which destination to read from, restore |
| Friends | friends' computers, and drives and cloud storage of your own, each under its own heading: whether each is reachable and how, what you store there, what they store here; invite, accept an invite, add a drive or cloud storage, set pace, quota or cap, and how much history each keeps; clean up what sets no longer sent there left behind |
| Settings | the essentials first (this computer's name, the most to send and to receive per second, and whether the recovery kit has been exported, with a button to export it), then whether friends may back up to this computer and how much room they get, then hours, pause, pace, warnings and history rules that apply to every set; the exclusion lists |
| History | what the daemon has done since it started |
| Status | the full `cahoots status` report |

## The GUI

`cahoots gui` opens the same thing in a window: the backup sets with their status,
restore with a timeline, friends, settings, history and status. A backup
set opens on the page, with tabs for its files, destinations and schedule. It is
another client of the daemon, built with egui into the same binary. It needs a
desktop session only when you run it; everything else works without one. Build
with `cargo build --release --no-default-features` to leave it out.

## The daemon

One background process per computer does all the work: it serves your friends,
watches your files and runs backups. Every `cahoots` command and the TUI are clients
that talk to it over a private local socket. The first command starts it if it
is not running, and replaces it if it was started by an older build;
`cahoots stop` stops it and `cahoots service install` starts it at login. Its log is
`~/.cahoots/daemon.log`.

## On Windows

The download holds two programs. `cahoots.exe` is the command. `cahootsw.exe` is the same
program without a console: start it from Explorer for the window, and it is what
`cahoots service install` has Windows start at login to run the daemon. Everything lives
in `%LOCALAPPDATA%\Cahoots`.

A Windows computer works with friends on any system, and what one system backed up
restores on another from its recovery kit. What is different on Windows:

- There is no owner or permission to record: only the read-only flag is kept and put back.
- Making a link during a restore needs Developer Mode or an administrator. Without either,
  each link is reported and left out.
- A file OneDrive keeps online only is not fetched. One that was backed up before stays in
  the backup; a new or changed one is counted in the backup's summary line.
- Restoring a Mac or Linux backup, a name Windows cannot hold is spelled out (`a:b` becomes
  `a%3Ab`) and a second file whose name differs only in capitals gets a number. Each is
  reported.
- The downloads are not signed, so Windows shows "Windows protected your PC" the first
  time: choose More info, then Run anyway.
- There are no desktop notifications yet.

To build it yourself you need the Visual Studio Build Tools with the C++ workload, and for
an Arm computer also `clang`. `cargo build --release --features windowed` builds both
programs.

## Running the daemon in Docker

For a machine that only needs the daemon, such as a server that stores friends'
backups, there is an image for amd64 and arm64 Linux. It has no window; the
command line and the TUI are both in it.

```bash
docker run -d --name cahoots --restart unless-stopped --network host \
  -e CAHOOTS_NAME=myserver \
  -v /srv/cahoots/config:/config -v /bigdisk/cahoots:/backups \
  ghcr.io/awick/cahoots-backup
```

Or in a `docker-compose.yml`:

```yaml
services:
  cahoots:
    image: ghcr.io/awick/cahoots-backup:latest
    container_name: cahoots
    network_mode: host
    environment:
      - CAHOOTS_NAME=myserver
    volumes:
      - /srv/cahoots/config:/config
      - /bigdisk/cahoots:/backups
    restart: unless-stopped
```

The first start sets the node up. It keeps two folders:

| In the container | What is there |
| --- | --- |
| `/config` | the node's identity, its settings and its caches: small, and the part to look after |
| `/backups` | what friends store on this node: mount the disk with the room here |

The daemon runs as user 1000, which must be able to write to both
(`chown 1000` them, or add `--user "$(id -u):$(id -g)"` to run as yourself).

Every `cahoots` command works through `docker exec`:

```bash
docker exec cahoots cahoots invite                        # a code to send to a friend
docker exec cahoots cahoots join cahoots1...                  # or accept theirs
docker exec cahoots cahoots destination quota andy 500    # how much a friend may store
docker exec cahoots cahoots status
docker exec -it cahoots cahoots tui                       # the interface in the terminal
```

To back up folders of the machine itself, mount them and add them:
`-v /home/me/photos:/backup/photos:ro`, then
`docker exec cahoots cahoots add /backup/photos`. Keep the recovery kit outside the
container: `docker exec -it cahoots cahoots recovery export /config/NAME.cahoots-recovery-kit`, then
move that file somewhere else.

`--network host` lets friends reach the node directly and lets it ask the
router to forward its port. Without it, give the node a fixed port the first
time it starts and publish that: `-e CAHOOTS_PORT=45000 -p 45000:45000/udp`. It
works either way, since nodes fall back to the relay, only more slowly.

`:latest` is the newest release and each release also has its own tag. Friends
need a release that speaks the same protocol version, so update both ends
together: `docker pull`, then remove and start the container again; the node
lives in the two folders, not in the container. To build the image yourself, run
`docker build -t cahoots .` in a checkout, or in a compose file give the checkout
as `build:` next to `image: cahoots`.

A node that already runs on the machine can move into the container and stay
the same node to its friends: stop it (`cahoots service uninstall`, `cahoots stop`),
copy `~/.cahoots` into the folder mounted at `/config`, and set
`store_dir = "/backups"` in its `config.toml`, mounting the folder that held
friends' data there.

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
`min_free_gb` (default 5) free. `cahoots status` reports how many unknown callers
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

**One-way friends.** A computer can back up to its friends without taking
anything from them: untick "friends may back up to this computer" in Settings,
or run `cahoots accept off`. What friends already keep there stays, and they can
still restore or remove it, but nothing new is accepted. On their side that
computer shows a purple dot and "takes no backups from you", and their backups
skip it without deleting anything.

**Backup sets.** A set is a named group of folders with its own destinations,
pace, hours and history rules; folders start in the `default` set, which goes to
every destination. Each restore point belongs to one set. Sets share storage, so
identical data in two sets is stored once per destination. In the TUI and GUI
the Backup page lists the sets and has an entry for making a new one.

**Retention.** `keep_last / hourly / daily / weekly / monthly` choose which
restore points survive, `keep_days` caps their age, and `max_versions` (global
or per pattern) removes older versions of a file from older restore points.
Tagged restore points are exempt. Pruning then deletes unreferenced chunks at
each destination. A destination can have history rules of its own
(`cahoots destination keep`), so a drive can keep two years while a cloud bucket
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
A cap in GB (`cahoots destination quota`) works like a friend's quota: when it is
reached the oldest restore points there go first, never the newest of a set.
Restores read from a local drive first, then friends, then the cloud, since
cloud downloads usually cost money. Plain `http://` endpoints are only accepted
on this computer itself. Storage whose certificate no public authority vouches
for (a private server, an emulator) is refused unless you name its certificate
with `--ca-file`; that certificate, and nothing else, is then trusted for that
one destination.

**What is left out.** Each set uses one exclusion list. `Standard` (the
default) names things nobody wants back: trash, caches, `node_modules`,
`__pycache__` and the like. `Developer` adds build output such as `target`,
`build`, `dist` and `.venv`. Both come with cahoots and can gain patterns in a later
release; to change one, copy it into a list of your own. `.git` is in neither.
A folder you untick in the file picker is left out of that set only. Leaving
something out does not remove it from restore points made so far; it ages out
under the history rules, or `cahoots prune --apply-exclusions` takes it out of them
now (restore points you named stay as they are).

**Quotas.** You set how many GB each friend may store on your computer; the
store refuses anything beyond it. Because only the owner can tell which chunks
belong to which restore point, the owner's node does the trimming: when it hits
(or is over) a quota it removes its oldest restore points at that friend,
never the newest of any set, and retries.

**Going back in time.** Restore, in the TUI and the GUI, starts with the choice
of backup set and then shows that set's files as of any restore point. Its menu
goes back in time, jumps to a date and lists the versions of the highlighted
file, and entries are marked *different now*, *deleted since* or *changes
inside* relative to the latest restore point.

**Corruption.** Every chunk is authenticated (AEAD) and its plaintext is
re-hashed on restore, and the restored size must match, so damaged data is
never written back silently; restore falls through to another destination when
one copy is bad. A friend's node keeps a
BLAKE3 checksum of each stored chunk, so it can detect disk rot without any key
(`cahoots scrub`, and on every read) and drops damaged chunks so they show up as
missing. `cahoots verify` finds missing or damaged chunks and repairs them from
another friend; what no friend has is uploaded again by the next backup if the
file is still unchanged locally. `cahoots verify --local` re-reads files whose size
and mtime are unchanged and compares fingerprints with the last backup. The
daemon runs a scrub and a sampled verify weekly, and prunes daily. It remembers
when it last did each, so restarting it does not put them off, and it leaves
both alone for the first ten minutes after it starts.

**Format versions.** Every stored thing names its format so a later release can
tell old from new: `format` and `created` in `config.toml`, `identity.key` and
the recovery kit; a version byte at the start of every encrypted blob and
restore point manifest (which also records its time and the cahoots release that
wrote it); `SDBI`/`SDBC` magic plus version on snapshot indexes and stored
chunk files; `cahoots1` on invites; and a versioned protocol name on the wire. A
release that meets a newer format stops with an "upgrade cahoots" message instead
of guessing.

**The recovery kit.** `cahoots recovery export` writes your identity, master key,
settings and friend list into one file, encrypted with a key made from your
passphrase of 16 characters or more (Argon2id, 64 MiB of memory, with a random
salt). The master key
itself is 256 random bits, not made from the passphrase, so the data at your
friends cannot be guessed open; the passphrase only guards the kit file. See
[Recovering on a new computer](#recovering-on-a-new-computer).

## Files

Everything lives in `~/.cahoots`, or `%LOCALAPPDATA%\Cahoots` on Windows (override with
`CAHOOTS_HOME`):
`config.toml` (also holds any cloud storage keys), `identity.key` (secret),
`store/` (friends' encrypted data, move it with `store_dir`), `cache/`.

## Limits worth knowing

- Each chunk is one file on the friend's disk; very large numbers of tiny files
  work but are not packed together.
- Extended attributes, ACLs and hard-link relationships are not recorded.
- Discovery and relaying use n0's free public servers, which are rate-limited
  and carry no uptime guarantee; `cahoots` has no setting for your own servers yet.
- A friend can delete or withhold what they hold for you; back up to more than
  one friend and run `cahoots verify` occasionally.
- On macOS the binary needs Full Disk Access to read Documents, Desktop and
  Downloads when run by launchd.

## Commands

Everything the TUI and GUI do, and a few things only the command line does.

| Command | What it does |
| --- | --- |
| `cahoots status` | folders, destinations, usage, how you are reachable |
| `cahoots backup [--tag NAME]` | restore point now; tagged ones are kept until `cahoots forget` |
| `cahoots snapshots` | list restore points |
| `cahoots history FILE` | every version of one file |
| `cahoots diff A B` / `cahoots ls SNAP [PATH]` | what changed / what is inside |
| `cahoots restore SNAP [PATHS] --to DIR` | SNAP is an id, a tag, `latest`, or a date such as `2026-11-20` or `"2026-11-20 14:30"`; `~/Documents/a.txt` lands at `DIR/Documents/a.txt`; `--from NAME` uses one friend or drive only; `--set NAME` makes `latest` that backup set's newest |
| `cahoots keep --days 365 --versions 10` | maximum age and versions per file |
| `cahoots keep --versions 5 --pattern '**/*.iso'` | version limit for matching files only |
| `cahoots prune [--dry-run]` | apply the keep rules now (the daemon does it daily) |
| `cahoots prune --apply-exclusions [--set NAME] [--dry-run]` | also take out of older restore points what the exclusion lists and skipped folders now leave out |
| `cahoots add PATH --set NAME` | put a folder in a backup set (created if new) |
| `cahoots set NAME --to usb,barney --every 15 --hours 22-7 --versions 5 --days 90` | where a set goes, how often, when, and its own history rules |
| `cahoots sets` / `cahoots backup --set NAME` | list sets / back up one set now |
| `cahoots drive NAME PATH` | also back up to an external drive or folder (encrypted the same way; skipped while unplugged) |
| `cahoots cloud NAME --endpoint URL --bucket B --access-key K` | also back up to cloud storage: any S3-compatible bucket (Amazon S3, Backblaze B2, Cloudflare R2, Wasabi, MinIO); the secret key is asked for, never typed on the command line; `--ca-file FILE` is for storage with a certificate of its own |
| `cahoots destination keep NAME --days 30 --last 5` | how much history one destination keeps, instead of each set's rules (`--default` goes back) |
| `cahoots every MINUTES [NAME]` | how often automatic backups go to one destination (friends default to 60 minutes, a new drive to 15) |
| `cahoots name NAME` | give this computer another name (friends you already have keep the name they know) |
| `cahoots limit MBPS` | cap the speed of your backups going out, to friends and cloud storage, in MB/s such as 0.5 or 20 (0 = unlimited) |
| `cahoots limit --incoming MBPS` | cap what all your friends together send to your computer (0 = unlimited) |
| `cahoots accept off` / `cahoots accept on` | whether friends may back up to your computer; off makes it one that only backs up to its friends |
| `cahoots default-quota GB` | what a friend you pair with from now on may store on your computer (100 unless you change it) |
| `cahoots keep-free GB` | friends' backups are refused while your disk has less free than this (default 5, 0 = no check) |
| `cahoots schedule 22-7` / `cahoots schedule always` | hours of the day automatic backups may run |
| `cahoots pause [HOURS]` / `cahoots resume` | hold automatic backups |
| `cahoots warn-after DAYS` | warn when a backup, yours or a friend's to you, is this stale (default 3) |
| `cahoots destinations` | list friends and drives |
| `cahoots destination quota NAME GB` | how much a friend may store on your computer; for cloud storage, the most you keep there (0 = no cap) |
| `cahoots destination limit NAME MBPS` | fastest one friend may send backups to your computer, in MB/s (0 = unlimited) |
| `cahoots exclude lists` | the exclusion lists, their patterns and which sets use them |
| `cahoots set NAME --exclude Developer` | the exclusion list a set uses (`Standard` unless you choose) |
| `cahoots exclude new NAME [--from LIST]` / `cahoots exclude add LIST PATTERN` / `cahoots exclude rm LIST PATTERN` / `cahoots exclude delete LIST` | make and change lists of your own |
| `cahoots skip PATH [--remove]` | leave one folder or file out of the set that backs it up |
| `cahoots set NAME --rename NEW` | rename a backup set; its restore points stay with it |
| `cahoots destination backup-to NAME false` | stop using a destination |
| `cahoots destination clean NAME [--dry-run]` | after taking a destination off a set: delete that set's restore points there and the chunks only they used |
| `cahoots verify --set NAME` | the same checks for one backup set (also on each set in the TUI and GUI) |
| `cahoots verify [--full]` | check friends still hold your data and it decrypts; repairs damage from another friend |
| `cahoots verify --local` | fingerprint your unmodified files against the last backup to catch silent disk corruption |
| `cahoots scrub` | check the data friends store on your computer for disk damage |

## Development

```bash
git config --local core.hooksPath .githooks/   # once per clone: quick checks before every commit
./check.sh quick                               # what that hook runs: formatting, lints, unit tests
./check.sh                                     # those plus the end-to-end, damage and cloud tests
```

GitHub runs the full `./check.sh` on Linux, macOS and Windows (there under Git Bash) for
every push and pull request, so a commit only waits for the quick part.

`tests/e2e.sh` starts three local nodes and a drive (no internet needed) and
exercises pairing, backup, restore, sets, a refused stranger, corruption
detection and repair, pruning and exclusion lists.

`tests/damage.sh` ruins things on purpose and checks what happens: truncated,
emptied, swapped and missing chunks, damaged restore point records, caches and
config, odd file names, a file that changes mid-backup, a daemon killed
mid-backup, a restore stopped half way, a friend with no room, and pruning
while a friend is away.

`tests/cloud.sh` tests cloud destinations against a local stand-in for S3, and
can use two. `./check.sh` runs it against `tests/mini-aws.pl`, a small S3
server in Perl shared with the Arkime tests: nothing to install, done in
seconds. GitHub also runs it against
[AWS Emulator](https://github.com/aws/aws-emulator), AWS's own stand-in, which
checks signatures and serves https with a certificate of its own. To do that
yourself you need the AWS CLI and either the `aws-emulator` program (Linux) or
Docker (macOS): `CAHOOTS_TEST_S3=emulator tests/cloud.sh target/release/cahoots`.

To publish a release, raise `version` in `Cargo.toml`, push, and run the
`release` workflow on GitHub (`gh workflow run release`). It names the release
after that version and attaches binaries for macOS, Linux and Windows, built with the
slower, smaller `dist` profile (`cargo build --profile dist`). It also pushes the
Docker images to `ghcr.io/awick/cahoots-backup`. GitHub builds and tries
the image on every push (`tests/docker.sh`), without publishing it. To rehearse a
release without publishing anything, run it with `-f dry_run=true`.

The app icon is drawn in `assets/icon.svg`; `icon.png` (built into the window)
and `icon.icns` (for a Mac app bundle) are made from it with `rsvg-convert` and
`iconutil`.

## License

Copyright Andy Wick. Licensed under the [Apache License, Version 2.0](LICENSE).
