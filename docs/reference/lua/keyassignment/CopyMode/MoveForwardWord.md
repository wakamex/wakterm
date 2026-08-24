# CopyMode `MoveForwardWord`

Moves the CopyMode cursor position one word to the right.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      { key = 'w', mods = 'NONE', action = act.CopyMode 'MoveForwardWord' },
    },
  },
}
```

