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
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/GlassMac"
sed -e "s/__VERSION__/$VERSION/" -e "s/__BUILD__/$BUILD/" app-mac/packaging/Info.plist > "$APP/Contents/Info.plist"
sed -e "s/__VERSION__/$VERSION/" app-mac/packaging/README.txt > "$STAGE/README.txt"
# Ad-hoc signature: required to run on Apple Silicon. Not notarized.
# LATER(M3b): Developer ID signing so Screen Recording/Accessibility grants survive rebuilds.
codesign --force --deep --sign - "$APP"
ln -s /Applications "$STAGE/Applications"

mkdir -p dist
DMG="dist/Glass-mac-$VERSION.dmg"
rm -f "$DMG"
hdiutil create -volname "Glass $VERSION" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"
echo "$DMG"
