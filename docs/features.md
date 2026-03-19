---
hide:
  - toc
---

## Features

wakterm keeps the core WezTerm feature set, with extra focus on mux
reliability, persistent layouts, and agent-driven workflows.

## Fork Highlights

* Persistent layouts: auto-save and restore tabs, split trees, working
  directories, titles, and active-tab selection across mux server restarts
* Manual layout snapshots via `wakterm cli save-layout` and
  `wakterm cli restore-layout`
* First-class agent harness panes for Claude, Codex, Gemini, and OpenCode
* Live agent progress with `wakterm cli agent watch` and
  `wakterm cli agent list -f`
* Prompt send and interrupt flows that keep the real harness UI in the pane
* Better multi-client mux behavior, including fewer flickers, redraw storms,
  and resize feedback loops
* More reliable split and spawn sizing so panes agree on the same layout more
  often across clients
* User-set tab titles are preserved instead of being overwritten by terminal
  escape sequences

## Core Terminal Features

* Runs on Linux, macOS, Windows 10, FreeBSD and NetBSD
* [Multiplex terminal panes, tabs and windows on local and remote hosts, with native mouse and scrollback](multiplexing.md)
* Tabs, panes, and multiple windows, with native keyboard-driven navigation
* [SSH client with native tabs](ssh.md)
* [Connect to serial ports for embedded and Arduino work](serial.md)
* Connect to a local multiplexer server over unix domain sockets
* Connect to a remote multiplexer using SSH or TLS over TCP/IP
* [Searchable Scrollback](scrollback.md) with keyboard navigation and search mode
* Hyperlinks, shell integration, and dynamic status areas
* <a href="https://github.com/tonsky/FiraCode#fira-code-monospaced-font-with-programming-ligatures">Ligatures</a>, Color Emoji, font fallback, and true color with [dynamic color schemes](config/appearance.md)
* Configuration via a [configuration file](config/files.md) with hot reloading
* iTerm2-compatible image protocol support and built-in [imgcat command](imgcat.md)
* Kitty graphics support
* Sixel graphics support (experimental: starting in `20200620-160318-e00b076c`)

<video width="80%" controls src="screenshots/wakterm-tabs.mp4" loop></video>
