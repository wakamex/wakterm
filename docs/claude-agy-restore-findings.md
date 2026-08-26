# Claude and Agy restart failures on Fedora

## Failures observed before the fixes

The mux restart on 2026-08-26 exposed two separate gaps. The service PATH and
Agy restore changes described below were then implemented as separate commits.

| Harness | Saved exact session | Restore command available | Failure |
| --- | --- | --- | --- |
| Claude 2.1.246 | Yes | Yes | The mux service cannot find `claude` in its `PATH` |
| Agy 1.1.21 | No | Not implemented in Wakterm | The pane is recreated as a generic shell instead of resuming Agy |

Claude displayed:

```text
Unable to spawn claude because:
No viable candidates found in PATH "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin"
```

The attempted command contained the expected session UUID:

```text
claude --dangerously-skip-permissions --add-dir /home/mihai --add-dir /code --add-dir .git --resume 7cba9828-8b08-4544-ba86-95ebb0efc866
```

The Agy pane was empty after restoration.

## Claude executable lookup

The installed user unit runs the mux directly and does not set `PATH`:

```ini
ExecStart=/home/mihai/.local/bin/wakterm-mux-server
EnvironmentFile=-/home/mihai/.config/wakterm-mux-server.env
```

The environment file also has no `PATH` override. The resulting service path is:

```text
/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin
```

Both harness launchers are outside it:

```text
/home/mihai/.local/bin/claude
/home/mihai/.local/bin/agy
```

Reproducing command lookup with the service path fails for both names. Prepending
`/home/mihai/.local/bin` makes both resolve.

Wakterm did persist the Claude restore intent. The current session file contains
the normalized launch arguments, harness `Claude`, and exact session UUID. The
native restore builder correctly removes prior session selectors, appends
`--resume <uuid>`, and removes `CLAUDECODE`. The failure happens later when the
PTY command builder resolves the relative executable name against the inherited
service `PATH`.

The live-process normalization also explains why an interactive launch worked
before restart. It converts an alias such as `cl` into the concrete process argv,
but that argv begins with `claude`, not an absolute executable path. Alias
normalization therefore preserves the provider arguments but does not make the
executable durable across a service environment boundary.

### Claude fix direction

The smallest system-level repair is to give the user service a path that includes
the normal user executable directory. The repository already provides
`~/.config/wakterm-mux-server.env` for service overrides, so the immediate local
workaround is:

```text
PATH=/home/mihai/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin
```

The package fix is a deterministic user-service path containing the standard user
binary directory and system binary directories:

```ini
Environment="PATH=%h/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin"
```

The environment file is loaded after that default, so users with launchers under
NVM, mise, or another custom prefix can override `PATH` without changing the unit.
Wakterm uses the current launcher at restore time so an upgraded provider is
picked up, rather than pinning a resolved version target or binary hash. Running a
login shell to recover aliases would add shell startup behavior and is not needed.

Required regression coverage:

- A normalized Claude launch whose executable is in the user binary directory
  resumes the exact UUID under the service environment.
- Repointing a stable launcher symlink before restart uses the current provider
  version.
- A genuinely missing executable keeps the restore intent and presents the exact
  launch failure for retry.

## Agy restore support

The Agy binary already exposes an exact native resume selector:

```text
--conversation  Resume a previous conversation by ID
```

Wakterm can already identify the exact live Agy conversation on Linux. The Agy
observer matches the process PID and start time to an open presence lock, then
maps the lock UUID to:

```text
~/.gemini/antigravity-cli/brain/<conversation-id>/.system_generated/logs/transcript.jsonl
```

That evidence is sufficient to derive the stable conversation UUID while the
process is alive. At the failing revision, it was used for observation but not
restoration.

At the failing revision, three gates excluded Agy from recovery:

- `agent_restore_intent_for_pane` returned an intent only for Claude and Codex.
- `restorable_session_id` extracted IDs only for Claude and Codex.
- `native_resume_command` and `register_agent_restore_intent` accepted only Claude
  and Codex.

The saved session captured after that restart contained 20 Codex intents and one Claude intent,
but no Agy intent. Without an intent, layout restoration starts the default
program for that pane. Wakterm has no expected harness or provider session to
retry or report, which accounts for the empty Agy pane.

The Agy pane was detected rather than durably registered before the observed
restart. Detection alone is intentionally not durable. At that revision, even an
adopted Agy pane hit the implementation gates above, so adoption by itself did
not solve the recovery gap.

### Implemented Agy recovery

The implementation extends the existing native restore boundary instead of
adding an Agy-specific recovery subsystem:

1. Extract the conversation UUID from the confirmed transcript path.
2. Persist an Agy restore intent only after the exact process-to-presence-lock
   match succeeds.
3. Normalize the concrete Agy argv by removing `-c`, `--continue`, and any prior
   `--conversation` selector and value. Do not replay a bare initial prompt.
4. Append `--conversation <uuid>` and launch in the declared working directory.
5. Confirm that the restarted process owns the presence lock for the same UUID
   before rebinding the existing Wakterm agent identity.
6. Retain the intent and show a visible retryable failure if launch or identity
   confirmation fails.

Regression coverage:

- A confirmed Agy transcript produces the correct conversation UUID.
- Launch normalization preserves safe options and inserts exactly one
  `--conversation <uuid>` selector.
- A restored process is accepted only when its PID incarnation owns the expected
  conversation lock.
- A different or missing conversation remains a visible recoverable failure and
  never becomes a fresh Agy session or generic shell.
- A detected but unconfirmed Agy pane remains non-restorable.

## Validation order

1. Fix the service executable path and rerun the existing Claude restart test.
2. Add Agy to the shared native restore pipeline with exact-session confirmation.
3. Test Claude and Agy together through two consecutive mux restarts, including a
   provider upgrade between restarts.

The Claude test isolates executable resolution in code that otherwise reaches
the correct native resume path. The Agy work then exercises the provider
extension boundary without mixing the two causes.
