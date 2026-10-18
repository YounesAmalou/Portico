# Portico v1 Host Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a fast Linux CLI/TUI that safely manages an isolated OpenSSH host fragment while leaving existing SSH configuration read-only.

**Architecture:** One Rust binary exposes a flat CLI and a master-detail TUI over one in-process application core. Portico owns `~/.ssh/portico/config`, discovers external aliases read-only, delegates effective resolution and connections to system OpenSSH, and uses locked atomic file transactions; there is no daemon or database.

**Tech Stack:** Rust 1.85+ with the 2024 edition; `clap`, `ratatui`, `crossterm`, `thiserror`, `fs2`, `glob`, and `signal-hook`; `tempfile` for tests; system `ssh` and `xdg-terminal-exec` at runtime.

**Spec:** `docs/superpowers/specs/2026-09-13-portico-host-management-design.md`

## Global Constraints

- OpenSSH remains authoritative for connection behavior and effective configuration.
- Routine CRUD writes only `~/.ssh/portico/config`; existing SSH files remain read-only.
- Portico never reads, copies, or stores private-key contents.
- Child processes receive separate arguments; host data never enters a shell command string.
- The managed directory and files use modes `0700` and `0600` respectively.
- Failed or concurrent mutations must preserve the last valid active file without lost completed updates.
- Keep v1 limited to host CRUD, inspection, and connection. Do not add adoption, identity discovery, arbitrary directives, tunnels, reachability, Quickshell, herdr, JSON output, a daemon, a database, plugins, or an async runtime.
- Use `cargo fmt`, warning-free `cargo clippy`, and focused tests in every task.

## File Map

| Path | Responsibility |
|---|---|
| `Cargo.toml` / `Cargo.lock` | Binary package and minimal dependency set |
| `.gitignore` | Ignore Rust build output and `.superpowers/` visual scratch files |
| `src/lib.rs` | Export the concrete v1 modules |
| `src/host.rs` | Validated managed-host model and edit semantics |
| `src/managed.rs` | Strict Portico-fragment codec, filesystem paths, initialization, locks, backups, and atomic transactions |
| `src/discovery.rs` | Read-only traversal of external `Include` and concrete `Host` aliases |
| `src/openssh.rs` | `ssh -G`, interactive SSH, and `xdg-terminal-exec` process boundaries |
| `src/app.rs` | Ownership rules and shared host operations used by CLI and TUI |
| `src/tui.rs` | Master-detail state, rendering, forms, key handling, and terminal restoration |
| `src/cli.rs` | `clap` contract, confirmations, output, and exit-code mapping |
| `src/main.rs` | Environment wiring and process exit |
| `tests/support/mod.rs` | Shared isolated-home and fake-process integration-test helpers |
| `tests/cli.rs` | Binary-level CLI flows with isolated homes and fake executables |
| `tests/acceptance.rs` | Cross-cutting safety, concurrency, real `ssh -G`, and no-network acceptance proof |

---

### Task 1: Rust package and validated host model

**Files:**
- Create: `Cargo.toml`
- Create: `Cargo.lock`
- Create: `.gitignore`
- Create: `src/lib.rs`
- Create: `src/host.rs`

**Interfaces:**
- Consumes: Nothing; this is the foundation.
- Produces: `Host`, `HostChanges`, `Change<T>`, `ClearField`, and `HostError`; `Host::new`, getters, and `Host::apply`.

- [x] **Step 1: Create the package manifest and ignore file**

Use this manifest, then run `cargo generate-lockfile`:

```toml
[package]
name = "portico"
version = "0.1.0"
edition = "2024"

[dependencies]
clap = { version = "4.5", features = ["derive"] }
crossterm = "0.28"
fs2 = "0.4"
glob = "0.3"
ratatui = "0.29"
signal-hook = "0.3"
thiserror = "2"

[dev-dependencies]
tempfile = "3"
```

`.gitignore` must contain:

```gitignore
/target/
/.superpowers/
```

Run: `cargo generate-lockfile`

Expected: exit 0 and a new `Cargo.lock`.

- [x] **Step 2: Write failing host-model tests**

Create `src/lib.rs` with `pub mod host;`. In `src/host.rs`, define the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host::new("dev-vps", "203.0.113.8", None, None, None, None).unwrap()
    }

    #[test]
    fn validates_alias_and_port() {
        assert!(matches!(
            Host::new("-bad", "example.com", None, None, None, None),
            Err(HostError::InvalidAlias(_))
        ));
        assert!(matches!(
            Host::new("good", "example.com", None, Some(0), None, None),
            Err(HostError::InvalidPort(0))
        ));
    }

    #[test]
    fn applies_only_requested_changes() {
        let changed = host()
            .apply(HostChanges {
                user: Change::Set("deploy".into()),
                port: Change::Set(2222),
                ..HostChanges::default()
            })
            .unwrap();

        assert_eq!(changed.alias(), "dev-vps");
        assert_eq!(changed.hostname(), "203.0.113.8");
        assert_eq!(changed.user(), Some("deploy"));
        assert_eq!(changed.port(), Some(2222));
    }

    #[test]
    fn clear_rejects_required_fields() {
        let changes = HostChanges::from_clear_fields([ClearField::User]);
        assert_eq!(host().apply(changes).unwrap().user(), None);
        assert!("hostname".parse::<ClearField>().is_err());
    }
}
```

- [x] **Step 3: Run the focused test to verify failure**

Run: `cargo test host::tests --lib`

Expected: compilation fails because the host types are not defined.

- [x] **Step 4: Implement the minimal validated model**

Use these exact public shapes:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Host {
    alias: String,
    hostname: String,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<String>,
    proxy_jump: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Change<T> {
    #[default]
    Keep,
    Set(T),
    Clear,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostChanges {
    pub alias: Change<String>,
    pub hostname: Change<String>,
    pub user: Change<String>,
    pub port: Change<u16>,
    pub identity_file: Change<String>,
    pub proxy_jump: Change<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearField { User, Port, Identity, Jump }

impl HostChanges {
    pub fn from_clear_fields(fields: impl IntoIterator<Item = ClearField>) -> Self;
}
```

Implement `FromStr` for `ClearField`, typed `HostError` variants, `Host::new`, getters, and `Host::apply`. Enforce the spec's alias expression, control/whitespace rules, port range, and one-argument identity/jump rules in one `validate` path used by both construction and edits. Do not touch identity files.

- [x] **Step 5: Run model tests and static checks**

Run: `cargo test host::tests --lib`

Expected: 3 passed, 0 failed.

Run: `cargo fmt --check`

Expected: exit 0.

Run: `cargo clippy --lib -- -D warnings`

Expected: exit 0.

- [x] **Step 6: Commit the foundation**

```bash
git add Cargo.toml Cargo.lock .gitignore src/lib.rs src/host.rs
git commit -m "feat: add validated host model"
```

---

### Task 2: Strict managed-fragment codec

**Files:**
- Create: `src/managed.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::host::{Host, HostError}`.
- Produces: `ManagedConfig::{empty, parse, render, hosts, find, insert, replace, remove}` and `ManagedError`.

- [x] **Step 1: Write failing codec tests**

Add `pub mod managed;` to `src/lib.rs`, then add these tests to `src/managed.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_supported_hosts_in_stable_order() {
        let input = "# Managed by Portico. Do not edit manually.\n\nHost zed\n    HostName z.example\n\nHost alpha\n    HostName a.example\n    User deploy\n    Port 2222\n    IdentityFile ~/.ssh/id_alpha\n    ProxyJump bastion\n";
        let parsed = ManagedConfig::parse(input).unwrap();
        let rendered = parsed.render();

        assert!(rendered.find("Host alpha").unwrap() < rendered.find("Host zed").unwrap());
        assert_eq!(ManagedConfig::parse(&rendered).unwrap(), parsed);
    }

    #[test]
    fn rejects_unknown_or_duplicate_directives() {
        assert!(matches!(
            ManagedConfig::parse("Host dev\n  HostName dev\n  LocalForward 3000 dev:3000\n"),
            Err(ManagedError::UnsupportedDirective { .. })
        ));
        assert!(matches!(
            ManagedConfig::parse("Host dev\n  HostName one\n  HostName two\n"),
            Err(ManagedError::DuplicateDirective { .. })
        ));
    }

    #[test]
    fn quotes_identity_paths_as_one_argument() {
        let input = "Host dev\n  HostName dev.example\n  IdentityFile \"~/.ssh/key with space\"\n";
        let parsed = ManagedConfig::parse(input).unwrap();
        let rendered = parsed.render();
        assert!(rendered.contains("IdentityFile \"~/.ssh/key with space\""));
        assert_eq!(ManagedConfig::parse(&rendered).unwrap(), parsed);
    }
}
```

- [x] **Step 2: Run the focused tests to verify failure**

Run: `cargo test managed::tests --lib`

Expected: compilation fails because `ManagedConfig` and `ManagedError` are missing.

- [x] **Step 3: Implement the strict codec and collection operations**

Define:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManagedConfig { hosts: Vec<Host> }

impl ManagedConfig {
    pub fn empty() -> Self;
    pub fn parse(input: &str) -> Result<Self, ManagedError>;
    pub fn render(&self) -> String;
    pub fn hosts(&self) -> &[Host];
    pub fn find(&self, alias: &str) -> Option<&Host>;
    pub fn insert(&mut self, host: Host) -> Result<(), ManagedError>;
    pub fn replace(&mut self, old_alias: &str, host: Host) -> Result<(), ManagedError>;
    pub fn remove(&mut self, alias: &str) -> Result<Host, ManagedError>;
}
```

The parser accepts blank lines, comments, one concrete alias per `Host`, and only `HostName`, `User`, `Port`, `IdentityFile`, and `ProxyJump`. Match directive names case-insensitively, require exactly one `HostName`, reject duplicate aliases case-insensitively, and report line numbers in every parse error. Use one shared quote/unquote helper for supported values: emit an unquoted value only when it contains no whitespace, quote it otherwise, and backslash-escape `\` and `"` inside quotes. The renderer emits the fixed header, four-space indentation, one blank line between blocks, and case-insensitive alias ordering.

- [x] **Step 4: Add mutation behavior tests**

```rust
#[test]
fn collection_operations_are_case_insensitive() {
    let mut config = ManagedConfig::empty();
    config.insert(Host::new("Prod", "prod.example", None, None, None, None).unwrap()).unwrap();
    assert!(matches!(
        config.insert(Host::new("prod", "other.example", None, None, None, None).unwrap()),
        Err(ManagedError::DuplicateAlias(_))
    ));
    assert_eq!(config.remove("PROD").unwrap().hostname(), "prod.example");
}
```

Run: `cargo test managed::tests --lib`

Expected: all managed codec tests pass.

- [x] **Step 5: Run static checks and commit**

Run: `cargo fmt --check`

Expected: exit 0.

Run: `cargo clippy --lib -- -D warnings`

Expected: exit 0.

```bash
git add src/lib.rs src/managed.rs
git commit -m "feat: add managed ssh config codec"
```

---

### Task 3: Read-only external alias discovery

**Files:**
- Create: `src/discovery.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: Canonical entry-config and managed-fragment paths.
- Produces: `discover(entry: &Path, managed: &Path) -> Result<Discovery, DiscoveryError>`, `parse_directive`, `ConfigDirective`, `ExternalHost`, and `DiscoveryWarning`.

- [x] **Step 1: Write failing recursive-discovery tests**

Add `pub mod discovery;` to `src/lib.rs`. Use `tempfile::tempdir` and this test shape:

```rust
#[test]
fn follows_includes_skips_managed_and_ignores_patterns() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join(".ssh");
    std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
    let managed = ssh.join("portico/config");
    std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
    std::fs::write(&managed, "Host owned\n  HostName owned.example\n").unwrap();
    std::fs::write(
        ssh.join("config"),
        "Include conf.d/*.conf\nInclude portico/config\nHost direct *.corp !blocked\n  User deploy\n",
    ).unwrap();
    std::fs::write(
        ssh.join("conf.d/hosts.conf"),
        "Host bastion\n  HostName bastion.example\nInclude ../config\n",
    ).unwrap();

    let result = discover(&ssh.join("config"), &managed).unwrap();
    let aliases: Vec<_> = result.hosts.iter().map(|host| host.alias.as_str()).collect();

    assert_eq!(aliases, ["bastion", "direct"]);
    assert!(result.warnings.is_empty());
}
```

Add these failure-boundary tests:

```rust
#[test]
fn missing_include_marks_alias_inventory_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::write(ssh.join("config"), "Include missing/*.conf\nHost visible\n  HostName visible.example\n").unwrap();

    let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();
    assert!(!result.aliases_complete);
    assert_eq!(result.hosts[0].alias, "visible");
    assert_eq!(result.warnings.len(), 1);
}

#[test]
fn case_only_duplicate_is_reported_as_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::write(ssh.join("config"), "Host Prod\n  HostName one.example\nHost prod\n  HostName two.example\n").unwrap();

    let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();
    assert!(!result.aliases_complete);
    assert!(result.warnings.iter().any(|warning| matches!(warning, DiscoveryWarning::DuplicateAlias { alias, .. } if alias.eq_ignore_ascii_case("prod"))));
}
```

- [x] **Step 2: Run discovery tests to verify failure**

Run: `cargo test discovery::tests --lib`

Expected: compilation fails because discovery types and functions are undefined.

- [x] **Step 3: Implement the narrow read-only scanner**

Use these shapes:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalHost { pub alias: String, pub source: PathBuf }

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Discovery {
    pub hosts: Vec<ExternalHost>,
    pub warnings: Vec<DiscoveryWarning>,
    pub aliases_complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigDirective {
    Include(Vec<String>),
    Host(Vec<String>),
    Other,
}

pub(crate) fn parse_directive(line: &str) -> Result<Option<ConfigDirective>, DiscoveryError>;
pub fn discover(entry: &Path, managed: &Path) -> Result<Discovery, DiscoveryError>;
```

Resolve relative Includes against `~/.ssh` (the entry config's parent), expand `~` and globs, sort matches for deterministic output, and track canonical visited paths to stop cycles. `parse_directive` tokenizes only enough OpenSSH syntax to recognize comments, quoted arguments, `Include`, and `Host`, and discovery reuses it rather than splitting lines independently. Skip the canonical managed path. Treat unreadable/unmatched includes and duplicate case-folded aliases as warnings and set `aliases_complete = false`; later app operations must refuse add/rename while it is false.

- [x] **Step 4: Run tests and static checks**

Run: `cargo test discovery::tests --lib`

Expected: all discovery tests pass.

Run: `cargo fmt --check`

Expected: exit 0.

Run: `cargo clippy --lib -- -D warnings`

Expected: exit 0.

- [x] **Step 5: Commit discovery**

```bash
git add src/lib.rs src/discovery.rs
git commit -m "feat: discover external ssh hosts"
```

---

### Task 4: Safe initialization and managed-file transactions

**Files:**
- Modify: `src/managed.rs`

**Interfaces:**
- Consumes: `ManagedConfig`, `discovery::parse_directive`, and a validation closure with signature `Fn(&Path, &[String]) -> Result<(), String>` where the path is a temporary entry config.
- Produces: `ConfigPaths`, `ManagedStore`, `InitPlan`, `InitOutcome`, `TransactionError`; `plan_init`, `apply_init`, `load`, and `transact`.

- [x] **Step 1: Write failing initialization tests**

Add tests that construct paths from a temporary home rather than changing process-global `HOME`:

```rust
#[test]
fn initialization_backs_up_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::from_home(dir.path());
    std::fs::create_dir_all(&paths.ssh_dir).unwrap();
    std::fs::write(&paths.entry, "Host existing\n  HostName old.example\n").unwrap();
    let store = ManagedStore::new(paths.clone());

    let plan = store.plan_init().unwrap();
    assert!(plan.requires_confirmation());
    store.apply_init(true, |_, _| Ok(())).unwrap();
    let once = std::fs::read(&paths.entry).unwrap();
    assert!(String::from_utf8_lossy(&once).starts_with("Include ~/.ssh/portico/config\n"));
    assert_eq!(store.apply_init(true, |_, _| Ok(())).unwrap(), InitOutcome::Unchanged);
    assert_eq!(std::fs::read(&paths.entry).unwrap(), once);
    assert_eq!(std::fs::read(paths.init_backups().unwrap().first().unwrap()).unwrap(), b"Host existing\n  HostName old.example\n");
}
```

Add these safety tests:

```rust
#[test]
fn symlinked_entry_requires_manual_setup() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::from_home(dir.path());
    std::fs::create_dir_all(&paths.ssh_dir).unwrap();
    let target = dir.path().join("dotfiles-ssh-config");
    std::fs::write(&target, "Host external\n  HostName external.example\n").unwrap();
    symlink(&target, &paths.entry).unwrap();

    let error = ManagedStore::new(paths).plan_init().unwrap_err();
    assert!(matches!(error, InitError::SymlinkRequiresManualSetup { .. }));
}

#[test]
fn rejected_initialization_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::from_home(dir.path());
    let store = ManagedStore::new(paths.clone());
    let error = store.apply_init(false, |_, _| Ok(())).unwrap_err();

    assert!(matches!(error, InitError::ConfirmationRequired));
    assert!(!paths.ssh_dir.exists());
}

#[test]
fn created_paths_have_restrictive_modes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::from_home(dir.path());
    ManagedStore::new(paths.clone()).apply_init(true, |_, _| Ok(())).unwrap();

    assert_eq!(std::fs::metadata(&paths.portico_dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&paths.managed).unwrap().permissions().mode() & 0o777, 0o600);
}
```

- [x] **Step 2: Run initialization tests to verify failure**

Run: `cargo test managed::tests::initialization --lib`

Expected: compilation fails because path/store/init types are missing.

- [x] **Step 3: Implement path derivation and initialization**

Define:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigPaths {
    pub ssh_dir: PathBuf,
    pub entry: PathBuf,
    pub portico_dir: PathBuf,
    pub managed: PathBuf,
    pub backup: PathBuf,
    pub lock: PathBuf,
}

pub struct ManagedStore { paths: ConfigPaths }

impl ManagedStore {
    pub fn new(paths: ConfigPaths) -> Self;
    pub fn paths(&self) -> &ConfigPaths;
    pub fn plan_init(&self) -> Result<InitPlan, InitError>;
    pub fn apply_init<V>(&self, approved: bool, validate: V) -> Result<InitOutcome, InitError>
    where V: Fn(&Path, &[String]) -> Result<(), String>;
    pub fn load(&self) -> Result<ManagedConfig, TransactionError>;
}

impl ConfigPaths {
    pub fn from_home(home: impl AsRef<Path>) -> Self;
    pub fn init_backups(&self) -> Result<Vec<PathBuf>, std::io::Error>;
}
```

`apply_init` acquires the lock, recomputes the plan after confirmation, creates a non-overwriting `config.portico-backup-<unix-seconds>-<pid>` backup with `create_new(true)`, prefixes or repositions exactly one active Portico Include, writes same-directory temporary files, validates before activation, preserves existing entry permissions, and syncs files/directories. Use `fs2::FileExt::lock_exclusive`; no stale PID lock protocol.

- [x] **Step 4: Write failing transaction tests**

```rust
#[test]
fn failed_validation_preserves_active_bytes() {
    let fixture = initialized_store();
    let before = std::fs::read(&fixture.paths.managed).unwrap();
    let host = Host::new("dev", "dev.example", None, None, None, None).unwrap();

    let result = fixture.store.transact(
        |config| config.insert(host),
        |_, _| Err("invalid candidate".into()),
    );

    assert!(matches!(result, Err(TransactionError::Validation(_))));
    assert_eq!(std::fs::read(&fixture.paths.managed).unwrap(), before);
}
```

Add the successful backup test:

```rust
#[test]
fn successful_mutation_keeps_exact_previous_version() {
    let fixture = initialized_store();
    let first = Host::new("one", "one.example", None, None, None, None).unwrap();
    fixture.store.transact(|config| config.insert(first), |_, _| Ok(())).unwrap();
    let before_second = std::fs::read(&fixture.paths.managed).unwrap();
    let second = Host::new("two", "two.example", None, None, None, None).unwrap();
    fixture.store.transact(|config| config.insert(second), |_, _| Ok(())).unwrap();

    assert_eq!(std::fs::read(&fixture.paths.backup).unwrap(), before_second);
}
```

Define a local `InitializedStore { _dir: TempDir, store: ManagedStore, paths: ConfigPaths }` test fixture and `initialized_store() -> InitializedStore` helper in `src/managed.rs`. The helper calls `apply_init(true, |_, _| Ok(()))`; `_dir` keeps the temporary home alive through every assertion.

- [x] **Step 5: Implement the atomic transaction**

Add:

```rust
pub fn transact<R, M, V>(&self, mutate: M, validate: V) -> Result<R, TransactionError>
where
    M: FnOnce(&mut ManagedConfig) -> Result<R, ManagedError>,
    V: Fn(&Path, &[String]) -> Result<(), String>;
```

Under one exclusive lock: reload, mutate, render, create a unique same-directory candidate with `create_new(true)`, write/sync it, build a temporary entry config that substitutes the Portico Include with the candidate path, validate every managed alias, rotate the one-step backup, atomically rename, sync the directory, and remove temporary validation files. A drop guard must remove unused candidates after errors.

- [x] **Step 6: Run managed tests and static checks**

Run: `cargo test managed::tests --lib`

Expected: all codec, initialization, and transaction tests pass.

Run: `cargo fmt --check`

Expected: exit 0.

Run: `cargo clippy --lib -- -D warnings`

Expected: exit 0.

- [x] **Step 7: Commit safe storage**

```bash
git add src/managed.rs
git commit -m "feat: add atomic ssh config storage"
```

---

### Task 5: OpenSSH boundary and shared application operations

**Files:**
- Create: `src/openssh.rs`
- Create: `src/app.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Host`, `HostChanges`, `ManagedStore`, `discover`, and configured executable paths.
- Produces: `OpenSsh`, `ResolvedConfig`, `App`, `HostSummary`, `HostDetails`, `Ownership`, and all shared operations.

- [x] **Step 1: Write failing process-boundary tests**

Add `pub mod openssh;` and `pub mod app;` to `src/lib.rs`. Create an executable fake `ssh` in a temporary directory that records one argument per output line. Test these exact calls:

```rust
#[test]
fn passes_alias_as_one_argument_without_a_shell() {
    let fake = fake_ssh();
    let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
    let status = openssh.connect("dev-vps", false).unwrap();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(fake.log()).unwrap(), "dev-vps\n");
}

#[test]
fn parses_effective_configuration() {
    let fake = fake_resolver("hostname dev.example\nuser deploy\nport 2222\nidentityfile ~/.ssh/id_dev\nproxyjump bastion\n");
    let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
    let resolved = openssh.resolve("dev-vps").unwrap();

    assert_eq!(resolved.hostname, "dev.example");
    assert_eq!(resolved.user.as_deref(), Some("deploy"));
    assert_eq!(resolved.port, Some(2222));
}
```

In the `openssh` test module, define `FakeProgram { _dir: TempDir, path: PathBuf, log: PathBuf }` with `path(&self) -> PathBuf` and `log(&self) -> &Path`. `fake_ssh()` writes a mode-`0700` script that records `"$@"` one argument per line and exits 0. `fake_resolver(output: &str)` writes a mode-`0700` script that prints the supplied fixed output when its first argument is `-G`. Build script contents only from test-controlled strings.

- [x] **Step 2: Run OpenSSH tests to verify failure**

Run: `cargo test openssh::tests --lib`

Expected: compilation fails because the process adapter is missing.

- [x] **Step 3: Implement the concrete process adapter**

Define:

```rust
#[derive(Clone, Debug)]
pub struct OpenSsh { ssh: PathBuf, terminal: PathBuf }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConfig {
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
}

impl OpenSsh {
    pub fn system() -> Self;
    pub fn new(ssh: PathBuf, terminal: PathBuf) -> Self;
    pub fn validate(&self, entry: &Path, aliases: &[String]) -> Result<(), OpenSshError>;
    pub fn resolve(&self, alias: &str) -> Result<ResolvedConfig, OpenSshError>;
    pub fn connect(&self, alias: &str, new_terminal: bool) -> Result<ExitStatus, OpenSshError>;
}
```

Use `Command` directly. Validation runs `ssh -G -F <temporary-entry> <alias>` for each alias, using a harmless validation alias when the list is empty. Resolution runs `ssh -G <alias>` and parses only the five displayed keys. New-terminal connection runs `xdg-terminal-exec -- ssh <alias>` as distinct arguments.

- [x] **Step 4: Write failing application-operation tests**

Build an `App` with temporary paths and fake executables, then test:

```rust
#[test]
fn external_hosts_are_visible_but_immutable() {
    let app = test_app_with_external("Host legacy\n  HostName legacy.example\n");
    let listed = app.hosts().unwrap();
    assert!(listed.iter().any(|host| host.alias == "legacy" && host.ownership == Ownership::External));

    let error = app.remove("legacy").unwrap_err();
    assert!(matches!(error, AppError::ReadOnlyExternal(alias) if alias == "legacy"));
}

#[test]
fn add_edit_rename_and_remove_share_one_transaction_path() {
    let app = initialized_test_app();
    app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap()).unwrap();
    app.edit("dev", HostChanges { alias: Change::Set("prod".into()), user: Change::Set("deploy".into()), ..Default::default() }).unwrap();
    assert_eq!(app.show("PROD").unwrap().declared.unwrap().user(), Some("deploy"));
    app.remove("prod").unwrap();
    assert!(matches!(app.show("prod"), Err(AppError::UnknownHost(_))));
}
```

Add the ownership-boundary tests:

```rust
#[test]
fn add_rejects_case_only_external_collision() {
    let app = test_app_with_external("Host Prod\n  HostName prod.example\n");
    let error = app.add(Host::new("prod", "other.example", None, None, None, None).unwrap()).unwrap_err();
    assert!(matches!(error, AppError::AliasCollision(alias) if alias == "prod"));
}

#[test]
fn incomplete_discovery_blocks_add_but_not_existing_connect() {
    let app = test_app_with_external("Include missing/*.conf\nHost legacy\n  HostName legacy.example\n");
    let error = app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap()).unwrap_err();
    assert!(matches!(error, AppError::IncompleteDiscovery(_)));
    assert!(app.show("legacy").is_ok());
    assert!(app.connect("legacy", false).unwrap().success());
}
```

- [x] **Step 5: Implement the application core**

Use these public methods:

```rust
pub struct App { store: ManagedStore, openssh: OpenSsh }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ownership { Managed, External }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSummary {
    pub alias: String,
    pub ownership: Ownership,
    pub source: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDetails {
    pub summary: HostSummary,
    pub declared: Option<Host>,
    pub resolved: ResolvedConfig,
    pub warnings: Vec<DiscoveryWarning>,
}

impl App {
    pub fn new(store: ManagedStore, openssh: OpenSsh) -> Self;
    pub fn init_plan(&self) -> Result<InitPlan, AppError>;
    pub fn initialize(&self, approved: bool) -> Result<InitOutcome, AppError>;
    pub fn hosts(&self) -> Result<Vec<HostSummary>, AppError>;
    pub fn show(&self, alias: &str) -> Result<HostDetails, AppError>;
    pub fn add(&self, host: Host) -> Result<(), AppError>;
    pub fn edit(&self, alias: &str, changes: HostChanges) -> Result<(), AppError>;
    pub fn remove(&self, alias: &str) -> Result<(), AppError>;
    pub fn connect(&self, alias: &str, new_terminal: bool) -> Result<ExitStatus, AppError>;
}
```

In the `app` test module, define `TestApp { _dir: TempDir, app: App }`. `initialized_test_app() -> TestApp` creates a temporary home, initializes its `ManagedStore`, and injects a fake resolver. `test_app_with_external(contents: &str) -> TestApp` additionally writes `contents` to the user entry config after the Portico Include. Implement `Deref<Target = App>` for `TestApp` so the shown calls compile while the temporary directory stays alive.

`HostSummary` contains alias, ownership, and source path. `HostDetails` adds optional declared `Host`, resolved config, and discovery warnings. Resolve effective configuration only for `show` or the currently selected TUI host, not every row. Before add/rename, discover again and reject incomplete discovery or any case-insensitive collision. Before edit/remove, prove the alias exists in the managed store; otherwise return the external/unknown typed error.

- [x] **Step 6: Run core tests and static checks**

Run: `cargo test --lib`

Expected: all process and application tests pass.

Run: `cargo fmt --check`

Expected: exit 0.

Run: `cargo clippy --lib -- -D warnings`

Expected: exit 0.

- [x] **Step 7: Commit the shared core**

```bash
git add src/lib.rs src/openssh.rs src/app.rs
git commit -m "feat: add shared host operations"
```

---

### Task 6: Master-detail TUI

**Files:**
- Create: `src/tui.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `App`, `HostSummary`, `HostDetails`, `Host`, `HostChanges`, and `ClearField`.
- Produces: `tui::run(&App) -> Result<(), TuiError>` and testable `UiState` update/render behavior.

- [x] **Step 1: Write failing state-transition and render tests**

Add `pub mod tui;` to `src/lib.rs`. Use `ratatui::backend::TestBackend`:

```rust
#[test]
fn renders_master_detail_and_hides_mutation_actions_for_external_host() {
    let hosts = vec![
        summary("dev", Ownership::Managed),
        summary("legacy", Ownership::External),
    ];
    let mut state = UiState::new(hosts);
    state.select_alias("legacy");
    let buffer = render_to_buffer(&state, details("legacy", Ownership::External));
    let text = buffer_text(&buffer);

    assert!(text.contains("legacy"));
    assert!(text.contains("external"));
    assert!(text.contains("Enter connect"));
    assert!(!text.contains("e edit"));
    assert!(!text.contains("x remove"));
}

#[test]
fn filtering_preserves_a_valid_selection() {
    let mut state = UiState::new(vec![summary("dev", Ownership::Managed), summary("prod", Ownership::Managed)]);
    state.set_filter("pro");
    assert_eq!(state.selected_alias(), Some("prod"));
}
```

In the TUI test module, define `summary(alias: &str, ownership: Ownership) -> HostSummary`, `details(alias: &str, ownership: Ownership) -> HostDetails`, `render_to_buffer(state: &UiState, details: HostDetails) -> Buffer`, and `buffer_text(buffer: &Buffer) -> String`. They create fixed test-only values, render through `ratatui::backend::TestBackend`, and flatten each cell's symbol row by row; no production mock interface is needed.

- [x] **Step 2: Run TUI tests to verify failure**

Run: `cargo test tui::tests --lib`

Expected: compilation fails because TUI state and rendering are missing.

- [x] **Step 3: Implement state and pure rendering**

Use these tested state interfaces:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
enum Mode { Browse, Filter, AddForm, EditForm, ConfirmRemove }

#[derive(Clone, Debug)]
struct UiState {
    hosts: Vec<HostSummary>,
    visible: Vec<usize>,
    selected: Option<usize>,
    filter: String,
    mode: Mode,
    status: Option<String>,
}

impl UiState {
    fn new(hosts: Vec<HostSummary>) -> Self;
    fn set_filter(&mut self, filter: &str);
    fn select_alias(&mut self, alias: &str);
    fn selected_alias(&self) -> Option<&str>;
}
```

Render one horizontal split: host list/filter on the left and details or the active form on the right. The footer derives its keys from current mode and selected ownership. Implement movement, `/`, `a`, `e`, `x`, Enter, `n`, Esc, and `q`; forms must expose exactly the six v1 fields and reuse `Host::new`/`HostChanges` for validation.

- [x] **Step 4: Write failing terminal-suspension test**

```rust
#[test]
fn restores_terminal_around_child_even_when_child_fails() {
    let events = std::cell::RefCell::new(Vec::new());
    let result = suspend_for_child(
        || { events.borrow_mut().push("leave"); Ok(()) },
        || { events.borrow_mut().push("child"); Err(TuiError::Child("failed".into())) },
        || { events.borrow_mut().push("enter"); Ok(()) },
    );

    assert!(result.is_err());
    assert_eq!(*events.borrow(), ["leave", "child", "enter"]);
}
```

- [x] **Step 5: Implement terminal lifecycle and the event loop**

Implement the tested orchestration helper with this signature:

```rust
fn suspend_for_child<T, L, C, E>(mut leave: L, child: C, mut enter: E) -> Result<T, TuiError>
where
    L: FnMut() -> Result<(), TuiError>,
    C: FnOnce() -> Result<T, TuiError>,
    E: FnMut() -> Result<(), TuiError>;
```

Create an RAII terminal guard whose `Drop` disables raw mode, leaves the alternate screen, and restores the cursor. Register `SIGINT`, `SIGTERM`, and `SIGHUP` with `signal_hook::flag` against an `AtomicBool`; poll terminal events with a short timeout so the event loop observes the flag, exits normally, and lets the guard restore the terminal. `suspend_for_child` must always attempt re-entry after the child closure. `run` detects an uninitialized store, presents `InitPlan`, and requires explicit confirmation before calling `App::initialize`. It then loads hosts, resolves only the selected details, calls `App` operations for forms/actions, refreshes after mutations, retains selection by alias, and shows errors in a footer/status line. Before in-place SSH it suspends the TUI; new-terminal SSH leaves the TUI active.

- [x] **Step 6: Run TUI tests and static checks**

Run: `cargo test tui::tests --lib`

Expected: all TUI state, render, form, and restoration tests pass.

Run: `cargo fmt --check`

Expected: exit 0.

Run: `cargo clippy --lib -- -D warnings`

Expected: exit 0.

- [x] **Step 7: Commit the TUI**

```bash
git add src/lib.rs src/tui.rs
git commit -m "feat: add host management tui"
```

---

### Task 7: CLI contract and executable wiring

**Files:**
- Create: `src/cli.rs`
- Create: `src/main.rs`
- Create: `tests/support/mod.rs`
- Create: `tests/cli.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: Every `App` operation and `tui::run`.
- Produces: Exact command/flag contract; `cli::run(args, app, input, output, error) -> ExitCode`; environment-backed executable.

- [x] **Step 1: Write failing command-shape tests**

Add `pub mod cli;` to `src/lib.rs`. In `src/cli.rs`, test parsing before implementing execution:

```rust
#[test]
fn parses_short_host_flags_and_new_terminal() {
    let cli = Cli::try_parse_from([
        "portico", "add", "dev", "-H", "dev.example", "-u", "deploy",
        "-p", "2222", "-i", "~/.ssh/id_dev", "-J", "bastion",
    ]).unwrap();
    assert!(matches!(cli.command, Some(Command::Add { alias, port: Some(2222), .. }) if alias == "dev"));

    let cli = Cli::try_parse_from(["portico", "connect", "dev", "-n"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Connect { new_terminal: true, .. })));
}

#[test]
fn parses_repeatable_unset_and_force() {
    let cli = Cli::try_parse_from(["portico", "edit", "dev", "-U", "user", "-U", "port"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Edit { unset, .. }) if unset.len() == 2));
    let cli = Cli::try_parse_from(["portico", "remove", "dev", "-f"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Remove { force: true, .. })));
}
```

- [x] **Step 2: Run CLI parser tests to verify failure**

Run: `cargo test cli::tests --lib`

Expected: compilation fails because `Cli` and `Command` are undefined.

- [x] **Step 3: Implement the exact `clap` surface**

Define the parser exactly at this boundary; no subcommand opens the TUI:

```rust
#[derive(clap::Parser, Debug)]
#[command(name = "portico", version)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    Init { #[arg(short = 'y', long)] yes: bool },
    Hosts,
    Show { alias: String },
    Add {
        alias: String,
        #[arg(short = 'H', long)] hostname: String,
        #[arg(short = 'u', long)] user: Option<String>,
        #[arg(short = 'p', long)] port: Option<u16>,
        #[arg(short = 'i', long)] identity: Option<String>,
        #[arg(short = 'J', long)] jump: Option<String>,
    },
    Edit {
        alias: String,
        #[arg(short = 'a', long = "alias")] new_alias: Option<String>,
        #[arg(short = 'H', long)] hostname: Option<String>,
        #[arg(short = 'u', long)] user: Option<String>,
        #[arg(short = 'p', long)] port: Option<u16>,
        #[arg(short = 'i', long)] identity: Option<String>,
        #[arg(short = 'J', long)] jump: Option<String>,
        #[arg(short = 'U', long = "unset")] unset: Vec<ClearField>,
    },
    Remove { alias: String, #[arg(short = 'f', long)] force: bool },
    Connect { alias: String, #[arg(short = 'n', long = "new-terminal")] new_terminal: bool },
}
```

- [x] **Step 4: Write failing binary-level CRUD and confirmation tests**

In `tests/support/mod.rs`, define `TestEnv { _dir: TempDir, home: PathBuf, bin: PathBuf }` and `RunResult(std::process::Output)`. `TestEnv::new()` creates fake `ssh` and `xdg-terminal-exec` executables in `bin`; fake `ssh` handles `-G` by printing deterministic valid configuration and records connection arguments, while the fake terminal records its arguments. Use this runner so arrays and dynamically generated concurrency arguments share one path:

```rust
impl TestEnv {
    pub fn run<I, S>(&self, args: I) -> RunResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(&inherited));
        let path = std::env::join_paths(paths).unwrap();
        RunResult(std::process::Command::new(env!("CARGO_BIN_EXE_portico"))
            .args(args)
            .env("HOME", &self.home)
            .env("PATH", path)
            .output()
            .unwrap())
    }
}
```

Implement `ssh_config(&self) -> PathBuf`, `managed_config(&self) -> PathBuf`, `connection_log(&self) -> PathBuf`, and assertion methods `assert_success`, `assert_failure`, `assert_stdout_contains`, and `assert_failure_stderr_contains` on `RunResult`. Do not change the production binary to accommodate tests.

In `tests/cli.rs`, declare `mod support; use support::TestEnv;` and exercise the binary:

```rust
#[test]
fn add_show_edit_and_force_remove_flow() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example", "-u", "deploy"]).assert_success();
    env.run(["show", "dev"]).assert_stdout_contains("dev.example");
    env.run(["edit", "dev", "-p", "2222", "-U", "user"]).assert_success();
    env.run(["remove", "dev"]).assert_failure_stderr_contains("--force");
    env.run(["remove", "dev", "-f"]).assert_success();
    env.run(["show", "dev"]).assert_failure();
}
```

Add these focused binary assertions using the same fixture:

```rust
#[test]
fn lists_ownership_and_rejects_external_mutation() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    std::fs::write(env.ssh_config(), "Include ~/.ssh/portico/config\nHost legacy\n  HostName legacy.example\n").unwrap();
    env.run(["hosts"]).assert_stdout_contains("legacy  external");
    env.run(["remove", "legacy", "-f"]).assert_failure_stderr_contains("read-only");
}

#[test]
fn connection_modes_propagate_status_and_arguments() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "dev.example"]).assert_success();
    env.run(["connect", "dev"]).assert_success();
    env.run(["connect", "dev", "-n"]).assert_success();
    let log = std::fs::read_to_string(env.connection_log()).unwrap();
    assert!(log.contains("dev"));
    assert!(log.contains("--\nssh\ndev"));
}
```

- [x] **Step 5: Implement command execution and `main`**

Use this boundary:

```rust
pub fn run<I, O, E>(
    cli: Cli,
    app: &App,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
) -> std::process::ExitCode
where
    I: std::io::BufRead,
    O: std::io::Write,
    E: std::io::Write;
```

`main` reads `HOME`, creates `ConfigPaths`, uses `OpenSsh::system()` so normal `PATH` lookup selects `ssh` and `xdg-terminal-exec`, parses `Cli`, computes `interactive` with `std::io::IsTerminal`, and exits with `run`. Before `init`, print `InitPlan`; require an interactive confirmation or `-y`. Before removal, prompt only when `interactive` is true; non-TTY removal requires `-f`. Do not add test-only production settings, colored output, or JSON output.

- [x] **Step 6: Run CLI tests and inspect help**

Run: `cargo test cli::tests --lib`

Expected: all CLI parser tests pass.

Run: `cargo test --test cli`

Expected: all binary-flow tests pass.

Run: `cargo run -- --help`

Expected: help lists `init`, `hosts`, `show`, `add`, `edit`, `remove`, and `connect` with the approved flags.

Run: `cargo fmt --check`

Expected: exit 0.

Run: `cargo clippy --all-targets -- -D warnings`

Expected: exit 0.

- [x] **Step 7: Commit the executable**

```bash
git add src/lib.rs src/cli.rs src/main.rs tests/support/mod.rs tests/cli.rs
git commit -m "feat: expose portico cli"
```

---

### Task 8: Cross-cutting acceptance and safety hardening

**Files:**
- Create: `tests/acceptance.rs`
- Modify only when a failing acceptance test identifies a defect: `src/managed.rs`, `src/discovery.rs`, `src/openssh.rs`, `src/app.rs`, `src/tui.rs`, `src/cli.rs`

**Interfaces:**
- Consumes: The complete binary, `tests/support/mod.rs`, and all public v1 behavior.
- Produces: Executable evidence for initialization safety, concurrency, OpenSSH compatibility, and terminal/process invariants.

- [x] **Step 1: Add the real-OpenSSH compatibility test**

Declare `mod support; use support::TestEnv;`. Use a temporary home, initialize with the fake validator, add a managed host, then create a test-only copy of the entry config that replaces the literal Portico Include with the absolute temporary managed path. Invoke installed OpenSSH against that validation copy:

```rust
#[test]
fn ordinary_openssh_resolves_a_portico_host() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "dev", "-H", "203.0.113.8", "-u", "deploy", "-p", "2222"]).assert_success();
    let entry = std::fs::read_to_string(env.ssh_config()).unwrap();
    let validation = env.ssh_config().with_file_name("acceptance-config");
    std::fs::write(
        &validation,
        entry.replace("~/.ssh/portico/config", env.managed_config().to_str().unwrap()),
    ).unwrap();

    let output = std::process::Command::new("ssh")
        .args(["-G", "-F"])
        .arg(validation)
        .arg("dev")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    assert!(output.status.success());
    assert!(stdout.contains("hostname 203.0.113.8"));
    assert!(stdout.contains("user deploy"));
    assert!(stdout.contains("port 2222"));
}
```

Run: `cargo test --test acceptance ordinary_openssh_resolves_a_portico_host`

Expected: PASS using the installed OpenSSH parser without touching the real home or opening a network connection.

- [x] **Step 2: Add concurrency and rollback acceptance tests**

Add these acceptance tests, extending `TestEnv` with `reject_alias(&self, alias: &str)` so its fake `ssh -G` exits nonzero for that exact alias:

```rust
#[test]
fn concurrent_adds_do_not_lose_completed_updates() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8).map(|index| {
            let barrier = &barrier;
            let env = &env;
            scope.spawn(move || {
                let alias = format!("host-{index}");
                let hostname = format!("host-{index}.example");
                barrier.wait();
                env.run(["add", alias.as_str(), "-H", hostname.as_str()]).assert_success();
            })
        }).collect();
        for handle in handles { handle.join().unwrap(); }
    });

    let managed = std::fs::read_to_string(env.managed_config()).unwrap();
    for index in 0..8 {
        assert!(managed.contains(&format!("Host host-{index}\n")));
    }
}

#[test]
fn rejected_candidate_preserves_active_and_backup_bytes() {
    let env = TestEnv::new();
    env.run(["init", "-y"]).assert_success();
    env.run(["add", "good", "-H", "good.example"]).assert_success();
    let active_before = std::fs::read(env.managed_config()).unwrap();
    let backup_path = env.managed_config().with_file_name("config.bak");
    let backup_before = std::fs::read(&backup_path).ok();
    env.reject_alias("rejected");

    env.run(["add", "rejected", "-H", "rejected.example"]).assert_failure();

    assert_eq!(std::fs::read(env.managed_config()).unwrap(), active_before);
    assert_eq!(std::fs::read(&backup_path).ok(), backup_before);
}
```

Run: `cargo test --test acceptance`

Expected: PASS if the earlier invariants are correct. If it fails, stop this task, add a focused regression test in the responsible earlier task, make the smallest correction there, and rerun this acceptance test before continuing.

- [x] **Step 3: Run the complete verification suite**

Run: `cargo test --all-targets`

Expected: all tests pass, including real `ssh -G`; no test opens a network connection or reads a private-key file.

Run: `cargo fmt --check`

Expected: exit 0.

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: exit 0 with no warnings.

Run: `cargo run -- --help`

Expected: approved commands and short flags are present, with no deferred commands.

- [x] **Step 4: Perform the isolated manual smoke test**

Run `cargo test --test acceptance -- --nocapture` as the isolated smoke harness, then use `cargo run -- --help` for the real binary surface. The harness creates its own temporary home, exercises `init -y`, `add`, `hosts`, `show`, `edit`, new-terminal connection through a fake terminal executable, and `remove -f`, and prints the temporary config, permissions, backup, and `ssh -G -F` results on failure. Do not run a network connection and do not point Portico at the user's real home.

Expected: every approved v1 flow works; external files differ only by the one initialization Include; managed removal leaves ordinary config valid.

- [x] **Step 5: Commit acceptance proof**

```bash
git add tests/acceptance.rs tests/support/mod.rs src/managed.rs src/discovery.rs src/openssh.rs src/app.rs src/tui.rs src/cli.rs
git commit -m "test: verify portico host management"
```

Do not stage a source file from this list if it did not change.
