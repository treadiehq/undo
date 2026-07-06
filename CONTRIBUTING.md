# Contributing to undo

Thanks for your interest in improving **undo**. This guide covers how to build,
test, and find your way around the codebase.

## Prerequisites

- **Rust 1.85+** (the project uses the 2024 edition). Install via [rustup](https://rustup.rs/).
- A Unix-like OS. undo is Unix-only: it relies on `flock`, Unix file modes
  (`0600`/`0700`), and OS-native file watching (FSEvents on macOS, inotify on
  Linux). It does not build or run on Windows.

No system dependencies are required — SQLite is bundled via the `rusqlite`
`bundled` feature.

## Build and run

```bash
git clone https://github.com/treadiehq/undo.git
cd undo

cargo build                 # debug build at target/debug/undo
cargo build --release       # optimized build at target/release/undo

cargo run -- status         # run a subcommand directly
```

### Trying it out without touching your real history

undo stores everything under `~/.undo/`, resolved from `$HOME`. To exercise a
local build against throwaway data instead of your real store, point `$HOME` at a
temp directory:

```bash
mkdir -p /tmp/undo-test-home /tmp/undo-test-proj
echo hello > /tmp/undo-test-proj/a.txt
( cd /tmp/undo-test-proj && HOME=/tmp/undo-test-home ./target/debug/undo start )
# in another shell:
( cd /tmp/undo-test-proj && HOME=/tmp/undo-test-home ./target/debug/undo status )
```

## Tests and lints

CI runs three checks (see `.github/workflows/ci.yml`); run them locally before
opening a PR:

```bash
cargo fmt --all --check                  # formatting
cargo clippy --all-targets -- -D warnings # lints (warnings are errors)
cargo test --all                          # unit tests
```

`cargo fmt --all` (without `--check`) applies formatting. If you have
[`cargo-nextest`](https://nexte.st/) installed, `cargo nextest run` is a faster
drop-in for the test step.

The test suite is fast and hermetic — no daemon, network, or real `~/.undo`
access is required.

## Project layout

All source lives in `src/`. Each module is focused:

| Module | Responsibility |
|--------|----------------|
| `main.rs` | Entry point, command dispatch, `safe_resolve_path` (path-traversal/symlink bounds checks), shared helpers |
| `cli.rs` | `clap` command and flag definitions |
| `daemon.rs` | `start` / `stop` / `status`; PID files, `flock` liveness, safety guards, overlap detection |
| `watcher.rs` | The watch loop and initial scan; debouncing, the fs-timeout watchdog, per-event handlers |
| `snapshots.rs` | Content-addressed gzip snapshot store (atomic temp-file + rename) |
| `db.rs` | SQLite schema and all queries (events, file state, retention) |
| `restore.rs` | `restore` — symlink refusal, safety backup, atomic write |
| `diff.rs` | `diff` — binary detection, capped reads, unified diff |
| `retention.rs` | Config loading (`config.toml` / `.undorc`), pruning (including leaked temp-file reaping), disk-usage accounting |
| `ignore.rs` | Builtin ignore list + `.gitignore`/`.undoignore` matching |
| `duration.rs` | Human-friendly duration parsing/formatting (`5m`, `2h`, `1d`) |
| `logging.rs` | Persistent daemon log at `~/.undo/undo.log` (with rotation) |
| `update.rs` | `update` — self-update with `SHA256SUMS` verification |
| `models.rs` | Plain data structs shared across modules |

Architecture and storage details are documented in
[`docs/detailed.md`](docs/detailed.md).

## Testing conventions

The codebase leans heavily on unit tests, and a few patterns keep them isolated:

- **In-memory database** — `Database::open_in_memory()` gives each test its own
  SQLite instance.
- **Redirected data dir** — `set_test_data_dir(path)` (a `#[cfg(test)]`
  thread-local) points `backtrack_dir()` at a tempdir so snapshot/retention tests
  never touch the real `~/.undo`.
- **Per-thread ignore matcher** — under `#[cfg(test)]` the ignore matcher is a
  thread-local, so tests can call `ignore::init()` without affecting each other.
- **Document the "why"** — most tests carry a doc comment explaining the behavior
  they pin, especially regression tests. When fixing a bug, add a test that fails
  against the old code and passes with the fix.

## Pull requests

- Keep changes focused; describe every production file touched (including
  `#[cfg(test)]`-only changes to files that ship in the binary).
- Make sure `fmt`, `clippy -D warnings`, and the tests all pass — CI gates on them.
- Add or update tests for behavior changes, and update
  [`docs/detailed.md`](docs/detailed.md) / [`README.md`](README.md) when behavior
  or commands change.
- Note user-facing changes in [`CHANGELOG.md`](CHANGELOG.md) under `Unreleased`.

## Reporting bugs and security issues

Open an issue for bugs and feature requests. For security vulnerabilities, follow
the process in [`SECURITY.md`](SECURITY.md) instead of filing a public issue.
