# Windows `connect` Hang When WSL Domains Auto-Enumerate

## Summary

On Windows, `wezterm connect ...` can appear to hang before normal domain attach
when WSL domain auto-enumeration blocks.

This is separate from mux focus/resize behavior issues (for example, #6885).

## Observed Behavior

- `wezterm.exe connect ...` or `wezterm-gui.exe connect ...` appears to hang.
- `wezterm-gui` process exists, but no visible window appears.
- Logs may only show early config reload lines.
- In this state, domain validation can be delayed or appear non-responsive.

## Reproduction Notes

1. On Windows, run:
   - `wezterm.exe --config-file "$HOME\.wezterm.lua" connect "__NO_SUCH_DOMAIN__"`
2. If startup is blocked before domain resolution, this may not fail immediately.
3. Add `config.wsl_domains = {}` and retry.
4. The invalid domain then fails immediately, indicating startup is no longer blocked.

## Workaround

Set this in Windows config:

```lua
config.wsl_domains = {}
```

It bypasses default WSL domain discovery and avoids the startup stall path.

## Suspected Root Cause

`Config::wsl_domains()` falls back to `WslDomain::default_domains()`, which runs:

- `wsl.exe -l -v`

This call is synchronous in startup domain setup. If it blocks on a given system,
`connect` can appear hung before normal attach behavior.

Relevant code paths:

- `config/src/config.rs` (`Config::wsl_domains`)
- `config/src/wsl.rs` (`WslDistro::load_distro_list`, `wsl.exe -l -v`)
- `wezterm-mux-server-impl/src/lib.rs` (`update_mux_domains_impl`)

## Potential Fix Directions

- Add timeout/guard rails around WSL distro enumeration.
- Defer WSL domain discovery off the startup-critical path.
- Improve logs/UI diagnostics when startup is blocked before domain attach.
- Cache last known WSL domain list and refresh asynchronously.
