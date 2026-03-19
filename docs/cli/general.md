# Command Line

This section documents the wakterm command line.

*Note that `wakterm --help` or `wakterm SUBCOMMAND --help` will show the precise
set of options that are applicable to your installed version of wakterm.*

wakterm is deployed with two major executables:

* `wakterm` (or `wakterm.exe` on Windows) - for interacting with wakterm from the terminal
* `wakterm-gui` (or `wakterm-gui.exe` on Windows) - for spawning wakterm from a desktop environment

You will typically use `wakterm` when scripting wakterm; it knows when to
delegate to `wakterm-gui` under the covers.

If you are setting up a launcher for wakterm to run in the Windows GUI
environment then you will want to explicitly target `wakterm-gui` so that
Windows itself doesn't pop up a console host for its logging output.

!!! note
    `wakterm-gui.exe --help` will not output anything to a console when
    run on Windows systems, because it runs in the Windows GUI subsystem and has no
    connection to the console.  You can use `wakterm.exe --help` to see information
    about the various commands; it will delegate to `wakterm-gui.exe` when
    appropriate.

## Synopsis

```console
{% include "../examples/cmd-synopsis-wakterm--help.txt" %}
```
