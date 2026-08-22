#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR"
DEST="${DEST:-$HOME/.local/bin}"
TARGET_ROOT="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
SRC="$TARGET_ROOT/release"

usage() {
    echo "Usage: ./deploy.sh [--restart] [--no-save] [--wipe-session] [--clean] [--dest DIR]"
    echo ""
    echo "  (no flags)  Build, save manual layout snapshot, copy binaries"
    echo "  --restart   Also kill the mux server (Mac reconnect triggers new binary)"
    echo "  --no-save   Skip wakterm cli save-layout (use when layout/session state is known bad)"
    echo "  --wipe-session  Remove saved session state after restart for a clean session"
    echo "  --clean     Run cargo clean for deployed crates before building"
    echo "  --dest DIR  Deploy binaries into this directory (default: $DEST)"
    echo ""
    echo "After --restart, reconnect from Mac then run:"
    echo "  cd $REPO_ROOT && target/release/wakterm cli restore-layout"
    echo ""
    echo "This script deploys binaries directly into ~/.local/bin."
    echo "To install them system-wide instead:"
    echo "  sudo ./install.sh --system"
    echo ""
    echo "To install or refresh the standalone user service:"
    echo "  ./install-user-service.sh"
}

RESTART=false
SAVE_SESSION=true
WIPE_SESSION=false
CLEAN_BUILD=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --restart)
            RESTART=true
            shift
            ;;
        --no-save)
            SAVE_SESSION=false
            shift
            ;;
        --wipe-session)
            WIPE_SESSION=true
            shift
            ;;
        --clean)
            CLEAN_BUILD=true
            shift
            ;;
        --dest)
            DEST="$2"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown arg: $1"
            usage
            exit 1
            ;;
    esac
done

SESSION_FILE="${XDG_DATA_HOME:-$HOME/.local/share}/wakterm/session.json"
LEGACY_SESSION_FILE="${XDG_RUNTIME_DIR:-/run/user/$UID}/wakterm/session.json"
CURRENT_HEAD="$(git -C "$REPO_ROOT" rev-parse --short=8 HEAD)"

binary_matches_head() {
    local bin="$1"
    local path="$2"
    [ -x "$path" ] && "$path" --version 2>/dev/null | grep -q "$CURRENT_HEAD"
}

should_skip_build() {
    if $CLEAN_BUILD; then
        return 1
    fi

    for bin in wakterm wakterm-gui wakterm-mux-server; do
        if ! binary_matches_head "$bin" "$SRC/$bin"; then
            return 1
        fi
        if ! binary_matches_head "$bin" "$DEST/$bin"; then
            return 1
        fi
    done

    return 0
}

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

restart_via_user_service() {
    local unit="wakterm-mux-server.service"
    if ! command -v systemctl >/dev/null 2>&1; then
        return 1
    fi
    if ! systemctl --user cat "$unit" >/dev/null 2>&1; then
        return 1
    fi

    if $WIPE_SESSION; then
        systemctl --user stop "$unit"
        rm -f "$SESSION_FILE" "$LEGACY_SESSION_FILE"
        echo "  Stopped $unit"
        echo "  Removed $SESSION_FILE"
        systemctl --user start "$unit"
        echo "  Started $unit"
    else
        systemctl --user restart "$unit"
        echo "  Restarted $unit"
    fi
    return 0
}

echo "=== Step 1: Build ==="
if $CLEAN_BUILD; then
    echo "  Cleaning mux/client/server artifacts first"
    cargo clean -p mux -p codec -p wakterm -p wakterm-gui -p wakterm-mux-server
fi
if should_skip_build; then
    echo "  Skipping build; release and deployed binaries already match HEAD ($CURRENT_HEAD)"
else
    CCACHE_DISABLE=1 cargo build --release -p wakterm -p wakterm-gui -p wakterm-mux-server 2>&1 | tail -3
fi
echo ""

if $SAVE_SESSION; then
    echo "=== Step 2: Save manual layout snapshot ==="
    cd "$REPO_ROOT"
    if [ ! -x "$DEST/wakterm" ]; then
        echo "  Cannot save through the running mux: $DEST/wakterm is not installed"
        echo "  Install once with --no-save, or pass --no-save intentionally"
        exit 1
    fi
    "$DEST/wakterm" cli save-layout
    echo ""
else
    echo "=== Step 2: Skip manual layout snapshot ==="
    echo "  Leaving layout.json/session.json untouched"
    echo ""
fi

echo "=== Step 3: Deploy binaries ==="
mkdir -p "$DEST"
if should_skip_build; then
    echo "  Skipping copy; deployed binaries already match HEAD ($CURRENT_HEAD)"
else
    for bin in wakterm wakterm-gui wakterm-mux-server; do
        rm -f "$DEST/$bin"
        cp "$SRC/$bin" "$DEST/$bin"
        echo "  $bin → $DEST/$bin"
    done
fi
echo "  Version: $($DEST/wakterm --version 2>&1)"
echo ""

if $RESTART; then
    echo "=== Step 4: Restart mux server ==="
    if restart_via_user_service; then
        echo ""
        if $WIPE_SESSION; then
            echo "Reconnect from Mac. The user service is running with a clean session."
            echo ""
            echo "To restore tabs manually afterward:"
            echo "  cd /code/wakterm && target/release/wakterm cli restore-layout"
        else
            echo "Reconnect from Mac. Your tabs will be restored automatically"
            echo "(the user service reads session.json on startup)."
        fi
    else
        PID=$(pgrep -f 'wakterm-mux-server.*pid-file' | head -1 || true)
        if [ -n "$PID" ]; then
            kill "$PID"
            wait_for_exit "$PID"
            echo "  Killed PID $PID"
            if $WIPE_SESSION; then
                rm -f "$SESSION_FILE" "$LEGACY_SESSION_FILE"
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
                echo "  cd $REPO_ROOT && target/release/wakterm cli restore-layout"
            fi
        else
            echo "  No running mux server found"
            if $WIPE_SESSION; then
                rm -f "$SESSION_FILE" "$LEGACY_SESSION_FILE"
                echo "  Removed $SESSION_FILE"
            fi
        fi
    fi
else
    echo "Binaries deployed. Run with --restart to kill the server."
    echo "Or manually: kill \$(pgrep -f 'wakterm-mux-server.*pid-file' | head -1)"
fi

echo ""
echo "Binaries are now installed in $DEST."
echo "To install them system-wide instead:"
echo "  sudo ./install.sh --system"
echo "To install or refresh the standalone user service:"
echo "  ./install-user-service.sh"
