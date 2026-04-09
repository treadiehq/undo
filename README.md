# Undo 

Filesystem history for your working directory

**See what changed. Diff it. Restore it. No git commit required.**

undo gives your project a local timeline. It watches your working directory, records every file change, and lets you see exactly what changed, when it changed, and restore files from minutes ago.

Think of it as:

- **Git for files that aren't in git yet**
- **Time-travel for your working directory**

## Why?

Because you've been there:

- You deleted a file by accident
- You can't remember what you changed 10 minutes ago
- You want to undo a change but haven't committed anything
- You need a safety net while prototyping

undo runs quietly in the background and gives you instant answers.

## Install

### Quick install (macOS / Linux)

```bash
curl -fsSL https://raw.githubusercontent.com/treadiehq/undo/main/install.sh | sh
```

### Download binaries

Prebuilt binaries for macOS (ARM + Intel) and Linux (x86_64) are available on the
[Releases page](https://github.com/treadiehq/undo/releases).

### Build from source

```bash
git clone https://github.com/treadiehq/undo.git
cd undo
cargo build --release
cp target/release/undo /usr/local/bin/
```

### Requirements

- macOS or Linux
- Rust 1.70+ (only for building from source)

## Usage

### Start watching

```bash
cd your-project/
undo start
```

Output:

```
undo — filesystem history
Watching: /Users/me/my-project
Recording changes...
```

The daemon runs in the foreground. Use `Ctrl+C` to stop, or run it in the background:

```bash
undo start &
```

### See what changed

```bash
undo what-changed 5m
undo what-changed 2h
undo what-changed 1d
```

Output:

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

### View timeline

```bash
undo timeline
undo timeline --limit 50
```

Output:

```
undo — recent activity

12:31 MODIFIED src/server.rs
12:30 CREATED logs/debug.log
12:29 DELETED assets/logo.png
12:28 RENAMED src/app_old.rs -> src/app.rs
```

### Diff a file

```bash
undo diff src/server.rs
```

Shows a unified diff between the current file and the latest captured snapshot.

### Restore a file

```bash
undo restore src/server.rs 10m
```

Output:

```
Backup of current file saved to /tmp/undo-restore-server.rs-1713200000.bak
Restored src/server.rs from snapshot captured 9 minute(s) ago.
```

A safety backup is always created before overwriting.

### Check status

```bash
undo status
```

Output:

```
undo — status

Project:   /Users/me/my-project
Daemon:    running (PID 12345)
Database:  /Users/me/.undo/database.db (24.0 KB)
Events:    142
Snapshots: 87
```

### Stop the daemon

```bash
undo stop
```

## Demo

```
$ cd my-project/
$ undo start &
undo — filesystem history
Watching: /Users/me/my-project
Recording changes...

$ echo "hello" > test.txt
$ echo "world" >> test.txt
$ rm old-file.txt

$ undo what-changed 1m

Changes in last 1m

MODIFIED
  - test.txt

DELETED
  - old-file.txt

$ undo restore old-file.txt 2m
Backup of current file saved to /tmp/undo-restore-old-file.txt-1713200100.bak
Restored old-file.txt from snapshot captured 2 minute(s) ago.
```

## How it works

undo runs a lightweight daemon that watches your project directory using OS-native file watching (FSEvents on macOS, inotify on Linux).

When files change, undo:

1. Hashes the file content (SHA-256)
2. Compares against the last known hash
3. Saves a compressed snapshot if the content changed
4. Records the event in a local SQLite database

All data is stored locally at `~/.undo/`:

| Path | Purpose |
|------|---------|
| `database.db` | Event history and file state |
| `snapshots/` | Gzip-compressed file snapshots |
| `pid` | Daemon process ID |

## Ignored paths

undo automatically ignores noisy directories:

- `.git/`
- `node_modules/`
- `target/`
- `.next/`
- `dist/`
- `build/`
- `.DS_Store`
- `__pycache__/`

Files larger than **5 MB** are tracked (events recorded) but not snapshotted.

## Platform support

- macOS
- Linux

## License

[FSL-1.1-MIT](LICENSE)
