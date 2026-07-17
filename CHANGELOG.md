# Changelog

All notable changes to **undo** are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it reaches
`1.0`.

Entries below `v0.1.12` were reconstructed from release commit summaries; they
describe the headline changes of each tag rather than an exhaustive list.

## [Unreleased]

## [0.2.1] — 2026-07-17

- Restore and Recovery filesystem operations are now capability-confined to the
  watched project, rejecting path traversal, unsafe symlink targets, and TOCTOU
  attempts before reads, writes, deletes, or backups.
- Snapshot publication and retention pruning now coordinate through a
  crash-safe shared/exclusive store lock, preventing concurrent prune from
  deleting a newly written snapshot before its database reference commits.
- Run, checkpoint, and intent event boundaries now use immediate transactions,
  preventing concurrent daemon commits from being omitted or misattributed.
- Recovery groups preserve top-level path context, so similarly named
  directories such as `app/auth` and `lib/auth` cannot merge into one selection.
- Diff and restore previews compare raw bytes and reversibly escape invalid
  UTF-8, preventing distinct file contents from being reported as unchanged.
- The daemon panic hook no longer waits on a logger mutex that may already be
  held by the panicking thread.
- `undo sessions --json` now behaves exactly like `undo runs --json`, and
  `undo timeline --since ... --limit ...` now honors its event limit.
- Intent recovery is idempotent when a file has already been manually restored
  to its pre-intent state.

## [0.2.0] — 2026-07-14

- Added Run-first recovery with stable `r_...` IDs, actor/agent/command metadata,
  completion status, and one active Run per project for honest filesystem-event
  attribution.
- `undo run <command>` now starts recording when needed, launches the child, and
  completes the Run on exit; `undo runs` and `undo run show` expose the recorded
  boundary and changes.
- New checkpoints are immutable and anchored to filesystem event IDs. Legacy
  checkpoints remain timestamp-only and recover through the compatibility path.
- Recovery previews now persist for 24 hours with stable `rec_...` IDs.
  `undo apply` verifies expected current existence/hashes, refuses stale
  conflicts, and is idempotent after success.
- Explicit completed intent boundaries support same-file selective reversal
  through inverse patch application, preserving clean non-overlapping later
  edits while blocking overlaps and ambiguous create/delete/rename states.
- Added the strict version 1 `undo event` JSON lifecycle format with
  `run_started`, `checkpoint`, `intent_started`, `intent_completed`, and
  `run_completed` events plus persisted idempotency responses.
- Run, checkpoint, and event boundaries automatically start and synchronize the
  local recorder when it is not already active.
- Point-in-time reconstruction now treats deletion and rename-away as real
  states, and restore writes preserve the current Unix mode of existing files.
- Panic apply now creates and previews a hash-checked persisted Recovery before
  mutation.
- Fixed `undo ask` treating the word `session` as a request to revert an entire
  Run.
- Existing `session` commands, `sessions`, `--session`, `mark`/`marks`, `--cp`,
  and panic burst names remain as compatibility aliases.
- The SQLite schema migrates existing stores to version 3, retaining the
  compatibility-named sessions table while adding Run metadata, event-anchored
  checkpoints, intents, Recoveries, and integration idempotency records.

## [0.1.17] — 2026-07-14

- Restore now detects deleted directory scopes, allowing selective recovery
  without restoring unrelated project changes.
- Cross-directory renames and session baselines remain recoverable after older
  event rows are pruned, using pinned `previous_hash` snapshots without
  misclassifying files created after the restore target.
- `undo ask` no longer mistakes clause keywords inside hyphenated group IDs for
  preserve intent, and compact queries such as `keepalive` match `keep-alive`.
- Panic-dashboard preview and restore commands now share an exact
  `--timestamp` target, preventing relative-duration drift between preview and
  application.

## [0.1.16] — 2026-07-09

- Recovery UX: added checkpoint, preview, deleted-file listing, timeline burst,
  and panic-dashboard flows for safer rollback after messy edits or AI-agent
  runs.
- Docs now position Undo as a preview-first AI coding safety net while keeping
  the current workflow honest: burst/checkpoint recovery, not magical semantic
  hunk selection.
- Semantic rollback groundwork: added manual sessions, deterministic change
  groups, and `undo recover --session ... [--group ...]` for selective,
  preview-first recovery of agent work.
- Added `undo ask "<intent>"` as a conservative semantic rollback MVP that maps
  intent to session groups, previews revert/keep proposals, and requires
  `--apply --yes` before writing.

## [0.1.15] — 2026-07-06

- Ignore rules: directory negations such as `!build/` now whitelist child files
  before builtin ignores are applied, matching the documented `.undoignore`
  behavior for initial scans and live watcher events.
- Self-update now compares release tags semantically and refuses downgrades when
  GitHub reports an older release as latest.
- Restore backups are collision-safe for same-named files restored within the
  same timestamp window, and backup creation never overwrites an existing backup.
- `undo diff` now compares recreated files against the last known content from a
  `DELETED` event instead of incorrectly claiming the file is still deleted.
- Rename-overwrite handling now preserves overwritten destination content through
  retention by recording the overwrite and pinning surviving event
  `previous_hash` values until those events age out.

## [0.1.14] — 2026-07-05

- Integrity check is now tiered for speed on large histories: daemon startup runs
  a shallow existence-only check (a stat per distinct snapshot, which still catches
  the power-loss symptom of a committed row pointing at missing bytes), while
  `undo status` runs the full decompress/CRC check on demand and reports it as the
  `Integrity:` line.
- Documentation: fixed accuracy drift (toolchain version, ignore-list patterns,
  data-storage layout, the `Log:` and `Integrity:` status lines) and added
  `CONTRIBUTING.md`, `CHANGELOG.md`, `SECURITY.md`, and Cargo package metadata.
- Prune now reclaims leaked snapshot temp files (`<hash>.gz.tmp.*`) left behind
  when a durable write is interrupted by a hard kill or power loss. Previously
  nothing reclaimed them and they inflated disk usage indefinitely; reaping is
  age-guarded so an in-flight write is never touched. `undo status` also no
  longer counts such temp files toward the snapshot total.

## [0.1.13] — 2026-06-17

- Crash/power-loss durability for the live write path: snapshots written by the
  live watcher are fsync'd (file + parent directory) before the referencing
  database row is committed, and each event's row plus file-state update now commit
  in a single transaction, so an interrupted write can't leave a dangling hash
  chain or a row pointing at bytes that never reached disk. (`undo status` gains an
  `Integrity:` line backed by a startup snapshot-verification pass.)
- Retention/duration time math is saturated against overflow: a very large
  retention window no longer wraps the prune cutoff into the future (which would
  have deleted all history), and duration→timestamp subtraction no longer
  underflows for extreme durations in `restore` / `what-changed`.
- CI: replaced deprecated Node-20 GitHub Actions with current majors.

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

[Unreleased]: https://github.com/treadiehq/undo/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/treadiehq/undo/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/treadiehq/undo/compare/v0.1.17...v0.2.0
[0.1.17]: https://github.com/treadiehq/undo/compare/v0.1.16...v0.1.17
[0.1.16]: https://github.com/treadiehq/undo/compare/v0.1.15...v0.1.16
[0.1.15]: https://github.com/treadiehq/undo/compare/v0.1.14...v0.1.15
[0.1.14]: https://github.com/treadiehq/undo/compare/v0.1.13...v0.1.14
[0.1.13]: https://github.com/treadiehq/undo/compare/v0.1.12...v0.1.13
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
