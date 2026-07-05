# Undo

**Give your files an undo button.**

Undo watches your project in the background so you can see what changed, compare versions, and restore files when something goes wrong.

You deleted a file. You changed something 10 minutes ago and can't remember what. You haven't committed yet. It feels gone.

Not anymore. Undo keeps local history, then lets you bring files back instantly.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/treadiehq/undo/main/install.sh | sh
```

You can also download a binary from the [Releases page](https://github.com/treadiehq/undo/releases). macOS (ARM + Intel) and Linux are supported.

## 30-second demo

```bash
cd my-project/
undo start &                     # start saving history

# ... work on your project ...

undo what-changed 5m             # see changes from the last 5 minutes
undo diff src/server.rs          # compare with the saved version
undo restore src/server.rs 10m   # bring back the version from 10 minutes ago
undo timeline                    # show recent file activity
```

That's it. No commits, no setup, no server.

## Commands

| Command | What it does |
|---------|-------------|
| `undo start` | Start saving history for this folder |
| `undo what-changed 5m` | See what changed recently |
| `undo timeline` | Show recent file activity |
| `undo diff <file>` | Compare a file with its saved version |
| `undo restore <file> <time>` | Bring back an older version |
| `undo status` | See if Undo is running and how much space it uses |
| `undo prune` | Delete old saved history |
| `undo stop` | Stop saving history (`--all` stops every folder) |
| `undo update` | Update to the latest release |

## How it works

Undo runs in the background and watches your project for file changes. When a file changes, Undo saves a local copy and records what happened. Everything stays on your machine at `~/.undo/`.

Undo keeps history for 7 days by default, with a 1 GB space limit. It skips noisy folders like `node_modules` and `target`, and it follows your root `.gitignore` plus any `.undoignore` rules you add. (Nested `.gitignore` files in subdirectories are not yet supported.)

Undo writes logs to `~/.undo/undo.log`, so you can still see errors after the terminal closes. If something looks off, check there first (and see [Troubleshooting](docs/detailed.md#troubleshooting)).

## Configuration

Set global defaults in `~/.undo/config.toml`, or project-specific settings in `.undorc`:

```toml
retention_days = 7
max_size_mb = 1024
```

For every command flag, cleanup rules, storage details, safety checks, and multi-project use, see the [detailed docs](docs/detailed.md).

## More

- [Detailed documentation](docs/detailed.md) — command options, storage details, platform support, troubleshooting
- [Contributing](CONTRIBUTING.md) — build, test, and the project layout
- [Changelog](CHANGELOG.md) — release history
- [Security policy](SECURITY.md) — threat model and how to report a vulnerability

## License

[FSL-1.1-MIT](LICENSE)
