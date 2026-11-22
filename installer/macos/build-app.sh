#!/usr/bin/env bash
# Build Relay.app (PRD §5.1): the Tauri window (relay-app) and the engine
# (relay) side by side in Contents/MacOS. The window starts the engine from
# there, so macOS attributes Screen Recording / Accessibility to "Relay".
#
# Output: target/installer/macos/Relay.app
# Environment: UNIVERSAL=1 (arm64 + x86_64), MACOS_APP_SIGN_ID (Developer ID
# Application identity; unsigned without it).
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT=target/installer/macos
APP="$OUT/Relay.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

if [ "${UNIVERSAL:-0}" = 1 ]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release -p relay-app -p relay-ui --target "$t"
  done
  for b in relay relay-app; do
    lipo -create -output "$APP/Contents/MacOS/$b" \
      "target/aarch64-apple-darwin/release/$b" "target/x86_64-apple-darwin/release/$b"
  done
else
  cargo build --release -p relay-app -p relay-ui
  cp target/release/relay target/release/relay-app "$APP/Contents/MacOS/"
fi

# Icon: .icns from the 512 px PNG.
ICONSET="$OUT/icon.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
SRC=app/src-tauri/icons/icon.png
for s in 16 32 128 256; do
  sips -z $s $s "$SRC" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$SRC" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
cp "$SRC" "$ICONSET/icon_512x512.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Relay.icns"
rm -rf "$ICONSET"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Relay</string>
  <key>CFBundleDisplayName</key><string>Relay</string>
  <key>CFBundleIdentifier</key><string>com.relay.app</string>
  <key>CFBundleExecutable</key><string>relay-app</string>
  <key>CFBundleIconFile</key><string>Relay</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>Relay</string>
</dict>
</plist>
PLIST

xattr -cr "$APP"
if [ -n "${MACOS_APP_SIGN_ID:-}" ]; then
  # Inner executables first, then the bundle (hardened runtime, timestamp).
  for b in relay relay-app; do
    codesign --force --timestamp --options runtime \
      --entitlements installer/macos/entitlements.plist \
      --sign "$MACOS_APP_SIGN_ID" "$APP/Contents/MacOS/$b"
  done
  codesign --force --timestamp --options runtime \
    --entitlements installer/macos/entitlements.plist \
    --sign "$MACOS_APP_SIGN_ID" "$APP"
  codesign --verify --deep --strict --verbose=2 "$APP"
else
  # Ad-hoc signature: required to run on Apple silicon; not trusted for
  # distribution (Gatekeeper will still warn).
  codesign --force --deep --sign - "$APP"
  echo "warning: MACOS_APP_SIGN_ID not set; Relay.app is ad-hoc signed" >&2
fi
echo "$APP"
