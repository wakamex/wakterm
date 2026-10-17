---
tags:
  - font
  - command_palette
---
# `command_palette_line_height = 1.0`

Scales the computed line height used by [ActivateCommandPalette](../lua/keyassignment/ActivateCommandPalette.md). The default is `1.0`, which uses the font-specified metrics.

Set `command_palette_line_height = 1.2` to increase the vertical spacing by 20%, or `0.9` to decrease it by 10%.

```lua
config.command_palette_line_height = 1.2
```

See also [line_height](line_height.md), which applies to terminal cells.
