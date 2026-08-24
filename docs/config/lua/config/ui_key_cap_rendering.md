---
tags:
  - appearance
---

# ui_key_cap_rendering

Controls how keyboard shortcuts are rendered in the Command Palette.

Possible values are:

* `"UnixLong"` - `Super`, `Meta`, `Ctrl`, `Shift`.
* `"Emacs"` - `Super`, `M`, `C`, `S`.
* `"AppleSymbols"` - use macOS style symbols for Command, Option and so on.
* `"WindowsLong"` - `Win`, `Alt`, `Ctrl`, `Shift`.
* `"WindowsSymbols"` - like `WindowsLong` but using a logo for the `Win` key.

The default is a platform-appropriate value.
