# Security Policy

## Reporting a vulnerability

Please **do not open a public issue** for security vulnerabilities.

Report privately through GitHub's
[**Report a vulnerability**](https://github.com/treadiehq/undo/security/advisories/new)
flow (Security → Advisories), which opens a private channel with the maintainers.

Please include enough detail to reproduce: the affected version (`undo --version`),
your OS, and a minimal set of steps. We'll acknowledge the report, investigate, and
coordinate a fix and disclosure timeline with you.

## Supported versions

undo is pre-1.0 and ships fixes on the latest release. Please reproduce against the
most recent version before reporting.

## Security model

undo is a **local, single-user** tool. It runs as your normal user and stores
everything under `~/.undo/`. It opens the network only for `undo update`
(release downloads from GitHub). The optional `undo ui` command starts a web
interface on a loopback-only listener; nothing is ever served beyond
127.0.0.1, and it runs only while you keep the command running. Understanding
what it does and does not protect helps set expectations:

### What undo does to protect you

- **Owner-only storage.** `~/.undo/` is created `0700`, and the database (plus its
  WAL/SHM sidecars), snapshots, backups, PID files, and log are `0600`. Snapshot
  contents are not readable by other users on the machine.
- **Secrets are ignored by default.** `.env` and any `.env.*` file, `.ssh/`, and
  common key/cert extensions (`*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.keystore`)
  are excluded from snapshotting so credentials don't end up in the store. You can
  re-include a path with a `!` negation in `.undoignore` if you explicitly want it
  tracked.
- **Path-traversal and symlink guards.** `diff` and `restore` resolve user-supplied
  paths and reject anything that escapes the project root, including via `..` or a
  parent-directory symlink. `restore` additionally refuses to write through a
  symlink, and writes atomically (temp file + rename) after saving a safety backup.
- **Refuses dangerous targets.** The daemon refuses to run as root, refuses to
  watch root-/system-owned directories, and refuses oversized trees — guards
  against accidentally snapshotting system files or a whole home directory.
  (`--force` overrides them, deliberately.)
- **Verified self-update.** `undo update` downloads the release artifact, verifies
  it against the release's published `SHA256SUMS`, and aborts if the checksum file
  is missing or the hash doesn't match. The binary is then replaced atomically.
- **Loopback-only, token-gated web UI.** `undo ui` binds 127.0.0.1 exclusively
  and generates a random 256-bit token per session, revealed only in the URL
  printed to your terminal. Every API request must present the token, so web
  pages you visit cannot read your history or trigger restores with drive-by
  requests. Non-loopback `Host` headers are rejected to block DNS-rebinding,
  and no CORS headers are emitted, keeping cross-origin responses opaque.
- **Decompression bounds.** Snapshots are size-capped on load to avoid
  decompression-bomb blowups.

### What is out of scope

- **Snapshots are not encrypted at rest.** They are plaintext-equivalent (gzip)
  and protected only by filesystem permissions. Anyone who can read your account
  (you, root, or a backup of your home directory) can read snapshot contents. If
  you work with secrets in tracked files, keep them in ignored paths.
- **No protection against a compromised account.** A process running as your user
  can already read `~/.undo/`. undo does not defend against local privilege
  escalation or malware running as you.
- **Multi-user / shared-directory threat models** are not a goal — undo is
  single-user by design.
- **Denial of disk space** from an attacker who can create huge numbers of files
  in a watched tree is mitigated by retention and the size cap, but not eliminated.
