#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR"
owner_user="${SUDO_USER:-$USER}"
owner_home="$(getent passwd "$owner_user" | cut -d: -f6)"
source_dir="${SOURCE_DIR:-${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release}"
mode="user"
desktop=false
user_prefix_default="${owner_home}/.local/bin"
system_prefix_default="/usr/local/bin"
prefix="${PREFIX:-$user_prefix_default}"
prefix_explicit=false

usage() {
    echo "Usage: ./install.sh [--user|--system] [--desktop] [--source DIR] [--prefix DIR]"
    echo ""
    echo "  --user        Install into ~/.local/bin (default)"
    echo "  --system      Install into /usr/local/bin (requires sudo)"
    echo "  --desktop     Also install the application launcher and icon (Linux)"
    echo "  --source DIR  Install from this directory (default: $source_dir)"
    echo "  --prefix DIR  Install into this directory (default depends on mode)"
    echo ""
    echo "Examples:"
    echo "  ./install.sh"
    echo "  ./install.sh --desktop"
    echo "  ./install.sh --system"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --user)
            mode="user"
            shift
            ;;
        --system)
            mode="system"
            shift
            ;;
        --desktop)
            desktop=true
            shift
            ;;
        --source)
            source_dir="$2"
            shift 2
            ;;
        --prefix)
            prefix="$2"
            prefix_explicit=true
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

if ! $prefix_explicit; then
    if [ "$mode" = "system" ]; then
        prefix="$system_prefix_default"
    else
        prefix="$user_prefix_default"
    fi
fi

if [ "$mode" = "system" ]; then
    if [ "${EUID:-$(id -u)}" -ne 0 ]; then
        echo "--system installs require sudo."
        exit 1
    fi
else
    if [ "${EUID:-$(id -u)}" -eq 0 ]; then
        echo "User installs should be run without sudo."
        exit 1
    fi
fi

if $desktop && [ "$(uname -s)" != "Linux" ]; then
    echo "--desktop is supported only on Linux."
    exit 1
fi

mkdir -p "$prefix"
prefix="$(cd "$prefix" && pwd)"
if $desktop; then
    case "$prefix" in
        *%*|*$'\n'*|*$'\r'*)
            echo "--desktop requires an install path without percent signs or line breaks."
            exit 1
            ;;
    esac
fi

echo "Installing binaries from $source_dir to $prefix ($mode mode)"
for bin in wakterm wakterm-gui wakterm-mux-server; do
    if [ ! -x "$source_dir/$bin" ]; then
        echo "Missing executable: $source_dir/$bin"
        exit 1
    fi
    install -Dm755 "$source_dir/$bin" "$prefix/$bin"
    echo "  $bin -> $prefix/$bin"
done

if $desktop; then
    if [ "$mode" = "system" ]; then
        data_dir="/usr/local/share"
    else
        data_dir="${XDG_DATA_HOME:-$owner_home/.local/share}"
    fi
    desktop_file="$data_dir/applications/org.wezfurlong.wakterm.desktop"
    icon_file="$data_dir/icons/hicolor/192x192/apps/org.wezfurlong.wakterm.png"

    # Desktop Exec values have two escaping layers: quoted arguments, then
    # desktop-entry string escapes.
    desktop_exec="$prefix/wakterm"
    desktop_exec="${desktop_exec//\\/\\\\}"
    desktop_exec="${desktop_exec//\"/\\\"}"
    desktop_exec="${desktop_exec//\`/\\\`}"
    desktop_exec="${desktop_exec//\$/\\\$}"
    desktop_exec="${desktop_exec//\\/\\\\}"

    install -Dm644 "$REPO_ROOT/assets/icon/wakterm-icon.png" "$icon_file"
    old_icon="$data_dir/icons/hicolor/scalable/apps/org.wezfurlong.wakterm.svg"
    if cmp -s "$old_icon" "$REPO_ROOT/assets/icon/wakterm-icon.svg"; then
        rm "$old_icon"
    fi
    install -Dm644 "$REPO_ROOT/assets/wakterm.desktop" "$desktop_file"
    while IFS= read -r line; do
        case "$line" in
            Name=*) printf '%s\n' 'Name=Wakterm' ;;
            TryExec=*) ;; # Exec uses an absolute path, independent of GUI PATH.
            Exec=*) printf 'Exec="%s" start\n' "$desktop_exec" ;;
            *) printf '%s\n' "$line" ;;
        esac
    done < "$REPO_ROOT/assets/wakterm.desktop" > "$desktop_file"
    echo "  desktop launcher -> $desktop_file"
    echo "  icon -> $icon_file"
fi

legacy_agent_shim="$prefix/agent"
legacy_agent_body="$(printf '#!/usr/bin/env bash\nexec "%s/wakterm" cli agent "$@"' "$prefix")"
if [ -f "$legacy_agent_shim" ] && [ ! -L "$legacy_agent_shim" ] && \
    [ "$(<"$legacy_agent_shim")" = "$legacy_agent_body" ]; then
    rm "$legacy_agent_shim"
    echo "  removed legacy Wakterm agent shim: $legacy_agent_shim"
fi

echo ""
echo "Installed versions:"
"$prefix/wakterm" --version
"$prefix/wakterm-mux-server" --version
echo ""
echo "To install and enable the standalone user service:"
echo "  ./install-user-service.sh --bin $prefix/wakterm-mux-server"
