# Command Line

This section documents the wakterm command line interface.

Note that `wakterm --help` or `wakterm SUBCOMMAND --help` will show the precise set of options applicable to your installed version of wakterm.

wakterm is deployed with two executables:

- `wakterm` (or `wakterm.exe` on Windows): command line interface for scripting, multiplexing, and agent commands
- `wakterm-gui` (or `wakterm-gui.exe` on Windows): desktop GUI client

You will typically use `wakterm` when scripting or interacting with wakterm from a terminal. It knows when to delegate to `wakterm-gui` under the covers.

If you are setting up a launcher for wakterm to run in the Windows GUI environment, target `wakterm-gui` so Windows does not open an extra console host for logging.

Note that `wakterm-gui.exe --help` will not output anything to a console when run on Windows systems because it runs in the Windows GUI subsystem. Use `wakterm.exe --help` to see command line help on Windows.

## Key subcommands

- [wakterm agent](agent.md): start, observe, and interact with AI coding agents and harness panes
- [wakterm cli](cli/index.md): control windows, tabs, panes, and layout snapshots in a running instance
- [wakterm connect](connect.md): connect to a multiplexer domain
- [wakterm ssh](ssh.md): establish an SSH terminal session
- [wakterm start](start.md): start the GUI

## Synopsis

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm--help.txt" %}
```
