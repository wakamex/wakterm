# CopyMode `PriorMatch`

Move the CopyMode/SearchMode selection to the previous matching text, if any.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    search_mode = {
      { key = 'Enter', mods = 'NONE', action = act.CopyMode 'PriorMatch' },
    },
  },
}
```

