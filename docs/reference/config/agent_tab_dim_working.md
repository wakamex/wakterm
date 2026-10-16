---
tags:
  - tab_bar
  - agent
---
# `agent_tab_dim_working = true`

In the fancy tab bar, a tab whose agents are all working is drawn darker, toward the tab bar's background, with a small dot blinking at the lower right of its harness icon. Tabs whose agent waits for you keep their normal colors, so they are easy to find.

A tab counts as working when at least one agent in it is in the middle of its turn and none waits for you. Tabs without agents, and agents whose state is unknown, are drawn as usual. The active tab keeps its colors and still shows the blinking dot while its agent works.

Set this to `false` to draw every tab in its normal colors:

```lua
config.agent_tab_dim_working = false
```

Set `agent_tab_dim_reviewed = true` to also dim, without the dot, a tab whose agent waits for you but whose latest reply you have already seen. Then only the active tab and tabs with an unread reply keep their normal colors. It is off by default:

```lua
config.agent_tab_dim_reviewed = true
```

See also: [agent_tab_attention_pulse](agent_tab_attention_pulse.md).
