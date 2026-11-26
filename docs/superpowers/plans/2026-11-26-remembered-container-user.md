# Remembered Container User Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The TUI remembers the user chosen for each container's shell in `settings.toml`, and `portico settings` lists and forgets those users.

**Architecture:** `domain::settings` gains a `users` map read from `[users.local.<backend>]` and `[users.host.<alias>.<backend>]` tables, and a `remember_user` writer that shares `set`'s locked read-modify-write path. `backend::listing::remember_key` names a container (`project/service` or its name). `App::remember_container_user` is called by the TUI when a shell opens and by `portico settings forget`. The TUI's user line becomes per container.

**Tech Stack:** Rust, `toml_edit`, `clap`, `ratatui`, `serde_json`; Python PTY harness for the end-to-end test.

**Spec:** `docs/superpowers/specs/2026-11-26-remembered-container-user-design.md`

## Global Constraints

- Do not touch `src/platform/` or `.github/workflows/` (a Windows session changes them in parallel).
- `portico shell` and every CLI command other than `portico settings` ignore the file.
- Proxmox: nothing is ever remembered; `remember_user` refuses a Proxmox key.
- Types are checked only for `users`, `users.local`, `users.host`, each `users.host.<alias>`, and the `docker`/`podman` tables and their entries (non-empty string after trim). Everything else under `users` is ignored and kept.
- Invalid file: every default is used and nothing is written over it (unchanged rule).
- Commit messages: Conventional Commits, no AI attribution trailers or lines of any kind.
- Run `scripts/ci-checks.sh` as a regular user (not root) before the final commit.

## Review Focus

1. A host alias containing a dot (`db.example`) must round-trip through write and load as one alias, not nested tables. → Task 1 test `alias_with_a_dot_round_trips`.
2. Forgetting the last entry, then reloading: the file stays valid and loads with no users. → Task 1 test `forget_then_load_is_empty`.
3. A user typed with spaces (`"  deploy "`) is stored trimmed; a line of only spaces forgets. → Task 4 test `blank_or_spaced_user_is_trimmed`.
4. Moving the selection to another container after `u` edits must not carry the edit over. → Task 4 test `user_line_follows_selection`.
5. An invalid settings file at TUI start: opening a shell still works and writes nothing. → Task 4 test `invalid_file_never_writes`.

---

### Task 1: Domain: remembered users in `settings.toml`

**Files:**
- Modify: `src/backend/mod.rs` (derive `PartialOrd, Ord` on `Target` and `BackendId`)
- Modify: `src/backend/listing.rs` (add `remember_key`)
- Modify: `src/domain/settings.rs`

**Interfaces:**
- Produces:
  - `backend::listing::remember_key(container: &Container) -> String` (re-export from `backend` like the other listing items).
  - `domain::settings::UserKey { pub target: Target, pub backend: BackendId, pub container: String }`, deriving `Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash`.
  - `Settings { pub tabs: Tabs, pub users: BTreeMap<UserKey, String> }`.
  - `Settings::user(&self, key: &UserKey) -> Option<&str>`.
  - `remember_user(path: &Path, key: &UserKey, user: Option<&str>) -> Result<bool, SettingsError>`.

- [ ] **Step 1: Write failing tests** in `listing.rs` (`remember_key_uses_project_service`, `remember_key_ignores_replica_number`, `remember_key_falls_back_to_name`) and in `settings.rs`:
  - `users_load_from_nested_tables`: text `[users.local.docker]\n"shop/api" = "deploy"\nweb = "www-data"\n[users.host.prod.podman]\n"shop/api" = "app"\n` gives three entries with the expected `UserKey`s.
  - `alias_with_a_dot_round_trips`: `remember_user` for `Target::Host("db.example")`, Docker, `shop/db`, `postgres`; the text contains `[users.host."db.example".docker]`; `load` gives that one entry.
  - `wrong_user_type_is_invalid_with_its_line`: `[users.local.docker]\n\napi = 3\n` → `Invalid { line: Some(3) }`; `api = "  "` → invalid (blank); `users = 1` → invalid.
  - `unknown_backend_and_proxmox_are_ignored_and_kept`: `[users.local.proxmox]\n"110" = 5\n[users.local.lxd]\nx = 1\n[users.remote]\ny = 2\n` loads with no users, and a later `remember_user` keeps all three in the text.
  - `remember_user_replaces_and_forgets`: set `deploy`, set `root` (returns true, file says root), set `root` again (returns false), forget (true), forget again (false).
  - `forget_then_load_is_empty`: after the forget, `load` is `Ok` with no users.
  - `forget_on_missing_file_creates_nothing`.
  - `remember_user_keeps_comments_and_tabs`: a file with `# mine\n[tabs]\npodman = false\n` keeps both after a remember.
  - `remember_user_refuses_an_invalid_file` and `remember_user_writes_through_a_symlink` (mirror the existing `set` tests).
  - `remember_user_refuses_proxmox`: a Proxmox key returns `Ok(false)` and creates no file (no panic, no `debug_assert!`).
- [ ] **Step 2:** `cargo test --lib settings listing` → the new tests fail to compile/fail.
- [ ] **Step 3: Implement.** Factor `set`'s body after the early check into `fn edit(path, already: impl Fn(&Settings) -> bool, change: impl FnOnce(&mut DocumentMut, &str) -> Result<(), SettingsError>) -> Result<bool, SettingsError>` (symlink target, early check, create dir, lock, re-read, recheck, parse, header for a new file, change, atomic write, sync). `set` and `remember_user` both call it. In `read`, parse `users` after `tabs`: walk `local` and each `host.<alias>`, then only the `docker`/`podman` keys (use `BackendId::parse`-like matching on `as_str()`), validating as in the constraints. `remember_user` creates missing tables with `Table::new()` marked implicit (`set_implicit(true)`) so the file shows `[users.local.docker]` and not empty `[users]` / `[users.local]` headers; removing an entry leaves its table. Use `toml_edit`'s key quoting (insert via `Table::insert` with the raw alias; it quotes as needed).
- [ ] **Step 4:** `cargo test --lib` → all pass.
- [ ] **Step 5: Commit** `feat: remember container users in the settings file`.

### Task 2: App entry point

**Files:**
- Modify: `src/app.rs`

**Interfaces:**
- Consumes: `settings::remember_user`, `UserKey`.
- Produces: `App::remember_container_user(&self, key: &UserKey, user: Option<&str>) -> Result<bool, SettingsError>`.

- [ ] **Step 1:** Test `remember_container_user_round_trips` beside `settings_default_under_the_home_and_round_trip`: remember `deploy` for local Docker `shop/api`, then `app.settings().unwrap().user(&key) == Some("deploy")`; forget, then `None`.
- [ ] **Step 2:** Run, fails. **Step 3:** Implement as a one-line delegate like `set_setting`. **Step 4:** Run, passes.
- [ ] **Step 5: Commit** `feat: let the app remember a container's user`.

### Task 3: CLI: list, forget, JSON

**Files:**
- Modify: `src/cli/settings.rs`, `src/cli/json.rs`, `src/cli/mod.rs` (only if the subcommand enum needs wiring)
- Modify: `tests/cli.rs`, `tests/json.rs`, `tests/fixtures/json/settings.json`, `docs/json-output.md`

**Interfaces:**
- Consumes: `App::settings`, `App::remember_container_user`, `UserKey`.
- Produces: `SettingsCommand::Forget { on: Option<String>, backend: BackendId, container: String }` (`--on <alias>`, `--backend docker|podman`, default Docker; `proxmox` is a clap value error: "a Proxmox container's shell runs as root, so no user is remembered").

- [ ] **Step 1: Tests in `tests/cli.rs`:**
  - `settings_lists_remembered_users`: write a file with the spec's example (`local docker shop/api deploy`, `local docker web www-data`, `host prod podman shop/api app`); `portico settings` output contains, in order,
    ```
    Remembered container users:
      this machine  docker  shop/api  deploy
      this machine  docker  web       www-data
      prod          podman  shop/api  app
    ```
    and the section is absent when no users are stored (extend `settings_lists_every_key_and_the_path` to assert `!contains("Remembered")`).
  - `settings_forget_removes_an_entry`: `settings forget --on prod --backend podman shop/api` succeeds, prints `forgot the user of shop/api (podman on prod)`, and the file no longer has `app`; the same again prints `nothing remembered for shop/api (podman on prod)` and exits 0. Without `--on`, the text says `on this machine`.
  - `settings_forget_refuses_proxmox`: exit 2 and the message above on stderr.
- [ ] **Step 2: Test in `tests/json.rs`:** extend `settings()` to `forget` nothing but first write a `[users.local.docker]\n"shop/api" = "deploy"\n` entry (append to the file after `set`), then `golden(…, "settings.json")`; update the fixture so `users` is `[{"backend":"docker","container":"shop/api","target":{"alias":null,"kind":"local"},"user":"deploy"}]`. Assert `users` is `[]` in the no-file case.
- [ ] **Step 3:** Run, fails. **Step 4: Implement.** Target JSON uses the same helper the containers document uses for `target` (find it in `cli/json.rs`). Human listing: column widths from the longest value of each column, two spaces between, rows from the `BTreeMap` order.
- [ ] **Step 5:** `cargo test --test cli --test json --test docs` → pass (the docs test checks the golden blocks in `docs/json-output.md`; update the Settings section's example and add one sentence describing `users`, plus `portico settings forget` beside the `set` sentence).
- [ ] **Step 6: Commit** `feat: list and forget remembered container users in portico settings`.

### Task 4: TUI: per-container user line, saved on shell open

**Files:**
- Modify: `src/tui/containers.rs`, `src/tui/mod.rs`

**Interfaces:**
- Consumes: `remember_key`, `UserKey`, `Settings::users`, `App::remember_container_user`.
- Produces:
  - `Panel` replaces `user: TextInput` with `users: HashMap<(Target, String), String>` (session map, key = target + `remember_key`) plus `user: TextInput` as the edit buffer only while `editing_user`.
  - `ContainerTabs::remember(&mut self, users: &BTreeMap<UserKey, String>)` seeds every panel with its backend's entries.
  - `PanelAction::Shell` gains `remember: Option<UserKey>` (None for Proxmox).
  - `UiState` gains `settings_valid: bool` (false when the startup load failed).

- [ ] **Step 1: State tests in `containers.rs`** (use the existing listing fixtures and helpers):
  - `user_line_follows_selection`: seed `shop/api = deploy`; selected `api` shows `user: deploy`; Down to `db` shows `user: image default`; `u`, type `root`, Enter on `db`, Up shows `deploy`, Down shows `root`.
  - `user_edit_does_not_write`: after `u` + Enter, the returned action is `PanelAction::None`.
  - `shell_carries_the_key_to_remember`: Enter on `api` gives `Shell { user: Some("deploy"), remember: Some(UserKey { target: Local, backend: Docker, container: "shop/api" }), .. }`; `n` the same with `new_terminal: true`.
  - `blank_or_spaced_user_is_trimmed`: `u`, `"  root "`, Enter → shell `user: Some("root")`; `u`, clear, type spaces, Enter → `user: None` and `remember: Some(..)` (so the caller forgets).
  - `proxmox_shell_remembers_nothing`: `remember: None`.
  - Update the existing user-line tests (`enter_and_n_produce_shell_with_user`, `esc_while_editing_user_restores`, the Proxmox/Docker switch test at ~line 1346) to the per-container behaviour.
- [ ] **Step 2: Tests in `mod.rs`** for the shell handler, extracted as `fn remember_after_shell(app: &App, state_valid: bool, key: Option<&UserKey>, user: Option<&str>) -> Option<String>` returning the status-line message on failure:
  - `remember_after_shell_writes_the_file` (temp settings path via `App::with_settings_path`).
  - `invalid_file_never_writes`: `state_valid = false` → returns `None` and the file text is unchanged.
  - `failed_write_is_reported`: file made invalid after startup → returns `Some(msg)` with `msg` starting `User not remembered: ` followed by the settings error.
- [ ] **Step 3:** Run, fails. **Step 4: Implement.** At startup (`browse_hosts`), on `Ok(settings)` also call `state.containers.remember(&settings.users)` and set `settings_valid = true`. In the `PanelAction::Shell` arm, call `remember_after_shell` before opening the shell; append its message to the shell's status (shell status first, then the message on the same status line separated by ` · `). The details pane's `user:` line uses the same per-container value.
- [ ] **Step 5:** `cargo test --lib tui` → pass.
- [ ] **Step 6: Commit** `feat: remember the user of each container's shell in the TUI`.

### Task 5: End-to-end PTY test

**Files:**
- Modify: `tests/support/tui_pty.py`, `tests/acceptance.rs`

- [ ] **Step 1:** Add scenario `remembered_user` to `tui_pty.py`: press `3` (Docker tab), wait for `api`, press `u`, paste `deploy` with bracketed paste, Enter, wait for `user: deploy`, press Enter (shell: the fake `docker` exits 0 at once), wait for the TUI to return, check `settings.toml` contains `[users.local.docker]` and `"shop/api" = "deploy"`, quit with `q`. Then the harness starts Portico a second time (factor the spawn into a function if needed), presses `3`, and waits for `user: deploy`.
- [ ] **Step 2:** Add `#[test] fn tui_remembers_a_container_user()` in `acceptance.rs` modelled on `tui_tabs_switch_on_a_real_pty`, installing a fake `docker` that prints one Compose container (`shop`/`api`) for `ps` (copy the JSON-lines format from an existing Docker fixture in `src/app/containers.rs` tests) and exits 0 for `exec`.
- [ ] **Step 3:** `cargo test --test acceptance tui_remembers_a_container_user` → pass.
- [ ] **Step 4: Commit** `test: remember a container user across TUI runs on a real PTY`.

### Task 6: Docs and roadmap

**Files:**
- Modify: `README.md`, `docs/architecture.md`, `ROADMAP.md`, `docs/superpowers/specs/2026-09-13-portico-host-management-design.md`, the new spec's status line

- [ ] **Step 1:** README: in the container tabs section, the user line is per container and remembered when a shell opens (blank forgets); in the settings section, the `[users…]` tables and `portico settings forget`.
- [ ] **Step 2:** `docs/architecture.md`: `domain::settings` also holds remembered users; `backend::listing::remember_key`.
- [ ] **Step 3:** `ROADMAP.md`: delete the v4 heading and its item; renumber the remaining items 1–7. v1 spec's "Delivered follow-up slices": add "Remembered container user" pointing at the new spec. Spec status → `Approved; implemented`.
- [ ] **Step 4:** `scripts/ci-checks.sh` as a regular user → all green.
- [ ] **Step 5: Commit** `docs: describe remembered container users and drop the slice from the roadmap`.
