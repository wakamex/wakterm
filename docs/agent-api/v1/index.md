# Wakterm Agent API v1 golden contract

These fixtures are the cross-repository semantic contract for the live Wakterm
Agent API. Panetone can use them for fake-adapter and compatibility tests.

`golden-fixtures.json` retains two capability snapshots:

- `current_capabilities` is an exact live Wakterm capability response.
- `event_stream_capabilities` is a compatibility alias for consumers that
  previously selected the event fixture profile.

Wakterm advertises `event_stream.v1` only while its durable event store is
available. Consumers must still negotiate the live capability before reading.

The implemented v1 boundary is capability negotiation, the agent catalog,
authoritative prompt admission, and the existing return-request terminal
stream. Admission is scoped to a stable Wakterm agent ID and opaque current
incarnation. Native observed sessions use process identity, while managed
Codex sessions use exact app-server provider identity. A definitive
non-acceptance means no prompt bytes were written.

Every live catalog entry has a unique agent ID. Wakterm assigns that ID when
an agent is registered and persists it across restoration. Admission resolves
the exact catalog agent and incarnation pair rather than selecting by agent ID
alone.
An indeterminate result is never safe to retry under a new request ID.

Each catalog entry also contains a fixed-width `pane_id`. It is the smallest
authoritative locator for joining a Panetone live route to the current Wakterm
catalog when route titles and agent names differ. Pane IDs are ephemeral mux
coordinates. Consumers must resolve them from a fresh catalog and must never
persist them as agent identity, process identity, or an idempotency key.

The durable event page provides:

- durable increasing sequence order
- agent, current-incarnation, and exact turn identity
- distinct assistant, plan, turn, observer, and agent-lifecycle events
- catalog ordering through `as_of_event_sequence`
- explicit bounded-retention metadata and `cursor_too_old` recovery
- classified incompatible-version and unknown-event failures

The public contract does not expose the event database, provider paths, parser
cursors, or transport implementation. Wakterm's experimental Codex output page
remains available for side-effect-free shadow comparison, but it is not the
durable event contract.

The catalog `as_of_event_sequence` is a conservative lower-bound cursor sampled
before the live catalog snapshot. Starting after that sequence can replay a
lifecycle state already visible in the snapshot, but cannot skip a concurrent
lifecycle change. Consumers must apply events idempotently by event ID and
agent incarnation.

The live stream starts each newly observed provider session at its tail. It
does not replay provider history from before the first durable lifecycle
baseline. Provider session replacement, truncation, or rewrite emits an
`observer_failure` and arms a new tail baseline instead of guessing across the
gap.

Codex, Claude, Gemini, and OpenCode projections are live. Turn IDs come from
provider records: Codex turn IDs, Claude human-user UUIDs, Gemini user-message
IDs, and OpenCode assistant `parentID` values. Finals require provider completion
evidence: Codex `task_complete`, `turn_aborted`, or app-server turn completion,
Claude `end_turn`, a persisted Gemini response message, or OpenCode
`finish: stop`. OpenCode `tool-calls` is
intermediate and never a final. Plans are emitted separately when the provider
records an explicit plan artifact, currently Claude `ExitPlanMode`.

For observer-backed sessions, a durable provider turn transition updates the catalog and admission snapshot in the same observation. A committed `waiting_on_user` transition therefore makes the exact agent idle without waiting for terminal input or unrelated API activity.

For Codex app-server TUI sessions, completed agent-message items and turns are
committed from the live app-server notification stream. This includes sessions
restored after a mux restart and does not require catalog or prompt activity.
The authoritative status returned by app-server resume initializes the restored
catalog entry, so an idle session can accept prompt admission immediately.

Return-final admission uses the same request and receipt contract for observer-backed Codex PTYs and managed Codex app-server sessions. An observer-backed request is correlated through its exact process, provider session, cursor, prompt hash, and provider turn. A managed request arms the durable event sequence for its exact app-server thread and session, binds the first subsequent provider turn, and accepts only that turn's durable final.

Gemini observation accepts both legacy JSON conversation snapshots and the
current append-only JSONL format. Duplicate JSONL records update the same
durable provider message, incomplete trailing records wait for the next
refresh, and a legacy session that migrates to its `.jsonl` sibling keeps the
same provider session and cursor. Claude can persist separate thinking and text
records with `end_turn` on both; only the user-visible text record produces the
turn final. Gemini messages with `toolCalls` are intermediate assistant output,
not finals. OpenCode finals use the provider completion timestamp and never the
earlier assistant-message creation time. Explicit Claude and Gemini provider
errors emit `observer_failure`; they do not synthesize a terminal outcome.

The default retention bound is 100,000 events. A reader whose sequence precedes
retained history receives `cursor_too_old` and must take a fresh catalog
snapshot. Wakterm records previously available incarnations as unavailable
with reason `mux_restarted` when a new mux runtime opens the store, then emits a
new available lifecycle event only if that exact incarnation is observed again.

Provider parsing and SQLite commits run on the observer worker, not the mux
reactor. Wakterm watches provider artifact roots for both detected and adopted
panes. Artifact changes schedule the existing throttled observer path, and a
confirmed adopted session receives a trailing refresh when a hint lands inside
the throttle window. Event consumers only read the durable stream. They do not
need to poll provider files or call the catalog to make the producer advance.

The CLI page operation is:

```console
wakterm agent events --after 123 --limit 100
```

Long-lived consumers can reuse one mux connection and receive JSON-lines pages:

```console
wakterm agent events --after 123 --limit 100 --follow
```

Follow mode emits one line per page and drains retained pages without delay. At
the stream head it holds one bounded `ReadAgentEvents` request until a durable
commit or `--wait-ms` timeout, then repeats from the returned sequence. It exits
after `cursor_too_old` so the consumer can perform the documented
catalog-snapshot recovery.

`ReadAgentEvents.wait_ms` is additive, defaults to zero when absent, and is
clamped to 30 seconds by the server. Existing events and retention gaps return
immediately.

Unknown additive fields must be tolerated. An incompatible major schema or an
unknown event kind must fail explicitly. Provider paths and parser cursors are
not public identities.

Wakterm tests validate the current DTO examples, receipt invariants, sequence
ordering, lifecycle relationship, retention gap, and required error classes.
Panetone should consume this same file rather than copying the examples.
