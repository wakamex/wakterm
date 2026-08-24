# CopyMode `Close`

Close copy mode.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      { key = 'q', mods = 'NONE', action = act.CopyMode 'Close' },
    },
  },
}
```

