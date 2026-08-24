# `wakterm.procinfo.executable_path_for_pid(pid)`

Returns the path to the executable image for the specified process id.

This function may return `nil` if it was unable to return the info.

```
> wakterm.procinfo.executable_path_for_pid(wakterm.procinfo.pid())
"/home/wez/wez-personal/wakterm/target/debug/wakterm-gui"
```

