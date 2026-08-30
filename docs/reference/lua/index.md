# Lua API Reference

Wakterm provides Lua 5.4 as a configuration language. This section documents the Lua functions and types available to the configuration file through the `wakterm` module:

```lua
local wakterm = require 'wakterm'
local config = {}
config.font = wakterm.font 'JetBrains Mono'
return config
```

## Configuration options

[Config Options](../config/index.md) has a list of the main configuration options.
