# CopyMode `MoveToViewportTop`

{{since('20220624-141144-bd1b7c5d')}}

Moves the CopyMode cursor position to the top of the viewport.


```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'H',
        mods = 'NONE',
        action = act.CopyMode 'MoveToViewportTop',
      },
    },
  },
}
```

