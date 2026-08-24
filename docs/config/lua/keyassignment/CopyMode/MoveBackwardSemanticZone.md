# CopyMode `MoveBackwardSemanticZone`

Moves the CopyMode cursor position one semantic zone to the left.

See [Shell Integration](../../../../shell-integration.md) for more information
about semantic zones.

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

return {
  key_tables = {
    copy_mode = {
      {
        key = 'z',
        mods = 'NONE',
        action = act.CopyMode 'MoveBackwardSemanticZone',
      },
    },
  },
}
```

