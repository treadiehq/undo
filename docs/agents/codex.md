# Codex

## Start Codex with Undo

Expected result: Codex's filesystem changes are grouped into one Run that you
can inspect and recover after Codex exits.

From the project root, launch Codex through Undo:

```bash
undo run codex
```

For Codex options, separate the child arguments:

```bash
undo run codex -- --help
```

Undo ensures continuous recording, opens a Run attributed to `Codex`, launches
the child, and completes the Run when it exits.

After Codex exits, inspect and recover the Run:

```bash
undo runs
undo run show r_421
undo ask r_421 "remove the generated migration"
undo apply rec_812
```

Nonzero child exit marks the Run `failed`; its recorded changes remain
inspectable. Launching Codex directly still benefits from continuous local
history if Undo is recording, but those changes are not reliably attributable
to Codex.

## Optional advanced lifecycle integration

Expected result: checkpoints and explicit intent boundaries let Undo identify
smaller tasks inside a Run, including clean, non-overlapping work in a shared
file.

Use this optional path when CI, an external launcher, or another lifecycle
system can call `undo event` at meaningful boundaries. Undo does not install or
claim a native Codex hook integration. The calls below are building blocks for
orchestration you control.

Start the Run and capture its ID:

```bash
START=$(undo event --json '{"version":1,"event":"run_started","idempotency_key":"codex-job-784-start","name":"Codex job 784","agent":"Codex","command":"codex"}')
RUN_ID=$(printf '%s' "$START" | python3 -c 'import json,sys; print(json.load(sys.stdin)["run_id"])')
```

Create a checkpoint:

```bash
undo event --json "{\"version\":1,\"event\":\"checkpoint\",\"idempotency_key\":\"codex-job-784-checkpoint-green\",\"run_id\":\"$RUN_ID\",\"name\":\"green-tests\",\"intent\":\"Tests green before refactor\"}"
```

Mark the start and end of one task:

```bash
undo event --json "{\"version\":1,\"event\":\"intent_started\",\"idempotency_key\":\"codex-job-784-cache-start\",\"run_id\":\"$RUN_ID\",\"intent\":\"Cache refactor\"}"

# Let Codex perform only the Cache refactor here.

undo event --json "{\"version\":1,\"event\":\"intent_completed\",\"idempotency_key\":\"codex-job-784-cache-end\",\"run_id\":\"$RUN_ID\",\"intent\":\"Cache refactor\"}"
```

Complete with the outcome reported by the orchestrator:

```bash
undo event --json "{\"version\":1,\"event\":\"run_completed\",\"idempotency_key\":\"codex-job-784-end\",\"run_id\":\"$RUN_ID\",\"status\":\"completed\"}"
```

The same idempotency key may be retried for the same logical event; do not reuse
it for another boundary. Only one Run and one explicit intent may be active in a
project/Run respectively.

An explicit completed intent lets Undo inverse-apply that task's patch to the
current file. Clean later edits survive; overlapping later edits make the
Recovery ambiguous and block apply.
