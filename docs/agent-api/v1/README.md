# Wakterm Agent API v1 golden contract

These fixtures are the cross-repository semantic contract for Panetone Phase 2
fake-adapter work. They deliberately separate operations Wakterm implements
today from the proposed durable general event page.

`golden-fixtures.json` has two capability snapshots:

- `current_capabilities` is an exact live Wakterm capability response.
- `event_stream_capabilities` is fixture-only. A fake adapter may advertise it,
  but production Panetone must not enable the general event consumer until a
  live Wakterm response advertises `event_stream.v1`.

The implemented v1 boundary is capability negotiation, the agent catalog,
authoritative prompt admission, and the existing return-request terminal
stream. Admission is scoped to a stable Wakterm agent ID and opaque process
incarnation. A definitive non-acceptance means no prompt bytes were written.
An indeterminate result is never safe to retry under a new request ID.

The fixture-only event page defines only what the fake adapter needs to prove:

- durable increasing sequence order
- agent, process-incarnation, and exact turn identity
- distinct assistant, plan, turn, observer, and agent-lifecycle events
- catalog ordering through `as_of_event_sequence`
- explicit bounded-retention metadata and `cursor_too_old` recovery
- classified incompatible-version and unknown-event failures

It does not specify an event database, queue, callback system, provider-file
format, or transport implementation. Wakterm's experimental Codex output page
remains the side-effect-free discovery source. It is not a substitute for the
fixture-only durable event contract.

Unknown additive fields must be tolerated. An incompatible major schema or an
unknown event kind must fail explicitly. Provider paths and parser cursors are
not public identities.

Wakterm tests validate the current DTO examples, receipt invariants, sequence
ordering, lifecycle relationship, retention gap, and required error classes.
Panetone should consume this same file rather than copying the examples.
