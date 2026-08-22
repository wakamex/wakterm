# Changelog - wakamex/wakterm fork

All changes relative to upstream wakterm/wakterm main at 05343b387.

## Changes since 2026-03-20

### Agent Harnesses and Agent API

- Top-level `wakterm agent` CLI command for starting, adopting, observing, and controlling agent harnesses.
- Cached automatic adoption: detected harness processes (Claude, Codex, Gemini, OpenCode) with confirmed backing sessions are automatically promoted to persistent agent metadata.
- Exact Codex session restoration: restores idle Codex sessions across multiplexer restart and system reboot in declared working directory, resuming exact provider threads and settling interrupted turns.
- Mux-supervised Codex app-server TUI (`wakterm agent launch codex`): multiplexer manages shared app-server over private Unix socket while the pane runs native Codex TUI.
- Versioned Agent API v1:
  - Capability negotiation (`wakterm agent capabilities`)
  - Narrow agent catalog (`wakterm agent catalog`) with stable agent ID, ephemeral pane ID, and sequence cursor
  - Authoritative prompt admission (`wakterm agent admit`) with process incarnation checks and idempotency keys
  - Durable normalized event stream (`wakterm agent events`) with strictly increasing sequence numbers
  - Asynchronous return requests (`wakterm agent send --return-final` and `wakterm agent request get|watch|cancel`)
- Experimental shadow output page (`wakterm agent output`) for normalized assistant message comparisons.
- Native harness tab icons for Claude, Codex, Gemini, and OpenCode in the fancy tab bar.
- Agent attention pulse (`agent_tab_attention_pulse`): smooth icon fade when an agent completes a turn and awaits review, with shared review acknowledgement across clients.
- `ActivateNextTabNeedingAttention`: shortcut action to jump directly to the next tab with an unreviewed completed turn.
- Automatic tab naming: unnamed tabs derive titles from the active agent leaf directory, with numeric suffixes to resolve collisions.

### Parked Tabs and Tab Navigator

- Parked tabs (`ParkCurrentTab` / `Ctrl-Shift-S` on Linux/Windows, `Cmd-Shift-S` on macOS): hide inactive tabs from the tab strip while keeping processes, PTYs, and sessions alive in the multiplexer.
- Dedicated tab navigator (`ShowTabNavigator` / `Ctrl-Shift-E` on Linux/Windows, `Cmd-E` on macOS):
  - Visible, Parked, and All views
  - Instant fuzzy search across title, CWD, branch, harness, and agent name
  - Responsive metadata columns: status, last response age, CWD, branch, pane count, and approximate process RSS
  - In-place parking and unparking (`Ctrl-Shift-S`)
  - Permanent tab close with confirmation (`Ctrl-X`)
  - Sort by tab order or response time (`Ctrl-R`)
  - Dense single-line or comfortable multi-line pane layout (`Ctrl-O`)

### Appearance and Theming

- Built-in per-tab generated colors (`tab_bar_color_mode = "Off"|"Hash"|"Assign"`, `tab_bar_color_palette = "Dark"|"Light"|"Mixed"`, `tab_bar_color_intensity`).
- Color similarity search in the interactive color scheme browser.
- Configurable agent tab badge mode (`agent_tab_badge_mode = "identity"|"attention"|"turn"|"off"`).
- Interactive tab renaming (`PromptRenameTab` / `Ctrl-Shift-<` on Linux/Windows, `Cmd-<` on macOS).

### Layout and Navigation

- `PaneSelect(mode="SwapWithActive")` bound to `Ctrl-Shift-M`.
- `RotatePanes(Clockwise)` bound to `Ctrl-Shift-O` (Linux/Windows) and `Cmd-O` (macOS).
- macOS pane navigation shortcuts (`Cmd-Alt-Left/Right/Up/Down`).
- `wakterm cli save-layout` and `wakterm cli restore-layout` for manual layout snapshots.
- Reordered `wakterm cli list` table columns for readability.

### Multiplexer Stability and Persistence

- Systemd service integration: `install-user-service.sh` and `install-system-service.sh` for surviving logout and reboot.
- Explicit resize origin tracking: suppresses client self-echoes without timing heuristics, eliminating resize flicker and feedback loops.
- Synchronized tab ordering: authoritative server tab order with drift reconciliation on client reconnect.
- Durable atomic layout updates for tab reordering and split divider dragging.

### Fixes

- Bound SynchronizedOutput buffer to 4MB to prevent memory leaks from unresponsive TUI applications.
- Bound adversarial terminal sequence parsing and Kitty image memory allocation.
- Prevent stack overflow from process tree cycles in procinfo.
- Fix cross-pane drag selection not copying to clipboard.
- Avoid reentrant window locks during title updates and IME borrow operations.
- Preserve hosted pane paths across platforms.
- Encode Escape correctly with Kitty keyboard disambiguation.
- Fix macOS attach task handoff and spawn queue dispatch.
- Honor disabled titlebar decorations on Wayland.
- Align Wayland buffers to buffer scale and share clipboard offers across windows.
- Terminate tmux send-keys commands with LF and support Unicode input in control mode.

## Historical Fork Features (prior to 2026-03-20)

### Agent Harnesses

- Add pane-owned agent identity and persistence ([dcd1d10](https://github.com/wakamex/wakterm/commit/dcd1d1068)). Agents (Claude, Codex, Gemini, OpenCode) are first-class mux panes with identity, state tracking, and persistence across server restarts.
- Add agent lifecycle commands ([9def1d0](https://github.com/wakamex/wakterm/commit/9def1d07b)). `wakterm cli agent start|stop|list` for managing agent harness panes.
- Add agent runtime, send, and client-side badges ([58ddee7](https://github.com/wakamex/wakterm/commit/58ddee750)). Send prompts and interrupts to running agents. Tab badges show agent status.
- Add native harness watch and observer-backed PTY runtime ([10e219e](https://github.com/wakamex/wakterm/commit/10e219e19)). `wakterm cli agent watch` and `wakterm cli agent list -f` for live progress across running harnesses.
- Fix raw input path for gemini agent sends ([fbe9ccd](https://github.com/wakamex/wakterm/commit/fbe9ccd0d)).

### Tab Management

- Add prompt rename tab action ([86e661f](https://github.com/wakamex/wakterm/commit/86e661f5c)). `PromptRenameTab` action lets users rename tabs interactively.
- Add default shortcut for prompt rename tab ([01d3ab0](https://github.com/wakamex/wakterm/commit/01d3ab07c), [f249f9f](https://github.com/wakamex/wakterm/commit/f249f9f42)). Bound to `Ctrl-Shift-<`.
- Add move-tab bracket shortcuts ([de89169](https://github.com/wakamex/wakterm/commit/de89169fe)). `Ctrl-Shift-[` and `Ctrl-Shift-]` to reorder tabs.
- Preserve user-set tab titles from escape sequences ([63c30dc](https://github.com/wakamex/wakterm/commit/63c30dcb0)). Titles set via `PromptRenameTab` or the Lua API are no longer overwritten by terminal escape sequences.
- Add safe tab effective title for Lua ([1624be0](https://github.com/wakamex/wakterm/commit/1624be0e0)). Exposes a Lua-accessible effective title that respects user overrides.

### GUI

- Remember window position and size on macOS ([05ed9a7](https://github.com/wakamex/wakterm/commit/05ed9a7c3)). Uses native `NSWindow` autosave so window geometry persists across restarts.
- Make tab reordering atomic ([946d01b](https://github.com/wakamex/wakterm/commit/946d01b93)). Tab drag-reorder is now a single atomic operation, avoiding intermediate invalid states.
- Clip pane glyphs to pane bounds ([d16d9b4](https://github.com/wakamex/wakterm/commit/d16d9b49f)). Glyphs that extend past pane edges are now clipped instead of bleeding into adjacent panes.
- Invalidate line quads when pane width changes ([3c82389](https://github.com/wakamex/wakterm/commit/3c8238958)). Fixes stale rendered content after pane resizes.
- Repaint window on tab resize ([7f4a541](https://github.com/wakamex/wakterm/commit/7f4a54187)).
- Improve `wakterm cli list` table layout ([a5a9966](https://github.com/wakamex/wakterm/commit/a5a996685)).

### Docs Site

- Replace colorscheme index pages with interactive browser ([839988a](https://github.com/wakamex/wakterm/commit/839988a5a)).
- Add fork build target comparison doc ([817ae9d](https://github.com/wakamex/wakterm/commit/817ae9d06)).
- Fix markdown code block indentation in CLI help docs ([f54c30c](https://github.com/wakamex/wakterm/commit/f54c30cf9)).
- Standardize all user-facing names to wakterm ([4232338](https://github.com/wakamex/wakterm/commit/4232338be)).

### Split & Resize Reliability

- Use real split size instead of active pane size in split_pane ([be53186](https://github.com/wakamex/wakterm/commit/be5318625)).
- Clamp split dimensions to remaining available space ([44b360a](https://github.com/wakamex/wakterm/commit/44b360a8b)).
- Recompute effective split bounds on pane tree changes ([70c9103](https://github.com/wakamex/wakterm/commit/70c9103bc)).
- Synchronize split tree and PTY dimensions after resize ([cb3a906](https://github.com/wakamex/wakterm/commit/cb3a90610)).
- Stop sending individual Pdu::Resize, rely on batched ResizeTab ([5adbc17](https://github.com/wakamex/wakterm/commit/5adbc17be)).
- Fix split-pane race by sending tab size with SplitPane PDU ([5d94a78](https://github.com/wakamex/wakterm/commit/5d94a7885)).
- Force tab resize after split_pane to sync PTY sizes with tree ([fffb3f8](https://github.com/wakamex/wakterm/commit/fffb3f825)).
- Clamp tiny resize geometry to at least 1x1 cells ([8968ff4](https://github.com/wakamex/wakterm/commit/8968ff422)). Prevents zero-dimension resize requests from reaching the mux layer.
- Restore tab size after top-level split ([9b04ef8](https://github.com/wakamex/wakterm/commit/9b04ef81c)). `split_and_insert` with `top_level=true` restored `self.size` after pre-resizing. Fixes #7654, #2579, #4984.
- Focus new pane after split ([1fa85af](https://github.com/wakamex/wakterm/commit/1fa85afef)).

### Multi-Client Stability

- Break resize feedback loop ([76a1695](https://github.com/wakamex/wakterm/commit/76a169534)). Client no longer resyncs on `TabResized`, breaking the loop where resize and resync spiraled.
- Suppress self-echo TabResized, forward from other clients ([daa899b](https://github.com/wakamex/wakterm/commit/daa899b9b)). Server no longer echoes `TabResized` back to the client that triggered it.
- Restore TabResized resync after self-echo filtering ([ec0250f](https://github.com/wakamex/wakterm/commit/ec0250f05)).
- Debounce resync storms instead of dropping ([039980c](https://github.com/wakamex/wakterm/commit/039980c0a)).
- Avoid pane focus feedback loops across clients ([cae82e4](https://github.com/wakamex/wakterm/commit/cae82e478)).
- Make active tab state client-local ([ff0b2a5](https://github.com/wakamex/wakterm/commit/ff0b2a5f3)). Each connected client tracks its own active tab instead of fighting over a shared global.
- Avoid reentrant window lock when moving tabs ([0a05f94](https://github.com/wakamex/wakterm/commit/0a05f947b)).
- Fix mux client registration handshake ordering ([d3993c2](https://github.com/wakamex/wakterm/commit/d3993c284)).

### Session Persistence

- Rewrite session restore with recursive tree walk ([26cc34b](https://github.com/wakamex/wakterm/commit/26cc34bed)). Replays the exact split tree instead of reconstructing from flat pane rectangles.
- Use percentage splits for session restore ([d81bd83](https://github.com/wakamex/wakterm/commit/d81bd830d)). Proportional sizing instead of absolute cell counts, so restores adapt to different window sizes.
- Heal degenerate splits before saving, clamp to 10-90% ([27dc63d](https://github.com/wakamex/wakterm/commit/27dc63d38)).
- Use generous initial size for session restore ([42299768](https://github.com/wakamex/wakterm/commit/42299768d)).
- Reconcile tree after session restore to fix column height mismatches ([6cb68dc](https://github.com/wakamex/wakterm/commit/6cb68dc9c)).
- Preserve active tab selection in manual and automatic restore. `ListPanesResponse` carries the active tab per window, `wakterm cli save-layout` records it, manual restore focuses the saved tab after rebuilding the window, client attach/resync tracks it, and built-in mux session restore reapplies the saved active tab.
- Add Rust `wakterm cli save-layout` / `restore-layout` and remove `wez-tabs`. Manual layout snapshots use the real mux pane tree. Restore replays exact split cells, preserves tab/window grouping, titles, workspaces, active tab selection, per-tab active pane selection, and zoom state.

### Mux Protocol and Server

- Fix OOM from unbounded SynchronizedOutput buffer. When a TUI app enables SynchronizedOutput (`CSI?2026h`) and crashes or gets stuck without disabling it, the mux server accumulated parsed actions without bound. Over days, this grew to 25GB+ and triggered the OOM killer. Added a 4MB safety valve that force-flushes the buffer during hold. Also added 60-second memory reporting (RSS + per-pane action buffer sizes) at INFO log level.
- Reject oversized PDUs before allocation ([e1e8510](https://github.com/wakamex/wakterm/commit/e1e8510b3)). Added `MAX_PDU_SIZE` (64 MiB) check. Fixes #7527.
- Fix deadlock in domain_was_detached ([1a9b10d](https://github.com/wakamex/wakterm/commit/1a9b10dbb)). Downgraded to `windows.read()` and released before operating on tabs. Fixes #7661.
- Add RotatePanes PDU ([3ebe927](https://github.com/wakamex/wakterm/commit/3ebe927ea)). Added `RotatePanes` PDU (codec type 64) to keep server in sync. Fixes #6397.
- Pass --attach flag through try_spawn ([f283ee0](https://github.com/wakamex/wakterm/commit/f283ee0ae)). Checks for existing panes and skips spawning. Fixes #7582.
- Clarify stale mux server version mismatch errors ([55f3de1](https://github.com/wakamex/wakterm/commit/55f3de1d8)).
- Log client version on connect ([95d16ce](https://github.com/wakamex/wakterm/commit/95d16ce8b)).

### Codec

- Accept legacy tab title PDUs without badge ([9eceae0](https://github.com/wakamex/wakterm/commit/9eceae0a8)). Backward compatibility for older clients that do not send the agent badge field.
- Restore codec version 46 ([1a16da1](https://github.com/wakamex/wakterm/commit/1a16da163)).

### Parser and Misc

- Fix tmux CC parser error on empty line during detach ([701b950](https://github.com/wakamex/wakterm/commit/701b9508c)). Fixes #7656.
- Add chrono clock feature ([6e5b38a](https://github.com/wakamex/wakterm/commit/6e5b38a9f)).

### Observability

- Add mux observability for layout issues. The mux server logs errors for `ResizeTab` pane-count mismatches, unknown pane ids, and split-tree invariant failures.
- Add `check-pane-layout.py` live layout validator. Validates `wakterm cli list --format json` output against a legal split tree.
- Track .git/HEAD and refs/heads for version string freshness ([dcd417b](https://github.com/wakamex/wakterm/commit/dcd417b0f)).

### Security

- Disable DECRQCRA screen scraping responses by default (`enable_checksum_rectangular_area = false`).
- Update serde_with dependency for GHSA-7gcf-g7xr-8hxj.

### Compatibility

- Mux protocol diverged from upstream. wakterm clients and servers must be the same build; connecting to an upstream wezterm mux server is not supported.

### Test Coverage

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
