# Experimental agent output shadow page

`wakterm agent output TARGET` reads normalized provider output without exposing provider session files to the caller. It is an experimental, per-agent discovery interface for side-effect-free Panetone shadow comparisons. It is not the durable Wakterm Agent API event contract and must not be used as production cutover evidence.

The first read establishes a baseline at the current complete provider record and returns no historical messages:

```sh
wakterm agent output zola
```

Save the returned `next_cursor` and pass it back after more output appears:

```sh
wakterm agent output zola --after OPAQUE_CURSOR
```

The command always returns structured JSON using schema `wakterm.agent-output-shadow.experimental.v1`. The experimental marker is intentional. Compatibility is not promised until the durable Agent API gate is defined. A successful page contains:

- `agent_id` and an opaque `session_id`
- normalized events with stable `event_id` values
- an opaque `next_cursor`
- `has_more`, which tells the consumer to read another page immediately
- `baseline`, which is true when the cursor starts at the current session tail

The cursor is scoped internally to one stable agent, confirmed process incarnation, observed provider session file, and content checkpoint. Callers store it but do not decode it. Provider-file replacement changes the session identity. Truncation or in-place rewriting invalidates a prior checkpoint, and event identity includes the source record content rather than byte position alone.

Expected non-success states are also returned as structured JSON:

- `cursor_invalid`
- `session_changed`, with a new baseline cursor
- `observer_unavailable`
- `unsupported_harness`

`session_changed` and any `cursor_invalid` response that supplies a new baseline cursor represent an explicit output gap. A consumer must record the gap and decide whether to establish that new baseline. It must never silently adopt the replacement cursor as continuous history.

The initial implementation supports Codex assistant messages. It uses Wakterm's existing process-confirmed Codex observer and reads the provider's append-only rollout directly, so it does not add another output database or background event service. Provider-file scanning runs outside the mux main thread. Each read is also bounded by internal record and byte budgets. A tool-heavy page can therefore return no assistant events with `has_more: true`, and the consumer should immediately request the next page.

## Deliberate gaps before a durable event API

This page does not satisfy the durable Panetone event-consumer gate. Wakterm
now exposes versioned capability, catalog, and authoritative prompt-admission
operations. The durable output contract still needs:

- catalog and lifecycle ordering relative to the event cursor
- distinct plan, turn lifecycle, final, observer failure, and agent lifecycle event kinds
- a strictly increasing durable sequence that survives restart
- defined retention and an explicit `cursor_too_old` result with recovery metadata
- Wakterm-owned golden fixtures consumed across repositories

Panetone may use the authoritative admission receipt now. It must continue to
use this output page only for side-effect-free shadow comparisons until the
remaining event contract is promoted.

This API does not change the experimental status of `agent output` or the
existing durable return-final terminal stream.
