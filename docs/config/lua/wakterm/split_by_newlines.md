---
title: wakterm.split_by_newlines
tags:
 - utility
 - string
---
# `wakterm.split_by_newlines(str)`

{{since('20200503-171512-b13ef15f')}}

This function takes the input string and splits it by newlines (both `\n` and `\r\n`
are recognized as newlines) and returns the result as an array of strings that
have the newlines removed.

```lua
local wakterm = require 'wakterm'

local example = 'hello\nthere\n'

for _, line in ipairs(wakterm.split_by_newlines(example)) do
  wakterm.log_error(line)
end
```


