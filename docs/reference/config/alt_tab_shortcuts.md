# `alt_tab_shortcuts = true`

On Linux and Windows, Alt+1 through Alt+8 activate the first eight tabs, Alt+9 activates the right-most tab, and Alt+E opens the [tab navigator](../lua/keyassignment/ShowTabNavigator.md). The same actions stay on Ctrl+Shift+1 through Ctrl+Shift+9 and Ctrl+Shift+E.

Set this to `false` to leave Alt with these keys to the program in the pane, such as a shell or an IRC client that uses Alt+number itself. The Ctrl+Shift shortcuts remain.

macOS uses Cmd+1 through Cmd+9 and Cmd+E instead, and this option does not change them.

```lua
config.alt_tab_shortcuts = false
```
