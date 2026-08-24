---
tags:
  - security
  - terminal
---
# `enable_checksum_rectangular_area = false`

When set to `true`, Wakterm responds to DECRQCRA checksum requests. This lets
terminal applications calculate checksums over displayed cells, but it can
also let an untrusted program recover text that is visible in the terminal.

The default is `false`. Enable this only when compatibility with tools such as
esctest requires DECRQCRA responses.
