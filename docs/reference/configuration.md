# Configuration Reference

Wakterm reads `wakterm.lua`, which returns a table containing its configuration. The configuration reference covers the fields in that table and the functions and types used to construct it.

```lua
local wakterm = require 'wakterm'
local config = {}
config.font = wakterm.font 'JetBrains Mono'
return config
```

## Configuration options

[Config Options](config/index.md) documents the fields available in that table.

## Functions and types

The [`wakterm` module](lua/wakterm/index.md) is the main entry point. The rest of this section organizes related functions and types by module or object.
