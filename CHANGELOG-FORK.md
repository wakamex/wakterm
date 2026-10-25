# Changelog - wakamex/wakterm fork

All changes relative to upstream wakterm/wakterm main at 05343b387.

## Changes from 2026-10-16 through 2026-10-25 (commits bdcaca827 through d70428695)

### Agent Harnesses and Agent API

- Devin as an agent harness: detection, exact binding to the session its process holds a lock on, busy and idle from its session database, `turn_started`, `input_accepted`, assistant and `turn_final` events (a message sent mid-turn is `input_accepted` on that turn), send admission from its input box, exact restore with `devin --resume`, `wakterm agent start devin`, and the database read from its `devin acp` worker's environment, with `DEVIN_SESSIONS_DIR` and `XDG_DATA_HOME` before the worker starts ([a4fbc9b](https://github.com/wakamex/wakterm/commit/a4fbc9b97), [c2c8cd5](https://github.com/wakamex/wakterm/commit/c2c8cd556), [ec21d47](https://github.com/wakamex/wakterm/commit/ec21d4730), [0c39f38](https://github.com/wakamex/wakterm/commit/0c39f38ee), [d21d6fe](https://github.com/wakamex/wakterm/commit/d21d6fef8))
- Devin permission prompts as `devin_permission` approvals (capability `devin_permission.v1`) with `allow_once` and `deny`, answered by keys checked against the screen; interrupted, denied and errored Devin turns end instead of staying busy ([575ea96](https://github.com/wakamex/wakterm/commit/575ea966e), [ba4a1ef](https://github.com/wakamex/wakterm/commit/ba4a1ef13), [f669795](https://github.com/wakamex/wakterm/commit/f669795a5))
- `agent send` acknowledges a send only by the agent's recorded input for harnesses that record it, reading the event stream after its oldest events were dropped, and reports a steer agy's input box took back as an `input_blocked` refusal ([666d131](https://github.com/wakamex/wakterm/commit/666d13139), [f85a060](https://github.com/wakamex/wakterm/commit/f85a060ef))
- agy is treated as idle only while its footer shows "? for shortcuts", since a queued message or other hint replaces "esc to cancel" mid-turn; a turn agy stops on an on-screen error or its feedback survey is reported waiting, and a send skips the survey with its Skip choice ([3a1ea62](https://github.com/wakamex/wakterm/commit/3a1ea620b), [c150032](https://github.com/wakamex/wakterm/commit/c15003268), [3382dcd](https://github.com/wakamex/wakterm/commit/3382dcdc1), [9354ec3](https://github.com/wakamex/wakterm/commit/9354ec380), [f2e36d1](https://github.com/wakamex/wakterm/commit/f2e36d1ee))
- Claude Code, Codex, Gemini CLI and OpenCode sessions are found under the home or data directory the harness process's own environment names (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GEMINI_CLI_HOME`, `XDG_DATA_HOME`) ([86ed147](https://github.com/wakamex/wakterm/commit/86ed147ca), [7a81015](https://github.com/wakamex/wakterm/commit/7a81015fc))
- `wakterm agent caller` reports the caller's tab, with the effective title Panetone routes by ([c9f1bb5](https://github.com/wakamex/wakterm/commit/c9f1bb537))
- Codex: a steer left in the composer when its turn ended is submitted as the next turn, and the TUI proxy relays app-server messages up to the TUI client's 128 MiB limit and forwards larger ones uninspected instead of closing, which had put every Codex TUI into a reconnect loop on a 17.7 MB `plugin/list` response ([9185061](https://github.com/wakamex/wakterm/commit/91850619b), [6f3a63a](https://github.com/wakamex/wakterm/commit/6f3a63a92), [46c2e7a](https://github.com/wakamex/wakterm/commit/46c2e7af7))
- Claude: confirming a `/clear` from the new session file, choosing a menu row by its number, not reporting history copied into a new session, and following a parked job's status and a window's own conversation after its job finished ([3062035](https://github.com/wakamex/wakterm/commit/306203568), [00cb71a](https://github.com/wakamex/wakterm/commit/00cb71aac), [bd8a339](https://github.com/wakamex/wakterm/commit/bd8a339f1), [627f35c](https://github.com/wakamex/wakterm/commit/627f35c37), [4c9d057](https://github.com/wakamex/wakterm/commit/4c9d0576e), [64b7efa](https://github.com/wakamex/wakterm/commit/64b7efa68))
- OpenCode replies are reported once their streamed text part is finished ([980f50a](https://github.com/wakamex/wakterm/commit/980f50acf))
- Restore: an agent the Wakterm CLI launched is restored by its own resume rather than refused as supervised, and a restore whose candidate describes a replaced process stays pending instead of failing, which had re-adopted agents under new IDs ([95fc6b9](https://github.com/wakamex/wakterm/commit/95fc6b9f1), [2f543be](https://github.com/wakamex/wakterm/commit/2f543bef3))

### Tabs and Tab Navigator

- `ActivateOldestTabNeedingAttention` reviews the attention queue from its other end, and `ActivateNextTabNeedingAttention` skips hidden tabs ([083f22e](https://github.com/wakamex/wakterm/commit/083f22e8a), [434610d](https://github.com/wakamex/wakterm/commit/434610d3b), [74b9e1b](https://github.com/wakamex/wakterm/commit/74b9e1bbb))
- Closing a tab passes focus to its neighbor in activity order, and a working agy tab keeps its place in the activity order ([4362110](https://github.com/wakamex/wakterm/commit/436211055), [1feccb7](https://github.com/wakamex/wakterm/commit/1feccb767))
- The working marker blinks, and shows only where a dimmed tab does not already say it ([99a55a3](https://github.com/wakamex/wakterm/commit/99a55a3bb), [ce70a76](https://github.com/wakamex/wakterm/commit/ce70a764f))
- A Devin tab icon, traced from Devin's official mark; stacked harness icons grow to seven ([3db8d82](https://github.com/wakamex/wakterm/commit/3db8d82cb))

### Fonts, Multiplexer, and Platform Portability

- A deadlock between installing an agent and reading a tab badge, which hung the mux server after a restart, is gone ([7c9f73d](https://github.com/wakamex/wakterm/commit/7c9f73db4))
- A second mux server is refused on the same saved session ([d291c0a](https://github.com/wakamex/wakterm/commit/d291c0a56))
- Upstream wezterm fixes and selected enhancements through 86d223ec8 ([b97adbf](https://github.com/wakamex/wakterm/commit/b97adbf68))

### Docs, Packaging, and Tooling

- v0.1.0 release, with bundled third-party crate and Windows licenses and the LGPL xcb-imdkit source ([031c14b](https://github.com/wakamex/wakterm/commit/031c14b7f), [b6215ba](https://github.com/wakamex/wakterm/commit/b6215ba19), [a2a46b7](https://github.com/wakamex/wakterm/commit/a2a46b794), [2aa174b](https://github.com/wakamex/wakterm/commit/2aa174b3d), [e2ce5fc](https://github.com/wakamex/wakterm/commit/e2ce5fce8))
- Windows `OSTYPE` matching in `deploy.sh`, and no tar SIGPIPE in `validation.yml` ([ba51b74](https://github.com/wakamex/wakterm/commit/ba51b7443))
- Test fixes for a spawn race in a Codex observer test and for pane IDs taken by concurrent tests ([d05a082](https://github.com/wakamex/wakterm/commit/d05a08251), [d704286](https://github.com/wakamex/wakterm/commit/d70428695))
- The periodic changelog and docs sweep is documented in AGENTS.md ([bdcaca8](https://github.com/wakamex/wakterm/commit/bdcaca827), [e4fcbae](https://github.com/wakamex/wakterm/commit/e4fcbae1a))

## Changes from 2026-09-27 through 2026-10-16 (commits 3d0a6cdb8 through bfe9f4e72)

### Agent Harnesses and Agent API

- Idle Claude agent freezing (`agent_idle_freeze`) on Linux and Windows (`NtSuspendProcess` and working-set trim), started once the GUI is running, with Windows session-status reading ([3e6b477](https://github.com/wakamex/wakterm/commit/3e6b477f2), [03a187d](https://github.com/wakamex/wakterm/commit/03a187d64), [ebf8c9d](https://github.com/wakamex/wakterm/commit/ebf8c9dce), [75aa077](https://github.com/wakamex/wakterm/commit/75aa077b7)).
- Mux-owned agent reminders (`wakterm agent remind add|list|cancel`) that persist across restarts and deliver one-shot or repeating messages to agents at a scheduled time or after a delay, waking frozen agents ([94cc26e](https://github.com/wakamex/wakterm/commit/94cc26ecf), [75aa077](https://github.com/wakamex/wakterm/commit/75aa077b7)).
- Exact interactive approvals and question forms (`wakterm agent approval`): managed Codex approvals and questions, Claude single-question and multi-question forms (`user_question` and `user_question_form`), Claude `Session paused` safeguard menus, and general Claude confirmation menus ([dfe0b21](https://github.com/wakamex/wakterm/commit/dfe0b210d), [e022a74](https://github.com/wakamex/wakterm/commit/e022a74b6), [ba4aff4](https://github.com/wakamex/wakterm/commit/ba4aff4fb), [56dea77](https://github.com/wakamex/wakterm/commit/56dea7713), [e8051b2](https://github.com/wakamex/wakterm/commit/e8051b203), [ea23a4d](https://github.com/wakamex/wakterm/commit/ea23a4dcd), [8f354f2](https://github.com/wakamex/wakterm/commit/8f354f276)).
- Return-final support (`--return-final`) for Claude and Agy agents, observer-backed return correlation from provider events, `input_sha256` recorded on `turn_started` and encoded across Agent API events (with tabs hashed as four spaces as Claude records them), and skipping finals from turns that ended before a return request existed ([f09987f](https://github.com/wakamex/wakterm/commit/f09987f15), [20d3ec1](https://github.com/wakamex/wakterm/commit/20d3ec1f5), [e26a0ac](https://github.com/wakamex/wakterm/commit/e26a0ac27), [60ccd4c](https://github.com/wakamex/wakterm/commit/60ccd4c5b), [594e35b](https://github.com/wakamex/wakterm/commit/594e35b77), [12a9c41](https://github.com/wakamex/wakterm/commit/12a9c4152), [e9b1a1a](https://github.com/wakamex/wakterm/commit/e9b1a1a54), [2fbf7b3](https://github.com/wakamex/wakterm/commit/2fbf7b331)).
- Draft-safe and keyboard-safe prompt submission: refuse sends when a Claude or Codex input box holds an active draft or does not have focus, automatically stash an idle Claude draft (using the user's configured stash keybinding) before sending, hold prompts until Claude finishes starting, wait for the typed prompt to be drawn before pressing Enter, and report blocked sends as structured refusals naming the blocking input ([1ec92f9](https://github.com/wakamex/wakterm/commit/1ec92f99b), [71a9135](https://github.com/wakamex/wakterm/commit/71a9135c9), [05c0c3c](https://github.com/wakamex/wakterm/commit/05c0c3c15), [cbc4698](https://github.com/wakamex/wakterm/commit/cbc4698a4), [f510fa8](https://github.com/wakamex/wakterm/commit/f510fa8dc), [ca51bee](https://github.com/wakamex/wakterm/commit/ca51bee96), [9db2dc4](https://github.com/wakamex/wakterm/commit/9db2dc4d1), [00e7b40](https://github.com/wakamex/wakterm/commit/00e7b4055), [fc60f3a](https://github.com/wakamex/wakterm/commit/fc60f3aa0), [74544e7](https://github.com/wakamex/wakterm/commit/74544e791), [6193c7a](https://github.com/wakamex/wakterm/commit/6193c7a89), [d770451](https://github.com/wakamex/wakterm/commit/d77045130), [e821922](https://github.com/wakamex/wakterm/commit/e82192222)).
- Prompt confirmation and turn tracking: confirm sends only once the provider records the exact input or starts working on it, acknowledge queued Claude input, slash commands, background-shell input, and managed Codex steers, and keep Claude turns open across tool runs, skipped records, and conversation compaction while closing turns stopped by provider errors or idle transitions ([3c23896](https://github.com/wakamex/wakterm/commit/3c238964a), [79e9261](https://github.com/wakamex/wakterm/commit/79e9261d6), [682a13a](https://github.com/wakamex/wakterm/commit/682a13aaf), [637249b](https://github.com/wakamex/wakterm/commit/637249ba1), [e3b8961](https://github.com/wakamex/wakterm/commit/e3b896118), [18d5053](https://github.com/wakamex/wakterm/commit/18d5053e3), [1c84b59](https://github.com/wakamex/wakterm/commit/1c84b5952), [2ba419e](https://github.com/wakamex/wakterm/commit/2ba419ea1), [b6416c5](https://github.com/wakamex/wakterm/commit/b6416c557), [324cf69](https://github.com/wakamex/wakterm/commit/324cf6909), [a6635ed](https://github.com/wakamex/wakterm/commit/a6635ed71), [cb7eb48](https://github.com/wakamex/wakterm/commit/cb7eb48fa), [56ed2fc](https://github.com/wakamex/wakterm/commit/56ed2fccf), [658adba](https://github.com/wakamex/wakterm/commit/658adbaa3), [f7be4c1](https://github.com/wakamex/wakterm/commit/f7be4c10a), [3bd93f5](https://github.com/wakamex/wakterm/commit/3bd93f5b3), [4ab57e3](https://github.com/wakamex/wakterm/commit/4ab57e3cb), [c8bf849](https://github.com/wakamex/wakterm/commit/c8bf84938)).
- Calling-agent resolution (`wakterm agent caller`) for managed Codex tool commands and explicit caller inputs ([bb3ea77](https://github.com/wakamex/wakterm/commit/bb3ea773f), [90b7578](https://github.com/wakamex/wakterm/commit/90b75783f)).
- Agent list and event consumer ergonomics: filter `wakterm agent list` by status, show last turn as an age, let Agent API consumers skip unknown event kinds, and retry opening the SQLite event store when busy ([e0bce8f](https://github.com/wakamex/wakterm/commit/e0bce8f31), [78c0fc0](https://github.com/wakamex/wakterm/commit/78c0fc0b6), [9587493](https://github.com/wakamex/wakterm/commit/958749346), [d5f8d55](https://github.com/wakamex/wakterm/commit/d5f8d5521)).
- Observation and discovery: release unconfirmed managed Codex bindings to other harnesses, observe manually launched remote Codex TUIs through their resumed thread, find Codex rollouts held by child processes of launcher threads, explain Codex TUIs running in Codex's background daemon, read Claude transcripts from the end with per-record rules, find Claude transcripts for working directories with dots or underscores, ignore trailing slashes in declared working directories, and skip invalid JSON lines ([2d0dfdd](https://github.com/wakamex/wakterm/commit/2d0dfdd8b), [a25a5d4](https://github.com/wakamex/wakterm/commit/a25a5d4b0), [db2fb80](https://github.com/wakamex/wakterm/commit/db2fb8010), [9acb05c](https://github.com/wakamex/wakterm/commit/9acb05c91), [07f7b73](https://github.com/wakamex/wakterm/commit/07f7b730f), [db21bec](https://github.com/wakamex/wakterm/commit/db21bec2f), [39dca5e](https://github.com/wakamex/wakterm/commit/39dca5e3d), [f5c9bf8](https://github.com/wakamex/wakterm/commit/f5c9bf813), [431300a](https://github.com/wakamex/wakterm/commit/431300ac0), [6adae03](https://github.com/wakamex/wakterm/commit/6adae032f), [0f5d30f](https://github.com/wakamex/wakterm/commit/0f5d30f2e), [84605fb](https://github.com/wakamex/wakterm/commit/84605fb51), [f4c6aff](https://github.com/wakamex/wakterm/commit/f4c6aff33)).
- Agent restoration and deploy recovery: notify the user when an agent restore fails, record Claude folder trust before resuming, keep a resumed agent's seen-reply state across mux restarts, focus the first pane of each restored tab, save the agent list and catalog before `deploy.sh` restarts the mux, and add `deploy.sh --notify` to resume interrupted agents and ask the repo's agent to verify the deploy ([d1f3544](https://github.com/wakamex/wakterm/commit/d1f354459), [e07bd08](https://github.com/wakamex/wakterm/commit/e07bd084d), [1a4314b](https://github.com/wakamex/wakterm/commit/1a4314b09), [46bfe6e](https://github.com/wakamex/wakterm/commit/46bfe6ec7), [e3b659a](https://github.com/wakamex/wakterm/commit/e3b659a10), [0544ea5](https://github.com/wakamex/wakterm/commit/0544ea5b0), [7d12edc](https://github.com/wakamex/wakterm/commit/7d12edc9d)).

### Tabs and Tab Navigator

- Live activity-order tab sorting (`ToggleTabActivityOrder`, `Ctrl-Shift-B`) that orders tabs by most recent agent turn completion on the client while keeping the server's tab order authoritative ([989f1b9](https://github.com/wakamex/wakterm/commit/989f1b9c9), [064d1fe](https://github.com/wakamex/wakterm/commit/064d1feb2), [668aee0](https://github.com/wakamex/wakterm/commit/668aee0e0)).
- Overflowing fancy tab titles shrink to a shared measured width, fade smoothly by color, and narrow down to their harness icon when space is tight (`tab_titles_shrink_to_fit`) ([796cd6c](https://github.com/wakamex/wakterm/commit/796cd6ccd), [c06dd65](https://github.com/wakamex/wakterm/commit/c06dd6518), [254258c](https://github.com/wakamex/wakterm/commit/254258c05)).
- Working agent tabs dim with a blinking activity marker, and tabs whose latest agent reply was already reviewed can optionally dim (`agent_tab_dim_reviewed`) ([4f27a27](https://github.com/wakamex/wakterm/commit/4f27a27d8), [bfe9f4e](https://github.com/wakamex/wakterm/commit/bfe9f4e72)).
- Switch tabs with `Alt-1..9`, open the tab navigator with `Alt-E` (`alt_tab_shortcuts`), and hide or show the selected tab with `Ctrl-H` inside the tab navigator ([8a78423](https://github.com/wakamex/wakterm/commit/8a7842349), [2b8a200](https://github.com/wakamex/wakterm/commit/2b8a20079)).

### Fonts, Multiplexer, and Platform Portability

- Load WOFF and WOFF2 fonts directly, and bundle JetBrains Mono 2.304, Roboto 3.016, Noto Color Emoji 2.057, and Symbols Nerd Font Mono 3.5.1 as WOFF2 with updated glyph names and licenses ([521f39d](https://github.com/wakamex/wakterm/commit/521f39d5a), [fae0e02](https://github.com/wakamex/wakterm/commit/fae0e028b), [ea54f3c](https://github.com/wakamex/wakterm/commit/ea54f3c85), [36b0e01](https://github.com/wakamex/wakterm/commit/36b0e019f)).
- Load configuration without live reload when the OS has no file watcher handles left, instead of failing startup ([19a8801](https://github.com/wakamex/wakterm/commit/19a880164)).
- Exit quietly when the CLI output reader closes, detect closed pipe readers on macOS and Linux (`POLLOUT`), and identify Codex output files by volume and file index on Windows ([981476a](https://github.com/wakamex/wakterm/commit/981476ad4), [eaef9ad](https://github.com/wakamex/wakterm/commit/eaef9ad6f), [dc92e22](https://github.com/wakamex/wakterm/commit/dc92e2290), [511c4b1](https://github.com/wakamex/wakterm/commit/511c4b1fb)).
- Cross-platform mux test portability across Linux, macOS, and Windows ([c762814](https://github.com/wakamex/wakterm/commit/c762814bf), [6397542](https://github.com/wakamex/wakterm/commit/63975420d), [42baad2](https://github.com/wakamex/wakterm/commit/42baad248), [7db4131](https://github.com/wakamex/wakterm/commit/7db413127), [b0f1102](https://github.com/wakamex/wakterm/commit/b0f110226), [24f12f8](https://github.com/wakamex/wakterm/commit/24f12f8e7), [bba7235](https://github.com/wakamex/wakterm/commit/bba72352b), [78146f0](https://github.com/wakamex/wakterm/commit/78146f0ea), [14739d2](https://github.com/wakamex/wakterm/commit/14739d293), [8724c9d](https://github.com/wakamex/wakterm/commit/8724c9d2f), [9e702c5](https://github.com/wakamex/wakterm/commit/9e702c57d)).

### Docs, Packaging, and Tooling

- GUI startup smoke check (`ci/gui-smoke.sh`, `make smoke`, and `deploy.sh`) run under Xvfb in CI ([9b22588](https://github.com/wakamex/wakterm/commit/9b225886c)).
- Release policy alignment: `VERSION` and `RELEASES.md`, validated release workflows (`validation.yml`, `release-eligibility.yml`, `publish.yml`) with pinned runners and action SHAs, standalone Fedora `.rpm` packaging, Homebrew tap (`wakamex/tap`) and Scoop bucket (`wakamex/scoop-bucket`) templates and sync script, bundled licenses in all archives, and trimmed installation docs ([0079c7a](https://github.com/wakamex/wakterm/commit/0079c7ae2), [2a5899f](https://github.com/wakamex/wakterm/commit/2a5899fc0), [dcc023b](https://github.com/wakamex/wakterm/commit/dcc023baf)).
- Regenerated CLI help and shell completions, and documented derived-file regeneration, lowercase default shortcut rules, and field-stable mux codec encoding in `AGENTS.md` ([1deaa80](https://github.com/wakamex/wakterm/commit/1deaa801a), [e6a64be](https://github.com/wakamex/wakterm/commit/e6a64be8b), [259ee63](https://github.com/wakamex/wakterm/commit/259ee6381), [3c9329d](https://github.com/wakamex/wakterm/commit/3c9329de4)).

## Changes from 2026-08-23 through 2026-09-27 (commits 6f9043880 through 09a30ab99)

### Agent Harnesses and Agent API

- ZCode (Z.ai's coding CLI) as a harness: detection, status and events from its OpenCode-schema session database, exact restore with `zcode --resume`, `wakterm agent start zcode`, and a tab icon traced from the official ZCode mark ([d7751d4](https://github.com/wakamex/wakterm/commit/d7751d483), [d757cbd](https://github.com/wakamex/wakterm/commit/d757cbd63)).
- OpenCode-schema event projection skips injected runtime messages instead of opening phantom turns, and reports provider failures such as rate limits once per turn ([d7751d4](https://github.com/wakamex/wakterm/commit/d7751d483)).
- Claude panes that show a background job, either the window that sent it there or `claude attach`, are observed through the job's live worker; the pane reports the job with a hint for moving the conversation back, and a clear error when the worker is gone ([c1005ec](https://github.com/wakamex/wakterm/commit/c1005ecbb), [2fe9827](https://github.com/wakamex/wakterm/commit/2fe9827e7), [df7b155](https://github.com/wakamex/wakterm/commit/df7b155d0)).
- Exact Claude restoration through shared agent recovery, keeping the values of options such as `--append-system-prompt-file` and `--system-prompt-snapshot` ([fcc15c5](https://github.com/wakamex/wakterm/commit/fcc15c5ee), [2eea046](https://github.com/wakamex/wakterm/commit/2eea046f9)).
- Exact Agy conversation restoration ([8b34ef2](https://github.com/wakamex/wakterm/commit/8b34ef2da), [94acdec](https://github.com/wakamex/wakterm/commit/94acdeca5)).
- Foreground harnesses running under a sandbox supervisor are observed and bound by process incarnation, and restoration keeps the supervised session instead of launching the inner harness unconfined ([638ca88](https://github.com/wakamex/wakterm/commit/638ca88ff), [efedff3](https://github.com/wakamex/wakterm/commit/efedff3ea)).
- Managed Codex:
  - Restore through the current app server, promote connected sessions to managed restore, and preserve managed identity and last-turn timestamps ([57a2e8b](https://github.com/wakamex/wakterm/commit/57a2e8b2c), [e9999e8](https://github.com/wakamex/wakterm/commit/e9999e8d6), [329358b](https://github.com/wakamex/wakterm/commit/329358bb3), [44b5180](https://github.com/wakamex/wakterm/commit/44b518036), [c033720](https://github.com/wakamex/wakterm/commit/c03372022), [47d2e3e](https://github.com/wakamex/wakterm/commit/47d2e3e44)).
  - Follow thread changes, native forks, and TUI replacement within a pane; make launch readiness atomic ([7941d17](https://github.com/wakamex/wakterm/commit/7941d17ab), [14d51ca](https://github.com/wakamex/wakterm/commit/14d51ca5f), [8aa0bda](https://github.com/wakamex/wakterm/commit/8aa0bda30), [afff1fe](https://github.com/wakamex/wakterm/commit/afff1febe)).
  - Model and reasoning-effort overrides, options kept across argument positions, and permission flags kept off remote resume, which current Codex rejects ([720002d](https://github.com/wakamex/wakterm/commit/720002d40), [39bef1f](https://github.com/wakamex/wakterm/commit/39bef1f47), [60d8548](https://github.com/wakamex/wakterm/commit/60d8548d1), [541ac25](https://github.com/wakamex/wakterm/commit/541ac25e5)).
  - `--return-final` support, a resume picker scoped to the pane's working directory, and bounded resume responses ([9f266b7](https://github.com/wakamex/wakterm/commit/9f266b7b8), [d19b9cf](https://github.com/wakamex/wakterm/commit/d19b9cf23), [160734b](https://github.com/wakamex/wakterm/commit/160734bdd)).
  - Failed turns expose safe terminal details and distinguish model capacity from connection failures; terminal turn state survives delayed activity ([5df52d3](https://github.com/wakamex/wakterm/commit/5df52d379), [1dd8d46](https://github.com/wakamex/wakterm/commit/1dd8d465f), [f6279a6](https://github.com/wakamex/wakterm/commit/f6279a668), [e5ec1b1](https://github.com/wakamex/wakterm/commit/e5ec1b15a)).
  - Internal worker sessions and ephemeral threads stay out of pane identity; rollouts are discovered from process descriptors ([2206725](https://github.com/wakamex/wakterm/commit/2206725f6), [df1b673](https://github.com/wakamex/wakterm/commit/df1b67383), [6265dcf](https://github.com/wakamex/wakterm/commit/6265dcfee)).
  - App-server events are persisted, bound replies are recovered, and oversized provider records are skipped ([360d7be](https://github.com/wakamex/wakterm/commit/360d7be1c), [b35c94f](https://github.com/wakamex/wakterm/commit/b35c94f50), [e292fa0](https://github.com/wakamex/wakterm/commit/e292fa047), [c28ee97](https://github.com/wakamex/wakterm/commit/c28ee9725)).
- Agent status updates are driven by pane output and provider artifacts instead of polling, with artifact events routed by exact session, bounded watcher state, lossless observer batches, and owed trailing refreshes ([e7b57f8](https://github.com/wakamex/wakterm/commit/e7b57f826), [f804bd9](https://github.com/wakamex/wakterm/commit/f804bd98c), [828cd0a](https://github.com/wakamex/wakterm/commit/828cd0acf), [6583a2e](https://github.com/wakamex/wakterm/commit/6583a2e85), [1a31165](https://github.com/wakamex/wakterm/commit/1a3116579), [2dead1c](https://github.com/wakamex/wakterm/commit/2dead1c9d), [d363c22](https://github.com/wakamex/wakterm/commit/d363c22ef)).
- Agent event and catalog consistency: single-snapshot event pages, blocking idle reads, reused output workers, unique catalog identities, catalog aligned with durable turns, primed cursors for adopted sessions, and return correlation kept through steering ([3fbd2e7](https://github.com/wakamex/wakterm/commit/3fbd2e765), [d3521ca](https://github.com/wakamex/wakterm/commit/d3521ca2e), [a188c57](https://github.com/wakamex/wakterm/commit/a188c57bb), [4a00853](https://github.com/wakamex/wakterm/commit/4a00853f0), [3ccdb7e](https://github.com/wakamex/wakterm/commit/3ccdb7e6a), [eb90e52](https://github.com/wakamex/wakterm/commit/eb90e521b), [24ae141](https://github.com/wakamex/wakterm/commit/24ae14191), [f98775d](https://github.com/wakamex/wakterm/commit/f98775d41), [fac0ead](https://github.com/wakamex/wakterm/commit/fac0eade0), [a150a30](https://github.com/wakamex/wakterm/commit/a150a3082)).

### Shell Integration

- Local bash, zsh, fish, and Wsh shells load Wakterm's shell integration automatically without changes to their startup files; `shell_integration = false` turns it off ([2f27987](https://github.com/wakamex/wakterm/commit/2f279879d)).
- Each pane has a durable `WAKTERM_PANE_TOKEN` that survives mux restarts ([84190a6](https://github.com/wakamex/wakterm/commit/84190a623)).
- Pane-local shell history: Up recalls the pane's own commands first, and commands merge into the shared history on exit, including those of a killed shell ([902d052](https://github.com/wakamex/wakterm/commit/902d052c8), [d40862b](https://github.com/wakamex/wakterm/commit/d40862b83)).
- Wsh owns OSC 7, OSC 133, pane history, and restored foreground jobs natively where it implements them ([6f4377e](https://github.com/wakamex/wakterm/commit/6f4377e95), [e33307f](https://github.com/wakamex/wakterm/commit/e33307f0b), [4147a3d](https://github.com/wakamex/wakterm/commit/4147a3dd8)).

### Tabs and Tab Navigator

- Parked tabs are presented as hidden tabs ([75f37a4](https://github.com/wakamex/wakterm/commit/75f37a473)).
- Tab navigator: filter by tab name, select the best search match, clearer agent indicators, and mux-scoped Codex memory totals ([bb74a62](https://github.com/wakamex/wakterm/commit/bb74a6290), [afa6811](https://github.com/wakamex/wakterm/commit/afa68110d), [174984d](https://github.com/wakamex/wakterm/commit/174984d2a), [43bf07d](https://github.com/wakamex/wakterm/commit/43bf07da4)).
- `ToggleTabBarPosition` (`Ctrl-Shift-A`) moves the tab bar between the top and bottom of the window ([8639ea0](https://github.com/wakamex/wakterm/commit/8639ea023)).
- Automatic tab titles persist across nested shells ([a133db1](https://github.com/wakamex/wakterm/commit/a133db191)).
- Exhaustive generated tab color palettes ([d6d0c16](https://github.com/wakamex/wakterm/commit/d6d0c1661)).

### Multiplexer Security and Reliability

- On Linux, clients from a different user, mount, PID, or IPC namespace get metadata-only mux access ([97932a0](https://github.com/wakamex/wakterm/commit/97932a068), [033ec8b](https://github.com/wakamex/wakterm/commit/033ec8b8e), [d558c19](https://github.com/wakamex/wakterm/commit/d558c192c)).
- Crash-safe session recovery: restorable panes stay in the layout when their processes exit, and snapshots are written atomically with one previous generation retained ([def8bad](https://github.com/wakamex/wakterm/commit/def8bad78)).
- Remote clients no longer loop on resize echoes and replace agent status atomically across reconciliation ([c132108](https://github.com/wakamex/wakterm/commit/c13210850), [934fe55](https://github.com/wakamex/wakterm/commit/934fe559e), [8346f0f](https://github.com/wakamex/wakterm/commit/8346f0f74)).
- App-server terminal state is preserved, and the user service includes user binaries in `PATH` ([d6e8bfb](https://github.com/wakamex/wakterm/commit/d6e8bfb92), [2b6e6af](https://github.com/wakamex/wakterm/commit/2b6e6af42)).

### Performance and Benchmarks

- Repeatable 20-workspace benchmark: restoring 20 tabs and 27 PTYs takes 149 ms and holds 34.44 MiB PSS at 0.0016% of one core ([5b35480](https://github.com/wakamex/wakterm/commit/5b35480b6)).
- Repeatable agent infrastructure workloads and tab navigator resource-sampling profiles; cached process-tree lookups fell from 1.321 s to 7.33 ms for 200 lookups ([0746c2d](https://github.com/wakamex/wakterm/commit/0746c2db8), [3510ac0](https://github.com/wakamex/wakterm/commit/3510ac004), [e7b57f8](https://github.com/wakamex/wakterm/commit/e7b57f826)).

### Docs, Packaging, and Tooling

- Documentation moved from MkDocs to Zola, with a mobile drawer, route-driven navigation, a full-width layout, and a separate configuration reference ([7b5b447](https://github.com/wakamex/wakterm/commit/7b5b44756), [b477fee](https://github.com/wakamex/wakterm/commit/b477feedb), [6ccece7](https://github.com/wakamex/wakterm/commit/6ccece753), [72f9e55](https://github.com/wakamex/wakterm/commit/72f9e5543), [d81ece3](https://github.com/wakamex/wakterm/commit/d81ece359), [6feaab8](https://github.com/wakamex/wakterm/commit/6feaab8c7)).
- Default key documentation is generated from a versioned `wakterm show-keys` JSON catalog ([ac5d20a](https://github.com/wakamex/wakterm/commit/ac5d20a9f)).
- Public documentation audit and hardened derived generation ([6f90438](https://github.com/wakamex/wakterm/commit/6f9043880), [78d6b8c](https://github.com/wakamex/wakterm/commit/78d6b8cbb)).
- Regenerated app icons, an optional Linux desktop launcher in `install.sh`, and cold Nix CI builds fixed ([8cafe2a](https://github.com/wakamex/wakterm/commit/8cafe2a03), [956f9f2](https://github.com/wakamex/wakterm/commit/956f9f217), [e305c98](https://github.com/wakamex/wakterm/commit/e305c9884), [feafa8c](https://github.com/wakamex/wakterm/commit/feafa8c8d)).
- Tab icon review sheets render transparently at full resolution, and the icon tracing steps are documented ([c7e8f00](https://github.com/wakamex/wakterm/commit/c7e8f000e)).

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
