# Undo

**Ctrl+Z for your filesystem. Protecting working directories from accidents, made simple.**

You deleted a file. You changed something 10 minutes ago and can't remember what. You haven't committed yet. It's gone.

Not anymore. Undo watches your project directory and records every change. See what happened, diff it, restore it, instantly.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/treadiehq/undo/main/install.sh | sh
```

Or grab a binary from the [Releases page](https://github.com/treadiehq/undo/releases). macOS (ARM + Intel) and Linux.

## 30-second demo

```bash
cd my-project/
undo start &                     # start watching

# ... work on your project ...

undo what-changed 5m             # what changed in the last 5 minutes?
undo diff src/server.rs          # show me the diff
undo restore src/server.rs 10m   # bring back the version from 10 minutes ago
undo timeline                    # full activity log
```

That's it. No commits, no staging, no ceremony.

## Commands

| Command | What it does |
|---------|-------------|
| `undo start` | Start watching the current directory |
| `undo what-changed 5m` | See what changed in a time window |
| `undo timeline` | Chronological activity log |
| `undo diff <file>` | Diff a file against its last snapshot |
| `undo restore <file> <time>` | Restore a file from the past |
| `undo status` | Daemon status, disk usage, retention info |
| `undo prune` | Clean up old history |
| `undo stop` | Stop watching |

## How it works

Undo runs a tiny background daemon that uses native OS file watching. When a file changes, it hashes the content, saves a compressed snapshot, and logs the event to a local SQLite database. Everything stays on your machine at `~/.undo/`.

It auto-prunes old history (default: 7 days, 1 GB cap), respects `.gitignore` and `.undoignore`, and refuses to watch dangerous directories like `/` or `~/`.

## Configuration

Global defaults in `~/.undo/config.toml`, per-project overrides in `.undorc`:

```toml
retention_days = 7
max_size_mb = 1024
```

For the full reference — every command flag, how pruning works, data storage layout, safety guards, multi-project support — see the [detailed docs](docs/detailed.md).

## License

[FSL-1.1-MIT](LICENSE)
