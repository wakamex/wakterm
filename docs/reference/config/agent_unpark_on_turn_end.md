# `agent_unpark_on_turn_end = false`

Whether a new window brings parked tabs back when one of their agents completes a turn. A tab parked with [`ParkCurrentTab`](../lua/keyassignment/ParkCurrentTab.md) then returns to the tab bar as soon as its agent finishes the turn it was working on, or any later turn. [`ToggleUnparkOnTurnEnd`](../lua/keyassignment/ToggleUnparkOnTurnEnd.md) switches it for one window.

```lua
config.agent_unpark_on_turn_end = true
```
