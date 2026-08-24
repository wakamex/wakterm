# CopyMode `MoveUp`

Moves the CopyMode cursor position one cell up.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      { key = 'UpArrow', mods = 'NONE', action = act.CopyMode 'MoveUp' },
    },
  },
}
```

