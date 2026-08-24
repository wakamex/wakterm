# TabInformation

The `TabInformation` struct describes a tab.  `TabInformation` is purely a
snapshot of some of the key characteristics of the tab, intended for use in
synchronous, fast, event callbacks that format GUI elements such as the window
and tab title bars.

The `TabInformation` struct contains the following fields:

* `tab_id` - the identifier for the tab
* `tab_index` - the logical tab position within its containing window, with 0 indicating the leftmost tab
* `is_active` - is true if this tab is the active tab
* `is_last_active` - is true if this tab is the previously active tab.
* `active_pane` - the [PaneInformation](PaneInformation.md) for the active pane in this tab, or `nil` if unavailable
* `window_id` - the ID of the window that contains this tab
* `window_title` - the title of the window that contains this tab
* `tab_title` - the title of the tab
* `effective_title` - the tab title if it was explicitly set, otherwise the active pane title if available, otherwise an empty string
* `assigned_color` - the generated background color assigned to this tab by the built-in tab color system, or `nil` if no generated color is assigned. This is useful in `format-tab-title` callbacks if you want to consume the built-in assignment while still customizing the tab contents.
