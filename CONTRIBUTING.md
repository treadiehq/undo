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

- `main.rs` — entry point, command dispatch, path-boundary checks, and shared
  helpers.
- `cli.rs` — `clap` commands, aliases, arguments, and machine-output flags.
- `runs.rs` — Run start/stop/list/show, child-process wrappers, actor inference,
  and automatic recorder synchronization.
- `agent_events.rs` — strict version 1 lifecycle JSON validation, processing,
  responses, and retry idempotency.
- `recoveries.rs` — persisted Recovery plans, expected-hash preflight, apply
  idempotency, and explicit-intent inverse patches.
- `ask.rs` — deterministic intent-label and path-group selection.
- `recover.rs` — explicit whole-Run and whole-group compatibility recovery.
- `sessions.rs` — pre-Run session command compatibility.
- `activity.rs` — checkpoints, timeline, deleted-file listing, burst detection,
  and panic mode.
- `groups.rs` — deterministic path-derived change groups and diff statistics.
- `daemon.rs` — recorder start/stop/status, PID files, `flock` liveness, safety
  guards, overlap detection, and automatic startup.
- `watcher.rs` — initial scan and watch loop, reconciliation, debouncing,
  filesystem watchdog, and event handlers.
- `snapshots.rs` — content-addressed gzip snapshot storage.
- `db.rs` — SQLite schema, migrations, and queries.
- `restore.rs` — point-in-time planning, symlink refusal, backups, and per-file
  atomic writes.
- `diff.rs` — capped reads, binary detection, and unified diff.
- `retention.rs` — config, pruning, stale temp-file reaping, and disk accounting.
- `ignore.rs` — built-in and root `.gitignore`/`.undoignore` matching.
- `integrity.rs` — shallow startup and deep on-demand snapshot verification.
- `duration.rs`, `logging.rs`, `update.rs`, and `models.rs` — duration parsing,
  logs, self-update, and shared data records.

Documentation is organized under `docs/`:

- `docs/README.md` is the introduction and index.
- `docs/getting-started.md`, `concepts.md`, and `recovery.md` explain the primary
  user model.
- `docs/cli.md` and `machine-output.md` are command/output references.
- `docs/agent-integration-spec.md` defines `undo event` version 1.
- `docs/agents/` contains practical agent wrapper and lifecycle guides.
- `docs/storage.md` covers local data and operations.
- `docs/demos.md` contains reproducible recovery exercises.
- `docs/detailed.md` preserves compatibility links and remaining operational
  notes.

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
  the relevant page under [`docs/`](docs/README.md) and [`README.md`](README.md)
  when behavior or commands change.
- Note user-facing changes in [`CHANGELOG.md`](CHANGELOG.md) under `Unreleased`.

## Reporting bugs and security issues

Open an issue for bugs and feature requests. For security vulnerabilities, follow
the process in [`SECURITY.md`](SECURITY.md) instead of filing a public issue.
