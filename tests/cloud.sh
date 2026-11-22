#!/bin/sh
# Cloud destination test against a local S3 emulator (MinIO). Not part of check.sh:
# it needs the `minio` program, or Docker to run it.
# Usage: tests/cloud.sh path/to/sdb
set -eu
SDB=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
T=$(cd "$(mktemp -d /tmp/sdb-cloud.XXXXXX)" && pwd -P)
PORT=${SDB_TEST_S3_PORT:-19391}
KEY=sdbtestkey
export SDB_OFFLINE=1 SDB_HOME=$T/home SDB_CLOUD_SECRET=sdbtestsecret$$
fail() { echo "FAILED: $1" >&2; exit 1; }
step() { echo "  $1"; }
s() { "$SDB" "$@"; }
MINIO=
cleanup() {
    s stop >/dev/null 2>&1 || true
    [ -z "$MINIO" ] || kill "$MINIO" 2>/dev/null || true
    docker rm -f "sdb-cloud-test-$$" >/dev/null 2>&1 || true
    rm -rf "$T"
}
trap cleanup EXIT

mkdir -p "$T/minio"
if command -v minio >/dev/null 2>&1; then
    MINIO_ROOT_USER=$KEY MINIO_ROOT_PASSWORD=$SDB_CLOUD_SECRET \
        minio server --address "127.0.0.1:$PORT" --console-address "127.0.0.1:$((PORT + 1))" "$T/minio" >"$T/minio.log" 2>&1 &
    MINIO=$!
elif command -v docker >/dev/null 2>&1; then
    docker run -d --rm --name "sdb-cloud-test-$$" -p "127.0.0.1:$PORT:9000" \
        -e MINIO_ROOT_USER=$KEY -e MINIO_ROOT_PASSWORD="$SDB_CLOUD_SECRET" minio/minio server /data >/dev/null
else
    echo "skipped: neither minio nor docker is installed"
    exit 0
fi
for _ in $(seq 1 40); do
    curl -s -o /dev/null "http://127.0.0.1:$PORT/minio/health/live" && break
    sleep 0.5
done
ENDPOINT=http://127.0.0.1:$PORT
objects() { find "$T/minio/sdbtest" -name xl.meta 2>/dev/null | wc -l | tr -d ' '; }

cd "$T"
mkdir -p data/sub drive
s init --name alice >/dev/null
s pause >/dev/null

step "bad keys and plain http elsewhere are refused"
env SDB_CLOUD_SECRET=wrong "$SDB" cloud s3 --endpoint "$ENDPOINT" --bucket sdbtest --access-key $KEY --path-style --create \
    >bad.out 2>&1 && fail "a wrong secret key was accepted"
grep -q "check the access key" bad.out || fail "no hint about the keys"
s cloud s3 --endpoint http://192.0.2.1:9000 --bucket sdbtest --access-key $KEY >http.out 2>&1 && fail "plain http was accepted"
grep -q "https://" http.out || fail "no hint about https"
s destinations | grep -q s3 && fail "a destination that failed its check was saved"

step "add cloud storage"
s cloud s3 --endpoint "$ENDPOINT" --bucket sdbtest --access-key $KEY --path-style --create | grep -q "s3 added" || fail "add"
s destinations | grep -q "s3 .*cloud storage, bucket sdbtest" || fail "not listed"
[ "$(stat -f %Lp home/config.toml 2>/dev/null || stat -c %a home/config.toml)" = 600 ] || fail "config is not private"
s status | grep -q "$SDB_CLOUD_SECRET" && fail "status shows the secret key"

step "backup, change, backup"
head -c 3000000 /dev/urandom >data/big
echo one >data/sub/note.txt
echo PLAINTEXT-MARKER-4711 >data/sub/marker.txt
s add data >/dev/null
s backup | grep -q "restore point .* created on s3" || fail "first backup"
s backup | grep -q "no changes" || fail "an unchanged tree made a new restore point"
echo two >data/sub/note.txt
s backup --tag second | grep -q "created on s3" || fail "second backup"
grep -rq PLAINTEXT-MARKER-4711 "$T/minio/sdbtest" && fail "plain text reached the bucket"

step "restore and verify"
s restore latest --to out >/dev/null
diff -r out/data data || fail "restored tree differs"
s verify --full | grep -q "all good" || fail "verify"

step "a restart finds everything again"
s stop >/dev/null
sleep 1
s snapshots | grep -q second || fail "restore points not found after a restart"
s backup | grep -q "no changes" || fail "known chunks were sent again after a restart"

step "damage in the bucket is detected and repaired from a drive"
s drive usb drive >/dev/null
s backup >/dev/null
VICTIM=$(du -sk "$T"/minio/sdbtest/*/chunks/* | sort -n | tail -1 | cut -f2)
[ -d "$VICTIM" ] || fail "no chunk found in the bucket"
rm -rf "$VICTIM"
s verify --full >verify.out 2>&1 || true
grep -q "repaired from another friend" verify.out || fail "a chunk missing from the bucket was not repaired"
s verify --full | grep -q "all good" || fail "verify after repair"

step "restore prefers the drive, and can be told to use the cloud"
s restore latest --to fromcloud --from s3 >/dev/null
diff -r fromcloud/data data || fail "restore from the cloud"

step "the cloud keeps its own, shorter history"
for n in 3 4 5; do
    echo "$n" >data/sub/note.txt
    s backup >/dev/null
done
ALL=$(s snapshots | grep -c " s3")
s destination keep s3 --last 1 --hourly 0 --daily 0 --weekly 0 --monthly 0 | grep -q "s3 keeps: last 1" || fail "keep"
s prune --dry-run | grep -q "would remove restore point .* from s3" || fail "dry run names nothing at s3"
[ "$(s snapshots | grep -c ' s3')" = "$ALL" ] || fail "a dry run removed something"
BEFORE=$(objects)
s prune | grep -q "restore points kept" || fail "prune"
# The newest and the tagged one stay; the drive keeps them all.
[ "$(s snapshots | grep -c ' s3')" = 2 ] || fail "the cloud should hold 2 restore points, holds $(s snapshots | grep -c ' s3')"
[ "$(s snapshots | grep -c 'usb')" -ge 4 ] || fail "the drive lost restore points to the cloud's rules"
[ "$(objects)" -lt "$BEFORE" ] || fail "pruning freed nothing in the bucket"
s snapshots | grep -q "second .*s3" || fail "a tagged restore point was pruned"
s backup | grep -q "no changes" || fail "pruned restore points came back"
s destination keep s3 --default | grep -q "each set's history rules" || fail "back to the default rules"

step "a cap removes only as many old restore points as it must"
s destination rm usb >/dev/null
s forget second >/dev/null
s destination quota s3 1 >/dev/null
# Seven restore points of 140 MB leave the bucket over nine tenths full; the eighth does not fit.
for n in 1 2 3 4 5 6 7 8; do
    head -c 140000000 /dev/urandom >data/big
    s backup >cap.out 2>&1 || fail "backup $n under the cap: $(cat cap.out)"
done
grep -q "at the cap you set" cap.out || fail "the cap was never reached"
USED=$(du -sk "$T/minio/sdbtest" | awk '{print $1}')
[ "$USED" -lt 1100000 ] || fail "the bucket holds ${USED} KB despite a 1 GB cap"
LEFT=$(s snapshots | grep -c " s3")
# The small old one and one large one make enough room; anything more is history lost for nothing.
[ "$LEFT" = 7 ] || fail "reaching the cap should leave 7 restore points, left $LEFT"
s restore latest data/big --to capped >/dev/null
cmp capped/data/big data/big || fail "the newest restore point did not survive the cap"
s status | grep -q "s3 .*you use [0-9.]* MB" || fail "status does not show what is stored after the cap was applied"

step "new keys replace the old ones"
s cloud s3 --endpoint "$ENDPOINT" --bucket sdbtest --access-key $KEY --path-style | grep -q "s3 added" || fail "re-adding"
s destinations | grep -q "capped at 1 GB" || fail "re-adding lost the destination's settings"
echo "cloud test passed"
