#!/usr/bin/env bash
# Build inkbuddy and install the first-party plugin into ~/.inkbuddy/plugins.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
HOME_DIR="${INKBUDDY_HOME:-$HOME/.inkbuddy}"

echo "==> Building workspace (release)"
cd "$ROOT"
cargo build --release

echo "==> Creating $HOME_DIR/plugins"
mkdir -p "$HOME_DIR/plugins/lyrics" "$HOME_DIR/plugins/clock" "$HOME_DIR/cache/lyrics"

HOST_BIN_ABS="$ROOT/target/release/inkbuddy"
PLUGIN_BIN_ABS="$ROOT/target/release/inkbuddy-plugin-lyrics"
CLOCK_BIN_ABS="$ROOT/target/release/inkbuddy-plugin-clock"

cat > "$HOME_DIR/plugins/lyrics/plugin.toml" <<EOF
[plugin]
id = "lyrics"
name = "歌词"
version = "0.1.0"
description = "正在播放的同步歌词，粤语歌带逐字粤拼"
command = "$PLUGIN_BIN_ABS"

[runtime]
enabled = true
default_on = true
timeout_ms = 4000
args = []
EOF

cat > "$HOME_DIR/plugins/clock/plugin.toml" <<EOF
[plugin]
id = "clock"
name = "时钟"
version = "0.1.0"
description = "当前日期与时间"
command = "$CLOCK_BIN_ABS"

[runtime]
enabled = true
default_on = false
timeout_ms = 2000
args = []
EOF

echo "==> Writing default config (if absent)"
if [ ! -f "$HOME_DIR/config.toml" ]; then
  "$HOST_BIN_ABS" --init-config
fi

echo
echo "Done. Start the host with:"
echo "  $HOST_BIN_ABS"
echo
echo "Then open on the E-Ink reader:"
echo "  http://<mac-lan-ip>:9012/"
echo "Plugin selection page:"
echo "  http://127.0.0.1:9012/manage"
