# Changelog

All notable changes to **undo** are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it reaches
`1.0`.

Entries below `v0.1.12` were reconstructed from release commit summaries; they
describe the headline changes of each tag rather than an exhaustive list.

## [Unreleased]

- Prune now reclaims leaked snapshot temp files (`<hash>.gz.tmp.*`) left behind
  when a durable write is interrupted by a hard kill or power loss. Previously
  nothing reclaimed them and they inflated disk usage indefinitely; reaping is
  age-guarded so an in-flight write is never touched. `undo status` also no
  longer counts such temp files toward the snapshot total.

## [0.1.12] — 2026-06-17

- Watch-loop and history robustness: per-event panic guard so a single bad event
  can't kill the daemon; reconciliation guards against treating an empty remount
  or swapped disk as mass deletion; non-UTF8 paths are skipped rather than mangled.
- Hot-path performance: single-walk `undo status`, single-allocation hex encoding,
  size/mtime fast path that skips re-hashing unchanged files, batched initial-scan
  inserts.
- Self-update integrity: release artifacts are verified against a published
  `SHA256SUMS` before install.
- Persistent daemon log at `~/.undo/undo.log`.
- Owner-only (`0600`) permissions extended to the SQLite WAL/SHM sidecars.

## [0.1.11] — 2026-04-16

- Restore now writes to a collision-safe sibling temp file before renaming, so an
  interrupted restore never leaves a partially written target.

## [0.1.10] — 2026-04-16

- Expanded unit-test coverage and per-test documentation.

## [0.1.9] — 2026-04-14

- `flock`-based daemon liveness so stale PID files are detected reliably.
- Pruning safety fixes and a rename-handling fix.

## [0.1.8] — 2026-04-13

- Filesystem-operation watchdog timer so a hung mount can't wedge the watch loop.
- Filesystem-compatibility documentation and additional test coverage.

## [0.1.7] — 2026-04-13

- Security hardening.
- `.undoignore` negation patterns (`!build/`) can override the builtin ignore list.

## [0.1.6] — 2026-04-13

- Retention and pruning (TTL + size cap).
- Overlap guard preventing two daemons from watching nested directories.
- Streamlined README.

## [0.1.5] — 2026-04-13

- Safety guards (refuse root/sudo, system-owned directories, oversized trees).
- Multi-project daemon support with per-project PID files.

## [0.1.4] — 2026-04-13

- Maintenance release.

## [0.1.3] — 2026-04-09

- Added the `undo update` self-update command.
- Raised the per-file snapshot limit to 100 MB.

## [0.1.2] — 2026-04-09

- Restore falls back to the earliest available snapshot when none matches the
  requested time.

## [0.1.1] — 2026-04-09

- Renamed the binary from `backtrack` to `undo`.

## [0.1.0] — 2026-04-09

- Initial release (originally "Backtrack"): filesystem history for your working
  directory — watch, snapshot, diff, and restore.

[Unreleased]: https://github.com/treadiehq/undo/compare/v0.1.12...HEAD
[0.1.12]: https://github.com/treadiehq/undo/compare/v0.1.11...v0.1.12
[0.1.11]: https://github.com/treadiehq/undo/compare/v0.1.10...v0.1.11
[0.1.10]: https://github.com/treadiehq/undo/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/treadiehq/undo/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/treadiehq/undo/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/treadiehq/undo/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/treadiehq/undo/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/treadiehq/undo/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/treadiehq/undo/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/treadiehq/undo/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/treadiehq/undo/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/treadiehq/undo/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/treadiehq/undo/releases/tag/v0.1.0
