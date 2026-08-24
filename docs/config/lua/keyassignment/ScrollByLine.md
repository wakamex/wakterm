# `ScrollByLine`

Adjusts the scroll position by the number of lines specified by the argument.
Negative values scroll upwards, while positive values scroll downwards.

```lua
local act = wakterm.action

config.keys = {
  { key = 'UpArrow', mods = 'SHIFT', action = act.ScrollByLine(-1) },
  { key = 'DownArrow', mods = 'SHIFT', action = act.ScrollByLine(1) },
}
```

