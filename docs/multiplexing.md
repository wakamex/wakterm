## Multiplexing

Multiplexing lets terminal panes, tabs, and windows live in persistent server domains rather than being tied to a single desktop GUI window.

Out of the box, wakterm manages local tabs and windows. You can also connect to local or remote domains in a way that feels similar to tmux or screen, with native mouse, clipboard, scrollback, and GUI integration.

Multiplexing in wakterm is organized around multiplexing domains. A domain is a distinct set of windows and tabs. When wakterm starts up it creates a default local domain to manage windows and tabs in the UI, but it can also start or connect to additional domains.

The word local is always relative to the mux host, not necessarily to the human or client machine using wakterm. For a normal desktop GUI, local means processes on the same machine as the GUI. If a Windows GUI connects to a Linux mux over SSHMUX:, that remote Linux mux's local domain means processes running on Linux, not on Windows.

Separate clients only share panes, tabs, and windows when they attach to the same explicit domain, such as a unix domain, SSHMUX: domain, or TLS domain. When multiple clients attach to the same domain, the domain contents are shared, while each client keeps its own view state such as local focus and active tab selection.

### Key multiplexer capabilities

- Local layouts can be saved and restored with `wakterm cli save-layout` and `wakterm cli restore-layout`.
- Automatic session persistence coalesces recovery-relevant layout changes into atomic snapshots and restores split trees, working directories, tab titles, active-tab selection, and restorable agent sessions across server restarts and reboot. If the newest snapshot cannot be read, Wakterm loads the previous valid generation.
- Multi-client synchronization uses authoritative server state for windows, tabs, panes, tab ordering, and parked state.
- Resize origin tracking suppresses client self-echoes without timing heuristics, avoiding resize loops and flicker during window adjustments.
- Reconnecting clients reconcile remote tab order to prevent order drift.
- Hidden tabs (`ParkCurrentTab` / `Ctrl-Shift-S` on Linux/Windows, `Cmd-Shift-S` on macOS) stay alive in the multiplexer without cluttering the visible tab strip.
- Agent harness panes (Agy, Claude, Codex, Gemini, OpenCode) live inside the multiplexer model with automatic adoption and background session tracking.

## Systemd background service and reboot persistence

On Linux systems, wakterm can run its multiplexer as a managed systemd service so your sessions survive desktop logout and system reboot.

### User service

For desktop users, install the user service:

```sh
./install-user-service.sh
```

This installs `~/.config/systemd/user/wakterm-mux-server.service` and enables it.

To ensure your user service continues running after you log out of the desktop or SSH session, enable user lingering:

```sh
loginctl enable-linger $USER
```

Manage the user service with systemctl:

```sh
systemctl --user status wakterm-mux-server.service
systemctl --user restart wakterm-mux-server.service
```

### System service

For shared multi-user systems or dedicated hosts, the system service script performs a dry-run check by default. Pass `--apply` to perform the actual installation:

```sh
# Dry-run check
./install-system-service.sh

# Apply installation (requires root)
sudo ./install-system-service.sh --apply
```

This installs the service under `/etc/systemd/system/wakterm-mux-server.service`.

To promote an existing user service and migrate live socket and session state to the system service, use:

```sh
# Check readiness
./promote-system-service.sh --check

# Apply migration
sudo ./promote-system-service.sh --apply
```

## SSH Domains

wakterm supports regular ad-hoc ssh connections as well as persistent multiplexed sessions that run a wakterm daemon on the remote side over SSH.

Rule of thumb:

- `wakterm ssh host` or `SSH:host` is a plain SSH connection. It is not a persistent mux session.
- `wakterm connect host` for a configured SSH domain, or `wakterm connect SSHMUX:host`, attaches to a shared persistent remote mux domain.

A connection to a remote wakterm multiplexer over SSH is referred to as an SSH domain. A compatible version of wakterm must be installed on the remote system to use SSH domains. SSH domains are supported on all systems via libssh2.

To configure an SSH domain, add a block like this to your `wakterm.lua` file:

```lua
config.ssh_domains = {
  {
    -- This name identifies the domain
    name = 'my.server',
    -- The hostname or address to connect to. Will be used to match settings
    -- from your ssh config file
    remote_address = '192.168.1.1',
    -- The username to use on the remote host
    username = 'wez',
  },
}
```

See [SshDomain](reference/lua/SshDomain.md) for more information on possible settings to use with SSH domains.

To connect to the system, run:

```console
$ wakterm connect my.server
```

This will launch an SSH session that connects to the specified address and may pop up authentication dialogs (using SSH keys for authentication is strongly recommended). Once connected, it will attempt to spawn the wakterm multiplexer daemon on the remote host and connect to it via a unix domain socket.

SSH domains also auto-populate from your `~/.ssh/config` file. Each populated host will have both a plain SSH and a multiplexing SSH domain. Plain SSH hosts are defined with a `SSH:` prefix to their name and multiplexing hosts are defined with a prefix `SSHMUX:`. For example, to connect to a host named `my.server` in your `~/.ssh/config` using a multiplexing domain, run:

```console
$ wakterm connect SSHMUX:my.server
# or to spawn into a new tab in an existing GUI instance:
$ wakterm cli spawn --domain-name SSHMUX:my.server
```

To customize this functionality, see [wakterm.default_ssh_domains()](reference/lua/wakterm/default_ssh_domains.md).

## Unix Domains

A connection to a multiplexer made via a unix socket is referred to as a unix domain. Unix domains are supported on all systems, including Windows, and are a way to connect the native win32 GUI into the Windows Subsystem for Linux (WSL).

The bare minimum configuration to enable a unix domain is this, which will spawn a server if needed and connect the GUI to it automatically when wakterm is launched:

```lua
config.unix_domains = {
  {
    name = 'unix',
  },
}

-- Connect to unix domain automatically on startup:
config.default_gui_startup_args = { 'connect', 'unix' }
```

If you prefer to connect manually, omit the `default_gui_startup_args` setting and run:

```console
$ wakterm connect unix
```

The possible configuration values are:

```lua
config.unix_domains = {
  {
    -- The name; must be unique amongst all domains
    name = 'unix',

    -- The path to the socket. If unspecified, a default value is computed.
    -- socket_path = "/some/path",

    -- If true, do not attempt to start this server if connection fails.
    -- no_serve_automatically = false,

    -- If true, bypass checking for secure ownership of the socket_path.
    -- Useful when running the server inside a container with the socket on a host volume.
    -- skip_permissions_check = false,
  },
}
```

### Proxy command

You can specify a `proxy_command` that will be used in place of making a direct unix connection. When `proxy_command` is specified, it will be used instead of `socket_path`:

```lua
config.unix_domains = {
  {
    name = 'unix',
    proxy_command = { 'nc', '-U', '/Users/wez/.local/share/wakterm/sock' },
  },
}
```

### Predictive local echo

You can specify the round-trip latency threshold for enabling predictive local echo using `local_echo_threshold_ms`. If the measured round-trip latency between the wakterm client and the server exceeds the specified threshold, the client predicts the server's response to key events and echoes the result locally:

```lua
config.unix_domains = {
  {
    name = 'unix',
    local_echo_threshold_ms = 10,
  },
}
```

### Connecting into Windows Subsystem for Linux

For WSL 1, you can share a Unix domain socket directly across the host filesystem. Inside your WSL instance, configure `wakterm.lua` with:

```lua
config.unix_domains = {
  {
    name = 'wsl',
    socket_path = '/mnt/c/Users/USERNAME/.local/share/wakterm/sock',
    skip_permissions_check = true,
  },
}
```

In the host Windows configuration, configure the domain to spawn the WSL server:

```lua
config.unix_domains = {
  {
    name = 'wsl',
    serve_command = { 'wsl', 'wakterm-mux-server', '--daemonize' },
  },
}
config.default_gui_startup_args = { 'connect', 'wsl' }
```

To manually connect into your WSL instance:

```console
$ wakterm connect wsl
```

For WSL 2, direct AF_UNIX socket interop across the VM boundary is not supported by WSL. Use `proxy_command` with a network bridge utility such as socat or netcat to connect to the multiplexer socket.

## TLS Domains

A connection to a multiplexer made via a TLS encrypted TCP connection is referred to as a TLS Domain.

wakterm can bootstrap a TLS session by performing an initial connection via SSH to start the wakterm multiplexer on the remote host and securely obtain a key. Once bootstrapped, the client uses a TLS protected TCP connection to communicate with the server.

### Configuring the client

```lua
config.tls_clients = {
  {
    name = 'server.name',
    remote_address = 'server.hostname:8080',
    bootstrap_via_ssh = 'server.hostname',
  },
}
```

See [TlsDomainClient](reference/lua/TlsDomainClient.md) for more information on possible settings.

### Configuring the server

```lua
config.tls_servers = {
  {
    bind_address = 'server.hostname:8080',
  },
}
```

See [TlsDomainServer](reference/lua/TlsDomainServer.md) for more information on possible settings.

### Connecting

```console
$ wakterm connect server.name
```
