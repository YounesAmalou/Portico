# Security policy

## Supported versions

Only the latest released 0.x version receives fixes.

## Scope

Portico reads and edits SSH configuration. It rewrites the first line of `~/.ssh/config` (the `Include` of the Portico-owned file) and manages `~/.ssh/portico/`. Reports about data loss, unexpected modification of `~/.ssh/config`, unsafe file permissions, command injection through host fields, or credential exposure are in scope. Portico never reads private keys or passwords.

## Reporting a vulnerability

Please report vulnerabilities privately using GitHub's private vulnerability reporting ("Report a vulnerability" under the repository's Security tab). Do not open a public issue for a security problem. You can expect an acknowledgement within a few days and a fix or mitigation plan once the issue is confirmed.
