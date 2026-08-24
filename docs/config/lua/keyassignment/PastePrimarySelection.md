# `PastePrimarySelection`

X11: Paste the Primary Selection to the current tab.
On other systems, this behaves identically to [Paste](Paste.md).

This action is considered to be deprecated and will be removed in
a future release; please use [PasteFrom](PasteFrom.md) instead.

This action has been removed. Please use [PasteFrom](PasteFrom.md) instead.

## Example

```lua
local wakterm = require 'wakterm'
local act = wakterm.action

config.keys = {
  { key = 'v', mods = 'SHIFT|CTRL', action = act.PastePrimarySelection },
}

-- Middle mouse button pastes the primary selection.
config.mouse_bindings = {
  {
    event = { Up = { streak = 1, button = 'Middle' } },
    mods = 'NONE',
    action = act.PastePrimarySelection,
  },
}
```

