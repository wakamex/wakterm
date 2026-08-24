# `AttachDomain(domain_name)`

Attempts to attach the named multiplexing domain.  The name can be any of the
names used in your `ssh_domains`, `unix_domains` or `tls_clients`
configurations.

Attaching a domain will attempt to import the windows, tabs and panes from the
remote system into those of the local GUI.

If there are no remote panes in that domain, wakterm will spawn a default
program into it.

This action is not bound to any keys by default. The [Launcher Menu](../../../guides/configuration/launch.md#the-launcher-menu)
(default: right click on the new tab `+` button in the tab bar) will synthesize
entries with this action.

The example below shows how to bind a key to trigger attaching to an ssh domain:

```lua
config.ssh_domains = {
  {
    name = 'devhost',
    remote_address = 'devhost.example.com',
  },
}
config.keys = {
  {
    key = 'U',
    mods = 'CTRL|SHIFT',
    action = wakterm.action.AttachDomain 'devhost',
  },
}
```

See also: [DetachDomain](DetachDomain.md)
