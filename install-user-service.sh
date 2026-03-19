#!/usr/bin/env bash
set -euo pipefail

unit_name="${UNIT_NAME:-wakterm-mux-server}"
systemd_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
env_file="${ENV_FILE:-$HOME/.config/wakterm-mux-server.env}"
unit_path="$systemd_dir/$unit_name.service"
template_path="$(cd "$(dirname "$0")" && pwd)/systemd/wakterm-mux-server.service"

default_user_bin="$HOME/.local/bin/wakterm-mux-server"
default_system_bin="/usr/local/bin/wakterm-mux-server"
if [ -n "${BIN_PATH:-}" ]; then
    bin_path="$BIN_PATH"
elif [ -x "$default_user_bin" ]; then
    bin_path="$default_user_bin"
elif [ -x "$default_system_bin" ]; then
    bin_path="$default_system_bin"
else
    bin_path="$default_user_bin"
fi

usage() {
    echo "Usage: ./install-user-service.sh [--bin PATH] [--unit-name NAME] [--env-file PATH] [--no-start]"
    echo ""
    echo "  --bin PATH       Binary to run"
    echo "                   Use --bin auto-path to resolve via command -v when needed"
    echo "  --unit-name NAME Service name without .service (default: $unit_name)"
    echo "  --env-file PATH  Optional EnvironmentFile path (default: $env_file)"
    echo "  --no-start       Install/enable only; do not start or restart the service"
}

start_service=true
bin_mode="auto"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --bin)
            bin_path="$2"
            bin_mode="explicit"
            shift 2
            ;;
        --unit-name)
            unit_name="$2"
            shift 2
            ;;
        --env-file)
            env_file="$2"
            shift 2
            ;;
        --no-start)
            start_service=false
            shift
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

if [ "$bin_mode" = "explicit" ] && [ "$bin_path" = "auto-path" ]; then
    resolved="$(command -v wakterm-mux-server || true)"
    if [ -z "$resolved" ]; then
        echo "Could not resolve wakterm-mux-server via command -v"
        exit 1
    fi
    bin_path="$resolved"
fi

if [ "$bin_mode" = "auto" ] && [ ! -x "$bin_path" ]; then
    resolved="$(command -v wakterm-mux-server || true)"
    if [ -n "$resolved" ]; then
        bin_path="$resolved"
    fi
fi

if [ ! -x "$bin_path" ]; then
    echo "Missing executable: $bin_path"
    exit 1
fi

mkdir -p "$systemd_dir"

sed \
    -e "s|ExecStart=__WAKTERM_MUX_SERVER_BIN__|ExecStart=$bin_path|" \
    -e "s|EnvironmentFile=-%h/.config/wakterm-mux-server.env|EnvironmentFile=-$env_file|" \
    "$template_path" >"$unit_path"

echo "Installed $unit_path"

if [ ! -f "$env_file" ]; then
    mkdir -p "$(dirname "$env_file")"
    cat >"$env_file" <<'EOF'
# Optional overrides for the wakterm user service.
# Examples:
# RUST_LOG=wakterm_mux_server_impl=debug,mux=debug
# RUST_BACKTRACE=1
EOF
    echo "Created $env_file"
fi

if systemctl --user daemon-reload >/dev/null 2>&1; then
    systemctl --user enable "$unit_name.service" >/dev/null
    if $start_service; then
        if systemctl --user is-active --quiet "$unit_name.service"; then
            systemctl --user restart "$unit_name.service"
            echo "Restarted $unit_name.service"
        else
            systemctl --user start "$unit_name.service"
            echo "Started $unit_name.service"
        fi
    else
        echo "Enabled $unit_name.service"
    fi
else
    echo "Installed the unit, but could not reach the user systemd manager."
    echo "Run these manually in a normal login shell:"
    echo "  systemctl --user daemon-reload"
    echo "  systemctl --user enable $unit_name.service"
    if $start_service; then
        echo "  systemctl --user restart $unit_name.service"
    fi
fi

echo ""
echo "Notes:"
echo "  - The installed unit runs $bin_path directly under systemd."
echo "  - Optional service env lives in $env_file"
echo "  - Other user services that call 'wakterm cli' should depend on"
echo "    $unit_name.service and use WAKTERM_UNIX_SOCKET=%t/wakterm/sock"
