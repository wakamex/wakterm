---
tags:
  - tab_bar
---
# `tab_titles_shrink_to_fit = true`

When the tabs in the fancy tab bar do not fit in the window, every tab but the active one first loses its close button, and then the longest titles are shortened to a shared width so that every tab fits. Titles shorter than that width stay whole, so tabs with long titles lose characters first, and the active tab always keeps its whole title. A shortened title fades into its tab's background over its last two characters instead of ending in an ellipsis. Once a title is down to three characters, the fade narrows to one character and then none.

A tab with a harness icon can narrow further: its title slides left under the icon, each character fading as the icon covers it, until only the icon is left. A tab without an icon keeps three characters. If the tabs still do not fit, the remaining tabs overflow the tab bar.

Titles grow back as tabs close or the window widens. Harness icons and the attention marker are never shortened.

Set this to `false` to keep every title whole. It applies to the fancy tab bar only; the retro tab bar divides the width between tabs and cuts titles at [`tab_max_width`](tab_max_width.md).

```lua
config.tab_titles_shrink_to_fit = false
```
