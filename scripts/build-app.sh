#!/usr/bin/env bash
# Build InkBuddy.app: a real macOS bundle that runs the host in the background
# (no Dock icon). The E-Ink reader still just opens a URL in its browser.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
cd "$ROOT"

echo "==> cargo build --release"
cargo build --release

APP="$ROOT/dist/InkBuddy.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$ROOT/target/release/inkbuddy" "$APP/Contents/MacOS/inkbuddy"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>InkBuddy</string>
  <key>CFBundleDisplayName</key><string>InkBuddy</string>
  <key>CFBundleIdentifier</key><string>cn.inkbuddy.host</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleExecutable</key><string>inkbuddy</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "==> Built: $APP"
echo
echo "The host reads ~/.inkbuddy/config.toml and ~/.inkbuddy/plugins/."
echo "Run scripts/install.sh first, then double-click InkBuddy.app."
echo "It has no Dock icon; check http://127.0.0.1:9012/manage or stop it with:"
echo "  pkill -f InkBuddy.app/Contents/MacOS/inkbuddy"
