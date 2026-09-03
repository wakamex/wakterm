# `wakterm show-keys`

Prints the complete set of key assignments based on your config file.

The command shows each key table as well as the set of mouse bindings.

Use `--json` to export the built-in command metadata and default keys as structured data. The export does not include overrides from your config. Pass `--platform linux`, `--platform macos`, or `--platform windows` to inspect a platform other than the current one.

```console
wakterm show-keys --json --platform macos
```

A truncated example of the output is shown below.

```
Default key table
-----------------

        CTRL                 Tab                ->   ActivateTabRelative(1)
        SHIFT | CTRL         Tab                ->   ActivateTabRelative(-1)
        ...

Key Table: copy_mode
--------------------

                Tab          ->   CopyMode(MoveForwardWord)
        SHIFT   Tab          ->   CopyMode(MoveBackwardWord)
        SHIFT   $            ->   CopyMode(MoveToEndOfLineContent)
        ...

Key Table: search_mode
----------------------

               Enter       ->   CopyMode(PriorMatch)
               Escape      ->   CopyMode(Close)
        CTRL   n           ->   CopyMode(NextMatch)
        ...

Mouse
-----

                       Down { streak: 1, button: Left }     ->   SelectTextAtMouseCursor(Cell)
        SHIFT          Down { streak: 1, button: Left }     ->   ExtendSelectionToMouseCursor(None)
        ALT            Down { streak: 1, button: Left }     ->   SelectTextAtMouseCursor(Block)
        ...
```

## Synopsis

```console
{% include "../../generated/cli-help/cmd-synopsis-wakterm-show-keys--help.txt" %}
```
