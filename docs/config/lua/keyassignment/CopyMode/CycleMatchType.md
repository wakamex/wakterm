# CopyMode `CycleMatchType`

Move the CopyMode/SearchMode cycle between case-sensitive, case-insensitive
and regular expression match types.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    search_mode = {
      { key = 'r', mods = 'CTRL', action = act.CopyMode 'CycleMatchType' },
    },
  },
}
```

