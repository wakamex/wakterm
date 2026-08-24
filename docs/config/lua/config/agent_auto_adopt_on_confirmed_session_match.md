---
tags:
  - agent
---
# `agent_auto_adopt_on_confirmed_session_match = true`

Controls whether detected agent harnesses in terminal panes are automatically adopted into persistent agent metadata once their session observer confirms a backing session.

When enabled, Wakterm detects supported harnesses (Agy, Claude, Codex, Gemini, OpenCode) from runtime process and title evidence, waits for the background observer to confirm an exact provider session match, and promotes the pane to persistent adopted agent metadata.

Confirmation is required before metadata is persisted. Weak title or process heuristics never adopt a pane on their own.

Example:

```lua
config.agent_auto_adopt_on_confirmed_session_match = true
```

The default is `true`.

See also: [Agent Harness Lifecycle](../../../agent-lifecycle.md).
