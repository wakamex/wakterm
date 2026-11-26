# Replacing repeated local fixes with general ones

This page tracks classes of problems that Wakterm's commit history shows being fixed one symptom at a time, and the general change that would remove each class. Each item lists the commits that show the pattern, the general change, the cheapest check to run before building it, and its status.

The items come from reading the fork's commit messages through 2026-11-21. An item marked `unverified` has not yet been checked against the code; its check comes first, and an item whose check shows the pattern is not real, or that the general change would not help, is marked `dropped` with the reason rather than built.

Status values: `unverified`, `confirmed` (the check found the problem), `in progress`, `done` (with commits), `dropped` (with the reason).

When a commit advances or settles an item, update its status here in the same commit.

## Order of work

1. Item 5's steps in order, since main-thread stalls reach every client.
2. The possible regressions R2, R5, R6 and R9, each a short check against the code.
3. The harness table in item 7, then its per-harness modules together with item 2.
4. Items 1, 4 and 6, one at a time, each starting with its check.
5. Items 2, 3, 8 and 9, and the remaining regressions.

## 1. Prompt delivery runs through four separate paths

Problem: admission, `wakterm cli agent send`, reminders and interrupted-turn resume each type into an agent's input box with their own checks, retries and refusals. The CLI path is split across several requests, so the server grew protocol data units (PDUs) for each step, and the paths raced into the same box until a lease held across requests serialized them, which then needed its own fixes.

Evidence: `71a9135c9` and `9db2dc4d1` (`agent send` checked fewer startup phases than admission and typed into Claude's trust prompt), `f7be4c10a` (two Enter-retry loops), `ca51bee96` and `d77045130` (`SubmitTypedInput` and `PrepareAgentInputBox` added so the CLI could reach server-side checks), `3a1ea620b` and `f85a060ef` (`taken_back` added because the two paths reported a rollback differently), `6a8d65d24` (the CLI polled `ListAgents` every 50 ms), `6b5bea585` and `e9779b974` (the input lease and its three bugs).

General change: one server-side delivery operation that every caller uses with policy options, serialized per pane by an in-process lock released when the delivery ends. It would delete `PrepareAgentInputBox`, `SubmitTypedInput`, `taken_back`, the connection-held lease with its expiry and revocation, and the CLI's own retry and polling loops.

Check first: list every path that writes a prompt into a pane and compare their pre-checks, retries and refusal codes. If no caller needs to run its own logic between preparing the box and pressing Enter, the split across requests has no purpose.

Status: `unverified`

## 2. Two readers interpret each harness's session data

Problem: the status observer and the event projector each parse a harness's transcript or database, and they disagree about when a turn ends. Observations are also copied into runtime state field by field, and fields were repeatedly lost in the copy.

Evidence: `79e9261d6` (Claude turns ended on tool requests in one reader and on `end_turn` in the other), `d7751d483` and `f669795a5` (the same ZCode and Devin fixes made in both readers), `e9b1a1a54` and `143d44103` (admission and the freezer acted on one reader before the other caught up), `324cf6909` and `537f4cd00` (an idle-report settle timer, then a `reply_owed` flag when it was not enough), `db21bec2f`, `ea23a4dcd`, `f5c9bf813` and `0544ea5b0` (fields dropped or wiped in field-by-field copies). `60ccd4c5b` moved return-final tracking onto provider events and shows the direction.

General change: one projector per harness that emits the durable events and a single turn snapshot from the same parse, stored as one replaced value, with presentation state such as `seen_at` kept apart so replacing runtime state cannot wipe it. Admission, the freezer, return-final and tab badges read the one snapshot.

Check first: compare the record parsing for each harness between `mux/src/agent.rs` and `mux/src/agent_event.rs`, and time seeding the projector from the tail of a large Claude transcript. Worth building if most of the parsing is duplicated and tail seeding stays in the sub-millisecond range `431300ac0` measured.

Also seen live on 2026-11-21: the OpenCode observer reports no observed turn, so its next poll clears the observed turn the projector set when the turn ended.

Status: `unverified`

## 3. Notifications both redraw views and trigger requests

Problem: the client sent requests to the server from handlers of mux notifications, and some mux setters notified even when nothing changed, so applying server state echoed back to the server. Each kind of echo loop was found and fixed separately.

Evidence: `76a169534`, `69fcd28fa`, `daa899b9b`, `a9612626d` and `c13210850` (tab resize echoes, including a two-second timing heuristic later removed), `f499f5776`, `d967c5548`, `3f7b4ad82` and `5486543a6` (focus echoes), `d71160004` and `6e151d79f` (a title loop of about 1,000 round trips a second that froze the Windows GUI), `8e94dd7ae` (a one-second delay to break title echoes), `c05924b01` (workspace echoes), `091f95b92` (a detector for notification loops), `66244d50f` (a resize that changed nothing still notified every client).

General change: the client never sends a request from a notification handler; every user or Lua change goes through an explicit domain method, as titles and workspaces already do. Every mux setter notifies only when the stored value changes.

Check first: find every notification handler in `wakterm-client/src/` that sends a request, and every `notify` in `mux/src/` that is not behind a changed-value check. If none remain, this item is done.

Status: `unverified`

## 4. Clients compute every pane's size when resizing a tab

Problem: on a window, font or zoom resize, the client computes each pane's size and sends the list. The server must then validate the pane set, the client must map local pane IDs to remote ones, zoomed tabs need an exception, and a refused list leaves the tab at the wrong size.

Evidence: `f39b4cc6a` and `5adbc17be` (per-pane batches), `5d94a7885` (`SplitPane.tab_size` for stale sizes), `a5a49b0a8` and `c1deb1a22` (unmapped pane IDs), `bbad72266` and `66244d50f` (validation), `bb981b7b4` (the zoomed-tab exception), `30ca3f1ab` (logging refusals), `e64bc130c` (a retry after resync and a refit when a tab is shown).

General change: for viewport, font and zoom resizes, the client reports only the tab's outer size and the server computes the pane layout from its own tree. Divider drags keep a separate request that sets split ratios. This deletes the client's pane-size collection and ID mapping for these resizes, the server's pane-set exceptions, the retry, the refit and `SplitPane.tab_size`, and gives a place for a rule about which client's size wins.

Check first: after `30ca3f1ab` is deployed, read the server's refused-resize log over several days of use with more than one client. If refusals and size fights stop, the existing fixes suffice.

Status: `unverified`

## 5. One main thread serializes every client, agent and background job

Problem: the mux server applies every client request, agent operation and periodic job on one main thread, so any slow step there, from a scrollback rewrap or a session save to a page fault under host memory pressure, makes every client's keystrokes and rendering wait behind it. Blocking reads and lock inversions were found one outage at a time and moved or fixed individually.

Evidence: `1a9b10dbb`, `455f3cb9d`, `2ff8f7c9d` and `7c9f73db4` (lock-order inversions and re-entry, one hanging `SetClientId` for up to 50 minutes), `6a8d65d24`, `72667323a`, `dcb2d8bd3`, `1055be667`, `ad42b4ccc` and `87ded88ba` (main-thread stalls from `/proc` scans, the `.claude.json` lock and the file watcher), and startup restore spending 7 of its 10 seconds spawning processes on the main thread. On 2026-11-26 two font size steps and a window resize made the GUI send 242 tab resizes in about 15 seconds, each rewrapping a tab's scrollback, and 7 session saves of about a second each followed; with the host at 28 of 31 GB of memory and 68 GB of swap in use, the main thread waited in reclaim and block-layer throttling, and the user's window answered keystrokes up to 7 seconds late for about a minute. The rewraps themselves cost 2 to 20 ms of processor time each; the rest was waiting.

End state: three planes. Each pane is an actor that owns its terminal on a small pool of interactive threads, with a queue for input, rendering of what clients see and visible resizes ahead of a queue for agent screen reads and prompt delivery, so that one pane's flood of output or a slow agent read cannot delay another pane. Windows, tabs, split layouts, client views and agent status are an immutable snapshot replaced whole by one coordinator that does no input or output, so listings and session saves read it without locks. Sockets, disk, `/proc`, SQLite, transcript reading, process spawning and saves run on a separate pool. The authoritative server with mirroring clients, the protocol and its codec guard, the terminal emulation and PTY-backed panes stay as they are.

Steps, each gated on a measurement:

1. Stop doing work for tabs nobody sees and protect the server's memory: the GUI resizes only the tab it shows, the server keeps the newest size requested for a tab connected clients show another tab in place of and applies it when one shows it, session saves wait for 3 seconds without changes, and the server claims 2 GB of memory protection (`MemoryLow=` on the service and `memory.low` on its `wakterm-server` cgroup). Gate: a window resize with about 100 tabs, under host memory pressure, answers keystrokes and render requests within 50 ms in 99 cases out of 100.
2. The layout snapshot: listings, the agent catalog, request routing and session saves read it instead of locking the mux. Gate: the save's "reading the mux" share falls from up to 1,940 ms to under 5 ms, and concurrent saves, resizes and client attaches show no lock-order inversion under stress.
3. Pane actors and the background pool, replacing the per-request hop to the main thread one request type at a time; the mux crate is shared by the GUI and the server, so each move covers both. Gate: with one pane's output flooding and the disk stalled for 2 seconds, another visible pane answers keystrokes and renders within 10 ms in 99 cases out of 100.

Deferred until measured: rewrapping only the visible rows on resize, since processor time was a small part of the slow resizes; and spilling hidden scrollback to disk, since the server's 1 GB was not the host's memory pressure. A default memory limit for agent jobs, which caused that pressure, belongs to `agent-job`, not Wakterm.

Stop: moving one call at a time off the main thread, and the lock-order checker, since step 2 removes the locks it would check. `mux/src/main_thread_blocking.rs` and the slow-request logs stay until step 3 lands, as the gates are measured with them.

Status: `in progress`. Step 1 is built, except that its gate has not been measured live.

## 6. Freezing idle agents fights prompt delivery

Problem: idle agents are frozen in control groups (cgroups) to save memory, and a thawing agent is slow to accept input, so delivery grew longer waits and retries. Managed Codex prompts are still typed into the pane even though Wakterm holds its app-server connection.

Evidence: `3e6b477f2` (freezing, after 44 idle Claudes held 7.5 GB), `ca51bee96` and `6193c7a89` (an output-settle wait stretched to 10 s for thawing agents), `71e475cea` (a thawed Claude was frozen again with the prompt in its box), `e17eea6b7` (Enter retries stretched to 3 minutes during compaction), `143d44103`, `bbad72266`, `495510c23` and `448f92d20` (waits for a discarded draft stretched to 30 s), `74544e791` and `91850619b` (managed Codex prompts checked by reading the screen, and a 10-minute watcher that presses Enter on a steer left in the composer).

General change: deliver managed Codex prompts and steers through the app-server connection. Keep parked restored agents dormant, with their identity saved and no process, and start them when the tab is viewed or unparked, when a prompt or reminder arrives, or when an interrupted turn is resumed, instead of running and freezing them.

Check first: send one `turn/start` and one `turn/steer` to a live managed Codex pane over the app-server connection and see whether its TUI shows them cleanly; and measure the memory and swap held by parked versus unparked agents, and how often parked tabs wake, over a day.

Status: `unverified`

## 7. Harness support is spread across separate lists

Problem: each harness's matchers, commands, help text, icons and special behavior are listed separately, and adding a harness repeatedly missed one of them. The tab navigator encodes every combination of harness icons as its own code point, 2^N of them, in the Unicode [Private Use Area](https://www.unicode.org/faq/private_use.html) (PUA): nine harnesses use 511 of the 534 free code points there, so a tenth will not fit.

Evidence: `3db8d82cb`, `c5e0d343f`, `a5e87898f` and `da16c158c` (the icon range collided with powerline glyphs, missed a harness, and moved again), `ec21d4730`, `1deaa801a` and `fa70c30bd` (lists that missed Devin or ZCode), `8309050f5` and `42426c349` (exemptions for harnesses that survive a mux restart), `4a990a7dc`, `53f36ef5d`, `d90d2151c` and `a87b11b60` (four fixes for a fresh agent with no session until its first input), `c152548c1` and `20d3ec1f5` (argument matching that took OpenCode for Claude, and a misspelled variable).

General change, in three steps, each worth doing on its own:

1. Icons: a tab's harness icons stay one overlapping glyph three cells wide, but each set of icons gets a code point the first time the GUI shows it, so the range holds only the sets in use, and the harness order and shapes come from `TabHarnessIcon` alone.
2. One harness table: a `HarnessSpec` row per harness holding its data (program names, title prefixes, wrapper programs, launch and resume style, environment to drop, capabilities such as restorable and session-after-first-input), with `AgentHarness::KNOWN` listing them in detection order, replacing the separate name tables, the copied restore-capability lists and the catch-all `_ =>` arms on `AgentHarness`.
3. One module per harness, `mux/src/harness/<name>.rs`, each with a single session reader, done together with item 2 one harness per commit rather than as a move of all harnesses first, which would relocate code about to be deleted and fix nothing. The first commit builds the seam: a reader that returns the provider cursor, the events and one turn snapshot from the same parse, and runtime state replaced as a whole with presentation fields such as `seen_at` kept apart. Devin comes first, since both its readers already load the same message list and its observer starts a new turn at a steer, which its projector treats as input to the running turn; then Muse, OpenCode and ZCode, Gemini, agy, Claude and Codex. Shared code stays small helpers (process environment lookup, SQLite file lists, argument parsing), not a generic transcript or database framework. Claude gets one forward state machine with two entries: a seed that reads back from the end of the transcript up to a byte limit and replays those lines without emitting their events, and an incremental read from the cursor. Its largest transcript on the host is 560 MB with 101,162 records: a full forward parse takes seconds and the last 256 KB milliseconds.

Keep the detection order of the two inference functions when they become one loop, and keep `AgentHarness`'s variant order, which the protocol depends on.

Status: `in progress`. Step 1 is done: stacked icon code points are given on demand, so the icon range no longer limits the number of harnesses, and the stacks draw as before. Step 2 is done: `HarnessSpec` in `mux/src/agent.rs` holds each harness's name, launch command, programs, wrapper programs, title prefixes, restore kind and whether its turns survive a mux restart, and detection, launch commands, restore checks, the endchat exemptions, and the name tables in admission, the mux and the CLI read it. Step 3 has started with Devin: its observer and its event projector now decide turns by one rule, `devin::TurnTracker` with `devin::latest_turn`, so a steer stays part of the running turn for both, as a test checks. Devin's code now lives in `mux/src/harness/devin/`: its session and database reading, its turn rule, its observer (`devin::observe`) and its event projector (`devin::project`), with its prompt reading in `devin/prompt.rs`. One shared read of the session per poll was not built for Devin: a second walk of its longest conversation on the host, 36 messages, takes 0.32 ms on the observer thread. Build such a read only for a harness whose per-poll reading shows up in the observer or main-thread timings, Claude being the likeliest. Muse is done the same way: `muse::TurnTracker` holds its turn rule for both readers, and its code lives in `mux/src/harness/muse/`, with the projector's per-record logic in `muse::project_record` on top of the shared incremental reader of JSONL session logs, which stays in `agent_event.rs` for the harnesses that use it. OpenCode and ZCode follow one rule as well, `opencode::starts_turn` and `opencode::turn_end` with `opencode::reply_to`: a turn is a user message and ends with a reply to it that stopped, or a provider failure. Their code now lives in `mux/src/harness/opencode/`: database and session discovery, the observer (`opencode::observe`), the event projector (`opencode::project`) with its cursor, and the input box reading. Gemini follows one rule as well, `gemini::starts_turn` and `gemini::ends_turn`: a turn is a user message and ends with a reply from Gemini that calls no tools; its code now lives in `mux/src/harness/gemini/`: finding its project and chat files, reading a conversation, the observer (`gemini::observe`) and the event projector (`gemini::project`) with its cursor. A live Gemini run on 2026-11-21 kept the agent working through a 20-second tool step, and found that Gemini records a tool's result, and the context it injects when a session opens, as user messages; `gemini::starts_turn` now counts only user messages with text other than that context, so a tool result no longer starts a turn of its own. It also showed the projector re-arming its baseline when Gemini rewrites its session file mid-turn. agy follows one rule as well, `agy::TurnTracker`, settled from 996 agy transcripts on the host: a turn starts at user input, or at a system notice that arrives while no turn runs, such as a background task finishing, which in every case wakes agy to work; it ends at a finished response that calls no tools, text or not; and a step that errors ends nothing, since agy went on after every one of them. Its code now lives in `mux/src/harness/agy/`: finding its root and the session a process owns, the observer (`agy::observe`), the projector's handling of each step (`agy::project_record`) on the shared JSONL reader, and the input box and feedback survey readers. A live agy run on 2026-11-22 confirmed the rule, including a turn a system notice starts: agy ran a command as a background task, answered "Waiting for the command to finish.", which ended the user's turn, and then answered "finished" in the turn the task's notice started; both readers named the same two turns. That gave a return-final request agy's interim answer, so the rule now keeps a turn open while a background task it started, a step agy marks `RUNNING`, waits for its notice: an answer given meanwhile is interim, and the answer after the notice ends the turn. Since 39 of 773 background steps in 499 full transcripts got no notice, a turn that waits more than 30 minutes after its interim answer ends with that answer; all but one of 338 measured waits ended within 30 minutes (median 19 seconds). Both readers apply that limit at each step's own time as well as the clock's, so a notice after it starts a new turn for both; agy runs commands in the background as `RUN_COMMAND` steps as well as `GENERIC` ones, and all 148 of those got a notice, 43 after an interim answer. A replay of agy's 498 stored conversations (34,486 steps), with the clock set to each step's time, finds the two readers agree at every step. Claude and Codex are next, Claude first with its single reader seeded from the end of the transcript. A live run of Devin, OpenCode and ZCode on 2026-11-21 confirmed the rules: OpenCode stayed working through a 20-second tool step, a Devin steer kept its turn in both readers, and an interruption and a provider failure ended turns. It also found two differences left between the readers: Devin named a turn by its node, which a chain rebuild changes, so the projector and the observer could name one turn differently (both now use the message ID); and on an OpenCode or ZCode provider failure the observer ends the turn while the projector only records a recoverable failure. The projector now ends such a turn too, with an aborted final whose detail is fixed text, as Codex's is, while the provider's message stays in the failure event. A replay of every session stored on the host through both readers, step by step (`replay_stored_sessions` in `mux/src/agent_event_replay.rs`, run by hand), compares whether a turn runs, which turn it is and its final answer after each step: 27 Muse sessions (1,679 steps), 53 OpenCode sessions (2,067) and 4 ZCode sessions (284) now agree everywhere, and the 15 disagreements before the provider-failure change were all provider failures. Devin cannot be replayed this way, since a chain rebuild rewrites a conversation's rows and the rows the readers saw before it are gone. The Gemini replay reads a chat only between Gemini CLI's synchronous writes, since it records a reply that calls tools first without its tool calls and the event projector would end the turn on that version; a live poll could land between those writes only within microseconds, so the projector does not wait for a reply to settle unless a Gemini final is seen before its turn's tool calls. A live Panetone run on 2026-11-22, with one request per harness that asks for a reply, got the final answer back from OpenCode, Devin after an approval, and agy through a background task, whose interim answer came as progress; ZCode's out-of-credit provider failure ended its turn with an aborted final; and a new Gemini session, holding only the context Gemini injects, was refused as not idle, so `gemini::read_last_observation` now reports a session with no open turn as waiting for input. A mux restart during a turn of agy, Devin and OpenCode on 2026-11-22 found three gaps, now fixed: agy's idle prompt while it waits for a background task made the mux end the turn as stopped on screen, so agy's observer now reports the phase `background_task` and the screen rule leaves such a turn alone, unless all its steps predate the pane's agy process, whose restore ended the task; prompt admission refreshed an agent from its records without the screen rules the observer applies, so it refused to resume a Devin turn the observer showed stopped, and both now apply `Mux::agent_screen`; and OpenCode's records keep a turn a restart killed running, so its screen, which shows a spinner and its interrupt hint under the input box only while a turn runs, now ends such a turn too. Gemini is now restored too, with `gemini --resume` and the session UUID its chat file begins with. Claude's event projector and the seed of its cursor now follow one turn rule, `claude_user_record`: in 486 Claude transcripts from six days on the host, 1,333 of 25,546 turns were left open when the projector started another turn in the same session, because notes Claude adds to a running turn (604) and interruptions started turns of their own, and input after a provider error, a local command or a refusal never ended the stopped turn. Notes now continue the running turn, an interruption ends it, input while a turn runs steers it (288 cases), and input after a stopped turn ends that turn first (279). The status observer still judges a Claude turn by its latest input and reply times; the single reader seeded from the end of the transcript remains to be built.

## 8. Agent logic reads the host directly

Problem: agent, restore and gate code reads the environment, `/proc`, `/etc/machine-id` and the runtime directory, uses process-wide counters, and starts real child processes in tests, which breaks CI on Nix, macOS and Windows about twice a week.

Evidence: `c762814bf` and `bba72352b` (a missing machine ID and a poisoned environment lock failing dozens of tests), `d70428695` and `24f12f8e7` (process-wide counters and schedulers), `d83f89bce` and `b0f110226` (the real runtime directory and shell), `14739d293`, `9e702c57d`, `d05a08251` and `9f97fbe45` (tests that spawn `sleep` or a shell), `7db413127`, `78146f0ea`, `a4f5097ed`, `670ab2f44`, `276492934` and `29d692d6f` (platform assumptions), `8724c9d2f` and `d5f8d5521` (timer granularity and SQLite under parallel tests).

General change: pass host access in through a small interface with an in-memory version for unit tests, scope pane ID counters to each mux, and keep real `/proc` and process tests in a small tagged Linux suite. A single sanitized base environment built at mux startup would also replace the variable-by-variable scrubbing in `5eac174ec`, `d40862b83` and `84190a623`.

Check first: count the users of the environment lock and the tests that start `sleep` or `sh` in `mux/src/`, and how many of them test logic rather than `/proc` parsing.

Status: `unverified`

## 9. One pane can exhaust the host's memory

Problem: every program the mux starts shares one unlimited `wakterm-panes` cgroup, so a single runaway program can use all of the host's memory and swap. On 2026-11-21 an agent's test grew to about 20 GB on the 31 GB host twice; before each out-of-memory kill the whole host stalled for minutes swapping, with a load average of 265, which stalled every agent and client. The mux server itself stayed responsive in its no-swap cgroup.

Evidence: kernel out-of-memory kills of `block_probe-251` at 17:38:30 and 17:43:00 on 2026-11-21, both charged to `wakterm-panes`; `6207c1241` created that cgroup without a limit.

General change: give `wakterm-panes` a `memory.high` some gigabytes below the host's RAM, set by `protect_server` from the host's memory size, so the kernel reclaims and slows the panes as a group before the host as a whole runs short. A runaway program then slows the agents and fails on its own limits instead of freezing the machine. A per-pane or per-agent limit is a later refinement only if one agent slowing the others proves to be a problem.

Check first: measure the panes' normal combined peak over a few days from `wakterm-panes`' `memory.peak` and `memory.current`, and confirm a limit above that peak leaves headroom for the host's other services; then reproduce a runaway allocation in a pane with the limit set and compare host responsiveness with and without it.

Status: `unverified`

## Possible regressions

Fixes that a later commit appears to have undone, or a change that may have broken an earlier one.

| ID | Possible regression | Check | Status |
| --- | --- | --- | --- |
| R1 | The process sampler keeps a process's old executable and arguments after it calls `exec` with non-empty arguments; `4d547c837` re-reads them only when they were empty. | Read the sampler's cache rule; test a shell that `exec`s another program. | `unverified` |
| R2 | `535b1864b` cut a retried admission's wait for an in-flight request from 5 minutes to 10 s during a hang whose cause `6164b44c3` then fixed, and the wait was not restored, though `e17eea6b7` lets an admission run for 3 minutes. | Read the current wait and whether a retry within 3 minutes gets a real receipt. | `dropped`: the 10 s cap stands on its own. A repeated request answers indeterminate, saying the first attempt still runs, well within the caller's deadline, and the caller repeats later, which also fits admissions that run for 3 minutes. |
| R3 | `a6635ed71` removed `1ec92f99b`'s guard against typing into Claude's `!` shell mode without another check for that mode. | Put Claude in `!` mode and see whether admission types into it. | `unverified` |
| R4 | `2fe9827e7` replaced following Claude's `continued-in` records, and a fork made while the window stays attached may no longer be followed. | Fork a Claude session in place and see whether the observer follows it. | `unverified` |
| R5 | `6867187d1` compares a finished turn with when the window first saw the tab parked, so a turn that ends while the GUI is disconnected never unparks its tab. | Read where the parked time is recorded and test a turn that ends while disconnected. | `dropped` as a regression: unpark-on-turn-end is decided in the GUI's memory, so a GUI that starts, or remaps tabs after reconnecting, records its baseline then, and turns that ended while it was away never unparked under the earlier rule either. Recording the parked time on the server would lift that limitation if it is wanted. |
| R6 | `118b6e944` sends End to a Claude pane to learn whether its input box is empty, which moves the user's cursor during a read-only check. | Read when the probe runs and whether it can fire while the user is typing. | `dropped`: the End probe runs only while a prompt is being delivered, only with the caret at the box start and text after it, and only when the pane has had no recent input; a real draft is then set aside before the prompt is written, so it never moves the cursor of someone typing. |
| R7 | The macOS draw throttle disabled in `1f30d77b9` and the invalidate fast path reverted in `f132de6b6` were not redone. | Decide whether macOS still needs them. | `unverified` |
| R8 | `ActivateNextTabNeedingAttention`'s command description may still disagree with its behavior after `434610d3b`, `48168a424` and `74b9e1bbb`. | Compare the description with the code. | `unverified` |
| R9 | `ba4aff4fb` added fields to serialized agent events and approval resolutions without bumping `CODEC_VERSION`. | Compare the codec version history with protocol type changes since. | `done`: bumped to 74 in `7551a26cb`, so clients built before these changes are refused at the handshake, and the codec test `a_protocol_change_comes_with_a_codec_version_bump` now fails when a protocol type's shape changes without a bump. Found: `CODEC_VERSION` was bumped to 73 on 2026-10-17 (`b97adbf68`), but since then `f85a060ef` added the `SubmitTypedInputResponse` PDU and `a4fbc9b97`, `ee1173498` and `5a4015c4c` appended the `Devin`, `Muse` and `Endchat` harnesses, which travel in agent metadata, without a bump. Appended variants are not misread, but a client built before them fails to decode an agent list that contains them, and the handshake still accepts it. General fix: a test that records the protocol types' serialized shapes in a committed file and fails when they change without a codec version bump. |
| R10 | The `protect_server` startup call, lost between `6207c1241` and `51fc0490b`, is guarded only by a warning in `deploy.sh`. | Decide whether a startup assertion or test should guard it. | `unverified` |

## Ideas weighed against the history

- Server-computed tab sizing: the history supports it; it is item 4.
- Starting panes directly in their cgroup with `clone3`'s `CLONE_INTO_CGROUP`: the history shows no memory problem from joining the panes' cgroup after fork. The reason to consider it is the restore time since the protection returned (3.0 s before, 11.0 s on 2026-11-21); measure where that time goes before building anything.
- Starting parked agents only when viewed: the history supports dormant parked agents, but they must also start for prompts, reminders and interrupted-turn resumes; it is part of item 6.
