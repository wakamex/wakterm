---
tags:
  - tab_bar
---
# `tab_bar_color_intensity`

Controls how strongly built-in generated tab background colors are dimmed for
each visual state when [tab_bar_color_mode](tab_bar_color_mode.md) is set to
`Hash` or `Assign`.

The value is a table with these fields:

- `active` - multiplier for the active tab generated background
- `hover` - multiplier for the hovered inactive tab generated background
- `inactive` - multiplier for the normal inactive tab generated background

For example:

```lua
config.tab_bar_color_intensity = {
  active = 0.6,
  hover = 0.7,
  inactive = 0.5,
}
```

The defaults are:

```lua
config.tab_bar_color_intensity = {
  active = 0.6,
  hover = 0.5,
  inactive = 0.4,
}
```

This setting only affects the generated tab background brightness. It does not
change the configured tab text colors.
