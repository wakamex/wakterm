# Changelog - wakamex/wakterm fork

All changes relative to upstream wakterm/wakterm main at 05343b387.

## Changes from 2026-03-20 through 2026-08-23 (commits 711022567 through 8b4429a98)

### Agent Harnesses and Agent API

- Top-level `wakterm agent` CLI command for starting, adopting, observing, and controlling agent harnesses ([a4716fd](https://github.com/wakamex/wakterm/commit/a4716fdbe), [ae2019f](https://github.com/wakamex/wakterm/commit/ae2019f0d)).
- Automatic adoption: promote detected harness processes (Agy, Claude, Codex, Gemini, OpenCode) with confirmed backing sessions to persistent agent metadata automatically ([7c6b731](https://github.com/wakamex/wakterm/commit/7c6b731bc), [a4d1fdb](https://github.com/wakamex/wakterm/commit/a4d1fdb03), [8b4429a](https://github.com/wakamex/wakterm/commit/8b4429a98)).
- Add Agy agent detection, tab bar harness icon, and observer-backed session adoption ([6ec0b15](https://github.com/wakamex/wakterm/commit/6ec0b1503), [4b344f0](https://github.com/wakamex/wakterm/commit/4b344f0a8), [8b4429a](https://github.com/wakamex/wakterm/commit/8b4429a98)).
- Refresh agent identity and observer binding when an agent harness restarts inside an existing pane ([39d5c4a](https://github.com/wakamex/wakterm/commit/39d5c4a2c)).
- Exact Codex session restoration: restore Codex sessions with persisted restore intent across multiplexer restart and system reboot in their declared working directory, resuming exact provider threads and settling interrupted turns without guaranteeing that an in-flight turn continues ([2053ce0](https://github.com/wakamex/wakterm/commit/2053ce025), [820d911](https://github.com/wakamex/wakterm/commit/820d911dd), [734be6b](https://github.com/wakamex/wakterm/commit/734be6ba5), [ca10d85](https://github.com/wakamex/wakterm/commit/ca10d85a0)).
- Mux-supervised Codex app-server TUI (`wakterm agent launch codex`): multiplexer manages shared app-server over private Unix socket while the pane runs native Codex TUI ([ae2019f](https://github.com/wakamex/wakterm/commit/ae2019f0d), [5e8bca8](https://github.com/wakamex/wakterm/commit/5e8bca8f9)).
- Versioned Agent API v1:
  - Golden schema fixtures and specification ([efea923](https://github.com/wakamex/wakterm/commit/efea923c5))
  - Narrow agent catalog (`wakterm agent catalog`) with stable agent ID, ephemeral pane ID, and sequence cursor ([c65d27a](https://github.com/wakamex/wakterm/commit/c65d27af8))
  - Authoritative prompt admission (`wakterm agent admit`) with process incarnation checks and idempotency keys ([f0dce1e](https://github.com/wakamex/wakterm/commit/f0dce1ed5))
  - Durable normalized event stream (`wakterm agent events`) with strictly increasing sequence numbers ([af748ff](https://github.com/wakamex/wakterm/commit/af748ffc5))
  - Asynchronous return requests (`wakterm agent send --return-final` and `wakterm agent request get|watch|cancel`) ([1ac7932](https://github.com/wakamex/wakterm/commit/1ac79325d))
- Experimental shadow output page (`wakterm agent output`) for normalized assistant message comparisons ([d403026](https://github.com/wakamex/wakterm/commit/d403026a8)).
- Native harness tab icons for Agy, Claude, Codex, Gemini, and OpenCode in the fancy tab bar ([5f1927b](https://github.com/wakamex/wakterm/commit/5f1927b23), [66afcca](https://github.com/wakamex/wakterm/commit/66afccadc), [4b344f0](https://github.com/wakamex/wakterm/commit/4b344f0a8)).
- Agent attention pulse (`agent_tab_attention_pulse`): smooth icon fade when an agent completes a turn and awaits review, with shared review acknowledgement across clients ([419db5a](https://github.com/wakamex/wakterm/commit/419db5a22), [5c25cc4](https://github.com/wakamex/wakterm/commit/5c25cc434), [0f6aefb](https://github.com/wakamex/wakterm/commit/0f6aefb95)).
- `ActivateNextTabNeedingAttention`: shortcut action to jump directly to the next tab with an unreviewed completed turn ([419db5a](https://github.com/wakamex/wakterm/commit/419db5a22)).
- Automatic tab naming: unnamed tabs derive titles from the active agent leaf directory, with numeric suffixes to resolve collisions ([4f7242c](https://github.com/wakamex/wakterm/commit/4f7242c26), [f8d9ae6](https://github.com/wakamex/wakterm/commit/f8d9ae61a)).

### Parked Tabs and Tab Navigator

- Parked tabs (`ParkCurrentTab` / Ctrl-Shift-S on Linux/Windows, Cmd-Shift-S on macOS): hide inactive tabs from the tab strip while keeping processes, PTYs, and sessions alive in the multiplexer ([419db5a](https://github.com/wakamex/wakterm/commit/419db5a22), [3661dc4](https://github.com/wakamex/wakterm/commit/3661dc41e)).
- Dedicated tab navigator (`ShowTabNavigator` / Ctrl-Shift-E on Linux/Windows, Cmd-E on macOS) ([419db5a](https://github.com/wakamex/wakterm/commit/419db5a22), [f183c9d](https://github.com/wakamex/wakterm/commit/f183c9dd9), [701a2c6](https://github.com/wakamex/wakterm/commit/701a2c6ad)):
  - Visible, Parked, and All views
  - Instant fuzzy search across title, CWD, branch, harness, and agent name
  - Responsive metadata columns: status, last response age, CWD, branch, pane count, and approximate process RSS
  - In-place parking and unparking (Ctrl-Shift-S)
  - Permanent tab close with confirmation (Ctrl-X)
  - Sort by tab order or response time (Ctrl-R)
  - Dense single-line or comfortable multi-line pane layout (Ctrl-O)

### Appearance and Theming

- Built-in per-tab generated colors (`tab_bar_color_mode = "Off"|"Hash"|"Assign"`, `tab_bar_color_palette = "Dark"|"Light"|"Mixed"`, `tab_bar_color_intensity`) ([c02b576](https://github.com/wakamex/wakterm/commit/c02b57652), [3dbcf35](https://github.com/wakamex/wakterm/commit/3dbcf35bb), [f608264](https://github.com/wakamex/wakterm/commit/f6082641f)).
- Color similarity search in the interactive color scheme browser ([e5565cb](https://github.com/wakamex/wakterm/commit/e5565cb1d)).
- Configurable agent tab badge mode (`agent_tab_badge_mode = "identity"|"attention"|"turn"|"off"`) ([66afcca](https://github.com/wakamex/wakterm/commit/66afccadc)).

### Layout and Navigation

- `PaneSelect(mode="SwapWithActive")` bound to Ctrl-Shift-M ([c7736da](https://github.com/wakamex/wakterm/commit/c7736da26)).
- macOS pane navigation shortcuts (Cmd-Alt-Left/Right/Up/Down) ([0f9963b](https://github.com/wakamex/wakterm/commit/0f9963b26)).
- Shift-Enter compatibility binding ([8016097](https://github.com/wakamex/wakterm/commit/8016097e4)).

### Multiplexer Persistence and Reliability

- Systemd service integration: `install-user-service.sh` for user service management with lingering, and `install-system-service.sh` for system-wide service boot persistence ([cd0a922](https://github.com/wakamex/wakterm/commit/cd0a9225c), [21f6417](https://github.com/wakamex/wakterm/commit/21f6417f7), [9b09fc1](https://github.com/wakamex/wakterm/commit/9b09fc133)).
- Explicit resize origin tracking: suppresses client self-echoes without timing heuristics, eliminating resize flicker and feedback loops ([a961262](https://github.com/wakamex/wakterm/commit/a9612626d)).
- Synchronized tab ordering: authoritative server tab order with drift reconciliation on client reconnect ([6c6b4fa](https://github.com/wakamex/wakterm/commit/6c6b4fa69), [63e94d1](https://github.com/wakamex/wakterm/commit/63e94d1f8)).
- Fix deadlock in session save vs main thread executor ([2ff8f7c](https://github.com/wakamex/wakterm/commit/2ff8f7c9d)).
- Fix mux subscriber lifetime leak ([bba995d](https://github.com/wakamex/wakterm/commit/bba995d8e)).
- Prevent notification backlog disconnects during heavy event traffic ([4dacab7](https://github.com/wakamex/wakterm/commit/4dacab75c)).
- Wake mux loop for clean shutdown on server termination ([18467f6](https://github.com/wakamex/wakterm/commit/18467f6e6)).
- Settle interrupted Codex turns after restore ([734be6b](https://github.com/wakamex/wakterm/commit/734be6ba5)).
- Restore agents in declared working directory ([820d911](https://github.com/wakamex/wakterm/commit/820d911dd)).
- Preserve hosted pane paths across platforms ([738575c](https://github.com/wakamex/wakterm/commit/738575cab)).
- OpenSSH symlink compatibility: resolve binary path across symlinked OpenSSH binaries ([0cf3fe0](https://github.com/wakamex/wakterm/commit/0cf3fe08a)).
- Fix duplicate tempdir stores idempotency in blob leases ([8d7abe4](https://github.com/wakamex/wakterm/commit/8d7abe40e)).
- Avoid sync agent detection blocking during client attach ([c3d57f8](https://github.com/wakamex/wakterm/commit/c3d57f8df)).

### Terminal and Input Fixes

- Reset extended state and modes on RIS escape sequence, [#26](https://github.com/wakamex/wakterm/issues/26) ([5cc8708](https://github.com/wakamex/wakterm/commit/5cc87080c)).
- Prevent divide-by-zero panic in inline image placement, [#22](https://github.com/wakamex/wakterm/issues/22) ([8699d68](https://github.com/wakamex/wakterm/commit/8699d68c2)).
- Stop pane search after regex errors, [#24](https://github.com/wakamex/wakterm/issues/24) ([f42a581](https://github.com/wakamex/wakterm/commit/f42a58123)).
- Route IME composed text through prompt overlays, [#25](https://github.com/wakamex/wakterm/issues/25) ([9a7c66d](https://github.com/wakamex/wakterm/commit/9a7c66d93)).
- Encode Escape correctly with Kitty keyboard disambiguation, [#23](https://github.com/wakamex/wakterm/issues/23) ([1d7c5d7](https://github.com/wakamex/wakterm/commit/1d7c5d7f3)).
- Disable DECRQCRA screen scraping responses by default, [#21](https://github.com/wakamex/wakterm/issues/21) ([1a3f253](https://github.com/wakamex/wakterm/commit/1a3f25377)).
- Terminate tmux send-keys commands with LF, [#28](https://github.com/wakamex/wakterm/issues/28) ([29b3d49](https://github.com/wakamex/wakterm/commit/29b3d4931)).
- Support Unicode input in tmux control mode, [#29](https://github.com/wakamex/wakterm/issues/29) ([5d108e0](https://github.com/wakamex/wakterm/commit/5d108e084)).
- Guard ZSH_NAME under Bash nounset (set -u), [#38](https://github.com/wakamex/wakterm/issues/38) ([174b81a](https://github.com/wakamex/wakterm/commit/174b81aeb)).
- Avoid reentrant window lock during title updates and IME borrow operations ([6a67d08](https://github.com/wakamex/wakterm/commit/6a67d0825)).

### Linux and Wayland Fixes

- Detect WSL case-insensitively, [#37](https://github.com/wakamex/wakterm/issues/37) ([a6245e0](https://github.com/wakamex/wakterm/commit/a6245e056)).
- Honor disabled titlebar decorations on Wayland, [#33](https://github.com/wakamex/wakterm/issues/33) ([f6d6bd6](https://github.com/wakamex/wakterm/commit/f6d6bd6ea)).
- Ignore data-device events for non-toplevel surfaces on Wayland, [#32](https://github.com/wakamex/wakterm/issues/32) ([4c8e004](https://github.com/wakamex/wakterm/commit/4c8e00452)).
- Share clipboard offers across windows on Wayland, [#31](https://github.com/wakamex/wakterm/issues/31) ([3b4e56f](https://github.com/wakamex/wakterm/commit/3b4e56fca)).
- Align Wayland buffers to buffer scale, [#30](https://github.com/wakamex/wakterm/issues/30) ([bd5495a](https://github.com/wakamex/wakterm/commit/bd5495a97)).
- Keep render subscription alive across workspace closure, [#36](https://github.com/wakamex/wakterm/issues/36) ([561a6e4](https://github.com/wakamex/wakterm/commit/561a6e4aa)).

### Windows and macOS Fixes

- Restore window placement on Windows ([2d4098f](https://github.com/wakamex/wakterm/commit/2d4098fef)).
- Handle TerminateProcess result correctly on Windows, [#35](https://github.com/wakamex/wakterm/issues/35) ([6e1f0cc](https://github.com/wakamex/wakterm/commit/6e1f0ccaf)).
- Avoid reentrant IME borrow panic on Windows, [#34](https://github.com/wakamex/wakterm/issues/34) ([9b497c5](https://github.com/wakamex/wakterm/commit/9b497c553)).
- Reconcile focus after showing window on Windows ([27d7673](https://github.com/wakamex/wakterm/commit/27d76738f)).
- Prevent GUI freeze and delayed tab opening on macOS during client attach and background spawn handoff ([9630dbe](https://github.com/wakamex/wakterm/commit/9630dbebb), [07422e2](https://github.com/wakamex/wakterm/commit/07422e220), [455f3cb](https://github.com/wakamex/wakterm/commit/455f3cb9d)).

### Memory and Resource Bounds

- Bound SynchronizedOutput buffer to 4MB to prevent memory leaks from unresponsive TUI applications ([0dc472e](https://github.com/wakamex/wakterm/commit/0dc472eca)).
- Bound adversarial terminal sequence parsing memory allocation ([a905a0a](https://github.com/wakamex/wakterm/commit/a905a0a05)).
- Bound Kitty image memory allocation work ([af5542e](https://github.com/wakamex/wakterm/commit/af5542e34)).
- Prevent stack overflow from process tree cycles in procinfo, [#20](https://github.com/wakamex/wakterm/issues/20) ([7729b72](https://github.com/wakamex/wakterm/commit/7729b72ab)).
- Update serde_with for GHSA-7gcf-g7xr-8hxj, [#27](https://github.com/wakamex/wakterm/issues/27) ([803456f](https://github.com/wakamex/wakterm/commit/803456f70)).
- Bound diagnostic log retention ([0c4b3c7](https://github.com/wakamex/wakterm/commit/0c4b3c746)).

## Historical Features and Fixes (prior to 2026-03-20)

### Agent Harnesses

- Add pane-owned agent identity and persistence ([dcd1d10](https://github.com/wakamex/wakterm/commit/dcd1d1068))
  Agents (Claude, Codex, Gemini, OpenCode) are first-class mux panes with identity, state tracking, and persistence across server restarts.

- Add agent lifecycle commands ([9def1d0](https://github.com/wakamex/wakterm/commit/9def1d07b))
  `wakterm cli agent start|stop|list` for managing agent harness panes.

- Add agent runtime, send, and client-side badges ([58ddee7](https://github.com/wakamex/wakterm/commit/58ddee750))
  Send prompts and interrupts to running agents. Tab badges show agent status (waiting/working/your turn).

- Add native harness watch and observer-backed PTY runtime ([10e219e](https://github.com/wakamex/wakterm/commit/10e219e19))
  `wakterm cli agent watch` and `wakterm cli agent list -f` for live progress across running harnesses.

- Fix raw input path for gemini agent sends ([fbe9ccd](https://github.com/wakamex/wakterm/commit/fbe9ccd0d))

### Tab Management

- Add prompt rename tab action ([86e661f](https://github.com/wakamex/wakterm/commit/86e661f5c))
  New `PromptRenameTab` action lets users rename tabs interactively.

- Add default shortcut for prompt rename tab ([01d3ab0](https://github.com/wakamex/wakterm/commit/01d3ab07c))
  Bound to Ctrl+Shift+< by default, later moved to Shift+Comma ([f249f9f](https://github.com/wakamex/wakterm/commit/f249f9f42)).

- Add move-tab bracket shortcuts ([de89169](https://github.com/wakamex/wakterm/commit/de89169fe))
  Ctrl+Shift+[ and Ctrl+Shift+] to reorder tabs.

- Preserve user-set tab titles from escape sequences ([63c30dc](https://github.com/wakamex/wakterm/commit/63c30dcb0))
  Titles set via `PromptRenameTab` or the Lua API are no longer overwritten by terminal escape sequences.

- Add safe tab effective title for Lua ([1624be0](https://github.com/wakamex/wakterm/commit/1624be0e0))
  Exposes a Lua-accessible effective title that respects user overrides.

### GUI

- Remember window position and size on macOS ([05ed9a7](https://github.com/wakamex/wakterm/commit/05ed9a7c3))
  Uses native `NSWindow` autosave so window geometry persists across restarts.

- Make tab reordering atomic ([946d01b](https://github.com/wakamex/wakterm/commit/946d01b93))
  Tab drag-reorder is now a single atomic operation, avoiding intermediate invalid states.

- Clip pane glyphs to pane bounds ([d16d9b4](https://github.com/wakamex/wakterm/commit/d16d9b49f))
  Glyphs that extend past a pane's edges are now clipped instead of bleeding into adjacent panes.

- Invalidate line quads when pane width changes ([3c82389](https://github.com/wakamex/wakterm/commit/3c8238958))
  Fixes stale rendered content after pane resizes.

- Repaint window on tab resize ([7f4a541](https://github.com/wakamex/wakterm/commit/7f4a54187))

- Improve `wakterm cli list` table layout ([a5a9966](https://github.com/wakamex/wakterm/commit/a5a996685))

### Docs Site

- Replace colorscheme index pages with interactive browser ([839988a](https://github.com/wakamex/wakterm/commit/839988a5a))
  Removed hundreds of static colorscheme pages and replaced them with a searchable, filterable browser with live previews.

- Modernize docs site ([839988a](https://github.com/wakamex/wakterm/commit/839988a5a))
  Dropped legacy asciinema player, mdbook assets, and custom CSS. Trimmed global page load cost.

### Split & Resize Reliability

- Sync divider drags via atomic ResizeTab batches
  `resize_split_by()` now sends the same tab-level `ResizeTab` batch used by full window resizes, so dragging a split divider updates the mux server coherently instead of leaving client-only pane widths behind.

- Fix spawn sizing across entry points
  `wakterm cli spawn`, delegation into an already-running GUI instance, and existing-window mux spawns now use the live tab size instead of falling back to tiny server defaults.

- Fix client ResizeTab pane id mapping
  Batched resize messages now translate client-local pane ids back to remote mux pane ids before sending them to the server, fixing fresh-session tabs that stayed at 80x24 despite correct pane sizes.

- Fix nested split pane sizes diverging after window resize ([de54b07](https://github.com/wakamex/wakterm/commit/de54b07d2))
  Per-pane `Pdu::Resize` messages interleave during rapid resizing, causing the mux server's tree to diverge. Added `reconcile_tree_sizes()` -- a top-down constraint enforcement pass after every tree mutation. 14 unit tests covering 6 layout patterns.
  Fixes [#6052](https://github.com/wez/wezterm/issues/6052), [#5011](https://github.com/wez/wezterm/issues/5011), [#5117](https://github.com/wez/wezterm/issues/5117).

- Fix infinite loop on extreme window shrink ([80447df](https://github.com/wakamex/wakterm/commit/80447dfde))
  `adjust_y_size`/`adjust_x_size` loop forever when both split children reach 1 row/col. Added early return when no progress is made.
  Fixes [#4878](https://github.com/wez/wezterm/issues/4878).

- Batch per-pane resize PDUs into atomic ResizeTab message ([f39b4cc](https://github.com/wakamex/wakterm/commit/f39b4cc6a))
  Eliminates the root cause of resize interleaving. New `ResizeTab` PDU (codec type 63) sends all pane sizes atomically. Individual `Pdu::Resize` still sent as fallback for older servers.

- Stop sending individual Pdu::Resize, rely on batched ResizeTab ([5adbc17](https://github.com/wakamex/wakterm/commit/5adbc17be))

- Fix split-pane race by sending tab size with SplitPane PDU ([5d94a78](https://github.com/wakamex/wakterm/commit/5d94a7885))

- Force tab resize after split_pane to sync PTY sizes with tree ([fffb3f8](https://github.com/wakamex/wakterm/commit/fffb3f825))

- Clamp tiny resize geometry to at least 1x1 cells ([8968ff4](https://github.com/wakamex/wakterm/commit/8968ff422))
  Prevents zero-dimension resize requests from reaching the mux layer.

- Restore tab size after top-level split ([9b04ef8](https://github.com/wakamex/wakterm/commit/9b04ef81c))
  `split_and_insert` with `top_level=true` didn't restore `self.size` after pre-resizing, causing subsequent splits to fail with "No space for split!".
  Fixes [#7654](https://github.com/wez/wezterm/issues/7654), [#2579](https://github.com/wez/wezterm/issues/2579), [#4984](https://github.com/wez/wezterm/issues/4984).

- Focus new pane after split ([1fa85af](https://github.com/wakamex/wakterm/commit/1fa85afef))

### Multi-Client Stability

- Break resize feedback loop ([76a1695](https://github.com/wakamex/wakterm/commit/76a169534))
  Client no longer resyncs on `TabResized`, breaking the loop where resize -> resync -> resize spiralled.

- Suppress self-echo TabResized, forward from other clients ([daa899b](https://github.com/wakamex/wakterm/commit/daa899b9b))
  Server no longer echoes `TabResized` back to the client that triggered it.

- Restore TabResized resync after self-echo filtering ([ec0250f](https://github.com/wakamex/wakterm/commit/ec0250f05))
  With self-echo gone, resync on `TabResized` from other clients is safe again.

- Debounce resync storms instead of dropping ([039980c](https://github.com/wakamex/wakterm/commit/039980c0a))

- Avoid pane focus feedback loops across clients ([cae82e4](https://github.com/wakamex/wakterm/commit/cae82e478))

- Make active tab state client-local ([ff0b2a5](https://github.com/wakamex/wakterm/commit/ff0b2a5f3))
  Each connected client tracks its own active tab instead of fighting over a shared global.

- Avoid reentrant window lock when moving tabs ([0a05f94](https://github.com/wakamex/wakterm/commit/0a05f947b))

- Fix mux client registration handshake ordering ([d3993c2](https://github.com/wakamex/wakterm/commit/d3993c284))

### Session Persistence

- Rewrite session restore with recursive tree walk ([26cc34b](https://github.com/wakamex/wakterm/commit/26cc34bed))
  Replays the exact split tree instead of reconstructing from flat pane rectangles.

- Use percentage splits for session restore ([d81bd83](https://github.com/wakamex/wakterm/commit/d81bd830d))
  Proportional sizing instead of absolute cell counts, so restores adapt to different window sizes.

- Heal degenerate splits before saving, clamp to 10-90% ([27dc63d](https://github.com/wakamex/wakterm/commit/27dc63d38))

- Use generous initial size for session restore ([42299768](https://github.com/wakamex/wakterm/commit/42299768d))

- Reconcile tree after session restore to fix column height mismatches ([6cb68dc](https://github.com/wakamex/wakterm/commit/6cb68dc9c))

- Preserve active tab selection in manual and automatic restore
  `ListPanesResponse` now carries the active tab per window, `wakterm cli save-layout` records it, manual restore focuses the saved tab after rebuilding the window, client attach/resync tracks it, and built-in mux session restore reapplies the saved active tab.

- Add Rust `wakterm cli save-layout` / `restore-layout` and remove `wez-tabs`
  Manual layout snapshots now use the real mux pane tree instead of reconstructing split order from flat pane rectangles. Restore replays exact split cells, preserves tab/window grouping, titles, workspaces, active tab selection, per-tab active pane selection, and zoom state.

### Mux Protocol / Server

- Fix OOM from unbounded SynchronizedOutput buffer
  When a TUI app enables SynchronizedOutput (`CSI?2026h`) and crashes or gets stuck without disabling it, the mux server accumulated parsed actions without bound. Over days, this grew to 25GB+ and triggered the OOM killer. Added a 4MB safety valve that force-flushes the buffer during hold. Also added 60-second memory reporting (RSS + per-pane action buffer sizes) at INFO log level.

- Reject oversized PDUs before allocation ([e1e8510](https://github.com/wakamex/wakterm/commit/e1e8510b3))
  Both `decode_raw` and `decode_raw_async` allocated buffers from untrusted wire data without bounds. Added `MAX_PDU_SIZE` (64 MiB) check.
  Fixes [#7527](https://github.com/wez/wezterm/issues/7527).

- Fix deadlock in domain_was_detached ([1a9b10d](https://github.com/wakamex/wakterm/commit/1a9b10dbb))
  Held `windows.write()` while calling into `tab.kill_panes_in_domain()`, creating a lock-ordering deadlock with the GUI render path. Downgraded to `windows.read()` and released before operating on tabs.
  Fixes [#7661](https://github.com/wez/wezterm/issues/7661).

- Add RotatePanes PDU ([3ebe927](https://github.com/wakamex/wakterm/commit/3ebe927ea))
  `rotate_clockwise`/`rotate_counter_clockwise` were local-only -- the server's tree diverged after rotation. Added `RotatePanes` PDU (codec type 64) to keep server in sync.
  Fixes [#6397](https://github.com/wez/wezterm/issues/6397).

- Pass --attach flag through try_spawn ([f283ee0](https://github.com/wakamex/wakterm/commit/f283ee0ae))
  `wakterm start --attach --domain X` delegated to an existing instance but always spawned a new tab, ignoring `--attach`. Now checks for existing panes and skips spawning.
  Fixes [#7582](https://github.com/wez/wezterm/issues/7582).

- Clarify stale mux server version mismatch errors ([55f3de1](https://github.com/wakamex/wakterm/commit/55f3de1d8))

- Log client version on connect ([95d16ce](https://github.com/wakamex/wakterm/commit/95d16ce8b))

### Codec

- Accept legacy tab title PDUs without badge ([9eceae0](https://github.com/wakamex/wakterm/commit/9eceae0a8))
  Backward compatibility for older clients that don't send the agent badge field.

- Restore codec version 46 ([1a16da1](https://github.com/wakamex/wakterm/commit/1a16da163))
  Both client and server are built from the fork, so the intermediate version bump was unnecessary.

### Parser / Misc

- Fix tmux CC parser error on empty line during detach ([701b950](https://github.com/wakamex/wakterm/commit/701b9508c))
  Empty lines during tmux `-CC` detach caused parser errors in the debug overlay.
  Fixes [#7656](https://github.com/wez/wezterm/issues/7656).

- Add chrono clock feature ([6e5b38a](https://github.com/wakamex/wakterm/commit/6e5b38a9f))
  The workspace chrono dependency was missing the `clock` feature, preventing `Utc::now()` from compiling.

## Observability

- Add mux observability for layout issues
  The mux server logs hard errors for `ResizeTab` pane-count mismatches, unknown pane ids, and split-tree invariant failures.

- Add `check-pane-layout.py` live layout validator
  Validates `wakterm cli list --format json` output against a legal split tree so offscreen panes, overlaps, gaps, and degenerate rectangles are easy to catch from a live session.

- Track .git/HEAD and refs/heads for version string freshness ([dcd417b](https://github.com/wakamex/wakterm/commit/dcd417b0f))

## Security

- Disable DECRQCRA checksum responses by default to prevent terminal screen
  contents from being queried silently. Set
  `enable_checksum_rectangular_area = true` only when compatibility requires
  it.

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
