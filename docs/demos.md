# Reproducible demos

Use these exercises to see the recovery result before connecting Undo to a real
agent. Each demo uses a temporary project and scripted changes, so the result
does not depend on an agent model.

Run each demo separately with `undo` installed. Replace each `rec_...`
placeholder with the Recovery ID printed by `undo ask`.

The demos use the same recorder, Run boundaries, planner, and apply path used
with real agents. Demo 2 also uses the optional, advanced lifecycle event API.

## Demo 1: keep dashboard work while removing auth

Expected result: auth returns to `provider=password`, while the newer dashboard
title remains `Realtime Dashboard`.

Create a project with two known-good files:

```bash
DEMO_DIR=$(mktemp -d)
cd "$DEMO_DIR"
git init
mkdir -p app/dashboard app/auth
printf 'title=Classic Dashboard\n' > app/dashboard/panel.conf
printf 'provider=password\n' > app/auth/login.conf
```

Run a reproducible "agent" command that changes both areas:

```bash
undo run exec --agent "Demo Agent" --name dashboard-and-auth -- sh -c '
  printf "title=Realtime Dashboard\n" > app/dashboard/panel.conf
  printf "provider=broken-oauth\n" > app/auth/login.conf
'
```

Find and inspect the completed Run:

```bash
undo runs
RUN_ID=$(undo runs --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["run_id"])')
undo run show "$RUN_ID"
```

Ask to remove auth and preserve dashboard:

```bash
undo ask "$RUN_ID" "remove auth but keep dashboard"
```

The preview should say it matched file and folder names, restore only
`app/auth/login.conf`, and leave `app/dashboard/panel.conf` out of the plan.
Apply the printed Recovery ID:

```bash
undo apply rec_...
cat app/auth/login.conf
cat app/dashboard/panel.conf
```

Expected contents:

```text
provider=password
title=Realtime Dashboard
```

Why it works: when a Run has no explicit intent boundaries, Undo uses
deterministic path groups. Here those groups are `auth` and `dashboard`. There
is no semantic model involved. Because the wanted and unwanted work use
different files, whole-file baseline recovery is sufficient.

Clean up the recorder before deleting the temporary project:

```bash
undo stop
```

## Demo 2: remove one task from a shared file

Expected result: `mode = legacy` returns, while `title = Live` and
`refresh_seconds = 5` remain.

This demo uses optional, advanced lifecycle events to put precise boundaries
around two tasks that edit the same file. A normal `undo run` wrapper does not
require these events.

Create one configuration file with distant auth and dashboard sections:

```bash
DEMO_DIR=$(mktemp -d)
cd "$DEMO_DIR"
git init
cat > service.conf <<'EOF'
[auth]
mode = legacy
issuer = internal

# separation 01
# separation 02
# separation 03
# separation 04
# separation 05
# separation 06
# separation 07
# separation 08

[dashboard]
title = Classic
refresh_seconds = 60
EOF
```

Start an integrated Run with a unique key:

```bash
DEMO_KEY="$(date +%s)-$$"
START=$(undo event --json "{\"version\":1,\"event\":\"run_started\",\"idempotency_key\":\"demo-$DEMO_KEY-run-start\",\"name\":\"same-file-intents\",\"agent\":\"Demo Agent\"}")
RUN_ID=$(printf '%s' "$START" | python3 -c 'import json,sys; print(json.load(sys.stdin)["run_id"])')
```

Bound and perform the first intent:

```bash
undo event --json "{\"version\":1,\"event\":\"intent_started\",\"idempotency_key\":\"demo-$DEMO_KEY-auth-start\",\"run_id\":\"$RUN_ID\",\"intent\":\"Auth migration\"}"

python3 - <<'PY'
from pathlib import Path
p = Path("service.conf")
p.write_text(p.read_text().replace("mode = legacy", "mode = oauth"))
PY

undo event --json "{\"version\":1,\"event\":\"intent_completed\",\"idempotency_key\":\"demo-$DEMO_KEY-auth-end\",\"run_id\":\"$RUN_ID\",\"intent\":\"Auth migration\"}"
```

Bound and perform a later intent in the same file:

```bash
undo event --json "{\"version\":1,\"event\":\"intent_started\",\"idempotency_key\":\"demo-$DEMO_KEY-dashboard-start\",\"run_id\":\"$RUN_ID\",\"intent\":\"Dashboard refresh\"}"

python3 - <<'PY'
from pathlib import Path
p = Path("service.conf")
p.write_text(
    p.read_text()
    .replace("title = Classic", "title = Live")
    .replace("refresh_seconds = 60", "refresh_seconds = 5")
)
PY

undo event --json "{\"version\":1,\"event\":\"intent_completed\",\"idempotency_key\":\"demo-$DEMO_KEY-dashboard-end\",\"run_id\":\"$RUN_ID\",\"intent\":\"Dashboard refresh\"}"
undo event --json "{\"version\":1,\"event\":\"run_completed\",\"idempotency_key\":\"demo-$DEMO_KEY-run-end\",\"run_id\":\"$RUN_ID\",\"status\":\"completed\"}"
```

Recover only the first intent:

```bash
undo run show "$RUN_ID"
undo ask "$RUN_ID" "remove the Auth migration"
undo apply rec_...
cat service.conf
```

Selective same-file reversal uses inverse patch application. It applies the
after-to-before patch for `Auth migration` to the current file, preserving the
clean, non-overlapping dashboard edits. If a later intent had changed the same
auth lines, patch application would fail; the Recovery would be marked ambiguous
and `undo apply` would refuse the entire plan.

```bash
undo stop
```

## Demo 3: restore a destructive deletion burst

Expected result: all three files under `src/critical` return to their captured
contents, and `KEEP.txt` remains unchanged.

This exercise deletes several files, inspects emergency options, and restores
the exact pre-burst state.

Create a project with critical files plus one unaffected file:

```bash
DEMO_DIR=$(mktemp -d)
cd "$DEMO_DIR"
git init
mkdir -p src/critical
printf 'database_url=local\n' > src/critical/config.toml
printf 'pub fn authorize() { /* safe */ }\n' > src/critical/auth.rs
printf 'pub fn migrate() { /* safe */ }\n' > src/critical/migrate.rs
printf 'unrelated and should remain\n' > KEEP.txt
```

Run a destructive command. The delay separates the initial baseline scan from
the deletion burst, matching the panic detector's ten-second grouping rule:

```bash
undo run exec --agent "Demo Agent" --name destructive-delete -- sh -c '
  sleep 11
  rm -rf src/critical
'
```

Inspect the Run and emergency dashboard:

```bash
undo runs
undo deleted
undo panic
```

`undo panic` is read-only. It should report a latest burst with three deletions
and print preview/restore commands using the same exact Unix timestamp. Run the
printed `Preview:` command first if you want to inspect the full project plan.

Create and apply the persisted panic Recovery:

```bash
undo panic --restore-before-latest-burst --yes
```

Verify the result:

```bash
cat src/critical/config.toml
cat src/critical/auth.rs
cat src/critical/migrate.rs
cat KEEP.txt
```

The same captured deletion can also be recovered one file at a time with
`undo restore-deleted <path>`.

Panic recovery is a time/burst heuristic, not agent attribution. Prefer the Run
and explicit-intent paths when they identify the unwanted work. Apply is
preflighted, and existing overwritten/deleted files receive backups, but
multi-file filesystem mutation is not fully transactional.

```bash
undo stop
```
