---
title: wakterm.read_dir
tags:
 - utility
 - filesystem
---
# `wakterm.read_dir(path)`

{{since('20200503-171512-b13ef15f')}}

This function returns an array containing the absolute file names of the
directory specified.  Due to limitations in the lua bindings, all of the paths
must be able to be represented as UTF-8 or this function will generate an
error.

```lua
local wakterm = require 'wakterm'

-- logs the names of all of the entries under `/etc`
for _, v in ipairs(wakterm.read_dir '/etc') do
  wakterm.log_error('entry: ' .. v)
end
```


