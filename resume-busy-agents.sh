#!/usr/bin/env bash
# Asks the agents that were busy before a mux restart to continue their
# interrupted turns, and asks the agent working on this repository to check
# that the deploy went well. deploy.sh --notify starts this as a transient
# user unit before it restarts the mux, so it outlives a deploy run from an
# agent pane.
#
# Usage: resume-busy-agents.sh SNAPSHOT_DIR OLD_MUX_PID REPO_ROOT
#
# SNAPSHOT_DIR holds agents-before-restart.json; the log goes to
# SNAPSHOT_DIR/resume-busy-agents.log. The verifier is the one live agent
# whose working directory is REPO_ROOT; with none or several, no check is
# requested.
set -uo pipefail

SNAPSHOT_DIR="$1"
OLD_PID="$2"
REPO_ROOT="${3:-}"
WAKTERM="${WAKTERM:-$HOME/.local/bin/wakterm}"
LOG="$SNAPSHOT_DIR/resume-busy-agents.log"
RESUME="Wakterm's mux server restarted to deploy an update, which interrupted your turn. Continue where you left off."
DEADLINE=$((SECONDS + 900))

log() { echo "$(date -Is) $*" >>"$LOG"; }

# Prints the name of every alive agent in an agent list read from stdin whose
# status is $1.
agents_with_status() {
    python3 -c '
import json, sys
agents = json.load(sys.stdin)
agents = agents.get("agents", agents) if isinstance(agents, dict) else agents
for agent in agents:
    runtime = agent.get("runtime", {})
    if runtime.get("alive") and str(runtime.get("status")).lower() == sys.argv[1]:
        print(agent["metadata"]["name"])
' "$1"
}

# Prints the name of the one alive agent in an agent list read from stdin
# whose working directory is $1, and nothing when there is none or several.
agent_in_directory() {
    python3 -c '
import json, os, sys
agents = json.load(sys.stdin)
agents = agents.get("agents", agents) if isinstance(agents, dict) else agents
root = os.path.normpath(sys.argv[1])
names = [
    agent["metadata"]["name"]
    for agent in agents
    if agent.get("runtime", {}).get("alive")
    and os.path.normpath(agent["metadata"].get("declared_cwd") or "/") == root
]
if len(names) == 1:
    print(names[0])
' "$1"
}

mapfile -t BUSY < <(agents_with_status busy <"$SNAPSHOT_DIR/agents-before-restart.json")
log "busy before restart: ${BUSY[*]:-none}"
VERIFIER=""
if [ -n "$REPO_ROOT" ]; then
    VERIFIER=$(agent_in_directory "$REPO_ROOT" <"$SNAPSHOT_DIR/agents-before-restart.json")
fi
log "deploy verifier: ${VERIFIER:-none}"

declare -A MESSAGE=()
for name in "${BUSY[@]}"; do
    MESSAGE[$name]="$RESUME"
done
if [ -n "$VERIFIER" ]; then
    check="Wakterm was just deployed with a mux restart. Check that it went well: the running mux server's version (wakterm --version) matches the commit deployed from $REPO_ROOT, every agent in $SNAPSHOT_DIR/agents-before-restart.json is back and alive in wakterm agent list, and $LOG shows each busy agent was sent its resume message. Report anything wrong to the user."
    if [ -n "${MESSAGE[$VERIFIER]:-}" ]; then
        check="$check The restart also interrupted your turn; continue where you left off afterwards."
    fi
    MESSAGE[$VERIFIER]="$check"
fi
[ "${#MESSAGE[@]}" -gt 0 ] || exit 0

# Wait for the new mux server to answer.
while [ $SECONDS -lt $DEADLINE ]; do
    pid=$(pgrep -f wakterm-mux-server | head -1)
    if [ -n "$pid" ] && [ "$pid" != "$OLD_PID" ] && "$WAKTERM" agent list >/dev/null 2>&1; then
        break
    fi
    sleep 2
done
log "mux server pid ${pid:-none}"

# Sends each named agent its message once it is back and idle, until
# DEADLINE_AT. A refused send writes nothing to the pane, so only errors and
# refusals are retried.
send_when_idle() {
    local deadline_at=$1
    shift
    local names=("$@")
    local -A sent=()
    while [ $SECONDS -lt "$deadline_at" ] && [ "${#sent[@]}" -lt "${#names[@]}" ]; do
        mapfile -t IDLE < <("$WAKTERM" agent list --format json 2>/dev/null | agents_with_status idle)
        for name in "${names[@]}"; do
            [ -z "${sent[$name]:-}" ] || continue
            printf '%s\n' "${IDLE[@]}" | grep -qxF "$name" || continue
            # Give the resumed harness a moment to finish drawing its prompt.
            sleep 5
            if out=$("$WAKTERM" agent send "$name" "${MESSAGE[$name]}" 2>&1) &&
                ! grep -q '"refusal": *{' <<<"$out"; then
                sent[$name]=1
                log "sent $name"
            else
                log "send to $name failed, will retry: $out"
            fi
        done
        [ "${#sent[@]}" -lt "${#names[@]}" ] && sleep 10
    done
    for name in "${names[@]}"; do
        [ -n "${sent[$name]:-}" ] || log "gave up on $name: not idle and reachable in time"
    done
}

# The busy agents first, then the verifier, so its check sees how their
# resume messages went, including any that were given up on.
OTHERS=()
for name in "${BUSY[@]}"; do
    [ "$name" = "$VERIFIER" ] || OTHERS+=("$name")
done
[ "${#OTHERS[@]}" -eq 0 ] || send_when_idle $((SECONDS + 600)) "${OTHERS[@]}"
[ -z "$VERIFIER" ] || send_when_idle "$DEADLINE" "$VERIFIER"
log "done"
