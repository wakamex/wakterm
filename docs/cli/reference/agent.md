# `wakterm agent`

The `agent` subcommand starts, observes, and interacts with AI coding agents and harness panes.

Supported harnesses include Agy, Claude, Codex, Gemini, and OpenCode.

## Overview of agent subcommands

- `wakterm agent start`: start an agent harness in the current pane, a split, a new tab, or a new window
- `wakterm agent launch codex`: launch Codex through a mux-supervised app-server transport
- `wakterm agent adopt`: adopt an existing pane as an agent with explicit metadata
- `wakterm agent adopt-detected`: promote a detected harness pane into persistent agent metadata
- `wakterm agent list`: list adopted and detected agent panes
- `wakterm agent watch`: stream latest observer-backed harness messages
- `wakterm agent inspect`: inspect a single adopted or detected agent
- `wakterm agent output`: read experimental normalized agent output for shadow comparison
- `wakterm agent events`: read durable normalized Agent API v1 events
- `wakterm agent capabilities`: print versioned Wakterm Agent API capabilities
- `wakterm agent catalog`: print the narrow Agent API catalog
- `wakterm agent admit`: atomically admit and submit an agent prompt
- `wakterm agent send`: send a prompt to an agent pane with optional return correlation
- `wakterm agent request`: inspect, stream, or cancel durable agent return requests
- `wakterm agent interrupt`: interrupt a native harness turn
- `wakterm agent set`: attach agent metadata to a pane
- `wakterm agent clear`: remove agent metadata from a pane

See also:

- [Agent Harness Lifecycle](../../agent-lifecycle.md)
- [Agent Prompt Submission and Final Responses](../../agent-send.md)
- [Agent API v1 Contract](../../agent-api/v1/index.md)
- [Experimental Agent Output Shadow Page](../../agent-output.md)

## `wakterm agent start`

Starts a supported harness in the current pane, a new split, a new tab, or a new window.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-start--help.txt" %}
```

Examples:

```sh
# Start Codex in a new tab
wakterm agent start codex --new-tab

# Start Claude in a split pane to the right
wakterm agent start claude --right --percent 50

# Start Gemini in a specific working directory
wakterm agent start gemini --cwd /code/project
```

## `wakterm agent launch codex`

Launches Codex as a mux-supervised app-server TUI. The mux manages the app-server connection over a private Unix socket while the pane runs the native Codex TUI.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-launch-codex--help.txt" %}
```

When run inside a Wakterm pane, the command runs the native TUI in the current pane and returns to the shell when Codex exits. Use `--new-tab` when running outside Wakterm or when a separate tab is desired.

Examples:

```sh
# Launch in current pane
wakterm agent launch codex

# Launch in a new tab
wakterm agent launch codex --new-tab

# Resume an exact Codex thread UUID
wakterm agent launch codex --resume 12345678-1234-1234-1234-123456789abc
```

## `wakterm agent list`

Lists adopted and detected agent panes.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-list--help.txt" %}
```

By default, `agent list` prints a compact table. Use `-v` for verbose details including turn state and launch command, `-f` to follow updates, or `--format json` for JSON output.

```sh
# Compact table
wakterm agent list

# Verbose table
wakterm agent list -v

# Stream live updates
wakterm agent list -f

# JSON output
wakterm agent list --format json
```

## `wakterm agent watch`

Streams latest observer-backed harness messages across adopted and detected panes.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-watch--help.txt" %}
```

Output formats:

```sh
# Tab-separated streaming output
wakterm agent watch

# JSON lines output
wakterm agent watch --format json
```

## `wakterm agent inspect`

Inspects detailed runtime and metadata state for a single agent.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-inspect--help.txt" %}
```

```sh
wakterm agent inspect zola
```

## `wakterm agent adopt` and `adopt-detected`

Adopts an existing pane or promotes a detected harness pane into persistent agent metadata.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-adopt--help.txt" %}
```

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-adopt-detected--help.txt" %}
```

## `wakterm agent send`

Sends a message to an agent pane.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-send--help.txt" %}
```

For idle Codex agents, `--return-final` enables durable asynchronous return correlation.

```sh
# Send prompt to an agent pane
wakterm agent send zola "Run test suite"

# Asynchronous prompt with durable return correlation
wakterm agent send zola --return-final "Refactor module"
```

## `wakterm agent admit`

Atomically admits and submits an agent prompt with process incarnation validation and idempotency keys. This is the primary submission interface for external orchestrators.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-admit--help.txt" %}
```

```sh
wakterm agent admit zola \
  --incarnation INCARNATION_ID \
  --request-id REQUEST_UUID \
  --return-final \
  "Complete task"
```

## `wakterm agent request`

Manages durable agent return requests created with `--return-final`.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-request--help.txt" %}
```

### `wakterm agent request get`

Views details for a specific return request by ID.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-request-get--help.txt" %}
```

### `wakterm agent request watch`

Streams terminal return request events.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-request-watch--help.txt" %}
```

### `wakterm agent request cancel`

Cancels an in-flight return request.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-request-cancel--help.txt" %}
```

## `wakterm agent events`

Reads durable normalized Agent API v1 events.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-events--help.txt" %}
```

```sh
wakterm agent events --after 0 --limit 100
wakterm agent events --after 0 --limit 100 --follow
```

`--follow` keeps one mux connection open and writes each page as one JSON line.
It drains retained pages without delay and holds one bounded request at the
stream head until a durable commit or `--wait-ms` timeout. It exits after a
`cursor_too_old` page so the consumer can take a fresh catalog snapshot.

## `wakterm agent capabilities` and `catalog`

Prints Agent API capabilities and the current narrow agent catalog.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-capabilities--help.txt" %}
```

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-catalog--help.txt" %}
```

## `wakterm agent interrupt`

Interrupts a native harness turn.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-interrupt--help.txt" %}
```

```sh
wakterm agent interrupt zola
```

## `wakterm agent set` and `clear`

Attaches or removes agent metadata for a pane.

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-set--help.txt" %}
```

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-agent-clear--help.txt" %}
```
