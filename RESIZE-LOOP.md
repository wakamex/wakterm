# Resize Bug Execution Loop

## Objective

**Zero split tree invariant violations under any resize sequence.**

This means:
- correctness beats performance: a resize path that's always correct is worth more than one that's fast but drifts
- reproducible beats anecdotal: every bug needs a unit test before a fix
- defensive beats optimistic: invariants should be enforced at every mutation site, not assumed from callers

## Priority Order

When choosing work, prefer:
1. live violations — bugs visible in current session data right now
2. reproduction — converting a live violation into a deterministic unit test
3. root cause — understanding WHY the invariant broke (which code path, which interleaving)
4. fix — code change that makes the failing test pass
5. hardening — adding reconciliation/validation to other mutation sites

Do not write fixes for bugs you cannot reproduce in a test.

## The Loop

Once started, run autonomously. Do not pause to ask the user if you should continue.

```
LOOP:
  1. Detect
     - run: wezterm cli list --format json | python3 track-pane-sizes.py --once
     - or:  wezterm cli list --format tree | python3 -c "..." (tree-level check)
     - or:  cargo test -p mux --lib tab::test (existing unit tests)
     - record any violations in experiments/violations/

  2. Pick a violation
     - choose the simplest unresolved violation (fewest panes, smallest delta)
     - define the invariant that's broken BEFORE writing code

  3. Reproduce in a unit test
     - add a test to mux/src/tab.rs that:
       a) constructs the layout (use make_l_shaped_tab or build a new helper)
       b) simulates the mutation sequence that caused the violation
       c) asserts the invariant IS broken (assert_ne! on raw pane sizes)
       d) asserts the fix restores it (assert! on check_tree_invariants)
     - the test MUST fail without the fix and pass with it

  4. Commit the failing test
     - prefix: RESIZE: add test for <violation description>
     - cargo test must compile (test itself will fail, that's expected)

  5. Write the fix
     - prefer the smallest change that makes the test pass
     - common fix sites:
       - reconcile_tree_sizes() — top-down constraint enforcement
       - rebuild_splits_sizes_from_contained_panes() — bottom-up from pane sizes
       - adjust_y_size() / adjust_x_size() — delta distribution
       - apply_pane_size() / cascade_size_from_cursor() — divider drag cascade
     - do not fix what isn't tested

  6. Verify
     - cargo test -p mux --lib tab::test — ALL tests must pass
     - wezterm cli list --format json | python3 track-pane-sizes.py --once
       (re-check live session if available)

  7. Keep or discard
     - KEEP: test passes, no regressions → commit with prefix RESIZE: fix <description>
     - DISCARD: test still fails or introduces new failures → git reset
     - PARTIAL: test passes but live violations remain → keep, loop back to step 1

  8. Push on success
     - after every KEEP commit: git push origin <branch>
     - push target is always origin (wakamex fork), NEVER upstream (wezterm/wezterm)
     - if push fails (network, auth), note it and continue — don't block the loop

  9. Update RESIZE-TASKS.md if the result changes a durable conclusion

  10. Go to 1
```

## Phase Commands

**Detection (live session):**
```bash
# One-shot check
wezterm cli list --format json | python3 track-pane-sizes.py --once

# Continuous monitor (logs violations as they occur)
python3 track-pane-sizes.py --interval 1 --json > experiments/violations/monitor.jsonl &

# Tree-level dump for manual inspection
wezterm cli list --format tree | python3 -m json.tool
```

**Reproduction (unit tests):**
```bash
# Run all resize-related tests
cargo test -p mux --lib tab::test

# Run a specific test
cargo test -p mux --lib tab::test::interleaved_pdus_break_pane_size_invariant
```

**Stress testing:**
```bash
# Automated divider hammering (uses adjust-pane-size CLI)
./stress-resize.sh --rounds 100

# Direct per-pane resize (exercises Pdu::Resize path)
wezterm cli resize-pane --pane-id 5 --rows 30 --cols 80
```

**Interleaving reproduction (scripted):**
```bash
# Create L-shaped layout
RIGHT=$(wezterm cli split-pane --right --percent 50)
BOT=$(wezterm cli split-pane --pane-id $RIGHT --bottom --percent 50)
wezterm cli adjust-pane-size --pane-id $RIGHT --direction Down --amount 10

# Send interleaved resize PDUs (simulates two rapid events)
wezterm cli resize-pane --pane-id 0 --rows 90 --cols 80
wezterm cli resize-pane --pane-id $RIGHT --rows 50 --cols 40
wezterm cli resize-pane --pane-id $BOT --rows 35 --cols 40  # stale from event 1

# Check
wezterm cli list --format json | python3 track-pane-sizes.py --once
```

## Invariants

Every split node in the tree must satisfy:

| Split direction | Constraint | Why |
|---|---|---|
| Horizontal | `first.rows == second.rows == allocated.rows` | Both sides of a vertical divider have the same height |
| Horizontal | `first.cols + 1 + second.cols == allocated.cols` | Widths sum to parent minus divider |
| Vertical | `first.cols == second.cols == allocated.cols` | Both sides of a horizontal divider have the same width |
| Vertical | `first.rows + 1 + second.rows == allocated.rows` | Heights sum to parent minus divider |

These are checked by `check_tree_invariants()` in the test module.

## Key Code Paths

All in `mux/src/tab.rs` unless noted:

| Function | What it does | Bug risk |
|---|---|---|
| `adjust_y_size()` | Distributes row delta through tree (window resize) | Low — preserves sum invariant |
| `adjust_x_size()` | Distributes col delta through tree | Low — same |
| `apply_pane_size()` | Sets child sizes from parent allocation (divider drag cascade) | Low — uses saturating_sub |
| `reconcile_tree_sizes()` | Top-down constraint enforcement after rebuild | **The fix** — call site matters |
| `rebuild_splits_sizes_from_contained_panes()` | Bottom-up from actual pane sizes (mux server) | **High** — can break invariants |
| `resize()` (TabInner) | Entry point for window resize | Medium — orchestrates adjust + apply |
| `resize_split_by()` | Entry point for divider drag | Low |
| `Pdu::Resize` handler (sessionhandler.rs) | Server processes per-pane resize | **High** — interleaving source |
| `ClientPane::resize()` (clientpane.rs) | Client spawns async resize PDU | **High** — independent async tasks |

## Metrics

- **Primary:** `cargo test -p mux --lib tab::test` — all tests pass (binary)
- **Secondary:** `track-pane-sizes.py --once` — violation count in live session (should decrease over time)
- **Regression:** existing `tab_splitting` test must never break

## Verification Protocol

For every code change:
1. `cargo test -p mux --lib tab::test` — must pass
2. `cargo check -p wezterm` — must compile (catches downstream breakage)
3. Commit before running experiments (so discard = clean git reset)

For changes that touch `rebuild_splits_sizes_from_contained_panes` or `reconcile_tree_sizes`:
4. Run `track-pane-sizes.py --once` against a live session if available
5. Run `stress-resize.sh --rounds 50` if a mux server is accessible

## Remotes

- `origin` = `git@github.com:wakamex/wezterm.git` (your fork — push here)
- `upstream` = `git@github.com:wezterm/wezterm.git` (upstream — NEVER push here)
- Always `git push origin <branch>`. If you ever see `upstream` in a push command, STOP.

## Commit Rules

- Prefix resize-loop commits with `RESIZE:`.
- Failing tests get their own commit: `RESIZE: add test for <violation>`.
- Fixes get a separate commit: `RESIZE: fix <description>`.
- Include the key delta if it matters: `RESIZE: fix column overflow (was +1, now 0)`.
- Do not commit fixes without a test that exercises them.

## Decision Rules

- If a test clearly fails without the fix → write the fix.
- If a fix introduces new test failures → discard and try a different approach.
- If a live violation can't be reproduced in a unit test → add logging to narrow down the code path.
- If a code path is too complex to test directly → extract the logic into a testable function first.
- If the violation involves protocol-level interleaving → use `wezterm cli resize-pane` to script it.

## Run Storage

Store run detail under `experiments/`:

- `experiments/violations/*.jsonl` — violation records from track-pane-sizes.py
- `experiments/tree-dumps/*.json` — tree snapshots from `wezterm cli list --format tree`
- `experiments/results.tsv` — experiment log (not committed to git)

Results TSV format (tab-separated):
```
commit	tests_pass	live_violations	status	description
a1b2c3d	5/5	6	keep	baseline: reconcile_tree_sizes in rebuild
b2c3d4e	6/6	4	keep	add reconciliation to resize() path too
```
