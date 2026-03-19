---
title: wakterm.hostname
tags:
 - utility
---

# `wakterm.hostname()`

This function returns the current hostname of the system that is running wakterm.
This can be useful to adjust configuration based on the host.

Note that environments that use DHCP and have many clients and short leases may
make it harder to rely on the hostname for this purpose.

```lua
local wakterm = require 'wakterm'
local hostname = wakterm.hostname()

local font_size
if hostname == 'pixelbookgo-localdomain' then
  -- Use a bigger font on the smaller screen of my PixelBook Go
  font_size = 12.0
else
  font_size = 10.0
end

return {
  font_size = font_size,
}
```


