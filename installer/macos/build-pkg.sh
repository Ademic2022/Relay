#!/usr/bin/env bash
# Build Relay-<version>.pkg (PRD §12 M8, §10 macOS).
#
# Environment (all optional; without them the package is unsigned):
#   UNIVERSAL=1                  build arm64 + x86_64 and lipo them
#   MACOS_APP_SIGN_ID            "Developer ID Application: …" for the binary
#   MACOS_INSTALLER_SIGN_ID      "Developer ID Installer: …" for the .pkg
#   MACOS_NOTARY_PROFILE         notarytool keychain profile (xcrun notarytool store-credentials)
#   RELAY_DEFAULT_SERVER / RELAY_DEFAULT_SERVER_KEY / RELAY_UPDATE_URL / RELAY_UPDATE_KEY
#                                baked into the build (see crates/relay-app/src/settings.rs)
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT=target/installer/macos
STAGE="$OUT/root"
rm -rf "$OUT"
mkdir -p "$STAGE/usr/local/bin"

if [ "${UNIVERSAL:-0}" = 1 ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release -p relay-app --target "$t"
  done
  lipo -create -output "$STAGE/usr/local/bin/relay" \
    target/aarch64-apple-darwin/release/relay target/x86_64-apple-darwin/release/relay
else
  cargo build --release -p relay-app
  cp target/release/relay "$STAGE/usr/local/bin/relay"
fi

if [ -n "${MACOS_APP_SIGN_ID:-}" ]; then
  codesign --force --timestamp --options runtime \
    --entitlements installer/macos/entitlements.plist \
    --identifier com.relay.relay \
    --sign "$MACOS_APP_SIGN_ID" "$STAGE/usr/local/bin/relay"
  codesign --verify --strict --verbose=2 "$STAGE/usr/local/bin/relay"
else
  echo "warning: MACOS_APP_SIGN_ID not set; binary is unsigned" >&2
fi

# No AppleDouble (._*) files in the payload.
xattr -cr "$STAGE"
export COPYFILE_DISABLE=1
pkgbuild --root "$STAGE" --identifier com.relay.client --version "$VERSION" \
  --install-location / --scripts installer/macos/scripts "$OUT/relay-component.pkg"

PKG="$OUT/Relay-$VERSION.pkg"
if [ -n "${MACOS_INSTALLER_SIGN_ID:-}" ]; then
  productbuild --package "$OUT/relay-component.pkg" --sign "$MACOS_INSTALLER_SIGN_ID" "$PKG"
else
  echo "warning: MACOS_INSTALLER_SIGN_ID not set; package is unsigned" >&2
  productbuild --package "$OUT/relay-component.pkg" "$PKG"
fi

if [ -n "${MACOS_NOTARY_PROFILE:-}" ]; then
  xcrun notarytool submit "$PKG" --keychain-profile "$MACOS_NOTARY_PROFILE" --wait
  xcrun stapler staple "$PKG"
  xcrun stapler validate "$PKG"
fi

SIZE=$(stat -f %z "$PKG")
echo "$PKG: $((SIZE / 1024 / 1024)) MB"
# PRD §2 goal 7: installer ≤ 25 MB.
if [ "$SIZE" -gt $((25 * 1024 * 1024)) ]; then
  echo "error: installer is larger than 25 MB" >&2
  exit 1
fi
