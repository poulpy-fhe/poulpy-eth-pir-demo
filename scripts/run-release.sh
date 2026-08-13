#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ETH_RPC_URL=${ETH_RPC_URL:-https://rpc.ankr.com/eth}
export ETH_RPC_URL

cd "$ROOT"

if [[ "${USDT_PIR_SKIP_BUILD:-0}" != "1" ]]; then
  RUSTFLAGS="${RUSTFLAGS:--C target-feature=+avx2,+fma}" \
    cargo build --release --features avx2-fhe -p usdt-pir
fi

exec ./target/release/usdt-pir "$@"
