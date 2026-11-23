#!/bin/sh
# End-to-end test: three local nodes and a drive, no internet needed.
# Usage: tests/e2e.sh path/to/cahoots
set -eu
CAHOOTS=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
T=$(mktemp -d /tmp/cahoots-e2e.XXXXXX)
export CAHOOTS_OFFLINE=1
fail() { echo "FAILED: $1" >&2; exit 1; }
# On Windows this runs under Git Bash, which copies where `ln -s` should link unless told.
case "$(uname -s)" in
MINGW* | MSYS*) WINDOWS=1 && export MSYS=winsymlinks:nativestrict ;;
*) WINDOWS= ;;
esac
step() { echo "  $1"; }
# Every command talks to that node's daemon, starting it when needed.
a() { CAHOOTS_HOME=$T/alice "$CAHOOTS" "$@"; }
b() { CAHOOTS_HOME=$T/bob "$CAHOOTS" "$@"; }
m() { CAHOOTS_HOME=$T/mallory "$CAHOOTS" "$@"; }
# Alice again, on the computer set up from her recovery kit.
r() { CAHOOTS_HOME=$T/again "$CAHOOTS" "$@"; }
cleanup() {
    r stop >/dev/null 2>&1 || true
    a stop >/dev/null 2>&1 || true
    b stop >/dev/null 2>&1 || true
    m stop >/dev/null 2>&1 || true
    rm -rf "$T"
}
trap cleanup EXIT
invite() { "$@" invite | grep -o 'cahoots1[a-z0-9]*'; }

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
# Both ways at once: the side that made the invite reaches its new friend without a restart.
a status | grep -q "^  bob  *online" || fail "the joining side cannot reach its new friend"
b status | grep -q "^  alice  *online" || fail "the inviting side cannot reach its new friend"

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
m join "cahoots1$(printf 'a%.0s' $(seq 1 90))" >/dev/null 2>&1 && fail "a bogus invite was accepted"

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
a limit 1 >/dev/null
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
b destination limit alice 1 >/dev/null
head -c 4000000 /dev/urandom >data/throttled
START=$(date +%s)
a backup --set default >/dev/null
TOOK=$(($(date +%s) - START))
[ "$TOOK" -ge 3 ] || fail "4 MB arrived in ${TOOK}s despite a 1 MB/s limit"
b destination limit alice 0 >/dev/null

step "the incoming limit for all friends slows them too, and takes decimals"
b limit --incoming 0.5 >/dev/null
b status | grep -q "max incoming: 0.5 MB/s" || fail "the incoming limit was not saved as 0.5 MB/s"
head -c 2000000 /dev/urandom >data/throttled2
START=$(date +%s)
a backup --set default >/dev/null
TOOK=$(($(date +%s) - START))
[ "$TOOK" -ge 3 ] || fail "2 MB arrived in ${TOOK}s despite a 0.5 MB/s limit for everyone"
b limit --incoming 0 >/dev/null

step "this computer can be given another name"
a name renamed | grep -q "now called renamed" || fail "the name was not changed"
a status | grep -q "name: *renamed" || fail "status does not show the new name"
a name alice >/dev/null

step "a command right after stop gets a new daemon, not the one leaving"
for _ in 1 2 3; do
    a stop >/dev/null
    a status >/dev/null || fail "the command after stop was cut off"
done

step "a computer that takes no backups from friends"
HELD=$(b destinations | grep alice)
b accept off | grep -q "no longer back up" || fail "accept off was not saved"
echo closed >data/closed
a backup --set default >backup.out 2>&1 || fail "backup with a closed friend and a drive"
grep -q "bob does not accept backups" backup.out || fail "a closed friend was not skipped"
grep -q "created on usb:" backup.out || fail "the drive should still get the backup"
[ "$(b destinations | grep alice)" = "$HELD" ] || fail "what a friend already kept here changed"
a status | grep -q "takes no backups from you" || fail "status does not say bob takes no backups"
b accept on >/dev/null
a backup --set default | grep -q "created on .*bob" || fail "bob did not get the backup after accepting again"

step "the recovery kit, and knowing whether there is one"
a status | grep -q "recovery kit: never exported" || fail "a kit that was never written should say so"
CAHOOTS_PASSPHRASE=correct-horse a recovery export "$T/kit.json" >/dev/null 2>&1 && fail "a short passphrase was accepted"
CAHOOTS_PASSPHRASE="my horse eats batteries" a recovery export "$T/kit.json" >/dev/null || fail "recovery export"
(mkdir "$T/cwd" && cd "$T/cwd" && CAHOOTS_PASSPHRASE="my horse eats batteries" a recovery export >/dev/null) || fail "export with no file named"
[ -f "$T/cwd/alice.cahoots-recovery-kit" ] || fail "the kit was not named for the computer"
a status | grep -q "recovery kit: exported .* up to date" || fail "status does not know the kit"
CAHOOTS_PASSPHRASE=wrong-horse-9 CAHOOTS_HOME=$T/again "$CAHOOTS" recovery import "$T/kit.json" >/dev/null 2>&1 &&
    fail "a wrong passphrase opened the kit"
CAHOOTS_PASSPHRASE="my horse eats batteries" CAHOOTS_HOME=$T/again "$CAHOOTS" recovery import "$T/kit.json" | grep -q 'Recovered "alice"' ||
    fail "recovery import"
[ -f "$T/again/identity.key" ] || fail "the recovered folder has no identity"
# The old computer is gone; the new one gets the files back from the friend alone.
a stop >/dev/null
r restore latest --set default --to recovered --from bob >/dev/null || fail "restore on the recovered computer"
diff -r recovered/data data || fail "the recovered computer restored something else"
r restore latest --set nosuchset --to recovered2 >/dev/null 2>&1 && fail "restore from a set that does not exist"
r stop >/dev/null

step "a copy of the kit kept up to date in a folder"
mkdir synced
CAHOOTS_PASSPHRASE=too-short a recovery copy-to synced >/dev/null 2>&1 && fail "a short passphrase was accepted for the copy"
CAHOOTS_PASSPHRASE="my horse eats batteries" a recovery copy-to synced >/dev/null || fail "starting the copy"
KIT=synced/alice.cahoots-recovery-kit
[ -f "$KIT" ] || fail "the copy is not named for the computer: $(ls synced)"
grep -q '"data"' "$KIT" || fail "the copy is not a kit"
grep -q "master" "$KIT" && fail "the copy is not sealed"
a status | grep -q "a copy is kept in .*synced" || fail "status does not mention the copy"
a name alice2 >/dev/null
a recovery copy-to --now >/dev/null || fail "writing the copy again"
[ -f synced/alice2.cahoots-recovery-kit ] || fail "the copy did not follow the new name"
[ ! -e "$KIT" ] || fail "the copy under the old name was left behind"
a name alice >/dev/null
a recovery copy-to --now >/dev/null
a recovery copy-to --now >/dev/null
[ "$(ls synced | wc -l | tr -d ' ')" = 2 ] || fail "expected the kit and the one before it, found: $(ls synced | tr '\n' ' ')"
[ -f "$KIT.before" ] || fail "the kit before this one was not kept"
CAHOOTS_PASSPHRASE="my horse eats batteries" CAHOOTS_HOME=$T/fromcopy "$CAHOOTS" recovery import "$KIT" | grep -q 'Recovered "alice"' ||
    fail "recovering from the copy"
# Bob, renamed to alice, shares the folder: his kit must not replace hers.
b name alice >/dev/null
CAHOOTS_PASSPHRASE="my horse eats batteries" b recovery copy-to synced >/dev/null || fail "a second computer of the same name"
ls synced/alice-*.cahoots-recovery-kit >/dev/null 2>&1 || fail "the second computer's kit has no name of its own: $(ls synced)"
CAHOOTS_PASSPHRASE="my horse eats batteries" CAHOOTS_HOME=$T/fromcopy2 "$CAHOOTS" recovery import "$KIT" >/dev/null || fail "her kit was replaced"
cmp -s "$T/fromcopy2/identity.key" "$T/alice/identity.key" || fail "her kit is no longer hers"
b recovery copy-to --off >/dev/null
rm -f synced/alice-*.cahoots-recovery-kit*
b name bob >/dev/null
rmdir_test=$T/synced.gone
mv synced "$rmdir_test"
a recovery copy-to --now >/dev/null 2>&1 && fail "a missing folder went unnoticed"
a status | grep -q "WARNING.*could not be copied" || fail "no warning about the failing copy"
mv "$rmdir_test" synced
a recovery copy-to --off | grep -q "no longer copied" || fail "stopping the copy"
a status | grep -q "copy is kept in" && fail "status still mentions the copy"

step "space for new friends and free space to keep"
b default-quota 250 >/dev/null
b keep-free 3 >/dev/null
grep -q "default_quota_gb = 250" "$T/bob/config.toml" || fail "the quota for new friends was not saved"
grep -q "min_free_gb = 3" "$T/bob/config.toml" || fail "the free space to keep was not saved"

step "verify one set"
a verify --set pics | grep -q "usb: " || fail "verify of one set"
a verify --set pics | grep -q "bob" && fail "a set that is not sent to bob was checked there"
a verify --set nosuchset >/dev/null 2>&1 && fail "verify accepted a set that does not exist"

step "prune and local fingerprint check"
a keep --last 1 --hourly 0 --daily 0 --weekly 0 --monthly 0 >/dev/null
a prune | grep -q "restore points kept" || fail "prune"
a snapshots | grep -q second || fail "a tagged restore point was pruned"
a verify --local | grep -q "all match" || fail "local check"
step "restore does not write through planted links"
mkdir -p victim trap1 trap2/data/sub
ln -s "$T/victim" trap1/data
a restore latest --to trap1 >trap.out 2>&1 && fail "restore went through a linked folder"
[ -z "$(ls victim)" ] || fail "restore wrote into the folder a link pointed at"
echo untouched >victimfile
ln -s "$T/victimfile" trap2/data/sub/note.txt.sdb-tmp
a restore latest data/sub --to trap2 >/dev/null || fail "restore beside a planted link"
[ "$(cat victimfile)" = untouched ] || fail "restore wrote through a planted link"
[ ! -L trap2/data/sub/note.txt ] || fail "the planted link became the restored file"

step "set-id bits are not restored"
echo three >data/sub/note.txt
# Windows has no such bits.
if [ -z "$WINDOWS" ]; then
    chmod 4755 data/sub/note.txt
    a backup --set default >/dev/null
    a restore latest data/sub/note.txt --to suid >/dev/null
    [ -u suid/data/sub/note.txt ] && fail "a set-uid file was restored set-uid"
    [ -x suid/data/sub/note.txt ] || fail "ordinary permissions were lost"
    chmod 644 data/sub/note.txt
else
    a backup --set default >/dev/null
fi

step "a forgotten restore point cannot be brought back"
GONE=$(a snapshots | awk '/ pics / {id=$1} END {print id}')
KEPT=$(find drive/sdb-store -path "*snapshots/$GONE")
cp "$KEPT" kept-copy
a forget "$GONE" >/dev/null
cp kept-copy "$KEPT"
a snapshots | grep -q "$GONE" && fail "a destination resurrected a forgotten restore point"
[ ! -e "$KEPT" ] || fail "the resurrected restore point was not deleted again"

step "a different disk at the drive's path is refused"
mv drive drive.real
mkdir -p drive/sdb-store
a backup --set pics >impostor.out 2>&1 && fail "a backup went to an impostor drive"
grep -q "not the one that was added" impostor.out || fail "impostor drive not reported"
rm -rf drive
mv drive.real drive
echo pic2 >photos/p2.jpg
a backup --set pics | grep -q "created on usb" || fail "the real drive stopped working"

step "removing a set's only destination does not widen it"
a destination rm usb | grep -q "now has no destination" || fail "no notice about the stranded set"
a backup --set pics >stranded.out 2>&1 && fail "a set went somewhere after its only destination was removed"
a set pics --to bob >/dev/null
a backup --set pics | grep -q "created on bob" || fail "choosing a new destination"

step "taking a destination off a set, then cleaning it up"
a drive usb drive >/dev/null
mkdir -p elsewhere
head -c 600000 /dev/urandom >elsewhere/only-here
a add elsewhere --set moved >/dev/null
a backup --set moved | grep -q "created on" || fail "backup of the set that will move"
a backup --set default >/dev/null
# The drive still holds what the pics set left when it moved to bob, earlier in this test.
a destination clean usb | grep -q "set pics is no longer sent to usb" || fail "leftovers of an earlier move were not found"
a destination clean usb | grep -q "nothing to clean" || fail "clean removed something from sets still sent there"
ONUSB=$(find drive/sdb-store -path '*chunks*' -type f | wc -l)
a set moved --to bob >/dev/null
a destination clean usb --dry-run | grep -q "would remove 1 restore points" || fail "dry run of clean"
a snapshots | grep " moved " | grep -q usb || fail "a dry run removed something"
a destination clean usb | grep -q "removed 1 restore points" || fail "clean"
a snapshots | grep " moved " | grep -q usb && fail "the set's restore point is still on the drive"
a snapshots | grep " moved " | grep -q bob || fail "clean removed the set from where it still goes"
[ "$(find drive/sdb-store -path '*chunks*' -type f | wc -l)" -lt "$ONUSB" ] || fail "clean freed no chunks"
a verify --full | grep -q "all good" || fail "clean removed chunks another set still needs"
a restore latest --to afterclean --from usb >/dev/null || fail "the other set no longer restores from the drive"
a backup --set moved | grep -q "no changes" || fail "the cleaned restore point came back"
a destination rm usb >/dev/null

step "exclusion lists"
mkdir -p data/proj/node_modules data/proj/target data/keepout
echo x >data/proj/node_modules/a.js
echo y >data/proj/target/b.o
echo z >data/keepout/c.txt
echo s >data/proj/main.rs
a backup --set default >/dev/null
a ls latest >ls.out
grep -q node_modules ls.out && fail "node_modules was backed up under the Standard list"
grep -q "target/b.o" ls.out || fail "the Standard list left out a build folder"
a set default --exclude developer | grep -q '"Developer"' || fail "choosing a list"
a backup --set default >/dev/null
a ls latest | grep -q "proj/target" && fail "the Developer list did not leave out target"
a exclude add Standard '*.o' >/dev/null 2>&1 && fail "a built-in list was changed"
a exclude new mine --from Developer >/dev/null
a exclude add mine keepout | grep -q '\*\*/keepout added' || fail "a bare name should mean that name anywhere"
a exclude add mine '[' >/dev/null 2>&1 && fail "a broken pattern was accepted"
a set default --exclude mine >/dev/null
a skip data/sub >/dev/null
a backup --set default >/dev/null
a ls latest >ls.out
grep -q keepout ls.out && fail "a pattern in my own list was ignored"
grep -q "data/sub" ls.out && fail "a folder left out by hand was backed up"
grep -q "proj/main.rs" ls.out || fail "too much was left out"
a skip data/sub --remove >/dev/null
a exclude delete mine | grep -q "now uses Standard" || fail "deleting a list in use"
a backup --set default >/dev/null
a ls latest >ls.out
grep -q "target/b.o" ls.out || fail "the set did not go back to the Standard list"
grep -q "sub/note.txt" ls.out || fail "a folder put back was not backed up"

step "taking what is now left out from older restore points"
a keep --last 24 >/dev/null
a backup --set default >/dev/null
OLD=$(a snapshots | awk '/ default / {id=$1} END {print id}')
a ls "$OLD" | grep -q "target/b.o" || fail "the older restore point should hold the build folder"
a set default --exclude Developer >/dev/null
a prune --set default >/dev/null 2>&1 && fail "--set was accepted without --apply-exclusions"
a prune --apply-exclusions --set default --dry-run >dry.out
grep -q "would be taken out of" dry.out || fail "dry run of apply-exclusions"
grep -q "bob: would free roughly .* of the .* chunks it holds" dry.out || fail "the dry run does not say what would be freed where"
a ls "$OLD" | grep -q "target/b.o" || fail "a dry run changed a restore point"
a prune --apply-exclusions --set default >applied.out
grep -q "set default: .* taken out of" applied.out || fail "apply-exclusions"
grep -q "bob: freed .* chunks)" applied.out || fail "no word on what was freed where: $(cat applied.out)"
a ls "$OLD" >ls.out
grep -q "proj/target" ls.out && fail "an older restore point still holds what is now left out"
grep -q "proj/main.rs" ls.out || fail "apply-exclusions took out too much"
a restore "$OLD" --to thinned >/dev/null || fail "a thinned restore point no longer restores"
[ ! -e thinned/data/proj/target ] || fail "the left-out folder was restored"
[ -f thinned/data/proj/main.rs ] || fail "a kept file was not restored"
a verify --full | grep -q "all good" || fail "apply-exclusions removed chunks still needed"
a prune --apply-exclusions | grep -q "no older restore point holds anything" || fail "a second pass found more to take out"
a set default --exclude Standard >/dev/null

step "files put back as they were still get a new restore point"
echo v1 >data/flip
a backup --set default >/dev/null
cp -p data/flip flip.saved
echo v2 >data/flip
a backup --set default >/dev/null
cp -p flip.saved data/flip
a backup --set default | grep -q "created on" || fail "going back to an earlier state made no restore point"
a restore latest data/flip --to flipped >/dev/null
[ "$(cat flipped/data/flip)" = v1 ] || fail "latest does not hold the files as they are now"
echo "end-to-end test passed"
