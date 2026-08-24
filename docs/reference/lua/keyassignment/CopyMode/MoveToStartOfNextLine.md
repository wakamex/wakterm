# CopyMode `MoveToStartOfNextLine`

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

