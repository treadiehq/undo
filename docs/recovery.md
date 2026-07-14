# Recovery

Undo gives you two ways to get back to a known-good state:

1. Reverse selected work from a recorded Run.
2. Recover from continuous local history by time, path, checkpoint, deletion,
   or recent burst.

## Reverse selected work from a Run

```bash
undo ask r_421 "remove the auth migration work"
```

The target Run must be complete. `undo ask` uses your request to choose work,
shows the proposed result, and saves it as a Recovery. It does not change files.

A **Recovery** is a saved, expiring plan of file writes and deletions. Selection
follows this order.

### 1. Prefer one matching intent

An **intent** is a marked task inside a Run. If exactly one completed intent
matches the request, Undo selects that task. This can preserve later edits in
the same file as long as they do not overlap the selected work. Overlap blocks
apply instead of risking the later work.

The exact selection is deterministic: Undo normalizes significant request terms
and scores completed intent labels. One intent must have the highest positive
score.

For each path touched between that intent's event-ID boundaries, Undo finds the
state before and after the intent:

- If the current state still equals the intent's after-state, Undo directly
  plans the before-state.
- If the file has later edits, Undo builds the inverse patch from after-state to
  before-state and applies it to the current bytes.
- If the inverse patch applies cleanly, non-overlapping later edits are kept.
- If later edits overlap, or a changed create/delete/rename or unavailable
  snapshot cannot be separated safely, the Recovery is ambiguous and cannot be
  applied.

The later-edit path applies an inverse patch to the current bytes. This is
deterministic patch application, not LLM semantic inference.

### 2. Otherwise, select whole files

If no completed intent uniquely matches, Undo uses words from the request to
choose matching change groups and paths. A keep clause such as
`but keep dashboard` excludes matching groups.

Selected paths are restored to their whole-file state at the Run start. This
fallback is conservative and predictable, but it cannot separate wanted and
unwanted edits that share a selected file. Inspect groups with:

```bash
undo run show r_421
```

For exact matching, Undo groups Run changes under stable path-derived IDs and
scores normalized request terms against group IDs, labels, and paths. This
fallback is deterministic; `undo ask` does not use LLM semantic inference.

## Review and apply the plan

`undo ask` previews by default and saves the exact plan:

```bash
undo ask r_421 "remove auth"
# review rec_812
undo apply rec_812
```

Review every proposed write and deletion before applying. The Recovery remains
available for 24 hours. Applying it again after success is a safe no-op.

### Exact apply checks

Each Recovery entry records whether the current path exists and its SHA-256
hash. Before writing anything, apply checks the complete plan:

- the Recovery must still be `planned` and unexpired;
- no ambiguity may be recorded;
- every path must still match its expected existence state and hash;
- every target snapshot must still be readable.

If any path changed after preview, no entry is applied and the Recovery becomes
`conflicted`. Create a new preview.

The compatibility shortcut remains available:

```bash
undo ask r_421 "remove auth" --apply --yes
```

It creates and immediately applies a Recovery, so the separate preview/apply
flow is preferable when a human is reviewing the plan.

## Reverse a whole Run or path group

`undo recover` selects an entire Run or one deterministic path group without
natural-language matching:

```bash
undo recover --run r_421 --preview
undo recover --run r_421 --group auth --preview
undo recover --run r_421 --group auth --yes
```

`--session` is an alias for `--run`. This path uses whole-file Run baselines; it
does not use explicit intent inverse patches.

## Recover from continuous history

Runs are not required for local recovery.

### Time and path

```bash
undo what-changed 30m
undo diff src/server.rs 30m
undo preview src/server.rs 30m
undo restore src/server.rs 30m
```

Directories are planned as a set of writes and deletions. Multi-file direct
restores require `--yes`.

Relative durations are evaluated separately by each command. For a stable
boundary between preview and restore, use the same Unix timestamp:

```bash
undo restore . --timestamp 1713200000 --preview
undo restore . --timestamp 1713200000 --yes
```

Unlike a persisted `rec_...` plan, separate direct preview and restore commands
do not hash-lock the current files between invocations.

### Named checkpoints

A **checkpoint** is a named point in history. Create one before risky work, then
preview or restore that point later:

```bash
undo checkpoint before-refactor
undo restore . --checkpoint before-refactor --preview
undo restore . --checkpoint before-refactor --yes
```

New checkpoints restore at an immutable event-ID boundary. Legacy checkpoints
without an event ID use their stored timestamp.
Checkpoint labels may repeat across Runs. When a label is ambiguous, use the
`cp_...` ID shown by `undo checkpoints` or `undo run show`.

### Deleted files

```bash
undo deleted
undo restore-deleted src/old-api.rs
# equivalent:
undo restore src/old-api.rs --deleted
```

Deletion recovery requires the last captured contents to remain in retention.

### Panic recovery

```bash
undo panic
```

The dashboard is read-only. It shows recent bursts, deleted files, checkpoints,
and exact-timestamp preview/restore commands. To create and apply a persisted
Recovery for the latest detected burst:

```bash
undo panic --restore-before-latest-burst --yes
```

Panic mode is an emergency heuristic. Prefer a Run or checkpoint when available.

## Backups and file safety

Undo checks a persisted Recovery as a complete plan and backs up existing files
before replacing or deleting them. Each individual write is published through a
temporary sibling file and rename, so a target is not left partially written.
Backups use collision-safe names under `~/.undo/backups/`.

The multi-file mutation itself runs one entry at a time; it is not a single
transaction. An I/O failure after earlier entries succeed can leave a partially
applied multi-file Recovery. Backups remain available for entries that were
overwritten or deleted.

Retention limits recovery to content Undo still has. Ignored, unreadable,
non-UTF-8-path, missing-snapshot, corrupt-snapshot, and files over 100 MiB are
not guaranteed recoverable.

Overwriting an existing file preserves its current Unix mode. Recreating a
missing file uses owner-only mode `0600`; historical executable bits are not
currently stored with snapshots.
