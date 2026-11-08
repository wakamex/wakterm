# `ToggleUnparkOnTurnEnd`

Switches whether the current window brings parked tabs back: while it is on, a tab parked with [`ParkCurrentTab`](ParkCurrentTab.md) returns to the tab bar as soon as one of its agents completes a turn. Only turns completed after the tab was parked, or after the switch was turned on, count. A notification says which way it was switched.

Together with [`ToggleTabActivityOrder`](ToggleTabActivityOrder.md), a returning tab has the newest completed turn and so appears first in activity order, which lets parking stand for "out of the way until its agent is done".

Parking is shared, so a tab that returns does so for every window and client that shows it. Each window starts with the switch set by [`agent_unpark_on_turn_end`](../../config/agent_unpark_on_turn_end.md).

The default shortcut is Ctrl+Shift+Alt+B on Linux and Windows and Cmd+Shift+Opt+B on macOS.
