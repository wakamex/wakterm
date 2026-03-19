# CopyMode `MoveToViewportBottom`

{{since('20220624-141144-bd1b7c5d')}}

Moves the CopyMode cursor position to the bottom of the viewport.


```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'L',
        mods = 'NONE',
        action = act.CopyMode 'MoveToViewportBottom',
      },
    },
  },
}
```
