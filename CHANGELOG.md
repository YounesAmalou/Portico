# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org/). Before 1.0, minor versions
may contain breaking changes.

## [Unreleased]

## [0.6.0] - 2026-10-19

### Added

- Install the desktop panel, launcher entry and shortcut with portico desktop install ([#54](https://github.com/YounesAmalou/Portico/pull/54))
- Switch between hosts, tunnels and containers with tabs ([#53](https://github.com/YounesAmalou/Portico/pull/53))
- Control hosts and tunnels from a Quickshell panel ([#50](https://github.com/YounesAmalou/Portico/pull/50))
- Print hosts, containers and tunnels as JSON ([#49](https://github.com/YounesAmalou/Portico/pull/49))
- Save tunnels and start or stop them on demand ([#47](https://github.com/YounesAmalou/Portico/pull/47))
- Show which identities each host uses and whether they are available ([#46](https://github.com/YounesAmalou/Portico/pull/46))

### Changed

- Share one home-path helper ([#52](https://github.com/YounesAmalou/Portico/pull/52))

### Fixed

- Make Omarchy reload the panel after Portico updates it ([#57](https://github.com/YounesAmalou/Portico/pull/57))
- Find loaded containers with the panel's search ([#56](https://github.com/YounesAmalou/Portico/pull/56))
- Wait for Omarchy to register the panel before enabling it ([#55](https://github.com/YounesAmalou/Portico/pull/55))

## [0.5.1] - 2026-10-19

### Fixed

- Tighten the fuzzy pickers' edge cases ([#44](https://github.com/YounesAmalou/Portico/pull/44))

## [0.5.0] - 2026-10-19

### Added

- Pick hosts and containers with a built-in fuzzy picker ([#42](https://github.com/YounesAmalou/Portico/pull/42))

### Fixed

- Exit when the terminal closes instead of spinning ([#41](https://github.com/YounesAmalou/Portico/pull/41))

## [0.4.0] - 2026-10-19

### Added

- Filter the containers panel and load hosts on c ([#39](https://github.com/YounesAmalou/Portico/pull/39))

### Fixed

- Address the v0.3.0 review of adoption, export and containers ([#36](https://github.com/YounesAmalou/Portico/pull/36))

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
- List containers and open shells from the TUI
- Add the TUI containers panel
- Open a container shell from the CLI with a picker
- Add portico containers
- List containers and open shells on this machine or a host
- Run remote commands through the ssh connect paths
- Detect docker group access and build the fixes
- Build container shell commands and classify docker failures
- Resolve container references by name, service or id
- Parse and group running docker containers

### Fixed

- Scroll the migrate checklist to the cursor
- Report a cancelled adoption as cancelled with nothing changed
- Keep adoption backups when a panic unwinds
- Explain a changed effective configuration that differs only in order
- Name loop paths, key snapshots once and guard the rollback message
- Drop comments of cleared managed host fields on replace
- Show container fixes in full and wrap panel errors
- Tidy TUI containers panel focus and scrolling
- Let remote container commands read the terminal
- Tighten container parsing and ordering
- Leave a later jump host's offered keys out of the target's
- Keep a late signal from cancelling the retest after trusting a key
- Recognise a login through a fd-passing ProxyCommand
- Tell a later jump host's login from the target's

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
