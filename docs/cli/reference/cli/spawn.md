# `wakterm cli spawn`

*Run `wakterm cli spawn --help` to see more help*

Spawn a command into a new tab or window.  Outputs the pane-id for the newly
created pane on success.

When run with no arguments, it will spawn a new tab running the default
program; this example spawns a new pane with id 1 running that default program
(most likely: your shell):


```
$ wakterm cli spawn
1
```

You may spawn an alternative program by passing the argument list; it is
recommended that you use `--` to denote the end of the arguments being passed
to `wakterm cli spawn` so that any parameters you may wish to pass to the
program are not confused with parameters to `wakterm cli spawn`.  This example
launches `top` in a new tab:

```
$ wakterm cli spawn -- top
2
```

This example explicitly runs bash as a login shell:

```
$ wakterm cli spawn -- bash -l
3
```

A program given this way runs in the mux server's environment, as the default program does, rather than in the environment of the shell that ran this command. Variables set in the calling session, such as an activated virtual environment or the markers an agent sets for programs it runs, therefore do not follow into the new pane. Pass `--env KEY=VALUE` for a variable the program needs, or `--inherit-env` to run it with this command's whole environment:

```
$ wakterm cli spawn --env RUST_LOG=debug -- cargo run
$ wakterm cli spawn --inherit-env -- python
```

The following options affect the behavior:

* `--cwd CWD` - Specifies the current working directory that should be set for the spawned program
* `--env KEY=VALUE` - Sets an environment variable for the program. Repeatable.
* `--inherit-env` - Runs the program with the environment of the shell that ran this command, instead of the mux server's.
* `--domain-name DOMAIN_NAME` - Spawn into the named multiplexer domain. The default is to spawn into the domain of the current pane.
* `--new-window` - Spawns the tab into a window of its own.
* `--workspace WORKSPACE` - when using `--new-window`, set the workspace name rather than using the default name of `"default"`.
* `--window-id WINDOW_ID` - Spawn the tab into the specified window, rather than using the current window


## Synopsis

```console
{% include "../../../generated/cli-help/cmd-synopsis-wakterm-cli-spawn--help.txt" %}
```
