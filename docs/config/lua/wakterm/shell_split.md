---
title: wakterm.shell_split
tags:
 - utility
 - open
 - spawn
 - string
---
# wakterm.shell_split(line)

{{since('20220807-113146-c2fee766')}}

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
