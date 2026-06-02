#!/usr/bin/env bash
# Always run in release (the engine relies on optimization: in debug, blame and
# other gix operations are ~65x slower).
# Usage:  ./run.sh [repo-path] [commit-limit]
set -e
cd "$(dirname "$0")"
REPO="${1:-$PWD}"
LIMIT="${2:-50000}"
cargo build --release --bin diff
exec ./target/release/diff "$REPO" "$LIMIT"
