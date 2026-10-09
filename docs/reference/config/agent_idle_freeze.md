---
tags:
  - agent
---
# `agent_idle_freeze = false`

When `true`, Wakterm freezes an idle Claude agent after 30 seconds without input, so the operating system can page out its memory, and thaws it before input reaches it. An idle Claude Code process otherwise wakes every second to check files, which keeps its memory resident.

A frozen agent is thawed before any input reaches its pane: keys, mouse input the program receives, pastes, `wakterm agent send`, admissions, approval answers and reminders. Focusing the pane or resizing it also thaws it. A frozen process looks idle in `wakterm agent list`, as it did before it was frozen.

Wakterm freezes a Claude agent only when every wakeup reaches it as input through Wakterm, and Claude reports in its session record that:

- it is idle, with no background task running;
- no Remote Control session is connected;
- it has no socket for messages from other Claude sessions, so they reach it through Wakterm instead.

Claude's own timers must also be off. In `~/.claude/settings.json`, or the project's settings, set:

```json
{
  "env": {
    "CLAUDE_CODE_DISABLE_CRON": "1",
    "CLAUDE_CODE_HARBOR_KITE": "0"
  },
  "permissions": {
    "deny": ["ScheduleWakeup"]
  }
}
```

`CLAUDE_CODE_DISABLE_CRON` turns off Claude's scheduled tasks, the `ScheduleWakeup` rule turns off the timer behind `/loop`, and `CLAUDE_CODE_HARBOR_KITE` turns off messages between Claude sessions. Agents then schedule their wakeups with [`wakterm agent remind`](../../cli/reference/agent.md#wakterm-agent-remind), which reaches a frozen agent. A Claude that has any of these on keeps running.

Freezing is available on Linux and Windows. On Linux it uses the cgroup v2 freezer: the agent's process moves into a child of the mux's own cgroup, which is frozen. Unlike a stop signal, this leaves the shell the agent runs under unaware of it. When the mux runs as a systemd service, set `Delegate=yes` on the service so that systemd leaves those child cgroups to the mux. On Windows the process is suspended, which its shell does not observe, and its memory is moved to the page file at once.

```lua
config.agent_idle_freeze = true
```
