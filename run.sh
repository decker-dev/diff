#!/usr/bin/env bash
# Ejecuta rebased-rs SIEMPRE en release (el motor depende de optimización:
# en debug, blame y otras operaciones de gix son ~65x más lentas).
# Uso:  ./run.sh [ruta-repo] [limite-commits]
set -e
cd "$(dirname "$0")"
REPO="${1:-$PWD}"
LIMIT="${2:-50000}"
cargo build --release --bin rebased-rs
exec ./target/release/rebased-rs "$REPO" "$LIMIT"
