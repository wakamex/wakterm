# Resize Tasks

Execution loop and verification protocol: [RESIZE-LOOP.md](./RESIZE-LOOP.md).

This file is a compact decision log. Keep durable conclusions, current state, and live work items. Put per-run detail in `experiments/`.

## Retrospective: Why Pane Sizes Diverge

Nested split pane sizes have been drifting for years. At least 4 open issues report the same family of bugs (#6052, #5011, #4878, #5117), plus related flickering/crash issues (#6885, #7540, #7527). None have been fixed upstream. Here is an honest accounting of the root causes.

### 1. Per-pane resize PDUs interleave across events

When a client resizes its window, `apply_sizes_from_splits` calls `pane.resize()` on each leaf. For mux-connected panes, each `ClientPane::resize()` spawns an **independent async task** that sends a `Pdu::Resize` to the server. During rapid resizing (dragging the window edge), PDUs from different resize events interleave — some panes end up at sizes from event N while others are still at event N-1.

The server calls `rebuild_splits_sizes_from_contained_panes()` after each individual PDU, which reads whatever sizes panes currently report and replaces the tree node data wholesale. This locks in the inconsistency: the horizontal split's `first.rows` and `second.rows` can differ, and vertical sub-split children can overflow or underflow their parent's allocation.

### 2. `rebuild_splits_sizes_from_contained_panes` has no constraint enforcement

The function does a pure bottom-up read: it asks each pane for its current size and propagates up. It does NOT enforce the top-down constraints that the tree structure requires (horizontal split children must have equal rows, vertical split children must have equal cols). Any inconsistency in the panes passes straight into the tree.

### 3. `adjust_y_size` is NOT the problem

Despite the initial doc hypothesis, `adjust_y_size` preserves the sum invariant perfectly. Each loop iteration processes exactly ±1 from each child, and the total delta is always consumed. The same applies to `adjust_x_size`, `adjust_node_at_cursor`, and `cascade_size_from_cursor`. The normal single-client resize path is correct.

### 4. Multiple clients amplify the problem

When two clients with different window sizes connect to the same mux server, each sends its own resize PDUs. The server processes these interleaved — sizes from the Mac client mix with sizes from the Windows client. The coalesce commit (`e3c44c3e9`) addresses resync storms but not the per-pane PDU interleaving.

### Summary

The core bug is architectural: per-pane resize PDUs are fire-and-forget async tasks with no generation tracking, and the server's tree rebuild has no constraint enforcement. The fix needs both a defensive layer (tree reconciliation) and ideally a transport improvement (batched resizes or generation counters).

---

## Tooling

| Tool | Location | Purpose |
|---|---|---|
| `track-pane-sizes.py` | repo root | Live monitor: polls pane sizes, detects column height/width violations |
| `stress-resize.sh` | repo root | Stress test: rapid divider adjustments + violation checking |
| `check-pane-sizes.py` | repo root | One-shot snapshot checker (original, simpler) |
| `check_tree_invariants()` | mux/src/tab.rs (test module) | Unit test helper: validates tree node constraints |
| `make_l_shaped_tab()` | mux/src/tab.rs (test module) | Creates the 3-pane L-shaped layout for tests |
| `make_interleaved_resize_state()` | mux/src/tab.rs (test module) | Simulates PDU interleaving from two rapid resize events |
| `wezterm cli resize-pane` | wezterm/src/cli/resize_pane.rs | Sends raw `Pdu::Resize` per pane (for scripted interleaving) |
| `wezterm cli list --format tree` | wezterm/src/cli/list.rs | Dumps split tree with node-level sizes as JSON |
| `check-tree-invariants.py` | repo root | Tree-level checker: reads `--format tree`, validates split node constraints |
| `reproduce-interleaving.sh` | repo root | End-to-end: creates layout, sends interleaved resize-pane PDUs, checks |

---

## Current State

- **Branch:** `investigate_pane_resizing` (tooling), `fix/6885-minimal+coalesce` (fix)
- **Fix implemented:** `reconcile_tree_sizes()` — top-down constraint enforcement called from both `rebuild_splits_sizes_from_contained_panes()` and `TabInner::resize()`. `debug_assert_tree_invariants()` catches violations in debug builds.
- **Tests:** 11 tests on the fix branch. 6 fail without fix, all 11 pass with fix.
  - Bug-proof: `interleaved_pdus_break_pane_size_invariant`, `interleaved_pdus_break_column_width`
  - Fix-proof: `reconcile_fixes_interleaved_pdu_overflow`, `deep_nested_interleaved_pdus`, `t_shaped_interleaved_pdus`, `grid_interleaved_pdus`, `first_pane_stale_interleaving`
  - Baselines: `nested_split_normal_resize_preserves_invariants`, `all_layouts_resize_preserves_invariants`, `tab_splitting`, `tab_is_send_and_sync`
- **Live violations:** 6 tabs show overflow/underflow on the server running WITHOUT the fix.

---

## Known Bug Patterns

### Pattern 1: Column height mismatch (confirmed, tested, fixed)

**Trigger:** Rapid window resize with nested splits (L-shaped layout).
**Mechanism:** Interleaved per-pane resize PDUs from different events.
**Symptom:** Right column's pane heights sum to more or fewer rows than the left pane.
**Test:** `interleaved_pdus_break_pane_size_invariant`, `reconcile_fixes_interleaved_pdu_overflow`
**Fix:** `reconcile_tree_sizes()` in `rebuild_splits_sizes_from_contained_panes()`

### Pattern 2: Column width inconsistency (confirmed, tested, fixed)

**Trigger:** Interleaved PDUs with different col counts across resize events.
**Symptom:** Panes in the same vertical column have different widths (e.g., tab 0: pane 1=134, pane 8=133).
**Test:** `interleaved_pdus_break_column_width`
**Fix:** `reconcile_tree_sizes()` enforces `first.cols == second.cols` for vertical splits.

### Pattern 3: Multi-level nesting overflow (confirmed, tested, fixed)

**Trigger:** 3+ level nesting with interleaved PDUs (e.g., tab 31: 4 panes, height underflow -14).
**Test:** `deep_nested_interleaved_pdus`
**Fix:** `reconcile_tree_sizes()` recurses through all levels.

### Pattern 4: T-shaped / cross-nesting (confirmed, tested, fixed)

**Trigger:** V-split with H-sub-split on top, interleaving within the H-sub-split.
**Symptom:** H-split children have different row counts (tab 22: bottom pane width 247 vs top 124 — real V-split overflow of -2).
**Test:** `t_shaped_interleaved_pdus`
**Fix:** `reconcile_tree_sizes()` enforces `first.rows == second.rows` for H-splits.

### Pattern 5: Infinite loop on extreme shrink (confirmed, tested, fixed)

**Trigger:** Resize window to near-minimum with nested splits.
**Mechanism:** `adjust_y_size`/`adjust_x_size` shrink loop runs forever when both children reach 1 row/col.
**Test:** `extreme_shrink_and_grow`
**Fix:** Track progress per iteration; return early when neither child can shrink.
**Issue:** #4878

---

## Phases

### Phase 0: Foundation (complete)

- [x] Identify root cause (interleaved per-pane PDUs)
- [x] Write `reconcile_tree_sizes()` fix
- [x] Write unit tests proving bug exists and fix works
- [x] Build `track-pane-sizes.py` monitor
- [x] Build `wezterm cli resize-pane` command
- [x] Build `wezterm cli list --format tree` output
- [x] Build `stress-resize.sh`

### Phase 1: Coverage (complete)

- [x] Test Pattern 2: column width inconsistency (`interleaved_pdus_break_column_width`)
- [x] Test Pattern 3: 4-pane deeply nested layout (`deep_nested_interleaved_pdus`)
- [x] Test Pattern 4: T-shaped layout with H-sub-split interleaving (`t_shaped_interleaved_pdus`)
- [x] All-layout regression guard (`all_layouts_resize_preserves_invariants`)
- [x] Verify: all 4 fix-proof tests fail without fix, all 9 pass with fix
- [ ] Run `track-pane-sizes.py` against a session running WITH the fix
- **Test count:** 9 total (4 fix-proof, 2 bug-proof, 2 baseline, 1 original)

### Phase 2: Hardening (complete)

- [x] Add `reconcile_tree_sizes` to `TabInner::resize()` — defends against drift in window-resize path
- [x] Add `debug_assert_tree_invariants()` after `resize()` and `rebuild_splits` — catches violations in debug builds
- [x] Fix infinite loop in `adjust_y_size`/`adjust_x_size` on extreme shrink (#4878)
- [x] Batched resize PDU — `ResizeTab` (codec type 63, version 46) on `fix/batched-resize` branch
  - `Pane::send_resize_batch()` trait method, `ClientPane` implements it
  - `TabInner::resize()` sends batch after `apply_sizes_from_splits`
  - Server handler applies all pane sizes atomically, rebuilds once
  - Individual `Pdu::Resize` still sent as fallback for older servers
- [x] Cherry-picked `gui: clamp tiny resize geometry to at least 1x1 cells` from fix/resize-hardening
- Generation counter — not needed, batched PDU solves the problem more directly
- Pass criterion: `stress-resize.sh --rounds 1000` produces zero violations

### Phase 3: Upstream (in progress)

- [ ] Clean up commits for PR to wezterm/wezterm (squash RESIZE: commits into fix + tests)
- [x] Write PR description — see PR-DESCRIPTION.md
- [ ] Coordinate with #7590 (focus loop fix, same #6885 umbrella)
- [ ] Build fix branch binary, validate live violations → 0

---

## Related Issues (upstream)

| Issue | Title | Relation |
|---|---|---|
| #6052 | resizing window does not resize panes proportionally | Direct — our fix |
| #5011 | relative sizing of panes do not persist on GUI resize | Direct — our fix |
| #4878 | panes can be resized to zero and negative sizes | Direct — boundary case |
| #5117 | some panes don't resize properly when reattaching to domain | Direct — mux path |
| #6885 | window going crazy when reconnecting to shared mux | Umbrella — includes focus + resize |
| #7540 | SSHMUX confused, rapidly redraws, crashes | Related — resize storms |
| #7590 | PR: fix flickering across multiple mux clients | Complementary — focus loop fix |

---

## Notes

- Keep this file short. Update it when a conclusion changes, not for every run.
- Per-run detail belongs in `experiments/`, not here.
- The fix branch is `fix/6885-minimal+coalesce`. Tooling branch is `investigate_pane_resizing`.
