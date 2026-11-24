# Modular Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reorganize the `portico-ssh` crate into platform, domain, backend, app and interface layers, with Docker behind a backend interface, without changing any behaviour, CLI flag or JSON output.

**Architecture:** Six pull requests, one per part below, each a series of moves that leaves every test passing. Platform code is selected at compile time with `#[cfg(target_os = "linux")]`; backends are compiled in and dispatched through the `BackendId` enum. The app layer keeps running commands (locally or over `ssh`); a backend adapter decides which commands to run and what their output means.

**Tech Stack:** Rust 2024 edition, MSRV 1.88, `libc`, `thiserror`, `ratatui`/`crossterm` (TUI), `clap` (CLI).

**Spec:** `docs/superpowers/specs/2026-11-24-modular-architecture-design.md`

## Global Constraints

- No change in behaviour, CLI flags, human output or JSON output. The JSON golden documents and `docs/json-output.md` stay byte-for-byte identical.
- Tests move with the code they test. A test's body changes only where a type it constructs was replaced (`DockerIssue`, `Offer`); no assertion is weakened or removed.
- Every task ends green on `./scripts/ci-checks.sh fmt`, `clippy` and `test`; every part ends green on `e2e` and `docs` too. Run them as a regular user, never as root.
- Layer rule: a layer uses only the layers below it (`interface → app → domain + backend → platform`). `domain` never touches the OS directly, and nothing below `app` imports `crate::app`, `crate::cli` or `crate::tui`.
- The core layer's module is `domain`, never `core`.
- Platform code lives in `src/platform/linux/` and is re-exported from `src/platform/<concern>.rs` under `#[cfg(target_os = "linux")]`. No trait objects for platform concerns.
- No file-size rule: split along the seams named here, nothing more.
- Branch per part: `refactor/platform-layer`, `refactor/backend-interface`, `refactor/domain-layer`, `refactor/cli-modules`, `refactor/tui-modules`, `docs/architecture`. PR titles follow Conventional Commits (`refactor:` appears in the changelog under Changed, so write the title as a user-readable line).
- No AI attribution in commits or PR descriptions (`CLAUDE.md`).
- Do the moves with `git mv` where a file moves whole, so history follows.

## Review Focus

1. **Cancelling a container listing during Docker's probe** (Esc in the TUI while a host is slow): the listing must stop as cancelled, not report "permission denied". Test `a_cancelled_probe_stops_the_listing` in Task 2.2.
2. **A probe that fails for any other reason** (no `sh`, an ssh hiccup): the user must still see the plain Docker issue, not an ssh error. Test `a_failed_probe_reports_the_plain_issue` in Task 2.2.
3. **A TUI key that matches no offer**: pressing `s` while the failure offers `g` must do nothing. Test `a_key_runs_only_the_offer_bound_to_it` in Task 2.3.
4. **Fault-injection hooks** (`fail_pidfd_open_after`, `fail_next_directory_sync`, `fail_directory_sync_after`, `fail_next_transaction_rollback_write`): they must still reach the moved code, or their tests pass for the wrong reason. Each hook moves into the same file as the function it faults (Tasks 1.1 and 1.2), and the tests that use them stay where they are and must still fail if the hook is deleted. Check this once by deleting each hook locally.
5. **Building for another OS**: a build for a target other than Linux must fail with one clear message, not dozens of unresolved `libc` symbols. Covered by the `compile_error!` in Task 1.1, checked with `cargo check --target x86_64-apple-darwin` if the target is installed (skip otherwise and say so in the PR).

---

## Part 1: Platform layer (`refactor/platform-layer`)

### Task 1.1: Process control

**Files:**
- Create: `src/platform/mod.rs`, `src/platform/process.rs`, `src/platform/linux/mod.rs`, `src/platform/linux/process.rs`
- Modify: `src/lib.rs`, `src/openssh.rs:787-1211`, `src/tunnel/runtime.rs:195-530`, `src/app/tunnels.rs:23,353`

**Interfaces:**
- Produces, re-exported from `platform::process`:
  - `kill_process_tree(root: u32) -> io::Result<()>`, `open_pidfd(pid: libc::pid_t) -> io::Result<OwnedFd>`, `signal_pidfd(pidfd: &OwnedFd, signal: libc::c_int) -> io::Result<()>`
  - `terminate_process_tree(child: &mut Child) -> io::Result<()>`, `terminate_process_group(child: &mut Child)`, `kill_process_group(child: &mut Child)`, `kill_direct_child`, `reap_child_bounded`, `has_exited(child: &Child) -> io::Result<bool>`
  - `Stat`, `Proc` (both `impl` blocks), `Logged`, `current_uid() -> u32`, `stop_process(pid: u32, verify: impl FnOnce() -> bool) -> io::Result<bool>` with `TERM_GRACE_MS`
  - `#[cfg(test)] fail_pidfd_open_after(skipped: u32)`
- Stays in `tunnel/runtime.rs`: `RuntimeDir` (until Task 1.3), `RunningRecord`, `Record`, `find_in`, `names_log`, `Identity`, `now`, the log readers.

- [ ] **Step 1:** Create `src/platform/mod.rs` declaring `pub mod process;` and, at the top:

```rust
#[cfg(not(target_os = "linux"))]
compile_error!("Portico supports Linux only for now; macOS and Windows are planned in ROADMAP.md (v5).");
#[cfg(target_os = "linux")]
mod linux;
```

`src/platform/process.rs` is `pub use super::linux::process::*;`. Add `pub mod platform;` to `src/lib.rs`.

- [ ] **Step 2:** Move the items listed under Produces from `openssh.rs` (lines 787-1211, minus `parse_fingerprint`, `append_known_hosts` and `Copier`) and `tunnel/runtime.rs` (lines 195-387 and 496-530, minus `find_in` and `names_log`) into `src/platform/linux/process.rs`. Tests that call a moved function directly move with it; tests that drive `OpenSsh` or a tunnel (such as `cancellation_without_a_pidfd…`) stay where they are and only change their imports. Make the moved items `pub` inside the private `linux` module.

- [ ] **Step 3:** Update every caller to `crate::platform::process::…` (`openssh.rs`, `tunnel/runtime.rs`, `app/tunnels.rs`). Test helpers that read `/proc/<pid>/stat` directly (`tui/lister.rs`, `tui/tester.rs`, `app/tunnels.rs` tests) stay as they are: test-only code does not need the platform layer.

- [ ] **Step 4:** Run `./scripts/ci-checks.sh fmt && ./scripts/ci-checks.sh clippy && ./scripts/ci-checks.sh test`. Expected: all pass, same test count as on `main` (`cargo test 2>&1 | grep "test result"` before and after).

- [ ] **Step 5:** Commit: `git commit -m "refactor: move process control into the platform layer"`

### Task 1.2: Files: locks, atomic writes, file identity

**Files:**
- Create: `src/platform/fs.rs`, `src/platform/linux/fs.rs`
- Modify: `src/managed.rs:1299-1575`, `src/tui/watch.rs:30-60`, `src/tunnel/store.rs:12`, `src/tunnel/runtime.rs:17`, `src/openssh.rs:23`, `src/app/tunnels.rs:22`

**Interfaces:**
- Produces, re-exported from `platform::fs`: `symlink_target`, `normalize_path`, `create_private_dir`, `reject_symlink`, `open_lock`, `acquire_lock`, `read_lossy`, `write_unique`, `write_named_new`, `sync_dir`, `PendingFile`, with their current signatures; `#[cfg(test)] fail_next_directory_sync`, `fail_directory_sync_after`, `fail_next_transaction_rollback_write`.
- Produces: `pub struct Stamp { device: u64, inode: u64, size: u64, modified: (i64, i64), changed: (i64, i64) }` (moved from `tui/watch.rs`, fields as today) and `pub fn stamp(path: &Path) -> Option<Stamp>` (the body of `Filesystem::stamp`).

- [ ] **Step 1:** Move the listed helpers from `managed.rs` into `src/platform/linux/fs.rs` with their fault hooks and the tests that call them directly; re-export from `src/platform/fs.rs`.
- [ ] **Step 2:** Move `Stamp` into `src/platform/linux/fs.rs` and add `stamp(path)`; `tui/watch.rs` keeps the `Metadata` trait and `Filesystem::stamp` becomes `platform::fs::stamp(path)`.
- [ ] **Step 3:** Update callers (`managed.rs`, `tunnel/store.rs`, `tunnel/runtime.rs`, `openssh.rs`, `app/tunnels.rs`, `tui/watch.rs`).
- [ ] **Step 4:** Run fmt, clippy, test. Expected: all pass, same count.
- [ ] **Step 5:** Commit: `refactor: move file locking, atomic writes and file identity into the platform layer`

### Task 1.3: Paths and the terminal launcher

**Files:**
- Create: `src/platform/paths.rs`, `src/platform/launcher.rs`, `src/platform/linux/paths.rs`, `src/platform/linux/launcher.rs`
- Delete: `src/home.rs` (moved)
- Modify: `src/main.rs:21,56-60`, `src/tunnel/runtime.rs:23-45`, `src/managed.rs:93-110`, `src/openssh.rs:28-30,174-180,557-611`, `src/lib.rs`

**Interfaces:**
- Produces in `platform::paths`: `tilde`, `tilde_text`, `expand` (from `home.rs`, same signatures); `home_dir() -> Option<PathBuf>` (reads `HOME`); `runtime_dir(home: &Path, xdg_runtime_dir: Option<&OsStr>) -> PathBuf` (the body of `RuntimeDir::locate`, returning the directory); `SYSTEM_SSH_CONFIG: &str = "/etc/ssh/ssh_config"`.
- `RuntimeDir::locate(home, xdg)` keeps its signature and calls `platform::paths::runtime_dir`.
- Produces in `platform::launcher`: `DEFAULT_LAUNCHER: &str = "xdg-terminal-exec"`; `pub enum Launch { Requested, Failed(ExitStatus) }`; `pub fn launch(launcher: &Path, command: &[OsString]) -> io::Result<Launch>` (spawn with null stdio, the 500 ms `LAUNCH_GRACE` loop, the detached reaper thread, `terminate_process_tree` on a `try_wait` error).
- `OpenSsh::launch_new_terminal_with` keeps its signature; it builds the command (portico args or fallback) and maps `Launch` to `ConnectionOutcome::LaunchRequested` / `LaunchFailed`, and an `io::Error` to `OpenSshError::Io { program: self.terminal.clone(), source }`.
- `src/desktop/launcher.rs` is unchanged: its `xdg-terminal-exec` is the Omarchy `.desktop` file's text, not a launch.

- [ ] **Step 1:** Write the failing test in `src/platform/linux/paths.rs`:

```rust
#[test]
fn runtime_dir_falls_back_under_the_home_without_a_usable_xdg_dir() {
    let home = Path::new("/home/me");
    assert_eq!(runtime_dir(home, None), home.join(".ssh/portico/run/tunnels"));
    assert_eq!(
        runtime_dir(home, Some(OsStr::new("relative"))),
        home.join(".ssh/portico/run/tunnels")
    );
}
```

- [ ] **Step 2:** Run `cargo test runtime_dir_falls_back`. Expected: FAIL, `runtime_dir` not found.
- [ ] **Step 3:** Implement the Produces items; `git mv src/home.rs src/platform/linux/paths.rs` first so history follows, then add the new items. Move the existing `RuntimeDir::locate` tests next to `runtime_dir`.
- [ ] **Step 4:** Point `main.rs` at `home_dir()` (its error message when `HOME` is unset stays the same), `ConfigPaths::from_home` at `SYSTEM_SSH_CONFIG`, `OpenSsh::system` at `DEFAULT_LAUNCHER`, and every `crate::home::` path at `crate::platform::paths::`.
- [ ] **Step 5:** Run fmt, clippy, test, then `./scripts/ci-checks.sh e2e` and `docs`. Expected: all pass. The `early_launcher_failure_is_reported` and `missing_launcher_is_an_error` tests in `openssh.rs` pass unchanged.
- [ ] **Step 6:** Commit: `refactor: move paths and the terminal launcher into the platform layer`, push the branch, open the PR `refactor: move Linux-specific code behind a platform layer`.

## Part 2: Backend interface (`refactor/backend-interface`)

### Task 2.1: The backend types

**Files:**
- Create: `src/backend/mod.rs`
- Modify: `src/lib.rs`, `src/app/containers.rs:25-86`, `src/container.rs:433-437`

**Interfaces:**
- Produces in `backend` (spec, "The backend interface" and "What a backend reports"):
  - `Target` (moved from `app/containers.rs` with `describe` and `Display`), `GroupTool` (moved from `container.rs`), `ContainerList` (moved, fields unchanged).
  - `pub enum BackendId { Docker }` with `as_str(self) -> &'static str` (`"docker"`).
  - `pub enum Problem { NotInstalled, ServiceDown, PermissionDenied, Other }` with `as_str(self) -> &'static str`.
  - `pub struct BackendIssue { backend, target, problem, headline, next, offer: Option<Offer>, stderr }`.
  - `pub struct Offer { key: char, hint: String, prompt: String, words: Vec<String> }`.
  - `RunFailure<E> { Cancelled(E), Failed(E) }`, `ListFailure<E> { Run(E), Issue(BackendIssue) }`.
  - `pub trait Backend` with `id`, `list<E>`, `shell_words`, `plain_issue`, signatures as in the spec.
  - The dispatch functions come with the first adapter in Task 2.2.

- [ ] **Step 1:** Write the failing test in `src/backend/mod.rs`:

```rust
#[test]
fn ids_and_problems_keep_the_json_strings() {
    assert_eq!(BackendId::Docker.as_str(), "docker");
    assert_eq!(Problem::NotInstalled.as_str(), "not-installed");
    assert_eq!(Problem::ServiceDown.as_str(), "daemon-down");
    assert_eq!(Problem::PermissionDenied.as_str(), "permission-denied");
    assert_eq!(Problem::Other.as_str(), "other");
}
```

- [ ] **Step 2:** Run `cargo test ids_and_problems_keep`. Expected: FAIL, module `backend` not found.
- [ ] **Step 3:** Create the module with the types above; move `Target`, `GroupTool` and `ContainerList`, and re-point their users (`app`, `cli`, `cli/json.rs`, `tui/containers.rs`) at `crate::backend::…`. Leave `DockerIssue`, `DockerProblem` and the enum `Offer` in place for now.
- [ ] **Step 4:** Run fmt, clippy, test. Expected: all pass.
- [ ] **Step 5:** Commit: `refactor: add the backend types`

### Task 2.2: Docker as the first adapter

**Files:**
- Create: `src/backend/docker.rs` (from `git mv src/container.rs`)
- Modify: `src/app/containers.rs:60-600`, `src/backend/mod.rs`, `src/lib.rs`

**Interfaces:**
- Consumes: Task 2.1's types.
- Produces the dispatch functions in `backend/mod.rs`, each a `match backend { BackendId::Docker => Docker.… }`: `pub fn list<E>(backend: BackendId, target: &Target, run: &mut dyn FnMut(&[String]) -> Result<Output, RunFailure<E>>) -> Result<ContainerList, ListFailure<E>>`, `pub fn shell_words(backend: BackendId, id: &str, user: Option<&str>, via: Option<GroupTool>) -> Vec<String>`, `pub fn plain_issue(backend: BackendId, target: &Target, problem: Problem, stderr: String) -> BackendIssue`. `App::container_shell` builds its words with `backend::shell_words(BackendId::Docker, …)`.
- Produces: `pub struct Docker;` implementing `Backend`. `Docker::list` holds today's decision logic from `App::containers` (lines 151-206): run `ps_words()`, classify, probe with `PROBE_SCRIPT`, rerun through `in_group` for `Access::RerunInGroup`, and the issue builders `issue`, `plain_issue`, `daemon_down`, `join_offer`, `no_offer`, `docker_message`, `ask_administrator`, `tool_name`, `listed` (lines 447-600). `classify` returns `Problem`; `DockerProblem` is deleted.
- Produces: Docker's offers as `Offer` values. `join_offer` builds `key: 'g'`, `hint: "add to docker group"`, `prompt: format!("Add {user} to the docker group on {target}? Members of the docker group have root-equivalent access. This runs: {}", remote_command(&words))`, `words: join_group_words(user, socket)`. `daemon_down` (when `has_systemctl`) builds `key: 's'`, `hint: "start docker"`, `prompt: format!("Start the Docker service on {target}? This runs: {}", remote_command(&words))`, `words: start_daemon_words()`.
- `App` keeps `capture`, `run_local`, `local_command`, `local_program`, `direct_words`, `container_target`, `local_failure`, `ContainerPrograms`. `App::containers(target, batch, cancelled)` keeps its signature and becomes: resolve the target, call `backend::list(BackendId::Docker, &target, &mut |words| …)` where the closure calls `self.capture` and maps `ContainerError::App(AppError::OpenSsh(OpenSshError::Cancelled))` to `RunFailure::Cancelled` and every other error to `RunFailure::Failed`; then map `ListFailure::Run(e)` to `e` and `ListFailure::Issue(i)` to `ContainerError::Backend(Box::new(i))`.
- `ContainerError::Docker(Box<DockerIssue>)` becomes `ContainerError::Backend(Box<BackendIssue>)` with the same `#[error("{}", .0.headline)]`. `DockerIssue` and the enum `Offer` are deleted. `local_failure` builds its issue with `backend::plain_issue(BackendId::Docker, …, Problem::NotInstalled, …)`.
- `App::docker_fix(target, offer)` is renamed `App::run_offer(target: &Target, offer: &Offer) -> Result<ExitStatus, ContainerError>` and runs `offer.words`.

- [ ] **Step 1:** Write the failing tests in `src/backend/docker.rs`, using a fake `run` closure that returns scripted `Output`s per call:

```rust
#[test]
fn a_cancelled_probe_stops_the_listing() {
    // call 1: `docker ps` exits 1 with "permission denied while trying to connect to the docker…"
    // call 2 (the probe): Err(RunFailure::Cancelled("cancelled"))
    assert!(matches!(Docker.list(&Target::Local, &mut run), Err(ListFailure::Run("cancelled"))));
}

#[test]
fn a_failed_probe_reports_the_plain_issue() {
    // call 1 as above; call 2: Err(RunFailure::Failed("ssh hiccup"))
    let Err(ListFailure::Issue(issue)) = Docker.list(&Target::Local, &mut run) else { panic!() };
    assert_eq!(issue.problem, Problem::PermissionDenied);
    assert_eq!(issue.offer, None);
}

#[test]
fn join_offer_keeps_its_key_hint_and_prompt() {
    // probe output: user=me, database groups without docker, socket=/var/run/docker.sock,
    // socket_group=docker, sg=yes
    let offer = issue.offer.unwrap();
    assert_eq!((offer.key, offer.hint.as_str()), ('g', "add to docker group"));
    assert_eq!(offer.words, join_group_words("me", "/var/run/docker.sock"));
    assert!(offer.prompt.starts_with(
        "Add me to the docker group on this machine? Members of the docker group have root-equivalent access. This runs: "
    ));
}

#[test]
fn start_offer_keeps_its_key_hint_and_prompt() {
    // `docker ps` stderr "cannot connect to the docker daemon", probe systemctl=yes
    assert_eq!((offer.key, offer.hint.as_str()), ('s', "start docker"));
    assert_eq!(offer.words, start_daemon_words());
    assert!(offer.prompt.starts_with("Start the Docker service on this machine? This runs: "));
}
```

- [ ] **Step 2:** Run `cargo test --lib backend::docker`. Expected: FAIL, `Docker` not found.
- [ ] **Step 3:** Implement the Produces items. Move the App-level tests in `app/containers.rs` (lines 697-960) unchanged: they drive `App` through fake programs, so they now cover the adapter through the transport.
- [ ] **Step 4:** Run fmt, clippy, test. Expected: all pass, including the four new tests.
- [ ] **Step 5:** Commit: `refactor: move Docker behind the backend interface`

### Task 2.3: Interfaces read the generic offer

**Files:**
- Modify: `src/cli.rs:896-1004`, `src/cli/json.rs:118-147,865-880`, `src/tui/containers.rs:250-265,425-440,745-750`, `src/tui.rs:6860-6880`

**Interfaces:**
- Consumes: `BackendIssue`, `Offer`, `App::run_offer` from Task 2.2.
- CLI: `list_with_offers` prints `format!("{} [y/N] ", offer.prompt)`; `offer_prompt` is deleted; `print_docker_issue*` become `print_backend_issue*` (same output).
- JSON: `docker_failure` becomes `backend_failure(issue: &BackendIssue) -> Failure` with `kind: issue.backend.as_str()` and `problem: Some(issue.problem.as_str())`; the "Run {command} in a terminal to be offered this fix." line is unchanged. `Failure.kind` changes from `&'static str` only if needed; `as_str` already returns `&'static str`.
- TUI: in `Panel::handle_key` (`tui/containers.rs:168`), the `KeyCode::Char('g' | 's')` arm becomes `KeyCode::Char(character)` guarded by "the state is `PanelState::Failed` with `Some(offer)` and `offer.key == character`", returning `PanelAction::Fix { target, offer: offer.clone() }`. It stays after every fixed key (`u`, `k`, `j`, `n`, …), so an offer key can never shadow a panel key. The footer hint (line ~431) becomes `format!("{} {}", offer.key, offer.hint)`.

- [ ] **Step 1:** Write the failing test in `src/tui/containers.rs`:

```rust
#[test]
fn a_key_runs_only_the_offer_bound_to_it() {
    // a panel in PanelState::Failed whose issue offers key 'g' (build it as the existing
    // failure tests in this module do)
    assert!(matches!(press(&mut panel, KeyCode::Char('s')), PanelAction::None));
    assert!(matches!(press(&mut panel, KeyCode::Char('g')), PanelAction::Fix { .. }));
}
```

- [ ] **Step 2:** Run `cargo test a_key_runs_only`. Expected: FAIL (does not compile against the generic `Offer` yet).
- [ ] **Step 3:** Implement the Interfaces changes; update the two test constructors (`cli/json.rs:871`, `tui.rs:6865`) to build `BackendIssue` and `Offer`.
- [ ] **Step 4:** Run fmt, clippy, test, e2e, docs. Expected: all pass; `tests/json.rs` and the docs golden check prove the JSON is unchanged.
- [ ] **Step 5:** Commit: `refactor: show backend offers without knowing the backend`, push, open the PR `refactor: put Docker behind a backend interface`.

## Part 3: Domain layer (`refactor/domain-layer`)

### Task 3.1: Create `domain` and move the small modules

**Files:**
- Create: `src/domain/mod.rs`
- Move (`git mv`): `host.rs`, `discovery.rs`, `identity.rs`, `suggest.rs`, `fuzzy.rs`, `explain.rs`, `config_error.rs`, `probe.rs`, `tunnel.rs` → `src/domain/tunnel/mod.rs`, `tunnel/store.rs`, `tunnel/runtime.rs`, `adopt.rs` → `src/domain/adopt/mod.rs`, `adopt/diff.rs`, `adopt/scan.rs`
- Modify: `src/lib.rs`, `src/main.rs:8-16`, `tests/acceptance.rs`, every `use crate::<moved>` path

**Interfaces:**
- Produces: `crate::domain::{host, discovery, identity, suggest, fuzzy, explain, config_error, probe, tunnel, adopt}`, contents unchanged.

- [ ] **Step 1:** Do the moves; declare the modules in `src/domain/mod.rs`; replace `crate::host` with `crate::domain::host` (and so on) across `src/`, plus `portico::…` paths in `src/main.rs` and `tests/acceptance.rs`.
- [ ] **Step 2:** Check the layer rule: `grep -rn "crate::\(app\|cli\|tui\)" src/domain src/backend src/platform`. Expected: no output. Fix any hit by moving the offending item down, never by importing upward.
- [ ] **Step 3:** Run fmt, clippy, test. Expected: all pass.
- [ ] **Step 4:** Commit: `refactor: gather the domain modules under domain`

### Task 3.2: Split the managed configuration

**Files:**
- Create: `src/domain/managed/{mod.rs, config.rs, render.rs, init.rs, store.rs}` (from `git mv src/managed.rs src/domain/managed/mod.rs`)

**Interfaces:**
- `mod.rs`: `HEADER`, `PORTICO_INCLUDE`, `ManagedError`, `HostComments`, `ManagedConfig` (struct), `ConfigPaths`, and `pub use` of everything the submodules export, so `crate::domain::managed::X` paths stay valid.
- `config.rs`: `impl ManagedConfig` (lines 888-1156), `ParsedHost`, `split_directive`, `is_supported_directive`, `set_once`, `parse_value`.
- `render.rs`: `render_host`, `quote`, `quote_path`.
- `init.rs`: `InitPlan`, `InitOutcome`, `InitError`, an `impl ManagedStore` block with `plan_init`, `apply_init`, `apply_init_cancellable`, `apply_init_inner`, `entry_target`, `init_managed`, plus `EntryAnalysis`, `analyze_entry`, `append_include`, `substitute_include`, `line_count`, `is_managed_include`, `init_io`.
- `store.rs`: `ManagedStore` (struct and the remaining methods), `TransactionError`, `interpret_rejection`, `Staged`, `transaction_io`, `transaction_storage_io`, `transaction_entry_error`.
- Fields that `init.rs` and `store.rs` both use become `pub(super)`.

- [ ] **Step 1:** Split as listed. Move each test in the old test module (lines 1775-3366) to the file whose code it exercises; a test that drives both init and a transaction stays in `mod.rs`.
- [ ] **Step 2:** Run fmt, clippy, test. Expected: all pass, same test count.
- [ ] **Step 3:** Commit: `refactor: split the managed configuration by responsibility`

### Task 3.3: Split OpenSSH access

**Files:**
- Create: `src/domain/openssh/{mod.rs, config.rs, keys.rs, connect.rs, pipes.rs}` (from `git mv src/openssh.rs src/domain/openssh/mod.rs`)

**Interfaces:**
- `mod.rs`: `OpenSsh` (struct, `system`, `new`, `with_*`, program getters, `ssh_program`), `OpenSshError`, `ConnectionOutcome`, `run_remote`, `validate_alias`, `checked_output`, `cancellable_output`, and `pub use` of the submodules' exports.
- `config.rs`: `ResolvedConfig`, `default_identity_files`, `validate*`, `effective`, `resolve*`.
- `keys.rs`: `KeyFingerprint`, `PresentedKey`, `KeyFetch`, `fetch_host_key`, `parse_fingerprint`, `append_known_hosts`.
- `connect.rs`: `Watched`, `probe`, `probe_with_deadline`, `watch_ssh`, `launch_in_new_terminal`, `launch_new_terminal_with`, `Mirrored`, `CONNECT_TAIL_LIMIT`, `MIRROR_DRAIN`, `connect_mirrored`, `connect_mirrored_command`, `Copier`.
- `pipes.rs`: `push_capped`, `read_lines`, `read_pipe`, `finish_pipe`, `finish_pipe_cancellable`.

- [ ] **Step 1:** Split as listed; methods go in `impl OpenSsh` blocks in their submodule. Move tests (lines 1312-2517) with the code they exercise.
- [ ] **Step 2:** Run fmt, clippy, test, e2e, docs. Expected: all pass.
- [ ] **Step 3:** Commit: `refactor: split OpenSSH access by responsibility`, push, open the PR `refactor: gather hosts, the managed file and OpenSSH access into a domain layer`.

## Part 4: CLI modules (`refactor/cli-modules`)

### Task 4.1: One module per command group

**Files:**
- Create: `src/cli/mod.rs` (from `git mv src/cli.rs`), `src/cli/hosts.rs`, `src/cli/containers.rs`, `src/cli/migrate.rs`
- Move: `src/picker.rs` → `src/cli/picker.rs`
- Modify: `src/main.rs` (`portico::cli::picker::TerminalPicker`), `src/lib.rs`

**Interfaces:**
- `mod.rs`: `Cli`, `Command`, `run`, `fail`, `run_desktop`, `json_command`, `execute` (its arms call into the submodules), `exit_code`, `Interrupts`, `Step`, `Via`, `confirm`.
- `hosts.rs`: `test_command`, `finish_connection`, `host_argument`, `print_untrusted_key`, `print_cancelled`, `print_connection_failure`, `print_report`, `field_change`, `print_init_plan`, `print_details`, `print_identities`, `print_inventory`, `print_optional`, `ownership`, `format_warning`.
- `containers.rs`: `container_rows`, `on_argument`, `list_with_offers`, `print_backend_issue`, `print_backend_issue_next`, `report_container_failure`, `print_listing`.
- `migrate.rs`: `Migration`, `run_migration`, `describe_source`, `capitalized`.
- Existing `cli/desktop.rs`, `cli/json.rs`, `cli/tunnels.rs` stay. Items become `pub(super)` where a sibling calls them.

- [ ] **Step 1:** Split as listed; move the tests (lines 1587-2894) with the code they exercise; tests that call `run` end to end stay in `mod.rs`.
- [ ] **Step 2:** Run fmt, clippy, test, e2e, docs. Expected: all pass; `tests/cli.rs` proves the output is unchanged.
- [ ] **Step 3:** Commit: `refactor: split the CLI into one module per command group`, push, open the PR with the same title.

## Part 5: TUI modules (`refactor/tui-modules`)

### Task 5.1: Terminal session and shared widgets

**Files:**
- Create: `src/tui/mod.rs` (from `git mv src/tui.rs`), `src/tui/terminal.rs`, `src/tui/text_input.rs`, `src/tui/form.rs`

**Interfaces:**
- `terminal.rs`: `redraw_from_scratch`, `suspended`, `Signals`, `ForegroundChild`, `CrosstermControl`, `TerminalSession`, `TerminalGuard`, `PanicHook`, `suspend_for_child`.
- `text_input.rs`: `TextInput` and its `impl` (lines 1245-1377), still `pub(crate)`.
- `form.rs`: `FormField`, `FormState` and its impls, `FormTestResult`, `IDENTITY_FILE`, `PROXY_JUMP`, `optional`, `set_changed`, `set_optional_changed`, `form_lines`, and an `impl UiState` block with `handle_form_key`, `form_input_changed`, `suggest`, `pick_suggestion`, `submit_form`, `test_form`, `form_has_foldable_raw`, `report_form_error`.
- `UiState` fields used across files become `pub(super)`.

- [ ] **Step 1:** Split as listed; move tests with the code they exercise.
- [ ] **Step 2:** Run fmt, clippy, test. Expected: all pass.
- [ ] **Step 3:** Commit: `refactor: move the TUI's terminal session, text input and host form into their own modules`

### Task 5.2: One module per tab

**Files:**
- Create: `src/tui/hosts.rs`, `src/tui/init.rs`
- Modify: `src/tui/mod.rs`, `src/tui/containers.rs`, `src/tui/migrate.rs`

**Interfaces:**
- `hosts.rs`: `DetailsCache`, `TestMark`, `PerHost`, `PendingTrust`, and an `impl UiState` block with `set_filter`, `refilter`, `select_alias`, `selected_*`, `replace_hosts`, `handle_hosts_key`, `handle_paste`, `handle_browse_key`, `begin_edit`, `open_pending_edit`, `handle_filter_key`, `test_selected`, `selected_has_foldable_raw`, `finish_connect`, `record_test`, `can_prompt`, `show_result`, `handle_trust_key`, `handle_remove_key`, `finish_identities`, `move_selection`, `selected_is_managed`, `connect_action`; plus `details_for_selected`, `render_hosts`, `trust_lines`, `details_text`, `explanation_lines`, `has_foldable_raw`, `DetailRow`, `resolved_rows`, `declared_rows`, `detail_text`, `push_rows`, `declared_in_effect`, `format_warning`.
- `init.rs`: `confirm_initialization`, `render_init`, `ownership`, `init_decision`.
- `containers.rs` gains `render_containers`; `migrate.rs` gains `handle_migrate_key` and `finish_plan`.
- `mod.rs` keeps `run`, `TuiError`, `browse`, `browse_hosts`, `on_tick`, `run_tunnel_action`, the mutation and reload functions, `Workers`, `Resolver` and its worker, `Mode`, `Tab`, `UiState` (struct, `new`, `handle_key`, `browsing`, `handle_tab_key`, `in_form`, `status_line`, `report_error`), `UiAction`, `render`, `tab_bar`, `tab_areas`, `footer_text`, `local_time`.

- [ ] **Step 1:** Split as listed. Move each test in the old test module (lines 3343-6914) to the tab module it exercises; tests that switch tabs or drive `run` stay in `mod.rs`.
- [ ] **Step 2:** Run fmt, clippy, test, e2e, docs. Expected: all pass; the `tui_*` end-to-end tests prove the screens are unchanged.
- [ ] **Step 3:** Commit: `refactor: give each TUI tab its own module`, push, open the PR `refactor: split the TUI into one module per tab`.

## Part 6: Architecture document (`docs/architecture`)

### Task 6.1: Record what was built

**Files:**
- Create: `docs/architecture.md`
- Modify: `CONTRIBUTING.md`, `tests/docs.rs:8-20`, `ROADMAP.md` (v4 section), `docs/superpowers/specs/2026-11-24-modular-architecture-design.md` (status line)

**Interfaces:**
- `docs/architecture.md` holds: the layer diagram and the dependency rule (copied from the spec), the module map as actually built (one line per module), and how to add a backend (the adapter file, one `BackendId` variant, one arm per dispatch function) and a platform (a sibling of `src/platform/linux/`). No file-size rule.

- [ ] **Step 1:** Write the failing tests in `tests/docs.rs`: add `"docs/architecture.md"` to `required_project_documents_exist`, and

```rust
#[test]
fn contributing_links_the_architecture_document() {
    assert!(read("CONTRIBUTING.md").contains("docs/architecture.md"));
}
```

- [ ] **Step 2:** Run `./scripts/ci-checks.sh docs`. Expected: FAIL on both.
- [ ] **Step 3:** Write `docs/architecture.md`; link it from `CONTRIBUTING.md` next to the line about `docs/superpowers/`.
- [ ] **Step 4:** Update `ROADMAP.md`: remove the shipped "Modular architecture" slice, renumber the remaining slices, and replace "the backend interface from slice 1" (Proxmox, Podman, v6, external adapters) with "the backend interface (`docs/architecture.md`)". Set the spec's status to `Approved; implemented`. Keep `roadmap_keeps_every_pending_follow_up_slice_from_the_v1_spec` green.
- [ ] **Step 5:** Run `./scripts/ci-checks.sh docs`. Expected: all pass.
- [ ] **Step 6:** Commit: `docs: describe the architecture and mark the modular architecture slice shipped`, push, open the PR with the same title.
