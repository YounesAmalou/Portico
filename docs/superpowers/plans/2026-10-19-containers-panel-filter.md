# Containers Panel Filter and Loading Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The containers panel loads this machine at start and a host only on `c`, Tab only moves the focus, and `/` filters the panel's rows.

**Architecture:** `src/tui/containers.rs` replaces following the selection (`follow`, `follow_selection`, `on_focus`, `pinned_local`) with an explicit target (`host: Option<String>`, `None` meaning this machine) and a `show(target)` method returning the listing to start. The panel gains a filter `TextInput` and an editing flag; `rows()` and `body()` skip containers that do not match. `src/tui.rs` binds `c` in the hosts panel, drops the listing from Tab, drops the follow after each key, and lists this machine before the main loop. `suggest::fold` becomes `pub(crate)` for reuse.

**Tech Stack:** Rust 2024, ratatui and crossterm as already used. No new crates.

**Spec:** `docs/superpowers/specs/2026-10-19-containers-panel-filter-design.md`

## Global Constraints

- No new dependencies; the CLI is unchanged.
- Keep `src/tui.rs` changes to the panel's key routing, footer, title and start-up listing, so they merge cleanly with other work on that file.
- Before committing run `scripts/ci-checks.sh fmt`, `clippy`, `test` and `e2e` as a regular user.
- Conventional Commits, no AI attribution.

### Task 1: Explicit target in the panel

- [ ] Replace `pinned_local`/`follow`/`follow_selection`/`on_focus` with `host: Option<String>`, `target() -> Target` and `show(target) -> PanelAction` (lists when unlisted or failed; resets selection, scroll and filter when the target changes).
- [ ] `L` calls `show(Target::Local)`; `abandon` leaves the target unlisted; the unlisted line reads `Not listed: r lists containers`.
- [ ] Tests: `show` lists once per target and again after a failure, keeps a kept listing, resets selection on retarget; `L` switches back to this machine.

### Task 2: The filter

- [ ] Add `filter: TextInput`, `editing_filter: bool`, `filter_before_edit: String`; `/` begins editing, Enter applies, Esc restores; Esc outside editing clears an applied filter before leaving.
- [ ] `matches(container, all)` folds the filter and the shown name, name, project, service and image; `rows()` and `body()` use it, skipping headers without matches and showing `No containers match "<filter>".`.
- [ ] After each filter change keep the selected container by id, else select the first match.
- [ ] `title()` returns a `Line` with ` /` and the filter spans; `footer()` returns a `String` with the filter hints.
- [ ] Paste goes to the filter while editing it.
- [ ] Tests: matching on each field, case and accents; headers kept only with matches; selection kept or moved to the first match; scrolling of a filtered listing; Esc order; title and footer.

### Task 3: Routing in the TUI

- [ ] Tab/BackTab only set the focus; remove `follow_selection` from `UiState`.
- [ ] `c` (without Ctrl) in the hosts panel returns `UiAction::Containers(panel.show(Target::Host(alias)))` for the selected host.
- [ ] `browse_hosts` lists this machine before the loop.
- [ ] Hosts footer gains `c list containers` when a host is selected.
- [ ] Tests: Tab toggles focus without an action; moving the selection keeps the target; `c` lists the selected host and keeps the focus.

### Task 4: Docs

- [ ] README TUI keys: `c`, Tab wording, the panel's loading paragraph and `/` and `Esc` rows.
