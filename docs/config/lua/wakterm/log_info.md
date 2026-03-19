---
title: wakterm.log_info
tags:
 - utility
 - log
 - debug
---
# `wakterm.log_info(arg, ..)`

{{since('20210314-114017-04b7cedd')}}

This function logs the provided message string through wakterm's logging layer
at 'INFO' level, which can be displayed via [ShowDebugOverlay](../keyassignment/ShowDebugOverlay.md) action.  If you started wakterm from a terminal that text will print
to the stdout of that terminal.  If running as a daemon for the multiplexer
server then it will be logged to the daemon output path.

```lua
local wakterm = require 'wakterm'
wakterm.log_info 'Hello!'
```

{{since('20210814-124438-54e29167')}}

Now accepts multiple arguments, and those arguments can be of any type.


See also [log_error](log_error.md) and [log_warn](log_warn.md).

