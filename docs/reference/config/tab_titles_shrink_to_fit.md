---
tags:
  - tab_bar
---
# `tab_titles_shrink_to_fit = true`

When the tabs in the fancy tab bar do not fit in the window, the longest titles are shortened to a shared width so that every tab fits. Titles shorter than that width stay whole, so tabs with long titles lose characters first. A shortened title fades into its tab's background over its last two characters instead of ending in an ellipsis, and keeps at least three characters. If the tabs still do not fit at that size, the remaining tabs overflow the tab bar.

Titles grow back as tabs close or the window widens. Harness icons and the attention marker are never shortened.

Set this to `false` to keep every title whole. It applies to the fancy tab bar only; the retro tab bar divides the width between tabs and cuts titles at [`tab_max_width`](tab_max_width.md).

```lua
config.tab_titles_shrink_to_fit = false
```
