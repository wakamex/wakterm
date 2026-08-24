# CopyMode `MoveToViewportMiddle`

Moves the CopyMode cursor position to the middle of the viewport.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'M',
        mods = 'NONE',
        action = act.CopyMode 'MoveToViewportMiddle',
      },
    },
  },
}
```

