# Undo documentation

Undo helps you inspect and reverse unwanted coding-agent changes without giving
up the work you want to keep. It also keeps local history for human edits,
scripts, deleted files, and work that was not wrapped as an agent Run.

## Start and recover work

- [Getting started](getting-started.md) — install Undo, record an agent, preview
  a reversal, and apply the plan you reviewed.
- [Recovery](recovery.md) — undo selected Run work or recover by time, path,
  checkpoint, deletion, or recent burst.
- [Demos](demos.md) — reproducible selective and emergency recovery exercises.

## Use Undo with an agent

- [Agent Integration Spec](agent-integration-spec.md) — mark Runs, intents, and
  checkpoints through the version 1 `undo event` JSON protocol.
- [Claude Code](agents/claude-code.md)
- [OpenCode](agents/opencode.md)
- [Codex](agents/codex.md)

## Understand and automate

- [Concepts](concepts.md) — learn what Runs, intents, checkpoints, and
  Recoveries mean and where attribution stops.
- [CLI reference](cli.md) — look up every implemented command and flag.
- [Machine output](machine-output.md) — consume JSON-producing commands and
  their output contracts.

## Operate and troubleshoot

- [Storage and operations](storage.md) — manage local data, retention, ignores,
  supported platforms, and common problems.

[`detailed.md`](detailed.md) remains as a compatibility and operations reference
for links written before this documentation was reorganized.
