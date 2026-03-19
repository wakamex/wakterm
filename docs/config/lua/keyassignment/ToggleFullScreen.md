# `ToggleFullScreen`

Toggles full screen mode for the current window.

```lua
local wakterm = require 'wakterm'

config.keys = {
  {
    key = 'n',
    mods = 'SHIFT|CTRL',
    action = wakterm.action.ToggleFullScreen,
  },
}
```

See also: [native_macos_fullscreen_mode](../config/native_macos_fullscreen_mode.md).

