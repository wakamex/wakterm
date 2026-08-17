# System mux service maintenance

This procedure moves Wakterm from the per-user mux service to one system
service running as the same user. It does not restore provider sessions and it
does not combine the Wakterm maintenance with a Panetone implementation
cutover.

The system service owns one socket at `/run/wakterm/sock`. Stop Python
Panetone before the mux transition, then change its configured Wakterm socket
to that path while it is stopped. Do not add a second compatibility listener
unless a discovered consumer cannot be reconfigured.

## Before the maintenance

1. Record the exact candidate commit, release versions, binary hashes, active
   unit, mux PID, socket inode, Agent API catalog, event head, and effective
   configuration.
2. Record a tested exact provider-session resume command and working directory
   for every required agent. A route without that recovery evidence blocks the
   maintenance.
3. Bring agents to observed idle boundaries and save the Wakterm layout.
4. Stop Panetone. Confirm that it has no active worker or pending control
   request.
5. Stop the user Wakterm service once. Confirm that no mux process remains.
6. Back up the user unit, installed binaries, layout, runtime session file, and
   durable Agent API database. Use SQLite's online backup command if the
   database might still be open, and run `PRAGMA quick_check` on the copy.

The installer checks the release and unit without changing the host by
default:

```sh
./install-system-service.sh --source /path/to/exact/release
```

## Install without starting

Use a new persistent backup directory. The installer snapshots any replaced
system files, installs the three exact binaries plus the unit and mux config,
restores SELinux labels, and reloads systemd. It does not enable or start the
mux.

```sh
sudo /bin/bash ./install-system-service.sh \
  --source /path/to/exact/release \
  --backup-dir /var/tmp/wakterm-system-backup-YYYYMMDD \
  --apply
```

Confirm that `/usr/local/bin/wakterm-mux-server` has the expected version and
`bin_t` label. Review `systemctl cat wakterm-mux-server.service`, then disable
the old user unit so the two managers cannot race. Enable the system unit only
at the controlled start boundary.

Start the system unit once. Verify exactly one mux process, one socket inode at
`/run/wakterm/sock`, restrictive ownership and permissions, the expected wire
codec, and the complete Agent API capability set. Restore the terminal layout,
then run each previously tested exact provider-session resume command in its
recorded pane and working directory. Verify the provider session identity, not
only the pane title or process name.

Only after every required agent is restored should stopped Python Panetone be
pointed at `/run/wakterm/sock` and started. The Rust Panetone cutover remains a
separate maintenance operation.

## Rollback

Keep Panetone stopped. Stop the system Wakterm service and confirm that its mux
process exited. Restore the separate runtime and database backup before opening
it with the previous binary. Restore installed system artifacts with:

```sh
sudo /bin/bash ./install-system-service.sh \
  --backup-dir /var/tmp/wakterm-system-backup-YYYYMMDD \
  --rollback
```

The rollback command does not start either service. Re-enable and start the
previous user service, verify its old socket and codec, restore the layout, and
manually resume every exact provider session again. Rollback is not complete
until every required session identity and Panetone route is directly verified.

## Isolated rehearsal

Use `--root` to exercise file installation and rollback without root access,
systemd changes, production sockets, or production state:

```sh
./install-system-service.sh \
  --source /path/to/exact/release \
  --root /path/to/empty/staging-root \
  --backup-dir /path/to/staging-backup \
  --apply

./install-system-service.sh \
  --root /path/to/empty/staging-root \
  --backup-dir /path/to/staging-backup \
  --rollback
```
