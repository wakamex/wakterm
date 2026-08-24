# `wakterm cli save-layout`

Saves the current multiplexer layout to a JSON file or stdout.

The saved layout preserves the full split tree, proportional split sizes, working directories, tab titles, active tabs, active panes, and zoom state.

Default file path is `~/.config/wakterm/layout.json`.

## Synopsis

```console
{% include "../../../generated/cli-help/cmd-synopsis-wakterm-cli-save-layout--help.txt" %}
```

## Examples

```sh
# Save to default layout file
wakterm cli save-layout

# Save to a specific file
wakterm cli save-layout my-layout.json

# Output JSON to stdout
wakterm cli save-layout --stdout
```

See also: [wakterm cli restore-layout](restore-layout.md).
