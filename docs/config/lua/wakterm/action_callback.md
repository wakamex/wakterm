---
title: wakterm.action_callback
tags:
 - keys
 - event
---

# `wakterm.action_callback(callback)`

{{since('20211204-082213-a66c61ee9')}}

This function is a helper to register a custom event and return an action triggering it.

It is helpful to write custom key bindings directly, without having to declare
the event and use it in a different place.

The implementation is essentially the same as:
```lua
function wakterm.action_callback(callback)
  local event_id = '...' -- the function generates a unique event id
  wakterm.on(event_id, callback)
  return wakterm.action.EmitEvent(event_id)
end
```

See [wakterm.on](./on.md) and [wakterm.action](./action.md) for more info on what you can do with these.


## Usage

```lua
local wakterm = require 'wakterm'

return {
  keys = {
    {
      mods = 'CTRL|SHIFT',
      key = 'i',
      action = wakterm.action_callback(function(win, pane)
        wakterm.log_info 'Hello from callback!'
        wakterm.log_info(
          'WindowID:',
          win:window_id(),
          'PaneID:',
          pane:pane_id()
        )
      end),
    },
  },
}
```
