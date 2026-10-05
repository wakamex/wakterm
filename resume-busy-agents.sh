#!/usr/bin/env bash
# Asks the agents that were busy before a mux restart to continue their
# interrupted turns. deploy.sh --notify starts this as a transient user unit
# before it restarts the mux, so it outlives a deploy run from an agent pane.
#
# Usage: resume-busy-agents.sh SNAPSHOT_DIR OLD_MUX_PID
#
# SNAPSHOT_DIR holds agents-before-restart.json; the log goes to
# SNAPSHOT_DIR/resume-busy-agents.log.
set -uo pipefail

SNAPSHOT_DIR="$1"
OLD_PID="$2"
WAKTERM="${WAKTERM:-$HOME/.local/bin/wakterm}"
LOG="$SNAPSHOT_DIR/resume-busy-agents.log"
MESSAGE="Wakterm's mux server restarted to deploy an update, which interrupted your turn. Continue where you left off."
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

mapfile -t TARGETS < <(agents_with_status busy <"$SNAPSHOT_DIR/agents-before-restart.json")
log "busy before restart: ${TARGETS[*]:-none}"
[ "${#TARGETS[@]}" -gt 0 ] || exit 0

# Wait for the new mux server to answer.
while [ $SECONDS -lt $DEADLINE ]; do
    pid=$(pgrep -f wakterm-mux-server | head -1)
    if [ -n "$pid" ] && [ "$pid" != "$OLD_PID" ] && "$WAKTERM" agent list >/dev/null 2>&1; then
        break
    fi
    sleep 2
done
log "mux server pid ${pid:-none}"

# Send each agent the message once it is back and idle. A refused send writes
# nothing to the pane, so only errors and refusals are retried.
declare -A SENT=()
while [ $SECONDS -lt $DEADLINE ] && [ "${#SENT[@]}" -lt "${#TARGETS[@]}" ]; do
    mapfile -t IDLE < <("$WAKTERM" agent list --format json 2>/dev/null | agents_with_status idle)
    for name in "${TARGETS[@]}"; do
        [ -z "${SENT[$name]:-}" ] || continue
        printf '%s\n' "${IDLE[@]}" | grep -qxF "$name" || continue
        # Give the resumed harness a moment to finish drawing its prompt.
        sleep 5
        if out=$("$WAKTERM" agent send "$name" "$MESSAGE" 2>&1) &&
            ! grep -q '"refusal": *{' <<<"$out"; then
            SENT[$name]=1
            log "sent $name"
        else
            log "send to $name failed, will retry: $out"
        fi
    done
    sleep 10
done

for name in "${TARGETS[@]}"; do
    [ -n "${SENT[$name]:-}" ] || log "gave up on $name: not idle and reachable within 15 minutes"
done
log "done"
