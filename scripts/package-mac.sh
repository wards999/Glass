#!/usr/bin/env bash
# Builds Glass.app and dist/Glass-mac-<version>.dmg. Run on macOS (Apple Silicon).
# Usage: scripts/package-mac.sh [version]
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION="${1:-$(git describe --tags --always --dirty)}"
BUILD="$(git rev-list --count HEAD)"

cargo build -p glass-core --release
(cd app-mac && swift build -c release --product GlassMac)
BIN="$(cd app-mac && swift build -c release --show-bin-path)/GlassMac"

STAGE="$(mktemp -d)"
APP="$STAGE/Glass.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/GlassMac"
cp app-mac/packaging/Glass.icns "$APP/Contents/Resources/Glass.icns"
sed -e "s/__VERSION__/$VERSION/" -e "s/__BUILD__/$BUILD/" app-mac/packaging/Info.plist > "$APP/Contents/Info.plist"
sed -e "s/__VERSION__/$VERSION/" app-mac/packaging/README.txt > "$STAGE/README.txt"
# Signature: with MAC_SIGN_P12 / MAC_SIGN_PASSWORD (CI secrets) a stable
# self-signed certificate ("Glass Self-Signed"), so macOS keeps Screen
# Recording and Accessibility grants across test builds (ADR 0011);
# otherwise ad-hoc (local builds). Not notarized either way.
if [[ -n "${MAC_SIGN_P12:-}" && -n "${MAC_SIGN_PASSWORD:-}" ]]; then
  KC="$RUNNER_TEMP/glass-sign.keychain-db"
  KCPASS="$(uuidgen)"
  echo "$MAC_SIGN_P12" | base64 --decode > "$RUNNER_TEMP/glass-sign.p12"
  security create-keychain -p "$KCPASS" "$KC"
  security set-keychain-settings -lut 21600 "$KC"
  security unlock-keychain -p "$KCPASS" "$KC"
  security import "$RUNNER_TEMP/glass-sign.p12" -k "$KC" -P "$MAC_SIGN_PASSWORD" -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KCPASS" "$KC" >/dev/null
  security list-keychains -d user -s "$KC" $(security list-keychains -d user | tr -d '"')
  rm -f "$RUNNER_TEMP/glass-sign.p12"
  # Trust it for code signing on this runner only, so codesign accepts it.
  security find-certificate -c "Glass Self-Signed" -p "$KC" > "$RUNNER_TEMP/glass-sign.pem"
  sudo security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain "$RUNNER_TEMP/glass-sign.pem"
  codesign --force --deep --sign "Glass Self-Signed" --keychain "$KC" "$APP"
  codesign -dr - "$APP" 2>&1 | sed 's/^/  /'
else
  codesign --force --deep --sign - "$APP"
fi
ln -s /Applications "$STAGE/Applications"

mkdir -p dist
DMG="dist/Glass-mac-$VERSION.dmg"
rm -f "$DMG"
hdiutil create -volname "Glass $VERSION" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"
echo "$DMG"
