# macOS and Windows Desktop Interfaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A menu bar app (macOS) and tray app (Windows), one Rust program `portico-tray` that consumes Portico's CLI, installed, updated and removed by `portico desktop install | uninstall | status` on those systems.

**Architecture:** `desktop/tray/` is its own Cargo project (not a member of `portico-ssh`): a pure `model` (JSON documents to a menu model) tested on Linux, a `portico` runner, and a GUI layer (`tray-icon` + `muda`) compiled only on macOS and Windows. In Portico, `src/desktop/tray.rs` holds the pure plan (asset, digest, login entry, layout checks) and a `TrayHost` trait for the side effects; `src/cli/tray.rs` is the install/uninstall/status flow, dispatched from `src/cli/desktop.rs` outside Linux.

**Tech Stack:** Rust 2024; `tray-icon`, `muda` (and an event loop chosen by the builder), `serde_json`; in Portico, `sha2` for SHA-256; `curl` and `tar` as external tools; `launchctl` on macOS, the registry through `windows-sys` on Windows.

**Spec:** `docs/superpowers/specs/2026-11-26-desktop-interfaces-design.md`

## Global Constraints

- No CLI or JSON change: the app reads only what `docs/json-output.md` documents under `"schema": 1`.
- No daemon, no database, no plugin system inside Portico; nothing in the app runs on a timer.
- Every process is run as an argument list, never through a shell; on Windows every child of the app gets `CREATE_NO_WINDOW`.
- The app never reads OpenSSH files, `settings.toml` or tunnel records, and never runs `ssh`, `docker`, `podman` or `pct`.
- LaunchAgent label `io.github.younesamalou.portico.tray`; registry value `Portico Tray` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
- App locations: `~/Applications/Portico Tray.app` (macOS); `%LOCALAPPDATA%\Programs\Portico\portico-tray.exe` (Windows).
- Asset: `https://github.com/YounesAmalou/Portico/releases/download/v<version>/portico-tray-v<version>-<target>.<tar.gz|zip>` (`tar.gz` on macOS, `zip` on Windows).
- curl: `curl --fail --location --proto =https --tlsv1.2 --silent --show-error -o <file> <url>`.
- Baked digest: `option_env!("PORTICO_TRAY_SHA256")`; app version: `option_env!("PORTICO_TRAY_VERSION")`, else `dev`; `desktop/tray/Cargo.toml` stays at `0.0.0`.
- Self-update log: `tray-update.log` in Portico's runtime directory.
- Footprint: CI fails when the tray archive is larger than the `portico` archive of the same target.
- Commits follow Conventional Commits, with no AI attribution of any kind. Tests run as a regular user. `scripts/ci-checks.sh` passes before each commit.

## Review Focus

1. **A `portico` path with spaces** (`C:\Users\Jane Doe\...`, `~/Applications/Portico Tray.app`): the Run value and plist must quote/escape correctly. Tests in Task 4 (`run_value_quotes_paths_with_spaces`, `plist_escapes_xml`).
2. **A malicious or broken archive** (an entry with `..`, an absolute path, extra files, a symlink): the unpacked-layout check refuses it before anything moves into place. Tests in Task 4 (`layout_rejects_*`).
3. **Homebrew upgrade removes the Cellar path**: the recorded path is the stable `PATH` link; the app falls back to the well-known locations. Tests in Task 4 (`stable_portico_prefers_path_link`) and Task 2 (`locate_falls_back_when_recorded_path_is_gone`).
4. **A tunnels document with an unknown `status.state` or extra entries** (the parallel v6 slice): the app shows the raw state and no action, and never panics. Test in Task 1 (`unknown_tunnel_state_has_no_action`).
5. **A second `portico-tray` started while one runs** (login item plus a manual start): it exits at once. Covered by Task 3's single-instance guard; the lock is tested on Linux in Task 2 (`second_lock_is_refused`).

---

### Task 1: Tray crate and its pure menu model

**Files:**
- Create: `desktop/tray/Cargo.toml` (package `portico-tray`, version `0.0.0`, `publish = false`, edition 2024, empty `[workspace]`), `desktop/tray/Cargo.lock`, `desktop/tray/src/main.rs` (Linux stub only, for now), `desktop/tray/src/lib.rs`, `desktop/tray/src/model.rs`, `desktop/tray/tests/model.rs`
- Modify: `Cargo.toml` (`exclude` gains `"desktop/tray/"`), `scripts/ci-checks.sh` (new `tray` subcommand and usage line), `.gitignore` if needed (`desktop/tray/target/`)

**Interfaces:**
- Produces (in `portico_tray::model`, all pure, no GUI types):
  - `pub enum Parsed { Doc(serde_json::Value), Problem(String) }` and `pub fn parse(stdout: &str, exit: Option<i32>) -> Parsed`: the panel's `portico.parse` rules (not JSON or no `schema`, or exit 2: "This Portico does not print the JSON this app reads; it needs a Portico release with `--json`."; schema N≠1: "This Portico prints schema N; this app reads schema 1. Update the tray app" for N>1, "Update Portico" for N<1).
  - `pub fn error_of(doc: &Value) -> Option<(String, Vec<String>)>`: `error.message` and `error.next`.
  - `pub struct Tabs { pub tunnels: bool, pub docker: bool, pub podman: bool, pub proxmox: bool }`, `pub fn tabs(doc: &Value) -> Tabs` (all `true` when a key is missing), `pub fn enabled_backends(t: &Tabs) -> Vec<&'static str>` in the order docker, podman, proxmox.
  - `pub struct Tunnel { pub name: String, pub label: String, pub state: TunnelState, pub warnings: Vec<String> }`, `pub enum TunnelState { Running, Stopped, Failed(String), PortInUse { address: String, by: Option<String> }, Unknown(String) }`, `pub enum Action { Start, Stop, None }`, `impl Tunnel { pub fn action(&self) -> Action }`, `pub fn tunnels(doc: &Value) -> Vec<Tunnel>`.
  - `pub struct Host { pub alias: String, pub external: bool }`, `pub fn hosts(doc: &Value) -> (Vec<Host>, Vec<String>)` (first of a duplicate alias; warnings' `message`).
  - `pub struct Container { pub id: String, pub backend: String, pub label: String }` (label: `"<Backend>: <display_name> · <image> · <status>"`), `pub fn containers(doc: &Value) -> Result<Vec<Container>, (String, Vec<String>)>`.
  - `pub fn containers_args(alias: Option<&str>, backends: &[&str]) -> Option<Vec<String>>` (`None` when no backend), `pub fn connect_args(alias: &str) -> Vec<String>`, `pub fn shell_args(alias: Option<&str>, backend: &str, id: &str) -> Vec<String>`, `pub fn tunnel_args(action: Action, name: &str) -> Vec<String>`: the exact argument lists of the spec (`--on=<alias>`, `--backend=<b>`, `--` before positional values).
  - `pub fn launch_failure(exit: Option<i32>, stderr: &str) -> String` and `pub fn version_of(stdout: &str) -> Option<String>`.
  - `pub fn title(text: &str) -> String`: truncates to 60 characters with `…`.

- [ ] **Step 1: Write the failing tests** in `desktop/tray/tests/model.rs`, reading the golden files from `../../tests/fixtures/json/` with `include_str!`:
  - `parses_hosts_golden` (aliases and `external` of `hosts.json`, its warnings), `duplicate_alias_shown_once`;
  - `parses_every_tunnel_state` (each state in `tunnels.json`: Running → `Action::Stop`, Stopped/Failed/PortInUse → `Action::Start`), `unknown_tunnel_state_has_no_action` (a copy of a tunnel with `"state": "teleporting"` → `Unknown("teleporting")`, `Action::None`);
  - `parses_containers_per_backend` (`containers.json`, `containers-podman.json`, `containers-proxmox.json`), `container_errors_have_message_and_next` (`containers-docker-down.json`, `containers-proxmox-denied.json`);
  - `settings_tabs_and_backends` (`settings.json` → tunnels on, podman off, backends `["docker","proxmox"]`), `missing_tabs_default_on`;
  - `schema_problems` (not JSON, `{}`, `{"schema":2}`, `{"schema":0}`, exit 2 with empty stdout → the exact messages above), `error_document_is_scoped` (`error-unknown-host.json`);
  - `argument_lists` (`connect_args("-dash") == ["connect","--new-terminal","--","-dash"]`; `shell_args(Some("api"),"podman","abc") == ["shell","--new-terminal","--on=api","--backend=podman","--","abc"]`; `shell_args(None,..)` has no `--on`; `containers_args(Some("api"),&["docker","proxmox"]) == ["containers","--json","--on=api","--backend=docker","--backend=proxmox"]`; `containers_args(_, &[]) == None`; `tunnel_args(Stop,"db") == ["tunnel","stop","--json","--","db"]`);
  - `launch_failure_text` (last non-empty stderr line, else "Portico could not open a terminal (exit status N)"), `version_parsing` (`"portico 0.7.0\n"` → `0.7.0`), `title_truncates`.
- [ ] **Step 2: Run** `cd desktop/tray && cargo test` — expected: compile failure (no `model`).
- [ ] **Step 3: Implement** `model.rs` to the interfaces above; `main.rs` on Linux prints `portico-tray runs on macOS and Windows; on Linux use the Quickshell panel (portico desktop install)` to stderr and exits 1.
- [ ] **Step 4: Run** `cd desktop/tray && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check` — expected: all pass.
- [ ] **Step 5: Add `tray()` to `scripts/ci-checks.sh`** running fmt, clippy (`-D warnings`) and test in `desktop/tray`; run `scripts/ci-checks.sh tray` and `scripts/ci-checks.sh package` (the crate must not include `desktop/tray/`). Expected: pass.
- [ ] **Step 6: Commit** `feat: add the tray app's menu model, read from Portico's JSON`

### Task 2: The tray app's process runner and single-instance lock

**Files:**
- Create: `desktop/tray/src/portico.rs`, `desktop/tray/src/instance.rs`, `desktop/tray/tests/runner.rs`
- Modify: `desktop/tray/src/lib.rs`

**Interfaces:**
- Consumes: `model::*` argument builders.
- Produces:
  - `pub fn locate(recorded: Option<&Path>, path_var: Option<&OsStr>, home: &Path) -> Option<PathBuf>`: the recorded path if it is an executable file, else `portico` on `PATH`, else the well-known list (macOS: `/opt/homebrew/bin`, `/usr/local/bin`, `~/.cargo/bin`; Windows: `%LOCALAPPDATA%\Microsoft\WinGet\Links`, `%USERPROFILE%\.cargo\bin`), each tried in order; on Linux (tests) the macOS list.
  - `pub struct Output { pub stdout: String, pub stderr: String, pub exit: Option<i32> }`, `pub fn run(program: &Path, args: &[String]) -> std::io::Result<Output>` (stdin null; `CREATE_NO_WINDOW` on Windows).
  - `pub struct Worker` with `pub fn spawn(program: PathBuf, sender: std::sync::mpsc::Sender<(Request, std::io::Result<Output>)>) -> Worker` and `pub fn send(&self, request: Request)`: runs requests one at a time per kind on background threads, never blocking the caller. `pub enum Request { Version, Settings, Hosts, Tunnels, Containers(Option<String>, Vec<String>), Tunnel(model::Action, String), Connect(String), Shell(Option<String>, String, String) }`.
  - `pub fn start_update(program: &Path, log: &Path) -> std::io::Result<()>`: starts `portico desktop install --yes --tray-only` detached (`setsid` via `libc::setsid` in `pre_exec` on macOS; `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` on Windows), stdout and stderr appended to `log`.
  - `pub fn update_failure(log: &Path, app_modified: SystemTime) -> Option<String>`: the log's last non-empty line when the log is newer than the app and that line starts with `error:`.
  - `pub struct Instance` and `pub fn acquire(dir: &Path) -> std::io::Result<Option<Instance>>`: an exclusive `flock`-style lock on `dir/tray.lock` (Unix), a named mutex `Local\PorticoTray` on Windows; `None` when another holds it.

- [ ] **Step 1: Write failing tests** (Unix, using a fake `portico` shell script in a temp dir): `locate_prefers_recorded`, `locate_falls_back_when_recorded_path_is_gone` (recorded path deleted, a fake in a `PATH` dir is found), `locate_well_known_after_path` (empty `PATH`, fake in `<home>/.cargo/bin`), `run_reports_exit_and_streams`, `worker_answers_requests_in_background` (receives `(Request::Hosts, Ok(..))` with the fake's stdout), `update_failure_reads_last_error_line` (log with `error: download failed` newer than the app → `Some`; older → `None`; success line → `None`), `second_lock_is_refused`.
- [ ] **Step 2: Run** `cd desktop/tray && cargo test --test runner` — expected: FAIL (missing items).
- [ ] **Step 3: Implement** `portico.rs` and `instance.rs` to the interfaces.
- [ ] **Step 4: Run** `scripts/ci-checks.sh tray` — expected: pass.
- [ ] **Step 5: Commit** `feat: run portico from the tray app, off the menu's thread`

### Task 3: The tray app's menus on macOS and Windows

**Files:**
- Create: `desktop/tray/src/menu.rs`, `desktop/tray/src/app.rs` (both `#[cfg(any(target_os = "macos", windows))]`), `desktop/tray/assets/` (template PNG for the macOS menu bar, `.ico` and `.icns` derived from `packaging/desktop/portico.svg`), `desktop/tray/macos/Info.plist` (`LSUIElement` true, `CFBundleIdentifier` `io.github.younesamalou.portico.tray`, `CFBundleExecutable` `portico-tray`, `CFBundleShortVersionString` `@VERSION@`), `desktop/tray/build.rs` (Windows icon and manifest), `desktop/tray/README.md`
- Modify: `desktop/tray/src/main.rs`, `desktop/tray/Cargo.toml` (target-specific `tray-icon`, `muda`, event-loop and resource dependencies)

**Interfaces:**
- Consumes: `model::*`, `portico::{locate, Worker, Request, start_update, update_failure}`, `instance::acquire`.
- Produces: the `portico-tray` binary. Arguments: `--portico <path>`, `--version` (prints `portico-tray <version>`). `menu.rs`: `pub struct State` (the last of each document, container listings per target, busy tunnels, row errors, problem, versions) and `pub fn build(state: &State) -> (muda::Menu, HashMap<muda::MenuId, Click>)` with `pub enum Click { Tunnel(Action, String), Connect(String), ShowContainers(Option<String>), Shell(Option<String>, String, String), Refresh, Update, Quit }`.

- [ ] **Step 1: Implement `menu::build`** with the layout of the spec ("What the menu shows"): Tunnels (only when `tabs.tunnels`), Hosts with "Open shell" and "Show containers"/"Refresh containers" (left out with no container tab), "This machine", the version banner (none when the app is `dev`: a disabled "Development build"), "Update failed: …" from `update_failure`, Refresh, Quit; disabled lines for errors and busy tunnels ("Starting…", "Stopping…"). All titles through `model::title`.
- [ ] **Step 2: Implement `app.rs`'s event loop**: acquire the instance lock (exit 0 when taken), locate `portico`, send `Version, Settings, Hosts, Tunnels` at start and on Refresh, `Tunnels` on every tray-icon click that opens the menu; rebuild the menu when a worker result arrives; on `Update`, `start_update` then quit. No timers.
- [ ] **Step 3: Lint for Windows from Linux**: `cd desktop/tray && cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings` — expected: pass (it does not link). macOS is checked by CI (Task 7).
- [ ] **Step 4: Run** `scripts/ci-checks.sh tray` — expected: pass (Linux build still the stub).
- [ ] **Step 5: Commit** `feat: show Portico's hosts, containers and tunnels in a macOS menu bar and Windows tray menu`

### Task 4: Portico's tray plan (pure)

**Files:**
- Create: `src/desktop/tray.rs`
- Modify: `src/desktop.rs` (`pub mod tray;`), `Cargo.toml` (`sha2`), `deny.toml` only if `sha2` needs it

**Interfaces:**
- Produces (in `portico::desktop::tray`, compiled on every system):
  - `pub enum System { MacOs, Windows }` with `pub fn current() -> Option<System>` (`None` on Linux), `pub fn app_name(self) -> &'static str` ("Menu bar app" / "Tray app").
  - `pub const LABEL: &str = "io.github.younesamalou.portico.tray"; pub const RUN_VALUE: &str = "Portico Tray"; pub const UPDATE_LOG: &str = "tray-update.log";`
  - `pub fn target() -> Option<&'static str>`: the four targets from `cfg(target_os, target_arch)`, `None` elsewhere.
  - `pub fn asset_name(system: System, version: &str, target: &str) -> String`, `pub fn asset_url(version: &str, name: &str) -> String`, `pub fn sums_url(version: &str) -> String`.
  - `pub enum Expected { Baked(String), Sums }`, `pub fn expected_digest() -> Expected` (from `option_env!("PORTICO_TRAY_SHA256")`, lowercased; empty means `Sums`), `pub fn sums_digest(sums: &str, name: &str) -> Option<String>` (the `<hex>  <name>` and `<hex> *<name>` forms), `pub fn sha256_file(path: &Path) -> io::Result<String>`.
  - `pub struct Paths { pub app: PathBuf, pub binary: PathBuf, pub launch_agent: Option<PathBuf> }`, `pub fn paths(system: System, home: &Path, local_app_data: Option<&Path>) -> Paths`.
  - `pub fn launch_agent_plist(binary: &Path, portico: &Path) -> String` (label, `ProgramArguments` [binary, `--portico`, portico], `RunAtLoad` true, `AbandonProcessGroup` true, `LimitLoadToSessionType` `Aqua`; XML-escaped).
  - `pub fn run_value(binary: &Path, portico: &Path) -> String`: `"<binary>" --portico "<portico>"`.
  - `pub fn check_layout(dir: &Path, system: System) -> Result<PathBuf, String>`: exactly one top-level entry, `Portico Tray.app` (a directory containing `Contents/MacOS/portico-tray`) or `portico-tray.exe` (a file); no symlink anywhere in it; returns that entry's path.
  - `pub fn stable_portico(exe: &Path, path_var: Option<&OsStr>) -> PathBuf`: the first `portico`/`portico.exe` on `path_var` whose canonical path equals `exe`'s canonical path, else `exe`.
  - `pub fn plist_version(info_plist: &str) -> Option<String>`: `CFBundleShortVersionString` from a bundle's `Info.plist` text (macOS's `TrayHost::app_version` uses it; Windows runs `portico-tray.exe --version`).

- [ ] **Step 1: Write failing unit tests** in `src/desktop/tray.rs`: `asset_names_per_system` (`portico-tray-v0.8.0-aarch64-apple-darwin.tar.gz`, `portico-tray-v0.8.0-x86_64-pc-windows-msvc.zip`), `asset_url_points_at_the_release`, `sums_digest_both_forms`, `sums_digest_missing_line`, `sha256_known_vector` (`"abc"` → `ba7816bf…15ad`), `plist_has_label_arguments_and_abandon`, `plist_escapes_xml` (a path with `&` and `<`), `run_value_quotes_paths_with_spaces`, `layout_accepts_app_bundle`, `layout_accepts_exe`, `layout_rejects_extra_entry`, `layout_rejects_symlink`, `layout_rejects_missing_binary`, `stable_portico_prefers_path_link` (a symlink in a `PATH` dir to the exe wins over the exe's own path), `stable_portico_falls_back_to_exe`, `paths_per_system`, `plist_version_reads_short_version`.
- [ ] **Step 2: Run** `cargo test desktop::tray` — expected: FAIL.
- [ ] **Step 3: Implement** to the interfaces.
- [ ] **Step 4: Run** `cargo test desktop::tray && scripts/ci-checks.sh clippy && scripts/ci-checks.sh deny` — expected: pass.
- [ ] **Step 5: Commit** `feat: plan the tray app's download, digest check and login entry`

### Task 5: `portico desktop install | uninstall | status` on macOS and Windows

**Files:**
- Create: `src/cli/tray.rs`, `src/desktop/tray/host.rs` (or a `host` submodule: the real `TrayHost` per system)
- Modify: `src/cli/desktop.rs` (flags, dispatch), `src/cli/mod.rs` (no longer hidden outside Linux), `src/main.rs` (no longer refuses `desktop` outside Linux), `src/desktop/status.rs` (`status` dispatches to the tray lines outside Linux), `src/tui/settings.rs` and its caller (the overlay offers the desktop section on every system), `tests/cli.rs` or a new `tests/desktop_tray.rs`

**Interfaces:**
- Consumes: Task 4's `tray::*`; `platform::process::{detach, stop_process, Proc}`; `DesktopEnv`.
- Produces:
  - `pub trait TrayHost { fn download(&self, url: &str, to: &Path) -> Result<(), String>; fn unpack(&self, archive: &Path, into: &Path) -> Result<(), String>; fn app_version(&self, app: &Path) -> Option<String>; fn login_entry(&self) -> Option<String>; fn set_login_entry(&self, binary: &Path, portico: &Path) -> Result<(), String>; fn remove_login_entry(&self) -> Result<(), String>; fn running(&self) -> Vec<u32>; fn stop(&self, during_update: bool) -> Result<(), String>; fn start(&self, binary: &Path, portico: &Path, via_login: bool) -> Result<(), String>; }` in `portico::desktop::tray`, with `pub struct SystemHost` implementing it on macOS (curl, `tar -xzf`, `launchctl bootstrap|bootout|kickstart`, `xattr -d com.apple.quarantine`) and Windows (curl.exe, `tar.exe -xf`, the registry through `windows-sys`, `DETACHED_PROCESS` start).
  - `src/cli/tray.rs`: `pub struct TrayOptions { pub from: Option<PathBuf>, pub login: bool, pub yes: bool, pub tray_only: bool }`, `pub fn install(env: &DesktopEnv, system: System, host: &dyn TrayHost, options: &TrayOptions, input, output, interactive) -> Result<ExitCode, String>`, `pub fn uninstall(env, system, host, yes, input, output, interactive) -> Result<ExitCode, String>`, `pub fn status_lines(env, system, host) -> desktop::status::Status`.
  - `DesktopCommand::Install` gains `--no-login`, `--from <PATH>`, hidden `--tray-only`; Linux-only flags are refused outside Linux with "only on Linux", and the new flags on Linux with "only on macOS and Windows".

- [ ] **Step 1: Write failing tests** for `src/cli/tray.rs` that run on Linux with a `FakeHost` (records calls; serves a fixture archive built in the test from a fake `Portico Tray.app` or `portico-tray.exe`), for both `System::MacOs` and `System::Windows`:
  - `install_downloads_checks_and_installs` (preview lines as in the spec's "Preview and questions" block; answers `y`, `y`; app in place, login entry set to `run_value`/plist with the stable portico path, started);
  - `install_refuses_digest_mismatch` (nothing moved, temp removed, message names both digests);
  - `install_without_baked_digest_uses_sums` and `install_reports_missing_release` (download of the asset fails with 404 → message naming `--from`);
  - `install_from_archive_skips_download` (preview says the digest is not checked);
  - `installed_current_is_not_asked`, `older_app_is_offered_as_update`;
  - `tray_only_keeps_login_entry_and_restarts` (`stop(true)`, no `remove_login_entry`, `start(.., via_login = entry exists)`), `tray_only_failure_restarts_old_app` (unpack fails → the old app is started, the error is returned);
  - `uninstall_removes_login_stops_and_deletes`, `stopping_at_a_question_changes_nothing`;
  - `status_lines_report_app_login_and_running` (installed version and "out of date", login entry pointing at a missing path → "points at <path>, which no longer exists").
- [ ] **Step 2: Run** `cargo test cli::tray` — expected: FAIL.
- [ ] **Step 3: Implement** `src/cli/tray.rs`, the dispatch in `src/cli/desktop.rs` (outside Linux: `tray::install|uninstall|status` with `System::current()` and `SystemHost`), `main.rs` and `cli/mod.rs` changes, the status dispatch, and the overlay on every system. Keep the `main` panel refresh Linux-only.
- [ ] **Step 4: Implement `SystemHost`** for macOS and Windows; check Windows from Linux: `cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings`.
- [ ] **Step 5: Add CLI tests on macOS and Windows** (`#![cfg(any(target_os = "macos", windows))]`): a fake `curl` and `tar` on `PATH` (using `tests/support/executable.rs`, as other tests fake `ssh` and `docker`): run `portico desktop install --yes --no-login --from <fixture>` and `portico desktop status` and `portico desktop uninstall --yes` in a temporary `HOME` (`LOCALAPPDATA` on Windows), asserting the files and the output. Starting the app is behind an environment seam the test sets (`PORTICO_TRAY_NO_START=1`, read only in debug builds, as the existing test hooks are).
- [ ] **Step 6: Run** `scripts/ci-checks.sh fmt && scripts/ci-checks.sh clippy && scripts/ci-checks.sh test` — expected: pass on Linux (Linux desktop tests unchanged).
- [ ] **Step 7: Commit** `feat: install, update and remove the tray app with portico desktop on macOS and Windows`

### Task 6: Build, CI and release

**Files:**
- Create: `scripts/build-tray.sh` (`<version> <target> <outdir>`: builds `desktop/tray` with `PORTICO_TRAY_VERSION=<version>` and `--locked --release`, packages `portico-tray-v<version>-<target>.tar.gz` (the bundle from `macos/Info.plist` with `@VERSION@` filled in, `codesign -s - --force --deep`) or `.zip` (with `tar.exe --format zip`, as `package-release.sh` does), prints the archive path)
- Modify: `.github/workflows/ci.yml` (`changes` gains a `tray` output for `desktop/tray/*`; job `tray` on Linux running `scripts/ci-checks.sh tray`; `macos (tray)` and `windows (tray)`: clippy, `scripts/build-tray.sh 0.0.0 <host target> dist`, and the footprint check comparing it with `scripts/build-release.sh`'s archive size — or a fixed 4 MB cap when building portico is not part of that job, builder's call; `audit` and `deny` also run in `desktop/tray`), `.github/workflows/release.yml` (`build-macos`/`build-windows`: build the tray first, `PORTICO_TRAY_SHA256=$(sha256 of the archive)` exported to the portico build of the same target, attest the tray archive; `checksums` includes them; rehearsals too), `.github/dependabot.yml` (cargo, `/desktop/tray`), `scripts/ci-checks.sh` (`audit` and `deny` cover `desktop/tray`), `tests/release_scripts.rs` if it checks scripts' usage lines

- [ ] **Step 1: Write** `scripts/build-tray.sh`; run on Linux `scripts/build-tray.sh 0.0.0 x86_64-unknown-linux-gnu /tmp/x` — expected: refuses with "macOS or Windows target required" and exit 2.
- [ ] **Step 2: Edit the workflows**; validate with `actionlint` if installed, else by reading. Pin any new action to a commit SHA with its version comment.
- [ ] **Step 3: Run** `cargo test --test release_scripts` and `cargo test --test docs` — expected: pass.
- [ ] **Step 4: Commit** `ci: build, check and release the tray app for macOS and Windows`

### Task 7: Documentation and roadmap

**Files:**
- Modify: `README.md` ("Desktop panel" becomes "Desktop integration", with Linux, macOS and Windows subsections: what `portico desktop install` downloads and where it goes, removing it, Gatekeeper and SmartScreen notes), `docs/architecture.md` (the `desktop` row: no longer Linux only; the tray module), `docs/json-output.md` (the tray app named as a consumer), `CONTRIBUTING.md` (the tray CI jobs, `scripts/ci-checks.sh tray`, the release's tray artifacts), `ROADMAP.md` (remove v5 and its item; renumber nothing else unless the guards require it), `docs/superpowers/specs/2026-09-13-portico-host-management-design.md` ("Delivered follow-up slices" gains this slice), the spec's status line ("Approved; implemented")

- [ ] **Step 1: Edit the documents.**
- [ ] **Step 2: Run** `cargo test --test docs` — expected: pass (the doc guards).
- [ ] **Step 3: Run the full suite as a regular user:** `scripts/ci-checks.sh fmt`, `clippy`, `test`, `e2e`, `package`, `tray`, `deny` — expected: pass.
- [ ] **Step 4: Commit** `docs: document the macOS and Windows desktop interfaces`
