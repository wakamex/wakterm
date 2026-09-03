# `ToggleTabBarPosition`

Moves the tab bar between the top and bottom of the current window. Toggling it a second time returns the tab bar to the position configured by [`tab_bar_at_bottom`](../../config/tab_bar_at_bottom.md).

The default key assignment is `Ctrl-Shift-A`.

```lua
config.keys = {
  {
    key = 'A',
    mods = 'CTRL|SHIFT',
    action = wakterm.action.ToggleTabBarPosition,
  },
}
```
