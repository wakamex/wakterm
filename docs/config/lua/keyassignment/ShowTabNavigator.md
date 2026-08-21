# `ShowTabNavigator`

Activate the tab navigator UI in the current tab.  The tab
navigator displays a list of tabs and allows you to select
and activate a tab from that list.

```lua
config.keys = {
  { key = 'F9', mods = 'ALT', action = wakterm.action.ShowTabNavigator },
}
```

{{since('nightly')}}

The choice corresponding to the current tab is initially selected. Plain text
filters the list. Tab cycles Visible, Parked, and All views. `Ctrl-Shift-S` parks
or unparks the selected tab, `Ctrl-X` closes it permanently after confirmation,
`Ctrl-R` changes sorting, and `Ctrl-O` toggles row density. Parked rows include
approximate process RSS when it is available.
