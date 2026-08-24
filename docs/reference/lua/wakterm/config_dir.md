---
title: wakterm.config_dir
tags:
 - filesystem
---

# `wakterm.config_dir`

This constant is set to the path to the directory in which your `wakterm.lua`
configuration file was found.

```lua
local wakterm = require 'wakterm'
wakterm.log_error('Config Dir ' .. wakterm.config_dir)
```


