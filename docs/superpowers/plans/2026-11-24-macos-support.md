# macOS Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build, test and release Portico on macOS through a second implementation of the platform layer, without changing Linux behaviour.

**Architecture:** `src/platform/unix/` holds what Linux and macOS share (files, home paths, the process algorithms); `src/platform/linux/` and `src/platform/macos/` hold only what differs (process pinning and the process table, the terminal launcher, the runtime directory). Selection stays at compile time with `cfg(target_os)`, without trait objects.

**Tech Stack:** Rust 2024, MSRV 1.88, `libc` (kqueue, sysctl, libproc on macOS), GitHub Actions `macos-latest`.

**Spec:** `docs/superpowers/specs/2026-11-24-macos-support-design.md`

## Global Constraints

- Linux behaviour, CLI flags, human output and JSON output do not change.
- Every task ends green on `scripts/ci-checks.sh fmt`, `clippy` and `test` on Linux, run as a regular user, and on `cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings` (checking for macOS needs no Apple linker). macOS tests run in CI only.
- Areas another session is changing (`src/backend`, `src/app`, `src/tui`, `src/cli`) get only the minimal edits listed here.
- Conventional Commits, no AI attribution.

## Review Focus

1. **PID reuse on macOS**: `stop_process` must not signal a process whose kqueue exit watch has fired. Test `stop_process_never_signals_an_exited_process` (both systems).
2. **Fault hooks**: `fail_pin_after` must still reach the tree walk; the connection tests that use it must fail if the hook is deleted (check once locally).
3. **Linux unchanged**: the moved Linux code keeps its exact behaviour; `git diff` of the moved functions should show moves and renames only.

## Tasks

### Task 1: Shared Unix platform code

- [ ] Move `linux/fs.rs` to `unix/fs.rs` whole (`git mv`); add `cloexec_socket` and `nonblocking_pipe` there (atomic flags on Linux, `fcntl` after the call on macOS).
- [ ] Split `linux/paths.rs`: `home_dir`, `tilde`, `tilde_text`, `expand` and their tests to `unix/paths.rs`; `SYSTEM_SSH_CONFIG` and `runtime_dir` stay per system.
- [ ] Split `linux/process.rs`: the algorithms (`has_exited`, `terminate_*`, `kill_direct_child`, `reap_child_bounded`, `kill_process_tree`, `kill_process_group`, `stop_process`, `current_uid`, `TERM_GRACE_MS`, `Logged`, `Proc::scan`, the fault hook renamed `fail_pin_after`/`pin_failure_injected`) to `unix/process.rs`; Linux keeps `Pin` (pidfd), `children`, `wait_until_stopped`, `Stat` and `Proc`'s readers.
- [ ] Point `domain::tunnel::runtime::bind_v6_only` and `tui::hangup` at the new helpers; rename the hook in `domain::openssh::connect` tests.

### Task 2: macOS platform

- [ ] `macos/process.rs`: `Pin` on kqueue, `children` via `proc_listchildpids`, `wait_until_stopped` via `KERN_PROC_PID`, `Proc` via sysctl (`KERN_PROC_PID`, `KERN_PROCARGS2`, `proc_listallpids`, `kern.bootsessionuuid`).
- [ ] `macos/paths.rs`: `SYSTEM_SSH_CONFIG`, `runtime_dir` (always `~/.ssh/portico/run/tunnels`).
- [ ] `macos/launcher.rs`: `DEFAULT_LAUNCHER = "open"`, the terminal choice (`PORTICO_TERMINAL`, `TERM_PROGRAM`, Terminal), the `.command` script, unit tests of the choice and of the invocations.
- [ ] `platform/mod.rs` and the four re-export files select `macos`; `compile_error!` names Linux and macOS.
- [ ] Cross-platform live-process test in `domain::tunnel::runtime`; Linux-only gating for the fake `/proc` tests.

### Task 3: Desktop panel on macOS

- [ ] Hide `portico desktop` from the help on macOS; `main` reports it as unsupported and skips the panel refresh.

### Task 4: Tests on macOS

- [ ] Replace direct `/proc` reads in `tests/` with helpers in `tests/support` (process alive or zombie, start time, boot ID, command lines).
- [ ] Gate Linux-only test code (`F_SETLEASE`, `/proc/self/fd`, `xdg-terminal-exec` desktop expectations) with `cfg(target_os = "linux")`; give the launcher tests a fake launcher through `PORTICO_TERMINAL` on macOS.

### Task 5: CI and release

- [ ] `ci.yml`: `macos` job (clippy, test, e2e).
- [ ] `release.yml`: `build-macos` matrix (aarch64, x86_64), into `checksums`; `homebrew` job.
- [ ] `packaging/homebrew/portico.rb` template and `scripts/homebrew-formula.sh`, with a test in `tests/release_scripts.rs`.

### Task 6: Documents

- [ ] `ROADMAP.md`: remove slice 5, renumber, fix "slice N" references.
- [ ] `docs/architecture.md`: the `unix`/`linux`/`macos` layout and the remaining `libc` calls.
- [ ] `README.md` and `CONTRIBUTING.md`: macOS install and the macOS CI job.
- [ ] Spec status: implemented, with "As built" notes.
