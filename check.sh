#!/bin/sh
# The checks. `./check.sh quick` is what the pre-commit hook runs: formatting, lints and
# unit tests. `./check.sh` adds the end-to-end, damage and cloud tests, as GitHub does on every push.
set -e
cd "$(dirname "$0")"
echo "== format" && cargo fmt --check
echo "== lints" && cargo clippy --release --all-targets --features windowed --quiet -- -D warnings
echo "== unit tests"
# Quiet when they pass; when one fails, all it said, so the reason is there to read.
if tests=$(cargo test --release --quiet 2>&1); then
    echo "$tests" | grep -E "^test result"
else
    echo "$tests"
    exit 1
fi
if [ "${1:-}" = quick ]; then
    echo "quick checks passed (GitHub runs the end-to-end, damage and cloud tests)"
    exit 0
fi
echo "== build" && cargo build --release --quiet
echo "== end to end" && sh tests/e2e.sh target/release/cahoots
echo "== damage and failures" && sh tests/damage.sh target/release/cahoots
echo "== cloud storage" && CAHOOTS_TEST_S3=mini sh tests/cloud.sh target/release/cahoots
echo "all checks passed"
