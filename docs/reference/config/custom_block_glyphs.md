---
tags:
  - appearance
  - font
---
## `custom_block_glyphs = true`

When set to `true` (the default), wakterm will compute its own idea of what the glyphs
in the following unicode ranges should be, instead of using glyphs resolved from a font.

Ideally this option wouldn't exist, but it is present to work around a [hinting issue in freetype](https://gitlab.freedesktop.org/freetype/freetype/-/issues/761).

|Block|What|
|-----|----|
|[U2500](https://www.unicode.org/charts/PDF/U2500.pdf)|Box Drawing|
|[U2580](https://www.unicode.org/charts/PDF/U2580.pdf)|unicode block elements|
|[U1FB00](https://www.unicode.org/charts/PDF/U1FB00.pdf)|Symbols for Legacy Computing (Sextants and Smooth mosaic graphics)|
|[U1CC00](https://www.unicode.org/charts/PDF/U1CC00.pdf)|Symbols for Legacy Computing Supplement (Block mosaic terminal graphic characters)|
|[U2800](https://www.unicode.org/charts/PDF/U2800.pdf)|Braille Patterns|
|[Powerline](https://github.com/ryanoasis/powerline-extra-symbols#glyphs)|Powerline triangle, curve and diagonal glyphs|
|[Git Branch Symbols](https://github.com/wakamex/wakterm/issues/6328)|Custom branch drawing symbols for rendering DAGs such as Git branch structure|
|[Progress Bar Symbols](https://github.com/ryanoasis/nerd-fonts/issues/1345)|Fixed and indeterminate progress bar elements|

You can set this to `false` to use the block characters provided by your font selection.

See also [anti_alias_custom_block_glyphs](anti_alias_custom_block_glyphs.md).
