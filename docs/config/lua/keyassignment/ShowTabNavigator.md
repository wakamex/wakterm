# `ShowTabNavigator`

Opens the dedicated tab navigator overlay to search, review, park, unpark, and switch tabs.

The default shortcut is `Ctrl-Shift-E` (or `Cmd-E` on macOS).

The choice corresponding to the current tab is initially selected.

## Controls inside the tab navigator

- Plain text: filters the tab list using fuzzy matching against title, cwd, git branch, harness, workspace, and agent name
- Up / Down: move selection
- Enter: activate the selected tab and unpark it if parked
- Left / Right or Tab / Shift-Tab: switch between Visible, Parked, and All views
- Ctrl-Shift-S: toggle park or unpark for the selected tab
- Ctrl-X: prompt to permanently close the selected tab
- Ctrl-R: toggle sort between Tab order and Response time
- Ctrl-O: toggle row density between dense single-line and comfortable multi-line pane details
- Escape: clear search query, or exit the navigator if query is empty

Parked rows display approximate process RSS when available.

```lua
config.keys = {
  { key = 'e', mods = 'CTRL|SHIFT', action = wakterm.action.ShowTabNavigator },
}
```
