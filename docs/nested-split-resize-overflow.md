# Bug: Nested split pane sizes diverge from parent after window resize

## Summary

When a tab contains a nested split layout (e.g. one full-height left pane
with two stacked right panes), resizing the window causes the nested
sub-split's children to overflow or underflow the parent's row count.
Manually adjusting split dividers before the window resize makes it worse.
This is a standalone bug — it reproduces with a single client.

## Related issues

- #6052 — window resize doesn't scale panes proportionally
- #5011 — proportional pane ratios lost after GUI resize
- #4878 — nested splits go to zero/negative sizes
- #5117 — panes overshoot window bounds after domain reattach

## Layout to reproduce

A 3-pane "L-shaped" layout within one tab:

```
+----------+----------+
|          |  pane 1  |
|  pane 0  +----------+
|          |  pane 2  |
+----------+----------+
```

- pane 0: full-height left (horizontal split, first child)
- pane 1: top-right (vertical sub-split, first child)
- pane 2: bottom-right (vertical sub-split, second child)

## Steps to reproduce

1. Open wezterm, create a horizontal split (Ctrl+Shift+Alt+%)
2. In the right pane, create a vertical split (Ctrl+Shift+Alt+")
3. Manually drag the vertical divider on the right side (between pane 1
   and pane 2) to a non-50/50 ratio
4. Resize the window (drag the window edge to make it larger or smaller)
5. Run: `wezterm cli list --format json | python3 check-pane-sizes.py`

## Observed behavior

The right column's total rows (pane 1 rows + 1 divider + pane 2 rows)
does not equal the left pane's row count. Depending on direction of resize:

- **Overflow**: right column total exceeds window height — bottom-right
  pane extends past the visible window boundary
- **Underflow**: right column total is less than window height — unused
  space at the bottom of the right column

## Evidence collected

All data from `wezterm cli list --format json` piped through
`check-pane-sizes.py` (committed at repo root).

### Single client, after window resize (single_client_after.txt)

Window height = 79 rows (confirmed by all 2-pane tabs showing 79x79).

```
scrape (3 panes):
  pane 0: 147x79  (left, full height — correct)
  pane 1: 156x32  (top-right)
  pane 2: 156x56  (bottom-right)
  right column total: 32 + 1 + 56 = 89 rows  → OVERFLOW by 10

gitrep (3 panes):
  pane 5: 153x79  (left, full height — correct)
  pane 6: 150x31  (top-right)
  pane 34: 150x31 (bottom-right)
  right column total: 31 + 1 + 31 = 63 rows  → UNDERFLOW by 16

collab (3 panes):
  pane 22: 154x79 (left, full height — correct)
  pane 23: 149x30 (top-right)
  pane 24: 149x26 (bottom-right)
  right column total: 30 + 1 + 26 = 57 rows  → UNDERFLOW by 22
```

All 2-pane (simple horizontal split) tabs show correct matching heights
(79x79). The bug only manifests in nested splits.

### Multi-client vs single-client

The same 3 mismatched tabs appear regardless of whether one or two clients
are connected. Multi-client exacerbates the problem (introduces additional
1-2 row mismatches on simple splits due to competing resize requests) but
the nested split overflow/underflow is purely a standalone bug.

### Timeline of captures

| File                      | Clients     | Notes                      | Mismatches |
|---------------------------|-------------|----------------------------|------------|
| dual_client_before.txt    | 2 (Mac+Win) | Before Windows resize      | 6          |
| dual_client_after.txt     | 2 (Mac+Win) | After Windows resize       | 3          |
| single_client_before.txt  | 1 (Win)     | Mac disconnected           | 3          |
| single_client_after.txt   | 1 (Win)     | After Windows resize       | 3          |

The 3 extra mismatches in dual_client_before were 1-row height differences
on simple splits — these cleared after a single-client resize. The 3
nested-split mismatches persisted across all conditions.

## Root cause analysis

The resize path is `TabInner::resize()` (mux/src/tab.rs:1145):

1. `adjust_x_size()` distributes the column delta through the tree
2. `adjust_y_size()` distributes the row delta through the tree
3. `apply_sizes_from_splits()` walks the tree and calls `pane.resize()`
   on each leaf with the size stored in its split node

The problem is in how `adjust_y_size` handles a horizontal split node
that contains a vertical sub-split. When the window height changes:

- The horizontal split sets `node.first.rows = new_rows` and
  `node.second.rows = new_rows` (both sides get full height)
- But when `adjust_y_size` recurses into the right side's vertical
  sub-split, it distributes the *delta* (new_rows - old_rows) between
  the two children

The delta distribution doesn't account for the fact that the children's
rows may no longer sum to the parent's rows (due to earlier manual divider
adjustments or accumulated rounding). It adjusts relatively rather than
absolutely, so any existing discrepancy is preserved or amplified.

### Key code paths to investigate

- `adjust_y_size()` — mux/src/tab.rs:414
- `adjust_x_size()` — mux/src/tab.rs:344
- `apply_sizes_from_splits()` — mux/src/tab.rs:487
- `TabInner::apply_pane_size()` — mux/src/tab.rs:1191
- `TabInner::resize()` — mux/src/tab.rs:1145
- `TabInner::rebuild_splits_sizes_from_contained_panes()` — mux/src/tab.rs:1224

### Possible fix directions

1. **Post-resize validation**: after `adjust_y_size` + `apply_sizes_from_splits`,
   walk the tree and verify that each split node's children sum to the
   parent's size. If not, redistribute the remainder.

2. **Proportional resize**: instead of distributing a delta, compute the
   ratio of new_size/old_size and apply it to each child proportionally,
   then fix up rounding by giving the remainder to the last child.

3. **Absolute constraint**: after setting `node.second.rows` in
   `apply_pane_size`, ensure the vertical sub-split's children are
   recalculated to sum to exactly `node.second.rows`.

## Diagnostic tool

`check-pane-sizes.py` at repo root. Usage:

```
wezterm cli list --format json | python3 check-pane-sizes.py
```

Reports pane positions, sizes, and flags mismatches where panes sharing
a starting column or row have different dimensions.

Note: the script currently flags expected differences in L-shaped layouts
(left pane height != top-right pane height) as mismatches. The real signal
is when the right column's total rows != the left pane's rows. A future
version should reconstruct the split tree to check parent-child consistency
instead.
