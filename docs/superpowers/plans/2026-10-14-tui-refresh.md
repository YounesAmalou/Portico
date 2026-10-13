# TUI Refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** `r` / `F5` in the TUI browse mode re-reads the inventory and discards cached host details, so external edits appear without restarting.

**Architecture:** `UiState::handle_browse_key` returns a new `UiAction::Refresh`. The event loop applies it with `apply_refresh`, a function next to `complete_mutation` that takes the re-read closure, so it is unit-testable without an `App`. It reports whether the inventory was replaced, which feeds the existing `sync_details(..., inventory_changed)` that cancels in-flight resolution, clears the cache and re-requests the selected host. `replace_hosts` gains an alias fallback so selection survives a host whose source file changed.

**Spec:** `docs/superpowers/specs/2026-10-14-tui-refresh-design.md` (v1 requirements in `2026-09-13-portico-host-management-design.md` are unchanged).

## Deviations from this plan

Recorded after execution; the original text below is kept as written.

- The PTY scenario is named `refresh`. It appends the new host to `~/.ssh/config` and changes what the `ssh -G` fake answers (through a `resolved-hostname` file) instead of editing an included file, and it makes the managed file unreadable (`chmod 0`) instead of corrupting it. Both go through the same `App::hosts()` read.
- `tests/docs.rs` splits the v1 spec at a new `### Delivered follow-up slices` heading rather than listing a delivered title in the follow-up list.
- The minimum-Rust-version check could not run offline (the toolchain download failed); the other checks passed.

## Global Constraints

- No change to the CLI, the managed file or the `App` API.
- Existing tests keep passing unchanged, except footer expectations that now include `r refresh`.
- Check as a non-root user: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, MSRV check.

## Tasks

- [x] **Task 1: Key handling and footer (TDD).** Tests: `r` and `F5` give `UiAction::Refresh` in browse mode; `r` types into the filter and form; the footer lists `r refresh` with and without a selection. Implement `UiAction::Refresh`, the two key arms and the footer text.
- [x] **Task 2: Applying a refresh (TDD).** Tests: selection kept by identity; by alias when the source moved; first host when removed; filter kept; a failing re-read leaves hosts, selection, filter and cache untouched and sets `Refresh failed: ...`; success status says `Refreshed: N hosts` (singular for one). Implement `apply_refresh` and the alias fallback in `replace_hosts`, then wire `UiAction::Refresh` in `browse_hosts` so `inventory_changed` is true only on success.
- [x] **Task 3: PTY scenario.** `refresh_external_edit` in `tests/support/tui_pty.py`: an external included config gains a host and changes a hostname after the TUI started; neither shows until `r`; then both do. A second step corrupts the managed file and checks `Refresh failed` while the list stays. The acceptance test provides an `Include`d external file and an `ssh` fake whose `-G` answers from the config under test.
- [x] **Task 4: Documentation.** README key table and caching note, `CHANGELOG.md` Unreleased, remove the item from `ROADMAP.md`, mark the v1 spec follow-up delivered, and adjust `tests/docs.rs` to require only the pending slices in the roadmap and the delivered ones to be absent from it.
- [x] **Task 5: Checks and commits.** Run the checks above and commit in logical commits (spec and plan; implementation with tests; documentation).
