# `QuickSelect`

Activates [Quick Select Mode](../../../quickselect.md).

```lua
local wakterm = require 'wakterm'

config.keys = {
  { key = ' ', mods = 'SHIFT|CTRL', action = wakterm.action.QuickSelect },
}
```

See also [QuickSelectArgs](QuickSelectArgs.md)
