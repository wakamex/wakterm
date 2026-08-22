---
tags:
  - agent
  - tab_bar
---
# `agent_tab_badge = "🤖 "`

Text prefix shown in tab titles for agent panes that do not have a dedicated harness icon.

When a harness-specific icon (Claude, Codex, Gemini, OpenCode) is available, the native icon is rendered instead and this text badge is suppressed.

Example:

```lua
config.agent_tab_badge = '🤖 '
```

Set to the empty string `""` to suppress the fallback text badge.

See also: [agent_tab_badge_mode](agent_tab_badge_mode.md).
