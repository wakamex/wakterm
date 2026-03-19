---
title: wakterm.run_child_process
tags:
 - utility
 - open
 - spawn
---
# `wakterm.run_child_process(args)`

{{since('20200503-171512-b13ef15f')}}

This function accepts an argument list; it will attempt to spawn that command
and will return a tuple consisting of the boolean success of the invocation,
the stdout data and the stderr data.

```lua
local wakterm = require 'wakterm'

local success, stdout, stderr = wakterm.run_child_process { 'ls', '-l' }
```

See also [background_child_process](background_child_process.md)
