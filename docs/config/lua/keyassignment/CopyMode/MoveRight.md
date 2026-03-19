# CopyMode `MoveRight`

{{since('20220624-141144-bd1b7c5d')}}

Moves the CopyMode cursor position one cell to the right.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'RightArrow',
        mods = 'NONE',
        action = act.CopyMode 'MoveRight',
      },
    },
  },
}
```
