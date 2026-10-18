# Security policy

## Supported versions

Only the latest released 0.x version receives fixes.

## Scope

Portico reads and edits SSH configuration. It inserts an `Include` line as the first line of `~/.ssh/config` (pointing at the Portico-owned file) and keeps the rest of that file unchanged, and it manages `~/.ssh/portico/`. The only other time it writes to your own SSH files is `portico adopt` and `portico export` (and the TUI `m` key), which move host blocks between them after an exact preview and confirmation, keep a temporary backup, and restore it on failure. Reports about data loss, unexpected modification of `~/.ssh/config`, unsafe file permissions, command injection through host fields, or credential exposure are in scope. Portico never reads private keys or passwords.

## Reporting a vulnerability

Please report vulnerabilities privately using GitHub's private vulnerability reporting ("Report a vulnerability" under the repository's Security tab). Do not open a public issue for a security problem. You can expect an acknowledgement within a few days and a fix or mitigation plan once the issue is confirmed.
