# `agent_tab_attention_pulse = true`

When `agent_tab_badge_mode = "identity"`, harness icons remain visible for the
life of the agent. If the latest completed response has not been reviewed, the
icon slowly pulses.

Set this to `false` to disable motion. A static attention marker remains
visible.

```lua
config.agent_tab_attention_pulse = false
```
