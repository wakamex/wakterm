# CopyMode `MoveToScrollbackTop`

Moves the CopyMode cursor position to the top of the scrollback.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'g',
        mods = 'NONE',
        action = act.CopyMode 'MoveToScrollbackTop',
      },
    },
  },
}
```

