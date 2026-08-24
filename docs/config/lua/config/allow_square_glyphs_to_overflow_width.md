---
tags:
  - font
---

# `allow_square_glyphs_to_overflow_width = "Never"`

Configures how square symbol glyph's cell is rendered:

* "WhenFollowedBySpace" - (this is the default) deliberately overflow the cell
  width when the next cell is a space.
* "Always" - overflow the cell regardless of the next cell
  being a space.
* "Never" - strictly respect the cell width.

This setting now applies to any glyph with an aspect ratio
larger than 0.9, which covers more symbol glyphs than in
earlier releases.

The default value for this setting was changed from `Never` to
`WhenFollowedBySpace`.
