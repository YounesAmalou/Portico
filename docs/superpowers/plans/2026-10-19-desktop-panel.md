# Desktop Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Quickshell panel in `desktop/quickshell/` that lists the saved tunnels with their status and a start or stop action, and the hosts (and, when expanded, their containers) to open in a new terminal, reading only the `--json` contract. It runs as a plain Quickshell popup and as an Omarchy plugin (bar widget and summoned panel).

**Architecture:** `portico.js` (pure, `.pragma library`) checks `schema` and turns documents into rows. `PorticoRun.qml` wraps one `Process` and reports `(stdout, stderr, exitCode)` once the process and both streams have ended, or `notStarted`. `PorticoModel.qml` resolves the program, runs listings and actions (actions under `setsid --fork --wait`), and owns state. `PorticoView.qml` is the keyboard-first view over one flat list of items. `shell.qml`, `OmarchyBarWidget.qml` and `OmarchyPanel.qml` are the thin hosts.

**Tech Stack:** QML (Qt 6), Quickshell 0.3 (`Quickshell.Io.Process`, `StdioCollector`, `IpcHandler`, `PanelWindow`, `WlrLayershell`), Omarchy shell's `qs.Commons` and `qs.Ui` in the Omarchy entries only; Qt Quick Test (`qmltestrunner`) and Qt 6 `qmllint` for checks; Rust tests for argument forms and guards.

**Spec:** `docs/superpowers/specs/2026-10-19-desktop-panel-design.md`

## Global Constraints

- The panel writes nothing and runs no `show`, `identities` or up-front container listing; containers are listed only when a host is expanded.
- Every Portico command is an argument list, with `--` before a positional alias, ID or name; the only shell script is the fixed lookup, which takes its candidates as positional arguments.
- Every `Text` showing document data uses `Text.PlainText`.
- No Portico code changes; only tests are added on the Rust side.
- Before each commit run `scripts/ci-checks.sh fmt`, `clippy`, `test` and `desktop` as a regular user; run all checks in the last task.
- Conventional Commits; no AI attribution anywhere.

## Review Focus

- **Contract first:** `parse` is the only reader of raw stdout; a schema other than 1 stops all reading. Pinned by `tst_portico.qml`.
- **No overlap, no stale status:** refresh ticks skip while one runs; a refresh started before the latest finished action is dropped.
- **Actions outlive a reload:** `setsid --fork --wait`, checked manually with the fake.
- **Optional CI job:** `desktop` depends on `changes` only, is not required, and nothing depends on it.

### Task 1: Library and tests

- [ ] `desktop/quickshell/portico.js`: `SCHEMA`, `parse`, `errorInfo`, `hostRows`, `tunnelRow`, `tunnelRows`, `containerRows`, `actionResult`, `warningMessages`, `filterRows`, `lastLine`, `launchFailure`, `notFound`, `parseLookup`.
- [ ] `desktop/quickshell/tests/tst_portico.qml`: every golden file of `tests/fixtures/json/` by name, plus the synthetic cases listed in the spec.
- [ ] `scripts/ci-checks.sh desktop`: Qt 6 `qmllint` on every QML file (Omarchy entries with unresolved imports allowed) and `qmltestrunner` offscreen with `QML_XHR_ALLOW_FILE_READ=1`; a missing tool fails with the package to install.

### Task 2: Model, view and hosts

- [ ] `PorticoRun.qml`, `PorticoModel.qml`, `PorticoView.qml`.
- [ ] `shell.qml` (overlay `PanelWindow`, scrim, `IpcHandler` target `portico`), `OmarchyPanel.qml` (`open`/`close`/`requestClose`, the same window), `OmarchyBarWidget.qml` (`BarIconButton` and `KeyboardPanel`), `manifest.json` (id `portico`, kinds `bar-widget` and `panel`, `keepLoaded`).
- [ ] `desktop/quickshell/README.md` pointing at the README section.

### Task 3: Fake, guards and CI

- [ ] `desktop/quickshell/tests/fake-portico`: answers `hosts`, `tunnels`, `containers`, `tunnel start|stop`, `connect`, `shell` from the golden files; start sleeps and can leave a marker (`FAKE_PORTICO_MARKER`).
- [ ] Rust CLI tests for the argument forms (`tests/` beside the JSON tests).
- [ ] `tests/docs.rs`: `SCHEMA` equals every golden file's `schema`; every golden file is named in `tst_portico.qml`; every `entryPoints` file exists; the README names `desktop/quickshell`.
- [ ] `.github/workflows/ci.yml`: a `desktop` output of `changes` and an optional `desktop` job.

### Task 4: Verify and document

- [ ] Run the popup from the repository with the fake and the real `portico`; screenshot it; the reload check.
- [ ] README "Desktop panel" section; spec status "Approved; implemented"; remove the v3 item and section from `ROADMAP.md` and add the packaging row; the "Desktop panel" bullet in the v1 design.
- [ ] All of `scripts/ci-checks.sh`; open the pull request with the Omarchy manual check list in its description.
