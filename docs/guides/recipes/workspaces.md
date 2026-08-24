# Workspaces / Sessions

If you are familiar with tmux then you may have used its session functionality
to manage separate groups of related tmux windows (which map to tabs in wakterm).

wakterm doesn't have an exact match to tmux sessions, but does have a similar
concept known as *Workspaces*.

Every [MuxWindow](../../reference/lua/mux-window/index.md) is associated with a
workspace, which is just a label.

The wakterm GUI is focused on the active workspace, which means that it will
present a GUI window for each MuxWindow that is present in that workspace.

You can spawn windows into differently named workspaces and they won't become
visible until you set the active workspace to that name.

When switching the active workspace, wakterm will swap the contents of the
GUI windows with the MuxWindows that belong to the now-focused workspace.

The following key assignments are helpful when working with workspaces:

* [SwitchToWorkspace](../../reference/lua/keyassignment/SwitchToWorkspace.md)
* [SwitchWorkspaceRelative](../../reference/lua/keyassignment/SwitchWorkspaceRelative.md)
* Various key assignments or functions that spawn windows also allow specifying
  the workspace name to be used
* [ShowLauncher](../../reference/lua/keyassignment/ShowLauncher.md) and
  [ShowLauncherArgs](../../reference/lua/keyassignment/ShowLauncherArgs.md) will list
  the current set of workspaces and allow switching between them

You can pre-define a layout of windows/tabs/panes in specific workspaces by
using these events:

* [gui-startup](../../reference/lua/gui-events/gui-startup.md)
* [mux-startup](../../reference/lua/mux-events/mux-startup.md)

