# Portico v1 Host Management Design

**Status:** Approved design, awaiting written-spec review
**Date:** 2026-09-13

## Purpose

Portico v1 makes adding and maintaining SSH hosts safe and fast without replacing OpenSSH or taking ownership of a user's existing SSH configuration. It provides both a keyboard-first TUI and a scriptable CLI over the same host-management core.

The v1 product succeeds when a user can initialize Portico, add a host, inspect its effective OpenSSH configuration, edit or remove Portico-managed hosts, and connect using either Portico or ordinary `ssh <alias>`.

## Scope

### Included

- Initialize an isolated Portico-owned OpenSSH config fragment.
- Discover concrete aliases from existing user SSH config as read-only hosts.
- Add, inspect, edit, rename, and remove Portico-managed hosts.
- Show both ownership and the effective configuration reported by `ssh -G`.
- Connect in the current terminal or a new terminal window.
- Expose all host operations through both CLI and TUI interfaces.

### Explicitly deferred

- Adopting or rewriting hosts from user-owned configuration.
- Discovering, labeling, generating, or assigning agent/provider identities.
- Arbitrary OpenSSH directives on Portico-managed hosts.
- Port forwarding, tunnel lifecycle management, and port-conflict detection.
- Reachability polling, connection history, and session management.
- Quickshell and Omarchy desktop components.
- herdr integration and machine-readable CLI output.
- A daemon, database, plugin system, or provider abstraction.

These are separate follow-up designs, not extension points that v1 must pre-build.

## Product invariants

1. OpenSSH remains authoritative for connection behavior and effective configuration.
2. Normal `ssh <alias>` usage works for every Portico-managed host.
3. Routine CRUD writes only the Portico-owned fragment.
4. Existing SSH configuration is visible and connectable but never editable in v1.
5. Portico never reads, copies, or stores private-key contents.
6. Failed or concurrent mutations cannot corrupt the active managed file or lose a completed update.
7. Portico passes arguments directly to child processes and never constructs shell commands from host data.

## Architecture

Portico is one Rust binary. Running `portico` without a subcommand opens the TUI; running a subcommand performs one CLI operation. Both interfaces call the same in-process application core. There is no background service.

The core has five concrete responsibilities:

1. **Managed-host model:** validates the six supported fields and ownership rules.
2. **Managed-config store:** parses and deterministically serializes Portico's fragment, locks mutations, and performs atomic replacement.
3. **External discovery:** walks the user's SSH config includes to enumerate concrete aliases and their source files without modifying them.
4. **OpenSSH adapter:** invokes `ssh -G` for authoritative effective values and invokes `ssh` for connections.
5. **Application operations:** implements initialize, list, show, add, edit, remove, and connect once for both interfaces.

These are ordinary Rust modules, not traits or provider interfaces. The initial implementation should use `clap` for CLI parsing and `ratatui` with `crossterm` for the TUI. It should not add an async runtime.

## Files and ownership

```text
~/.ssh/config                    user-owned OpenSSH entry point
~/.ssh/config.portico-backup-*   exact pre-init backup
~/.ssh/portico/                  mode 0700 when Portico creates it
~/.ssh/portico/config            Portico-owned fragment, mode 0600
~/.ssh/portico/config.bak        previous valid managed version, mode 0600
~/.ssh/portico/config.lock       mutation lock
```

The managed fragment starts with a clear machine-owned warning. Manual advanced rules belong in other user-owned config files.

If `~/.ssh/config` does not exist, initialization creates it with mode `0600`. If it is a symbolic link, v1 reads it for discovery but refuses to rewrite it automatically and prints the exact `Include` directive for manual installation; this avoids breaking dotfile-managed links. After the user adds the directive, rerunning initialization detects it and creates any missing Portico-owned files without touching the link.

## Initialization

`portico init` previews its change and requires confirmation. Non-interactive callers must pass `--yes/-y`.

The active directive is:

```sshconfig
Include ~/.ssh/portico/config
```

It is placed before other active directives because OpenSSH generally uses the first obtained value. Portico's explicit per-host values therefore win, while omitted values may inherit from later wildcard rules.

Initialization:

1. Inspects the entry config and detects an equivalent existing Include.
2. Shows whether it will insert or reposition that directive.
3. Creates an exact, non-overwriting timestamped backup before changing an existing entry config.
4. Writes the updated entry config atomically while preserving its permissions.
5. Creates the Portico directory and managed fragment with restrictive permissions.
6. Validates the complete configuration with OpenSSH.

Running initialization again with a correctly positioned Include makes no changes. Any failure restores or retains the prior entry config.

## Host model

```text
alias          required
hostname       required
user           optional
port           optional
identity_file  optional
proxy_jump     optional
```

Omitted fields remain absent from the generated block; Portico does not write defaults that could suppress ordinary OpenSSH inheritance.

Validation rules:

- Alias matches `[A-Za-z0-9][A-Za-z0-9._-]*` and is unique case-insensitively across managed and discovered concrete aliases.
- Hostname and user are non-empty single values without whitespace or control characters.
- Port is an integer from 1 through 65535.
- Identity file and ProxyJump reject line breaks and control characters and must serialize as one OpenSSH argument.
- Identity files are stored only as path references. Portico never opens the referenced private file.
- ProxyJump is passed to OpenSSH for semantic validation rather than reimplementing OpenSSH's complete destination grammar.

The deterministic representation is one `Host <alias>` block per managed host, ordered case-insensitively by alias, containing only supported directives.

## Existing-host discovery

Discovery starts at `~/.ssh/config`, follows readable `Include` directives recursively, expands their file globs, and prevents cycles by tracking canonical paths. When traversal reaches the canonical Portico fragment path, it skips that file because the managed-config store already owns it. From all other files, discovery extracts positive, concrete tokens from `Host` directives. Tokens containing wildcard syntax (`*`, `?`, or `!`) and negated tokens are rules, not host entries.

Discovery records each concrete alias and source path. It does not attempt to evaluate the full OpenSSH language. For every displayed alias, `ssh -G <alias>` supplies authoritative effective values, including contributions from wildcard and `Match` rules.

Unreadable includes, malformed discovery syntax, and OpenSSH resolution failures appear as warnings. They do not give Portico permission to repair user-owned files. A new or renamed managed alias that collides with any discovered concrete alias is rejected.

## Mutation transaction

Every add, edit, rename, or removal follows one transaction:

1. Acquire an advisory exclusive lock.
2. Re-read and strictly parse the current managed fragment.
3. Re-run external alias discovery and validate the requested operation.
4. Apply the operation in memory and deterministically serialize the complete fragment.
5. Write a uniquely named temporary file in `~/.ssh/portico/` with mode `0600`, flush it, and sync it to disk.
6. Create a temporary entry config beside `~/.ssh/config` that preserves the user's config but points the Portico Include at the candidate, then validate it with `ssh -G -F`. The candidate is not active during validation.
7. Replace `config.bak` with the prior valid version and atomically rename the candidate to `config`.
8. Sync the containing directory before releasing the lock.

Validation or I/O failure before replacement leaves the active file byte-for-byte unchanged. If the managed file is invalid when read, mutation stops and the error identifies `config.bak` as the one-step recovery source.

## CLI contract

```text
portico init [--yes/-y]
portico hosts
portico show <alias>
portico add <alias> --hostname/-H <host> [options]
portico edit <alias> [changed options] [--alias/-a <new>] [--unset/-U <field>]
portico remove <alias> [--force/-f]
portico connect <alias> [--new-terminal/-n]
```

Reusable host options are:

```text
--hostname/-H
--user/-u
--port/-p
--identity/-i
--jump/-J
```

`edit` changes only supplied fields. `--unset/-U` is repeatable and accepts `user`, `port`, `identity`, or `jump`; alias and hostname cannot be unset. Removal prompts on an interactive terminal and otherwise fails unless `--force/-f` is supplied.

Commands print concise human-readable output and actionable errors. Machine-readable output is deferred until a concrete Quickshell or herdr consumer defines its requirements.

## TUI contract

The TUI is a single master-detail screen:

- The left pane lists and filters concrete aliases and marks each `managed` or `external`.
- The right pane shows ownership, source, Portico-declared values where applicable, and resolved values from `ssh -G`.
- Add and edit forms replace the right pane rather than opening a new navigation hierarchy.
- External hosts expose inspect and connect actions only.
- Destructive removal always requires an in-TUI confirmation.

The primary keyboard actions are movement, `/` to filter, `a` to add, `e` to edit, `x` to remove, Enter to connect in place, `n` to connect in a new terminal, and `q` to quit. The footer always shows context-valid actions. Mouse selection is optional convenience; Ctrl-click is not a v1 contract because terminal emulators handle it inconsistently.

If Portico is not initialized, the TUI presents the same initialization preview and confirmation as the CLI before showing host management.

## Connection behavior

For an in-place connection, Portico starts the system `ssh` executable with the selected alias as a direct argument and inherits standard input, output, and error.

The CLI exits with the SSH process's result. The TUI leaves raw/alternate-screen mode before starting SSH, waits for it to exit, restores the terminal even on failure, and then shows the result without losing the current selection.

`--new-terminal/-n` and the TUI's `n` action invoke `xdg-terminal-exec` with `ssh` and the alias as distinct arguments. If `xdg-terminal-exec` is unavailable, Portico returns an actionable error rather than guessing an emulator-specific command.

## Errors and diagnostics

- Validation errors name the field and rejected constraint without exposing credential contents.
- OpenSSH errors preserve useful stderr and identify the affected alias or config path.
- Permission failures identify the exact path Portico could not access.
- External discovery warnings remain visible in `hosts`, `show`, and the TUI but do not prevent safe operations unless alias uniqueness cannot be established.
- A signal, panic, SSH failure, or terminal-launch failure must still restore terminal state before the TUI exits.

## Verification

Unit tests cover host validation, supported-fragment parsing, deterministic serialization, Include/Host discovery, cycle handling, and edit/unset semantics.

Integration tests use temporary home/config directories and controlled fake `ssh` and `xdg-terminal-exec` executables to prove:

- Initialization preserves content, permissions, and an exact backup and is idempotent.
- CLI add appears in TUI/application queries and resolves through `ssh -G`.
- Editing one field changes only that field and host.
- Rename and add reject managed or external alias collisions.
- External hosts cannot be edited or removed.
- Removal requires confirmation or `--force/-f`.
- Concurrent mutations do not lose updates or corrupt the file.
- A failed candidate validation leaves the active bytes unchanged.
- Process arguments are passed without a shell.
- In-place and new-terminal connection paths propagate success and failure correctly.
- TUI terminal state is restored after child-process exit and error paths.

A focused manual smoke test uses the installed OpenSSH tools to initialize an isolated temporary config, add a host, inspect `ssh -G`, and exercise the TUI. Tests do not require network access.

## Follow-up product slices

Later work retains these requirements without constraining their implementation prematurely:

1. **Safe adoption:** losslessly migrate a concrete external block only after showing an exact preview, creating a backup, and providing rollback.
2. **Identity awareness:** discover file references and agent/provider identities, including 1Password, while keeping private keys with their provider. Identity knowledge must remain separate from credential possession.
3. **Tunnels:** model local, remote, and dynamic forwards; detect local conflicts; and supervise explicitly started tunnels.
4. **Desktop surface:** add a lightweight Quickshell launcher/status view rather than duplicating management UI.
5. **Tool integration:** expose the minimum machine-readable contract required by herdr without granting credential access merely because a host is visible.

Each slice requires its own design review before implementation.
