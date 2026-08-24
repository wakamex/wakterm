# `wakterm cli restore-layout`

Restores a saved multiplexer layout from a JSON file.

Reconstructs the windows, tabs, split trees, working directories, tab titles, active tab selection, active pane selection, and zoom state previously captured by `wakterm cli save-layout`.

Default file path is `~/.config/wakterm/layout.json`.

## Synopsis

```console
{% include "../../../generated/cli-help/cmd-synopsis-wakterm-cli-restore-layout--help.txt" %}
```

## Examples

```sh
# Restore from default layout file
wakterm cli restore-layout

# Restore from a specific file
wakterm cli restore-layout my-layout.json
```

See also: [wakterm cli save-layout](save-layout.md).
