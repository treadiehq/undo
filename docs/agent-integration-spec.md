# Send lifecycle events from an agent

Use `undo event` when an agent launcher, hook, or orchestrator needs precise Run,
checkpoint, or intent boundaries. For a simple child process, `undo run` is the
shorter integration.

## Send one event

Version 1 accepts one lifecycle event either as an argument:

```bash
undo event --json '{"version":1,"event":"run_started","idempotency_key":"task-42-run-start","name":"Task 42"}'
```

or on stdin:

```bash
printf '%s' '{"version":1,"event":"run_started","idempotency_key":"task-43-run-start","name":"Task 43"}' | undo event
```

One invocation accepts one JSON object, performs the boundary operation for the
current project, and prints one JSON object. The command may start Undo's
filesystem recorder when the project is not already recording.

This is a local CLI contract, not a daemon, socket, streaming, or IPC protocol.

## Build a valid input object

The top-level value must be a strict JSON object. Unknown fields, missing
required fields, duplicate field names, wrong JSON types, malformed JSON, and
multiple concatenated objects are rejected. Trailing whitespace is allowed.

The only accepted fields are:

- `version`
- `event`
- `idempotency_key`
- `run_id`
- `name`
- `actor`
- `agent`
- `command`
- `intent`
- `status`
- `external_run_id`

`version`, `event`, and `idempotency_key` are required. Every other field is an
optional string and may be omitted or set to `null`.

Every string limit below is measured on the decoded string in UTF-8 bytes, not
Unicode scalar values.

### `version`

Required unsigned 32-bit JSON integer. It must equal `1`.

### `event`

Required string. It must be exactly one of:

- `run_started`
- `checkpoint`
- `intent_started`
- `intent_completed`
- `run_completed`

### `idempotency_key`

Required string. Its trimmed value must not be empty, and the decoded value must
be at most 200 UTF-8 bytes. Keys are global to the local `~/.undo` database, not
scoped to a project.

Generate a distinct key for every logical lifecycle event and reuse that key
only when retrying the same event. Reusing a key with a different payload returns
the first stored response; Undo does not compare the new payload.

### `run_id`

Optional string, maximum 200 bytes. For events after `run_started`, it may be a
public `r_<integer>` ID or an exact legacy Run/session name in the current
project. If omitted, Undo uses the project's active Run.

`run_started` does not use this field; integrations should omit it and retain the
`run_id` from the response.

### `name`

Optional string, maximum 200 bytes.

- `run_started`: human-readable Run name. A missing, empty, or whitespace-only
  value causes Undo to generate a name.
- `checkpoint`: required after trimming and must not be empty.
- Other events: ignored.

### `actor`

Optional string, maximum 32 bytes. For `run_started`, it must be exactly
`human`, `agent`, `tool`, or `mixed`. It defaults to `agent`. Other events ignore
it after the length check.

### `agent`

Optional string, maximum 200 bytes. `run_started` stores it as the agent
identity. Other events ignore it.

### `command`

Optional string, maximum 4096 bytes. `run_started` stores it as descriptive
command metadata. Other events ignore it.

### `intent`

Optional string, maximum 4096 bytes.

- `run_started`: descriptive initial Run metadata only; it does not open an
  explicit intent boundary.
- `checkpoint`: optional checkpoint metadata.
- `intent_started`: required after trimming and must not be empty.
- `intent_completed`: optional. When present, it must equal the active intent
  label; when absent, the active intent is completed.
- `run_completed`: ignored.

### `status`

Optional string, maximum 32 bytes. For `run_completed`, it must be exactly
`completed`, `failed`, or `aborted`; default is `completed`. Other events ignore
it after the length check.

### `external_run_id`

Optional string, maximum 200 bytes. It is valid only for `run_started`; sending
it with any other event is rejected.

The external ID is unique per project. If omitted, `run_started` uses the
`idempotency_key` as its external ID. Repeating `run_started` with the same
external ID returns the existing Run instead of creating another.

## Select the Run safely

- A project allows one active Run because filesystem events cannot reliably
  attribute concurrent actors.
- `run_started` creates the Run, or reuses one with the same external ID, and
  returns its public ID.
- `checkpoint`, `intent_started`, and `intent_completed` require the selected
  Run to be active.
- `run_completed` completes the referenced Run or, when omitted, the active Run.
  Completing an already completed referenced Run returns that existing Run.
- All resolution is within the project containing the process's current working
  directory.
- Completing a Run first completes any active intent at the current synchronized
  boundary.

## Handle each event

All newly processed successful responses include `"version":1`, the processed
`event`, and a public `run_id`. A reused idempotency key returns its first stored
response regardless of the retry payload.

### `run_started`

Undo ensures recording is ready, synchronizes the project, and opens the Run at
the current maximum filesystem event ID.

Input:

```json
{"version":1,"event":"run_started","idempotency_key":"task-42-run-start","name":"Auth refactor","actor":"agent","agent":"Claude Code","command":"claude"}
```

Response:

```json
{"version":1,"event":"run_started","run_id":"r_421","status":"active"}
```

### `checkpoint`

Undo synchronizes first, then creates an immutable checkpoint at the current
maximum filesystem event ID. Checkpoint names are unique within a Run; repeating
the same name returns the existing boundary.

Input:

```json
{"version":1,"event":"checkpoint","idempotency_key":"task-42-tests-pass","run_id":"r_421","name":"tests-pass","intent":"Auth tests pass"}
```

Response:

```json
{"version":1,"event":"checkpoint","run_id":"r_421","checkpoint_id":"cp_31","created":true}
```

### `intent_started`

Undo synchronizes, then opens one explicit intent at the current maximum event
ID. A Run can have only one active intent. Starting the same label again while
it remains active returns that intent; starting a different label fails.

Input:

```json
{"version":1,"event":"intent_started","idempotency_key":"task-42-auth-start","run_id":"r_421","intent":"Auth migration"}
```

Response:

```json
{"version":1,"event":"intent_started","run_id":"r_421","intent_id":"i_52","intent":"Auth migration"}
```

### `intent_completed`

Undo synchronizes, then closes the active intent at the current maximum event
ID.

Input:

```json
{"version":1,"event":"intent_completed","idempotency_key":"task-42-auth-end","run_id":"r_421","intent":"Auth migration"}
```

Response:

```json
{"version":1,"event":"intent_completed","run_id":"r_421","intent_id":"i_52","intent":"Auth migration"}
```

### `run_completed`

Undo synchronizes, closes any active intent, and completes the Run.

Input:

```json
{"version":1,"event":"run_completed","idempotency_key":"task-42-run-end","run_id":"r_421","status":"completed"}
```

Response:

```json
{"version":1,"event":"run_completed","run_id":"r_421","status":"completed"}
```

## Retry without duplicating boundaries

After a successful event response is stored, retrying the same
`idempotency_key` prints the exact stored JSON response and does not process the
new payload. This applies across invocations and projects sharing `~/.undo`.

The operation occurs before the integration response is stored. A hard process
interruption in that narrow interval can leave an operation without its cached
response. Run external IDs, immutable checkpoint names, active-intent checks,
and idempotent Run completion reduce duplicate effects, but integrations should
still use stable keys and inspect the returned Run after an interrupted retry.

## Copy a complete lifecycle

This example starts a Run, creates a checkpoint, records one explicit intent,
and completes the Run. It uses `--json`; any event can instead be supplied on
stdin.

```bash
START=$(undo event --json '{"version":1,"event":"run_started","idempotency_key":"job-784-run-start","name":"Agent job 784","agent":"My Agent"}')
RUN_ID=$(printf '%s' "$START" | python3 -c 'import json,sys; print(json.load(sys.stdin)["run_id"])')

undo event --json "{\"version\":1,\"event\":\"checkpoint\",\"idempotency_key\":\"job-784-baseline\",\"run_id\":\"$RUN_ID\",\"name\":\"baseline\"}"

undo event --json "{\"version\":1,\"event\":\"intent_started\",\"idempotency_key\":\"job-784-auth-start\",\"run_id\":\"$RUN_ID\",\"intent\":\"Auth migration\"}"
# integration performs the Auth migration here
undo event --json "{\"version\":1,\"event\":\"intent_completed\",\"idempotency_key\":\"job-784-auth-end\",\"run_id\":\"$RUN_ID\",\"intent\":\"Auth migration\"}"

undo event --json "{\"version\":1,\"event\":\"run_completed\",\"idempotency_key\":\"job-784-run-end\",\"run_id\":\"$RUN_ID\",\"status\":\"completed\"}"
```

Errors exit with status 1 and human-readable text rather than a JSON error
object.
