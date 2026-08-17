# Agent prompt submission and final responses

`wakterm cli agent send TARGET MESSAGE` keeps its existing behavior. It writes
the prompt to the native harness pane, submits it, waits briefly for observer
acknowledgement, and prints structured JSON.

For an idle Codex agent with a confirmed observer session, `--return-final`
creates a durable asynchronous return request:

```sh
wakterm cli agent send zola --return-final "Complete phases 2 and 3"
```

The command registers the exact correlation boundary, submits the prompt, and
exits. Its JSON receipt contains `reply_pending: true`, the request ID, target
process incarnation, observer session path, baseline provider turn, and armed
cursor. It does not keep a CLI process or RPC socket open for the delegated
turn.

Use a caller-generated UUID when another durable system owns the request:

```sh
wakterm cli agent send zola \
  --return-final \
  --request-id fe57dc90-994e-4e73-b09c-fac483d9f05b \
  --final-timeout-ms 3600000 \
  "Complete phases 2 and 3"
```

Repeating the same request ID and input returns its existing receipt without
submitting the prompt again. Reusing the ID with different input is rejected.
The timeout is an asynchronous request deadline. Zero, the default, disables
the deadline.

## Authoritative admission for orchestrators

`agent send` remains the convenient interactive command. An orchestrator that
must not steer an active turn uses the versioned admission API instead:

```sh
wakterm cli agent capabilities
wakterm cli agent catalog
wakterm cli agent admit zola \
  --incarnation PROCESS_INCARNATION \
  --request-id REQUEST_ID \
  --return-final \
  --final-timeout-ms 3600000 \
  "Complete phases 2 and 3"
```

The catalog supplies the stable agent ID and opaque process incarnation. The
admission call requires both, refreshes the provider observer away from the mux
reactor, and rechecks the same incarnation and authoritative idle state
immediately before input. Prompt text and Enter are serialized as one pane
write. Observer scanning, SQLite work, and the removed 200 ms submission delay
do not block the mux reactor.

The structured receipt classifies `accepted`, `busy`, `unsupported`,
`unavailable`, `stale_incarnation`, `invalid`, `observer_failure`,
`internal_failure`, and `indeterminate`. A definitive rejection always has
`prompt_written: false`. Any write error is `indeterminate` because a PTY write
may have been partial. Callers must not retry an indeterminate request under a
new ID.

The caller owns the request ID. Repeating the same ID, process incarnation,
prompt bytes, paste mode, return mode, and timeout cannot write the prompt a
second time. Reusing an ID with different input is rejected. One-way
admissions and return-final admissions both persist their idempotency state.
Return-final admissions continue to complete on the existing durable terminal
request stream described below.

## Results and subscriptions

Inspect or cancel one request:

```sh
wakterm cli agent request get REQUEST_ID
wakterm cli agent request cancel REQUEST_ID
```

Terminal results are appended to a durable ordered event stream. A consumer
keeps the last processed `terminal_event_sequence` and resumes after it:

```sh
wakterm cli agent request watch --after 42
```

The command emits one JSON object per terminal request and remains attached as
a single subscription. `--once` drains currently available events and exits.
Terminal states are `completed`, `aborted`, `timed_out`, `cancelled`,
`delivery_failed`, and `indeterminate`. Only `completed` contains
`final_message`.

The request database lives in Wakterm's data directory, uses SQLite WAL with
full synchronous commits, and survives mux and consumer restarts. Terminal
event sequence numbers are stable, so consumers can make their own delivery
idempotent without reading provider session files.

## Correlation safety

Wakterm binds a request only when all of these facts match:

- the same adopted process ID and process start time remain attached
- the same exact observer session remains attached
- the target was idle at submission with a completed baseline turn
- the provider starts a new turn after the armed observer cursor
- the first user message hash matches the submitted prompt
- the bound provider turn ID remains stable through completion
- no additional user message is added to the correlated turn

A mismatch becomes `indeterminate`. Wakterm never substitutes a later final
message. A request persisted before input but not durably marked submitted at a
mux crash also becomes `indeterminate`, which prevents a post-restart prompt
from satisfying it accidentally.

Codex session discovery first checks rollout files actually held open by the
adopted process tree and verifies the adopted process start time and declared
working directory. A reused pane can no longer remain attached to an older
rollout merely because that file was the previous preferred observer session.

The current return mode is Codex-only because Codex rollout records expose a
stable provider turn ID and ordinal cursor. Other harnesses need equivalent
evidence before they can safely support this primitive.

One PTY limitation remains: byte delivery and the durable submitted marker
cannot be a single transaction. Wakterm resolves a crash in that narrow window
as indeterminate instead of risking duplicate or unrelated completion. The
actual pane write still runs through the existing mux input path, but it is one
bounded write rather than observer work, database work, or a timed wait.
