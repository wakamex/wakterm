#!/usr/bin/env bash
set -euo pipefail

DEST="$HOME/wezterm-test"
SRC="/code/wezterm/target/release"

usage() {
    echo "Usage: ./deploy.sh [--restart] [--no-save] [--wipe-session]"
    echo ""
    echo "  (no flags)  Build, save manual layout snapshot, copy binaries"
    echo "  --restart   Also kill the mux server (Mac reconnect triggers new binary)"
    echo "  --no-save   Skip wezterm cli save-layout (use when layout/session state is known bad)"
    echo "  --wipe-session  Remove runtime session.json after restart for a clean session"
    echo ""
    echo "After --restart, reconnect from Mac then run:"
    echo "  cd /code/wezterm && target/release/wezterm cli restore-layout"
}

RESTART=false
SAVE_SESSION=true
WIPE_SESSION=false
for arg in "$@"; do
    case "$arg" in
        --restart) RESTART=true ;;
        --no-save) SAVE_SESSION=false ;;
        --wipe-session) WIPE_SESSION=true ;;
        --help|-h) usage; exit 0 ;;
        *) echo "Unknown arg: $arg"; usage; exit 1 ;;
    esac
done

SESSION_FILE="${XDG_RUNTIME_DIR:-/run/user/$UID}/wezterm/session.json"

if $WIPE_SESSION && ! $RESTART; then
    echo "--wipe-session requires --restart"
    exit 1
fi

wait_for_exit() {
    local pid="$1"
    local attempts=50

    while kill -0 "$pid" 2>/dev/null; do
        sleep 0.1
        attempts=$((attempts - 1))
        if [ "$attempts" -le 0 ]; then
            echo "  PID $pid did not exit after 5s; sending SIGKILL"
            kill -KILL "$pid"
            break
        fi
    done
}

echo "=== Step 1: Build ==="
CCACHE_DISABLE=1 cargo build --release -p wezterm -p wezterm-gui -p wezterm-mux-server 2>&1 | tail -3
echo ""

if $SAVE_SESSION; then
    echo "=== Step 2: Save manual layout snapshot ==="
    cd /code/wezterm
    "$SRC/wezterm" cli save-layout
    echo ""
else
    echo "=== Step 2: Skip manual layout snapshot ==="
    echo "  Leaving layout.json/session.json untouched"
    echo ""
fi

echo "=== Step 3: Deploy binaries ==="
for bin in wezterm wezterm-gui wezterm-mux-server; do
    rm -f "$DEST/$bin"
    cp "$SRC/$bin" "$DEST/$bin"
    echo "  $bin → $DEST/$bin"
done
echo "  Version: $($DEST/wezterm --version 2>&1)"
echo ""

if $RESTART; then
    echo "=== Step 4: Killing mux server ==="
    PID=$(pgrep -f 'wezterm-mux-server.*pid-file' | head -1 || true)
    if [ -n "$PID" ]; then
        kill "$PID"
        wait_for_exit "$PID"
        echo "  Killed PID $PID"
        if $WIPE_SESSION; then
            rm -f "$SESSION_FILE"
            echo "  Removed $SESSION_FILE"
        fi
        echo ""
        if $WIPE_SESSION; then
            echo "Reconnect from Mac. The next server will start with a clean session."
        else
            echo "Reconnect from Mac. Your tabs will be restored automatically"
            echo "(the new server reads session.json on startup)."
        fi
        echo ""
        if $WIPE_SESSION; then
            echo "To restore tabs manually afterward:"
            echo "  cd /code/wezterm && target/release/wezterm cli restore-layout"
        fi
    else
        echo "  No running mux server found"
        if $WIPE_SESSION; then
            rm -f "$SESSION_FILE"
            echo "  Removed $SESSION_FILE"
        fi
    fi
else
    echo "Binaries deployed. Run with --restart to kill the server."
    echo "Or manually: kill \$(pgrep -f 'wezterm-mux-server.*pid-file' | head -1)"
fi
