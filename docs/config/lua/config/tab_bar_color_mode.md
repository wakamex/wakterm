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

The default is `"Off"`.
