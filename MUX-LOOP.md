# Mux Bug Execution Loop

## Objective

**Stable, correct mux server behavior under all client operations.**

This means:
- no deadlocks, crashes, or OOM from any sequence of client actions
- pane/tab/window topology stays consistent across connect/disconnect/resize cycles
- protocol-level issues (PDU interleaving, unbounded allocation, stale state) are defended against at the server
- reproducible beats anecdotal: every bug needs a test before a fix

## Scope

This loop covers bugs in the mux server and client domain code:
- `mux/` — tab, pane, window, domain abstractions
- `codec/` — PDU types and serialization
- `wezterm-client/` — client-side pane/domain implementation, PDU sends
- `wezterm-mux-server-impl/` — server-side PDU handlers
- `wezterm-gui/` — GUI resize, focus, and window management (where it triggers mux operations)

Out of scope: rendering, font, input handling, wayland/platform-specific issues (unless they trigger mux bugs).

## Priority Order

When choosing work, prefer:
1. crashes and deadlocks — these lose user data and require force-kill
2. OOM / unbounded allocation — progressive failure, hard to diagnose
3. state corruption — pane sizes, topology, focus diverge from reality
4. resize/redraw storms — excessive PDU traffic causing visible glitches
5. protocol hardening — defensive checks, bounds, timeouts

## The Loop

Once started, run autonomously. Do not pause to ask the user if you should continue.

```
LOOP:
  1. Pick a hypothesis
     - choose an issue from MUX-TASKS.md or detect a live problem
     - state the hypothesis clearly: "I think X happens because Y"
     - define what would confirm or refute it BEFORE investigating

  2. Investigate
     - read the relevant code paths (see Key Code Paths below)
     - trace the call chain from trigger to symptom
     - look for: missing error handling, unbounded loops, race conditions,
       async fire-and-forget without coordination, state mutations without
       invariant enforcement
     - use gh cli to read issue comments for reproduction steps
     - check git blame for recent changes that might have introduced the bug

  3. Reproduce — EVERY FIX NEEDS A FAILING TEST
     - write a unit test that FAILS without the fix and PASSES with it
     - for unit tests: construct the scenario in the relevant test module
     - for protocol-level bugs: test the codec/handler directly
     - for timing/race bugs: simulate the interleaving in a test (see
       make_interleaved_resize_state pattern)
     - verify the test actually fails by temporarily reverting the fix
     - if you truly cannot write a test (e.g., deadlock requires threads):
       document what you tried and why, commit investigation notes instead
     - the bar is: "could someone remove the fix and this test would catch it?"

  4. Commit the failing test FIRST, then the fix SEPARATELY
     - test commit: MUX: add test for <description>
     - the test commit must compile but the test itself should fail
       (or be marked #[ignore] with a comment explaining why)
     - fix commit: MUX: fix <description> (#issue)
     - this makes it trivially verifiable: cherry-pick test → fails,
       cherry-pick fix → passes

  5. Write the fix
     - prefer the smallest change that addresses the root cause
     - common fix patterns:
       a) add bounds/limits (max PDU size, max retries, timeout)
       b) add invariant enforcement (reconcile after mutation)
       c) make operations idempotent (tolerate duplicate/stale messages)
       d) add coordination (batch async operations, generation counters)
       e) add defensive early returns (skip invalid state instead of panicking)
     - do not fix what isn't tested (exception: obvious one-line safety fixes)

  6. Verify
     - cargo test -p mux (all mux tests)
     - cargo check -p wezterm -p wezterm-mux-server-impl (full compile)
     - if touching protocol: cargo check -p codec -p wezterm-client
     - if a live session is available: manual smoke test

  7. Keep or discard
     - KEEP: tests pass, no regressions → commit with prefix MUX: fix <description>
     - DISCARD: tests fail or regressions → git reset
     - INCONCLUSIVE: can't reproduce but found useful information → commit notes

  8. Push on success
     - after every KEEP: git push origin <branch>
     - push target is always origin (wakamex fork), NEVER upstream
     - if push fails, note it and continue

  9. Update MUX-TASKS.md
     - record findings, durable conclusions, and state changes
     - keep it concise — per-run detail goes in experiments/

  10. Go to 1
```

## Investigation Protocol

When exploring a new hypothesis:

### Reading code
```bash
# Find where a PDU is handled on the server
grep -n 'Pdu::FooBar' wezterm-mux-server-impl/src/sessionhandler.rs

# Find where the client sends a PDU
grep -rn 'rpc!(foo' wezterm-client/src/client.rs

# Trace a function through the call chain
grep -rn 'fn foo_bar' mux/src/ --include='*.rs'

# Check git blame for recent changes
git log --oneline -20 -- mux/src/tab.rs
git blame mux/src/tab.rs -L 100,120
```

### Reading issues
```bash
# Get issue body and comments
gh api repos/wezterm/wezterm/issues/7540 --jq '.body'
gh api repos/wezterm/wezterm/issues/7540/comments --jq '.[] | "[\(.created_at[:10])] \(.user.login): \(.body[:200])"'

# Search issues by keyword
curl -s "https://api.gitrep.fyi/v1/repos/wezterm/wezterm/issues?title=deadlock&state=open" | python3 -m json.tool

# Get related issues
gh api "search/issues?q=mux+deadlock+repo:wezterm/wezterm&per_page=10" --jq '.items[] | "#\(.number) \(.title)"'
```

### Checking for existing fixes
```bash
# Search for PRs that touch the same code
gh api "repos/wezterm/wezterm/pulls?state=all&per_page=20" --jq '.[] | select(.title | test("mux|resize|pdu"; "i")) | "#\(.number) [\(.state)] \(.title)"'

# Check if an issue was referenced in commits
git log --all --grep='#7540' --oneline
```

### Reproducing with CLI
```bash
# Create a test layout
wezterm cli split-pane --right --percent 50
wezterm cli split-pane --bottom --percent 50

# Simulate operations
wezterm cli resize-pane --pane-id 1 --rows 30 --cols 80
wezterm cli adjust-pane-size --pane-id 1 --direction Down --amount 5
wezterm cli kill-pane --pane-id 2
wezterm cli activate-pane-direction Up

# Monitor state
wezterm cli list --format json | python3 track-pane-sizes.py --once
wezterm cli list-clients
```

### Adding instrumentation
When a bug can't be reproduced in a unit test, add temporary logging:
```rust
log::warn!("DEBUG: foo_bar called with pane_id={} size={:?}", pane_id, size);
```
Build with `cargo build --release`, restart the mux server, trigger the bug,
then check logs. Remove instrumentation before committing the fix.

## Key Code Paths

### Tab/Pane/Window topology (mux/)

| File | Key functions | Risk areas |
|------|--------------|------------|
| `tab.rs` | `resize`, `rebuild_splits`, `split_and_insert`, `remove_pane`, `sync_with_pane_tree` | Tree invariants, interleaving |
| `lib.rs` (Mux) | `add_tab_to_window`, `remove_tab`, `prune_dead_panes`, `focus_pane_and_containing_tab` | Topology mutations, lock ordering |
| `domain.rs` | `attach`, `detach`, domain lifecycle | State cleanup on disconnect |
| `client.rs` | `ClientId`, `ClientInfo`, focus tracking | Multi-client coordination |

### Protocol (codec/, wezterm-client/, wezterm-mux-server-impl/)

| File | Key functions | Risk areas |
|------|--------------|------------|
| `codec/src/lib.rs` | PDU definitions, `CODEC_VERSION` | Backwards compatibility |
| `wezterm-client/src/client.rs` | `rpc!` macro, `resolve_pane_id`, PDU sends | Async fire-and-forget |
| `wezterm-client/src/domain.rs` | `resync`, `resync_coalesced`, `attach` | Resync storms, deadlocks |
| `wezterm-client/src/pane/clientpane.rs` | `resize`, `process_unilateral` | PDU interleaving |
| `wezterm-mux-server-impl/src/sessionhandler.rs` | All `Pdu::*` handlers | Unbounded allocation, missing error handling |

### GUI triggers (wezterm-gui/)

| File | Key functions | Risk areas |
|------|--------------|------------|
| `termwindow/resize.rs` | `apply_dimensions` | Resize cascade |
| `termwindow/mod.rs` | window event handling | Focus, tab switching |

## Metrics

- **Primary:** `cargo test -p mux` — all tests pass
- **Secondary:** no crashes/hangs during manual testing with mux server
- **Regression:** all existing tests in the repo must pass

## Remotes

- `origin` = `git@github.com:wakamex/wezterm.git` (your fork — push here)
- `upstream` = `git@github.com:wezterm/wezterm.git` (upstream — NEVER push here)

## Commit Rules

- Prefix mux-loop commits with `MUX:`.
- One hypothesis per commit when investigating.
- Include the issue number: `MUX: fix deadlock on detach (#7661)`.
- Do not commit fixes without a test (unless it's a one-line defensive fix).

## Decision Rules

- If a bug is reproducible → write the test, then the fix.
- If a bug is not reproducible but the code is clearly wrong → fix it defensively, add a comment explaining why.
- If a bug requires protocol changes → create a new branch, bump codec version.
- If investigation is inconclusive after 30 minutes → document findings, move to next hypothesis.
- If two bugs share a root cause → fix the root cause, test both symptoms.
