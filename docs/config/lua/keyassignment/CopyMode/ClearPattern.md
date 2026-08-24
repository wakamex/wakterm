# CopyMode `ClearPattern`

Clear the CopyMode/SearchMode search pattern.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    search_mode = {
      { key = 'u', mods = 'CTRL', action = act.CopyMode 'ClearPattern' },
    },
  },
}
```

