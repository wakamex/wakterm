# Agent Harness Lifecycle and Managed Backend Direction

## Status

Wakterm supports agent harnesses as terminal processes running in PTY panes. It
can detect supported harnesses, observe provider session state, and adopt
confirmed sessions into its persistent agent registry. Codex can also be
started as an app-server TUI: the mux supervises one shared Codex app-server
and each pane still renders the native Codex TUI.

Reliable automatic restoration of adopted harnesses is the next lifecycle
goal. Structured managed-agent backends are a later project. This document
defines the intended boundaries now so that adoption and restoration metadata
converge on the long-term design.

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
| Managed | Wakterm owns a structured backend connection and renders its own agent presentation | Yes |

These states are not aliases:

- Detection does not authorize persistence.
- Adoption does not prove that a session can be resumed.
- Restoration of a native TUI does not make the session managed.
- An app-server TUI is not managed mode because Wakterm does not render its
  presentation or own its approval UI.
- Managed mode is a separate way to start a session, not an automatic upgrade
  of a running PTY.

`Restorable` and `Managed` describe lifecycle capabilities rather than public
origins. `App-server TUI` is represented by the `CodexAppServerTui` transport.

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

This transport is intentionally narrower than managed mode. It does not render
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

Process IDs are incarnation identifiers only. When a PID is recorded, its
start time must also match so PID reuse cannot attach stale metadata to an
unrelated process. Neither value is a durable session identity.

Automatic adoption may promote a detected pane only after a confirmed session
match. If the harness exits back to a shell, stale automatically adopted state
must be cleared instead of making the shell look like a live agent.

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

## Managed agents

A managed agent is not a native TUI with extra input injection. The mux server
owns the backend process and structured protocol connection. Wakterm owns the
pane presentation, approvals, status, transcript projection, and recovery
state. GUI clients receive mirrored state from the mux and do not connect to
the provider independently.

PTY and managed modes remain separate:

```text
existing shell or TUI
  -> detect
  -> confirm
  -> optionally adopt
  -> restore as a native TUI

Wakterm managed start
  -> mux launches a backend
  -> mux owns the structured session
  -> Wakterm renders a managed pane
```

Do not automatically promote a detected or adopted PTY into managed mode. A
live promotion would create ambiguous turn ownership, approval ownership, and
transcript reconciliation. If migration is added later, it must be explicit,
limited to a verified idle session, and switch transports only after the new
backend resumes and validates the same provider session.

The initial managed model should use one backend instance per managed pane.
Sharing a provider process across panes can be considered later if evidence
shows that the efficiency gain is worth the additional failure coupling.

## Managed backend protocol policy

Wakterm should expose one internal managed-backend interface and normalized
lifecycle model. Protocol implementations sit behind that boundary.

The default selection policy is ACP-first with capability-gated native
exceptions:

1. Prefer direct Agent Client Protocol support when the provider implements it
   natively.
2. Prefer a maintained ACP adapter when it faithfully exposes Wakterm's
   required behavior.
3. Add or select a native provider backend when a measured capability,
   reliability, recovery, or diagnostic gap justifies it.
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

The initial provider stance is:

| Provider | Starting preference | Reason |
| --- | --- | --- |
| Gemini | Direct ACP | Gemini CLI implements ACP itself |
| OpenCode | Direct ACP | OpenCode implements ACP itself; use its HTTP API only for a demonstrated gap |
| Claude | Maintained ACP adapter | A custom Wakterm bridge would still need to wrap the Claude Agent SDK and reproduce substantial translation logic |
| Codex | Compare ACP with native app-server | ACP offers reuse, while app-server may be necessary for exact turn, steering, approval, subagent, or recovery behavior |

This table is a starting hypothesis, not an implementation commitment. Before
choosing the Codex transport, run the same lifecycle scenarios through both
interfaces and attribute any difference to the protocol, adapter, provider, or
Wakterm layer.

Useful upstream references:

- [Agent Client Protocol](https://agentclientprotocol.com/)
- [Gemini CLI ACP mode](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/acp-mode.md)
- [OpenCode CLI ACP mode](https://opencode.ai/docs/cli/)
- [Claude Agent ACP adapter](https://github.com/agentclientprotocol/claude-agent-acp)
- [Codex ACP adapter](https://github.com/agentclientprotocol/codex-acp)
- [Codex app-server](https://developers.openai.com/codex/app-server)

## Managed runtime authority

ACP or a native provider protocol is a transport, not Wakterm's persistence
authority. A managed pane needs a durable Wakterm record containing at least:

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

### Managed backends

When managed work begins, one reusable fake backend should test the normalized
state machine. Each real provider then needs a smaller compatibility suite
covering:

- initialize and capability negotiation
- new session and exact-session resume
- streamed turns, commands, edits, and terminal activity
- permission requests and user questions
- cancellation and immediate subsequent input
- backend death and restart
- session close and resource cleanup
- mux restart and GUI reconnect
- two GUI clients observing one mux-owned backend

For Codex, the transport comparison must additionally cover active-turn
steering, subagent completion, nested or concurrent approvals, and provider
upgrade compatibility. If ACP fails critical scenarios because of unstable
extensions or adapter behavior, app-server should implement the same internal
Wakterm backend interface instead.

## Implementation order

The current priority order is:

1. Make detection falsifiable and observable.
2. Make confirmed automatic adoption reliable.
3. Persist authoritative provider session identity and explicit restore intent.
4. Restore native harnesses by resuming the exact session.
5. Make automatic layout restoration report partial and failed recovery
   honestly.
6. Revisit managed backends only after PTY lifecycle behavior is dependable.

The managed architecture informs today's identity and persistence choices, but
it must not expand the current restoration work into premature backend
implementation.
