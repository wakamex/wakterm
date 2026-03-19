# Mux Tasks

Execution loop and investigation protocol: [MUX-LOOP.md](./MUX-LOOP.md).

This file tracks hypotheses, findings, and durable conclusions for mux server bugs. Keep it concise — per-run detail goes in `experiments/`.

## Prior Art

The resize investigation (RESIZE-LOOP.md) established:
- Per-pane `Pdu::Resize` tasks interleave across events → tree inconsistency
- Fixed with `reconcile_tree_sizes()` (defensive) and `ResizeTab` PDU (root cause)
- `adjust_y_size`/`adjust_x_size` infinite loop on extreme shrink (#4878)
- 14 unit tests covering 6 layout patterns

These fixes are merged to `origin/main`. The mux bug cluster shares code paths and patterns.

---

## Issue Inventory

Source: 200 most recently active open issues, filtered for ssh/mux/domain keywords.

### Tier 1: Crashes, deadlocks, OOM (fix first)

| Issue | Title | Hypothesis |
|-------|-------|-----------|
| #7661 | tmux CC domain detach deadlocks entire GUI | Detach triggers cleanup that takes a lock already held by the GUI thread |
| #7540 | SSHMUX rapidly redraws and moves panes → crash | Resize storm creates feedback loop — client resync triggers more resizes |
| #7527 | Unbounded PDU memory allocation → OOM and stack overflow | Server allocates PDU buffer from untrusted size field without bounds check |
| #7444 | Render loop freeze when closing workspaces | Workspace closure races with the render loop's tab iteration |

### Tier 2: State corruption, glitches

| Issue | Title | Hypothesis |
|-------|-------|-----------|
| #6397 | RotatePanes doesn't work via mux server | Rotation modifies local tree but doesn't propagate to server |
| #5142 | Resizing in mux domains has issues | Our resize fix may address this — needs verification |
| #6666 | Resizing pane with neovim on unix domain | Neovim's terminal response interacts badly with resize PDUs |
| #7117 | Unrecognized tmux CC line for %unlinked-window-renamed | Parser doesn't handle this tmux event type |

### Tier 3: Protocol hardening

| Issue | Title | Hypothesis |
|-------|-------|-----------|
| #7656 | tmux CC parser error on empty line during detach | Parser chokes on empty line in protocol stream |
| #7659 | SSH can't handle long passphrases | Input buffer too small or passphrase prompt handling broken |
| #6685 | Clipboard not working between terminals | Clipboard PDU routing between mux clients is incomplete |

---

## Hypotheses to Investigate

### H1: PDU allocation is unbounded (#7527)

**Claim:** The server reads a PDU length from the wire and allocates that many bytes without checking. A malformed or corrupted length field causes OOM.

**Where to look:**
- `codec/src/lib.rs` — PDU deserialization, `decode_raw` or equivalent
- `wakterm-mux-server-impl/src/sessionhandler.rs` — how incoming bytes become PDUs
- Search for `Vec::with_capacity`, `vec![0u8; len]`, or similar allocations gated on wire data

**What would confirm:** Finding an allocation path where `len` comes from the wire with no upper bound.

**What would refute:** Finding a max PDU size check before allocation.

**Fix pattern:** Add `const MAX_PDU_SIZE: usize = 64 * 1024 * 1024;` and reject PDUs larger than that.

### H2: Detach deadlocks from lock ordering (#7661)

**Claim:** When the last tmux CC window is closed via Ctrl+D, the detach path acquires locks in an order that conflicts with the GUI's render path.

**Where to look:**
- `wakterm-client/src/domain.rs` — detach/cleanup code
- `mux/src/lib.rs` — window removal, tab removal
- `wakterm-gui/src/termwindow/mod.rs` — what locks the GUI holds during render
- Search for nested `.lock()` calls on different mutexes

**What would confirm:** Finding two code paths that acquire the same two locks in opposite order.

**What would refute:** All lock acquisitions are in a consistent order, and the deadlock is elsewhere.

**Fix pattern:** Establish a lock ordering protocol, or use `try_lock` with fallback.

### H3: SSHMUX redraw storm is resize feedback (#7540)

**Claim:** Client resize → server resizes panes → server sends resize notification → client resyncs → client resizes again → infinite loop.

**Where to look:**
- `wakterm-client/src/client.rs` — `process_unilateral` handling of `TabResized`
- `wakterm-client/src/domain.rs` — `resync` and what triggers it
- The coalesce fix (`resync_coalesced`) — is it sufficient?

**What would confirm:** Finding a path where `TabResized` notification triggers a resync that sends new resize PDUs.

**What would refute:** The coalesce fix already breaks this loop.

**Fix pattern:** Don't resync in response to resize notifications that the client itself caused. Add a "I caused this" flag or generation counter.

### H4: RotatePanes is local-only (#6397)

**Claim:** `RotatePanes` modifies the local tab's pane tree but doesn't send any PDU to the mux server, so the server's tree diverges.

**Where to look:**
- `mux/src/tab.rs` — `rotate_clockwise`, `rotate_counter_clockwise`
- Search for PDU sends in the rotation path
- Compare with `split_and_insert` which does propagate

**What would confirm:** Rotation functions modify the tree without sending a PDU.

**What would refute:** Finding a rotation PDU or a resync triggered after rotation.

**Fix pattern:** Either add a rotation PDU, or trigger a full resync after rotation.

---

## Phases

### Phase 0: Triage (complete)

- [x] Inventory open issues in the ssh/mux/domain cluster
- [x] Write hypotheses for top issues
- [x] All 4 hypotheses investigated (H1-H4)

### Phase 1: Quick wins (complete)

- [x] Fix unbounded PDU allocation (#7527) — MAX_PDU_SIZE check
- [x] Fix tmux CC parser error on empty line (#7656) — early return for empty buffer
- [x] #7117 already handled in current grammar
- [x] Fix --attach flag not passed through (#7582)
- [ ] Verify #5142 and #6666 are fixed by our resize work (needs build + live test)

### Phase 2: Deeper bugs (complete)

- [x] Fix detach deadlock (#7661) — lock ordering fix in domain_was_detached
- [x] Fix RotatePanes via mux (#6397) — added RotatePanes PDU
- [x] Fix top-level split tab size (#7654, #2579, #4984)
- [x] Redraw storm (#7540) — likely fixed by resize reconciliation + batched PDU

### Phase 3: Protocol hardening (complete)

- [x] Audit all PDU handlers — no missing error handling found
- [x] Add max PDU size limit (64 MiB) — MAX_PDU_SIZE in decode_raw
- [x] swap_active_with_index missing PDU — low priority, deferred
- [ ] Add timeout on PDU reads — not yet investigated
- [ ] Build binary and validate fixes live

---

## Current State

- **All work on main** — 14 commits above upstream
- **10 bugs fixed:** #4878, #5011, #5117, #6052, #6397, #7527, #7582, #7654, #7656, #7661
- **11 response drafts** in responses/
- **26 tests** passing (17 mux + 9 codec)
- **Running binary:** `e3c44c3e` (Mar 3 build) — does NOT include our fixes yet
- **Next action:** build binary, validate live, or find more bugs

---

## Notes

- Keep this file short. Update when a conclusion changes, not for every investigation step.
- When an issue is confirmed fixed, move it to a "Fixed" section with the commit hash.
- Hypotheses that are refuted should be marked as such with a brief explanation.
