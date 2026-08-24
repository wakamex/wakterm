# `pane:tab()`

Returns the [MuxTab](../MuxTab/index.md) that contains this pane.

Note that this method can return `nil` when *pane* is a GUI-managed overlay
pane (such as the debug overlay), because those panes are not managed by the
mux layer.
