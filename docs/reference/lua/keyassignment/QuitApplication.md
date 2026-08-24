# `QuitApplication`

Terminate the wakterm application, killing all tabs.

```lua
local wakterm = require 'wakterm'

config.keys = {
  { key = 'q', mods = 'CMD', action = wakterm.action.QuitApplication },
}
```


