#!/usr/bin/env bash
# Build inkbuddy and install the first-party plugin into ~/.inkbuddy/plugins.
set -euo pipefail

# A login/launchd-launched shell often lacks ~/.cargo/bin on PATH.
if [ -f "$HOME/.cargo/env" ]; then
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found; install the Rust toolchain with rustup first" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOME_DIR="${INKBUDDY_HOME:-$HOME/.inkbuddy}"

echo "==> Building workspace (release)"
cd "$ROOT"
cargo build --release

echo "==> Creating $HOME_DIR/plugins"
mkdir -p "$HOME_DIR/plugins/lyrics" "$HOME_DIR/plugins/clock" "$HOME_DIR/cache/lyrics"

HOST_BIN_ABS="$ROOT/target/release/inkbuddy"
PLUGIN_BIN_ABS="$ROOT/target/release/inkbuddy-plugin-lyrics"
CLOCK_BIN_ABS="$ROOT/target/release/inkbuddy-plugin-clock"

# Escape backslashes and double quotes so an install path containing them
# still produces a valid TOML basic string in command = "..." (backslashes
# first, then quotes).
toml_escape() {
  local s="$1"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  printf '%s' "$s"
}

LYRICS_CMD_TOML="$(toml_escape "$PLUGIN_BIN_ABS")"
CLOCK_CMD_TOML="$(toml_escape "$CLOCK_BIN_ABS")"

# Only create a manifest when one is absent. Re-running install.sh must not
# silently clobber a user's edited enabled/default_on/timeout_ms/args.
if [ ! -f "$HOME_DIR/plugins/lyrics/plugin.toml" ]; then
  cat > "$HOME_DIR/plugins/lyrics/plugin.toml" <<EOF
[plugin]
id = "lyrics"
name = "歌词"
version = "0.1.0"
description = "正在播放的同步歌词，粤语歌带逐字粤拼"
command = "$LYRICS_CMD_TOML"

[runtime]
enabled = true
default_on = true
timeout_ms = 4000
args = []
EOF
fi

if [ ! -f "$HOME_DIR/plugins/clock/plugin.toml" ]; then
  cat > "$HOME_DIR/plugins/clock/plugin.toml" <<EOF
[plugin]
id = "clock"
name = "时钟"
version = "0.1.0"
description = "当前日期与时间"
command = "$CLOCK_CMD_TOML"

[runtime]
enabled = true
default_on = false
timeout_ms = 2000
args = []
EOF
fi

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
