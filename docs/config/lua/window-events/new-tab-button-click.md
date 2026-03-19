# `new-tab-button-click`

{{since('20230326-111934-3666303c')}}

The `new-tab-button-click` event is emitted when the user clicks on the
"new tab" button in the tab bar. This is the `+` button that is drawn
to the right of the last tab.

The first event parameter is a [`window` object](../window/index.md) that
represents the gui window.

The second event parameter is a [`pane` object](../pane/index.md) that
represents the active pane in the window.

The third event parameter is an indication of which mouse button was clicked.
The following values are possible:

* `"Left"` - the left mouse button
* `"Right"` - the right mouse button
* `"Middle"` - the middle mouse button

The last event parameter is a [KeyAssignment](../keyassignment/index.md) which
encodes the default, built-in action that wakterm will take.  It may be `nil`
in the case where wakterm would not take any action.

You may take any action you wish in this event handler.

If you return `false` then you will prevent wakterm from carrying out its
default action.

Otherwise, wakterm will proceed to perform that action once your event
handler returns.

This following two examples are equivalent in functionality:

```lua
wakterm.on(
  'new-tab-button-click',
  function(window, pane, button, default_action)
    -- just log the default action and allow wakterm to perform it
    wakterm.log_info('new-tab', window, pane, button, default_action)
  end
)
```

```lua
wakterm.on(
  'new-tab-button-click',
  function(window, pane, button, default_action)
    wakterm.log_info('new-tab', window, pane, button, default_action)
    -- We're explicitly going to perform the default action
    if default_action then
      window:perform_action(default_action, pane)
    end
    -- and tell wakterm that we handled the event so that it doesn't
    -- perform it a second time.
    return false
  end
)
```

See also [window:perform_action()](../window/perform_action.md).
