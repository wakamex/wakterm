# Claude ACP adapter Rust evaluation

Initial adapter research was performed on 2026-08-18. The Remote Control and
hidden-flag follow-up was performed on 2026-08-21. This note evaluates
Claude-to-ACP adapters for a possible structured Wakterm supervisor. It is an
evidence record, not a commitment to a particular adapter version.

## Conclusion

Do not use any of the evaluated ACP adapters for Wakterm's normal Claude launch
path. They make the ACP client the user interface instead of preserving the
native Claude TUI. Wakterm is a terminal and mux, not a replacement agent UI.

No evaluated adapter provides the required topology:

```text
wakterm-mux-server structured supervisor
  <-> exact Claude session
native Claude TUI in the Wakterm pane
  <-> the same exact Claude session
```

Until Claude exposes a supported same-session supervision or attachment
interface, keep Claude in the native-TUI observed and restorable PTY path. Do
not describe inferred PTY state as authoritative, and do not launch a headless
ACP session as a substitute.

Claude Remote Control proves that the provider can support a native terminal
TUI and another synchronized UI on the same local session. It does not
currently expose a supported local interface that Wakterm can use as a
supervisor.

The implementation language of an adapter is hidden behind ACP JSON-RPC over
stdio. Rust could simplify distribution and may change resource usage, but no
resource comparison was measured. Provider fidelity, exact recovery, and
cancellation correctness are more important than removing the Node runtime.
The official adapter currently has substantially stronger maintenance and
provider compatibility evidence than the Rust alternatives, but this does not
overcome the UI ownership mismatch.

The best Rust candidate is useful for experiments but would make Wakterm
responsible for maintaining an unofficial Rust port of the Claude Agent SDK
and tracking fast-moving Claude CLI behavior. That cost is not justified by
current evidence.

## Scope and scoring

The score measures quality as a headless authoritative Claude ACP backend. It
is not a score for Wakterm's native-TUI requirement. The separate native-TUI
column is the product gate: every candidate currently fails it.

A score of 100 would require evidence for:

- structured session, turn, item, permission, and cancellation events
- exact provider-session recovery without silent replacement
- concurrent sessions with independent working directories
- cancellation followed immediately by a new turn
- backend death and restart
- deterministic protocol tests and real current-Claude lifecycle tests
- Windows, macOS, and Linux support
- maintained compatibility with the current ACP and Claude Agent SDKs

The official TypeScript adapter is included as the comparison baseline even
though the search target was Rust.

| Implementation | Headless ACP score | Preserves native TUI | Finding |
| --- | ---: | --- | --- |
| [Official `claude-agent-acp`](https://github.com/agentclientprotocol/claude-agent-acp) | 93/100 | No | Strongest headless adapter. It uses the official Claude Agent SDK and has the broadest current lifecycle implementation. |
| [`soddygo/claude-code-acp-rs`](https://github.com/soddygo/claude-code-acp-rs) | 58/100 | No | Only serious structured Rust equivalent found. Broad implementation and test suite, but stale relative to Claude and built on an unofficial Rust SDK. |
| [`moabualruz/claude-code-cli-acp`](https://github.com/moabualruz/claude-code-cli-acp) | 35/100 | No | Its ACP path owns a Claude PTY and translates it for another client; it does not expose an independently attached native TUI. State also depends on terminal recognition and transcript JSONL. |
| [`serenorg/seren-acp-claude`](https://github.com/serenorg/seren-acp-claude) | 31/100 | No | Structured SDK approach, but `loadSession` is explicitly unsupported and the tests are narrow. |
| [`aptove/claude-acp`](https://github.com/aptove/claude-acp) | 14/100 | No | Small `claude -p --output-format stream-json` prototype with a hand-written protocol, no tests, and in-memory ACP-to-Claude session mapping. |

Rust ACP clients, general agent frameworks, orchestration servers, and ACP
bridges that do not adapt Claude Code were excluded. They do not replace
`claude-agent-acp` even when they are implemented in Rust.

## Reference implementation

At the time of research, the official
[`@agentclientprotocol/claude-agent-acp` package](https://github.com/agentclientprotocol/claude-agent-acp/blob/main/package.json)
was version 0.69.0, required Node 22 or newer, used ACP SDK 1.3.0, and used the
official Claude Agent SDK 0.3.232.

Its source implements session load, resume, fork, list, close, delete,
structured updates, permission requests, cancellation recovery, models,
configuration, background tasks, and provider-specific extensions. This does
not prove every Wakterm lifecycle scenario, but it provides a much stronger
starting point than reproducing that translation layer locally.

Node is a packaging and process dependency, not the deciding issue. The
deciding issue is that the ACP client owns the conversation and approval
presentation. Using the official adapter would therefore require Wakterm to
become the Claude UI.

## Rust candidates

### `claude-code-acp-rs`

This is the closest Rust equivalent. It uses `sacp`, implements an ACP agent,
and wraps the community
[`claude-code-agent-sdk`](https://github.com/soddygo/claude-code-agent-sdk).
The implementation includes session load and resume, cancellation, permission
requests, modes, MCP integration, fork support, and multiple in-memory
sessions.

Positive evidence:

- published as `claude-code-acp-rs` 0.1.22
- Rust 1.90 minimum version
- substantial implementation rather than a thin protocol sketch
- locked no-default-feature test suite completed successfully
- 504 tests were listed across its test targets

Blocking concerns:

- the inspected revision was last updated on 2026-04-20
- the adapter depends on unofficial `claude-code-agent-sdk` 0.1.39
- its default `bundled-cli` feature selects Claude CLI 2.1.41, while the
  installed CLI used for this evaluation was 2.1.234
- the broad suite is deterministic but does not establish live compatibility
  with the installed Claude version
- its documentation still describes an older official package lineage
- current multi-session, exact-resume, cancellation-race, and cross-platform
  behavior remain unproven against real Claude

This candidate should not be adopted merely because it compiles or has many
unit tests. It first needs a current-provider lifecycle run and a maintenance
plan for the community Claude SDK.

### `claude-code-cli-acp`

This project has the strongest deterministic test evidence among the smaller
Rust candidates. Its locked suite completed successfully with 62 listed tests;
three opt-in live or drift tests were ignored by default.

Its architecture is nevertheless incompatible with Wakterm supervision. The
adapter runs the interactive Claude TUI through a PTY, reads Claude transcript
JSONL for content, recognizes permission screens, and has terminal-screen
fallbacks. Its compatibility work is careful, but turn completion and other
lifecycle facts can still depend on terminal and transcript observations.

Wakterm's lifecycle contract forbids terminal output, process names, and
provider session JSONL from becoming authoritative lifecycle evidence. This
adapter may be useful to other ACP clients, but adopting it would recreate the
observed-PTY limitations inside a nominally structured transport. Its separate
interactive pass-through mode does not provide simultaneous ACP supervision of
that native TUI.

### `seren-acp-claude`

This project uses a community Rust Claude Agent SDK and an ACP library. It
supports new sessions, prompts, cancellation, modes, and tool approval
requests. Its README explicitly says `loadSession` is unsupported.

The locked suite completed successfully but contains only five model-parsing
tests. The inspected revision included a Windows Claude executable discovery
fix, but there is no evidence for full lifecycle behavior on any platform.
Without exact session load after backend or mux restart, it cannot satisfy
Wakterm recovery.

### `aptove/claude-acp`

This is a five-source-file prototype that manually implements enough JSON-RPC
to start, load, prompt, and cancel sessions. It launches `claude -p` with
`--output-format stream-json` and resumes using a returned Claude session ID.

The project compiled, but it had no tests. Its ACP session store is in memory,
so a new adapter process cannot resolve an old ACP session ID. It also lacks
the permission and lifecycle coverage required for structured supervision.

## Local verification

Environment:

```text
Fedora development host
rustc 1.95.0 (59807616e 2026-04-14)
Claude Code 2.1.234
```

Commands used after cloning each candidate into a temporary directory:

```sh
cargo test --locked --no-default-features  # claude-code-acp-rs
cargo test --locked                        # seren-acp-claude
cargo test --locked                        # claude-code-cli-acp
cargo test --locked                        # aptove/claude-acp
```

Results:

| Candidate | Result |
| --- | --- |
| `claude-code-acp-rs` | Passed; 504 tests listed across targets |
| `seren-acp-claude` | Passed; 5 tests listed |
| `claude-code-cli-acp` | Passed; 62 tests listed, including 3 ignored live or drift tests |
| `aptove/claude-acp` | Compiled; 0 tests |

No paid live Claude prompt was sent. Deterministic tests can establish local
build and protocol behavior, but not provider compatibility. A real lifecycle
smoke test remains a promotion requirement.

The official
[`agent-client-protocol` Rust SDK](https://github.com/agentclientprotocol/rust-sdk)
was available as crate version 2.0.0 with Rust 1.88 as its minimum version.
It is an SDK for clients and agents, not a Claude adapter or a reason for
Wakterm to add an agent presentation layer.

## Native TUI boundary

No candidate found provides both structured ACP authority and a native Claude
TUI presentation attached to the same session.

The official adapter and the structured Rust adapters are headless ACP agents.
The ACP client is expected to render the conversation, approvals, tools, and
status. The PTY-based Rust adapter owns a real Claude TUI internally but
translates it into ACP for another UI and obtains important state through PTY
and transcript observation.

The supported direction is therefore:

```text
Claude TUI in a Wakterm pane
  -> native TUI remains in a PTY
  -> Wakterm uses detection, confirmed adoption, and observed-PTY restoration

Future supervised Claude TUI
  -> native TUI remains the only interactive presentation
  -> mux attaches a structured observer to the same exact provider session
  -> only valid when Claude exposes and supports that topology
```

Do not implement or advertise a Claude ACP launch as an app-server TUI
transport. Unlike the Codex app-server transport, it does not retain the
provider's native TUI. Do not offer a Wakterm-rendered Claude mode as the
normal launch path.

## Remote Control, the Agent SDK bridge, and `--sdk-url`

This section was checked on 2026-08-21 against Claude Code 2.1.238,
`@anthropic-ai/claude-agent-sdk` 0.3.238, the complete official documentation
corpus, and the files published in the official npm package.

Claude Code 2.1.238 exposes the documented `--remote-control` or `--rc` flag:

```sh
claude --remote-control
```

This starts the normal interactive terminal TUI and also makes the same local
session available through claude.ai and the Claude mobile app. Anthropic's
[Remote Control documentation](https://code.claude.com/docs/en/remote-control)
states that terminal, browser, and mobile surfaces can be used at once. The
separate `claude remote-control` command is a non-interactive server that can
host multiple remotely created sessions. It is not a native TUI.

Remote Control demonstrates the product topology Wakterm wants, but its
documented transport is not a local Wakterm integration point:

- the local Claude process makes outbound HTTPS connections to Anthropic
- Anthropic relays messages to claude.ai and the mobile app
- authentication uses short-lived service credentials
- connected transcripts are stored on Anthropic servers
- no private local socket or attach endpoint is exposed

The public [CLI reference](https://code.claude.com/docs/en/cli-reference) and
complete official documentation corpus do not document `--sdk-url`. The only
occurrence in that corpus is a
[changelog entry](https://code.claude.com/docs/en/changelog) about a Remote
Control startup bug. Inspection of the installed binary and a no-model
localhost experiment established that the flag is not the desired side
channel:

```text
--sdk-url <url>
Use remote WebSocket endpoint for SDK I/O streaming
(only with -p and stream-json format)
```

The binary treats `--sdk-url` as non-interactive, requires stream-JSON input
and output, and rejected a localhost endpoint with:

```text
Error: --sdk-url rejected: host "127.0.0.1" is not an approved Anthropic
endpoint. This flag is reserved for Remote Control worker processes connecting
to Anthropic's backend.
```

### Community reverse engineering

The flag and its wire protocol have been implemented outside Anthropic. The
earlier conclusion that it was merely an opaque private protocol was too
strong:

- [Companion](https://github.com/The-Vibe-Company/companion/blob/90804d3d86371184c0854bed40fb13a0013853ca/archived/legacy-companion/WEBSOCKET_PROTOCOL_REVERSED.md)
  documented the NDJSON-over-WebSocket messages, initialization, permissions,
  cancellation, reconnection, and session management. Its browser UI used the
  CLI as a headless WebSocket client.
- [agent-quickstart](https://github.com/lebovic/agent-quickstart) implemented
  a self-hosted `session_ingress`-compatible service, persistent sessions, and
  a browser UI. It launches Claude with a custom `--sdk-url` and resume URL and
  uses an API key rather than Claude subscription OAuth.
- Origin's
  [protocol analysis](https://www.originhq.com/blog/reversing-remote-control)
  independently reversed both Remote Control transports and implemented a
  compatible server.
- [Praxis](https://github.com/originsec/praxis/blob/791d348d3ffe057fff4fa8183b3dc4582c32b1f1/docs/src/connectors/claude-bridge.md)
  implements the WebSocket protocol and the newer HTTP, SSE, worker-state,
  heartbeat, delivery, and epoch protocol.

Arbitrary endpoints worked through Claude Code 2.1.119. Claude Code 2.1.121
added the hostname allowlist, breaking Companion's local bridge as recorded in
[its compatibility issue](https://github.com/The-Vibe-Company/companion/issues/655).
Origin subsequently showed that the client-side restriction could be bypassed
by resolving an approved hostname to a private server and satisfying TLS. Its
[May 2026 follow-up](https://depletionmode.com/all-your-claude-are-belong-to-us-redux/)
also demonstrated a local proxy variant. Praxis packages the hostname
redirection and private-CA approach into its bridge documentation and code.

The installed 2.1.238 binary still contains the same rejection and approved
hostname strings. A no-model test passed hostname validation when given an
approved staging hostname, then stopped at worker registration with
`no_auth_headers`. This confirms that the current guard is still hostname and
credential based. A complete current-version Praxis connection was not run.

These implementations prove that a determined integrator can use
`--sdk-url`. They do not make it a stable or supported product interface.
Pinning an old Claude binary would forfeit security and compatibility updates.
Redirecting an Anthropic hostname deliberately circumvents a guard that
Anthropic added to reserve the transport for its own workers. Either approach
would leave Wakterm coupled to an unversioned internal protocol and moving CLI
checks.

The official Agent SDK package provides more explanation than the product
documentation. Its
[`package.json`](https://unpkg.com/@anthropic-ai/claude-agent-sdk@0.3.238/package.json)
deliberately exports both `@anthropic-ai/claude-agent-sdk/browser` and
`@anthropic-ai/claude-agent-sdk/bridge`. The published
[Browser declarations](https://unpkg.com/@anthropic-ai/claude-agent-sdk@0.3.238/browser-sdk.d.ts)
and
[Bridge declarations](https://unpkg.com/@anthropic-ai/claude-agent-sdk@0.3.238/bridge.d.ts)
describe these as public type surfaces:

- the Browser SDK creates a structured `Query` over SSE or WebSocket and
  accepts session event URLs and caller-provided authorization headers
- the Bridge SDK can create a remote code session, mint worker credentials,
  attach a worker, send structured messages and permission requests, receive
  prompts and interrupts, report lifecycle state, and resume an SSE stream
  from a persisted sequence number
- the Bridge SDK marks this entire surface `@alpha` and says breaking changes
  do not bump the package major version

This changes the narrow conclusion that the protocol is wholly opaque. There
is an exported and typed implementation surface. It does not yet provide a
supported Wakterm supervisor for a native TUI:

1. `--sdk-url` is the headless worker transport. It cannot preserve the native
   TUI. A current CLI cannot connect directly to a Wakterm-owned endpoint; the
   demonstrated routes require an old binary or circumvention of its hostname
   restriction.
2. The Bridge SDK attaches as the session worker, not as a passive observer.
   Fetching bridge credentials bumps the worker epoch, and the declarations
   define an epoch-superseded close for the displaced worker. Attaching it to
   an interactive session could therefore take ownership away from the TUI.
3. The Browser SDK is the surface that could coexist with a TUI in principle,
   but it requires the caller to supply session URLs and authorization. It
   does not provide a documented third-party credential or discovery flow.
4. Anthropic's
   [Agent SDK quickstart](https://code.claude.com/docs/en/agent-sdk/quickstart)
   says third-party products may not offer claude.ai login or subscription
   rate limits unless Anthropic has approved them. API keys and
   `claude setup-token` credentials cannot establish Remote Control sessions.
5. Both SDK surfaces use the Anthropic-hosted Remote Control service. Neither
   is a private local attachment or observation channel.

The practical result is that `claude --remote-control` is a supported way for
a user to keep the native TUI while also using Anthropic's own remote clients.
It does not make Wakterm an authoritative lifecycle observer. Do not build the
normal Wakterm launch path around `--sdk-url`, credential extraction, or the
alpha Bridge SDK.

Revisit the Browser SDK if Anthropic documents and approves a third-party
client authentication flow. Revisit the Bridge SDK only if it gains a stable,
read-only same-session attachment that cannot replace the native TUI worker.
Also revisit this conclusion if Claude exposes a supported local event stream,
plugin hook, or same-session observer API.

## Recommended Wakterm direction

1. Keep the native Claude TUI as the process running in the Wakterm pane.
2. Continue improving confirmed adoption and exact `claude --resume` recovery
   without overstating observed turn authority.
3. Persist the exact Claude session identity and validated native-TUI resume
   specification. Never substitute a fresh session after resume failure.
4. Keep current Claude ACP adapters out of the normal launch path.
5. Watch for a supported Claude protocol that permits a structured supervisor
   and native TUI to attach to the same exact session concurrently.
6. If such an interface appears, put it behind a provider-neutral supervision
   boundary in the mux. It must not move input, rendering, or approvals out of
   the provider TUI.
7. Keep existing Claude PTY detection and adoption unchanged.

The TypeScript-versus-Rust adapter choice is deferred because neither option
satisfies the native-TUI product requirement.

## Promotion gate for a future Claude supervisor

Before making any structured Claude supervisor a supported default, first
prove that it observes the same session used by an independently running native
Claude TUI. Then run the lifecycle tests against the exact packaged components
and installed Claude version:

- initialize once and persist negotiated capabilities and versions
- attach the native TUI and supervisor to the same exact session
- keep all user input, rendering, questions, and approvals in the native TUI
- create at least two sessions with different working directories
- run concurrent turns and attribute every update to the exact session ID
- observe simultaneous permission requests without taking ownership from the TUI
- cancel one turn and immediately start another in the same session
- disconnect the supervisor while the native TUI continues correctly
- close one native TUI while another session continues
- restart the supervisor and reattach to each exact session
- restart the mux and recover each exact session
- reject resume failure without creating a fresh session
- recover once from adapter death without duplicate sessions or panes
- surface partial restore failure per pane
- verify two GUI clients observe the same mux-owned state
- verify clean process shutdown
- run the lifecycle suite on Windows, macOS, and Linux

Version and retain the test results. Passing with one adapter or Claude version
does not establish compatibility with a later release.
