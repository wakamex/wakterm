---
tags:
  - tuning
---
# `max_fps = 60`

Limits the maximum number of frames per second that wakterm will
attempt to draw.

Defaults to `60`.

This setting applies on X11, macOS, and Windows. Wayland ignores it and uses
information from the compositor to schedule frames.
