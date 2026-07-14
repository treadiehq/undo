# CLI reference

Use this reference to run a command under Undo, inspect what changed, preview a
recovery, and apply it. For the shortest path, run an agent with `undo run`,
inspect it with `undo run show`, then use `undo ask` and `undo apply`.

Run `undo <command> --help` for exact syntax. `--verbose` is a global flag; it
currently shows each recorded change when used with `undo start`.

## Run an agent or command

### `undo run <command> [args...]`

Use this form to record one child process as a Run:

```bash
undo run claude
undo run opencode
undo run codex -- --full-auto
```

These commands start recording if needed and leave a Run you can inspect or
recover. Undo infers the three listed agent identities from the executable name.
Other commands are attributed as tools unless overridden through `run exec`.

The Run completes when the child exits: status is `completed` for exit 0 and
`failed` otherwise. A nonzero child causes the Undo wrapper itself to exit with
status 1.

For argument forwarding, the shorthand consumes its first standalone `--` as
Undo's separator. Use `undo run exec -- <command>` when the child itself must
receive a later literal `--`, such as
`undo run exec -- cargo test -- --nocapture`.

### `undo run exec [options] -- <command> [args...]`

Use `run exec` for an arbitrary child or to attach metadata:

```bash
undo run exec --agent "My Agent" --name auth-work --intent "Refactor auth" -- my-agent
```

Options are `--agent <name>`, `--name <name>`, and `--intent <text>`.

### `undo run start [options]`

Use this when another process will do the work but you still want a Run
boundary:

```bash
undo run start --name auth-work --actor agent --agent "My Agent"
```

Options:

- `--name <name>`; otherwise Undo generates one.
- `--actor <human|agent|tool|mixed>`; defaults to `agent` when `--agent` is set,
  otherwise `human`.
- `--agent <name>`.
- `--intent <text>` for descriptive initial metadata.
- `--external-id <id>` for integration deduplication.
- `--json` for machine output.

The command starts recording if needed and synchronizes the project before
opening the boundary. It fails if another Run is active in the project.

### `undo run stop [RUN] [options]`

Complete a Run after synchronizing its latest filesystem changes. Without a
reference, this uses the active Run.

Options:

- `--status <completed|failed|aborted>`; default `completed`.
- `--json`.

### `undo run show <RUN> [--json]`

Inspect Run metadata, changes, checkpoints, and explicit intents. A reference
may be an `r_...` ID or an older Run/session name.

### `undo run list [--json]` and `undo runs [--json]`

List Runs newest first. The two forms are equivalent.

## Preview and apply recovery

### `undo ask [RUN] <QUERY> [options]`

Describe the work to remove and create a persisted, preview-first Recovery:

```bash
undo ask r_421 "remove auth but keep dashboard"
undo ask "remove auth" --run r_421
```

If no Run is supplied, Undo selects the latest completed Run. Options:

- `--run <RUN>`; `--session` is an alias.
- `--apply` to apply immediately.
- `--yes`, required with `--apply`.

Do not provide both the positional Run and `--run`.

### `undo apply <RECOVERY>`

Apply the exact persisted `rec_...` plan shown by `undo ask`. Apply rejects
expired, ambiguous, or hash-conflicted Recoveries and is a no-op after prior
success.

### `undo recover --run <RUN> [options]`

Preview or apply a whole-Run or whole-group baseline recovery:

```bash
undo recover --run r_421 --preview
undo recover --run r_421 --group auth --preview
undo recover --run r_421 --group auth --yes
```

`--session` aliases `--run`. `--preview` writes nothing. Without `--preview`,
`--yes` is required.

## Add precise boundaries from another tool

### `undo event [--json <OBJECT>]`

Send one strict, versioned lifecycle JSON object from `--json` or stdin and
receive one JSON response:

```bash
printf '%s' '{"version":1,"event":"run_started","idempotency_key":"task-42-start","agent":"Claude Code"}' | undo event
```

See the exact [Agent Integration Spec](agent-integration-spec.md).

### `undo checkpoint <NAME> [options]`

Create an immutable boundary after synchronizing the project at the current
event ID:

```bash
undo checkpoint before-migration
undo checkpoint validated --run r_421 --intent "Auth tests pass" --json
```

Options:

- `--run <RUN>`; otherwise the active Run is used when one exists.
- `--intent <text>` for checkpoint metadata. This is not an explicit intent
  interval.
- `--json`.

`undo mark` is an alias.

### `undo checkpoints`

List project checkpoints. `undo marks` is an older alias.

## Inspect and restore recorded history

### `undo start [--force]`

Start recording the current folder in the foreground. Run wrappers normally
start recording for you. For a manually managed recorder, use another terminal,
a process manager, or shell backgrounding:

```bash
undo start &
```

`--force` bypasses root/ownership, overlap, and 50,000-file startup guards. Run
wrappers start this recorder automatically when needed, without `--force`.

### `undo timeline [options]`

Show recent filesystem events.

- `--limit <N>`; default 20, minimum 1.
- `--since <duration>`, such as `30m`, `2h`, or `1d`.
- `--bursts` to summarize rapid large changes.
- `--deleted` to show only deletion events.

### `undo what-changed <duration>`

Show the latest event type for each changed path in a recent window.

### `undo diff <PATH> [duration] [options]`

Compare a file with a saved version. Without a duration or checkpoint, Undo
uses the latest recorded version.

- `--checkpoint <name-or-cp-id>` / `-c` / `--cp` selects the diff by checkpoint
  timestamp and cannot be combined with a duration.
- `--summary` prints only line counts.
- `--stat` prints line counts before the unified diff.

### `undo preview <PATH> <duration>`

Preview the same time-based restore plan used by `undo restore`, without writes.

### `undo restore <PATH> <target> [options]`

Restore a file or directory. Select exactly one target:

- a relative duration, such as `10m`;
- `--checkpoint <name-or-cp-id>` / `-c` / `--cp`;
- `--timestamp <UNIX_SECONDS>`;
- `--deleted` for the latest retained deleted version of one path.

Other options:

- `--preview` writes nothing.
- `--yes` is required when a plan changes more than one file.

For modern checkpoints, restore uses the event-ID anchor. Older checkpoints
without an event anchor fall back to their timestamp. This differs from
`undo diff --checkpoint`, which selects by timestamp.
Labels may repeat across Runs; use the displayed `cp_...` ID when a label is
ambiguous.

### `undo deleted [--limit <N>]`

List recent deleted paths whose previous snapshot is still referenced. Default
limit is 20; minimum is 1.

### `undo restore-deleted <PATH>`

Recover the latest captured contents of a deleted file. This is equivalent to
`undo restore <PATH> --deleted`.

### `undo panic [options]`

Show the read-only emergency dashboard. It examines the last 24 hours for change
bursts and prints exact-timestamp recovery commands.

`--restore-before-latest-burst --yes` creates and applies a Recovery for the
latest detected burst. `--undo-burst` aliases the long restore flag.

## Manage recording and storage

### `undo status`

Show the matching watched project, recorder liveness, database and snapshot
counts, deep snapshot integrity result, retention settings, disk use, and log
path.

### `undo stop [--all]`

Stop the recorder for the current project. `--all` stops every live recorder
known under `~/.undo/pids/`.

### `undo prune [options]`

Delete expired events, unreferenced snapshots, stale snapshot temp files, and
expired backups.

- `--keep <duration>` overrides the retention window for this invocation.
- `--dry-run` reports without deleting.

### `undo update`

Check GitHub for the latest release, refuse downgrades, download the matching
artifact and `SHA256SUMS`, verify the checksum, and replace the executable.
This is Undo's network-using CLI command; the documented installer is the other
network exception.

## Use older session commands

The older, pre-Run surface remains available:

```bash
undo session start legacy-work
undo session stop
undo session show legacy-work
undo sessions
```

These commands use the same `sessions` table as Runs. `undo sessions` lists Runs.
The older session start/show output remains for compatibility; prefer
`undo run ...`, `undo runs`, and `undo run show ...` in new integrations.
