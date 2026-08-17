#!/bin/bash

set -euo pipefail

readonly candidate_commit=cd0a9225c5a9f60fccde69796f8f0c52fa47ba4b
readonly candidate_short=cd0a9225
readonly service_user=mihai
readonly user_wakterm=/home/mihai/.local/bin/wakterm
readonly system_wakterm=/usr/local/bin/wakterm
readonly user_socket=/run/user/1000/wakterm/sock
readonly system_socket=/run/wakterm/sock
readonly runtime_session=/run/user/1000/wakterm/session.json
readonly agent_database=/home/mihai/.local/share/wakterm/agent-requests.sqlite3
readonly panetone_pending=/home/mihai/.config/wez-tg/pending_sends.json
readonly panetone_journal=/home/mihai/.config/wez-tg/control-journal.sqlite3
readonly panetone_dropin=/home/mihai/.config/systemd/user/panetone.service.d/wakterm-system.conf
readonly user_mux_guard=/home/mihai/.config/systemd/user/wakterm-mux-server.service.d/system-service.conf

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
mode=check
backup_dir=
assume_yes=0
maintenance_started=0
user_mux_stopped=0
installer_applied=0
source_worktree=
build_root=
release_dir=

usage() {
    printf '%s\n' \
        "Usage: ./promote-system-service.sh [OPTIONS]" \
        "" \
        "Options:" \
        "  --check                 Run read-only readiness checks (default)" \
        "  --apply                 Migrate to the reviewed system service" \
        "  --resume BACKUP_DIR     Finish an interrupted post-install migration" \
        "  --rollback BACKUP_DIR   Restore the previous user service" \
        "  --backup-dir DIR        Backup directory for --apply" \
        "  --yes                   Skip the typed confirmation" \
        "  -h, --help              Show this help" \
        "" \
        "Run --apply and --rollback from an independent SSH or console shell." \
        "Stopping the mux would also stop a script running inside a Wakterm pane."
}

while (($#)); do
    case $1 in
        --check)
            mode=check
            shift
            ;;
        --apply)
            mode=apply
            shift
            ;;
        --resume)
            [[ $# -ge 2 ]] || { usage >&2; exit 2; }
            mode=resume
            backup_dir=$2
            shift 2
            ;;
        --rollback)
            [[ $# -ge 2 ]] || { usage >&2; exit 2; }
            mode=rollback
            backup_dir=$2
            shift 2
            ;;
        --backup-dir)
            [[ $# -ge 2 ]] || { usage >&2; exit 2; }
            backup_dir=$2
            shift 2
            ;;
        --yes)
            assume_yes=1
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

log() {
    printf '[wakterm-maintenance] %s\n' "$*"
}

die() {
    printf '[wakterm-maintenance] ERROR: %s\n' "$*" >&2
    exit 1
}

need_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command is missing: $1"
}

user_cli() {
    env WAKTERM_UNIX_SOCKET=$user_socket \
        "$user_wakterm" cli --no-auto-start --prefer-mux "$@"
}

system_cli() {
    env WAKTERM_UNIX_SOCKET=$system_socket \
        "$system_wakterm" cli --no-auto-start --prefer-mux "$@"
}

cleanup_build_worktree() {
    if [[ -n $source_worktree ]] && git -C "$repo" worktree list --porcelain \
        | grep -Fqx "worktree $source_worktree"; then
        git -C "$repo" worktree remove --force "$source_worktree" >/dev/null 2>&1 || true
    fi
    if [[ -n $build_root && -d $build_root ]]; then
        find "$build_root" -depth -delete >/dev/null 2>&1 || true
    fi
}

on_exit() {
    local status=$?
    cleanup_build_worktree
    if ((status != 0 && maintenance_started == 1)); then
        systemctl --user stop panetone.service >/dev/null 2>&1 || true
        if ((installer_applied == 0)); then
            if ((user_mux_stopped == 1)); then
                systemctl --user start wakterm-mux-server.service >/dev/null 2>&1 || true
            fi
            systemctl --user start panetone.service >/dev/null 2>&1 || true
            printf '%s\n' \
                '[wakterm-maintenance] Migration stopped before candidate installation.' \
                '[wakterm-maintenance] The previous user mux and Panetone were restarted.'
        else
            printf '%s\n' \
                '[wakterm-maintenance] Migration stopped after candidate installation.' \
                '[wakterm-maintenance] Panetone has been left stopped.' \
                '[wakterm-maintenance] After correcting a verification failure, continue with:' \
                "[wakterm-maintenance]   $repo/promote-system-service.sh --resume $backup_dir" \
                '[wakterm-maintenance] Or roll back with:' \
                "[wakterm-maintenance]   $repo/promote-system-service.sh --rollback $backup_dir"
        fi
    fi
    exit "$status"
}
trap on_exit EXIT

require_independent_shell() {
    if grep -Fq 'wakterm-mux-server.service' /proc/$$/cgroup; then
        die "run this operation from a separate SSH or console shell; stopping the mux would kill this script"
    fi
}

wait_for_unit() {
    local scope=$1
    local unit=$2
    local timeout_seconds=$3
    local elapsed=0
    while ! systemctl $scope is-active --quiet "$unit"; do
        ((elapsed < timeout_seconds)) || return 1
        sleep 1
        ((elapsed += 1))
    done
}

wait_for_socket() {
    local path=$1
    local timeout_seconds=$2
    local elapsed=0
    while [[ ! -S $path ]]; do
        ((elapsed < timeout_seconds)) || return 1
        sleep 1
        ((elapsed += 1))
    done
}

validate_backup_path() {
    [[ $backup_dir == /var/tmp/wakterm-system-backup-* ]] \
        || die "backup directory must be an explicit /var/tmp/wakterm-system-backup-* path"
    [[ $backup_dir != *".."* ]] || die "backup directory must not contain '..'"
}

check_panetone_queues() {
    local pending_items=0
    local pending_controls=0
    local pending_returns=0

    if [[ -f $panetone_pending ]]; then
        pending_items=$(jq '
            if type == "array" then length
            else ((.items // .pending // []) | length)
            end
        ' "$panetone_pending")
    fi
    ((pending_items == 0)) \
        || die "Panetone still has $pending_items queued outbound chunks"

    if [[ -f $panetone_journal ]]; then
        pending_controls=$(sqlite3 "$panetone_journal" \
            "select count(*) from control_request where state not in ('succeeded','failed','indeterminate');")
        pending_returns=$(sqlite3 "$panetone_journal" \
            "select count(*) from return_delivery where state != 'terminal' or agent_state != 'delivered' or telegram_state != 'delivered';")
    fi
    ((pending_controls == 0)) \
        || die "Panetone still has $pending_controls nonterminal control requests"
    ((pending_returns == 0)) \
        || die "Panetone still has $pending_returns incomplete return deliveries"
}

check_wakterm_requests() {
    local pending=0
    [[ -f $agent_database ]] || return 0
    pending=$(sqlite3 "$agent_database" \
        "select count(*) from agent_request where json_extract(snapshot_json,'$.state') in ('registered','submitted','bound');")
    ((pending == 0)) || die "Wakterm still has $pending nonterminal return requests"
}

show_agents() {
    local agents_json=$1
    jq -r '
        .[]
        | "agent=" + .metadata.name
          + " pane=" + (.pane_id | tostring)
          + " harness=" + (.runtime.harness | tostring)
          + " status=" + (.runtime.status | tostring)
          + " cwd=" + .metadata.declared_cwd
    ' <<<"$agents_json"
}

readiness_check() {
    local agents_json
    git -C "$repo" cat-file -e "$candidate_commit^{commit}" \
        || die "candidate commit is unavailable in $repo"
    [[ -x $user_wakterm ]] || die "current Wakterm CLI is missing: $user_wakterm"
    [[ -S $user_socket ]] || die "current Wakterm socket is missing: $user_socket"
    systemctl --user is-active --quiet wakterm-mux-server.service \
        || die "the user Wakterm service is not active"
    ! systemctl is-active --quiet wakterm-mux-server.service \
        || die "the system Wakterm service is already active"
    systemctl --user is-active --quiet panetone.service \
        || die "the Panetone service is not active"

    agents_json=$(user_cli agent list --format json)
    check_wakterm_requests
    check_panetone_queues
    show_agents "$agents_json"
    log "readiness checks passed; agent processes will be stopped and may be restarted manually"
}

build_candidate() {
    local target_dir=${WAKTERM_PROMOTION_TARGET_DIR:-$repo/target}
    build_root=$(mktemp -d /var/tmp/wakterm-candidate-build.XXXXXX)
    source_worktree=$build_root/source
    git -C "$repo" worktree add --detach "$source_worktree" "$candidate_commit"
    (
        cd "$source_worktree"
        CARGO_TARGET_DIR=$target_dir cargo build --release \
            -p wakterm -p wakterm-gui -p wakterm-mux-server
    )

    release_dir=$target_dir/release
    for binary in wakterm wakterm-gui wakterm-mux-server; do
        [[ -x $release_dir/$binary ]] || die "candidate build omitted $binary"
    done
    "$release_dir/wakterm" --version | grep -Fq "$candidate_short" \
        || die "candidate CLI version does not identify $candidate_short"
    "$release_dir/wakterm-mux-server" --version | grep -Fq "$candidate_short" \
        || die "candidate server version does not identify $candidate_short"
    /bin/bash "$source_worktree/install-system-service.sh" --source "$release_dir"
}

make_backup() {
    local maintenance=$backup_dir/maintenance
    local release_backup=$backup_dir/release
    local candidate_backup=$backup_dir/candidate
    local previous_enablement

    validate_backup_path
    [[ ! -e $backup_dir ]] || die "backup directory already exists: $backup_dir"
    install -d -m 0700 "$backup_dir" "$maintenance" "$release_backup" "$candidate_backup"

    install -m 0755 "$release_dir/wakterm" "$release_backup/wakterm"
    install -m 0755 "$release_dir/wakterm-gui" "$release_backup/wakterm-gui"
    install -m 0755 "$release_dir/wakterm-mux-server" "$release_backup/wakterm-mux-server"
    install -m 0755 "$source_worktree/install-system-service.sh" \
        "$candidate_backup/install-system-service.sh"
    cp -a "$source_worktree/systemd" "$candidate_backup/systemd"

    user_cli save-layout "$maintenance/layout.json"
    user_cli list --format json >"$maintenance/panes-before.json"
    user_cli agent list --format json >"$maintenance/agents-before.json"
    user_cli list-clients --format json >"$maintenance/clients-before.json"
    systemctl --user cat wakterm-mux-server.service >"$maintenance/wakterm-user-unit.txt"
    systemctl --user cat panetone.service >"$maintenance/panetone-user-unit.txt"
    systemctl --user show wakterm-mux-server.service >"$maintenance/wakterm-user-state.txt"
    systemctl --user show panetone.service >"$maintenance/panetone-user-state.txt"
    previous_enablement=$(systemctl --user is-enabled wakterm-mux-server.service 2>/dev/null || true)
    printf '%s\n' "$previous_enablement" >"$maintenance/wakterm-user-enablement.txt"

    if [[ -f $agent_database ]]; then
        sqlite3 "$agent_database" ".backup '$maintenance/agent-requests.sqlite3'"
        [[ $(sqlite3 "$maintenance/agent-requests.sqlite3" 'pragma quick_check;') == ok ]] \
            || die "Agent API backup failed quick_check"
    fi
    if [[ -f $runtime_session ]]; then
        install -m 0600 "$runtime_session" "$maintenance/session-before-stop.json"
    fi
    install -m 0755 "$user_wakterm" "$maintenance/wakterm-user-binary"
    if [[ -f $panetone_dropin ]]; then
        install -m 0644 "$panetone_dropin" "$maintenance/panetone-wakterm-system.conf"
    else
        touch "$maintenance/panetone-dropin-was-absent"
    fi
    if [[ -f $user_mux_guard ]]; then
        install -m 0644 "$user_mux_guard" "$maintenance/user-mux-system-service.conf"
    else
        touch "$maintenance/user-mux-guard-was-absent"
    fi

    sha256sum "$release_backup"/* >"$maintenance/candidate-sha256.txt"
    sha256sum "$user_wakterm" >"$maintenance/previous-user-binary-sha256.txt"
    printf '%s\n' "$candidate_commit" >"$maintenance/candidate-commit.txt"
    printf '%s\n' "$backup_dir" >"$maintenance/backup-directory.txt"
}

install_user_proxy_wrapper() {
    local wrapper
    wrapper=$(mktemp /home/mihai/.local/bin/wakterm.system-proxy.XXXXXX)
    printf '%s\n' \
        '#!/bin/bash' \
        ': "${WAKTERM_UNIX_SOCKET:=/run/wakterm/sock}"' \
        'export WAKTERM_UNIX_SOCKET' \
        'exec /usr/local/bin/wakterm "$@"' \
        >"$wrapper"
    chmod 0755 "$wrapper"
    mv -f "$wrapper" "$user_wakterm"
}

configure_panetone_for_system_mux() {
    local dropin_dir
    local temporary
    dropin_dir=$(dirname -- "$panetone_dropin")
    install -d -m 0755 "$dropin_dir"
    temporary=$(mktemp "$dropin_dir/.wakterm-system.XXXXXX")
    printf '%s\n' \
        '[Unit]' \
        'After=' \
        'Wants=' \
        '' \
        '[Service]' \
        'Environment=WAKTERM_BIN=/usr/local/bin/wakterm' \
        'Environment=WAKTERM_UNIX_SOCKET=/run/wakterm/sock' \
        >"$temporary"
    chmod 0644 "$temporary"
    mv -f "$temporary" "$panetone_dropin"
    systemctl --user daemon-reload
}

ensure_user_mux_guard_backup() {
    local maintenance=$backup_dir/maintenance
    if [[ -f $maintenance/user-mux-system-service.conf \
        || -f $maintenance/user-mux-guard-was-absent ]]; then
        return 0
    fi
    if [[ -f $user_mux_guard ]]; then
        install -m 0644 "$user_mux_guard" "$maintenance/user-mux-system-service.conf"
    else
        touch "$maintenance/user-mux-guard-was-absent"
    fi
}

configure_user_mux_guard() {
    local guard_dir
    local temporary
    ensure_user_mux_guard_backup
    guard_dir=$(dirname -- "$user_mux_guard")
    install -d -m 0755 "$guard_dir"
    temporary=$(mktemp "$guard_dir/.system-service.XXXXXX")
    printf '%s\n' \
        '[Unit]' \
        'ConditionPathExists=!/etc/systemd/system/wakterm-mux-server.service' \
        >"$temporary"
    chmod 0644 "$temporary"
    mv -f "$temporary" "$user_mux_guard"
    systemctl --user daemon-reload
}

verify_system_mux() {
    local capabilities
    local socket_mode
    systemctl is-active --quiet wakterm-mux-server.service \
        || die "the system Wakterm service is not active"
    ! systemctl --user is-active --quiet wakterm-mux-server.service \
        || die "the old user Wakterm service is still active"
    [[ -S $system_socket ]] || die "the system Wakterm socket is missing"
    [[ $(stat -c '%U' "$system_socket") == "$service_user" ]] \
        || die "the system Wakterm socket has the wrong owner"
    socket_mode=$(stat -c '%a' "$system_socket")
    (( (8#$socket_mode & 8#77) == 0 )) \
        || die "the system Wakterm socket grants group or other access: mode $socket_mode"
    "$system_wakterm" --version | grep -Fq "$candidate_short" \
        || die "the installed Wakterm CLI is not candidate $candidate_short"

    capabilities=$(system_cli agent capabilities)
    jq -e '
        .schema == "wakterm.agent-api.v1"
        and (.capabilities | contains([
            "catalog.v1",
            "prompt_admission.v1",
            "return_request_terminal_stream.v1",
            "event_stream.v1"
        ]))
    ' <<<"$capabilities" >/dev/null || die "the Agent API capability check failed"
}

complete_migration() {
    configure_user_mux_guard
    verify_system_mux
    system_cli list --format json >"$backup_dir/maintenance/panes-after.json"
    system_cli agent capabilities >"$backup_dir/maintenance/capabilities-after.json"

    log "starting Panetone on the system socket"
    check_panetone_queues
    systemctl --user start panetone.service
    wait_for_unit --user panetone.service 30 \
        || die "Panetone did not become active"
    systemctl --user show panetone.service -p Environment --value \
        | grep -Fq 'WAKTERM_UNIX_SOCKET=/run/wakterm/sock' \
        || die "Panetone did not load the system socket setting"
    ! systemctl --user is-active --quiet wakterm-mux-server.service \
        || die "starting Panetone restarted the old user mux"
    systemctl --user reset-failed wakterm-mux-server.service >/dev/null 2>&1 || true

    printf '%s\n' "completed_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        >"$backup_dir/maintenance/migration-complete"
    maintenance_started=0
    log "migration complete; backup retained at $backup_dir"
    log "reconnect Wakterm and manually restart wakterm_codex and panetone_codex when wanted"
}

confirm_apply() {
    local answer
    ((assume_yes == 1)) && return 0
    printf '%s\n' \
        "This will stop the current mux and its agent processes, install $candidate_short," \
        "start the system mux on $system_socket, and restart Panetone." \
        "Wakterm will restore shell layout, but you will restart any desired agents manually."
    read -r -p 'Type APPLY to continue: ' answer
    [[ $answer == APPLY ]] || die "confirmation declined"
}

apply_migration() {
    require_independent_shell
    readiness_check
    log "building exact candidate $candidate_commit before downtime"
    build_candidate
    readiness_check
    sudo -v
    confirm_apply

    if [[ -z $backup_dir ]]; then
        backup_dir=/var/tmp/wakterm-system-backup-$(date -u +%Y%m%dT%H%M%SZ)
    fi
    validate_backup_path
    make_backup
    maintenance_started=1

    log "stopping Panetone and checking its durable queues"
    systemctl --user stop panetone.service
    check_panetone_queues

    log "stopping the user Wakterm mux"
    systemctl --user stop wakterm-mux-server.service
    user_mux_stopped=1
    ! systemctl --user is-active --quiet wakterm-mux-server.service \
        || die "the user Wakterm service did not stop"
    if [[ -f $runtime_session ]]; then
        install -m 0600 "$runtime_session" \
            "$backup_dir/maintenance/session-final-user.json"
    fi

    log "installing exact candidate artifacts"
    sudo /bin/bash "$backup_dir/candidate/install-system-service.sh" \
        --source "$backup_dir/release" \
        --backup-dir "$backup_dir/install-artifacts" \
        --apply
    installer_applied=1

    install_user_proxy_wrapper
    configure_panetone_for_system_mux
    systemctl --user disable wakterm-mux-server.service >/dev/null

    log "starting and verifying the system Wakterm mux"
    sudo systemctl enable --now wakterm-mux-server.service
    wait_for_unit '' wakterm-mux-server.service 30 \
        || die "the system Wakterm service did not become active"
    wait_for_socket "$system_socket" 30 \
        || die "the system Wakterm socket did not appear"
    complete_migration
}

resume_migration() {
    validate_backup_path
    [[ -f $backup_dir/maintenance/candidate-commit.txt ]] \
        || die "not a Wakterm maintenance backup: $backup_dir"
    [[ $(<"$backup_dir/maintenance/candidate-commit.txt") == "$candidate_commit" ]] \
        || die "the maintenance backup belongs to another candidate"
    [[ -f $backup_dir/install-artifacts/wakterm-system-install.backup ]] \
        || die "the system-service installation backup is incomplete"
    ! systemctl --user is-active --quiet wakterm-mux-server.service \
        || die "the old user Wakterm service is active"
    systemctl is-active --quiet wakterm-mux-server.service \
        || die "the system Wakterm service is not active"

    maintenance_started=1
    installer_applied=1
    complete_migration
}

restore_panetone_dropin() {
    if [[ -f $backup_dir/maintenance/panetone-wakterm-system.conf ]]; then
        install -d -m 0755 "$(dirname -- "$panetone_dropin")"
        install -m 0644 "$backup_dir/maintenance/panetone-wakterm-system.conf" \
            "$panetone_dropin"
    else
        [[ -f $backup_dir/maintenance/panetone-dropin-was-absent ]] \
            || die "Panetone drop-in backup is incomplete"
        if [[ -f $panetone_dropin ]]; then
            rm -f -- "$panetone_dropin"
        fi
    fi
    systemctl --user daemon-reload
}

restore_user_mux_guard() {
    if [[ -f $backup_dir/maintenance/user-mux-system-service.conf ]]; then
        install -d -m 0755 "$(dirname -- "$user_mux_guard")"
        install -m 0644 "$backup_dir/maintenance/user-mux-system-service.conf" \
            "$user_mux_guard"
    else
        [[ -f $backup_dir/maintenance/user-mux-guard-was-absent ]] \
            || die "user mux guard backup is incomplete"
        if [[ -f $user_mux_guard ]]; then
            rm -f -- "$user_mux_guard"
        fi
    fi
    systemctl --user daemon-reload
}

restore_user_cli() {
    local temporary
    temporary=$(mktemp /home/mihai/.local/bin/wakterm.rollback.XXXXXX)
    install -m 0755 "$backup_dir/maintenance/wakterm-user-binary" "$temporary"
    mv -f "$temporary" "$user_wakterm"
}

confirm_rollback() {
    local answer
    ((assume_yes == 1)) && return 0
    printf '%s\n' \
        "This will stop the system mux and Panetone, restore the maintenance snapshot," \
        "then restart the previous user mux and Panetone. Agent processes must be" \
        "restarted manually."
    read -r -p 'Type ROLLBACK to continue: ' answer
    [[ $answer == ROLLBACK ]] || die "confirmation declined"
}

rollback_migration() {
    local previous_enablement
    require_independent_shell
    validate_backup_path
    [[ -f $backup_dir/maintenance/candidate-commit.txt ]] \
        || die "not a Wakterm maintenance backup: $backup_dir"
    [[ -x $backup_dir/candidate/install-system-service.sh ]] \
        || die "the saved installer is missing"
    [[ -x $backup_dir/maintenance/wakterm-user-binary ]] \
        || die "the previous user CLI is missing"
    sudo -v
    confirm_rollback

    maintenance_started=1
    installer_applied=1
    systemctl --user stop panetone.service
    sudo systemctl disable --now wakterm-mux-server.service || true
    sudo /bin/bash "$backup_dir/candidate/install-system-service.sh" \
        --backup-dir "$backup_dir/install-artifacts" \
        --rollback

    restore_user_cli
    restore_panetone_dropin
    restore_user_mux_guard
    if [[ -f $backup_dir/maintenance/agent-requests.sqlite3 ]]; then
        install -m 0600 "$backup_dir/maintenance/agent-requests.sqlite3" "$agent_database"
    fi

    previous_enablement=$(<"$backup_dir/maintenance/wakterm-user-enablement.txt")
    if [[ $previous_enablement == enabled ]]; then
        systemctl --user enable wakterm-mux-server.service >/dev/null
    fi
    systemctl --user start wakterm-mux-server.service
    wait_for_unit --user wakterm-mux-server.service 30 \
        || die "the previous user Wakterm service did not become active"
    wait_for_socket "$user_socket" 30 \
        || die "the previous user Wakterm socket did not appear"

    systemctl --user start panetone.service
    wait_for_unit --user panetone.service 30 \
        || die "Panetone did not become active after rollback"
    maintenance_started=0
    log "rollback complete; reconnect Wakterm and restart desired agents manually"
}

for command in cargo date find git grep install jq mktemp mv sha256sum sqlite3 stat systemctl; do
    need_command "$command"
done
[[ $(id -un) == "$service_user" ]] || die "run this script as $service_user"

case $mode in
    check)
        readiness_check
        ;;
    apply)
        apply_migration
        ;;
    resume)
        resume_migration
        ;;
    rollback)
        rollback_migration
        ;;
esac
