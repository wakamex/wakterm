# Changelog -- wakamex/wakterm fork

All changes relative to upstream `wakterm/wakterm` main at `05343b387`.

## Features

### Agent Harnesses

- **Add pane-owned agent identity and persistence** ([dcd1d10](https://github.com/wakamex/wakterm/commit/dcd1d1068))
  Agents (Claude, Codex, Gemini, OpenCode) are first-class mux panes with identity, state tracking, and persistence across server restarts.

- **Add agent lifecycle commands** ([9def1d0](https://github.com/wakamex/wakterm/commit/9def1d07b))
  `wakterm cli agent start|stop|list` for managing agent harness panes.

- **Add agent runtime, send, and client-side badges** ([58ddee7](https://github.com/wakamex/wakterm/commit/58ddee750))
  Send prompts and interrupts to running agents. Tab badges show agent status (waiting/working/your turn).

- **Add native harness watch and observer-backed PTY runtime** ([10e219e](https://github.com/wakamex/wakterm/commit/10e219e19))
  `wakterm cli agent watch` and `wakterm cli agent list -f` for live progress across running harnesses.

- **Fix raw input path for gemini agent sends** ([fbe9ccd](https://github.com/wakamex/wakterm/commit/fbe9ccd0d))

### Tab Management

- **Add prompt rename tab action** ([86e661f](https://github.com/wakamex/wakterm/commit/86e661f5c))
  New `PromptRenameTab` action lets users rename tabs interactively.

- **Add default shortcut for prompt rename tab** ([01d3ab0](https://github.com/wakamex/wakterm/commit/01d3ab07c))
  Bound to Ctrl+Shift+< by default, later moved to Shift+Comma ([f249f9f](https://github.com/wakamex/wakterm/commit/f249f9f42)).

- **Add move-tab bracket shortcuts** ([de89169](https://github.com/wakamex/wakterm/commit/de89169fe))
  Ctrl+Shift+[ and Ctrl+Shift+] to reorder tabs.

- **Preserve user-set tab titles from escape sequences** ([63c30dc](https://github.com/wakamex/wakterm/commit/63c30dcb0))
  Titles set via `PromptRenameTab` or the Lua API are no longer overwritten by terminal escape sequences.

- **Add safe tab effective title for Lua** ([1624be0](https://github.com/wakamex/wakterm/commit/1624be0e0))
  Exposes a Lua-accessible effective title that respects user overrides.

### GUI

- **Remember window position and size on macOS** ([05ed9a7](https://github.com/wakamex/wakterm/commit/05ed9a7c3))
  Uses native `NSWindow` autosave so window geometry persists across restarts.

- **Make tab reordering atomic** ([946d01b](https://github.com/wakamex/wakterm/commit/946d01b93))
  Tab drag-reorder is now a single atomic operation, avoiding intermediate invalid states.

- **Clip pane glyphs to pane bounds** ([d16d9b4](https://github.com/wakamex/wakterm/commit/d16d9b49f))
  Glyphs that extend past a pane's edges are now clipped instead of bleeding into adjacent panes.

- **Invalidate line quads when pane width changes** ([3c82389](https://github.com/wakamex/wakterm/commit/3c8238958))
  Fixes stale rendered content after pane resizes.

- **Repaint window on tab resize** ([7f4a541](https://github.com/wakamex/wakterm/commit/7f4a54187))

- **Improve `wakterm cli list` table layout** ([a5a9966](https://github.com/wakamex/wakterm/commit/a5a996685))

### Docs Site

- **Replace colorscheme index pages with interactive browser** ([839988a](https://github.com/wakamex/wakterm/commit/839988a5a))
  Removed hundreds of static colorscheme pages and replaced them with a searchable, filterable browser with live previews.

- **Modernize docs site** ([839988a](https://github.com/wakamex/wakterm/commit/839988a5a))
  Dropped legacy asciinema player, mdbook assets, and custom CSS. Trimmed global page load cost.

## Bug Fixes

### Resize / Split Tree

- **Sync divider drags via atomic ResizeTab batches**
  `resize_split_by()` now sends the same tab-level `ResizeTab` batch used by full window resizes, so dragging a split divider updates the mux server coherently instead of leaving client-only pane widths behind.

- **Fix spawn sizing across entry points**
  `wakterm cli spawn`, delegation into an already-running GUI instance, and existing-window mux spawns now use the live tab size instead of falling back to tiny server defaults.

- **Fix client ResizeTab pane id mapping**
  Batched resize messages now translate client-local pane ids back to remote mux pane ids before sending them to the server, fixing fresh-session tabs that stayed at `80x24` despite correct pane sizes.

- **Fix nested split pane sizes diverging after window resize** ([de54b07](https://github.com/wakamex/wakterm/commit/de54b07d2))
  Per-pane `Pdu::Resize` messages interleave during rapid resizing, causing the mux server's tree to diverge. Added `reconcile_tree_sizes()` -- a top-down constraint enforcement pass after every tree mutation. 14 unit tests covering 6 layout patterns.
  Fixes #6052, #5011, #5117

- **Fix infinite loop on extreme window shrink** ([80447df](https://github.com/wakamex/wakterm/commit/80447dfde))
  `adjust_y_size`/`adjust_x_size` loop forever when both split children reach 1 row/col. Added early return when no progress is made.
  Fixes #4878

- **Batch per-pane resize PDUs into atomic ResizeTab message** ([f39b4cc](https://github.com/wakamex/wakterm/commit/f39b4cc6a))
  Eliminates the root cause of resize interleaving. New `ResizeTab` PDU (codec type 63) sends all pane sizes atomically. Individual `Pdu::Resize` still sent as fallback for older servers.

- **Stop sending individual Pdu::Resize, rely on batched ResizeTab** ([5adbc17](https://github.com/wakamex/wakterm/commit/5adbc17be))

- **Fix split-pane race by sending tab size with SplitPane PDU** ([5d94a78](https://github.com/wakamex/wakterm/commit/5d94a7885))

- **Force tab resize after split_pane to sync PTY sizes with tree** ([fffb3f8](https://github.com/wakamex/wakterm/commit/fffb3f825))

- **Clamp tiny resize geometry to at least 1x1 cells** ([8968ff4](https://github.com/wakamex/wakterm/commit/8968ff422))
  Prevents zero-dimension resize requests from reaching the mux layer.

- **Restore tab size after top-level split** ([9b04ef8](https://github.com/wakamex/wakterm/commit/9b04ef81c))
  `split_and_insert` with `top_level=true` didn't restore `self.size` after pre-resizing, causing subsequent splits to fail with "No space for split!".
  Fixes #7654, #2579, #4984

- **Focus new pane after split** ([1fa85af](https://github.com/wakamex/wakterm/commit/1fa85afef))

### Multi-Client Stability

- **Break resize feedback loop** ([76a1695](https://github.com/wakamex/wakterm/commit/76a169534))
  Client no longer resyncs on `TabResized`, breaking the loop where resize -> resync -> resize spiralled.

- **Suppress self-echo TabResized, forward from other clients** ([daa899b](https://github.com/wakamex/wakterm/commit/daa899b9b))
  Server no longer echoes `TabResized` back to the client that triggered it.

- **Restore TabResized resync after self-echo filtering** ([ec0250f](https://github.com/wakamex/wakterm/commit/ec0250f05))
  With self-echo gone, resync on `TabResized` from other clients is safe again.

- **Debounce resync storms instead of dropping** ([039980c](https://github.com/wakamex/wakterm/commit/039980c0a))

- **Avoid pane focus feedback loops across clients** ([cae82e4](https://github.com/wakamex/wakterm/commit/cae82e478))

- **Make active tab state client-local** ([ff0b2a5](https://github.com/wakamex/wakterm/commit/ff0b2a5f3))
  Each connected client tracks its own active tab instead of fighting over a shared global.

- **Avoid reentrant window lock when moving tabs** ([0a05f94](https://github.com/wakamex/wakterm/commit/0a05f947b))

- **Fix mux client registration handshake ordering** ([d3993c2](https://github.com/wakamex/wakterm/commit/d3993c284))

### Session Persistence

- **Rewrite session restore with recursive tree walk** ([26cc34b](https://github.com/wakamex/wakterm/commit/26cc34bed))
  Replays the exact split tree instead of reconstructing from flat pane rectangles.

- **Use percentage splits for session restore** ([d81bd83](https://github.com/wakamex/wakterm/commit/d81bd830d))
  Proportional sizing instead of absolute cell counts, so restores adapt to different window sizes.

- **Heal degenerate splits before saving, clamp to 10-90%** ([27dc63d](https://github.com/wakamex/wakterm/commit/27dc63d38))

- **Use generous initial size for session restore** ([42299768](https://github.com/wakamex/wakterm/commit/42299768d))

- **Reconcile tree after session restore to fix column height mismatches** ([6cb68dc](https://github.com/wakamex/wakterm/commit/6cb68dc9c))

- **Preserve active tab selection in manual and automatic restore**
  `ListPanesResponse` now carries the active tab per window, `wakterm cli save-layout` records it, manual restore focuses the saved tab after rebuilding the window, client attach/resync tracks it, and built-in mux session restore reapplies the saved active tab.

- **Add Rust `wakterm cli save-layout` / `restore-layout` and remove `wez-tabs`**
  Manual layout snapshots now use the real mux pane tree instead of reconstructing split order from flat pane rectangles. Restore replays exact split cells, preserves tab/window grouping, titles, workspaces, active tab selection, per-tab active pane selection, and zoom state.

### Mux Protocol / Server

- **Reject oversized PDUs before allocation** ([e1e8510](https://github.com/wakamex/wakterm/commit/e1e8510b3))
  Both `decode_raw` and `decode_raw_async` allocated buffers from untrusted wire data without bounds. Added `MAX_PDU_SIZE` (64 MiB) check.
  Fixes #7527

- **Fix deadlock in domain_was_detached** ([1a9b10d](https://github.com/wakamex/wakterm/commit/1a9b10dbb))
  Held `windows.write()` while calling into `tab.kill_panes_in_domain()`, creating a lock-ordering deadlock with the GUI render path. Downgraded to `windows.read()` and released before operating on tabs.
  Fixes #7661

- **Add RotatePanes PDU** ([3ebe927](https://github.com/wakamex/wakterm/commit/3ebe927ea))
  `rotate_clockwise`/`rotate_counter_clockwise` were local-only -- the server's tree diverged after rotation. Added `RotatePanes` PDU (codec type 64) to keep server in sync.
  Fixes #6397

- **Pass --attach flag through try_spawn** ([f283ee0](https://github.com/wakamex/wakterm/commit/f283ee0ae))
  `wakterm start --attach --domain X` delegated to an existing instance but always spawned a new tab, ignoring `--attach`. Now checks for existing panes and skips spawning.
  Fixes #7582

- **Clarify stale mux server version mismatch errors** ([55f3de1](https://github.com/wakamex/wakterm/commit/55f3de1d8))

- **Log client version on connect** ([95d16ce](https://github.com/wakamex/wakterm/commit/95d16ce8b))

### Codec

- **Accept legacy tab title PDUs without badge** ([9eceae0](https://github.com/wakamex/wakterm/commit/9eceae0a8))
  Backward compatibility for older clients that don't send the agent badge field.

- **Restore codec version 46** ([1a16da1](https://github.com/wakamex/wakterm/commit/1a16da163))
  Both client and server are built from the fork, so the intermediate version bump was unnecessary.

### Parser / Misc

- **Fix tmux CC parser error on empty line during detach** ([701b950](https://github.com/wakamex/wakterm/commit/701b9508c))
  Empty lines during tmux `-CC` detach caused parser errors in the debug overlay.
  Fixes #7656

- **Add chrono clock feature** ([6e5b38a](https://github.com/wakamex/wakterm/commit/6e5b38a9f))
  The workspace chrono dependency was missing the `clock` feature, preventing `Utc::now()` from compiling.

## Observability

- **Add mux observability for layout issues**
  The mux server logs hard errors for `ResizeTab` pane-count mismatches, unknown pane ids, and split-tree invariant failures.

- **Add `check-pane-layout.py` live layout validator**
  Validates `wakterm cli list --format json` output against a legal split tree so offscreen panes, overlaps, gaps, and degenerate rectangles are easy to catch from a live session.

- **Track .git/HEAD and refs/heads for version string freshness** ([dcd417b](https://github.com/wakamex/wakterm/commit/dcd417b0f))

## Compatibility

The mux protocol has diverged from upstream. wakterm clients and servers must be the same build; connecting to an upstream wezterm mux server is not supported.

## Test Coverage

26 tests added (17 mux, 9 codec) covering:
- 6 layout patterns (L-shape, T-shape, grid, deep-nested, first-pane-stale, column-width)
- Interleaved PDU scenarios from rapid resize events
- Pane removal, split+resize, extreme shrink/grow cycles
- Oversized PDU rejection
- tmux CC empty line handling
- Top-level split tab size preservation
- Tab rename title fallbacks
- Headless agent watch smoke test
- SetClientId handshake regression
