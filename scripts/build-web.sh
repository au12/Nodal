#!/usr/bin/env bash
# Builds the web viewer into dist/web (WASM + JS glue + static shell).
# Requires: rustup target add wasm32-unknown-unknown; cargo install wasm-bindgen-cli --version <same as Cargo.lock>
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=${1:-dist/web}
cargo build --profile web --target wasm32-unknown-unknown -p nodal-app --lib
rm -rf "$OUT" && mkdir -p "$OUT"
wasm-bindgen --target web --no-typescript --out-dir "$OUT/pkg" target/wasm32-unknown-unknown/web/nodal_app.wasm
if command -v wasm-opt >/dev/null; then
  wasm-opt -Oz --strip-debug --strip-producers --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals "$OUT/pkg/nodal_app_bg.wasm" -o "$OUT/pkg/nodal_app_bg.wasm"
fi
cp web/* "$OUT/"
# Precompressed variants, served automatically by nodal-server (Content-Encoding negotiation).
for f in "$OUT"/pkg/*.wasm "$OUT"/pkg/*.js "$OUT"/*.html "$OUT"/*.webmanifest; do
  command -v brotli >/dev/null && brotli -f -k -q 11 "$f"
  gzip -f -k -9 "$f"
done
echo "web viewer → $OUT ($(du -sh "$OUT" | cut -f1))"
