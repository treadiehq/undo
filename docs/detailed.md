# Undo — Detailed Documentation

Complete reference for undo's features, internals, and configuration.

## Table of Contents

- [Installation](#installation)
- [Commands](#commands)
- [How It Works](#how-it-works)
- [Configuration](#configuration)
- [Retention and Pruning](#retention-and-pruning)
- [Ignored Paths](#ignored-paths)
- [Safety Guards](#safety-guards)
- [Data Storage](#data-storage)
- [Multi-Project Support](#multi-project-support)
- [Platform Support](#platform-support)

---

## Installation

### Quick install (macOS / Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/treadiehq/undo/main/install.sh | sh
```

### Download binaries

Prebuilt binaries for macOS (ARM + Intel) and Linux (x86_64) are available on the
[Releases page](https://github.com/treadiehq/undo/releases).

### Build from source

Requires Rust 1.70+.

```bash
git clone https://github.com/treadiehq/undo.git
cd undo
cargo build --release
cp target/release/undo /usr/local/bin/
```

---

## Commands

### `undo start`

Start watching the current directory.

```bash
cd your-project/
undo start
```

```
undo — filesystem history
Watching: /Users/me/my-project
Recording changes...
```

The daemon runs in the foreground. Use `Ctrl+C` to stop, or run it in the background:

```bash
undo start &
```

You can run multiple daemons simultaneously for different projects — each gets its own PID file.

Use `--force` to skip safety checks (ownership, file-count limit):

```bash
undo start --force
```

### `undo what-changed <duration>`

Show what changed in a time window.

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

Show a chronological log of recent file activity.

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

Show a unified diff between the current file and its latest captured snapshot.

```bash
undo diff src/server.rs
```

### `undo restore <path> <duration>`

Restore a file from a past snapshot.

```bash
undo restore src/server.rs 10m
```

```
Backup of current file saved to /Users/me/.undo/backups/server.rs_1713200000.bak
Restored src/server.rs from snapshot captured 9 minute(s) ago.
```

A safety backup is always created in `~/.undo/backups/` before overwriting, so it survives reboots.

### `undo prune`

Remove old history beyond the retention window.

```bash
undo prune
undo prune --keep 30d
undo prune --dry-run
```

```
Pruned 342 events, 89 snapshots, 3 backups.
Freed 12.4 MB. Current usage: 45.2 MB.
```

| Flag | Description |
|------|-------------|
| `--dry-run` | Preview what would be deleted without deleting |
| `--keep <duration>` | Override retention period (e.g. `30d`, `12h`) |

Auto-pruning also runs at daemon startup and every hour while the daemon is running.

### `undo status`

Show daemon status, event counts, retention config, and disk usage.

```bash
undo status
```

```
undo — status

Project:   /Users/me/my-project
Daemon:    running (PID 12345)
Database:  /Users/me/.undo/database.db (24.0 KB)
Events:    142
Snapshots: 87
Retention: 7 days, 1.0 GB max
Disk:      45.2 MB (snapshots: 38.1 MB, backups: 5.8 MB, db: 1.3 MB)
```

### `undo stop`

Stop the daemon.

```bash
undo stop          # stop daemon for this project
undo stop --all    # stop all running undo daemons
```

### `undo update`

Update undo to the latest release.

```bash
undo update
```

---

## How It Works

undo runs a lightweight daemon that watches your project directory using OS-native file watching (FSEvents on macOS, inotify on Linux).

When files change, undo:

1. Hashes the file content (SHA-256)
2. Compares against the last known hash
3. Saves a compressed snapshot atomically (write to temp file, then rename) if the content changed
4. Records the event in a local SQLite database

On startup, undo performs a reconciliation scan to detect any changes that happened while the daemon was stopped — so you never have a gap in history.

If the watched directory becomes inaccessible (e.g. a remote filesystem unmount), undo pauses recording and resumes automatically with a full reconciliation when the directory reappears.

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

Per-project values override global values. If neither file exists, the hardcoded defaults apply.

| Key | Default | Description |
|-----|---------|-------------|
| `retention_days` | `7` | Days of history to keep |
| `max_size_mb` | `1024` | Maximum total size of `~/.undo/` in MB |

---

## Retention and Pruning

undo automatically prunes old history to prevent unbounded disk usage.

### What gets pruned

Three things, in this order:

1. **Events** — database records older than the retention window are deleted
2. **Orphaned snapshots** — `.gz` snapshot files no longer referenced by any remaining event
3. **Backups** — files in `~/.undo/backups/` older than the retention window (by file mtime)

After TTL pruning, if total `~/.undo/` size still exceeds `max_size_mb`, the oldest snapshots are deleted across all projects until under the cap.

### When pruning runs

- At daemon startup (after the initial reconciliation scan)
- Every hour while the daemon is running
- Manually with `undo prune`

### Dry run

```bash
undo prune --dry-run
```

Shows what would be deleted without actually deleting anything.

### Override retention

```bash
undo prune --keep 30d
```

Temporarily overrides the configured retention period for this prune run.

---

## Ignored Paths

undo automatically ignores noisy and sensitive paths:

- `.git/`, `.undo/`
- `node_modules/`, `__pycache__/`
- `target/`, `dist/`, `build/`, `.next/`
- `.DS_Store`, `.idea/`, `.vscode/`
- `.env`, `.env.local`, `.env.production`, `.ssh/`
- `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.keystore`

### Custom ignore patterns

Add a `.undoignore` file to your project root with additional patterns (one per line, same syntax as `.gitignore`):

```
*.log
tmp/
*.sqlite
```

If a `.gitignore` file exists, undo respects it too. `.undoignore` patterns take precedence.

### Overriding the defaults

If a default-ignored path is actually something you want tracked, use a negation pattern in `.undoignore`:

```
!build/
!.env
!dist/
```

Negation patterns override the builtin ignore list. This is useful when a default like `build/` or `dist/` is actually source code in your project, or when you intentionally want to track `.env` changes.

### Large files

Files larger than **100 MB** are tracked (events recorded) but not snapshotted, to keep disk usage reasonable.

---

## Safety Guards

undo refuses to start when it detects dangerous conditions:

| Guard | Why |
|-------|-----|
| **Root/sudo** | Running as root writes data to root's home, invisible to your normal user |
| **System directories** | Directories owned by root or system accounts (`/`, `/etc`, `/usr`, etc.) |
| **Oversized directories** | More than 50,000 files (catches `~/` and similar broad directories) |

All guards can be overridden with `--force`.

### Overlapping directories

Undo prevents starting a daemon if another daemon is already watching a parent or child directory. For example, if you're watching `/foo/bar`, starting a second daemon on `/foo` would cause both to record events for files in `/foo/bar`, producing duplicates. Undo detects this and refuses to start with a message pointing to the conflicting daemon.

### Remote file clobbers (SCP, rsync, etc.)

Undo watches at the filesystem level, not the process level. If someone overwrites a file via `scp`, `rsync`, or any other remote tool, undo records the change like any other modification. The previous version is snapshotted and restorable with `undo restore`.

This makes undo a natural safety net for remote deployments and shared development servers — if a file gets clobbered by a remote write, you can get it back.

One caveat: if the file is owned by root or has restricted permissions, undo (running as your normal user) may not be able to read it for snapshotting. The event is still recorded, but no snapshot is saved.

---

## Data Storage

All data is stored locally at `~/.undo/`:

| Path | Purpose |
|------|---------|
| `database.db` | Event history and file state (SQLite) |
| `snapshots/<project_id>/` | Gzip-compressed file snapshots, named by content hash |
| `pids/<hash>.pid` | Per-project daemon PID files |
| `backups/` | Safety backups created before restoring a file |
| `config.toml` | Global configuration (optional) |

Snapshots are content-addressed: if two files have the same content, only one snapshot is stored.

Snapshots are written atomically (write to `.tmp`, then rename) to prevent partial snapshots from being treated as valid data.

---

## Multi-Project Support

You can run undo on multiple projects simultaneously. Each project gets:

- Its own PID file in `~/.undo/pids/`
- Its own snapshot directory in `~/.undo/snapshots/<project_id>/`
- Its own events in the shared SQLite database (partitioned by `project_id`)
- Its own retention config via `.undorc`

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

---

## License

[FSL-1.1-MIT](../LICENSE)
