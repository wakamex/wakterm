# CopyMode `MoveToScrollbackBottom`

Moves the CopyMode cursor position to the bottom of the scrollback.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'G',
        mods = 'NONE',
        action = act.CopyMode 'MoveToScrollbackBottom',
      },
    },
  },
}
```

