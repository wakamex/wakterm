---
tags:
  - keys
---
# `macos_forward_to_ime_modifier_mask = "SHIFT"`

On macOS systems, this option controls whether modified key presses are
routed via the IME when [use_ime = true](use_ime.md).

When processing a key event, if any modifiers are held, if the modifiers
intersect with the value of `macos_forward_to_ime_modifier_mask`, then the key
event is routed to the IME, which may choose to swallow the key event as
part of its own state management.

Users of a Japanese IME may wish to set this to `"SHIFT|CTRL"`,
but should note that it will prevent certain CTRL key combinations
that are commonly used in unix terminal programs from working as
expected.
