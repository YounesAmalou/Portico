# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org/). Before 1.0, minor versions
may contain breaking changes.

## [Unreleased]

## [0.3.0] - 2026-10-18

### Added

- Adopt and export hosts from the TUI
- Add portico adopt and portico export
- Adopt and export hosts in one rollback-safe transaction
- Plan adoption and export with eligibility reasons
- Render exact previews of configuration changes
- Scan external host blocks for adoption
- Read the raw effective configuration of a host
- Record where external hosts are declared
- Keep comments of managed hosts

### Fixed

- Scroll the migrate checklist to the cursor
- Report a cancelled adoption as cancelled with nothing changed
- Keep adoption backups when a panic unwinds
- Explain a changed effective configuration that differs only in order
- Name loop paths, key snapshots once and guard the rollback message
- Drop comments of cleared managed host fields on replace

## [0.2.6] - 2026-10-18

### Fixed

- Fit jump host suggestions to the pane and ignore accents when matching

## [0.2.5] - 2026-10-18

### Added

- Keep a new terminal open to explain a failed connection
- Show failure explanations and ssh output in the TUI
- Explain why a connection failed, keeping ssh's own output
- Explain connection test failures with their facts and next step
- Say which host, field or file OpenSSH rejected when saving
- Map OpenSSH configuration errors to the host and field

### Changed

- Share how a rejected configuration is interpreted
- Read offered keys and causes from debug output

### Fixed

- Tighten error wording and fold ssh output on new results
- Exit 128 plus the signal when a signal ends ssh on connect
- Tell an add form to correct the field OpenSSH rejects
- Point at the known_hosts line for a changed key on an aliased jump
- State no timeout limit when a connection times out
- Report a bad line in the user's config as theirs when saving
- Keep ssh's exit code when the hold prompt cannot read
- Report errors in the user's own entry file as theirs
- Show raw output and exact next steps for every explained failure
- Recognise only session drops as a lost connection

## [0.2.4] - 2026-10-18

### Added

- Trust a new host key from the TUI
- Test hosts from the TUI
- Add portico test

### Fixed

- Keep TUI test prompts and results where the test started
- Stop ssh when portico test is interrupted
- Never offer a host key without a fingerprint
- Hold the test deadline while ssh keeps writing
- Recognise a login through a jump host or proxy

## [0.2.3] - 2026-10-14

The `v0.2.0`, `v0.2.1` and `v0.2.2` tags were pushed on commits whose `Cargo.toml` version did not match, so their releases failed and published nothing; tags are immutable, so this release supersedes them.

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
