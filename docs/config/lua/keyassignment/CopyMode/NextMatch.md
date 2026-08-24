# CopyMode `NextMatch`

Move the CopyMode/SearchMode selection to the next matching text, if any.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    search_mode = {
      { key = 'n', mods = 'CTRL', action = act.CopyMode 'NextMatch' },
    },
  },
}
```
