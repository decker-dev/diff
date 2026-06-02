#!/usr/bin/env bash
# Package the `diff` binary into a real, signed macOS .app bundle (icon + identity
# + ad-hoc signature) so it behaves like an installed, "registered" native app.
#
# Usage:
#   ./bundle.sh            # build + bundle into target/release/diff.app
#   ./bundle.sh --install  # also copy the bundle into /Applications
#   ./bundle.sh --open     # also launch the bundle when done
set -euo pipefail
cd "$(dirname "$0")"

APP_NAME="diff"
ROOT="$PWD"
ASSETS="$ROOT/crates/app/assets"
ICON_SVG="$ASSETS/icon.svg"
ICNS="$ASSETS/diff.icns"
APP="$ROOT/target/release/$APP_NAME.app"

echo "==> Building release binary"
cargo build --release --bin "$APP_NAME"

echo "==> Rendering icon (.icns)"
ICONSET="$(mktemp -d)/diff.iconset"
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
	rsvg-convert -w "$s"  -h "$s"  "$ICON_SVG" -o "$ICONSET/icon_${s}x${s}.png"
	rsvg-convert -w "$((s*2))" -h "$((s*2))" "$ICON_SVG" -o "$ICONSET/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$ICONSET" -o "$ICNS"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/$APP_NAME" "$APP/Contents/MacOS/$APP_NAME"
cp "$ICNS" "$APP/Contents/Resources/diff.icns"
cp "$ROOT/crates/app/Info.plist" "$APP/Contents/Info.plist"

echo "==> Ad-hoc code signing"
codesign --force --deep --sign - "$APP"
codesign --verify --verbose=1 "$APP" 2>&1 | sed 's/^/    /'

# Refresh Finder/Dock icon caches so the new icon shows immediately.
touch "$APP"

echo "==> Done: $APP"

for arg in "$@"; do
	case "$arg" in
		--install)
			echo "==> Installing to /Applications"
			rm -rf "/Applications/$APP_NAME.app"
			cp -R "$APP" "/Applications/$APP_NAME.app"
			;;
		--open)
			echo "==> Launching"
			open "$APP"
			;;
	esac
done
