# CopyMode `{ SetSelectionMode = MODE }`

Sets the CopyMode selection mode.

MODE can be one of:

* `"Cell"` - selection expands a single cell at a time
* `"Word"` - selection expands by a word at a time
* `"Line"` - selection expands by a line at a time
* `"Block"` - selection expands to define a rectangular block using the starting point and current cursor position as the corners
* `"SemanticZone"` - selection expands to the current semantic zone. See [Shell Integration](../../../../shell-integration.md).

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'v',
        mods = 'NONE',
        action = act.CopyMode { SetSelectionMode = 'Cell' },
      },
    },
  },
}
```

See also: [ClearSelectionMode](ClearSelectionMode.md).
