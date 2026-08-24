# `ActivateWindowRelative(delta)`

Activates a GUI window relative to the current window.

`ActivateWindowRelative(1)` activates the next window, while
`ActivateWindowRelative(-1)` activates the previous window.

This action will wrap around and activate the appropriate window
at the start/end.

Here's an example of setting up (not very useful) hotkeys to cycle between
windows:

```lua
local wakterm = require 'wakterm'
local act = wakterm.action
local config = {}

config.keys = {
  { key = 'r', mods = 'ALT', action = act.ActivateWindowRelative(1) },
  { key = 'e', mods = 'ALT', action = act.ActivateWindowRelative(-1) },
}
return config
```

See also [ActivateWindowRelativeNoWrap](ActivateWindowRelativeNoWrap.md),
[ActivateWindow](ActivateWindow.md).
