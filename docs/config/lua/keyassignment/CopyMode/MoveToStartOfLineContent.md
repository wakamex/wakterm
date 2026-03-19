# CopyMode `MoveToStartOfLineContent`

{{since('20220624-141144-bd1b7c5d')}}

Moves the CopyMode cursor position to the first non-space cell in the current
line.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = '^',
        mods = 'NONE',
        action = act.CopyMode 'MoveToStartOfLineContent',
      },
    },
  },
}
```


