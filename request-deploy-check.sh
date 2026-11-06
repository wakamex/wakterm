#!/usr/bin/env bash
# Asks the agent working on this repository to check that a deploy with a
# mux restart went well. The restarted mux server itself asks the agents
# whose turns the restart interrupted to continue them. deploy.sh --notify
# starts this as a transient user unit before it restarts the mux, so it
# outlives a deploy run from an agent pane.
#
# Usage: request-deploy-check.sh SNAPSHOT_DIR OLD_MUX_PID REPO_ROOT
#
# SNAPSHOT_DIR holds agents-before-restart.json; the log goes to
# SNAPSHOT_DIR/deploy-check.log. The verifier is the one live agent whose
# working directory is REPO_ROOT; with none or several, no check is
# requested.
set -uo pipefail

SNAPSHOT_DIR="$1"
OLD_PID="$2"
REPO_ROOT="${3:-}"
WAKTERM="${WAKTERM:-$HOME/.local/bin/wakterm}"
LOG="$SNAPSHOT_DIR/deploy-check.log"
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
[ -n "$VERIFIER" ] || exit 0
CHECK="Wakterm was just deployed with a mux restart. Check that it went well: the running mux server's version (wakterm --version) matches the commit deployed from $REPO_ROOT, every agent in $SNAPSHOT_DIR/agents-before-restart.json is back and alive in wakterm agent list, and the mux server's log (journalctl --user -u wakterm-mux-server) shows each agent that was busy before the restart (${BUSY[*]:-none}) was resumed. Report anything wrong to the user."

# Wait for the new mux server to answer.
while [ $SECONDS -lt $DEADLINE ]; do
    pid=$(pgrep -f wakterm-mux-server | head -1)
    if [ -n "$pid" ] && [ "$pid" != "$OLD_PID" ] && "$WAKTERM" agent list >/dev/null 2>&1; then
        break
    fi
    sleep 2
done
log "mux server pid ${pid:-none}"

# Sends the verifier its check once it is back and idle. A refused send
# writes nothing to the pane, so only errors and refusals are retried.
while [ $SECONDS -lt $DEADLINE ]; do
    if "$WAKTERM" agent list --format json 2>/dev/null | agents_with_status idle | grep -qxF "$VERIFIER"; then
        # Give the resumed harness a moment to finish drawing its prompt.
        sleep 5
        if out=$("$WAKTERM" agent send "$VERIFIER" "$CHECK" 2>&1) &&
            ! grep -q '"refusal": *{' <<<"$out"; then
            log "sent $VERIFIER"
            log "done"
            exit 0
        fi
        log "send to $VERIFIER failed, will retry: $out"
    fi
    sleep 10
done
log "gave up on $VERIFIER: not idle and reachable in time"
