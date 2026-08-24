# CopyMode `MoveToStartOfLine`

Moves the CopyMode cursor position to the first cell in the current line.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = '0',
        mods = 'NONE',
        action = act.CopyMode 'MoveToStartOfLine',
      },
    },
  },
}
```

