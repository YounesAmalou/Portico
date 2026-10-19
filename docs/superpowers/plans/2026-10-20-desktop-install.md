# Desktop Install Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `portico desktop install | uninstall | status`, which installs the embedded Quickshell panel, a launcher entry and a keybinding, each previewed and confirmed, and keeps the installed panel current from ordinary `portico` runs.

**Architecture:** `src/desktop.rs` holds `DesktopEnv` (the environment, read in `main.rs`), target and compositor detection, and the plan and apply steps for each component. `src/desktop/combo.rs` parses and prints key combinations; `src/desktop/panel.rs` embeds the panel, writes it, stamps it and refreshes it; `src/desktop/launcher.rs` writes the marked `.desktop` file and icon; `src/desktop/keybinding.rs` checks clashes (`hyprctl binds -j`, the Sway configuration), builds the line for each setup and edits the marked block. `src/cli/desktop.rs` prints plans and status and asks the questions. `main.rs` dispatches `desktop` commands before building the `App`, and refreshes the panel before any other command.

**Tech Stack:** Rust (std only: `include_bytes!`, `std::process::Command`, `std::os::unix::fs::symlink`), `serde_json` for `hyprctl` and `omarchy plugin list` output, clap for the commands; QML `FileView` for the banner.

**Spec:** `docs/superpowers/specs/2026-10-20-desktop-install-design.md`

## Global Constraints

- No change outside Portico's panel directory without a printed plan and a yes (or `--yes`); the auto-refresh writes only inside the panel directory and prints nothing.
- External programs run with argument lists, found on `DesktopEnv`'s `PATH`.
- Tests never touch the real `~/.config`: integration tests use a temporary `HOME`, `XDG_*` inside it and a `PATH` of fake programs.
- Run `scripts/ci-checks.sh` checks as a regular user; Conventional Commits; no AI attribution.

## Review Focus

- The refresh cannot fail or slow down a command, and never runs for `desktop`.
- Uninstall finds only what install created (link target, marker comment, marker lines) and restores a keybinding file byte for byte.
- `--yes` still refuses a taken combination before changing anything.

### Task 1: Combinations and keybinding files

- [ ] `combo.rs`: `Combo::parse`, `Display`, `modmask`, Hyprland/Sway spellings, with unit tests.
- [ ] `keybinding.rs`: `Setup` (Omarchy Lua, Hyprland Lua, Hyprland conf, Sway, other), the file to edit, the line, `hyprctl` clash check, Sway parser, marked block append/remove/find, with unit tests.

### Task 2: Panel and launcher

- [ ] `panel.rs`: embedded list (test against `desktop/quickshell/`), stamp, `write`, `refresh`, `state`.
- [ ] `packaging/desktop/portico.desktop` and `portico.svg`; `launcher.rs` marks them and sets `Exec=`.
- [ ] `Cargo.toml` `exclude` narrowed; `tests/package.rs` updated.

### Task 3: CLI

- [ ] `Command::Desktop` with `install`, `uninstall`, `status` and their flags; `cli::desktop` plans, prompts, applies; `main.rs` reads `DesktopEnv` and refreshes the panel for other commands.

### Task 4: Panel banner and packaging

- [ ] `portico.js` `versionBanner(stamp, versionOutput)`, tested in `tst_portico.qml`; `PorticoModel.qml` reads `VERSION` and runs `--version`; `PorticoView.qml` shows the banner.
- [ ] `packaging/aur/PKGBUILD` installs the launcher files.

### Task 5: Integration tests

- [ ] `tests/desktop.rs` with fake programs, covering the spec's Testing list.

### Task 6: Docs and checks

- [ ] README "Desktop panel" and the ownership statement; ROADMAP row removed, Unscheduled item added; spec status; delivered slice bullet; doc guards.
- [ ] All of `scripts/ci-checks.sh`, plus `HOME=$(mktemp -d) SSH_AUTH_SOCK= XDG_RUNTIME_DIR= cargo test --all-targets`.
