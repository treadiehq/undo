# Local storage, limits, and operations

## Know what stays local

Undo is a local, single-user tool. It requires no account and has no cloud
service. Filesystem history, Run metadata, Recoveries, and backups stay on the
machine.

The two documented outbound network exceptions are `undo update` and the
installer. `undo update` queries GitHub Releases, downloads the platform
archive and `SHA256SUMS`, verifies SHA-256, and installs the update. The
installer also downloads from the network.

The optional `undo ui` command serves the local web interface from a listener
bound to 127.0.0.1 only, for as long as the command runs. Access requires the
random per-session token printed with the URL. Nothing is reachable from other
machines and no history leaves yours; see
[SECURITY.md](../SECURITY.md) for the full model.

## Know what Undo can capture

Undo snapshots regular files up to and including 100 MiB
(`100 * 1024 * 1024` bytes). Files over that limit, unreadable files, and
non-UTF-8 paths are skipped instead of being captured incompletely or keyed
incorrectly. Undo cannot recover bytes it did not capture.

Binary files within the limit are stored and restored byte-for-byte. Text diff
is unavailable when a NUL byte appears in the first 8 KiB.

The initial scan refuses a tree with more than 50,000 non-ignored files unless
`undo start --force` is used.

## Know how long recovery remains possible

Defaults are seven days of history and a 1 GiB cap for the complete
`~/.undo/` store. Configure global defaults in `~/.undo/config.toml` and override
them per project in `<project>/.undorc`:

```toml
retention_days = 30
max_size_mb = 2048
```

Cleanup runs at recorder startup, hourly while recording, and through
`undo prune`. It removes:

1. filesystem event rows older than the retention cutoff;
2. leaked snapshot temp files older than one hour;
3. snapshots no longer referenced by retained events, live file state, or an
   unexpired planned Recovery;
4. the current project's backups older than its retention cutoff.

If the store remains over the size cap, Undo removes oldest unreferenced
snapshots across projects. It does not delete snapshots still referenced by live
history merely to satisfy the cap; it prints a warning instead.

Runs and checkpoints can remain visible after the filesystem events and
snapshots needed to recover them have expired. A checkpoint records a boundary;
it does not duplicate or pin file contents beyond retention. Retention therefore
bounds actual recoverability. A planned Recovery's target snapshots are retained
until its 24-hour expiration.

## Check platform and filesystem support

Undo supports macOS through FSEvents and Linux through inotify. It relies on Unix
file modes and `flock`; Windows is not supported.

Local APFS/HFS+, ext4, btrfs, XFS, and ZFS filesystems normally provide the
events Undo needs. Network and FUSE filesystems vary: server-side writes may not
emit local notifications. Undo's startup/resume scan catches visible differences
when it runs, but that is not a guarantee of real-time capture.

Live-watcher metadata checks and reads use a five-second watchdog. If a watched
root disappears or becomes unhealthy, recording pauses; when it returns, Undo
reconciles before resuming. Restart the recorder after filesystem-level
rollbacks or mount changes if event delivery is uncertain.

## Choose what Undo records

Undo reads `.gitignore` and `.undoignore` from the project root. Nested
`.gitignore` files are not loaded. `.undoignore` is applied after the root
`.gitignore`, so it can override earlier matches.

Built-in ignored names include:

- `.git`, `.undo`, `.backtrack`, `.ssh`;
- `node_modules`, `target`, `.next`, `dist`, `build`, `__pycache__`;
- `.DS_Store`, `.idea`, `.vscode`;
- `.env` and every `.env.*` variant;
- files ending in `.pem`, `.key`, `.p12`, `.pfx`, or `.keystore`.

Add project-specific exclusions with gitignore syntax:

```text
*.log
tmp/
*.sqlite
```

A negation in `.undoignore` can opt a built-in path back in:

```text
!build/
!.env
```

Opting secrets back in stores their contents unencrypted under `~/.undo/`.

## Record more than one project

Each project has its own recorder PID file, snapshot directory, event stream,
and optional `.undorc`. Multiple non-overlapping projects can record at once:

```bash
cd ~/project-a && undo start &
cd ~/project-b && undo start &
undo stop --all
```

Undo rejects overlapping watched roots by default because parent and child
recorders would duplicate events. `--force` bypasses that guard.

Recording concurrency is separate from Run concurrency. Multiple projects can
each have an active Run, but one project allows only one active Run because
filesystem events cannot reliably attribute concurrent actors.

## Understand recording and restore safeguards

Without `--force`, `undo start` refuses:

- root/sudo execution;
- root- or system-account-owned project roots;
- projects over the 50,000-file startup limit;
- roots overlapping another active recorder.

Restore paths must stay inside the watched project. Undo refuses direct restore
through a symlink target, creates backups before overwriting/deleting existing
files, and uses an atomic rename for each written file. Multi-file application is
preflighted but is not one filesystem transaction.

## Find Undo's local data

Undo stores data under `~/.undo/`:

- `database.db`, `database.db-wal`, and `database.db-shm` — SQLite history and
  metadata.
- `snapshots/<project_id>/<sha256>.gz` — compressed, content-addressed file
  versions.
- `pids/<project-hash>.pid` — recorder liveness and project root.
- `backups/<project_id>/` — project-scoped pre-mutation safety copies. Legacy
  flat backup files remain untouched because their project ownership cannot be
  reconstructed safely.
- `config.toml` — optional global retention configuration.
- `undo.log` and `undo.log.1` — recorder logs, rotated at 5 MB.
- `undo.log.lock` — owner-only advisory lock coordinating shared-log writes and
  rotation across project recorders.

The top-level and snapshot directories are owner-only (`0700`). Database files,
snapshots, PID files, backups, and logs are restricted to the owner (`0600`)
where Undo creates or tightens them. Snapshots are gzip-compressed, not
encrypted; the user, root, and anyone with access to a home-directory backup can
read them.

Identical content is stored once per project by SHA-256. Live snapshots are
written to a unique temporary file, synced, and renamed before the referencing
event commits. Initial-scan snapshots skip the per-file durability sync because
their source files still exist and can be scanned again.

## Understand the database model

This section describes low-level storage details. The database includes:

- watched projects, filesystem events, and current file state;
- Runs in the compatibility-named `sessions` table plus stable event membership;
- checkpoints and explicit Run intents;
- persisted Recoveries and their per-path expected/target hashes;
- integration idempotency keys and stored JSON responses.

The current schema version is 3. Opening the database applies additive migration
for Run metadata, intent and Recovery tables, integration events, and event-ID
checkpoint fields. The checkpoint table is rebuilt when necessary to support
names scoped per Run while retaining legacy project-level rows. Existing history
is preserved. A binary refuses to open a database whose schema version is newer
than it supports.

## Troubleshoot missing or unusable history

Start with:

```bash
undo status
```

`status` checks recorder liveness, reports storage, and deeply decompresses every
referenced snapshot to distinguish missing from corrupt data. The startup check
only verifies snapshot existence.

Then inspect `~/.undo/undo.log` for startup failures, pause/resume notices,
watcher errors, and cleanup summaries.

If changes are missing:

- confirm the matching project recorder is running;
- check root `.gitignore`, `.undoignore`, and built-in ignores;
- confirm the path is UTF-8, readable, and no larger than 100 MiB
  (`100 * 1024 * 1024` bytes);
- account for network/FUSE event limitations;
- restart recording to run a reconciliation scan.

If restore reports no saved version, the path may have been ignored or skipped,
its event may have aged out, or its snapshot may be missing/corrupt. Undo cannot
reconstruct bytes it never captured or no longer retains.
