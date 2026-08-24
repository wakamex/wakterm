# `window:current_event()`

Returns the current event.
For now only implemented for mouse events.

This example prints the delta scroll value
when you scroll up with your mouse wheel while holding `CTRL`:

```lua
local wakterm = require 'wakterm'

return {
  mouse_bindings = {
    {
      event = { Down = { streak = 1, button = { WheelUp = 1 } } },
      mods = 'CTRL',
      action = wakterm.action_callback(function(window, pane)
        -- note that you want `WheelDown` for a `WheelDown` event
        local delta = window:current_event().Down.button.WheelUp
        wakterm.log_info('delta is: ' .. delta)
      end),
    },
  },
}
```
