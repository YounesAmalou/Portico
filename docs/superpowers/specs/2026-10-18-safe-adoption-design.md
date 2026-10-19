# Safe Adoption Design

**Status:** Approved; implemented
**Date:** 2026-10-18

## Purpose

Portico shows the hosts of the user's own OpenSSH configuration but never edits them. This slice lets the user hand such hosts over to Portico: adopting a concrete external `Host` block moves it, comments included, into the managed fragment and removes it from the file it came from, so that `ssh <alias>` behaves exactly as before and the host can then be edited in Portico. Exporting is the way back: it moves managed hosts out to `~/.ssh/config`.

It implements the "Safe adoption" slice of `ROADMAP.md` (v2): "losslessly migrate a concrete external block only after showing an exact preview, creating a backup, and providing rollback". It is the first time Portico writes to a file it does not own, so the rules below are conservative: anything Portico cannot move without changing behaviour is refused with a reason, not approximated.

## Decisions

### What is adoptable

An external host is adoptable when all of these hold:

- **One concrete alias.** Its `Host` line has exactly one token, without wildcard (`*`, `?`) or negation (`!`) syntax.
- **Declared once.** No other discovered file, and no other block of the same file, declares the alias (case-insensitively), and it is not already a managed alias.
- **Supported directives only.** Every directive in the block is one of `HostName`, `User`, `Port`, `IdentityFile` and `ProxyJump` (any letter case, `Key value` or `Key=value`), each at most once, and each value passes the managed host validation (`src/host.rs`). `ProxyJump` keeps its raw remainder, as the managed parser already does.
- **An unconditional, writable source.** The file is a regular file (a symbolic link is followed to its target, as `portico init` does for the entry config), is UTF-8, is owned by the current user, and both it and its directory are writable. It was not reached through an `Include` that sits inside a `Host` or `Match` block, because such an inclusion is conditional. An `Include` inside `Host *` or `Match all` applies to every host, so discovery treats it as unconditional; the effective configuration check remains the backstop.
- **Lossless.** The effective configuration check below passes.

Every other discovered external host is listed as ineligible with exactly one reason, the first rule it breaks, in the order above, for example `Host line declares 2 aliases`, `ForwardAgent is not supported by Portico`, `declared in 2 files` or `effective configuration would change (-user root, +user alice)`.

A missing `HostName` is written as `HostName <alias>`, which is OpenSSH's own default, so it does not change the effective configuration.

### What moves

Within the source file, the moved lines are:

1. **Leading comments:** the contiguous comment lines directly above the `Host` line, with no blank line between them and it.
2. **The block:** the `Host` line and every line up to and including the block's last directive. A block ends at the next `Host` or `Match` line or at the end of the file. Comment lines after the last directive are not part of the block: they stay in place or are the next block's leading comments.
3. **One separator:** a single blank line directly after the block's last directive, when there is one, so repeated adoptions do not pile up blank lines.

Everything else in the file keeps its exact bytes, including line endings. Blank lines inside the block are dropped (they are whitespace, not configuration).

### Comments in the managed fragment

Managed hosts gain comments, so that adoption is lossless and a future description feature can read the comment above a host:

- **Leading comments** are rendered directly above the host's `Host` line.
- **Inner comments** (comment lines between the `Host` line and the last directive) are attached to the directive that follows them and rendered just above it. Because the managed renderer writes directives in a fixed order (`HostName`, `User`, `Port`, `IdentityFile`, `ProxyJump`), a comment travels with its directive.
- **Trailing comments** on a directive line (`User bob  # old account`) become a comment line of their own just above that directive. The text is kept; only its position on the line changes.

The managed parser attaches comments the same way when it reads the fragment back, so they survive every later save. Editing a host keeps its comments; clearing a field also drops the comments attached to it, because they described it; renaming keeps them. Comment lines separated from the next `Host` line by blank lines still attach to it (the blank lines are not kept), and comments after the last host's last directive are kept below it, so a save never drops a comment. The header on the first line never attaches to a host.

Because every comment of an adopted block moves with it, adoption never drops a comment, and no "comments will be dropped" confirmation is needed. Should a later change make some comment unrepresentable, the preview must list the affected hosts and require an explicit confirmation; that rule is recorded here so it is not lost.

### Losslessness check

For every selected alias, the full output of `ssh -G <alias>` must be byte-identical before and after the change. Both runs read a staged entry config that ends with `Match all` and an `Include` of the system configuration when it exists, exactly as the connection test's preview does, because `ssh -F` otherwise skips it. If any alias differs, nothing is written, and the error names the alias and the differing `ssh -G` lines (before and after).

This catches the cases a text comparison cannot: the managed fragment is included on the first line, so a block that used to sit after a `Host *` rule setting the same option would win where it used to lose.

### Exact preview

A plan describes every change before anything is written: for the managed fragment, the lines it gains (or loses, for export); for each other file, the lines it loses (or gains), with their line numbers, as `-` and `+` lines grouped by file. The plan also carries a snapshot (the exact bytes) of every file it read. Applying a plan first re-reads those files under the lock and refuses with "the configuration changed since the preview; run the command again to see a fresh preview" when any of them differs, so what is applied is exactly what was shown.

### The transaction

Adoption of the selected hosts is one all-or-nothing transaction:

1. **Lock.** Take Portico's advisory lock, require a ready setup (as every mutation does), and re-run discovery.
2. **Re-check.** Re-read every file in the plan's snapshot and compare it byte for byte.
3. **Stage.** Write, without activating anything, the candidate managed fragment (hosts added) and one candidate per source file (blocks removed), each a uniquely named temporary file beside its original with the original's mode. A permission or directory problem therefore appears before any change.
4. **Validate before activation.** Build a validation entry whose Portico `Include` points at the candidate managed fragment, while the source files stay as they are. For each adopted alias this is its real final state: the managed block comes first and wins, and the leftover external copy carries identical values; removing a single-alias `Host` block affects only that alias. Run `ssh -G` for each alias against the current configuration and against the validation entry and require identical output.
5. **Back up.** Copy the managed fragment and each source file to a uniquely named `.<name>.portico-adopt-backup-<timestamp>` beside the original, with the original's mode, and sync.
6. **Activate, in an order that is safe at every instant.** First rename the candidate managed fragment into place (the previous version also becomes `config.bak`, as today): the alias is now declared twice, and the managed copy wins with identical values. Then, for each source file, re-check its bytes once more and rename its candidate into place. Sync each directory.
7. **Verify after activation.** Run `ssh -G` for each alias against the installed configuration and compare with the output from step 4.
8. **Finish.** On success, delete the backups. On a failure in steps 6 or 7, restore every replaced file from its backup and then delete the backups. If restoring fails, keep the backups and report that the state is uncertain, naming each backup and the file it restores, as `portico init`'s rollback error does.

Backups exist only to protect the operation: after success nothing is kept apart from the usual `config.bak` of the managed fragment. Undoing an adoption later is an export.

Concurrency follows the existing rules. Portico's own writers are serialized by the lock. Editors do not share it; the snapshot comparisons in steps 2 and 6 detect an editor's change made since the preview, and the remaining window between the last comparison and the rename is the one `portico init` already accepts.

### Export

Exporting moves managed hosts, comments included, out to the user's entry config `~/.ssh/config` (the symlink target, when it is one). The blocks are inserted directly below the Portico `Include` line, in alphabetical order, followed by one blank line: that is the only place where they keep the precedence they had as managed hosts. Every managed host is exportable; the losslessness check still applies.

The transaction mirrors adoption: lock, re-check, stage the candidate entry (blocks inserted) and candidate managed fragment (hosts removed), validate the candidate entry with its `Include` pointing at the candidate fragment (both files are known, so this is the complete final state) and compare `ssh -G`, back up, activate the entry first (the managed copy still wins while both exist) and the managed fragment second, verify, then delete the backups or roll back.

### Surfaces

**CLI**

```text
portico adopt [<alias>...] [--all/-A] [--yes/-y]
portico export [<alias>...] [--all/-A] [--yes/-y]
```

- Without aliases or `--all`, on an interactive terminal, the command lists the eligible hosts numbered (alias and source file, or `managed` for export), then the ineligible ones with their reasons, and asks `Adopt which hosts? (all, numbers like 1,3-4, or none)`. An empty answer or `none` changes nothing.
- It then prints the exact preview and asks `Adopt 3 hosts? [y/N]`; `--yes` skips this question.
- Without an interactive terminal, aliases or `--all` and `--yes` are required, and the command fails otherwise, as `remove` requires `--force`.
- A named alias that is ineligible (or unknown) fails the whole command with its reason before anything is written. `--all` takes every eligible host and lists the skipped ones with their reasons. With nothing eligible, the command says so and exits successfully without changes.
- Success prints `Adopted: a, b, c` (or `Exported: ...`).

**TUI**

- `m` (migrate) opens the migrate screen in the right pane: on an external host the adoption checklist of every external host, on a managed host the export checklist of every managed host, with the selected host preselected when it is eligible.
- In the checklist, Up/Down moves, Space toggles, `a` selects every eligible host, and ineligible hosts are shown dimmed with their reason and cannot be selected. Enter shows the exact preview, which scrolls with Up/Down and PageUp/PageDown. `y` applies, Esc goes back one step.
- The result goes through the existing mutation path: the status line reports success or the error, and the inventory is refreshed.
- The footer advertises `m` on the host list.

### Code layout

- `src/adopt.rs` (new): block scanning and eligibility, the plan and its preview, and the adoption and export transactions. Pure functions for scanning, planning and rendering the preview; I/O only in the transaction.
- `src/managed.rs`: comment support in the parser and renderer, and small `pub(crate)` exposure of existing helpers (`write_unique`, `sync_dir`, the lock, `substitute_include`).
- `src/discovery.rs`: discovery records, per external host, the line of its `Host` directive and whether its file was reached through a conditional `Include`.
- `src/openssh.rs`: an `effective` call that returns the raw `ssh -G -F <entry>` output.
- `src/app.rs`: `adoption_plan`, `adopt`, `export_plan` and `export`.
- `src/cli.rs`: the two subcommands. `src/tui/migrate.rs` (new): the migrate screen; `src/tui.rs` only gains the mode, the key and the footer hint.
- `README.md`: the two commands and the `m` key.

## Errors

- Ineligibility reasons name the rule and, where relevant, the directive, the line and the file.
- OpenSSH validation failures reuse the existing rejection interpretation and report that nothing was changed.
- Filesystem errors name the exact path.
- A failed rollback reports uncertain state and every backup path with the file it restores.

## Verification

Unit tests:

- Block scanning: leading comments (with and without a separating blank line), inner and trailing comments, blank lines inside a block, the single trailing separator, blocks ending at `Match` and at end of file, CRLF line endings, `Key=value` forms and letter case.
- Eligibility: one test per refusal reason, including multiple aliases, wildcards, negation, duplicate declarations, unsupported and repeated directives, invalid values, non-UTF-8 files, files owned by another user or read-only, and conditional `Include`.
- Managed comments: parse and render round trips, comments surviving edit and rename, comments of a cleared field removed, the header never attached.
- Preview rendering for adoption and export.

Integration tests with temporary homes and the controlled fake `ssh`:

- Adopting from the entry config and from an included file moves exactly the shown lines, keeps every other byte, and resolves identically.
- Adopting a block placed after a `Host *` rule that sets one of its options is refused by the losslessness check, and nothing changes.
- A configuration changed after the preview is refused, and nothing changes.
- A validation failure, and an I/O failure at each activation step (managed rename, each source rename, directory sync, post-activation check), leaves every file byte-for-byte as it was and no backup behind.
- A failed rollback keeps the backups and names them.
- Adopt then export (and export then adopt) round trips to an equivalent configuration, comments included.
- The CLI's interactive selection, non-interactive refusal without `--yes`, `--all` with skipped hosts, and named ineligible aliases.
- The TUI's migrate screen: checklist, preview, confirm and cancel.

A real-OpenSSH test (in `tests/real_openssh.rs`) adopts and exports a host against the installed `ssh` and compares `ssh -G` before and after.

## Not in this slice

- Adopting blocks with directives Portico does not model; this waits for the "More OpenSSH directives" item.
- Splitting a multi-alias `Host` line to adopt one of its aliases.
- Showing a host description from its leading comment (proposed as a separate roadmap item).
- Exporting to a file other than `~/.ssh/config`.
