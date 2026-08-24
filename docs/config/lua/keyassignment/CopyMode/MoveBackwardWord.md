# CopyMode `MoveBackwardWord`

Moves the CopyMode cursor position one word to the left.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      { key = 'b', mods = 'NONE', action = act.CopyMode 'MoveBackwardWord' },
    },
  },
}
```
