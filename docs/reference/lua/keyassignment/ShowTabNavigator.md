# `ShowTabNavigator`

Opens the dedicated tab navigator overlay to search, review, hide, show, and switch tabs.

The default shortcut is `Ctrl-Shift-E` (or `Cmd-E` on macOS).

The choice corresponding to the current tab is initially selected.

## Controls inside the tab navigator

- Plain text: filters the tab list using fuzzy matching against the tab title
- Up / Down: move selection
- Enter: activate the selected tab and show it if hidden
- Left / Right or Tab / Shift-Tab: switch between All, Visible, and Hidden views
- Ctrl-Shift-S: toggle hide or show for the selected tab
- Ctrl-X: prompt to permanently close the selected tab
- Ctrl-R: toggle sort between Tab order and Response time
- Ctrl-O: toggle row density between dense single-line and comfortable multi-line pane details
- Escape: clear search query, or exit the navigator if query is empty

Hidden rows display an eye-slash icon and approximate process RSS when available.

```lua
config.keys = {
  {
    key = 'e',
    mods = 'CTRL|SHIFT',
    action = wakterm.action.ShowTabNavigator,
  },
}
```
