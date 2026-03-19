# CopyMode `MoveToStartOfNextLine`

{{since('20220624-141144-bd1b7c5d')}}

Moves the CopyMode cursor position to the first cell in the next line.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'Enter',
        mods = 'NONE',
        action = act.CopyMode 'MoveToStartOfNextLine',
      },
    },
  },
}
```



