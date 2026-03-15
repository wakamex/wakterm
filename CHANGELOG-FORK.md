# Changelog — wakamex/wezterm fork

All changes relative to upstream `wezterm/wezterm` main at `05343b387`.

## Bug Fixes

### Resize / Split Tree

- **Fix nested split pane sizes diverging after window resize** ([de54b07](https://github.com/wakamex/wezterm/commit/de54b07d2))
  Per-pane `Pdu::Resize` messages interleave during rapid resizing, causing the mux server's tree to diverge. Added `reconcile_tree_sizes()` — a top-down constraint enforcement pass after every tree mutation. 14 unit tests covering 6 layout patterns.
  Fixes #6052, #5011, #5117

- **Fix infinite loop on extreme window shrink** ([80447df](https://github.com/wakamex/wezterm/commit/80447dfde))
  `adjust_y_size`/`adjust_x_size` loop forever when both split children reach 1 row/col. Added early return when no progress is made.
  Fixes #4878

- **Batch per-pane resize PDUs into atomic ResizeTab message** ([f39b4cc](https://github.com/wakamex/wezterm/commit/f39b4cc6a))
  Eliminates the root cause of resize interleaving. New `ResizeTab` PDU (codec type 63) sends all pane sizes atomically. Individual `Pdu::Resize` still sent as fallback for older servers.

- **Clamp tiny resize geometry to at least 1x1 cells** ([8968ff4](https://github.com/wakamex/wezterm/commit/8968ff422))
  Prevents zero-dimension resize requests from reaching the mux layer.

- **Restore tab size after top-level split** ([9b04ef8](https://github.com/wakamex/wezterm/commit/9b04ef81c))
  `split_and_insert` with `top_level=true` didn't restore `self.size` after pre-resizing, causing subsequent splits to fail with "No space for split!".
  Fixes #7654, #2579, #4984

### Mux Protocol / Server

- **Reject oversized PDUs before allocation** ([e1e8510](https://github.com/wakamex/wezterm/commit/e1e8510b3))
  Both `decode_raw` and `decode_raw_async` allocated buffers from untrusted wire data without bounds. Added `MAX_PDU_SIZE` (64 MiB) check.
  Fixes #7527

- **Fix deadlock in domain_was_detached** ([1a9b10d](https://github.com/wakamex/wezterm/commit/1a9b10dbb))
  Held `windows.write()` while calling into `tab.kill_panes_in_domain()`, creating a lock-ordering deadlock with the GUI render path. Downgraded to `windows.read()` and released before operating on tabs.
  Fixes #7661

- **Add RotatePanes PDU** ([3ebe927](https://github.com/wakamex/wezterm/commit/3ebe927ea))
  `rotate_clockwise`/`rotate_counter_clockwise` were local-only — the server's tree diverged after rotation. Added `RotatePanes` PDU (codec type 64) to keep server in sync.
  Fixes #6397

- **Pass --attach flag through try_spawn** ([f283ee0](https://github.com/wakamex/wezterm/commit/f283ee0ae))
  `wezterm start --attach --domain X` delegated to an existing instance but always spawned a new tab, ignoring `--attach`. Now checks for existing panes and skips spawning.
  Fixes #7582

### Parser / Misc

- **Fix tmux CC parser error on empty line during detach** ([701b950](https://github.com/wakamex/wezterm/commit/701b9508c))
  Empty lines during tmux `-CC` detach caused parser errors in the debug overlay.
  Fixes #7656

- **Add chrono clock feature** ([6e5b38a](https://github.com/wakamex/wezterm/commit/6e5b38a9f))
  The workspace chrono dependency was missing the `clock` feature, preventing `Utc::now()` from compiling.

## Codec Version

Bumped from 45 to 46 for new PDU types (`ResizeTab`, `RotatePanes`).

## Test Coverage

26 tests added (17 mux, 9 codec) covering:
- 6 layout patterns (L-shape, T-shape, grid, deep-nested, first-pane-stale, column-width)
- Interleaved PDU scenarios from rapid resize events
- Pane removal, split+resize, extreme shrink/grow cycles
- Oversized PDU rejection
- tmux CC empty line handling
- Top-level split tab size preservation
