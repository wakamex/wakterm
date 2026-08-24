# CopyMode `PriorMatchPage`

Move the CopyMode/SearchMode selection to the previous matching text on the previous page of the screen, if any.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    search_mode = {
      {
        key = 'PageUp',
        mods = 'CTRL',
        action = act.CopyMode 'PriorMatchPage',
      },
    },
  },
}
```

