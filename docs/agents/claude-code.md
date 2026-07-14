# Claude Code

## Start Claude Code with Undo

Expected result: Claude's filesystem changes are grouped into one Run that you
can inspect and recover after Claude exits.

From the project you want Undo to protect, run:

```bash
undo run claude
```

Pass Claude arguments after `--`:

```bash
undo run claude -- --help
```

Undo starts its recorder if needed, waits for the initial scan, opens a Run
attributed to `Claude Code`, launches the child, and completes the Run when
Claude exits.

After Claude exits, inspect and recover the Run:

```bash
undo runs
undo run show r_421
undo ask r_421 "remove the auth migration work"
undo apply rec_812
```

If Claude exits nonzero, the Run is retained with status `failed` and the wrapper
exits with status 1. You can still inspect and recover it.

Running `claude` directly while Undo records still preserves local filesystem
history, but Undo cannot reliably label those events as Claude Code work without
the wrapper or lifecycle events.

## Optional advanced lifecycle integration

Expected result: checkpoints and explicit intent boundaries let Undo identify
smaller tasks inside a Run, including clean, non-overlapping work in a shared
file.

Use this optional path only when an orchestrator or hook system can invoke shell
commands at meaningful lifecycle points. `undo event` creates the precise
boundaries. Undo does not install or claim a built-in Claude Code hook
integration; the following are generic CLI calls for a lifecycle system you
control.

Start and retain the returned Run ID:

```bash
START=$(undo event --json '{"version":1,"event":"run_started","idempotency_key":"claude-job-784-start","name":"Claude job 784","agent":"Claude Code","command":"claude"}')
RUN_ID=$(printf '%s' "$START" | python3 -c 'import json,sys; print(json.load(sys.stdin)["run_id"])')
```

Create an immutable checkpoint:

```bash
undo event --json "{\"version\":1,\"event\":\"checkpoint\",\"idempotency_key\":\"claude-job-784-checkpoint-tests\",\"run_id\":\"$RUN_ID\",\"name\":\"tests-pass\",\"intent\":\"Baseline tests pass\"}"
```

Mark one task precisely:

```bash
undo event --json "{\"version\":1,\"event\":\"intent_started\",\"idempotency_key\":\"claude-job-784-auth-start\",\"run_id\":\"$RUN_ID\",\"intent\":\"Auth migration\"}"

# Let Claude perform only the Auth migration here.

undo event --json "{\"version\":1,\"event\":\"intent_completed\",\"idempotency_key\":\"claude-job-784-auth-end\",\"run_id\":\"$RUN_ID\",\"intent\":\"Auth migration\"}"
```

Complete the Run:

```bash
undo event --json "{\"version\":1,\"event\":\"run_completed\",\"idempotency_key\":\"claude-job-784-end\",\"run_id\":\"$RUN_ID\",\"status\":\"completed\"}"
```

Use globally unique, stable idempotency keys for each lifecycle event. Send
`failed` or `aborted` instead of `completed` when that reflects the orchestrator
outcome.

Explicit intent boundaries are what allow Undo to reverse one Claude task in a
shared file while preserving clean, non-overlapping later edits. Overlap remains
a conflict and blocks apply.
