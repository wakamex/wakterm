#!/bin/bash

set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source_dir=${CARGO_TARGET_DIR:-$repo/target}/release
service_user=mihai
install_root=/
backup_dir=
mode=check

usage() {
    printf '%s\n' \
        "Usage: ./install-system-service.sh [OPTIONS]" \
        "" \
        "Options:" \
        "  --source DIR       Exact release candidate directory" \
        "  --user USER        Service account (default: mihai)" \
        "  --root DIR         Alternate root for isolated rehearsal" \
        "  --backup-dir DIR   Persistent install-artifact backup" \
        "  --apply             Install and enable, but do not start" \
        "  --rollback          Restore a prior --apply backup" \
        "" \
        "Without --apply or --rollback this performs a side-effect-free check."
}

while (($#)); do
    case $1 in
        --source)
            source_dir=$2
            shift 2
            ;;
        --user)
            service_user=$2
            shift 2
            ;;
        --root)
            install_root=$2
            shift 2
            ;;
        --backup-dir)
            backup_dir=$2
            shift 2
            ;;
        --apply)
            mode=apply
            shift
            ;;
        --rollback)
            mode=rollback
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case $install_root in
    /*) ;;
    *)
        printf '%s\n' '--root must be an absolute path' >&2
        exit 2
        ;;
esac

if [[ $install_root == / ]]; then
    root_prefix=
else
    root_prefix=${install_root%/}
fi

service_uid=$(id -u "$service_user")
service_group=$(id -gn "$service_user")
service_home=$(getent passwd "$service_user" | cut -d: -f6)
if [[ -z $service_home ]]; then
    printf 'could not resolve home directory for %s\n' "$service_user" >&2
    exit 1
fi

unit_template=$repo/systemd/wakterm-mux-server-system.service.in
server_config=$repo/systemd/wakterm-mux-server.lua
relative_targets=(
    usr/local/bin/wakterm
    usr/local/bin/wakterm-gui
    usr/local/bin/wakterm-mux-server
    etc/wakterm/mux-server.lua
    etc/systemd/system/wakterm-mux-server.service
)

target_path() {
    printf '%s/%s' "$root_prefix" "$1"
}

render_unit() {
    local destination=$1
    sed \
        -e "s|__USER_ID__|$service_uid|g" \
        -e "s|__USER__|$service_user|g" \
        -e "s|__GROUP__|$service_group|g" \
        -e "s|__HOME__|$service_home|g" \
        "$unit_template" >"$destination"
}

check_candidate() {
    local binary binary_version
    local version=
    for binary in wakterm wakterm-mux-server; do
        test -x "$source_dir/$binary"
        binary_version=$("$source_dir/$binary" --version | awk '{print $2}')
        if [[ -z $version ]]; then
            version=$binary_version
        elif [[ $binary_version != "$version" ]]; then
            printf 'candidate version mismatch: %s has %s, expected %s\n' \
                "$binary" "$binary_version" "$version" >&2
            exit 1
        fi
    done
    test -x "$source_dir/wakterm-gui"
    "$source_dir/wakterm-gui" --version >/dev/null
    test -f "$unit_template"
    test -f "$server_config"
    printf 'candidate_version=%s\n' "$version"
}

verify_unit() {
    local verify_root
    verify_root=$(mktemp -d "${TMPDIR:-/tmp}/wakterm-system-verify.XXXXXX")
    trap 'rm -rf -- "$verify_root"' RETURN
    install -d \
        "$verify_root/bin" \
        "$verify_root/etc/systemd/system" \
        "$verify_root/etc/wakterm" \
        "$verify_root/usr/local/bin"
    install -m 0755 /bin/true "$verify_root/bin/true"
    install -m 0755 /bin/true "$verify_root/usr/local/bin/wakterm-mux-server"
    install -m 0644 /etc/passwd "$verify_root/etc/passwd"
    install -m 0644 /etc/group "$verify_root/etc/group"
    install -m 0644 "$server_config" "$verify_root/etc/wakterm/mux-server.lua"
    render_unit "$verify_root/etc/systemd/system/wakterm-mux-server.service"
    printf '%s\n' \
        '[Unit]' \
        'DefaultDependencies=no' \
        >"$verify_root/etc/systemd/system/multi-user.target"
    printf '%s\n' \
        '[Unit]' \
        'DefaultDependencies=no' \
        >"$verify_root/etc/systemd/system/network.target"
    printf '%s\n' \
        '[Unit]' \
        'DefaultDependencies=no' \
        >"$verify_root/etc/systemd/system/sysinit.target"
    printf '%s\n' \
        '[Unit]' \
        'DefaultDependencies=no' \
        >"$verify_root/etc/systemd/system/basic.target"
    printf '%s\n' \
        '[Unit]' \
        'DefaultDependencies=no' \
        >"$verify_root/etc/systemd/system/shutdown.target"
    printf '%s\n' \
        '[Unit]' \
        'DefaultDependencies=no' \
        '[Service]' \
        'Type=oneshot' \
        'ExecStart=/bin/true' \
        'RemainAfterExit=yes' \
        >"$verify_root/etc/systemd/system/user-runtime-dir@.service"
    systemd-analyze verify --root="$verify_root" wakterm-mux-server.service
    rm -rf -- "$verify_root"
    trap - RETURN
}

restore_targets() {
    local relative target saved absent
    for relative in "${relative_targets[@]}"; do
        target=$(target_path "$relative")
        saved=$backup_dir/files/$relative
        absent=$backup_dir/absent/$relative
        if [[ -e $saved || -L $saved ]]; then
            install -d "$(dirname -- "$target")"
            rm -f -- "$target"
            cp -a -- "$saved" "$target"
        elif [[ -e $absent ]]; then
            rm -f -- "$target"
        else
            printf 'backup is incomplete for %s\n' "$relative" >&2
            return 1
        fi
    done
}

if [[ $mode != rollback ]]; then
    check_candidate
    verify_unit
fi

if [[ $mode == check ]]; then
    sha256sum \
        "$source_dir/wakterm" \
        "$source_dir/wakterm-gui" \
        "$source_dir/wakterm-mux-server" \
        "$unit_template" \
        "$server_config"
    printf '%s\n' 'candidate check passed; no files or services were changed'
    exit 0
fi

if [[ -z $backup_dir || $backup_dir != /* || $backup_dir == / ]]; then
    printf '%s\n' '--backup-dir must be a non-root absolute path for apply or rollback' >&2
    exit 2
fi

if [[ $install_root == / && $EUID -ne 0 ]]; then
    printf '%s\n' 'production apply and rollback require sudo' >&2
    exit 2
fi

if [[ $mode == rollback ]]; then
    test -f "$backup_dir/wakterm-system-install.backup"
    if [[ $install_root == / ]] && systemctl is-active --quiet wakterm-mux-server.service; then
        printf '%s\n' 'stop the system Wakterm service before rollback' >&2
        exit 1
    fi
    restore_targets
    if [[ $install_root == / ]]; then
        systemctl daemon-reload
        restorecon -Fi \
            /usr/local/bin/wakterm \
            /usr/local/bin/wakterm-gui \
            /usr/local/bin/wakterm-mux-server \
            /etc/wakterm/mux-server.lua \
            /etc/systemd/system/wakterm-mux-server.service
    fi
    printf '%s\n' 'install artifacts restored; no service was started'
    exit 0
fi

if [[ -e $backup_dir ]]; then
    printf 'backup directory already exists: %s\n' "$backup_dir" >&2
    exit 1
fi

if [[ $install_root == / ]]; then
    if systemctl --user --machine="$service_user@" is-active --quiet wakterm-mux-server.service 2>/dev/null; then
        printf '%s\n' 'the user Wakterm service is still active' >&2
        exit 1
    fi
    if systemctl --user --machine="$service_user@" is-active --quiet panetone.service 2>/dev/null; then
        printf '%s\n' 'the Python Panetone service is still active' >&2
        exit 1
    fi
    if systemctl is-active --quiet wakterm-mux-server.service; then
        printf '%s\n' 'the system Wakterm service is already active' >&2
        exit 1
    fi
fi

install -d -m 0700 "$backup_dir/files" "$backup_dir/absent"
for relative in "${relative_targets[@]}"; do
    target=$(target_path "$relative")
    if [[ -e $target || -L $target ]]; then
        install -d "$backup_dir/files/$(dirname -- "$relative")"
        cp -a -- "$target" "$backup_dir/files/$relative"
    else
        install -d "$backup_dir/absent/$(dirname -- "$relative")"
        touch "$backup_dir/absent/$relative"
    fi
done
touch "$backup_dir/wakterm-system-install.backup"
render_unit "$backup_dir/wakterm-mux-server.service"

install_succeeded=0
rollback_failed_install() {
    if ((install_succeeded == 0)); then
        restore_targets || true
        if [[ $install_root == / ]]; then
            systemctl daemon-reload || true
        fi
    fi
}
trap rollback_failed_install EXIT
trap 'exit 130' HUP INT TERM

install -Dm0755 "$source_dir/wakterm" "$(target_path usr/local/bin/wakterm)"
install -Dm0755 "$source_dir/wakterm-gui" "$(target_path usr/local/bin/wakterm-gui)"
install -Dm0755 "$source_dir/wakterm-mux-server" \
    "$(target_path usr/local/bin/wakterm-mux-server)"
install -Dm0644 "$server_config" "$(target_path etc/wakterm/mux-server.lua)"
install -Dm0644 "$backup_dir/wakterm-mux-server.service" \
    "$(target_path etc/systemd/system/wakterm-mux-server.service)"

if [[ $install_root == / ]]; then
    restorecon -F \
        /usr/local/bin/wakterm \
        /usr/local/bin/wakterm-gui \
        /usr/local/bin/wakterm-mux-server \
        /etc/wakterm/mux-server.lua \
        /etc/systemd/system/wakterm-mux-server.service
    systemctl daemon-reload
fi

install_succeeded=1
trap - EXIT HUP INT TERM
sha256sum \
    "$(target_path usr/local/bin/wakterm)" \
    "$(target_path usr/local/bin/wakterm-gui)" \
    "$(target_path usr/local/bin/wakterm-mux-server)" \
    "$(target_path etc/systemd/system/wakterm-mux-server.service)" \
    "$(target_path etc/wakterm/mux-server.lua)"
printf '%s\n' 'candidate installed but not enabled or started; retain the backup for rollback'
