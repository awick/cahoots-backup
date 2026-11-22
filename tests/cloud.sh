#!/bin/sh
# Cloud destination test against a local stand-in for S3. Two can play that part:
#   mini      tests/mini-aws.pl, a small S3 server in Perl. Needs nothing installed, checks
#             no signatures and speaks plain http. This is what check.sh runs.
#   emulator  AWS Emulator, AWS's own stand-in: https with its own certificate, and
#             signatures are checked. Needs the AWS CLI and the `aws-emulator` program
#             (Linux) or Docker (macOS; the image AWS documents is built here). GitHub
#             runs this one too.
# Usage: [CAHOOTS_TEST_S3=mini|emulator] tests/cloud.sh path/to/cahoots
# With nothing chosen, the emulator is used where it can run.
set -eu
CAHOOTS=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
HERE=$(cd "$(dirname "$0")" && pwd)
T=$(cd "$(mktemp -d /tmp/cahoots-cloud.XXXXXX)" && pwd -P)
PORT=${CAHOOTS_TEST_S3_PORT:-19391}
export CAHOOTS_OFFLINE=1 CAHOOTS_HOME=$T/home
fail() { echo "FAILED: $1" >&2; exit 1; }
step() { echo "  $1"; }
s() { "$CAHOOTS" "$@"; }
EMU=
MINI=
cleanup() {
    s stop >/dev/null 2>&1 || true
    [ -z "$EMU" ] || kill "$EMU" 2>/dev/null || true
    [ -z "$MINI" ] || { kill "$MINI" && wait "$MINI"; } 2>/dev/null || true
    [ "$MODE" != emulator ] || docker rm -f "cahoots-cloud-test-$$" >/dev/null 2>&1 || true
    rm -rf "$T"
}
trap cleanup EXIT

MODE=${CAHOOTS_TEST_S3:-}
if [ -z "$MODE" ]; then
    MODE=mini
    if command -v aws >/dev/null 2>&1 && { command -v aws-emulator || command -v docker; } >/dev/null 2>&1; then
        MODE=emulator
    fi
fi
# The small stand-in also serves the part of the test that fills a 1 GB cap: the emulator
# holds at most 64 MB in a bucket. It hands out listings five names at a time, which gives
# the paging a workout as well.
CAPURL=http://127.0.0.1:$((PORT + 1))
perl "$HERE/mini-aws.pl" "$((PORT + 1))" >"$T/mini.log" 2>&1 &
MINI=$!
# Everything a bucket of the small stand-in holds, as "size<TAB>name" lines.
mini_list() {
    curl -s "$1/$2?list-type=2&max-keys=10000000" | awk '
        /<Key>/ { k = $0; gsub(/.*<Key>|<\/Key>.*/, "", k) }
        /<Size>/ { n = $0; gsub(/.*<Size>|<\/Size>.*/, "", n); print n "\t" k }'
}

if [ "$MODE" = mini ]; then
    ENDPOINT=$CAPURL
    KEY=sdbtestkey
    CAOPT=
    export CAHOOTS_CLOUD_SECRET=sdbtestsecret
    listing() { mini_list "$ENDPOINT" sdbtest; }
    remove() { curl -s -o /dev/null -X DELETE "$ENDPOINT/sdbtest/$1"; }
    dump() {
        mkdir -p "$1"
        listing | cut -f2 | while read -r k; do curl -s "$ENDPOINT/sdbtest/$k" >>"$1/all"; done
    }
else
    command -v aws >/dev/null 2>&1 || fail "the emulator test needs the AWS CLI"
    # The emulator keeps its certificate and keys under its home folder: give it one of its own.
    mkdir -p "$T/emu/.aws/emulator"
    if command -v aws-emulator >/dev/null 2>&1; then
        HOME=$T/emu aws-emulator -p "$PORT" --telemetry off >"$T/emu.log" 2>&1 &
        EMU=$!
    elif command -v docker >/dev/null 2>&1; then
        if ! docker image inspect aws-emulator >/dev/null 2>&1; then
            case $(uname -m) in arm64 | aarch64) PKG=aws-emulator-aarch64.rpm ;; *) PKG=aws-emulator-x86_64.rpm ;; esac
            mkdir "$T/image"
            # The image AWS documents: their package on Amazon Linux.
            cat >"$T/image/Dockerfile" <<EOF
FROM public.ecr.aws/amazonlinux/amazonlinux:2023-minimal
ADD https://downloads.aws.com/aws-emulator/packages/aws-emulator/LTS/latest/$PKG /tmp/aws-emulator.rpm
RUN rpm -i /tmp/aws-emulator.rpm && rm /tmp/aws-emulator.rpm
ENV EMULATOR_BIND_ADDRESS=0.0.0.0
ENTRYPOINT ["aws-emulator"]
EOF
            docker build -q -t aws-emulator "$T/image" >/dev/null
        fi
        docker run -d --rm --name "cahoots-cloud-test-$$" --memory=2g --user "$(id -u):$(id -g)" \
            -p "127.0.0.1:$PORT:$PORT" -e HOME="$T/emu" -v "$T/emu/.aws/emulator:$T/emu/.aws/emulator" \
            aws-emulator -p "$PORT" --telemetry off >/dev/null
    else
        fail "the emulator test needs the aws-emulator program or Docker"
    fi
    E=$T/emu/.aws/emulator/$PORT
    CA=$E/ca.pem
    CAOPT="--ca-file $CA"
    ENDPOINT=https://127.0.0.1:$PORT
    for _ in $(seq 1 60); do
        [ -f "$CA" ] && curl -s -o /dev/null --cacert "$CA" "$ENDPOINT/__admin/health" && break
        sleep 0.5
    done
    [ -f "$E/emulator_credentials" ] || fail "the emulator did not start"
    # The emulator makes up its own keys at every start.
    KEY=$(sed -n 's/^aws_access_key_id *= *//p' "$E/emulator_credentials")
    CAHOOTS_CLOUD_SECRET=$(sed -n 's/^aws_secret_access_key *= *//p' "$E/emulator_credentials")
    export CAHOOTS_CLOUD_SECRET AWS_CONFIG_FILE=$E/config AWS_SHARED_CREDENTIALS_FILE=$E/emulator_credentials
    listing() { aws s3api list-objects-v2 --bucket sdbtest --output text --query 'Contents[].[Size,Key]'; }
    remove() { aws s3api delete-object --bucket sdbtest --key "$1" >/dev/null; }
    dump() { aws s3 sync s3://sdbtest "$1" --quiet; }
fi
for _ in $(seq 1 40); do
    curl -s -o /dev/null "$CAPURL/" && break
    sleep 0.25
done
objects() { listing | wc -l | tr -d ' '; }
# $CAOPT is one option and its value, or nothing.
add_cloud() { s cloud s3 --endpoint "$ENDPOINT" --bucket sdbtest --access-key "$KEY" $CAOPT "$@"; }
echo "  (against the $MODE stand-in)"

cd "$T"
mkdir -p data/sub drive
s init --name alice >/dev/null
s pause >/dev/null

if [ "$MODE" = emulator ]; then
    step "bad keys and an unknown certificate are refused"
    env CAHOOTS_CLOUD_SECRET=wrong "$CAHOOTS" cloud s3 --endpoint "$ENDPOINT" --bucket sdbtest --access-key "$KEY" $CAOPT --create \
        >bad.out 2>&1 && fail "a wrong secret key was accepted"
    grep -q "check the access key" bad.out || fail "no hint about the keys"
    s cloud s3 --endpoint "$ENDPOINT" --bucket sdbtest --access-key "$KEY" --create >nocert.out 2>&1 &&
        fail "a certificate nobody vouched for was accepted"
fi

step "a broken certificate file and plain http elsewhere are refused"
echo "not a certificate" >junk.pem
s cloud s3 --endpoint "$ENDPOINT" --bucket sdbtest --access-key "$KEY" --ca-file junk.pem >junk.out 2>&1 &&
    fail "a CA file with no certificate in it was accepted"
s cloud s3 --endpoint http://192.0.2.1:9000 --bucket sdbtest --access-key "$KEY" >http.out 2>&1 && fail "plain http was accepted"
grep -q "https://" http.out || fail "no hint about https"
s destinations | grep -q s3 && fail "a destination that failed its check was saved"

step "add cloud storage"
add_cloud --create | grep -q "s3 added" || fail "add"
s destinations | grep -q "s3 .*cloud storage, bucket sdbtest" || fail "not listed"
[ "$(ls -l home/config.toml | cut -c1-10)" = "-rw-------" ] || fail "config is not private"
s status | grep -q "$CAHOOTS_CLOUD_SECRET" && fail "status shows the secret key"

step "backup, change, backup"
head -c 3000000 /dev/urandom >data/big
echo one >data/sub/note.txt
echo PLAINTEXT-MARKER-4711 >data/sub/marker.txt
s add data >/dev/null
s backup | grep -q "restore point .* created on s3" || fail "first backup"
s backup | grep -q "no changes" || fail "an unchanged tree made a new restore point"
echo two >data/sub/note.txt
s backup --tag second | grep -q "created on s3" || fail "second backup"
dump "$T/dump"
[ -n "$(find dump -type f -size +100k)" ] || fail "nothing was stored in the bucket"
grep -rq PLAINTEXT-MARKER-4711 dump && fail "plain text reached the bucket"
rm -rf dump

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
VICTIM=$(listing | grep /chunks/ | sort -n | tail -1 | cut -f2)
[ -n "$VICTIM" ] || fail "no chunk found in the bucket"
remove "$VICTIM"
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
s destination backup-to s3 false >/dev/null
s cloud big --endpoint "$CAPURL" --bucket sdbcap --access-key x --create | grep -q "big added" || fail "adding the stand-in"
s destination quota big 1 >/dev/null
# Seven restore points of 140 MB leave it over nine tenths full; the eighth does not fit.
for n in 1 2 3 4 5 6 7 8; do
    head -c 140000000 /dev/urandom >data/big
    s backup >cap.out 2>&1 || fail "backup $n under the cap: $(cat cap.out)"
done
grep -q "at the cap you set" cap.out || fail "the cap was never reached"
USED=$(mini_list "$CAPURL" sdbcap | awk '{ n += $1 } END { print n + 0 }')
[ "$USED" -lt 1073741824 ] || fail "the bucket holds $USED bytes despite a 1 GB cap"
LEFT=$(s snapshots | grep -c " big")
# One old restore point makes enough room; anything more is history lost for nothing.
[ "$LEFT" = 7 ] || fail "reaching the cap should leave 7 restore points, left $LEFT"
s restore latest data/big --to capped >/dev/null
cmp capped/data/big data/big || fail "the newest restore point did not survive the cap"
s status | grep -q "big .*you use [0-9.]* MB" || fail "status does not show what is stored after the cap was applied"
s destination rm big >/dev/null
rm -f data/big

step "new keys replace the old ones"
s destination backup-to s3 true >/dev/null
s destination quota s3 1 >/dev/null
# No certificate is named this time: the one the storage was added with is kept.
s cloud s3 --endpoint "$ENDPOINT" --bucket sdbtest --access-key "$KEY" | grep -q "s3 added" || fail "re-adding"
s backup | grep -q "created on s3" || fail "the storage stopped working after its keys were given again"
s destinations | grep -q "capped at 1 GB" || fail "re-adding lost the destination's settings"
echo "cloud test passed"
