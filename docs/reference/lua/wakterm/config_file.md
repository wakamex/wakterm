---
title: wakterm.config_file
tags:
 - filesystem
---

# `wakterm.config_file`

This constant is set to the path to the `wakterm.lua` that is in use.

```lua
local wakterm = require 'wakterm'
wakterm.log_info('Config file ' .. wakterm.config_file)
```

