#!/bin/sh
# 构建 macOS 菜单栏 app（InkBuddy.app）
# 用法：./scripts/build-macos-app.sh [版本号，默认 0.3.3]
# 依赖：Rust 工具链、Swift 编译器（Xcode CLT）、可选 rsvg-convert（重新渲染菜单栏图标）
set -e
cd "$(dirname "$0")/.."

VERSION="${1:-0.3.3}"
APP_NAME=InkBuddy
BUILD_DIR=build-app
APP="$BUILD_DIR/$APP_NAME.app"
RESDIR="$APP/Contents/Resources"

echo "==> 编译 Rust host"
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p inkbuddy

echo "==> 组装 bundle"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$RESDIR/plugins/lyrics" "$RESDIR/plugins/clock"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDisplayName</key><string>inkbuddy</string>
	<key>CFBundleExecutable</key><string>$APP_NAME</string>
	<key>CFBundleIconFile</key><string>AppIcon</string>
	<key>CFBundleIdentifier</key><string>com.inkbuddy.app</string>
	<key>CFBundleName</key><string>inkbuddy</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$VERSION</string>
	<key>LSMinimumSystemVersion</key><string>13.0</string>
	<key>LSUIElement</key><false/>
	<key>LSEnvironment</key>
	<dict>
		<key>INKBUDDY_LATENCY_MS</key><string>300</string>
		<key>INKBUDDY_LATENCY_OTHER_MS</key><string>300</string>
	</dict>
</dict>
</plist>
EOF

echo "==> 编译 Swift 界面"
swiftc -O app/Sources/main.swift app/Sources/AppDelegate.swift app/Sources/AppState.swift \
    app/Sources/HostService.swift app/Sources/SettingsView.swift app/Sources/Updater.swift \
    app/Sources/MenuPreviewController.swift app/Sources/ViewEditor.swift \
    app/Sources/EditorPreview.swift \
    -o "$APP/Contents/MacOS/$APP_NAME"

cp target/release/inkbuddy "$RESDIR/inkbuddy-host"
cp target/release/inkbuddy-plugin-lyrics "$RESDIR/plugins/lyrics/inkbuddy-plugin-lyrics"
cp target/release/inkbuddy-plugin-clock "$RESDIR/plugins/clock/inkbuddy-plugin-clock"

# 歌词插件读取 Now Playing 用的 MediaRemote 组件随 app 分发，用户无需 brew install
cp vendor/nowplaying-cli/MediaRemoteMini.dylib "$RESDIR/plugins/lyrics/MediaRemoteMini.dylib"
cp vendor/nowplaying-cli/mediaremote-mini.pl "$RESDIR/plugins/lyrics/mediaremote-mini.pl"

cat > "$RESDIR/plugins/lyrics/plugin.toml" <<'EOF'
[plugin]
id = "lyrics"
name = "歌词"
version = "0.1.0"
description = "正在播放的同步歌词，粤语歌带逐字粤拼"
command = "./inkbuddy-plugin-lyrics"

[runtime]
enabled = true
default_on = true
timeout_ms = 4000
args = []
EOF

cat > "$RESDIR/plugins/clock/plugin.toml" <<'EOF'
[plugin]
id = "clock"
name = "时钟"
version = "0.1.0"
description = "当前日期与时间"
command = "./inkbuddy-plugin-clock"

[runtime]
enabled = true
default_on = false
timeout_ms = 2000
args = []
EOF

cat > "$RESDIR/config.toml" <<'EOF'
[server]
bind = "0.0.0.0"
port = 9012

[display]
title = "InkBuddy"
refresh_ms = 500
EOF

cp app/Assets/AppIcon.icns "$RESDIR/"
# 菜单栏图标：18pt（36px 为 @2x）
cp app/Assets/menu18.png "$RESDIR/MenuBarIcon.png"
cp app/Assets/menu36.png "$RESDIR/MenuBarIcon@2x.png"

echo "==> 签名"
codesign --force --deep -s - "$APP"

echo "==> 完成：$APP"
