# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org/). Before 1.0, minor versions
may contain breaking changes.

## [Unreleased]

### Added

- TUI refresh (`r` or `F5`) re-reads the host inventory and discards cached host details, so edits made outside Portico, including changes to included configs, appear without restarting. The selection and filter are kept, and a failed read leaves the list untouched and reports the error.

## [0.1.0] - 2026-10-13

First public release.

### Added

- `portico` CLI and keyboard-first TUI for listing, showing, adding, editing, removing and connecting to OpenSSH hosts.
- Portico-owned managed configuration in `~/.ssh/portico/config`, included from `~/.ssh/config`, with read-only discovery of hosts defined elsewhere.
- Automatic initialization (`portico init`) with backups, atomic replacement and rollback; ordinary commands refuse to run until setup is complete.
- Connection in place or in a new terminal (`Enter`, `n`, and `Ctrl+Enter` on terminals that support the kitty keyboard protocol).
- Field validation shared by the CLI and TUI forms: blank, padded or whitespace-containing values cannot be typed or saved where they are not valid, including ProxyJump.
- Responsive TUI details pane resolved through `ssh -G` without blocking navigation.

### Security

- Every change to the managed file is validated by OpenSSH before activation and rolled back on failure.
