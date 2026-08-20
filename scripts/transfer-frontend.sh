#!/usr/bin/env bash
# Copy only the files needed for the Netlify frontend deployment.
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
DEST="$(cd -- "$ROOT/.." && pwd)/poulpy-pir-frontend"

FILES=(
  netlify.toml
  netlify/functions/pir-proxy.mjs
  client/netlify-build.sh
  client/web/index.html
  client/web/pir.js
  client/web/poulpy.png
  client/web/usdt_pir_client.js
  client/web/usdt_pir_client_bg.wasm
)

# Validate the generated JS/WASM pair before modifying the destination.
"$ROOT/client/netlify-build.sh"
for path in "${FILES[@]}"; do
  if [[ ! -s "$ROOT/$path" ]]; then
    echo "missing frontend deployment file: $path" >&2
    exit 1
  fi
done

mkdir -p "$DEST"
for path in "${FILES[@]}"; do
  mkdir -p "$DEST/$(dirname -- "$path")"
  cp -p "$ROOT/$path" "$DEST/$path"
done

printf 'Copied %d frontend deployment files to %s\n' "${#FILES[@]}" "$DEST"
printf 'Set PIR_BACKEND_URL in Netlify before deploying.\n'
