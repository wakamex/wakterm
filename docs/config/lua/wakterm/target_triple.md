---
title: wakterm.target_triple
tags:
 - utility
 - version
---
# `wakterm.target_triple`

This constant is set to the [Rust target
triple](https://forge.rust-lang.org/release/platform-support.html) for the
platform on which `wakterm` was built.  This can be useful when you wish to
conditionally adjust your configuration based on the platform.

```lua
local wakterm = require 'wakterm'

if wakterm.target_triple == 'x86_64-pc-windows-msvc' then
  -- We are running on Windows; maybe we emit different
  -- key assignments here?
end
```

The most common triples are:

* `x86_64-pc-windows-msvc` - Windows
* `x86_64-apple-darwin` - macOS (Intel)
* `aarch64-apple-darwin` - macOS (Apple Silicon)
* `x86_64-unknown-linux-gnu` - Linux


