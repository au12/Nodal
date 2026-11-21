#!/usr/bin/env bash
# Packages the `nodal` binary as Nodal.app plus a drag-to-Applications disk image.
#
#   cargo build --release --locked -p nodal-app --bin nodal --target aarch64-apple-darwin
#   scripts/package-macos.sh [target-triple]            # default: aarch64-apple-darwin
#
# Signing (all optional; read from the environment):
#   MACOS_CERTIFICATE           base64 Developer ID Application .p12, imported into a temporary keychain
#   MACOS_CERTIFICATE_PASSWORD  its password
#   MACOS_SIGNING_IDENTITY      e.g. "Developer ID Application: Name (TEAMID)" — enables hardened-runtime signing
#   APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD — additionally notarize and staple the disk image
# Without them the app is ad-hoc signed: it runs, but Gatekeeper asks once on first launch
# (System Settings → Privacy & Security → Open Anyway; right-click → Open on macOS 14 and earlier).
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=${1:-aarch64-apple-darwin}
BIN=${BIN:-target/$TARGET/release/nodal}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
BUILD=${BUILD_NUMBER:-$VERSION}
ARCH=${TARGET%%-*}
[ "$ARCH" = aarch64 ] && ARCH=arm64
OUT=dist/macos
APP="$OUT/Nodal.app"
[ -x "$BIN" ] || { echo "error: $BIN not found — build it first" >&2; exit 1; }

echo "==> Nodal.app $VERSION ($BUILD) for $ARCH"
rm -rf "$OUT" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/nodal"
cp packaging/macos/Nodal.icns "$APP/Contents/Resources/Nodal.icns"
cp LICENSE "$APP/Contents/Resources/LICENSE.txt"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@BUILD@/$BUILD/g" packaging/macos/Info.plist.in > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

if [ "$(uname -s)" != Darwin ]; then
  echo "bundle written to $APP (code signing and the disk image need macOS)"
  exit 0
fi
plutil -lint "$APP/Contents/Info.plist"

# --- code signing ------------------------------------------------------------------------
TMP=$(mktemp -d)
cleanup() {
  [ -n "${KEYCHAIN:-}" ] && security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT
if [ -n "${MACOS_CERTIFICATE:-}" ]; then
  echo "==> importing signing certificate"
  KEYCHAIN="$TMP/nodal-signing.keychain-db"
  KEYCHAIN_PASSWORD=$(openssl rand -base64 24)
  echo "$MACOS_CERTIFICATE" | base64 --decode > "$TMP/cert.p12"
  security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
  security set-keychain-settings -lut 21600 "$KEYCHAIN"
  security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
  security import "$TMP/cert.p12" -P "${MACOS_CERTIFICATE_PASSWORD:-}" -A -t cert -f pkcs12 -k "$KEYCHAIN"
  security set-key-partition-list -S apple-tool:,apple: -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null
  # shellcheck disable=SC2046
  security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | tr -d '"')
fi
if [ -n "${MACOS_SIGNING_IDENTITY:-}" ]; then
  echo "==> signing with $MACOS_SIGNING_IDENTITY (hardened runtime)"
  codesign --force --options runtime --timestamp --sign "$MACOS_SIGNING_IDENTITY" "$APP"
else
  echo "==> ad-hoc signing (no Developer ID configured)"
  codesign --force --sign - "$APP"
fi
codesign --verify --strict --verbose=2 "$APP"

# --- disk image ----------------------------------------------------------------------------
DMG="$OUT/Nodal-$VERSION-macos-$ARCH.dmg"
STAGE="$TMP/dmg"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
# hdiutil occasionally fails with "Resource busy" on CI runners; retry a few times.
for attempt in 1 2 3 4 5; do
  hdiutil create -volname "Nodal" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null && break
  [ "$attempt" = 5 ] && { echo "error: hdiutil create failed" >&2; exit 1; }
  echo "hdiutil create failed (attempt $attempt), retrying…" >&2
  sleep $((attempt * 3))
done
if [ -n "${MACOS_SIGNING_IDENTITY:-}" ]; then
  codesign --force --timestamp --sign "$MACOS_SIGNING_IDENTITY" "$DMG"
  if [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ]; then
    echo "==> notarizing"
    xcrun notarytool submit "$DMG" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_APP_PASSWORD" --wait
    xcrun stapler staple "$DMG"
  fi
fi
echo "==> $DMG"
