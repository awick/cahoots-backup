#!/bin/sh
# End-to-end test: three local nodes and a drive, no internet needed.
# Usage: tests/e2e.sh path/to/sdb
set -eu
SDB=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
T=$(mktemp -d /tmp/sdb-e2e.XXXXXX)
export SDB_OFFLINE=1
fail() { echo "FAILED: $1" >&2; exit 1; }
step() { echo "  $1"; }
# Every command talks to that node's daemon, starting it when needed.
a() { SDB_HOME=$T/alice "$SDB" "$@"; }
b() { SDB_HOME=$T/bob "$SDB" "$@"; }
m() { SDB_HOME=$T/mallory "$SDB" "$@"; }
cleanup() {
    a stop >/dev/null 2>&1 || true
    b stop >/dev/null 2>&1 || true
    m stop >/dev/null 2>&1 || true
    rm -rf "$T"
}
trap cleanup EXIT
invite() { "$@" invite | grep -o 'sdb1[a-z0-9]*'; }

cd "$T"
mkdir -p data/sub photos drive
a init --name alice >/dev/null
b init --name bob >/dev/null
m init --name mallory >/dev/null
# Only explicit backups in this test: hold the daemons' automatic ones.
a pause >/dev/null
m pause >/dev/null
b status >/dev/null

step "pairing"
a join "$(invite b)" | grep -q "Paired with bob" || fail "pairing"
b friends | grep -q alice || fail "pairing was not mutual"

step "backup, change, backup"
head -c 3000000 /dev/urandom >data/big
echo one >data/sub/note.txt
ln -s note.txt data/sub/link
a add data >/dev/null
a backup | grep -q "restore point .* created on bob" || fail "first backup"
a backup | grep -q "no changes" || fail "an unchanged tree made a new restore point"
echo two >data/sub/note.txt
a backup --tag second | grep -q "created on bob" || fail "second backup"
[ "$(a history data/sub/note.txt | grep -c modified)" = 2 ] || fail "history should show two versions"

step "restore and compare"
a restore latest --to out >/dev/null
diff -r out/data data || fail "restored tree differs"
FIRST=$(a snapshots | awk 'NR==2 {print $1}')
a restore "$FIRST" data/sub/note.txt --to old >/dev/null
[ "$(cat old/data/sub/note.txt)" = one ] || fail "old version not restored"

step "an unknown node is refused"
B_ID=$(b status | awk '/node id/ {print $3}')
B_ADDR=$(grep -A12 'name = "bob"' alice/config.toml | grep -o '"[0-9][0-9.]*:[0-9]*"' | head -1 | tr -d '"')
m stop >/dev/null
sleep 1
grep -v '^friends = \[\]' mallory/config.toml >mallory/c2
printf '\n[[friends]]\nname = "bob"\nid = "%s"\nquota_gb = 1\naddrs = ["%s"]\n' "$B_ID" "$B_ADDR" >>mallory/c2
mv mallory/c2 mallory/config.toml
m add data >/dev/null
m backup >mallory.out 2>&1 && fail "a stranger was allowed to back up"
grep -q "unreachable" mallory.out || fail "unexpected stranger result"
m join "sdb1$(printf 'a%.0s' $(seq 1 90))" >/dev/null 2>&1 && fail "a bogus invite was accepted"

step "drive destination and a second set"
a drive usb drive >/dev/null
echo pic >photos/p.jpg
a add photos --set photos >/dev/null
a set photos --to usb >/dev/null
a backup >backup.out
grep -q '\[photos\] restore point .* created on usb:' backup.out || fail "photos set should go to usb only"
grep -q '\[default\] restore point .* created on usb' backup.out || fail "default set should catch up on the drive"

step "renaming a set keeps its restore points"
a set photos --rename pics | grep -q "is now pics" || fail "rename"
a snapshots | grep -q " pics " || fail "old restore points did not follow the rename"
a snapshots | grep -q " photos " && fail "a restore point is still listed under the old name"
a backup --set pics | grep -q "no changes" || fail "the renamed set forgot what it had backed up"
a set default --rename pics >/dev/null 2>&1 && fail "two sets were given the same name"

step "restore from the drive with the friend offline"
b stop >/dev/null
sleep 1
a restore second --to fromdrive --from usb >/dev/null
[ "$(cat fromdrive/data/sub/note.txt)" = two ] || fail "restore from drive"

step "corruption is detected"
a verify --full >/dev/null 2>&1 || true
VICTIM=$(find drive/sdb-store -path '*chunks*' -type f -size +100k | head -1)
printf 'ROT' | dd of="$VICTIM" bs=1 seek=4000 conv=notrunc 2>/dev/null
rm -rf bad
a restore second data/big --to bad --from usb >bad.out 2>&1 && fail "a damaged chunk was restored"
grep -q "missing or damaged" bad.out || fail "damage not reported"
[ -z "$(find bad -type f 2>/dev/null)" ] || fail "a partial file was left behind"

step "repair from the friend"
b status >/dev/null
a verify --full | grep -q "repaired from another friend" || fail "verify did not repair"
a verify --full | grep -q "all good" || fail "verify after repair"

step "a backup carries on after its client quits"
a limit 1000 >/dev/null
head -c 4000000 /dev/urandom >data/slow
a backup --set default >/dev/null 2>&1 &
CLIENT=$!
sleep 1
kill "$CLIENT" 2>/dev/null || true
wait "$CLIENT" 2>/dev/null || true
for _ in $(seq 1 40); do
    a ls latest data/slow 2>/dev/null | grep -q slow && break
    sleep 0.5
done
a ls latest data/slow | grep -q slow || fail "the backup stopped when its client went away"
a limit 0 >/dev/null

step "a friend's incoming speed limit slows their uploads"
b destination limit alice 1000 >/dev/null
head -c 4000000 /dev/urandom >data/throttled
START=$(date +%s)
a backup --set default >/dev/null
TOOK=$(($(date +%s) - START))
[ "$TOOK" -ge 3 ] || fail "4 MB arrived in ${TOOK}s despite a 1000 KB/s limit"
b destination limit alice 0 >/dev/null

step "verify one set"
a verify --set pics | grep -q "usb: " || fail "verify of one set"
a verify --set pics | grep -q "bob: 0 restore points" || fail "a set that is not sent to bob was counted there"
a verify --set nosuchset >/dev/null 2>&1 && fail "verify accepted a set that does not exist"

step "prune and local fingerprint check"
a keep --last 1 --hourly 0 --daily 0 --weekly 0 --monthly 0 >/dev/null
a prune | grep -q "restore points kept" || fail "prune"
a snapshots | grep -q second || fail "a tagged restore point was pruned"
a verify --local | grep -q "all match" || fail "local check"
echo "end-to-end test passed"
