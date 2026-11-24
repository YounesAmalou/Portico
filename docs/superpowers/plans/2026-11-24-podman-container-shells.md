# Podman Container Shells Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** List the running containers of a Podman host and open a shell in one, as a third adapter on the backend interface, and list every backend of a target (one TUI tab each, one CLI section each) instead of only the first installed one.

**Architecture:** A new adapter `src/backend/podman.rs` implements `Backend`. `AUTOMATIC` becomes `[Docker, Podman, Proxmox]` and keeps choosing the JSON's single backend. The CLI's human listing and `shell` walk `AUTOMATIC` themselves, each backend with `Some(backend)`. The TUI's `Tab::Containers` becomes `Tab::Containers(BackendId)`, with one panel and one listing worker per backend and a shared target.

**Tech Stack:** Rust 2024 edition, `clap`, `ratatui`, `serde_json`; tests with fake programs in temporary directories.

**Spec:** `docs/superpowers/specs/2026-11-24-podman-container-shells-design.md`

## Global Constraints

- Docker's and Proxmox's JSON documents do not change; their golden files stay byte-identical.
- A target with one installed backend prints exactly today's human listing.
- No `Offer` for Podman; Portico never writes on the target.
- Every task ends green on `scripts/ci-checks.sh fmt`, `clippy` and `test`, run as a regular user; the last one on all checks.
- Stay out of `src/platform` (another slice works there).
- Conventional Commits, no AI attribution.

## Task 1: The Podman adapter

**Files:** Create `src/backend/podman.rs`; modify `src/backend/mod.rs`

- [ ] Add `BackendId::Podman` (`"podman"`, program `podman`, `chooses_user` true, heading `Other`) and its dispatch arms; `AUTOMATIC: [BackendId; 3] = [Docker, Podman, Proxmox]`.
- [ ] Write `LIST_SCRIPT`, `parse_listing` (serde_json, infra containers left out, `skipped`), the user marker, `classify`, `exec_words(id, user)`, and the issues of the spec's table.
- [ ] Unit tests for parsing, the note, each failure row and the shell words.

## Task 2: The app

**Files:** `src/app/containers.rs`

- [ ] `ContainerPrograms::podman`, mapped in `local_program`.
- [ ] Tests with fake programs: automatic order with Podman between Docker and Proxmox, `Some(Podman)`, shell over ssh, new-terminal arguments with `--backend podman`.

## Task 3: CLI

**Files:** `src/cli/mod.rs`, `src/cli/containers.rs`, `tests/containers.rs`

- [ ] `BackendArg::Podman`.
- [ ] `list_every`: walk `AUTOMATIC` with offers per backend, skip `not-installed`, print other issues and go on, stop on ssh failure; returns the listed backends and whether any failed.
- [ ] `portico containers` (human, no `--backend`): sections when several list; exit status 1 when any failed.
- [ ] `portico shell` (no `--backend`): resolve across listings, cross-backend ambiguity, picker rows labelled by backend when several.
- [ ] Tests.

## Task 4: JSON

**Files:** `src/cli/json.rs`, `tests/json.rs`, `tests/fixtures/json/containers-podman*.json`, `docs/json-output.md`, `desktop/quickshell/tests/tst_portico.qml`

- [ ] `kind: podman`, `backend: podman`.
- [ ] Golden tests and documentation; QML test reads the new golden files.

## Task 5: TUI

**Files:** `src/tui/mod.rs`, `src/tui/containers.rs`, `src/tui/lister.rs`

- [ ] `Panel` carries its backend; the lister lists with `Some(backend)`.
- [ ] `Tab::Containers(BackendId)`, three panels and three listers, shared target, `c` to the last container tab, tab bar and footer.
- [ ] Tests.

## Task 6: Docs and roadmap

**Files:** `ROADMAP.md`, `docs/architecture.md`, `README.md`, the v1 design's delivered slices

- [ ] Remove the Podman slice from `ROADMAP.md`, renumber, fix every "slice N" reference.
- [ ] `docs/architecture.md`: `backend::podman`, and "Add a backend" step 4 (every backend listed; `AUTOMATIC` for the JSON).
- [ ] README: Podman, the five tabs, `--backend podman`.
- [ ] Run `scripts/ci-checks.sh`, commit, push, open the PR.
