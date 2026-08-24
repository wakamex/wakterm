---
tags:
  - multiplexing
  - wsl
---
# `wsl_domains`

Configures [WSL](https://docs.microsoft.com/en-us/windows/wsl/about) domains.

This option accepts a list of [WslDomain](../lua/WslDomain.md) objects.

The default is a list derived from parsing the output of `wsl -l -v`.  See
[wakterm.default_wsl_domains()](../lua/wakterm/default_wsl_domains.md) for more
about that list, and on how to override it.
