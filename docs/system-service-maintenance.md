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
2. Finish or abandon any agent turn whose in-flight work matters. Wakterm does
   not currently restore provider processes or provider sessions. The operator
   restarts any desired agent harnesses after the mux migration.
3. Save the Wakterm layout and record the names and working directories of any
   agent harnesses that should be recreated.
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
codec, and the complete Agent API capability set. Point stopped Python
Panetone at `/run/wakterm/sock` and start it. Reconnect Wakterm and manually
restart only the agent harnesses that are still wanted. The Rust Panetone
cutover remains a separate maintenance operation.

## Production helper

The reviewed helper builds the exact pinned candidate, takes the maintenance
backup, performs the service and socket transition, verifies the Agent API,
and restarts Panetone. It does not attempt to restore agent processes or
provider sessions.

Run it from an independent SSH or console shell, never from a Wakterm pane:

```sh
cd /code/wakterm
./promote-system-service.sh --check
./promote-system-service.sh --apply
```

The apply command prints the persistent backup directory. Keep it for the
rollback command:

```sh
./promote-system-service.sh --rollback /var/tmp/wakterm-system-backup-TIMESTAMP
```

If verification stops the migration after the system service was installed,
fix the reported condition and continue without reinstalling or rolling back:

```sh
./promote-system-service.sh --resume /var/tmp/wakterm-system-backup-TIMESTAMP
```

The helper also installs a condition on the disabled user mux service so a
remaining user-unit dependency cannot start a second mux. Rollback restores
the previous condition state.

Do not use `deploy.sh --restart` for this migration. It targets the existing
development deployment path rather than the reviewed system-service boundary.

## Rollback

Keep Panetone stopped. Stop the system Wakterm service and confirm that its mux
process exited. Restore the separate runtime and database backup before opening
it with the previous binary. Restore installed system artifacts with:

```sh
sudo /bin/bash ./install-system-service.sh \
  --backup-dir /var/tmp/wakterm-system-backup-YYYYMMDD \
  --rollback
```

The installer rollback command itself does not start either service. The
production helper restores and starts the previous user mux and Panetone after
verifying the old socket. Reconnect Wakterm and manually restart any desired
agent harnesses.

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
