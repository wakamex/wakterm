#!/usr/bin/env bash
set -euo pipefail

owner_user="${SUDO_USER:-$USER}"
owner_home="$(getent passwd "$owner_user" | cut -d: -f6)"
source_dir="${SOURCE_DIR:-$owner_home/wakterm-test}"
mode="user"
user_prefix_default="${owner_home}/.local/bin"
system_prefix_default="/usr/local/bin"
prefix="${PREFIX:-$user_prefix_default}"
prefix_explicit=false

usage() {
    echo "Usage: ./install.sh [--user|--system] [--source DIR] [--prefix DIR]"
    echo ""
    echo "  --user        Install into ~/.local/bin (default)"
    echo "  --system      Install into /usr/local/bin (requires sudo)"
    echo "  --source DIR  Install from this directory (default: $source_dir)"
    echo "  --prefix DIR  Install into this directory (default depends on mode)"
    echo ""
    echo "Examples:"
    echo "  ./install.sh"
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

mkdir -p "$prefix"

echo "Installing binaries from $source_dir to $prefix ($mode mode)"
for bin in wakterm wakterm-gui wakterm-mux-server; do
    if [ ! -x "$source_dir/$bin" ]; then
        echo "Missing executable: $source_dir/$bin"
        exit 1
    fi
    install -Dm755 "$source_dir/$bin" "$prefix/$bin"
    echo "  $bin -> $prefix/$bin"
done

cat >"$prefix/agent" <<EOF
#!/usr/bin/env bash
exec "$prefix/wakterm" cli agent "\$@"
EOF
chmod 755 "$prefix/agent"
echo "  agent -> $prefix/agent"

echo ""
echo "Installed versions:"
"$prefix/wakterm" --version
"$prefix/wakterm-mux-server" --version
echo ""
echo "To install and enable the standalone user service:"
echo "  ./install-user-service.sh --bin $prefix/wakterm-mux-server"
