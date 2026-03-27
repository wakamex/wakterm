---
tags:
  - tab_bar
---
# `tab_bar_color_palette = "Dark"`

Controls which family of built-in generated tab colors is used when
[tab_bar_color_mode](tab_bar_color_mode.md) is set to `Hash` or `Assign`.

Possible values are:

- `"Dark"`: use a curated dark categorical palette intended for inactive tab backgrounds
- `"Light"`: generate lighter tab backgrounds so they generally pair with dark text
- `"Mixed"`: allow both dark and light generated backgrounds

This option controls the family of generated background colors. In practice,
`"Dark"` keeps the darker curated palette that pairs with the built-in inactive
and hover text colors, `"Light"` tends to yield dark foreground text, and
`"Mixed"` allows either.

The default is `"Dark"`.
