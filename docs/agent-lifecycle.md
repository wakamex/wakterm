# Agent Harness Lifecycle and Supervised Backend Direction

## Status

Wakterm supports agent harnesses as terminal processes running in PTY panes. It detects supported harnesses (Agy, Claude, Codex, Gemini, OpenCode), observes provider session state, and automatically adopts confirmed sessions into its persistent agent registry.

Restorable Codex sessions are restored automatically across multiplexer restart and system reboot in their declared working directory, resuming the exact confirmed provider session. If a restart interrupts an active turn, Wakterm restores the session but does not guarantee that the in-flight turn continues. Codex can also be started as a supervised app-server TUI: the multiplexer supervises one shared Codex app-server while each pane renders the native Codex TUI.

Agent API v1 provides versioned capability negotiation, catalog queries, authoritative prompt admission, durable event streams, and return request tracking. Structured supervisor backends that preserve the provider native TUI remain the long-term direction. Wakterm-rendered agent presentation is not the normal product direction. This document defines the lifecycle boundaries and guarantees.

## Lifecycle states

The lifecycle uses distinct states because each one has stronger evidence and
stronger guarantees than the previous one.

| State | Meaning | Durable |
| --- | --- | --- |
| Detected | Runtime evidence suggests that a supported harness is running in a pane | No |
| Confirmed | The process is matched to a concrete provider session reference | No |
| Adopted | Wakterm persistently associates an agent identity with the pane and confirmed harness | Yes |
| Restorable | Wakterm has a verified recipe for starting the harness and resuming that exact provider session | Yes |
| App-server TUI | The mux owns a structured provider connection while the pane renders the provider's native TUI | Yes |
| Wakterm-rendered | Wakterm owns a headless backend connection and renders its own agent presentation | Yes, but not a normal launch target |

These states are not aliases:

- Detection does not authorize persistence.
- Adoption does not prove that a session can be resumed.
- Restoration of a native TUI does not make the session structurally supervised.
- An app-server TUI is supervised without making Wakterm responsible for the
  provider's presentation or approval UI.
- A Wakterm-rendered backend is not a substitute for a provider's native TUI.

`Restorable` describes a lifecycle capability rather than a public origin.
`App-server TUI` is represented by the `CodexAppServerTui` transport.

## Codex app-server TUI transport

`wakterm agent launch codex` starts or resumes an exact Codex thread through one
mux-owned app-server on a private Unix socket. The mux keeps one initialized
protocol connection, routes lifecycle events by exact thread ID, and persists
the distinct Codex thread ID and session ID. Each pane runs `codex resume`
against that socket, so input, rendering, approvals, and native interaction
remain Codex TUI responsibilities.

When invoked inside a Wakterm pane, the command runs the native TUI in that
current pane and returns to its shell when Codex exits. `--new-tab` explicitly
creates a separate tab instead. Invocations outside Wakterm must use
`--new-tab`, because there is no current Wakterm PTY to own the TUI.

This transport intentionally keeps the native provider UI. It does not render
a Wakterm agent UI, does not adopt an existing PTY into the app-server, and
does not change the observed-PTY path for manually launched Codex processes.
The shared process has one executable, version, `CODEX_HOME`, authentication
identity, environment policy, feature set, and remote Code Mode host. Launches
that need a different process-wide configuration must use a different mux or
the observed-PTY path.

## Detection and confirmed adoption

Detection may use process trees, foreground process information, terminal
titles, and provider-specific observer data. Process names and terminal titles
are useful discovery evidence but are too weak to persist by themselves.

Confirmed adoption requires a provider session reference discovered by the
observer. Depending on the provider, that reference may currently be a session
file, a database plus session ID, or another provider-owned record.

On Linux, Agy confirmation matches the exact process incarnation to its open
per-conversation presence lock, then observes that conversation's persistent
transcript. Wakterm does not select an Agy conversation by modification time or
working directory alone.

Process IDs are incarnation identifiers only. When a PID is recorded, its
start time must also match so PID reuse cannot attach stale metadata to an
unrelated process. Neither value is a durable session identity.

Automatic adoption may promote a detected pane only after a confirmed session
match. If the harness exits back to a shell, stale automatically adopted state
must be cleared instead of making the shell look like a live agent.

Provider artifact observation continues after adoption. Filesystem changes are
hints to refresh the exact pane and confirmed provider session through the
observer worker. This keeps durable agent events current even when no client is
listing agents or submitting prompts. Event reads remain side-effect free.

Unnamed tabs whose active pane contains an adopted or app-server agent use that
agent's leaf folder name as an automatic display title. The title follows the
active pane because one tab may contain multiple agent panes with different
working directories. An explicit user title always wins and is the only title
persisted as layout identity. Terminal and folder-derived titles remain
automatic. The rename-tab prompt preloads only an explicit title, so an empty
prompt also indicates that the visible title is automatic. Submitting an empty
title clears the explicit name and returns the tab to automatic naming.

## Restoration contract

Automatic restoration must resume the intended provider session or report a
visible failure. It must never silently replace an expected harness with a
fresh shell or silently start a new provider session.

### Durable restore intent

A restorable agent needs enough persisted intent to reconstruct an exact
resume operation:

- stable Wakterm agent ID
- harness or provider kind
- stable provider session ID or an equally authoritative provider reference
- declared working directory
- launch executable and arguments
- provider-specific resume recipe version
- required workspace roots or checkout information
- safe configuration needed to recreate the session

Do not persist process IDs as recovery handles. Do not persist access tokens,
temporary authentication material, or inherited environment secrets in layout
or session files.

The provider session identity is authoritative for recovery. File names,
timestamps, titles, and command lines are supporting evidence unless a
provider explicitly defines one of them as its stable identity.

### Restore sequence

For each expected harness pane, restoration should:

1. Load and validate the persisted restore intent.
2. Verify that the provider executable and referenced session are available.
3. Construct the provider's exact resume invocation. A supervised native TUI
   may use a minimal shell wrapper only when it provides a bounded reconnect
   after its mux-owned backend restarts.
4. Spawn the TUI in the restored pane and declared working directory.
5. Observe the new process incarnation.
6. Confirm that it opened the expected provider session.
7. Bind the existing Wakterm agent ID to the new pane only after confirmation.

If any step fails, keep the layout recoverable, surface the failure, and retain
enough intent for an explicit retry. A failure pane or equivalent diagnostic
surface is preferable to a convincing but incorrect fresh shell.

Restoration must be idempotent. Repeated reconciliation must not launch a
second harness after the first one has started but before observation has
finished. Persisted intent, launch attempts, and confirmed runtime bindings
must remain distinguishable.

Mid-turn recovery is provider-dependent and is not guaranteed by restoring a
session. The first target is confident idle-session resume after a mux-server
restart.

## Native TUI product boundary

Wakterm must preserve the provider's native TUI for interactive agent panes.
The provider TUI owns input, rendering, questions, approvals, and other native
interaction. Wakterm may add lifecycle supervision only when a structured
connection can attach to the same exact provider session without taking over
those responsibilities.

The supported paths are:

```text
existing shell or TUI
  -> detect
  -> confirm
  -> optionally adopt
  -> restore as a native TUI

Wakterm supervised start
  -> mux owns a structured supervisor when the provider supports one
  -> pane runs the provider's native TUI against the same exact session
  -> provider TUI remains the interactive authority
```

An ACP agent normally expects the ACP client to render the conversation and
approval UI. That topology does not qualify merely because its lifecycle
events are structured. A provider protocol qualifies for a normal Wakterm
launch only when the native TUI and mux supervisor can attach concurrently to
the same session. Otherwise retain the observed and restorable PTY path and be
honest about its weaker lifecycle evidence.

Do not automatically promote a detected or adopted PTY into a supervised
transport. If attachment is added later, it must be explicit, limited to a
verified session, and declare success only after the structured connection and
native TUI confirm the same provider identity.

## Supervised backend protocol policy

Wakterm should expose one internal supervision interface and normalized
lifecycle model only when at least one additional provider passes the required
same-session native-TUI tests. Protocol implementations sit behind that
boundary.

The selection policy is native-TUI-first:

1. Preserve the provider's native TUI as the interactive surface.
2. Prefer a structured provider protocol only when it can supervise the exact
   session used by that TUI.
3. Reject adapters that require Wakterm to render the agent interaction.
4. Keep provider-specific information available behind typed extensions rather
   than forcing every feature into a lowest-common-denominator model.

ACP provides protocol-version negotiation, advertised optional capabilities,
one reusable client implementation, and one reusable fake-agent test surface.
Those properties reduce code and compatibility logic owned by Wakterm.

ACP does not guarantee whole-system robustness. An adapter adds another
process, version relationship, translation state machine, and logging layer.
It may also omit or approximate provider-native concepts. Wakterm must record
the backend and protocol versions it actually loaded, negotiate capabilities,
and fail explicitly when a required capability is absent. Provider-specific
ACP metadata must not become a silent protocol contract without versioned
tests.

The provider stance is:

| Provider | Starting preference | Reason |
| --- | --- | --- |
| Agy | Native observed PTY; investigate structured supervision | The interactive TUI exposes exact conversation presence and a persistent transcript; stream JSON and state callbacks apply to managed launches rather than attachment to an existing TUI |
| Gemini | Native TUI; investigate same-session supervision | Direct ACP makes the client the UI unless Gemini supports concurrent native-TUI attachment |
| OpenCode | Native TUI; investigate same-session supervision | Direct ACP is not sufficient if it replaces the provider TUI |
| Claude | Native observed and restorable PTY | Remote Control preserves the TUI but exposes no supported local observer; reverse-engineered `--sdk-url`, SDK, and ACP paths are headless, cloud-constrained, or infer state from a PTY |
| Codex | Native app-server TUI | The mux and native TUI attach to the same exact app-server thread |

This table is a starting policy, not evidence that every provider already
supports structured supervision. Attribute each lifecycle fact to the
protocol, adapter, provider, or Wakterm layer before changing a transport.

Useful upstream references:

- [Agy title generation](https://antigravity.google/docs/cli/title)
- [Agy hooks](https://antigravity.google/docs/hooks)
- [Agy headless and stream JSON mode](https://antigravity.google/docs/cli/headless/)
- [Agy exact conversation resume](https://antigravity.google/docs/cli/commands/resume)
- [Agent Client Protocol](https://agentclientprotocol.com/)
- [Claude ACP adapter Rust evaluation](claude-acp-rust-evaluation.md)
- [Gemini CLI ACP mode](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/acp-mode.md)
- [OpenCode CLI ACP mode](https://opencode.ai/docs/cli/)
- [Claude Agent ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp)
- [Codex ACP adapter](https://github.com/agentclientprotocol/codex-acp)
- [Codex app-server](https://developers.openai.com/codex/app-server)

## Supervised runtime authority

A provider protocol is a transport, not Wakterm's persistence authority. A
supervised pane needs a durable Wakterm record containing at least:

- stable Wakterm agent ID
- provider and backend kind
- provider session ID
- working directory and workspace roots
- backend executable and validated version
- negotiated capabilities
- current provider turn or request identity when available
- outstanding approval identities
- last authoritative persisted checkpoint
- recovery result after the latest backend restart

Transient notifications update this state, but the last notification alone
must not define recovery behavior. Preview or delta events are display aids;
provider-confirmed terminal events and persisted session state are the
authoritative checkpoints.

## Validation gates

### PTY adoption and restoration

Before claiming reliable automatic restoration, cover:

- false-positive process and title detection
- PID reuse and stale process metadata
- provider session matching with multiple candidate sessions
- harness exit back to a shell
- mux restart followed by exact session resume
- missing, corrupt, and incompatible session references
- partial restore of a multi-pane layout
- repeated reconciliation without duplicate launches
- a failure that proves no fresh shell or fresh agent session was substituted

Provider-specific resume behavior needs real harness smoke tests where a small
fixture cannot establish correctness. Deterministic discovery, persistence,
and reconciliation logic should remain covered by unit or integration tests.

### Supervised native-TUI backends

When supervision work begins, one reusable fake backend should test the
normalized state machine. Each real provider then needs a smaller compatibility
suite covering:

- initialize and capability negotiation
- native TUI and supervisor attachment to the same exact session
- native ownership of input, rendering, questions, and approvals
- new session and exact-session resume
- streamed turns, commands, edits, and terminal activity
- observation of permission requests and user questions without taking ownership
- cancellation and immediate subsequent input
- supervisor disconnection while the native TUI continues
- supervisor death, restart, and same-session reattachment
- session close and resource cleanup
- mux restart and GUI reconnect
- two GUI clients observing one mux-owned supervisor

Codex app-server TUI additionally needs coverage for active-turn steering,
subagent completion, nested or concurrent approvals, and provider upgrade
compatibility. For another provider, a headless ACP mode is not a fallback when
same-session native-TUI attachment fails.

## Implementation order

The current priority order is:

1. Make detection falsifiable and observable.
2. Make confirmed automatic adoption reliable.
3. Persist authoritative provider session identity and explicit restore intent.
4. Restore native harnesses by resuming the exact session.
5. Make automatic layout restoration report partial and failed recovery
   honestly.
6. Revisit structured supervision only when a provider can preserve its native
   TUI and pass the same-session attachment gate.

The supervised architecture informs today's identity and persistence choices,
but it must not expand the current restoration work into premature backend
implementation or a Wakterm-rendered agent UI.
