#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BACKEND_ADDR=${USDT_PIR_BACKEND_ADDR:-127.0.0.1:8787}
PORTAL_ADDR=${USDT_PIR_PORTAL_ADDR:-127.0.0.1:8080}
STATE=${USDT_PIR_STATE:-data/balances.snapshot}
KEYWORD=${USDT_PIR_KEYWORD:-data/keyword}
CONFIRMATIONS=${USDT_PIR_CONFIRMATIONS:-4}
REBUILD_EVERY=${USDT_PIR_REBUILD_EVERY:-30}
COMPACT_TAIL_PERCENT=${USDT_PIR_COMPACT_TAIL_PERCENT:-100}
CHUNK=${USDT_PIR_CHUNK:-25}
ETH_RPC_URL=${ETH_RPC_URL:-https://rpc.ankr.com/eth}
export ETH_RPC_URL

cd "$ROOT"

if [[ ! -e "$STATE" ]]; then
  echo "required complete snapshot is missing: $STATE" >&2
  echo "run 'usdt-pir bootstrap --state $STATE' with an archive mainnet RPC first" >&2
  exit 1
fi

if [[ "${USDT_PIR_SKIP_BUILD:-0}" != "1" ]]; then
  RUSTFLAGS="${RUSTFLAGS:--C target-feature=+avx2,+fma}" \
    cargo build --release --features avx2-fhe -p usdt-pir
  ./client/build.sh web
fi

backend_args=(
  serve
  --state "$STATE"
  --keyword "$KEYWORD"
  --listen "$BACKEND_ADDR"
  --confirmations "$CONFIRMATIONS"
  --rebuild-every "$REBUILD_EVERY"
  --compact-tail-percent "$COMPACT_TAIL_PERCENT"
  --chunk "$CHUNK"
)

cleanup() {
  jobs -pr | xargs -r kill
}
trap cleanup EXIT INT TERM

echo "Starting backend on http://$BACKEND_ADDR"
./target/release/usdt-pir "${backend_args[@]}" &

echo "Starting local portal on http://$PORTAL_ADDR"
python3 scripts/local_portal.py \
  --listen "$PORTAL_ADDR" \
  --backend "http://$BACKEND_ADDR" \
  --web client/web &

echo
echo "Open http://$PORTAL_ADDR"
echo "Press Ctrl-C to stop both processes."
wait -n
