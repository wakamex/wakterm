---
tags:
  - tab_bar
---
# `tab_bar_color_mode = "Off"`

Controls built-in per-tab background coloring in the tab bar.

Possible values are:

- `"Off"`: disable built-in tab color assignment
- `"Hash"`: deterministically hash each tab identity to a generated color
- `"Assign"`: persist first-seen tab identities and assign new colors to stay distinct from prior assignments

When assigning colors, wakterm keys each tab by:

- the effective title without transient badges, if available. Collision suffixes
  such as `2` and `3` remain part of the identity
- otherwise the right-most segment of the active pane cwd
- otherwise the tab ID

Effective titles and cwd segments share the same name namespace, so a title and
cwd that both resolve to `x` receive the same color.

`"Assign"` persists these key-to-color assignments across sessions. It uses an
offline-generated farthest-first sequence and does not reuse an RGB color until
the 512-color sequence is exhausted.

Generated colors are only applied when your `format-tab-title` callback has
not already set explicit foreground/background colors for the tab.

Use [tab_bar_color_intensity](tab_bar_color_intensity.md) to adjust how much
generated tab backgrounds are dimmed in the active, hover, and inactive states.

The default is `"Off"`.
