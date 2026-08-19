#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: ./scripts/run-ec2-demo.sh <ETH_MAINNET_RPC_URL> [CONFIRMATIONS]

On Debian/Ubuntu, builds the AVX-512 + CBLAS server when AVX-512F is available,
otherwise the AVX2 + CBLAS server (AVX2 and FMA are required). NUMA
interleaving is disabled. If USDT_PIR_STATE exists, the launcher resumes it.
Otherwise it starts an empty, explicitly partial holder map about 25 blocks
behind the current head, as the launcher did before bootstrap support.

Optional environment variables:
  USDT_PIR_STATE=PATH         Default: data/ec2-demo.snapshot
  USDT_PIR_KEYWORD=PATH       Default: data/ec2-demo-keyword
  USDT_PIR_BACKEND_ADDR=ADDR  Default: 127.0.0.1:8787
  USDT_PIR_POLL_INTERVAL=N    Default: 4 seconds
  USDT_PIR_REBUILD_EVERY=N    Default: 30 seconds
  USDT_PIR_BATCH_WINDOW=N     Default: 0 ms (lowest single-query latency)
  PIR_THREADS=N               Override the script's derived physical-core count.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi
if (( $# < 1 || $# > 2 )); then
  usage >&2
  exit 2
fi

RPC_URL=$1
CONFIRMATIONS=${2:-${USDT_PIR_CONFIRMATIONS:-4}}
if [[ ! "$CONFIRMATIONS" =~ ^[1-9][0-9]*$ ]]; then
  echo "confirmations must be a positive integer, got: $CONFIRMATIONS" >&2
  exit 2
fi

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

STATE=${USDT_PIR_STATE:-data/ec2-demo.snapshot}
KEYWORD=${USDT_PIR_KEYWORD:-data/ec2-demo-keyword}
LISTEN=${USDT_PIR_BACKEND_ADDR:-127.0.0.1:8787}
POLL_INTERVAL=${USDT_PIR_POLL_INTERVAL:-4}
REBUILD_EVERY=${USDT_PIR_REBUILD_EVERY:-30}
BATCH_WINDOW=${USDT_PIR_BATCH_WINDOW:-0}

export ETH_RPC_URL="$RPC_URL"
export OPENBLAS_NUM_THREADS=1
export RUST_LOG="${RUST_LOG:-usdt_pir=info}"

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  echo "this launcher requires Linux x86_64" >&2
  exit 1
fi
if ! command -v apt-get >/dev/null || ! command -v dpkg-query >/dev/null; then
  echo "this launcher currently expects Debian/Ubuntu (apt-get and dpkg-query)" >&2
  exit 1
fi
if [[ ! -f ../eth-pir/Cargo.toml || ! -f ../poulpy-pir/Cargo.toml \
      || ! -f ../poulpy-pir/vendor/ptr_hash/Cargo.toml ]]; then
  echo "required sibling checkouts are missing: ../eth-pir and the project's" >&2
  echo "modified ../poulpy-pir (including vendor/ptr_hash) must be present" >&2
  exit 1
fi

CPU_FLAGS=$(awk '/^flags[[:space:]]*:/ { print; exit }' /proc/cpuinfo)
if [[ -z "$CPU_FLAGS" ]]; then
  echo "could not read CPU feature flags from /proc/cpuinfo" >&2
  exit 1
fi

cpu_has() {
  [[ " $CPU_FLAGS " == *" $1 "* ]]
}

missing_features=()
for feature in avx2 fma; do
  if ! cpu_has "$feature"; then
    missing_features+=("$feature")
  fi
done
if (( ${#missing_features[@]} > 0 )); then
  echo "the optimized server requires AVX2 and FMA CPU support" >&2
  echo "missing CPU features: ${missing_features[*]}" >&2
  exit 1
fi

if cpu_has avx512f; then
  SIMD_BACKEND="AVX-512"
  FHE_FEATURE="avx512-fhe"
  TARGET_FEATURES="+avx2,+fma,+avx512f"
else
  SIMD_BACKEND="AVX2"
  FHE_FEATURE="avx2-fhe"
  TARGET_FEATURES="+avx2,+fma"
fi

rpc_u64() {
  python3 - "$RPC_URL" "$1" <<'PY'
import json
import sys
import urllib.request

url, method = sys.argv[1:]
body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": []}).encode()
request = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
try:
    with urllib.request.urlopen(request, timeout=30) as response:
        result = json.load(response)
except Exception as error:
    raise SystemExit(f"RPC {method} failed ({type(error).__name__})")
if result.get("error"):
    code = result["error"].get("code", "unknown") if isinstance(result["error"], dict) else "unknown"
    raise SystemExit(f"RPC {method} returned error code {code}")
try:
    print(int(result["result"], 16))
except (KeyError, TypeError, ValueError) as error:
    raise SystemExit(f"RPC {method} returned an invalid result") from error
PY
}

CHAIN_ID=$(rpc_u64 eth_chainId)
if [[ "$CHAIN_ID" != "1" ]]; then
  echo "RPC must be Ethereum mainnet (chain ID 1), got: $CHAIN_ID" >&2
  exit 1
fi

if ! dpkg-query -W -f='${Status}' libopenblas-pthread-dev 2>/dev/null \
    | grep -q 'ok installed'; then
  if (( EUID == 0 )); then
    apt-get update
    apt-get install -y libopenblas-pthread-dev
  else
    sudo apt-get update
    sudo apt-get install -y libopenblas-pthread-dev
  fi
fi

echo "Building $SIMD_BACKEND + CBLAS server (NUMA DB interleaving disabled)..."
RUSTFLAGS="-C target-cpu=native -C target-feature=$TARGET_FEATURES" \
  cargo build --release --locked --no-default-features \
    --features "$FHE_FEATURE,eth-pir/cblas-gemm" \
    -p usdt-pir

# Poulpy otherwise uses the detected logical-CPU count. SMT measured worse for
# online PIR and grows its scratch pool, so default to one worker per core.
if [[ -z "${PIR_THREADS:-}" ]]; then
  if command -v lscpu >/dev/null; then
    PIR_THREADS=$(lscpu -p=CORE,SOCKET | grep -v '^#' | sort -u | awk 'END {print NR}')
  else
    PIR_THREADS=$(nproc)
  fi
fi
if [[ ! "$PIR_THREADS" =~ ^[1-9][0-9]*$ ]]; then
  echo "PIR_THREADS must be a positive integer, got: $PIR_THREADS" >&2
  exit 1
fi
export PIR_THREADS

from_args=()
if [[ -e "$STATE" ]]; then
  echo "Resuming existing state: $STATE"
else
  mapfile -t keyword_paths < <(
    python3 - "$KEYWORD" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
print(path.with_suffix(".index"))
print(path.with_suffix(".keys"))
PY
  )
  if [[ -e "${keyword_paths[0]}" || -e "${keyword_paths[1]}" ]]; then
    echo "state $STATE is new, but keyword files already exist:" >&2
    echo "  ${keyword_paths[0]}" >&2
    echo "  ${keyword_paths[1]}" >&2
    echo "choose a fresh USDT_PIR_KEYWORD path or remove both files intentionally" >&2
    exit 1
  fi

  # Ethereum slots are 12 seconds, so 25 blocks is approximately five minutes.
  HEAD=$(rpc_u64 eth_blockNumber)
  FROM_BLOCK=$((HEAD - 25))
  from_args=(--from-block "$FROM_BLOCK")
  echo "No state found at: $STATE"
  echo "Head block: $HEAD"
  echo "Starting fresh from block: $FROM_BLOCK"
  echo "WARNING: this is an empty, partial holder map; it learns only addresses"
  echo "that move from this block onward, not every existing USDT/USDC holder."
fi

echo "Confirmations: $CONFIRMATIONS"
echo "Listening on: http://$LISTEN"
echo "PIR threads: ${PIR_THREADS:-auto (physical cores)}"

exec ./target/release/usdt-pir serve \
  --state "$STATE" \
  --keyword "$KEYWORD" \
  --listen "$LISTEN" \
  --confirmations "$CONFIRMATIONS" \
  --reorg-window 64 \
  --poll-interval "$POLL_INTERVAL" \
  --chunk 25 \
  --rebuild-every "$REBUILD_EVERY" \
  --batch-window "$BATCH_WINDOW" \
  "${from_args[@]}"
