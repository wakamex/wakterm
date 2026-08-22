---
hide:
  - toc
---

## wakterm Focus

wakterm keeps the core WezTerm terminal foundation, with extra focus on multiplexer reliability, persistent layouts, and agent-driven workflows.

- Persistent layouts: auto-save and restore tabs, split trees, working directories, titles, and active-tab selection across mux server restarts and reboot
- Manual layout snapshots via `wakterm cli save-layout` and `wakterm cli restore-layout`
- First-class [agent harness panes](agent-lifecycle.md) for Agy, Claude, Codex, Gemini, and OpenCode
- Automatic agent detection and cached session adoption
- Exact-session Codex restoration across mux restarts and system reboot
- Supervised Codex app-server TUI via `wakterm agent launch codex`
- Versioned [Agent API v1](agent-api/v1/README.md) with capability negotiation, catalog, authoritative prompt admission, and durable event streams
- [Parked tabs and dedicated tab navigator](config/lua/keyassignment/ShowTabNavigator.md) (`Ctrl-Shift-E` / `Cmd-E` and `Ctrl-Shift-S` / `Cmd-Shift-S`)
- Agent attention indicators with subtle icon pulse and shared turn review acknowledgement across clients
- Automatic tab naming from active agent directories with collision disambiguation
- Native harness icons and generated per-tab background colors in the tab bar
- Live agent progress with `wakterm agent watch` and `wakterm agent list -f`
- Prompt submission and interrupt flows that keep the real harness UI in the pane
- Multi-client multiplexer synchronization with server authority, explicit resize origin tracking, and reconnect tab order reconciliation
- Reliable split and spawn sizing so panes agree on the same layout across clients
- User-set tab titles are preserved instead of being overwritten by terminal escape sequences

## Core Terminal Features

- Runs on Linux, macOS, and Windows
- [Multiplex terminal panes, tabs and windows on local and remote hosts, with native mouse and scrollback](multiplexing.md)
- Tabs, panes, and multiple windows, with keyboard-driven navigation
- [SSH client with native tabs](ssh.md)
- [Connect to serial ports for embedded and Arduino work](serial.md)
- Connect to a local multiplexer server over unix domain sockets
- Connect to a remote multiplexer using SSH or TLS over TCP/IP
- [Searchable Scrollback](scrollback.md) with keyboard navigation and search mode
- Hyperlinks, shell integration, and dynamic status areas
- Ligatures, Color Emoji, font fallback, and true color with [dynamic color schemes](config/appearance.md)
- Configuration via a [configuration file](config/files.md) with hot reloading
- iTerm2-compatible image protocol support and built-in [imgcat command](imgcat.md)
- Kitty graphics support
- Sixel graphics support

<video width="80%" controls src="screenshots/wakterm-tabs.mp4" loop></video>
