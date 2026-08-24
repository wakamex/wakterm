# CopyMode `MoveDown`

Moves the CopyMode cursor position one cell down.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      { key = 'DownArrow', mods = 'NONE', action = act.CopyMode 'MoveDown' },
    },
  },
}
```

