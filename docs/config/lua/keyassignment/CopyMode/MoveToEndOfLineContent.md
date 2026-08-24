# CopyMode `MoveToEndOfLineContent`

Moves the CopyMode cursor position to the last non-space cell in the current
line.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = '$',
        mods = 'NONE',
        action = act.CopyMode 'MoveToEndOfLineContent',
      },
    },
  },
}
```

