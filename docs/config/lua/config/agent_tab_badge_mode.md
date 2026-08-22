---
tags:
  - agent
  - tab_bar
---
# `agent_tab_badge_mode = "identity"`

Controls when agent indicators (harness icons and text badges) appear in tabs.

Possible values are:

- `"identity"`: always show the harness icon or badge when an agent is detected (default)
- `"attention"`: show only when an agent needs user review or input
- `"turn"`: show when an agent is actively waiting on user input
- `"off"`: never show agent indicators in the tab bar

Example:

```lua
config.agent_tab_badge_mode = 'identity'
```

When set to `"identity"` and a supported harness (Claude, Codex, Gemini, OpenCode) is active, the tab bar displays that harness's native icon. If no harness icon is available, the text badge from [agent_tab_badge](agent_tab_badge.md) is used.

See also: [agent_tab_attention_pulse](agent_tab_attention_pulse.md), [agent_tab_badge](agent_tab_badge.md).
