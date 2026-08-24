---
title: wakterm.shell_split
tags:
 - utility
 - open
 - spawn
 - string
---
# wakterm.shell_split(line)

Splits a command line into an argument array according to posix shell rules.

```
> wakterm.shell_split("ls -a")
[
    "ls",
    "-a",
]
```

```
> wakterm.shell_split("echo 'hello there'")
[
    "echo",
    "hello there",
]
```
