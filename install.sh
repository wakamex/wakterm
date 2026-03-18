#!/usr/bin/env bash
set -euo pipefail

owner_user="${SUDO_USER:-$USER}"
owner_home="$(getent passwd "$owner_user" | cut -d: -f6)"
source_dir="${SOURCE_DIR:-$owner_home/wezterm-test}"
prefix="${PREFIX:-/usr/local/bin}"

usage() {
    echo "Usage: sudo ./install.sh [--source DIR] [--prefix DIR]"
    echo ""
    echo "  --source DIR  Install from this directory (default: $source_dir)"
    echo "  --prefix DIR  Install into this directory (default: $prefix)"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --source)
            source_dir="$2"
            shift 2
            ;;
        --prefix)
            prefix="$2"
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

if [ "${EUID:-$(id -u)}" -ne 0 ]; then
    echo "Run this script with sudo."
    exit 1
fi

echo "Installing binaries from $source_dir to $prefix"
for bin in wezterm wezterm-gui wezterm-mux-server; do
    if [ ! -x "$source_dir/$bin" ]; then
        echo "Missing executable: $source_dir/$bin"
        exit 1
    fi
    install -Dm755 "$source_dir/$bin" "$prefix/$bin"
    echo "  $bin -> $prefix/$bin"
done

cat >"$prefix/agent" <<EOF
#!/usr/bin/env bash
exec "$prefix/wezterm" cli agent "\$@"
EOF
chmod 755 "$prefix/agent"
echo "  agent -> $prefix/agent"

echo ""
echo "Installed versions:"
"$prefix/wezterm" --version
"$prefix/wezterm-mux-server" --version
