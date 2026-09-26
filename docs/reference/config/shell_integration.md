---
tags:
  - shell_integration
---
# `shell_integration`

When true (the default), bash, zsh and fish shells started in local panes load Wakterm's [shell integration](../../shell-integration.md) without changes to their startup files. Their usual startup files still run, and the environment they see is restored before those files run.

Wakterm recognizes the default program and commands that start one of these shells directly. Other commands, and panes in SSH or other remote domains, are left unchanged. Bash is started as `bash --posix` so that it runs Wakterm's startup hook, which turns POSIX mode back off, so it is only used for the default program and for `bash` with `-l`, `--login`, `-i` or `-c`.

To load the integration yourself, or not at all:

```lua
config.shell_integration = false
```
