#!/bin/sh
# Everything that must pass before a commit: formatting, lints, unit tests, end-to-end and damage tests.
set -e
cd "$(dirname "$0")"
echo "== format" && cargo fmt --check
echo "== lints" && cargo clippy --release --all-targets --quiet -- -D warnings
echo "== unit tests" && cargo test --release --quiet 2>&1 | grep -E "^test result|FAILED|panicked|error" || true
cargo test --release --quiet >/dev/null 2>&1
echo "== build" && cargo build --release --quiet
echo "== end to end" && sh tests/e2e.sh target/release/sdb
echo "== damage and failures" && sh tests/damage.sh target/release/sdb
echo "all checks passed"
