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

The header shows a process count and proportional memory total for the known Codex agents on each connected mux, including their Codex subprocesses and the shared app-server counted once. It includes agents across all windows and hidden tabs, and filtering the tab list does not change its scope. Separate connections have separate labelled totals.

Proportional memory (PSS) accounts for shared pages without counting them repeatedly. It is available when the mux host runs Linux. The header uses decimal MB or GB, displays the sample time in UTC, and refreshes in the background while the navigator is open. An incomplete or unsupported measurement displays `PSS unavailable`. The total covers Codex processes; other programs and Wakterm use additional memory. Per-tab RSS remains a separate approximate measurement.

```lua
config.keys = {
  {
    key = 'e',
    mods = 'CTRL|SHIFT',
    action = wakterm.action.ShowTabNavigator,
  },
}
```
