# `PromptRenameTab`

Prompts for a new title for the current tab and applies it when you press Enter.

The default shortcut is `Ctrl-Shift-<` (or `Cmd-<` on macOS).

The input field is pre-populated with the current explicit tab title, if one is set. If the tab uses an automatic title derived from the active agent or process, the prompt starts empty.

Submitting a non-empty title sets an explicit tab title that is preserved across terminal escape sequences. Submitting an empty title clears any explicit title and returns the tab to automatic naming.

```lua
config.keys = {
  {
    key = '<',
    mods = 'CTRL|SHIFT',
    action = wakterm.action.PromptRenameTab,
  },
}
```
