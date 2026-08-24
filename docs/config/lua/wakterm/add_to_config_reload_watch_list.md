---
title: wakterm.add_to_config_reload_watch_list
tags:
 - reload
---

# wakterm.add_to_config_reload_watch_list(path)

Adds `path` to the list of files that are watched for config changes.
If [automatically_reload_config](../config/automatically_reload_config.md)
is enabled, then the config will be reloaded when any of the files
that have been added to the watch list have changed.

This function is now called implicitly when you `require` a lua file.
