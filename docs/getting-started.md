# Getting started

## Install

On macOS or Linux:

```bash
curl -fsSL https://useundo.co/install.sh | bash
```

Prebuilt binaries are also available from
[GitHub Releases](https://github.com/treadiehq/undo/releases).

## Undo an agent change

From the project root:

```bash
cd my-project

undo run claude
undo runs
undo run show r_421
undo ask r_421 "remove the auth migration work"
undo apply rec_812
```

A **Run** is a recorded work window. The first command records the work done by
Claude Code as one Run. The next two commands find and inspect it.

`undo ask` turns your request into a preview. It saves that preview as a
**Recovery**: a saved, expiring plan of file writes and deletions. No file
changes until you review the plan and run `undo apply` with its `rec_...` ID.
Use the actual IDs printed by your commands in place of `r_421` and `rec_812`.

## Record the agent

```bash
undo run claude
```

This command:

1. Starts Undo's continuous recorder if the project is not already being
   recorded, and waits until its initial scan is ready.
2. Opens a Run and attributes the child to Claude Code.
3. Launches `claude` with the remaining arguments.
4. Synchronizes filesystem changes and closes the Run when the child exits.

A successful child marks the Run `completed`. A nonzero exit marks it `failed`;
an explicit lifecycle integration can also finish a Run as `aborted`. Only one
Run can be active per project.

The same wrapper pattern works for other tools:

```bash
undo run opencode
undo run codex -- --full-auto
undo run exec --agent "My Agent" -- my-agent --non-interactive
```

Arguments after `--` go to the child command. See the practical guides for
[Claude Code](agents/claude-code.md), [OpenCode](agents/opencode.md), and
[Codex](agents/codex.md).

## Find and inspect the Run

```bash
undo runs
undo run show r_421
```

`undo runs` lists Runs newest first. `undo run show` displays metadata,
checkpoints, explicit intents, and deterministic path-based change groups.
Use the `r_...` ID printed by your commands; names are also accepted where a Run
reference is requested.

## Preview, review, and apply

```bash
undo ask r_421 "remove the auth migration work"
```

The command shows what Undo would write or delete and prints the saved Recovery
ID:

```text
Saved recovery plan rec_812.
...
No files changed.
Apply this exact plan with: undo apply rec_812
```

Review the proposed writes and deletions, then apply that exact plan:

```bash
undo apply rec_812
```

Recoveries expire after 24 hours. If any affected path changed after the
preview, apply refuses the entire plan and marks it `conflicted`. Applying an
already successful Recovery again is a safe no-op.

### How Undo chooses the work

An **intent** is a marked task inside a Run. If an integration marked intents
and exactly one completed intent matches your request, Undo plans to reverse
that task while preserving clean, non-overlapping later edits. Overlapping later
edits block apply.

If no intent uniquely matches, Undo deterministically matches request terms
against change-group IDs, labels, and paths. It then restores each selected path
as a whole file to its state at the start of the Run. This fallback cannot
separate wanted and unwanted edits in the same selected file. `undo ask` does
not use LLM semantic inference.

For stale-plan protection, each Recovery records whether every affected path
exists and its SHA-256 hash. Apply checks those expected states and hashes before
writing any file.

## Recover without a Run

Continuous local history remains available for unwrapped agent work and ordinary
human or tool changes:

```bash
undo timeline --since 30m --bursts
undo preview src/server.rs 10m
undo restore src/server.rs 10m
undo deleted
undo panic
```

See [Recovery](recovery.md) for the selection and apply guarantees, and
[CLI reference](cli.md) for all commands.

## Build from source

Undo requires Rust 1.85 or newer and uses the Rust 2024 edition.

```bash
git clone https://github.com/treadiehq/undo.git
cd undo
cargo build --release
cp target/release/undo /usr/local/bin/
```
