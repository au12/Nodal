#!/usr/bin/env bash
# Downloads six CC0 panoramas from Poly Haven (https://polyhaven.com, CC0) and builds the demo tour.
# Usage: scripts/make-demo.sh [out-dir]   (needs a built `nodal` binary: cargo build --release)
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=${1:-demo-tour}
SRC=$(mktemp -d)
for n in venice_sunset piazza_bologni royal_esplanade hamburg_hbf lebombo spruit_sunrise; do
  echo "downloading $n…"
  curl -fsSL -o "$SRC/$n.jpg" "https://dl.polyhaven.org/file/ph-assets/HDRIs/extra/Tonemapped%20JPG/$n.jpg"
done
./target/release/nodal tile --title "Nodal Demo Tour" -o "$OUT" \
  "$SRC/venice_sunset.jpg" "$SRC/piazza_bologni.jpg" "$SRC/royal_esplanade.jpg" \
  "$SRC/hamburg_hbf.jpg" "$SRC/lebombo.jpg" "$SRC/spruit_sunrise.jpg"
rm -rf "$SRC"
echo "Open it with:  ./target/release/nodal $OUT   (then press E to add hotspots)"
