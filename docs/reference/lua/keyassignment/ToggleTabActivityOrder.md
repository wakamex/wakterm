# `ToggleTabActivityOrder`

Switches the current window's tab bar between its own tab order and activity order, which shows the tabs by their agents' latest completed turn, newest first. A tab whose agents have not completed a turn yet sorts by when the newest of them started its turn, or was adopted if it has not started one, so a new agent sorts among the recently active tabs. Tabs without an agent follow in their own order. The tabs themselves do not move, so other windows and clients keep their order, and tab numbers in key assignments such as `ActivateTab` follow the order the tab bar shows.

While the tab bar shows activity order, closing the active tab activates its neighbor in that order: the tab after it, or the one before it when it was the last.

The default shortcut is Ctrl+Shift+B on Linux and Windows and Cmd+Shift+B on macOS.
