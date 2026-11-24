# Proxmox Container Shells Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** List the running LXC containers of a Proxmox VE node and open a root shell in one, through the existing containers flow (CLI, TUI, JSON), as a second adapter on the backend interface.

**Architecture:** A new adapter `src/backend/proxmox.rs` implements `Backend`. The app chooses the backend per listing from `backend::AUTOMATIC` (Docker, then Proxmox when Docker is not installed), or from an explicit `--backend`. `ContainerList` records its backend; the shell, the CLI, the TUI and the JSON read it from there. Docker's `GroupTool` becomes `Via { Sg, Sudo }`.

**Tech Stack:** Rust 2024 edition, `clap`, `ratatui`, `serde_json`; tests with fake programs in temporary directories.

**Spec:** `docs/superpowers/specs/2026-11-24-proxmox-container-shells-design.md`

## Global Constraints

- Docker's behaviour, human output, CLI arguments and JSON documents do not change. The golden files `containers.json`, `containers-on-host.json` and `containers-docker-down.json` stay byte-identical.
- No `Offer` for Proxmox; Portico never writes on the node.
- Every task ends green on `scripts/ci-checks.sh fmt`, `clippy` and `test`, run as a regular user; the last one also on `e2e` and `docs`.
- Stay out of `src/platform` (another slice works there).
- Conventional Commits, no AI attribution.

## Task 1: `Via` replaces `GroupTool`

**Files:** `src/backend/mod.rs`, `src/backend/docker.rs`, `src/app/containers.rs`, `src/cli/mod.rs`, `src/cli/json.rs`, `src/tui/containers.rs`

- [ ] Rename `backend::GroupTool` to `backend::Via` and add `Sudo` (`as_str` `"sudo"`). Rename the CLI's private clap enum `Via` to `ViaArg` with `Sg` and `Sudo`.
- [ ] Docker's `in_group` takes `Via` and is only called with `Via::Sg`; make it match `Sg` and treat `Sudo` as unreachable for Docker by wrapping in `sudo -n` (harmless, never produced by Docker).
- [ ] Run the tests: nothing else changes.

## Task 2: The Proxmox adapter

**Files:** Create `src/backend/proxmox.rs`; modify `src/backend/mod.rs`

- [ ] Add `BackendId::Proxmox` (`"proxmox"`), the dispatch arms, `pub const AUTOMATIC: [BackendId; 2] = [Docker, Proxmox]`, and `a `clap::ValueEnum` for it in the CLI (Task 4).
- [ ] Add `backend: BackendId` to `ContainerList`; Docker's `listed` sets `BackendId::Docker`.
- [ ] Write `LIST_SCRIPT`, `parse_listing` (header offsets, fallback by fields, running only), `classify`, `exec_words(vmid, via)`, the `Proxmox` adapter and its issues, as in the spec's tables.
- [ ] Unit tests: parsing (lock column, empty name, banners, stopped, unreadable rows), each failure row, shell words with and without sudo.

## Task 3: The app chooses the backend

**Files:** `src/app/containers.rs`, `src/backend/listing.rs`

- [ ] `App::containers(target, backend: Option<BackendId>, batch, cancelled)`: with `Some`, list with it; with `None`, walk `AUTOMATIC`, moving on only on a `not-installed` issue, and return the first backend's issue when all are missing.
- [ ] `App::container_shell` takes `backend`; refuses `user` for Proxmox (`ContainerError::UserNotSupported`); adds `--backend proxmox` and `--via sudo` to the new-terminal arguments.
- [ ] `local_program` maps `pct` too (`ContainerPrograms::pct`), for tests and symmetry.
- [ ] `resolve` gains the exact-ID step.
- [ ] Tests with fake programs: automatic order, explicit backend, shell over ssh with and without sudo, user refusal, new-terminal arguments.

## Task 4: CLI and JSON

**Files:** `src/cli/mod.rs`, `src/cli/containers.rs`, `src/cli/json.rs`, `docs/json-output.md`, `tests/fixtures/json/containers-proxmox*.json`

- [ ] `--backend docker|proxmox` on `containers` and `shell`; `--via sudo`; pass the backend through `list_with_offers` and the held shell.
- [ ] Human listing: `Proxmox containers` heading, VMID/name/status columns; skipped note names `pct`.
- [ ] JSON: `backend` key only for non-Docker lists, `image: null` for Proxmox rows, `kind: proxmox` errors without the offer step.
- [ ] Golden tests for `containers-proxmox.json` and `containers-proxmox-denied.json`; document them in `docs/json-output.md`.

## Task 5: TUI

**Files:** `src/tui/containers.rs`, `src/tui/mod.rs` (wherever `PanelAction::Shell` and the lister call `containers`)

- [ ] `PanelAction::Shell` carries `backend`; the lister passes `None` (automatic).
- [ ] Proxmox heading, `user: root (Proxmox)`, `u` ignored and left out of the footer, details without image, skipped note with `pct`.
- [ ] Tests for each.

## Task 6: Docs and roadmap

**Files:** `ROADMAP.md`, `docs/architecture.md`, `README.md` (if it documents containers)

- [ ] Remove slice 1 from `ROADMAP.md`, renumber, fix every "slice N" reference.
- [ ] `docs/architecture.md`: add `backend::proxmox` to the module table and describe `AUTOMATIC` in "Add a backend" step 4.
- [ ] README: mention Proxmox in the containers section.
- [ ] Run `scripts/ci-checks.sh` (all), commit, push, open the PR.
