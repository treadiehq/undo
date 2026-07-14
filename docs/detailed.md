# Older commands and low-level operations

This page is retained for older links and integrations. Most readers should use
the task guides below; the rest of this page documents older command names and
low-level recorder, durability, and restore behavior.

- [Introduction and index](README.md)
- [Getting started](getting-started.md)
- [Concepts](concepts.md)
- [Recovery guarantees](recovery.md)
- [CLI reference](cli.md)
- [Storage and operations](storage.md)
- [Machine output](machine-output.md)
- [Agent Integration Spec](agent-integration-spec.md)
- [Reproducible demos](demos.md)

## Use modern Run commands for new work

For new scripts and workflows, prefer:

```bash
undo run claude
undo runs
undo run show r_421
undo ask r_421 "remove auth"
undo apply rec_812
```

### Older commands and aliases

The pre-Run names below remain implemented for compatibility:

- `undo session start <name>` creates an active row in the same `sessions` table
  used by Runs.
- `undo session stop` completes the active compatibility session.
- `undo session show <name>` displays its deterministic path groups.
- `undo sessions` aliases `undo runs`.
- `--session` aliases `--run` on `undo ask` and `undo recover`.
- `undo mark` and `undo marks` alias `undo checkpoint` and
  `undo checkpoints`.
- `--cp` aliases `--checkpoint`.
- `undo panic --undo-burst --yes` aliases
  `--restore-before-latest-burst --yes`.

Older sessions remain readable and recoverable. New scripts and integrations
should use public `r_...` IDs and Run commands because they also expose actor,
agent, command, status, event anchors, JSON output, and explicit intents.

## Understand low-level recorder behavior

### Recorder lifecycle

`undo start` runs in the foreground. Run/checkpoint/event boundaries call the
same recorder automatically when it is absent, detach it with null stdio, and
wait up to ten seconds for the initial scan to become ready.

Each non-overlapping project has its own locked PID file. `undo stop` signals the
matching recorder; `undo stop --all` handles every live PID file. Stale PID files
are detected through `flock`, not trusted by PID text alone.

### Reconciliation and durability

At startup Undo scans the project to catch changes made while it was stopped.
When a watched root disappears, it pauses and reconciles after the root returns.
An empty or device-swapped remount is guarded so it is not immediately recorded
as mass deletion.

Live snapshots are synced before their event transaction commits. The event row
and current file-state update commit together in SQLite WAL mode. A hard kill
can leave an unpublished `.gz.tmp.*` or unreferenced snapshot; pruning reclaims
stale temp files after one hour and unreferenced snapshots during cleanup.

`undo status` runs the expensive deep snapshot check. Recorder startup only
checks that referenced snapshot files exist.

### Restore publication and conflict checks

Restore rejects paths outside the project and direct symlink targets. Existing
files are copied to owner-only backups before overwrite or deletion. Each write
is published with a sibling temp-file rename.

Persisted Recoveries add expected-current hash checks and 24-hour expiration.
Direct `undo restore --preview` followed by a separate restore does not carry
those hash checks; use `undo ask`/`undo apply` when a locked plan is needed.
Multi-file apply checks every planned path before writing, but the filesystem
changes are not committed as one transaction.

## Diagnose common failures

### Changes are not recorded

1. Run `undo status` from the project or a child directory.
2. Check root `.gitignore`, `.undoignore`, and built-in ignore rules.
3. Confirm the file is readable, has a UTF-8 path, and is no larger than
   100 MiB (`100 * 1024 * 1024` bytes).
4. Check network/FUSE event behavior and restart recording for reconciliation.
5. Read `~/.undo/undo.log`.

### A Run will not start

Run `undo runs`. Another active Run in the same project must be completed:

```bash
undo run stop r_421 --status completed
```

This single-active-Run rule reduces attribution ambiguity between Runs. It does
not prove which process made each write, so unrelated concurrent changes may
still be included.

### `undo ask` selects paths instead of an intent

Run `undo run show r_421` and verify the intent is completed and its label
uniquely matches significant terms in the request. Tied or absent intent matches
fall back to deterministic path groups and whole-file baselines.

### A Recovery is ambiguous

Explicit intent inversion could not safely separate later edits, often because
they overlap the same lines or involve a create/delete/rename with changed
current state. Undo intentionally blocks apply. Preserve the current file,
recover a broader boundary, or manually resolve the diff.

### A Recovery is conflicted or expired

One or more files changed after preview, or more than 24 hours elapsed. Create a
fresh preview:

```bash
undo ask r_421 "remove auth"
```

### A snapshot is missing or corrupt

`undo status` reports missing and decompression failures separately. History and
metadata may still be visible, but versions backed only by unreadable snapshots
cannot be restored. Undo does not synthesize missing bytes.

### Recorder startup is refused

By default Undo blocks root/sudo use, system-owned roots, more than 50,000 files,
and overlapping watched paths. Narrow the project or fix ownership. `--force`
bypasses these checks only when you accept the risk.

## License

[FSL-1.1-MIT](../LICENSE)
