#!/bin/sh
# Checks a built Docker image: the daemon sets itself up, answers commands, keeps its
# identity across a restart and stops cleanly.
# Usage: tests/docker.sh IMAGE
set -eu
IMAGE=$1
NAME=cahoots-image-test-$$
CONFIG=cahoots-image-test-config-$$
BACKUPS=cahoots-image-test-backups-$$
fail() { echo "FAILED: $1" >&2; docker logs "$NAME" 2>&1 | tail -20 >&2; exit 1; }
step() { echo "  $1"; }
x() { docker exec "$NAME" cahoots "$@"; }
cleanup() {
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker volume rm "$CONFIG" "$BACKUPS" >/dev/null 2>&1 || true
}
trap cleanup EXIT
start() {
    docker run -d --name "$NAME" -e CAHOOTS_OFFLINE=1 -e CAHOOTS_NAME=boxed -e CAHOOTS_PORT=45111 -v "$CONFIG:/config" \
        -v "$BACKUPS:/backups" "$IMAGE" >/dev/null
    for _ in $(seq 1 40); do
        x status >/dev/null 2>&1 && return
        sleep 0.5
    done
    fail "the daemon did not come up"
}

step "first start sets the node up"
start
x status >status.out 2>&1 || fail "status"
grep -q "^name: *boxed" status.out || fail "CAHOOTS_NAME was not used"
grep -q "daemon: *running" status.out || fail "the daemon is not running"
grep -q "the container runs the daemon" status.out || fail "status does not know it is in a container"
grep -q "service install" status.out && fail "status suggests a login service inside a container"
x service install >/dev/null 2>&1 && fail "a login service was installed inside a container"
x status >/dev/null 2>&1 || fail "refusing the service stopped the daemon"
x status | grep -q ":45111" || fail "CAHOOTS_PORT was not used"
[ "$(docker exec "$NAME" id -u)" != 0 ] || fail "the daemon runs as root"
ID=$(x status | awk '/node id/ {print $3}')
docker exec "$NAME" grep -q 'store_dir = "/backups"' /config/config.toml || fail "friends' backups are not kept in /backups"
docker exec "$NAME" test -f /config/identity.key || fail "the identity is not kept in /config"
docker exec "$NAME" sh -c 'touch /backups/.w && rm /backups/.w' || fail "the daemon cannot write to /backups"

step "commands work through docker exec"
x invite | grep -q "cahoots1" || fail "invite"
docker exec "$NAME" sh -c 'mkdir -p /tmp/files && echo hello >/tmp/files/a.txt'
x add /tmp/files >/dev/null || fail "add"
x gui >gui.out 2>&1 && fail "the image should not carry the window"

step "a new container on the same volumes is the same node"
docker rm -f "$NAME" >/dev/null
start
[ "$(x status | awk '/node id/ {print $3}')" = "$ID" ] || fail "the identity did not survive"
x sets | grep -q "/tmp/files" || fail "the settings did not survive"

step "docker stop ends it promptly"
BEGAN=$(date +%s)
docker stop "$NAME" >/dev/null
[ $(($(date +%s) - BEGAN)) -lt 8 ] || fail "the daemon ignored the stop signal"
rm -f status.out gui.out
echo "docker image test passed"
