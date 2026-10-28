#!/bin/bash
# Starts wakterm-gui on a virtual X display with a throwaway config and
# state, and fails unless it opens a window with two tabs within 60 s and is
# still running 5 s later without a panic.
#
# Usage: ci/gui-smoke.sh [BIN_DIR]   (default: target/release)
# Needs xvfb-run.
set -euo pipefail

BIN_DIR=${1:-target/release}
GUI="$BIN_DIR/wakterm-gui"
[ -x "$GUI" ] || { echo "gui-smoke: $GUI not found; build wakterm-gui first" >&2; exit 2; }
command -v xvfb-run >/dev/null || { echo "gui-smoke: xvfb-run is not installed" >&2; exit 2; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK"/{config,data,cache,runtime}
chmod 700 "$WORK/runtime"
MARKER="$WORK/started"

cat >"$WORK/config/wakterm.lua" <<EOF
local wakterm = require 'wakterm'
local config = wakterm.config_builder()
config.default_prog = { 'sleep', '600' }
wakterm.on('gui-startup', function()
  local tab, pane, window = wakterm.mux.spawn_window {}
  window:spawn_tab {}
  local f = io.open('$MARKER', 'w')
  f:write(#window:tabs())
  f:close()
end)
return config
EOF

# The GUI runs in its own process group, so that stopping it also stops
# the virtual display. A slow machine can take well over ten seconds to start
# it, so the test waits for the window instead of a fixed time.
setsid env -u SSH_AUTH_SOCK WAKTERM_CONFIG_FILE="$WORK/config/wakterm.lua" \
    XDG_CONFIG_HOME="$WORK/config" XDG_DATA_HOME="$WORK/data" \
    XDG_CACHE_HOME="$WORK/cache" XDG_RUNTIME_DIR="$WORK/runtime" \
    xvfb-run -a "$GUI" start --always-new-process --no-auto-connect \
    >"$WORK/gui.log" 2>&1 &
gui=$!
stop() { kill -TERM -- "-$gui" 2>/dev/null || true; wait "$gui" 2>/dev/null || true; }

fail() {
    stop
    echo "gui-smoke: $*" >&2
    sed 's/^/  | /' "$WORK/gui.log" >&2
    exit 1
}
for _ in $(seq 600); do
    [ "$(cat "$MARKER" 2>/dev/null)" = 2 ] && break
    kill -0 "$gui" 2>/dev/null || fail "wakterm-gui exited before opening its window"
    sleep 0.1
done
[ "$(cat "$MARKER" 2>/dev/null)" = 2 ] || fail "gui-startup did not open a window with two tabs within 60 s"
sleep 5
kill -0 "$gui" 2>/dev/null || fail "wakterm-gui exited within 5 s of opening its window"
stop
! grep -q "panic" "$WORK/gui.log" || fail "wakterm-gui logged a panic"
echo "gui-smoke: wakterm-gui started a window with two tabs and kept running"
