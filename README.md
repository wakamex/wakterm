# Wez's Terminal (wakamex fork)

<img height="128" alt="WezTerm Icon" src="https://raw.githubusercontent.com/wezterm/wezterm/main/assets/icon/wezterm-icon.svg" align="left"> *A GPU-accelerated cross-platform terminal emulator and multiplexer written by <a href="https://github.com/wez">@wez</a> and implemented in <a href="https://www.rust-lang.org/">Rust</a>*

User facing docs and guide at: https://wezterm.org/

## About this fork

This is an actively maintained fork of [wezterm/wezterm](https://github.com/wezterm/wezterm). Upstream development has slowed, and this fork fixes bugs that affect daily mux server usage.

### What's changed

See [CHANGELOG-FORK.md](CHANGELOG-FORK.md) for the detailed fork fix history.

**Buildability:**

- Builds again on current toolchains after fixing the missing `chrono` `clock` feature required since [`chrono` `0.4.32`](https://github.com/chronotope/chrono/releases/tag/v0.4.32), when Chrono [`split clock into clock and now`](https://github.com/chronotope/chrono/commit/c0f418bbbfa83b1c8392099cec9fdf42657c1e51)

**Session persistence** — tabs survive server restarts:
- Auto-saves tab layout, split tree structure, working directories, and titles every 60s and on SIGTERM
- Auto-restores on startup with correct nested splits, proportional sizing, and active-tab selection
- `wezterm cli save-layout` / `wezterm cli restore-layout` for exact Rust-backed manual snapshots, replay, and active-tab restore

**Window geometry** — macOS remembers window position and size across restarts via native `NSWindow` autosave

**Resize stability:**
- Atomic tab-level `ResizeTab` updates replace interleaving per-pane async resize PDUs
- Divider drags now use the same `ResizeTab` batch path as full tab resizes, keeping local and remote split trees in sync
- Spawn sizing now uses the live tab/window size across CLI spawn, GUI delegation, and mux server split flows
- Server suppresses self-echo `TabResized` to break feedback loops while forwarding to other clients
- Resync debounce queues instead of drops overlapping requests

**Observability:**
- Always-on `size-trace` logging for spawn, split, tab resize, and client/server `ResizeTab` traffic
- Mux server logs hard errors for `ResizeTab` pane-count mismatches, unknown pane ids, and split-tree invariant failures
- `check-pane-layout.py` validates live `wezterm cli list --format json` output against a legal split tree

**Tab titles** — user-set titles (via Ctrl+Shift+<) are no longer overwritten by terminal escape sequences

**5 new default key bindings:**

| Key | Action |
|-----|--------|
| Ctrl+Shift+D (Cmd+D) | Close current pane |
| Shift+Home | Scroll to top |
| Shift+End | Scroll to bottom |
| Ctrl+Shift+O (Cmd+O) | Rotate panes clockwise |
| Ctrl+Shift+E (Cmd+E) | Tab navigator |

Full hotkey reference: [HOTKEYS.md](HOTKEYS.md)

### Compatibility

- Codec version 48
- Both client and server should run this fork for full functionality
- No backwards-compatibility shims for removed fork-only tooling such as `wez-tabs`

---

## Installation

https://wezterm.org/installation

## Getting help

If you find any issues with this fork, just make a GitHub issue.

## Supporting the Project

If you use and like WezTerm, please consider sponsoring it: your support helps
to cover the fees required to maintain the project and to validate the time
spent working on it!

[Read more about sponsoring](https://wezterm.org/sponsor.html).

* [![Sponsor WezTerm](https://img.shields.io/github/sponsors/wez?label=Sponsor%20WezTerm&logo=github&style=for-the-badge)](https://github.com/sponsors/wez)
* [Patreon](https://patreon.com/WezFurlong)
* [Ko-Fi](https://ko-fi.com/wezfurlong)
* [Liberapay](https://liberapay.com/wez)
