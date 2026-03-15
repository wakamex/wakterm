# Wez's Terminal (wakamex fork)

<img height="128" alt="WezTerm Icon" src="https://raw.githubusercontent.com/wezterm/wezterm/main/assets/icon/wezterm-icon.svg" align="left"> *A GPU-accelerated cross-platform terminal emulator and multiplexer written by <a href="https://github.com/wez">@wez</a> and implemented in <a href="https://www.rust-lang.org/">Rust</a>*

User facing docs and guide at: https://wezterm.org/

## About this fork

This is an actively maintained fork of [wezterm/wezterm](https://github.com/wezterm/wezterm). Upstream development has slowed, and this fork fixes bugs that affect daily mux server usage.

### What's changed

**Bug fixes** — see [CHANGELOG-FORK.md](CHANGELOG-FORK.md) for details:

- Nested split pane sizes diverging after window resize (#6052, #5011, #5117)
- Infinite loop when shrinking window with splits (#4878)
- OOM crash from oversized PDU allocation (#7527)
- GUI deadlock on tmux CC domain detach (#7661)
- Pane rotation not syncing to mux server (#6397)
- `--attach` flag ignored when delegating to running instance (#7582)
- Tab size wrong after top-level split (#7654, #2579, #4984)
- tmux CC parser error on empty line during detach (#7656)
- Resize feedback loop between client and server
- GUI clamp for zero-dimension resize requests

**Session persistence** — tabs survive server restarts:
- Auto-saves tab layout, split tree structure, working directories, and titles every 60s and on SIGTERM
- Auto-restores on startup with correct nested splits and proportional sizing
- `wez-tabs save/restore/show` CLI for manual control and agent relaunching

**Window geometry** — macOS remembers window position and size across restarts via native `NSWindow` autosave

**Resize stability:**
- Batched `ResizeTab` PDU prevents per-pane interleaving
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

- Codec version 47
- Both client and server should run this fork for full functionality

---

![Screenshot](docs/screenshots/two.png)

*Screenshot of wezterm on macOS, running vim*

## Installation

https://wezterm.org/installation

## Getting help

This is a spare time project, so please bear with me.  There are a couple of channels for support:

* You can use the [GitHub issue tracker](https://github.com/wezterm/wezterm/issues) to see if someone else has a similar issue, or to file a new one.
* Start or join a thread in our [GitHub Discussions](https://github.com/wezterm/wezterm/discussions); if you have general
  questions or want to chat with other wezterm users, you're welcome here!
* There is a [Matrix room via Element.io](https://app.element.io/#/room/#wezterm:matrix.org)
  for (potentially!) real time discussions.

The GitHub Discussions and Element/Gitter rooms are better suited for questions
than bug reports, but don't be afraid to use whichever you are most comfortable
using and we'll work it out.

## Supporting the Project

If you use and like WezTerm, please consider sponsoring it: your support helps
to cover the fees required to maintain the project and to validate the time
spent working on it!

[Read more about sponsoring](https://wezterm.org/sponsor.html).

* [![Sponsor WezTerm](https://img.shields.io/github/sponsors/wez?label=Sponsor%20WezTerm&logo=github&style=for-the-badge)](https://github.com/sponsors/wez)
* [Patreon](https://patreon.com/WezFurlong)
* [Ko-Fi](https://ko-fi.com/wezfurlong)
* [Liberapay](https://liberapay.com/wez)
