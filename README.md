# Undo

## Give your files or agents an undo button

Undo records changes across your project, so you can see what happened and
restore earlier versions when something goes wrong. It works with coding agents,
scripts, tools, and your own edits.

## Install

```bash
curl -fsSL https://useundo.co/install.sh | bash
```

Available for macOS and Linux.

## Undo an agent's work

```bash
undo run claude
undo runs
undo run show r_421
undo ask r_421 "remove the auth migration work"
undo apply rec_812
```

`undo run` launches the agent and records its work. `undo ask` creates a preview
and changes nothing. `undo apply` applies the exact preview you reviewed.

Undo also works with `codex`, `opencode`, or any command:

```bash
undo run exec --agent "My Agent" -- my-agent --non-interactive
```

## Recover other changes

Undo can also restore recorded files by time, recover deleted files, and help
after a large unexpected change:

```bash
undo restore src/server.rs 10m
undo restore-deleted src/old-api.rs
undo panic
```

Everything stays under `~/.undo/`. There is no account or cloud service.

## Learn more

- [Getting started](docs/getting-started.md)
- [Documentation](docs/README.md)
- [Recovery behavior and safety](docs/recovery.md)
- [CLI reference](docs/cli.md)
- [Contributing](CONTRIBUTING.md)
- [Security](SECURITY.md)

## License

[FSL-1.1-MIT](LICENSE)
