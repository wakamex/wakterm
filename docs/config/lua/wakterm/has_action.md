---
title: wakterm.has_action
tags:
 - utility
 - version
---

# wakterm.has_action(NAME)

{{since('20230408-112425-69ae8472')}}

Returns true if the string *NAME* is a valid key assignment action variant
that can be used with [wakterm.action](action.md).

This is useful when you want to use a wakterm configuration across multiple
different versions of wakterm.

```lua
if wakterm.has_action 'PromptInputLine' then
  table.insert(config.keys, {
    key = 'p',
    mods = 'LEADER',
    action = wakterm.action.PromptInputLine {
      -- other parameters here
    },
  })
end
```
