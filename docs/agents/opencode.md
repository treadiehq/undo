# OpenCode

## Start OpenCode with Undo

Expected result: OpenCode's filesystem changes are grouped into one Run that you
can inspect and recover after OpenCode exits.

From the target project, run:

```bash
undo run opencode
```

Pass OpenCode arguments after `--`:

```bash
undo run opencode -- --help
```

Undo starts recording if needed, opens a Run attributed to `OpenCode`, launches
the child, and completes the Run on exit.

After OpenCode exits, inspect and recover the Run:

```bash
undo runs
undo run show r_421
undo ask r_421 "remove the database migration work"
undo apply rec_812
```

A failed child remains inspectable as a Run with status `failed`. If OpenCode is
started without the wrapper, Undo can still capture its file changes through
continuous history, but agent attribution is not reliable.

## Optional advanced lifecycle integration

Expected result: explicit intent boundaries let Undo identify smaller tasks
inside a Run; checkpoints preserve named boundaries for inspection and restore.

Use this optional path when an external OpenCode launcher, plugin, or hook system
can run shell commands at meaningful lifecycle points. Undo does not provide or
claim a native OpenCode plugin or hook package; these examples call the
implemented CLI directly.

Start a Run:

```bash
START=$(undo event --json '{"version":1,"event":"run_started","idempotency_key":"opencode-job-784-start","name":"OpenCode job 784","agent":"OpenCode","command":"opencode"}')
RUN_ID=$(printf '%s' "$START" | python3 -c 'import json,sys; print(json.load(sys.stdin)["run_id"])')
```

Anchor a checkpoint:

```bash
undo event --json "{\"version\":1,\"event\":\"checkpoint\",\"idempotency_key\":\"opencode-job-784-checkpoint-baseline\",\"run_id\":\"$RUN_ID\",\"name\":\"baseline\",\"intent\":\"Before generated API work\"}"
```

Bound one intent:

```bash
undo event --json "{\"version\":1,\"event\":\"intent_started\",\"idempotency_key\":\"opencode-job-784-api-start\",\"run_id\":\"$RUN_ID\",\"intent\":\"Generate API client\"}"

# Let OpenCode perform only this intent here.

undo event --json "{\"version\":1,\"event\":\"intent_completed\",\"idempotency_key\":\"opencode-job-784-api-end\",\"run_id\":\"$RUN_ID\",\"intent\":\"Generate API client\"}"
```

Complete the Run:

```bash
undo event --json "{\"version\":1,\"event\":\"run_completed\",\"idempotency_key\":\"opencode-job-784-end\",\"run_id\":\"$RUN_ID\",\"status\":\"completed\"}"
```

Keys must be globally unique in the local Undo database and stable across
retries. Use a distinct key for each boundary.

With explicit completed intents, `undo ask` first attempts a unique label match
and same-file inverse patch recovery. Without them, recovery uses deterministic
path groups and whole-file Run baselines.
