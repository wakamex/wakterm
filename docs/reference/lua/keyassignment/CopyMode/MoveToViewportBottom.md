# CopyMode `MoveToViewportBottom`

Moves the CopyMode cursor position to the bottom of the viewport.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'L',
        mods = 'NONE',
        action = act.CopyMode 'MoveToViewportBottom',
      },
    },
  },
}
```
