#!/bin/sh
# The checks. `./check.sh quick` is what the pre-commit hook runs: formatting, lints and
# unit tests. `./check.sh` adds the end-to-end, damage and cloud tests, as GitHub does on every push.
set -e
cd "$(dirname "$0")"
echo "== format" && cargo fmt --check
echo "== lints" && cargo clippy --release --all-targets --quiet -- -D warnings
echo "== unit tests" && cargo test --release --quiet 2>&1 | grep -E "^test result|FAILED|panicked|error" || true
cargo test --release --quiet >/dev/null 2>&1
if [ "${1:-}" = quick ]; then
    echo "quick checks passed (GitHub runs the end-to-end, damage and cloud tests)"
    exit 0
fi
echo "== build" && cargo build --release --quiet
echo "== end to end" && sh tests/e2e.sh target/release/sdb
echo "== damage and failures" && sh tests/damage.sh target/release/sdb
echo "== cloud storage" && SDB_TEST_S3=mini sh tests/cloud.sh target/release/sdb
echo "all checks passed"
