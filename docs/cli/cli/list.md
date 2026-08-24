# `wakterm cli list`

Lists the set of windows, tabs and panes that are being managed.

The default output is tabular:

```
$ wakterm cli list
WORKSPACE TAB  PANE                         SIZE WINID TABID PANEID CWD
default   main wakterm cli list -- wak@foo:~ 80x24     0     0      0 file://foo/home/wak/
```

Each row describes a pane. The meaning of the fields are:

- WORKSPACE - the workspace that the pane is associated with
- TAB - the tab title
- PANE - the pane title
- SIZE - the dimensions of the pane, measured in terminal cell columns x rows
- WINID - the window id of the window that contains the pane
- TABID - the tab id of the tab that contains the pane
- PANEID - the pane id
- CWD - the current working directory associated with the pane

Long PANE and TAB values are truncated in the default table output to keep the later columns aligned. Use `--format json` for full values.

You may request JSON output:

```json
$ wakterm cli list --format json
[
  {
    "window_id": 0,
    "tab_id": 0,
    "pane_id": 0,
    "workspace": "default",
    "size": {
      "rows": 24,
      "cols": 80,
      "pixel_width": 0,
      "pixel_height": 0,
      "dpi": 0
    },
    "title": "zsh",
    "tab_title": "",
    "effective_title": "wakterm",
    "window_title": "wakterm",
    "cwd": "file:///code/wakterm",
    "cursor_x": 0,
    "cursor_y": 0,
    "cursor_visibility": "Visible",
    "left_col": 0,
    "top_row": 0,
    "is_active": true,
    "is_zoomed": false,
    "tty_name": "/dev/pts/1"
  }
]
```

## Synopsis

```console
{% include "../../examples/cmd-synopsis-wakterm-cli-list--help.txt" %}
```
