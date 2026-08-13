#!/usr/bin/env bash
# Builds the wasm client. Default target is the browser bundle in client/web;
# pass `nodejs` for a Node bundle in client/pkg-node.
set -euo pipefail

target="${1:-web}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

case "$target" in
  web)    out="client/web" ;;
  nodejs) out="client/pkg-node" ;;
  *) echo "usage: $0 [web|nodejs]" >&2; exit 2 ;;
esac

want=$(sed -n 's/^wasm-bindgen = "\(.*\)"/\1/p' client/Cargo.toml | head -1)
have=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)
if [[ -z "$have" ]]; then
  echo "wasm-bindgen-cli not found: cargo install wasm-bindgen-cli --version $want" >&2
  exit 1
fi
if [[ "$have" != "$want" ]]; then
  echo "wasm-bindgen-cli $have but the crate wants $want; they must match" >&2
  exit 1
fi

cargo build --release --target wasm32-unknown-unknown -p usdt-pir-client
mkdir -p "$out"
wasm-bindgen --target "$target" --out-dir "$out" \
  target/wasm32-unknown-unknown/release/usdt_pir_client.wasm

echo "built $out/usdt_pir_client_bg.wasm ($(du -h "$out/usdt_pir_client_bg.wasm" | cut -f1))"
