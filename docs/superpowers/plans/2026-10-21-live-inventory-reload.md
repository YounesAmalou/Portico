# Live Inventory Reload Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** the TUI's hosts list follows external edits of the config files by itself, through the refresh key's path, never under an open form, and never for Portico's own writes.

**Architecture:** discovery records every path it depends on (`Discovery::inputs`), `App::hosts` adds the managed file (`HostInventory::inputs`), and `UiState` keeps them with the hosts. A new `src/tui/events.rs` gives the loop one source of `Step`s (input, tick, closed), reading the terminal through `hangup::next_event` in one place. A new `src/tui/watch.rs` holds `ConfigWatch`, a stat-polling state machine with injected metadata and explicit instants. The loop polls workers and the watch on ticks; the `r` key and the watch share one `reload` function; `finish_mutation` rebases the watch after its re-read.

**Spec:** `docs/superpowers/specs/2026-10-21-live-inventory-reload-design.md`.

## Deviations from this plan

Recorded after execution; the original text below is kept as written.

- The `live_reload` PTY scenario runs in the default scenario list of the existing `tui_paste_and_terminal_lifecycle_work_on_a_real_pty` acceptance test, which already provides the home, the managed hosts and the `ssh` fake, instead of a new acceptance test.
- Discovery lists a managed file reached by a literal `Include` among its inputs (it is where that Include looks), and `App::hosts` adds the managed file only when it is not there yet, so it is still watched once.
- `ConfigWatch` has no `changed` method: `tick` returns whether a settled change waits, which is all the loop needs.
- `UiState::config_changed` is recomputed on every tick from the watch and the mode, rather than set and cleared at separate places, so it can never outlive the change it announces.

## Global Constraints

- No change to the CLI, the JSON output, the managed file or the files Portico writes. No new dependency.
- `hangup.rs` is not changed; only `events.rs` calls it.
- Existing tests keep passing, except the PTY `refresh` scenario, whose "invisible until `r`" step now applies only to what the watch cannot see.
- Check as a non-root user with `scripts/ci-checks.sh` (`fmt`, `clippy`, `test`, `docs`, `e2e`, `msrv`).

## Tasks

- [x] **Task 1: Discovery inputs (TDD).** Tests in `discovery.rs`: the entry first, each file read by its opened path, a missing literal include, a glob's literal directory, each once, and the managed file never. Implement `Discovery::inputs` in the scanner (`scan_entry`, `expand_include`, `scan_include`), then `HostInventory::inputs` in `App::hosts` (managed file appended when absent) and fix the struct literals in tests.
- [x] **Task 2: `ConfigWatch` (TDD).** `src/tui/watch.rs`: `Stamp` (exists, device, inode, size, mtime and ctime in ns), `Metadata` trait with the `Filesystem` implementation, `ConfigWatch::{new, rebase, tick, changed}`, constants `CHECK_EVERY` (1 s) and `SETTLE` (300 ms). Unit tests with a fake metadata map and instants offset from one `Instant::now()`.
- [x] **Task 3: `Events` (TDD).** `src/tui/events.rs`: `Step`, `Events::{new, terminal, next}` with an injected reader `FnMut(Duration) -> io::Result<Input>` and clock. Unit tests with a scripted reader and a fake clock.
- [x] **Task 4: The loop on events.** `confirm_initialization` and `browse_hosts` take `&mut Events`; the worker polling moves into the `Step::Tick` arm; tick periods become named constants. No behaviour change yet; run the PTY suite.
- [x] **Task 5: Reload on change (TDD).** `apply_refresh` takes a `Reload` reason for its wording; a `reload` function holds the refresh key's path and rebases the watch; `finish_mutation` rebases after its read; `UiState::inputs` and `config_changed` with the status-line indicator; the tick arm applies or defers the reload. Unit tests for the wording, the indicator and its precedence.
- [x] **Task 6: PTY scenario.** `live_reload` in `tests/support/tui_pty.py`, run by a new acceptance test; adjust `refresh` so its "invisible until `r`" check covers only the `ssh -G` answer.
- [x] **Task 7: Documentation and checks.** README (hosts keys and a sentence on live reload), remove the roadmap row, spec status, then the checks above and a manual run against a temporary HOME.
