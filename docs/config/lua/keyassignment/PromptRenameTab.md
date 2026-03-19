# `PromptRenameTab`

{{since('nightly')}}

Prompts for a new title for the current tab and applies it when you press
<kbd>Enter</kbd>.

The input field is pre-populated with:

* the current explicit tab title, if one is set
* otherwise the current pane title

```lua
config.keys = {
  {
    key = 'E',
    mods = 'CTRL|SHIFT',
    action = wakterm.action.PromptRenameTab,
  },
}
```

This is a convenience wrapper around the common `PromptInputLine` +
`window:active_tab():set_title(...)` pattern.
