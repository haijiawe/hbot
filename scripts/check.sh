#!/usr/bin/env bash
# 一条命令的本地闸门。CI 跑同一条。
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== fmt =="
cargo fmt --all -- --check

echo "== clippy =="
cargo clippy --workspace --all-targets -- -D warnings

echo "== test =="
cargo test --workspace

echo "== deps direction =="
cargo run -q -p xtask -- deps

if [ -d apps/web/node_modules ]; then
  echo "== web =="
  (cd apps/web && pnpm run check)
else
  echo "== web skipped (no node_modules) =="
fi

echo "OK"
