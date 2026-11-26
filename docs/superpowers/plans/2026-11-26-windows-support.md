# Windows Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Portico builds, runs and is fully tested on Windows (x64, arm64) with Windows' own OpenSSH, and ships through winget.

**Architecture:** A `windows` implementation of the platform layer beside `unix`/`linux`/`macos`, selected with `cfg`. Code that is portable moves out of `unix` into a shared `common` module. Every direct Unix call outside `platform` moves behind a platform function first (on Unix, no behaviour change), then Windows implements those functions. The tests keep their `sh` fakes through a launcher shim.

**Tech Stack:** Rust 2024, `windows-sys` (Windows only), `libc` (Unix only), crossterm/ratatui, `portable-pty` (ConPTY), GitHub Actions `windows-latest`, winget manifests.

**Spec:** `docs/superpowers/specs/2026-11-26-windows-support-design.md`

## Global Constraints

- Linux and macOS behaviour, flags, human output and JSON documents do not change.
- `windows-sys` only under `[target.'cfg(windows)'.dependencies]`, used only in `src/platform/windows/`; `libc` moves to `[target.'cfg(unix)'.dependencies]`.
- No `cfg(windows)`/`cfg(unix)` outside `src/platform/` except: `src/tui/hangup.rs` (Unix-only module), test code, and the `desktop` refusal (same place macOS refuses it).
- Do not touch `domain::settings` logic or the container shell user picker (only `config_dir` moves).
- Home on Windows: `USERPROFILE` only. Settings: `%APPDATA%\portico\settings.toml`, `XDG_CONFIG_HOME` wins when set. System config: `%ProgramData%\ssh\ssh_config` (default `C:\ProgramData\ssh\ssh_config`). Runtime: `~\.ssh\portico\run\tunnels`.
- Paths written into OpenSSH files use `/` and `~`; paths shown to people use `\`.
- Windows tunnels: `ssh -N` without `-f`, `-o LogLevel=VERBOSE`, detached (`CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`: a hidden console of its own, which proxy programs inherit); every kind up when the log shows `Authenticated to` and ssh still runs 1 s later, `-L`/`-D` also with the listen address bound; at most 30 s.
- Commit messages: Conventional Commits, no AI attribution lines. Run `scripts/ci-checks.sh` checks as a regular user.
- After every task: `cargo check --target x86_64-pc-windows-gnu --all-targets` must not gain errors in files the task touched (from Task 7 on it must pass).

## Review Focus

1. A user whose `HOME` is set by Git Bash: Portico must still use `%USERPROFILE%\.ssh` (test in Task 4).
2. A tunnel whose `ssh` exits during the readiness wait (bad host, refused forward): reported as failed with the log tail, never as started (test in Task 6).
3. A local port already taken by another program when starting `-L` on Windows: the `bind_probe` with `SO_EXCLUSIVEADDRUSE` must see it as busy, not ready (test in Task 6).
4. Paths with spaces (`C:\Users\Jane Doe\.ssh`) in the managed `Include` and in launcher command lines (tests in Task 4 and Task 5).
5. A recycled PID: a record whose start time differs from the live process is `Gone`, and `stop` never terminates it (test in Task 5).

---

### Task 1: Prove the Windows end-to-end server (spike)

**Files:**
- Create: `scripts/windows-sshd-spike.ps1` (deleted again in Task 10)
- Modify: `.github/workflows/ci.yml` (temporary `windows-spike` job, `runs-on: windows-latest`, runs the script)

- [ ] **Step 1:** Write the script: install the OpenSSH Server capability if `C:\Windows\System32\OpenSSH\sshd.exe` is missing (else download the pinned Win32-OpenSSH release zip), generate a host key and a client key in `$env:RUNNER_TEMP`, write a config (`Port 0`-free port picked by the script, `ListenAddress 127.0.0.1`, `HostKey`, `AuthorizedKeysFile <abs path>`, `PidFile`, `StrictModes no`), start `sshd.exe -D -e -f <config>` as the runner user (not the service), then `ssh -i <key> -p <port> -o StrictHostKeyChecking=no $env:USERNAME@127.0.0.1 echo ok`.
- [ ] **Step 2:** Push the branch, open the PR as a draft, read the job log.
Expected: `ok` printed. If the unprivileged server fails, switch the script to the service fallback (spec: Settled questions) and record which one works in the spec's Tests section in the same commit.
- [ ] **Step 3:** Commit: `ci: prove an unprivileged sshd on the Windows runner`.

### Task 2: Platform skeleton, dependencies and line endings

**Files:**
- Modify: `Cargo.toml`, `src/platform/mod.rs`, `src/platform/{fs,paths,process,launcher}.rs`
- Create: `src/platform/common/{mod,paths,fs}.rs`, `src/platform/windows/{mod,fs,paths,process,launcher}.rs` (empty modules with `//!` docs), `.gitattributes` (`* text=auto eol=lf`)
- Move: from `unix/paths.rs`: `tilde`, `tilde_text`, `expand`, `config_dir` into `common/paths.rs`; from `unix/fs.rs`: `normalize_path`, `read_lossy`, `open_lock`/`acquire_lock` (fs2 is portable), `PendingFile`, the fault hooks, `Stamp` struct into `common/fs.rs`. `unix` keeps what calls `libc` or `std::os::unix`.

**Interfaces:**
- Produces: `platform::{fs,paths,process,launcher}` facades re-export `common` + (`unix` + `os`) on Unix, `common` + `windows` on Windows. `compile_error!` now only for targets that are none of linux/macos/windows.

- [ ] **Step 1:** Make `unix` `#[cfg(unix)]`, add `#[cfg(windows)] mod windows;` and `use windows as os` under Windows; facades re-export accordingly.
- [ ] **Step 2:** Move the portable items; `tilde`/`expand` accept `~\` and `~/` (unit test `expand_accepts_both_separators` in `common/paths.rs`: `expand("~\\a", home) == home.join("a")` and `expand("~/a", home)` likewise).
- [ ] **Step 3:** `cargo test --all-targets` on Linux. Expected: PASS, no behaviour change.
- [ ] **Step 4:** Commit: `refactor: share the portable platform code and prepare a Windows module`.

### Task 3: Seams for the Unix calls outside the platform layer

Every item below is a platform function implemented on Unix now (same behaviour as the code it replaces) and on Windows in Tasks 4–5. Callers stop using `libc` and `std::os::unix`.

**Files:**
- Modify: `src/cli/mod.rs` (`exit_code`, SIGHUP registration), `src/domain/openssh/connect.rs` (`process_group(0)`), `src/domain/tunnel/runtime.rs` (`bind_probe`, `OsStrExt`, `uid`), `src/domain/tunnel/mod.rs` and `src/tui/mod.rs` (`localtime_r`), `src/domain/adopt/mod.rs` (`access`, `geteuid`, errno), `src/app/identities.rs` (`getuid`), `src/app/tunnels.rs`, `src/domain/openssh/keys.rs`, `src/domain/managed/store.rs`, `src/desktop.rs` (`.mode(..)`, `permissions().mode()`)
- Create: `src/platform/unix/net.rs`, `src/platform/net.rs` facade, `src/platform/unix/time.rs`, `src/platform/time.rs` facade

**Interfaces (Produces):**
- `process::Uid` — `pub type Uid = u32` (Unix) / `String` (Windows); `current_uid() -> Uid`; `Proc::uid(&self, pid: u32) -> Option<Uid>`; `is_tunnel`, `find_tunnel`, `identify`, `scan` take `&Uid`.
- `process::exit_code(status: ExitStatus) -> Option<u8>` (code, or 128+signal on Unix; `None` when out of range).
- `process::own_group(command: &mut Command) -> &mut Command`.
- `process::detach(command: &mut Command) -> &mut Command` (Unix: `setsid` in `pre_exec`; used by Task 6 and the runtime test that does it today).
- `process::is_superuser() -> bool`.
- `process::HANGUP_SIGNALS: &[i32]` (Unix: SIGINT, SIGTERM, SIGHUP; Windows: SIGINT, SIGTERM).
- `net::bind_probe(addr: SocketAddr) -> io::Result<()>` (moves the current code unchanged; `cloexec_socket` becomes private to `unix`).
- `fs::writable(path: &Path) -> io::Result<bool>` and `fs::is_read_only_error(error: &io::Error) -> bool`.
- `fs::private(options: &mut OpenOptions, mode: u32) -> &mut OpenOptions` (sets mode on Unix, no-op on Windows); `fs::is_executable(meta: &Metadata) -> bool`.
- `time::local(epoch_secs: i64) -> Option<LocalTime>` with `pub struct LocalTime { year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8 }` (only the fields the two callers format).
- `paths::system_ssh_config() -> PathBuf` replaces the `SYSTEM_SSH_CONFIG` constant (keep the constant on Unix for the function to return).

- [ ] **Step 1:** Add each function on Unix and switch its callers. Tests under `#[cfg(test)]` that create files with modes keep `std::os::unix` but get `#[cfg(unix)]`.
- [ ] **Step 2:** `cargo clippy --all-targets -- -D warnings && cargo test --all-targets` on Linux. Expected: PASS.
- [ ] **Step 3:** `grep -rn "libc::\|std::os::unix\|os::fd" src --include=*.rs | grep -v "^src/platform\|^src/tui/hangup.rs"` lists only `#[cfg(unix)]` test code.
- [ ] **Step 4:** Update the `domain` exceptions paragraph in `docs/architecture.md`. Commit: `refactor: route the remaining Unix calls through the platform layer`.

### Task 4: Windows paths and files

**Files:**
- Create/fill: `src/platform/windows/paths.rs`, `src/platform/windows/fs.rs`, `src/platform/windows/time.rs`, `src/platform/windows/net.rs`
- Modify: `Cargo.toml` (`windows-sys` features as needed: `Win32_Foundation`, `Win32_Storage_FileSystem`, `Win32_Networking_WinSock`, `Win32_System_Threading`, `Win32_Security`, `Win32_System_Diagnostics_ToolHelp`, `Win32_System_Console`, `Win32_UI_Shell`)

**Interfaces (Produces):** the Windows versions of `home_dir`, `runtime_dir(home, _)`, `system_ssh_config`, `config_dir` (via common: `XDG_CONFIG_HOME` else `%APPDATA%\portico`), `create_private_dir`, `reject_symlink`, `symlink_target`, `write_unique` (`MoveFileExW` replace + write-through, retry up to 1 s on access denied), `sync_dir` (no-op, keeps the fault hooks), `stamp` (volume serial, file index, size, last-write, change time), `writable`, `is_read_only_error`, `private`, `is_executable` (`.exe`/`.cmd`/`.bat` extension), `bind_probe` (`SO_EXCLUSIVEADDRUSE`), `time::local` (`localtime_s`).

- [ ] **Step 1:** Unit tests in `windows/paths.rs` (run in Windows CI): `home_ignores_home_variable` (set `HOME=C:\elsewhere`, `USERPROFILE=<tmp>` → `<tmp>`), `system_config_defaults_under_programdata`, `runtime_dir_is_under_dot_ssh`. In `windows/fs.rs`: `stamp_changes_when_rewritten`, `write_unique_replaces_an_open_file` (another handle open with `FILE_SHARE_READ|FILE_SHARE_DELETE`), `bind_probe_sees_a_listening_port_as_busy`. In `common/paths.rs`: `tilde_keeps_spaces` (`C:\Users\Jane Doe\.ssh\x` → `~\.ssh\x`).
- [ ] **Step 2:** Implement. Managed-file rendering of `Include`/`IdentityFile` paths: a `paths::openssh_path(path, home) -> String` in `common` (forward slashes, `~` prefix under home; on Unix identical to today's output). Test `openssh_path_uses_forward_slashes`.
- [ ] **Step 3:** `cargo check --target x86_64-pc-windows-gnu --lib` reports no error in `src/platform/windows/{paths,fs,time,net}.rs`.
- [ ] **Step 4:** Commit: `feat: Windows paths and files in the platform layer`.

### Task 5: Windows process control

**Files:** `src/platform/windows/process.rs` (split into `process/{pin,proc,tree}.rs` if it passes ~400 lines)

**Interfaces (Produces):** `Pin` (`open(pid)`, `signal` = terminate after `WaitForSingleObject(h,0)` check, `wait_exit(ms)`), `fail_pin_after`, `pin_failure_injected`, `has_exited(&Child)`, `kill_process_tree(root: u32)`, `terminate_process_tree(&mut Child)`, `terminate_process_group(&mut Child)` (tree), `kill_direct_child`, `reap_child_bounded`, `stop_process(pid, verify) -> io::Result<bool>`, `TERM_GRACE_MS`, `Proc { system(), boot_id(), started_at(start), stat(pid) -> Option<Stat>, uid(pid) -> Option<Uid>, cmdline(pid) -> Option<Vec<Vec<u8>>> (UTF-8 bytes of each argument), pids(), scan(&Uid) -> Vec<Logged> }`, `current_uid()`, `exit_code`, `own_group` (`CREATE_NEW_PROCESS_GROUP`), `detach`, `is_superuser() = false`, `HANGUP_SIGNALS`.

- [ ] **Step 1:** Tests in `windows/process.rs`: `proc_reads_own_command_line` (spawn `cmd /c ping -n 30 127.0.0.1` with a marker argument; `cmdline` contains it; `uid == current_uid()`), `stat_start_is_stable_and_differs_between_processes`, `started_at_is_close_to_now`, `kill_process_tree_ends_children` (`cmd /c "ping -n 30 127.0.0.1"` grandchild gone), `stop_process_refuses_when_verify_fails`, `pin_reports_an_exited_process_as_gone`. The existing `domain::tunnel::runtime` live-process test (spec'd for macOS) gets a Windows command line (`cmd /c "ping -n 30 127.0.0.1 >nul & rem ssh -E <log> -L … -- host"` or a copied `ping.exe` renamed `ssh.exe`) so recognise/rebuild/identify run on Windows; `identity_is_gone_for_another_start` covers Review Focus 5.
- [ ] **Step 2:** Implement with Toolhelp snapshots, `OpenProcess`, `GetProcessTimes`, `GetTokenInformation(TokenUser)` + `ConvertSidToStringSidW`, `NtQueryInformationProcess(ProcessCommandLineInformation = 60)` from `ntdll` (declare it; `windows-sys` `Wdk_System_Threading` if available), `CommandLineToArgvW`, boot id = `(now − GetTickCount64)` rounded to seconds as text.
- [ ] **Step 3:** `cargo check --target x86_64-pc-windows-gnu --lib` reports no error in `src/platform/windows/process*`.
- [ ] **Step 4:** Commit: `feat: process control for Windows without signals`.

### Task 6: Starting tunnels on Windows

**Files:** `src/domain/tunnel/mod.rs` (`start_args`), `src/app/tunnels.rs` (start loop), tests in both.

**Interfaces:**
- Consumes: `process::detach`, `net::bind_probe`, `Proc::stat`.
- Produces: `start_args(log, host, forward, background: bool)`; `pub fn ready(forward: &Forward, log: &Path, alive_since_auth: Option<Instant>) -> bool` in `domain::tunnel::runtime`; `platform::process::SSH_BACKGROUNDS: bool` (`true` on Unix, `false` on Windows).

- [ ] **Step 1:** Tests: `start_args_without_background_drop_f_and_raise_log_level` (`background=false` → no `-f`, contains `-o LogLevel=VERBOSE`; `true` → today's exact list); `ready_for_local_forward_once_bound` (a `TcpListener` on the listen address → true; free → false); `ready_for_remote_forward_after_auth_and_a_second` (log with `Authenticated to host` and `alive_since_auth` 1.1 s ago → true; 0.2 s → false; no line → false); in `tests/tunnels.rs` (runs on all systems through the fakes, Windows path exercised in Windows CI): `a_tunnel_whose_ssh_exits_while_waiting_is_failed` and `a_busy_local_port_is_not_mistaken_for_ready` (Review Focus 2, 3).
- [ ] **Step 2:** In `start_tunnel`, when `!SSH_BACKGROUNDS`: spawn `detach`ed with `background=false`, poll every 20 ms: exited → failure path as today; `ready(..)` → record the child's own pid/start (no `find_tunnel` needed); cancelled → `kill_process_tree`.
- [ ] **Step 3:** `cargo test --all-targets` on Linux PASS (Unix path unchanged).
- [ ] **Step 4:** Commit: `feat: start saved port forwards on Windows, whose ssh cannot background itself`.

### Task 7: Launcher, TUI, desktop and local Docker on Windows

**Files:** `src/platform/windows/launcher.rs`, `src/tui/mod.rs` (`wait_for_enter`, `mod hangup` gated `#[cfg(unix)]` with a Windows `next_event` that reads crossterm directly and maps an error to `Closed`), `src/tui/terminal.rs`, `src/cli/mod.rs` and `src/main.rs` and `src/tui/settings.rs` (desktop refusal/hiding: extend the macOS condition to `not(target_os = "linux")`), `src/backend/docker.rs` / `src/app/containers.rs` (local target on Windows: skip the access probe, decision `NoOffer`).

**Interfaces (Produces):** `DEFAULT_LAUNCHER = "wt"`; `launch(launcher, command) -> io::Result<Launch>`: `PORTICO_TERMINAL` executable path → run with command as args; `PORTICO_TERMINAL=cmd` or no `wt.exe` on `PATH` → `CREATE_NEW_CONSOLE`; else `wt.exe -w new -- <command…>`; any other `launcher` → run with command as args.

- [ ] **Step 1:** Tests: `windows/launcher.rs` `wt_command_line_quotes_spaces` (pure function `wt_args(command) -> Vec<OsString>` → `["-w","new","--","C:\\Program Files\\x.exe","a b"]`), `falls_back_to_console_without_wt` (pure `choose(env_terminal, wt_found) -> Choice`); `backend::docker` `local_windows_target_offers_nothing`.
- [ ] **Step 2:** Implement; `wait_for_enter` uses `crossterm::event::poll(IDLE_TICK)` + key events on Windows.
- [ ] **Step 3:** `cargo check --target x86_64-pc-windows-gnu --all-targets` — lib: zero errors; tests may still fail to build (Task 8).
- [ ] **Step 4:** Commit: `feat: Windows Terminal launcher and the TUI on Windows`.

### Task 8: Test support on Windows (the shim)

**Files:** `examples/sh_shim.rs` (or tests-compiled helper; `exclude` it from the package in `Cargo.toml`), `tests/support/executable.rs`, `tests/support/mod.rs` (set `USERPROFILE` and `APPDATA` alongside `HOME`/`XDG_CONFIG_HOME`; helpers `exe(path)` adding `.exe` on Windows), every `tests/*.rs` and `src/**` test needing `#[cfg(unix)]` (list each in the commit message body).

**Interfaces (Produces):** `write_executable(path, script)` — Windows: writes `<path>.sh` (LF) and copies the shim to `<path>.exe`; returns nothing (as today). Shim: `argv[0]` minus `.exe` + `.sh`, runs `sh` (`SH` env var, else `C:\Program Files\Git\bin\sh.exe`, else `sh` on `PATH`) with the script and args, stdio inherited, exits with the child's code.

- [ ] **Step 1:** Test `tests/support` self-test `shim_passes_arguments_and_exit_code` (script `printf '%s\n' "$@"; exit 7` called with `["a b", "c"]` → stdout `a b\nc\n`, code 7).
- [ ] **Step 2:** Replace the `python3 tests/support/tui_pty.py` uses in `tests/acceptance.rs` with a `portable-pty` helper in `tests/support/pty.rs` (same inputs/outputs; delete `tui_pty.py` once unused).
- [ ] **Step 3:** Mark Unix-only tests `#[cfg(unix)]`; for permission-denied tests add Windows twins using `icacls <path> /deny "%USERNAME%:(W)"`. Paths compared with `Path::join`.
- [ ] **Step 4:** Linux: `scripts/ci-checks.sh test` PASS. Windows: verified in Task 10.
- [ ] **Step 5:** Commit: `test: run the integration tests on Windows through a shell shim`.

### Task 9: End-to-end suite on Windows

**Files:** `tests/e2e/sshd.rs` (Windows `SSHD`/`KEYGEN` paths, config from Task 1's result, key ACLs via `icacls /inheritance:r /grant:r "%USERNAME%:F"`), `tests/e2e/terminal.rs` (`portable-pty` everywhere), `tests/e2e/procs.rs` (Windows via `Proc`/`tasklist`), `tests/e2e/tui_hangup.rs` (`#[cfg(unix)]`), `scripts/ci-checks.sh` (`SSHD` variable, `.exe` handling under Git Bash).

- [ ] **Step 1:** Linux: `scripts/ci-checks.sh e2e` PASS.
- [ ] **Step 2:** Commit: `test: run the end-to-end suite on Windows through ConPTY`.

### Task 10: Windows CI

**Files:** `.github/workflows/ci.yml` (replace `windows-spike` with `windows` matrix `[clippy, test, e2e]`, `fail-fast: false`, `timeout-minutes: 30`, check limited to 20 min, `if` on the `rust` filter like `macos`, shell `bash`, OpenSSH Server set-up only for `e2e`), delete `scripts/windows-sshd-spike.ps1`, `CONTRIBUTING.md` CI paragraph.

- [ ] **Step 1:** Push; all three `windows (…)` jobs green, plus every existing job. Fix failures with `superpowers:systematic-debugging`.
- [ ] **Step 2:** Commit: `ci: run clippy, the tests and the end-to-end suite on Windows`.

### Task 11: Release builds and winget

**Files:** `.github/workflows/release.yml` (`build-windows`: both MSVC targets on `windows-latest`, `--version` of x64, zip with `portico.exe` + licences + README via `scripts/package-release.sh` extended for `.zip`, CycloneDX SBOM, `actions/attest`; `checksums` needs it; `winget` job after `release`, tags only, `winget` environment, skipped without `WINGET_TOKEN`), `packaging/winget/{YounesAmalou.Portico.yaml,YounesAmalou.Portico.installer.yaml,YounesAmalou.Portico.locale.en-US.yaml}` templates (manifest 1.9, `InstallerType: zip`, `NestedInstallerType: portable`, `PortableCommandAlias: portico`), `scripts/winget-manifest.sh <version> <x64 zip> <arm64 zip> <outdir>`, `tests/release_scripts.rs` test `winget_manifest_renders_version_urls_and_hashes`. Rehearsals run `winget validate` on the rendered manifests (on the Windows runner, skip when `winget` is absent).

- [ ] **Step 1:** Write the failing `release_scripts` test; implement the script; PASS.
- [ ] **Step 2:** `gh workflow run release.yml --ref feat/windows-support`; the rehearsal is green including `build-windows`.
- [ ] **Step 3:** Commit: `feat: release Windows builds and publish them to winget`.

### Task 12: Documentation and roadmap

**Files:** `README.md` (install with `winget install YounesAmalou.Portico`; Windows notes: Docker Desktop only in Linux containers mode, Docker Engine inside WSL → run Portico in WSL, `PORTICO_TERMINAL`, tunnels on Windows), `docs/architecture.md` (platform tree with `common` and `windows`; modules table; "Add a platform"), `CONTRIBUTING.md` (release: Windows artifacts, `winget` job), `ROADMAP.md` (remove v5 slice 2, renumber; Production readiness: a winget row like Homebrew's waiting for `WINGET_TOKEN`, an Authenticode row; drop the "Windows support… scheduled" sentence), v1 design's "Delivered follow-up slices" list, the spec's Status line → "Implemented".

- [ ] **Step 1:** `cargo test --test docs` PASS.
- [ ] **Step 2:** Commit: `docs: document Windows support`.
