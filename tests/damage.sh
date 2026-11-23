#!/bin/sh
# Damage and failure test: what happens when stored data, records, caches or the
# source files go wrong, or a run is cut short. Two local nodes and a drive, no
# internet needed.
# Usage: tests/damage.sh path/to/cahoots
set -eu
CAHOOTS=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
T=$(cd "$(mktemp -d /tmp/cahoots-damage.XXXXXX)" && pwd -P)
export CAHOOTS_OFFLINE=1
fail() { echo "FAILED: $1" >&2; exit 1; }
step() { echo "  $1"; }
a() { CAHOOTS_HOME=$T/alice "$CAHOOTS" "$@"; }
b() { CAHOOTS_HOME=$T/bob "$CAHOOTS" "$@"; }
cleanup() {
    a stop >/dev/null 2>&1 || true
    b stop >/dev/null 2>&1 || true
    chmod -R u+rwx "$T" 2>/dev/null || true
    rm -rf "$T"
}
trap cleanup EXIT
invite() { "$@" invite | grep -o 'cahoots1[a-z0-9]*'; }
# The stored copies on the drive, largest first.
chunks() { find drive/sdb-store -path '*chunks*' -type f -exec ls -S {} +; }
records() { find drive/sdb-store -path '*snapshots*' -type f; }
# Verify must notice, repair from bob, and then find nothing wrong.
heals() {
    a verify --full >verify.out 2>&1 || true
    grep -q "repaired from another friend" verify.out || fail "$1: not noticed or not repaired ($(tail -1 verify.out))"
    a verify --full | grep -q "all good" || fail "$1: still wrong after the repair"
}
# The daemon holds its home's lock file open; where lsof is missing, /proc says whose home is whose.
daemon_pid() {
    if command -v lsof >/dev/null 2>&1; then
        lsof -t "$T/$1/lock" 2>/dev/null | head -1
    else
        grep -lsa "CAHOOTS_HOME=$T/$1" /proc/[0-9]*/environ | head -1 | cut -d/ -f3
    fi
}

cd "$T"
mkdir -p data/sub drive
a init --name alice >/dev/null
b init --name bob >/dev/null
a pause >/dev/null
b status >/dev/null
a join "$(invite b)" | grep -q "Paired with bob" || fail "pairing"
a drive usb drive >/dev/null
head -c 3000000 /dev/urandom >data/big
head -c 700000 /dev/urandom >data/second
echo one >data/sub/note.txt
a add data >/dev/null
a backup | grep -q "created on" || fail "first backup"

step "a truncated chunk"
V=$(chunks | head -1)
head -c 1000 "$V" >cut && cat cut >"$V"
heals "truncated chunk"

step "an empty chunk"
: >"$(chunks | head -1)"
heals "empty chunk"

step "a chunk with bytes added"
printf 'extra bytes' >>"$(chunks | head -1)"
heals "chunk with a tail"

step "one chunk under another chunk's name"
FIRST=$(chunks | sed -n 1p)
OTHER=$(chunks | sed -n 2p)
cat "$OTHER" >"$FIRST"
heals "swapped chunk"

step "a missing chunk"
rm "$(chunks | head -1)"
heals "missing chunk"

step "every chunk on a drive ruined"
for f in $(chunks); do printf 'ROT' | dd of="$f" bs=1 seek=40 conv=notrunc 2>/dev/null; done
rm -rf alice/cache
a restore latest --to anyway >/dev/null 2>&1 || fail "restore did not fall back to the friend"
diff -r anyway/data data || fail "the fallback restore differs"
a restore latest --to onlyusb --from usb >onlyusb.out 2>&1 && fail "a ruined drive restored without complaint"
[ -z "$(find onlyusb -type f 2>/dev/null)" ] || fail "files were written from ruined data"
heals "ruined drive"
a restore latest --to healed --from usb >/dev/null || fail "restore from the repaired drive"
diff -r healed/data data || fail "the repaired drive restores something else"

step "a damaged restore point record"
R=$(records | head -1)
printf 'ROT' | dd of="$R" bs=1 seek=20 conv=notrunc 2>/dev/null
a snapshots >snaps.out 2>&1 || fail "listing stopped at a damaged record"
grep -q "is damaged" snaps.out || fail "the damaged record was not reported"
a restore latest --to pastrecord >/dev/null || fail "a damaged record on one destination blocked restore"
a backup >/dev/null || fail "backup after a damaged record"
a snapshots 2>&1 | grep -q "is damaged" && fail "the next backup did not replace the damaged record"

step "an empty restore point record"
: >"$(records | head -1)"
a snapshots >snaps.out 2>&1 || fail "listing stopped at an empty record"
a backup >/dev/null || fail "backup after an empty record"
a snapshots 2>&1 | grep -q "is damaged" && fail "the empty record was not replaced"
a verify --full | grep -q "all good" || fail "verify after record damage"

step "damaged local caches and notes"
find alice/cache -type f | while read -r f; do echo junk >"$f"; done
echo '{{{ not json' >alice/forgotten.json
a snapshots >/dev/null || fail "snapshots with a damaged cache"
a ls latest | grep -q "sub/note.txt" || fail "ls with a damaged cache"
echo two >data/sub/note.txt
a backup | grep -q "created on" || fail "backup with a damaged cache"
a restore latest --to aftercache >/dev/null || fail "restore with a damaged cache"
diff -r aftercache/data data || fail "restore after cache damage differs"
a status >/dev/null || fail "status with a damaged cache"

step "a chunk lost everywhere comes back with the next backup"
LOST=$(basename "$(chunks | head -1)")
find drive/sdb-store bob/store -name "$LOST" -type f -exec rm {} +
a verify --full >lost.out 2>&1 && fail "verify passed with a chunk gone everywhere"
grep -q "not available elsewhere" lost.out || fail "the loss was not reported"
a backup >/dev/null || fail "backup after the loss"
a verify --full | grep -q "all good" || fail "the next backup did not bring the chunk back"

step "a broken config or key file is an error, not a crash"
a stop >/dev/null
sleep 1
cp alice/config.toml config.keep
echo 'name = [broken' >alice/config.toml
a status >broken.out 2>&1 && fail "a broken config was accepted"
grep -q "bad config" broken.out || fail "the broken config was not named"
grep -qi "panicked" broken.out && fail "a broken config crashed cahoots"
cp config.keep alice/config.toml
cp alice/identity.key identity.keep
head -c 20 identity.keep >alice/identity.key
a status >broken.out 2>&1 && fail "a cut-off key file was accepted"
grep -qi "panicked" broken.out && fail "a cut-off key file crashed cahoots"
cp identity.keep alice/identity.key
a status >/dev/null || fail "status after putting the files back"

step "odd names, empty things, links, a pipe and an unreadable file"
mkdir -p "data/odd/empty dir" "data/odd/ leading space"
: >data/odd/empty-file
echo dash >"data/odd/-dash"
echo quote >"data/odd/it's \"quoted\""
echo uni >"data/odd/ünïcödé-日本語"
echo long >"data/odd/$(printf 'n%.0s' $(seq 1 240))"
echo nl >"$(printf 'data/odd/new\nline')"
echo tab >"$(printf 'data/odd/tab\there')"
ln -s nowhere data/odd/broken-link
ln -s "empty dir" data/odd/dir-link
mkfifo data/odd/pipe
echo secret >data/odd/locked
chmod 000 data/odd/locked
a backup >odd.out 2>&1 &
ODD=$!
for _ in $(seq 1 60); do
    kill -0 "$ODD" 2>/dev/null || break
    sleep 0.5
done
kill -0 "$ODD" 2>/dev/null && fail "the backup hung (a pipe in the folder?)"
wait "$ODD" || fail "backup of odd names: $(tail -1 odd.out)"
grep -q "unreadable skipped" odd.out || fail "the unreadable file was not mentioned"
a ls latest >ls.out
[ "$(grep -c 'new' ls.out)" = 1 ] || fail "a name with a line break took more than one line"
a restore latest --to odd >/dev/null 2>&1 || fail "restore of odd names"
for n in empty-file -dash "it's \"quoted\"" "ünïcödé-日本語" "$(printf 'n%.0s' $(seq 1 240))" "$(printf 'new\nline')" "$(printf 'tab\there')"; do
    cmp -s "odd/data/odd/$n" "data/odd/$n" || fail "odd name not restored: $n"
done
[ -d "odd/data/odd/empty dir" ] || fail "an empty folder was not restored"
[ -d "odd/data/odd/ leading space" ] || fail "a folder with a leading space was not restored"
[ "$(readlink odd/data/odd/broken-link)" = nowhere ] || fail "a broken link was not restored as a link"
[ -L odd/data/odd/dir-link ] || fail "a link to a folder was not restored as a link"
[ ! -e odd/data/odd/locked ] || fail "an unreadable file appeared in the restore"
[ ! -e odd/data/odd/pipe ] || fail "a pipe was restored as something"
chmod 600 data/odd/locked
rm -f data/odd/pipe

step "a file that changes while it is backed up"
head -c 2000000 /dev/urandom >data/growing
(
    for _ in $(seq 1 30); do
        head -c 50000 /dev/urandom >>data/growing
        sleep 0.1
    done
) &
WRITER=$!
a backup >/dev/null 2>&1 || fail "backup while a file was growing"
wait "$WRITER"
a verify --full | grep -q "all good" || fail "a growing file left something unreadable behind"
a backup >/dev/null || fail "backup after the file settled"
a restore latest data/growing --to grown >/dev/null || fail "restore of the settled file"
cmp grown/data/growing data/growing || fail "the settled file was not captured"

step "the daemon killed in the middle of a backup"
BEFORE=$(a snapshots | wc -l)
a limit 0.3 >/dev/null
head -c 5000000 /dev/urandom >data/interrupted
a backup >/dev/null 2>&1 &
CLIENT=$!
sleep 2
PID=$(daemon_pid alice)
[ -n "$PID" ] || fail "could not find the daemon to kill"
kill -9 "$PID"
wait "$CLIENT" 2>/dev/null || true
a limit 0 >/dev/null
[ "$(a snapshots | wc -l)" = "$BEFORE" ] || fail "a half-finished backup left a restore point"
a backup | grep -q "created on" || fail "backup after the kill"
a verify --full | grep -q "all good" || fail "verify after the kill"
a restore latest data/interrupted --to resumed >/dev/null || fail "restore after the kill"
cmp resumed/data/interrupted data/interrupted || fail "the interrupted file differs"

step "a restore stopped half way leaves no file that only looks finished"
head -c 60000000 /dev/urandom >data/huge
a backup >/dev/null || fail "backup of a large file"
a restore latest data/huge --to halfway --from usb >/dev/null 2>&1 &
RESTORE=$!
sleep 0.2
kill "$RESTORE" 2>/dev/null || true
wait "$RESTORE" 2>/dev/null || true
sleep 1
if [ -e halfway/data/huge ]; then
    cmp -s halfway/data/huge data/huge || fail "a half-restored file was left under its real name"
fi
a restore latest data/huge --to halfway --from usb >/dev/null || fail "restoring again over a stopped restore"
cmp halfway/data/huge data/huge || fail "the second restore differs"
# The stopped restore, if the daemon was still at it, ends within a few seconds; neither leaves its half-written file.
for _ in $(seq 1 20); do
    [ -z "$(find halfway -name '*.sdb-tmp')" ] && break
    sleep 0.5
done
[ -z "$(find halfway -name '*.sdb-tmp')" ] || fail "a half-written file was left behind: $(find halfway -name '*.sdb-tmp')"
rm -rf halfway data/huge

step "restoring over what is already there, and where a file is in the way"
a restore latest data/sub --to anyway >/dev/null || fail "restoring over an earlier restore"
[ "$(cat anyway/data/sub/note.txt)" = two ] || fail "an existing file was not replaced"
mkdir blocked
echo x >blocked/data
a restore latest data/sub --to blocked >blocked.out 2>&1 && fail "restore claimed success with a file in the way"
grep -qi "panicked" blocked.out && fail "a file in the way crashed the restore"
[ "$(cat blocked/data)" = x ] || fail "the file in the way was destroyed"

step "a folder that is missing is not recorded as an empty one"
BEFORE=$(a snapshots | wc -l)
mv data data.away
a backup >gone.out 2>&1 && fail "a backup of a missing folder claimed success"
grep -q "is missing or cannot be read" gone.out || fail "the missing folder was not named: $(tail -1 gone.out)"
[ "$(a snapshots | wc -l)" = "$BEFORE" ] || fail "a missing folder left a restore point"
mv data.away data
a backup >/dev/null || fail "backup once the folder is back"

step "a friend with no room is skipped, the drive still gets the backup"
b destination quota alice 0 >/dev/null
echo three >data/sub/note.txt
a backup >full.out 2>&1 || true
grep -q "created on usb" full.out || fail "the drive missed a backup because a friend was full: $(tail -1 full.out)"
b destination quota alice 100 >/dev/null
a backup >/dev/null || fail "backup after the friend made room"

step "pruning with a friend away, then everything left still restores"
a backup --tag keepme >/dev/null
for n in 1 2 3; do
    echo "v$n" >data/sub/note.txt
    a backup >/dev/null
done
b stop >/dev/null
sleep 1
a keep --last 1 --hourly 0 --daily 0 --weekly 0 --monthly 0 >/dev/null
a prune >prune.out 2>&1 || true
grep -q "restore points kept" prune.out || fail "prune did not finish at the drive: $(tail -1 prune.out)"
b status >/dev/null
a prune >/dev/null 2>&1 || fail "prune once the friend was back"
a verify --full | grep -q "all good" || fail "pruning removed something still needed"
a snapshots | grep -q keepme || fail "a tagged restore point was pruned"
for id in $(a snapshots | awk 'NR>1 {print $1}'); do
    a restore "$id" --to "after-prune-$id" >/dev/null 2>&1 || fail "restore point $id no longer restores after pruning"
done
[ "$(cat "after-prune-$(a snapshots | awk 'END {print $1}')/data/sub/note.txt")" = v3 ] || fail "the newest restore point is not the newest state"

for n in alice bob; do
    grep -i "panicked" "$n/daemon.log" && fail "$n's daemon panicked during the test"
done
echo "damage test passed"
