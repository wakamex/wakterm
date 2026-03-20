# `wakterm cli`

The *cli* subcommand interacts with a running wakterm GUI or multiplexer
instance, and can be used to spawn programs and manipulate tabs and panes.

# Targeting the correct instance

There may be multiple GUI processes running in addition to a multiplexer server.
wakterm uses the following logic to decide which one it should connect to.

* If the `$WAKTERM_UNIX_SOCKET` environment variable is set, use that socket.
  This is commonly injected by wakterm itself so that child processes and
  nested `wakterm cli` commands continue talking to the same host instance.
* Otherwise, if `--prefer-mux` was **not** passed, try to locate a running GUI
  instance. The `--class` argument specifies an optional window class that can
  be used to select the appropriate GUI window if that GUI window was also
  spawned using `--class` to override the default.
* Otherwise, or if no GUI socket can be found, consult `wakterm.lua` and use
  the first configured *unix domain*. If you have not customized it, this is
  typically the default mux socket under the runtime directory.

`--prefer-mux` skips the GUI lookup, but it does not override an explicitly
set `$WAKTERM_UNIX_SOCKET`.

This means that a plain local `wakterm cli ...` invocation may target a running
GUI by default. If you specifically want the standalone mux server, use
`--prefer-mux` or set `$WAKTERM_UNIX_SOCKET` to that server's socket path.

# Targeting Panes

Various subcommands target panes via a (typically optional) `--pane-id` argument.

The following rules are used to determine a pane if `--pane-id` is not specified:

* If the `$WAKTERM_PANE` environment variable is set, it will be used
* The list of clients is retrieved and sorted by the most recently interacted
  session. The focused pane id from that session is used

See also: [wakterm cli list](list.md)

# Available Subcommands
