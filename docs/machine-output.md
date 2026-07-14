# Machine output

Use a command's documented `--json` mode when a script needs stable data.
`undo event` also returns JSON on success without an output flag. These commands
emit one compact JSON value on stdout.

Do not parse Undo's human-readable output: it is for people and is not a stable
machine contract. JSON field order is also not a contract. On failure, Undo
exits with status 1 and writes a human-readable error; there is no JSON error
envelope.

## Keep the public IDs

Store and pass the public string returned by Undo:

- Runs: `r_<integer>`
- checkpoints: `cp_<integer>`
- explicit intents: `i_<integer>`
- Recoveries: `rec_<integer>`

Nested serialized records also expose their raw integer `id`, but integrations
should retain the public string.

## Read a Run record

A serialized Run has these fields:

```json
{
  "id": 421,
  "project_id": 7,
  "name": "claude-code-...",
  "kind": "run",
  "actor": "agent",
  "agent": "Claude Code",
  "command": "claude",
  "intent": null,
  "external_id": null,
  "status": "completed",
  "started_at": 1784000000,
  "ended_at": 1784000120,
  "start_event_id": 100,
  "end_event_id": 118,
  "created_at": 1784000000
}
```

Timestamps are Unix seconds. Optional fields serialize as `null`.

## Start a Run from a script

`undo run start --json` returns:

```json
{
  "event": "run_started",
  "run": {"id": 421, "project_id": 7, "name": "auth-work", "kind": "run", "actor": "agent", "agent": "My Agent", "command": null, "intent": null, "external_id": null, "status": "active", "started_at": 1784000000, "ended_at": null, "start_event_id": 100, "end_event_id": null, "created_at": 1784000000},
  "run_id": "r_421",
  "project": "/absolute/project/path"
}
```

## Stop a Run from a script

`undo run stop --json` returns:

```json
{
  "event": "run_completed",
  "run": {"id": 421, "project_id": 7, "name": "auth-work", "kind": "run", "actor": "agent", "agent": "My Agent", "command": null, "intent": null, "external_id": null, "status": "completed", "started_at": 1784000000, "ended_at": 1784000120, "start_event_id": 100, "end_event_id": 118, "created_at": 1784000000},
  "run_id": "r_421"
}
```

## List Runs from a script

`undo runs --json` and `undo run list --json` are equivalent. The output is a
JSON array of objects containing the public ID and Run record, newest first:

```json
[{"run_id":"r_421","run":{"id":421,"project_id":7,"name":"auth-work","kind":"run","actor":"agent","agent":"My Agent","command":null,"intent":null,"external_id":null,"status":"completed","started_at":1784000000,"ended_at":1784000120,"start_event_id":100,"end_event_id":118,"created_at":1784000000}}]
```

An empty list is `[]`.

## Inspect one Run from a script

`undo run show <RUN> --json` returns:

```json
{
  "run_id": "r_421",
  "run": {"id": 421, "project_id": 7, "name": "auth-work", "kind": "run", "actor": "agent", "agent": "My Agent", "command": null, "intent": null, "external_id": null, "status": "completed", "started_at": 1784000000, "ended_at": 1784000120, "start_event_id": 100, "end_event_id": 118, "created_at": 1784000000},
  "changes": [],
  "intents": [],
  "checkpoints": []
}
```

`run` is a Run record. A change record contains:

```json
{
  "id": 101,
  "project_id": 7,
  "timestamp": 1784000005,
  "path": "/absolute/project/path/src/auth.rs",
  "event_type": "MODIFIED",
  "current_hash": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "previous_hash": null,
  "snapshot_path": "/Users/me/.undo/snapshots/7/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.gz",
  "old_path": null,
  "file_size": 1234
}
```

An intent record contains `id`, `run_id`, `label`, `status`,
`start_event_id`, `end_event_id`, `started_at`, and `ended_at`.

A checkpoint record contains `id`, `project_id`, `run_id`, `name`, `timestamp`,
`event_id`, `intent`, and `created_at`.

## Create a checkpoint from a script

`undo checkpoint <NAME> --json` returns:

```json
{
  "checkpoint": {
    "id": 31,
    "project_id": 7,
    "run_id": 421,
    "name": "tests-pass",
    "timestamp": 1784000060,
    "event_id": 110,
    "intent": "Auth tests pass",
    "created_at": 1784000060
  },
  "checkpoint_id": "cp_31",
  "created": true
}
```

Creating the same checkpoint name at the same Run/project scope returns the
existing record with `"created": false`.

## Send lifecycle events

`undo event` always prints JSON on success, without a separate output flag. The
response depends on the input event:

```json
{"version":1,"event":"run_started","run_id":"r_421","status":"active"}
```

```json
{"version":1,"event":"checkpoint","run_id":"r_421","checkpoint_id":"cp_31","created":true}
```

```json
{"version":1,"event":"intent_started","run_id":"r_421","intent_id":"i_52","intent":"Auth migration"}
```

```json
{"version":1,"event":"intent_completed","run_id":"r_421","intent_id":"i_52","intent":"Auth migration"}
```

```json
{"version":1,"event":"run_completed","run_id":"r_421","status":"completed"}
```

Retries with a previously recorded idempotency key return the stored response.
See [Agent Integration Spec](agent-integration-spec.md) for input validation and
retry rules.

## Know which output is human-only

`undo ask` and `undo apply` currently emit human-readable text only, including
the persisted `rec_...` ID. Direct history commands such as `timeline`, `diff`,
`restore`, `status`, and `panic` also have no JSON mode. Their wording and layout
may change.

Integrations that need a stable lifecycle contract should use `undo event` and
the documented Run/checkpoint JSON commands. Do not extract values from
human-readable output.
