# Security policy

## Supported versions

Only the latest released 0.x version receives fixes.

## Scope

Portico reads and edits SSH configuration. It inserts an `Include` line as the first line of `~/.ssh/config` (pointing at the Portico-owned file) and keeps the rest of that file unchanged, and it manages `~/.ssh/portico/`. The only other time it writes to your own SSH configuration files is `portico adopt` and `portico export` (and the TUI `m` key), which move host blocks between them after an exact preview and a confirmation (skipped with `--yes`), keep a temporary backup, and restore it on failure. Trusting a host key in the connection test (the TUI `y` key or `portico test --accept-host-key`) appends that key to your `known_hosts` file. Container shells run `docker` on this machine or, over `ssh`, on a host. When Docker refuses access, Portico can offer to run a fix through `sudo` (starting the Docker daemon, or adding the user to the `docker` group, which grants root-equivalent access on that machine): the full command is shown first, it runs only after you confirm, and `sudo` asks for its own password, which Portico never sees. Reports about data loss, unexpected modification of `~/.ssh/config`, unsafe file permissions, command injection through host fields, or credential exposure are in scope. Portico never reads private keys or passwords.

## Reporting a vulnerability

Please report vulnerabilities privately using GitHub's private vulnerability reporting ("Report a vulnerability" under the repository's Security tab). Do not open a public issue for a security problem. You can expect an acknowledgement within a few days and a fix or mitigation plan once the issue is confirmed.
