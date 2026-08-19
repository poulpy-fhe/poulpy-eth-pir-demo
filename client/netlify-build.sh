#!/usr/bin/env bash
# Netlify publishes the checked-in browser bundle. Building it on Netlify is not
# possible because the Rust workspace intentionally uses sibling path dependencies.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

required=(
  client/web/index.html
  client/web/pir.js
  client/web/usdt_pir_client.js
  client/web/usdt_pir_client_bg.wasm
)

for path in "${required[@]}"; do
  if [[ ! -s "$path" ]]; then
    echo "missing Netlify asset: $path" >&2
    echo "rebuild and commit the browser bundle with: ./client/build.sh web" >&2
    exit 1
  fi
done

magic=$(od -An -tx1 -N4 client/web/usdt_pir_client_bg.wasm | tr -d '[:space:]')
if [[ "$magic" != "0061736d" ]]; then
  echo "client/web/usdt_pir_client_bg.wasm is not a WebAssembly module" >&2
  exit 1
fi

echo "Netlify client bundle is ready ($(du -h client/web/usdt_pir_client_bg.wasm | cut -f1) WASM)"
