# Undo Detailed Docs

Commands, settings, storage, and troubleshooting for Undo.

## Table of Contents

- [Installation](#installation)
- [Commands](#commands)
- [How It Works](#how-it-works)
- [Configuration](#configuration)
- [Cleanup Rules](#cleanup-rules)
- [Ignored Paths](#ignored-paths)
- [Safety Guards](#safety-guards)
- [Data Storage](#data-storage)
- [Multi-Project Support](#multi-project-support)
- [Platform Support](#platform-support)
- [Troubleshooting](#troubleshooting)

---

## Installation

### Quick install (macOS / Linux)

```bash
curl -fsSL https://useundo.co/install.sh | bash
```

### Download binaries

Prebuilt binaries are available for macOS (ARM + Intel) and Linux (x86_64) on the
[Releases page](https://github.com/treadiehq/undo/releases).

### Build from source

Requires Rust 1.85+ (the project uses the 2024 edition).

```bash
git clone https://github.com/treadiehq/undo.git
cd undo
cargo build --release
cp target/release/undo /usr/local/bin/
```

See [CONTRIBUTING.md](../CONTRIBUTING.md) for the development workflow (tests, lints, project layout).

---

## Commands

### `undo start`

Start saving history for the current folder.

```bash
cd your-project/
undo start
```

```
undo: give your files an undo button
Watching: /Users/me/my-project
Recording changes...
```

Undo runs in the foreground by default. Use `Ctrl+C` to stop it, or run it in the background:

```bash
undo start &
```

You can run Undo in several projects at the same time. Each project is tracked separately.

Use `--force` to skip startup safety checks, such as ownership and file-count checks:

```bash
undo start --force
```

### `undo what-changed <duration>`

Show what changed recently.

```bash
undo what-changed 5m
undo what-changed 2h
undo what-changed 1d
```

```
Changes in last 5m

MODIFIED
  - src/server.rs
  - Cargo.toml

CREATED
  - logs/debug.log

DELETED
  - assets/logo.png
```

### `undo timeline`

Show recent file activity in order.

```bash
undo timeline
undo timeline --limit 50
```

```
undo — recent activity

12:31 MODIFIED src/server.rs
12:30 CREATED logs/debug.log
12:29 DELETED assets/logo.png
12:28 RENAMED src/app_old.rs -> src/app.rs
```

### `undo diff <path>`

Compare the current file with the latest saved version.

```bash
undo diff src/server.rs
```

### `undo restore <path> <duration>`

Bring back an older version of a file.

```bash
undo restore src/server.rs 10m
```

```
Backup of current file saved to /Users/me/.undo/backups/server.rs_a1b2c3d4_1713200000000000000.bak
Restored src/server.rs from the version saved 9 minute(s) ago.
```

Before Undo overwrites a file, it saves a backup in `~/.undo/backups/`.

### `undo prune`

Delete saved history that is older than your cleanup rules.

```bash
undo prune
undo prune --keep 30d
undo prune --dry-run
```

```
Deleted 342 events, 89 saved copies, 3 backups.
Freed 12.4 MB. Current storage: 45.2 MB.
```

| Flag | Description |
|------|-------------|
| `--dry-run` | Preview what would be deleted without deleting |
| `--keep <duration>` | Keep this much history for this cleanup run (e.g. `30d`, `12h`) |

Cleanup also runs when Undo starts and then once an hour while it is running.

### `undo status`

Show whether Undo is running, how much history it has saved, how much space it uses, and whether referenced snapshots are readable.

```bash
undo status
```

```
undo — status

Project:   /Users/me/my-project
Status:    running (PID 12345)
Database:  /Users/me/.undo/database.db (24.0 KB)
Events:    142
Saved:     87 versions
Integrity: OK (87 verified)
Keep:      7 days, 1.0 GB max
Storage:   45.2 MB (saved: 38.1 MB, backups: 5.8 MB, db: 1.3 MB)
Log:       /Users/me/.undo/undo.log
```

The `Integrity:` line runs a deep verification of the snapshots referenced by retained history for this project. Each one is decompressed and checked, the expensive read-per-snapshot pass the daemon deliberately skips at startup. A clean store reads `OK (N verified)`. If anything is wrong it reads `X unreadable (M missing, C corrupt, of N checked)`, distinguishing snapshots whose file is gone from snapshots that fail to decompress.

### `undo stop`

Stop saving history.

```bash
undo stop          # stop Undo for this project
undo stop --all    # stop Undo in every watched folder
```

### `undo update`

Update Undo to the latest release.

```bash
undo update
```

Undo downloads the release for your platform, checks it against the published `SHA256SUMS`, then installs it with a copy-and-rename step. If the checksum is missing or does not match, the update stops without touching your current install. Downgrades are also refused — if the latest GitHub release is older than your current version, Undo leaves your install as-is.

---

## How It Works

Undo runs as a small background process that watches your project for file changes. It uses FSEvents on macOS and inotify on Linux.

When files change, Undo:

1. Reads the file
2. Checks whether the content changed
3. Saves a compressed copy when there is a new version
4. Records the change in a local SQLite database

When Undo starts, it scans once to catch changes that happened while it was stopped.

If the watched folder disappears, such as when a remote mount disconnects, Undo pauses. When the folder comes back, Undo scans again and resumes.

---

## Configuration

### Global config: `~/.undo/config.toml`

```toml
retention_days = 7
max_size_mb = 1024
```

### Per-project config: `.undorc` in the project root

```toml
retention_days = 30
```

Project settings override global settings. If neither file exists, Undo uses its built-in defaults.

| Key | Default | Description |
|-----|---------|-------------|
| `retention_days` | `7` | Days of history to keep |
| `max_size_mb` | `1024` | Maximum total size of `~/.undo/` in MB |

---

## Cleanup Rules

Undo deletes old history automatically so `~/.undo/` does not grow forever.

### What gets cleaned up

Undo cleans up three things, in this order:

1. **Events** — old database records
2. **Unused saved copies** — `.gz` files no remaining event points to
3. **Backups** — old files in `~/.undo/backups/`

If `~/.undo/` is still over `max_size_mb`, Undo deletes the oldest saved copies until it is under the limit.

### When cleanup runs

- When Undo starts
- Every hour while Undo is running
- Manually with `undo prune`

### Dry run

```bash
undo prune --dry-run
```

Shows what would be deleted without deleting it.

### Override the cleanup window

```bash
undo prune --keep 30d
```

Uses a different cleanup window for this one run.

---

## Ignored Paths

Undo automatically skips noisy and sensitive paths:

- `.git/`, `.undo/`
- `node_modules/`, `__pycache__/`
- `target/`, `dist/`, `build/`, `.next/`
- `.DS_Store`, `.idea/`, `.vscode/`
- `.env` and any `.env.*` variant (e.g. `.env.local`, `.env.production`, `.env.staging`), `.ssh/`
- `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.keystore`

### Custom ignore patterns

Add a `.undoignore` file to your project root for extra ignore rules. Use one pattern per line, with the same syntax as `.gitignore`:

```
*.log
tmp/
*.sqlite
```

Undo also follows your root `.gitignore`. `.undoignore` wins when both files match a path. Only the root `.gitignore` is loaded for now; nested `.gitignore` files are not supported. Add those patterns to `.undoignore` if you need them.

### Overriding the defaults

If a default-ignored path is actually something you want tracked, use a negation pattern in `.undoignore`:

```
!build/
!.env
!dist/
```

Negation patterns override the built-in ignore list. This is useful when a default like `build/` or `dist/` is source code in your project, or when you intentionally want to track `.env` changes.

### Large files

Files larger than **100 MB** appear in history, but Undo does not save their contents. This keeps disk usage under control.

---

## Safety Guards

Undo refuses to start when it detects a risky location:

| Guard | Why |
|-------|-----|
| **Root/sudo** | Running as root stores data where your normal user may not see it |
| **System directories** | Directories owned by root or system accounts (`/`, `/etc`, `/usr`, etc.) |
| **Oversized directories** | More than 50,000 files, which usually means the folder is too broad |

All guards can be overridden with `--force`.

### Overlapping directories

Undo will not start if another Undo process is already watching a parent or child folder. For example, if `/foo/bar` is already being watched, starting Undo on `/foo` would record some changes twice. Undo stops and points you to the conflicting process.

### Remote file clobbers (SCP, rsync, etc.)

Undo watches files, not apps. If `scp`, `rsync`, or another tool overwrites a file, Undo records it like any other change. The previous version can be restored with `undo restore`.

This makes Undo useful on shared development servers and deployment boxes. If a remote write clobbers a file, you can get it back.

One caveat: if the file is owned by root or has restricted permissions, Undo may not be able to read it. The change is still recorded, but the file contents are not saved.

---

## Data Storage

Undo stores all data locally at `~/.undo/`:

| Path | Purpose |
|------|---------|
| `database.db` | Change history and file state (SQLite) |
| `snapshots/<project_id>/` | Compressed saved copies, named by content |
| `pids/<hash>.pid` | Per-project process ID files |
| `backups/` | Safety backups created before restoring a file |
| `config.toml` | Global configuration (optional) |
| `undo.log` | Logs for errors, crashes, cleanup, and start/stop notices; rotates to `undo.log.1` at 5 MB |

Everything under `~/.undo/` is owner-only: the directory is `0700` and the database, saved copies, backups, PID files, and log are `0600`. Other users on the machine cannot read your saved file contents.

If two files have the same content, Undo stores that content once.

Saved copies are written to a `.tmp` file first, then renamed into place, so partial writes are not treated as valid history.

---

## Multi-Project Support

You can run Undo on multiple projects at the same time. Each project gets:

- Its own process ID file in `~/.undo/pids/`
- Its own saved-copy directory in `~/.undo/snapshots/<project_id>/`
- Its own events in the shared SQLite database
- Its own cleanup settings via `.undorc`

```bash
# Terminal 1
cd ~/project-a && undo start &

# Terminal 2
cd ~/project-b && undo start &

# Stop all
undo stop --all
```

---

## Platform Support

- macOS (FSEvents)
- Linux (inotify)

### Filesystem compatibility

| Filesystem | Status | Notes |
|------------|--------|-------|
| APFS, HFS+ (macOS) | Works | Full support via FSEvents |
| ext4, btrfs, xfs, zfs (Linux) | Works | Full support via inotify |
| NFS, SMB/CIFS | No events | inotify only fires for local changes; remote writes are invisible to Undo |
| sshfs, FUSE mounts | Varies | Depends on the FUSE implementation; some emit events, some don't |
| NTFS (via ntfs-3g) | Unreliable | FUSE-based on Linux, no native notifications |

For filesystems that do not send events, Undo will not see changes immediately. The startup scan still catches missed changes the next time Undo starts.

### ZFS rollbacks

If a ZFS rollback happens while Undo is running, Undo may see many change events or miss some changes, depending on how the rollback interacts with inotify. Restart Undo after a rollback so it can scan and get back in sync.

### Binary files

Binary files, such as images, compiled assets, and databases, are supported:

- **Save and restore** work on raw bytes
- **Events** are tracked the same as text files
- **Diff** detects binary content and prints "Binary file — text comparison not available"

---

## Troubleshooting

### Where to look first: the log

Undo writes to `~/.undo/undo.log`, which is shown in `undo status` as the `Log:` line. Errors, crashes, pause/resume notices, and cleanup summaries all go there. The log rotates to `undo.log.1` after 5 MB.

### "Undo is already saving history"

Undo is already running for this folder. Use `undo status` to see its process ID, or `undo stop` to stop it.

### "directory overlaps with an already-watched path"

Another Undo process is watching a parent or child folder, which would record some changes twice. Stop the other process, watch a different folder, or pass `--force` if duplicate events are acceptable.

### "refusing to run as root" / "owned by root" / "owned by a system account"

Undo hit a safety guard. Run it as your normal user from a folder you own, or pass `--force` if you understand the risk.

### "directory contains more than 50,000 files"

This usually means `undo start` was run somewhere too broad, such as your home folder or `/`. Watch a specific project folder, add ignore rules in `.undoignore`, or pass `--force`.

### A restore says "No saved versions found for this file"

Undo has no saved version for that path. This can happen if the file was ignored, was always larger than 100 MB, or its history was already cleaned up. `undo timeline` shows what Undo has recorded.

### Changes aren't being recorded

- Confirm Undo is running for this folder: `undo status`.
- Check the path isn't ignored (`.gitignore`, `.undoignore`, or a builtin like `node_modules/`, `target/`, `.env*`).
- On network/FUSE filesystems, real-time events may not fire; see [Platform Support](#platform-support). The startup scan still catches missed changes when Undo restarts.
- Check `~/.undo/undo.log` for paused-recording or watcher errors.

### Checking saved versions after a crash

Undo writes saved copies before it records them in the database, and SQLite WAL helps interrupted writes recover cleanly. At worst, a crash may leave an unused saved copy that the next cleanup removes. If you suspect a problem after a hard crash, restart Undo so it can scan and match the database to what is on disk.

---

## License

[FSL-1.1-MIT](../LICENSE)
