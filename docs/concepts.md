# Concepts

Undo continuously records local file history. Four terms describe the safer,
more selective workflow built on that history:

- A **Run** is a recorded work window.
- An **intent** is a marked task inside a Run.
- A **checkpoint** is a named point in history.
- A **Recovery** is a saved, expiring plan of file writes and deletions.

## Record work for later recovery

### Continuous history

Undo's recorder watches a project and stores content-addressed snapshots plus
filesystem events for created, modified, deleted, and renamed files. An initial
scan establishes or refreshes the baseline. This history exists independently
of Runs, so time-, path-, deleted-file-, checkpoint-, and panic recovery still
work for human edits and unwrapped tools.

The recorder observes files, not process ownership. It cannot infer which of
several concurrent programs caused a filesystem event.

### Runs

A Run groups the changes observed while a task or process is active. This makes
the work easy to find, inspect, and offer as a recovery target.

Each Run records:

- a stable public ID such as `r_421`;
- an actor (`human`, `agent`, `tool`, or `mixed`);
- optional agent, command, name, initial intent, and external integration ID;
- start and end event IDs;
- `active`, `completed`, `failed`, or `aborted` status.

`undo run claude` is the simplest Run integration. It ensures recording is
active, synchronizes the project, opens the Run, launches the child, synchronizes
again, and completes the Run when the child exits.

Undo allows one active Run per project. This is a deliberate attribution
boundary: filesystem events cannot reliably identify which concurrent actor
caused a change. Different projects can have active Runs and record history at
the same time.

The on-disk table remains named `sessions` so existing installations migrate
without rewriting history. `undo session ...`, `undo sessions`, and `--session`
flags remain compatibility aliases backed by that table. New documentation and
IDs use the Run model.

## Mark work you may want to revisit

### Intents

An intent marks one task inside an active Run. An agent integration starts the
intent immediately before the task and completes it immediately afterward.
These boundaries can let Undo reverse that task without discarding separate
later edits in the same file.

Only one intent can be active in a Run. Completed intent IDs use `i_...`. The
optional initial `intent` metadata on `run_started` or `undo run start` only
describes the Run; it does not create a bounded intent by itself.

For exact boundaries, integrations send `intent_started` and
`intent_completed`. Undo synchronizes each boundary and anchors it to a
filesystem event ID. When a completed intent uniquely matches a request, Undo
computes an inverse patch from the file state before and after that intent, then
applies it to the current file. Clean, non-overlapping later edits are
preserved. Overlapping later edits, or ambiguous create/delete/rename states,
block apply.

### Checkpoints

A checkpoint gives a memorable name to a point you may want to return to. It can
belong to a Run or to the project as a whole. Checkpoint IDs use the `cp_...`
form.

Current versions synchronize the project and anchor each new checkpoint to the
greatest recorded filesystem event ID, making it an immutable recovery
boundary. Creating the same name again within the same Run returns the existing
checkpoint instead of moving it. The same name may be used in another Run.

Older database rows created before event anchors were added have no event ID.
Those legacy checkpoints remain timestamp-only and restore using their recorded
time. Retention still controls whether the snapshots needed by either kind of
checkpoint remain available.

## Preview and apply a reversal

`undo ask`, `undo recover --preview`, and panic application planning save the
exact writes and deletions for later review. The resulting Recovery has a public
ID such as `rec_812`, changes no files during preview, and expires after 24
hours.

For exact validation, a Recovery stores:

- the requested reversal and selection confidence;
- each planned write or delete;
- target snapshot hashes;
- the expected current existence state and hash;
- ambiguity, status, creation time, and 24-hour expiration.

`undo apply rec_812` checks every entry before writing. It refuses expired,
ambiguous, or stale plans. After a successful apply, applying the same ID again
is a no-op.

## Know where attribution stops

Agent attribution is reliable when:

- Undo launched the process through `undo run`; or
- an integration sent Run lifecycle events through `undo event`.

Attribution is to the Run window, not proof of process ownership for each write.
Concurrent human or tool writes inside that window can be included, which is why
Undo allows only one active Run but still recommends avoiding unrelated
concurrent edits. Continuous history can recover changes from any process that
writes the project, but those events should not be described as belonging to a
particular agent without a Run boundary. Undo has no universal process-to-file
attribution and does not use an LLM to interpret rollback requests.
