# wakterm (wakamex fork)

This is an actively maintained fork of [wezterm/wezterm](https://github.com/wezterm/wezterm). Upstream development has slowed, and this fork fixes bugs that affect daily mux server usage. I also took the opportunity to make agent harnesses first-class citizens 🤖.

### What's changed

See [CHANGELOG-FORK.md](CHANGELOG-FORK.md) for the detailed fork fix history.

**Session persistence**
- Auto-saves tab layout, split tree structure, working directories, and titles every 60s and on SIGTERM
- Auto-restores on startup with correct nested splits, proportional sizing, and active-tab selection
- `wakterm cli save-layout` / `wakterm cli restore-layout` for exact Rust-backed manual snapshots, replay, and active-tab restore

**Agent harnesses**
- Claude, Codex, Gemini, and OpenCode are first-class citizens
- Start them directly with `wakterm cli agent start claude|codex|gemini|opencode`
- Watch live progress across running harnesses with `wakterm cli agent watch` or `wakterm cli agent list -f`
- Send prompts and interrupts through wakterm while keeping the real harness UI in the pane
- Tab title tells you if an agent is waiting on you, or if it's your turn (configurable)
- `agent` is a shortcut for `wakterm cli agent`

**Usability**
- macOS remembers window position and size across restarts via native `NSWindow` autosave
- Divider drags, window resizes, and spawned panes agree on the same layout more often
- Multi-client sessions are much less prone to flickering, jumpy redraws, and resize feedback loops
- User-set titles (via Ctrl+Shift+<) are no longer overwritten by terminal escape sequences

**5 new default key bindings:**

| Key | Action |
|-----|--------|
| Ctrl+Shift+D (Cmd+D) | Close current pane |
| Shift+Home | Scroll to top |
| Shift+End | Scroll to bottom |
| Ctrl+Shift+O (Cmd+O) | Rotate panes clockwise |
| Ctrl+Shift+E (Cmd+E) | Tab navigator |

Full hotkey reference: [HOTKEYS.md](HOTKEYS.md)

---

## Installation

https://wakterm.org/installation

## Getting help

If you find any issues with this fork, file a GitHub issue.

## Supporting the project

If you use and like wakterm, consider [supporting WezTerm](https://wezterm.org/sponsor.html), the project it was built on.
