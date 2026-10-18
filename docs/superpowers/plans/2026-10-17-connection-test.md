# Connection Test Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A manual connection test (`portico test <alias>`, `t` in the TUI, `Ctrl+T` in the forms) that passes only when OpenSSH authenticates, never opens a session, and lets the user trust a first-time host key after seeing its fingerprint.

**Architecture:** A pure `probe` module builds the `ssh` arguments and classifies OpenSSH's output. `OpenSsh` runs the test (streaming stderr, deadline, cancellation), fetches a host key into a temporary known-hosts file, and fingerprints it with `ssh-keygen`. `ManagedStore` gains a `preview` that stages a mutation like a save but never activates it, so form values are tested against exactly the configuration a save would produce. `App` combines these into `test` and `trust_and_retest`, which the CLI and a new TUI worker (`src/tui/tester.rs`) call.

**Tech Stack:** Rust 2024 (MSRV 1.88), `clap`, `ratatui`/`crossterm`, `libc`, `thiserror`; OpenSSH `ssh` and `ssh-keygen` from `PATH`. No new crates.

**Spec:** `docs/superpowers/specs/2026-10-17-connection-test-design.md`

## Global Constraints

- Test options, verbatim, in this order, before any `-F` and the alias: `-N -n -o BatchMode=yes -o ConnectTimeout=5 -o LogLevel=VERBOSE -o ClearAllForwardings=yes -o ControlMaster=no -o ControlPath=none -o RemoteCommand=none -o PermitLocalCommand=no -o ForwardAgent=no -o ForwardX11=no`, then `[-F <staged entry>] -- <alias>`.
- Key fetch adds, after the test options: `-o PubkeyAuthentication=no -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no -o GSSAPIAuthentication=no -o HostbasedAuthentication=no -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=<temp file>`.
- Overall deadline 15 seconds; `ConnectTimeout=5`.
- Success marker: a stderr line `Authenticated to <host> ([<address>]:<port>) using "<method>".`
- Never write `known_hosts` without the user's confirmation (TUI `y`, CLI `y`, or a matching `--accept-host-key`). A changed host key is never offered for trust.
- Known-hosts file: first `userknownhostsfile` from `ssh -G` (same `-F` for a form), leading `~/` expanded against the home directory; created `0600` if missing; a newline is written first if it does not end with one; the parent directory is not created.
- `ssh-keygen` is run from `PATH` like `ssh`.
- No attribution lines in commits or the PR (see `CLAUDE.md`). Commit titles are Conventional Commits. release-plz builds the changelog from every non-merge commit, so only the user-visible commits (Tasks 5, 6 and 7) use `feat:`; the building blocks (Tasks 1–4) use `chore:` so they stay out of the changelog, and Task 8 uses `docs:`.
- Run `scripts/ci-checks.sh` as a regular user before every commit.

## Review Focus

- **A test still running when the TUI quits or the user presses Ctrl+C:** the `ssh` process group must be killed, not left behind. Owned by Task 6 (the tester is dropped, cancelling, before the scope joins).
- **Form tested while another tool changes the managed file:** `preview` must take the lock while staging and release it before the up-to-15-second test, so it never blocks a concurrent save. Owned by Task 3.
- **`userknownhostsfile` using tokens (`%d`, `%u`) or an absolute path:** absolute paths are used as-is, tokens produce an error and nothing is written. Owned by Task 2.
- **Fingerprint mismatch with `--accept-host-key`, including a key fetched as several lines (for example with `CheckHostIP`):** trusted only when every fetched fingerprint equals the given one. Owned by Task 5.
- **A test result arriving after the user moved on** (another host selected, form closed, a second test started): the result is attached to the host it tested, a form result is dropped once the form is closed, and a superseded test's result is discarded. Owned by Task 6.

---

### Task 1: Probe module — arguments, results and the classifier

**Files:**
- Create: `src/probe.rs`, `tests/fixtures/ssh/*.txt`
- Modify: `src/lib.rs` (add `pub mod probe;`)
- Test: unit tests in `src/probe.rs`

**Interfaces:**
- Produces:
  - `pub const CONNECT_TIMEOUT_SECS: u32 = 5;` and `pub const TEST_DEADLINE: Duration = Duration::from_secs(15);`
  - `pub fn test_args(entry: Option<&Path>, alias: &str) -> Vec<OsString>`
  - `pub fn fetch_key_args(entry: Option<&Path>, known_hosts: &Path, alias: &str) -> Vec<OsString>`
  - `#[derive(Clone, Debug, Eq, PartialEq)] pub struct Authenticated { pub host: String, pub address: String, pub port: u16, pub method: String }`
  - `pub fn authenticated(line: &str) -> Option<Authenticated>`
  - `#[derive(Clone, Debug, Eq, PartialEq)] pub enum Failure { HostKeyUnknown, HostKeyChanged, AuthRefused { methods: String }, NameUnresolved { host: String }, ConnectionRefused, Unreachable, TimedOut, JumpHostFailed { jump: String, detail: String }, Other(String) }`
  - `impl Display for Failure` (the reason) and `impl Failure { pub fn hint(&self) -> Option<&'static str> }`
  - `#[derive(Clone, Debug, Eq, PartialEq)] pub enum ProbeResult { Authenticated(Authenticated), Failed(Failure) }`
  - `pub fn classify(stderr: &str, deadline_reached: bool, jump: Option<&str>) -> Failure`
  - `pub fn known_hosts_path(configured: &[String], home: &Path) -> Result<PathBuf, String>`

Reasons and hints (exact copy):

| Variant | `Display` | `hint()` |
|---|---|---|
| `HostKeyUnknown` | `host key not trusted` | `None` |
| `HostKeyChanged` | `host key changed` | `the server's host key differs from the one you trusted, which can mean an attack; confirm the new key with the server's administrator before changing known_hosts` |
| `AuthRefused { methods }` | `authentication refused (server accepts: {methods})` | `check the user and the key; a key with a passphrase must be loaded in an agent` |
| `NameUnresolved { host }` | `host name {host} does not resolve` | `check the host name` |
| `ConnectionRefused` | `connection refused` | `check the port, and that an SSH server is running there` |
| `Unreachable` | `host unreachable` | `check the address and your network` |
| `TimedOut` | `timed out` | `the host did not answer in time; check the address, the port and any firewall` |
| `JumpHostFailed { jump, detail }` | `jump host {jump} failed: {detail}` | `test the jump host on its own first` |
| `Other(line)` | `{line}` | `None` |

- [x] **Step 1: Capture the fixtures from a real OpenSSH.** In the session scratchpad (never the repository), run a throwaway user `sshd` on `127.0.0.1:22999`: generate a host key, a client key, a passphrase-protected client key (`-N pw`), an `authorized_keys` holding the client key, and an `sshd_config` with `Port 22999`, `ListenAddress 127.0.0.1`, absolute `HostKey`, `PidFile`, `AuthorizedKeysFile`, `StrictModes no`, `UsePAM no`, `PasswordAuthentication no`, `KbdInteractiveAuthentication no`; start it with `/usr/bin/sshd -f sshd_config -E log`. Run `ssh -F /dev/null` with the test options plus `-p 22999 -o IdentityAgent=none -o IdentitiesOnly=yes -i <key> -o UserKnownHostsFile=<file>` and save stderr verbatim as: `authenticated.txt` (good key, trusted), `host-key-unknown.txt` (empty known-hosts file), `host-key-changed.txt` (known-hosts entry with one base64 character altered), `auth-refused.txt` (wrong key), `passphrase-key.txt` (passphrase key), `connection-refused.txt` (port 22998), `name-unresolved.txt` (`probe.invalid`), `jump-connection-refused.txt` (`-J 127.0.0.1:22998`), `jump-target-refused.txt` (`-J` through the running sshd to `127.0.0.1:22998`), and `other.txt` (any failure not listed, for example `-o KexAlgorithms=none-such` against the sshd). Kill the sshd. Note the OpenSSH version in a header-free `tests/fixtures/ssh/VERSION` file.

- [x] **Step 2: Write the failing tests**

```rust
fn fixture(name: &str) -> String // reads tests/fixtures/ssh/{name}.txt via CARGO_MANIFEST_DIR

#[test] fn the_success_line_names_the_address_port_and_method() {
    let line = fixture("authenticated").lines().find_map(authenticated).unwrap();
    assert_eq!((line.address.as_str(), line.port, line.method.as_str()), ("127.0.0.1", 22999, "publickey"));
}
#[test] fn every_recorded_failure_has_its_own_result() {
    for (name, expected) in [
        ("host-key-unknown", Failure::HostKeyUnknown),
        ("host-key-changed", Failure::HostKeyChanged),
        ("auth-refused", Failure::AuthRefused { methods: "publickey".into() }),
        ("passphrase-key", Failure::AuthRefused { methods: "publickey".into() }),
        ("connection-refused", Failure::ConnectionRefused),
        ("name-unresolved", Failure::NameUnresolved { host: "probe.invalid".into() }),
    ] { assert_eq!(classify(&fixture(name), false, None), expected, "{name}"); }
}
#[test] fn failures_through_a_jump_host_name_it() // both jump fixtures, jump = Some("127.0.0.1:22998") or the sshd address as used; matches!(.., Failure::JumpHostFailed { .. })
#[test] fn unreachable_and_timeouts_are_recognised() // classify("ssh: connect to host 10.0.0.1 port 22: No route to host\n"..) == Unreachable; "Network is unreachable" == Unreachable; "Connection timed out" == TimedOut; classify("", true, None) == TimedOut
#[test] fn an_unrecognised_failure_reports_the_last_line() // classify(&fixture("other"), false, None) == Failure::Other(<last non-empty line of the fixture>)
#[test] fn test_arguments_are_exact() {
    let args = test_args(Some(Path::new("/tmp/entry")), "dev");
    assert_eq!(args, ["-N","-n","-o","BatchMode=yes","-o","ConnectTimeout=5","-o","LogLevel=VERBOSE","-o","ClearAllForwardings=yes","-o","ControlMaster=no","-o","ControlPath=none","-o","RemoteCommand=none","-o","PermitLocalCommand=no","-o","ForwardAgent=no","-o","ForwardX11=no","-F","/tmp/entry","--","dev"].map(OsString::from));
    assert!(!test_args(None, "dev").contains(&OsString::from("-F")));
}
#[test] fn fetch_arguments_disable_every_login_method_and_use_the_temporary_file() // contains each of the seven -o values from Global Constraints; ends with ["--", "dev"]
#[test] fn known_hosts_path_expands_home_and_refuses_tokens() {
    let home = Path::new("/home/u");
    assert_eq!(known_hosts_path(&["~/.ssh/known_hosts".into(), "~/.ssh/known_hosts2".into()], home).unwrap(), home.join(".ssh/known_hosts"));
    assert_eq!(known_hosts_path(&["/etc/kh".into()], home).unwrap(), PathBuf::from("/etc/kh"));
    assert!(known_hosts_path(&["~/.ssh/%u_hosts".into()], home).is_err());
    assert!(known_hosts_path(&[], home).is_err());
}
#[test] fn reasons_and_hints_match_the_spec() // Display and hint() for AuthRefused, HostKeyChanged and JumpHostFailed equal the table above
```

- [x] **Step 3: Run them** — `cargo test --lib probe::` → FAIL (module missing).

- [x] **Step 4: Implement `src/probe.rs`.** Classification order: changed-key banner, then `Host key verification failed.` (unknown; also when the line mentions the jump host, see below), then jump-host rules, then `Permission denied (`…`)` (methods are the text inside the parentheses), `Could not resolve hostname <host>:`, `Connection refused`, `No route to host` / `Network is unreachable`, `Connection timed out` / `Operation timed out` / `deadline_reached`, else `Other(last non-empty line)` (`Other("ssh failed without output")` when empty). Jump-host rule: when `jump` is `Some(j)` and a failure line names the jump host's host part, or any line contains `channel 0: open failed` or `UNKNOWN port 65535`, return `JumpHostFailed { jump: j, detail }` where `detail` is the `Display` of the classification of the remaining lines (or the matched line itself when that is `Other`). Let the fixtures decide the exact substrings; the table semantics above are fixed.

- [x] **Step 5: Run them** — `cargo test --lib probe::` → PASS.

- [x] **Step 6: Commit** — `git add src/probe.rs src/lib.rs tests/fixtures/ssh && git commit -m "chore: classify OpenSSH connection test output"`.

---

### Task 2: Running a test, fetching and trusting a host key

**Files:**
- Modify: `src/openssh.rs`
- Test: unit tests in `src/openssh.rs` (`mod tests`, with `write_executable` fake programs as the existing tests do)

**Interfaces:**
- Consumes: Task 1's `test_args`, `fetch_key_args`, `authenticated`, `classify`, `TEST_DEADLINE`, `ProbeResult`, `Failure`.
- Produces:
  - `ResolvedConfig` gains `pub known_hosts_files: Vec<String>` (from `userknownhostsfile`, split on whitespace); update the three existing struct literals.
  - `pub(crate) fn resolve_with(&self, entry: Option<&Path>, alias: &str, cancelled: Option<&AtomicBool>) -> Result<ResolvedConfig, OpenSshError>` (`ssh -G [-F entry] -- alias`); `resolve_inner` delegates to it.
  - `pub(crate) fn probe(&self, entry: Option<&Path>, alias: &str, jump: Option<&str>, cancelled: &AtomicBool) -> Result<ProbeResult, OpenSshError>`
  - `#[derive(Clone, Debug, Eq, PartialEq)] pub struct KeyFingerprint { pub host: String, pub fingerprint: String, pub key_type: String }` (from `ssh-keygen -l` lines `<bits> <fingerprint> <host> (<TYPE>)`)
  - `#[derive(Clone, Debug, Eq, PartialEq)] pub struct PresentedKey { pub lines: Vec<u8>, pub fingerprints: Vec<KeyFingerprint> }`
  - `#[derive(Clone, Debug, Eq, PartialEq)] pub enum KeyFetch { Presented(PresentedKey), Failed(Failure) }`
  - `pub(crate) fn fetch_host_key(&self, entry: Option<&Path>, alias: &str, jump: Option<&str>, cancelled: &AtomicBool) -> Result<KeyFetch, OpenSshError>`
  - `pub fn with_keygen(self, keygen: PathBuf) -> Self`; `OpenSsh::new` defaults `keygen` to `"ssh-keygen"`.
  - `pub(crate) fn append_known_hosts(path: &Path, lines: &[u8]) -> io::Result<()>`

- [x] **Step 1: Write the failing tests**

```rust
#[test] fn probe_stops_at_the_success_line_and_kills_the_group()
// fake ssh: prints the authenticated fixture line to stderr, then `sleep 30`; probe returns Authenticated within 2 s and the sleep's pid (written by the script) no longer exists
#[test] fn probe_passes_the_exact_options()   // fake ssh logs "$@" then exits 255; log == test_args(None, "dev") joined by newlines
#[test] fn probe_classifies_output_when_ssh_exits() // fake ssh prints the auth-refused fixture, exits 255 → Failed(AuthRefused{..})
#[test] fn probe_reports_a_timeout_at_the_deadline() // probe_with_deadline(.., Duration::from_millis(200)) against `sleep 30` → Failed(TimedOut), and the sleep is gone
#[test] fn probe_cancellation_kills_ssh()     // cancel flag set from another thread after 100 ms → Err(OpenSshError::Cancelled), sleep gone
#[test] fn fetch_reads_the_key_written_to_the_temporary_file_and_removes_it()
// fake ssh finds the value after UserKnownHostsFile=, writes "[h]:22 ssh-ed25519 AAAA…\n" there, exits 255; fake ssh-keygen prints "256 SHA256:abc [h]:22 (ED25519)"; result == Presented { lines: that line, fingerprints: [KeyFingerprint{host:"[h]:22", fingerprint:"SHA256:abc", key_type:"ED25519"}] }; the temp path no longer exists
#[test] fn fetch_with_nothing_written_reports_why() // fake ssh prints the connection-refused fixture → KeyFetch::Failed(ConnectionRefused)
#[test] fn append_adds_a_separating_newline_and_creates_missing_files_private() {
    // existing "a b c" (no newline) + b"x y z\n" → "a b c\nx y z\n"; missing file → created with mode 0o600 containing b"x y z\n"; missing parent dir → Err
}
#[test] fn resolve_with_reads_the_known_hosts_files() // -G output "hostname h\nuserknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2\n" → known_hosts_files == both; and "-F" plus the entry precede "--" in the logged args
```

- [x] **Step 2: Run them** — `cargo test --lib openssh::` → FAIL.

- [x] **Step 3: Implement.** `probe` spawns `ssh` with `process_group(0)`, stdin null, stdout null, stderr piped; a reader thread sends stderr lines over an `mpsc` channel; the loop checks `cancelled`, the deadline, the channel (`authenticated(line)` → kill group, reap, return success) and `try_wait`. On exit, drain the channel and `classify(all_lines, false, jump)`; on the deadline kill, `classify(all_lines, true, jump)`. Put the loop in a private `probe_with_deadline(.., deadline: Duration)` so tests can shorten it; `probe` passes `TEST_DEADLINE`. `fetch_host_key` creates an empty temp file with `managed::write_unique` in the system temp directory (`std::env::temp_dir()`), mode `0600`, made `pub(crate)` together with `PendingFile` so the file is removed on drop (`tempfile` is only a dev-dependency; do not add a crate), runs `ssh` with `fetch_key_args` through the same loop (a success line is impossible; treat any exit or the deadline the same), then reads the file: empty → `KeyFetch::Failed(classify(..))`; otherwise runs `<keygen> -l -f <file>` and parses each line. `append_known_hosts` opens with `OpenOptions::new().append(true).create(true).mode(0o600)`, reads the last byte first to decide the separating newline.

- [x] **Step 4: Run them** — `cargo test --lib openssh::` → PASS, and the existing `openssh` tests still pass.

- [x] **Step 5: Commit** — `chore: run connection tests and fetch host keys with OpenSSH`.

---

### Task 3: Staging a save without activating it

**Files:**
- Modify: `src/managed.rs`
- Test: unit tests in `src/managed.rs`

**Interfaces:**
- Produces:
  - `ConfigPaths` gains `pub system_config: PathBuf`, set by `from_home` to `/etc/ssh/ssh_config`.
  - `pub(crate) fn preview<R, E, M, U>(&self, mutate: M, use_entry: U, cancelled: Option<&AtomicBool>) -> Result<R, E> where E: From<TransactionError>, M: FnOnce(&mut ManagedConfig) -> Result<(), E>, U: FnOnce(&Path) -> Result<R, E>`

- [x] **Step 1: Write the failing tests**

```rust
#[test] fn preview_hands_over_the_candidate_and_changes_nothing()
// initialized store with host "dev"; snapshot bytes of managed, backup (if any) and entry;
// preview(|c| c.insert(host("new")), |entry| { read entry: it includes a path whose file contains "Host new"; Ok(entry.to_owned()) }, None)
// afterwards: the three files are byte-identical, the returned entry path and its candidate no longer exist,
// and no ".config.portico-" file remains in ssh_dir or portico_dir
#[test] fn preview_cleans_up_when_the_closure_fails() // use_entry returns Err → same cleanliness assertions
#[test] fn preview_reports_mutation_errors_before_staging() // mutate returns Err → use_entry never called
#[test] fn preview_ends_with_the_system_config_only_when_it_exists()
// paths.system_config = tempdir/ssh_config (exists): staged entry ends with "\nMatch all\nInclude <quoted path>\n"
// paths.system_config = missing path: staged entry has no "Match all"
#[test] fn preview_does_not_hold_the_lock_while_testing()
// inside use_entry, a transact() on the same store (another ManagedStore value with the same paths) succeeds
```

- [x] **Step 2: Run them** — `cargo test --lib managed::` → FAIL.

- [x] **Step 3: Implement.** Extract from `transact_inner` a private `stage(&self, mutate, cancelled) -> Result<(R, Staged), E>` covering `ensure_ready` through writing the validation entry (`Staged` owns the lock file, `active` bytes, the candidate and the validation entry `PendingFile`s, and the changed aliases). `transact_inner` becomes `stage` + validate + the existing activation; its behaviour and existing tests are unchanged. `preview` calls `stage`, appends the system include to a separate preview entry when `paths.system_config` exists (write it with `write_unique` in `ssh_dir`, quoting the path with the existing `quote_path`), drops the lock, then calls `use_entry` and lets the `PendingFile`s drop.

- [x] **Step 4: Run them** — `cargo test --lib managed::` → PASS, including every existing transaction test.

- [x] **Step 5: Commit** — `chore: stage managed changes separately from activating them`.

---

### Task 4: App operations

**Files:**
- Modify: `src/app.rs`
- Test: unit tests in `src/app.rs` (existing harness: a temp home and fake programs)

**Interfaces:**
- Consumes: Tasks 1–3.
- Produces:
  - `#[derive(Clone, Debug, Eq, PartialEq)] pub enum TestTarget { Saved(String), Add(Host), Edit { alias: String, changes: HostChanges } }` with `pub fn alias(&self) -> &str` (the alias tested: the saved alias, the new host's alias, or the edit's new alias when it changes)
  - `#[derive(Clone, Debug, Eq, PartialEq)] pub struct TestReport { pub alias: String, pub user: Option<String>, pub result: ProbeResult, pub key_fetch_failure: Option<Failure> }`
  - `#[derive(Clone, Debug, Eq, PartialEq)] pub enum TestOutcome { Done(TestReport), UnknownHostKey { report: TestReport, key: PresentedKey } }`
  - `pub fn test(&self, target: &TestTarget) -> Result<TestOutcome, AppError>` and `pub(crate) fn test_cancellable(&self, target: &TestTarget, cancelled: &AtomicBool) -> Result<TestOutcome, AppError>`
  - `pub fn trust_and_retest(&self, target: &TestTarget, key: &PresentedKey) -> Result<TestOutcome, AppError>` and `pub(crate) fn trust_and_retest_cancellable(..., cancelled: &AtomicBool)`
  - `AppError` gains `#[error("cannot update known hosts file {path}: {reason}")] KnownHosts { path: PathBuf, reason: String }`; `tui::may_have_changed_inventory` maps it to `false`.

- [x] **Step 1: Write the failing tests**

```rust
#[test] fn a_saved_host_is_tested_with_the_users_configuration() // fake ssh: -G prints "hostname h\nuser deploy\nport 22\n"; otherwise prints the success line. test(&Saved("DEV")) on host "dev" → Done(report) with alias "dev", user Some("deploy"), Authenticated; no "-F" in the probe args
#[test] fn an_unknown_alias_is_an_unknown_host_error()
#[test] fn an_add_form_is_tested_against_the_staged_config_and_nothing_is_saved()
// fake ssh records args; test(&Add(host("new"))) → probe args contain "-F" and end with "--\nnew"; managed file unchanged; App::hosts() has no "new"
#[test] fn an_edit_form_tests_the_renamed_alias() // Edit { alias: "dev", changes.alias = Set("prod") } → probe args end with "--\nprod"; managed file unchanged
#[test] fn form_errors_surface_as_on_save() // Add with an alias colliding with an external host → Err(AppError::AliasCollision(_))
#[test] fn an_unknown_key_comes_back_with_its_fingerprint() // probe output = host-key-unknown fixture; fetch writes a line; → UnknownHostKey { key.fingerprints.len() == 1, .. }
#[test] fn a_failed_fetch_keeps_host_key_not_trusted_and_says_why() // fetch writes nothing and prints connection refused → Done(report{ result: Failed(HostKeyUnknown), key_fetch_failure: Some(ConnectionRefused) })
#[test] fn trusting_appends_to_the_resolved_known_hosts_file_and_retests()
// -G prints "userknownhostsfile ~/.ssh/known_hosts"; trust_and_retest → file under the temp home contains key.lines; second probe ran (args log has two probes)
#[test] fn tokens_in_the_known_hosts_path_are_refused() // "userknownhostsfile ~/.ssh/%u" → Err(AppError::KnownHosts{..}) and nothing written
```

- [x] **Step 2: Run them** — `cargo test --lib app::` → FAIL.

- [x] **Step 3: Implement.** Pull the add and edit mutations out of `add_inner`/`edit_inner` into private helpers so `transact` and `store.preview` share them. For each target, run inside one closure that receives `entry: Option<&Path>` (`None` for `Saved` after the case-insensitive alias lookup used by `connect`; the staged entry for forms): `resolve_with` (for `user`, `proxy_jump`, `known_hosts_files`), then `probe`; on `Failed(HostKeyUnknown)` call `fetch_host_key` and return `UnknownHostKey` or `Done` with `key_fetch_failure`. `trust_and_retest` resolves again, computes the path with `probe::known_hosts_path(.., home)` (home = `store.paths().ssh_dir.parent()`), calls `append_known_hosts`, then runs the same test.

- [x] **Step 4: Run them** — `cargo test --lib app::` → PASS.

- [x] **Step 5: Commit** — `chore: test saved hosts and form values through App`.

---

### Task 5: `portico test`

**Files:**
- Modify: `src/cli.rs`
- Test: `tests/cli.rs` (fake `ssh` and `ssh-keygen` via `TestEnv::install_program`)

**Interfaces:**
- Consumes: Task 4's `TestTarget::Saved`, `test`, `trust_and_retest`, `TestOutcome`, `TestReport`.
- Produces: `Command::Test { alias: String, #[arg(long = "accept-host-key", value_name = "FINGERPRINT")] accept_host_key: Option<String> }`

Output (exact):
- Success: `{alias}: authenticated as {user}@{address}:{port} ({method})`, or `{alias}: authenticated to {address}:{port} ({method})` when no user resolved. Exit 0.
- Failure: `{alias}: failed — {reason}`, then `hint: {hint}` when there is one; with `key_fetch_failure`, the hint line is `hint: could not fetch the host key: {failure}`. Exit 1.
- Unknown key, before any prompt:
  ```
  {alias}: host key not trusted
  {host} presents an {TYPE} key with fingerprint {fingerprint}
  Compare it with the fingerprint from the server's administrator or console.
  ```
  (one middle line per fingerprint). Interactive: then `Trust this host key? [y/N] ` via the existing `confirm`; `y` → `trust_and_retest` and print its outcome; otherwise print `Host key not trusted.` and exit 1. Non-interactive without the flag: print `hint: pass --accept-host-key <fingerprint> to trust it` and exit 1. With the flag: trust only if every fetched fingerprint equals it, else print `{alias}: failed — the host key fingerprint does not match --accept-host-key` and exit 1. The flag is ignored when the key is already trusted.

- [x] **Step 1: Write the failing tests** in `tests/cli.rs`

```rust
#[test] fn test_reports_success_and_exits_zero() // ssh: -G → "hostname h\nuser deploy\nport 22\n", else success line for 10.0.0.5:22 → stdout contains "dev: authenticated as deploy@10.0.0.5:22 (publickey)"
#[test] fn test_reports_each_failure_with_its_hint_and_exits_one() // auth-refused fixture → status 1, stdout contains "dev: failed — authentication refused (server accepts: publickey)" and "hint: check the user and the key"
#[test] fn test_of_an_unknown_host_is_an_error() // stderr "error: unknown host: nope"
#[test] fn non_interactive_unknown_key_prints_the_fingerprint_and_never_writes() // contains "SHA256:abc" and "hint: pass --accept-host-key SHA256:abc to trust it"; known_hosts absent
#[test] fn a_matching_accept_host_key_trusts_and_retests() // second probe succeeds (fake ssh succeeds once known_hosts exists); known_hosts contains the line; exit 0
#[test] fn a_mismatching_accept_host_key_writes_nothing() // "--accept-host-key SHA256:other" → status 1, "does not match", known_hosts absent
#[test] fn external_hosts_can_be_tested() // host defined in ~/.ssh/config, not managed → exit 0
```

- [x] **Step 2: Run them** — `cargo test --test cli test_` → FAIL.
- [x] **Step 3: Implement the `Test` arm** of `execute`, returning `ExitCode::from(1)` for failures (not `Err`, so the output is not prefixed with `error:`).
- [x] **Step 4: Run them** — `cargo test --test cli` → PASS.
- [x] **Step 5: Commit** — `feat: add portico test`.

---

### Task 6: TUI — the tester worker, keys and results

**Files:**
- Create: `src/tui/tester.rs` (declared from `src/tui.rs` with `mod tester;`)
- Modify: `src/tui.rs`
- Test: unit tests in both files

**Interfaces:**
- Consumes: Task 4.
- Produces (in `tester.rs`):
  - `pub(super) enum Origin { Host(HostSummary), Form }`
  - `pub(super) enum Job { Test(TestTarget), Trust { target: TestTarget, key: PresentedKey } }`
  - `pub(super) struct Finished { pub origin: Origin, pub target: TestTarget, pub outcome: Result<TestOutcome, String> }`
  - `pub(super) struct Tester` with `fn spawn<'scope>(scope: &'scope thread::Scope<'scope, '_>, app: &'scope App) -> Self`, `fn start(&mut self, origin: Origin, job: Job)`, `fn cancel(&mut self)`, `fn running(&self) -> Option<&str>` (alias under test), `fn poll(&mut self) -> Option<Finished>`; dropping it cancels the running test. Same generation and cancel-flag scheme as `Resolver`; a cancelled job sends nothing.
- In `tui.rs`:
  - `UiAction` gains `Test { origin: Origin, target: TestTarget }`, `CancelTest`, `Trust(bool)`.
  - `UiState` gains `testing: Option<String>` (set by the loop from `tester.running()`), `results: TestResults` (keyed by `HostSummary`, value `TestMark { passed: bool, line: String, finished_at: String }`), `trust: Option<PendingTrust { origin: Origin, target: TestTarget, key: PresentedKey, previous: Mode }>`.
  - `FormState` gains `test_result: Option<String>`, cleared by editing like `error`.
  - `Mode` gains `ConfirmTrust` (used by Task 7).
  - `fn local_time() -> String` returning `HH:MM` via `libc::localtime_r`.

Behaviour (copy fixed by the spec):
- Browse: `t` on a selection → `Test { origin: Host(selected), target: Saved(alias) }`. Forms: `Ctrl+T` → `Test { origin: Form, target }` from `form.submission(&mode)` mapped `Add(host) → TestTarget::Add`, `Edit{..} → TestTarget::Edit`; a submission error is shown on its field as on save.
- `Esc` while `testing` is `Some` → `CancelTest` in every mode, before its usual meaning. Status after cancelling: `Test cancelled`.
- In-flight status (when `status` is `None`): `Testing {alias}… (Esc to cancel)`.
- Host results: the list row becomes `✓ {alias}  {ownership}` / `✗ {alias}  {ownership}` (two spaces of padding for untested rows so names stay aligned); the details pane gets `Connection test: {line} at {HH:MM}`; the status line gets `{alias}: {line}`. `line` is `authenticated as {user}@{address}:{port} ({method})` (or `authenticated to …`) or `failed — {reason}`.
- Form results go to `form.test_result` only, and are dropped if the form is no longer open.
- Results are cleared for an alias after a successful edit or removal of it, and entirely on refresh.
- Footers: browse adds `t test` when a host is selected; forms add `Ctrl+T test`.

- [x] **Step 1: Write the failing tests**

```rust
#[test] fn t_starts_a_test_of_the_selected_host_in_browse_mode_only() // browse → Test{Saved("dev")}; filter and forms: 't' is text, UiAction::None
#[test] fn ctrl_t_tests_the_typed_values_in_forms() // add form with alias "new", hostname "h" → Test{ origin: Form, target: Add(host) }; edit form renaming → TestTarget::Edit with changes.alias == Set(..)
#[test] fn ctrl_t_with_an_invalid_form_reports_the_field_error()
#[test] fn esc_cancels_a_running_test_before_leaving_a_form() // testing = Some; Esc → CancelTest, mode still EditForm; second Esc (testing None) → Browse
#[test] fn results_mark_the_host_and_appear_in_details_and_status()
#[test] fn a_result_for_a_closed_form_is_dropped()
#[test] fn results_are_cleared_on_edit_removal_and_refresh()
#[test] fn footers_advertise_the_test_keys()
#[test] fn the_status_shows_the_test_in_flight()
// tester.rs:
#[test] fn a_second_job_supersedes_the_first() // App with a fake ssh that sleeps for the first alias; start(a), start(b); poll yields only b's result
#[test] fn dropping_the_tester_kills_the_running_ssh() // pid file written by fake ssh; drop inside the scope; process gone
```

- [x] **Step 2: Run them** — `cargo test --lib tui::` → FAIL.
- [x] **Step 3: Implement.** Spawn the `Tester` next to the `Resolver` in `browse`, in the same `thread::scope`, and drop both before the scope ends (comment as for the resolver). In `browse_hosts`: poll the tester each loop iteration, set `state.testing`, keep the 25 ms poll interval while it is pending, and route `Finished` to the results, the form or (Task 7) the trust prompt. `UiAction::Test` → `tester.start(origin, Job::Test(target))`; `CancelTest` → `tester.cancel()`.
- [x] **Step 4: Run them** — `cargo test --lib` → PASS.
- [x] **Step 5: Commit** — `feat: test hosts from the TUI`.

---

### Task 7: TUI — trusting a host key, and the PTY run

**Files:**
- Modify: `src/tui.rs`, `tests/support/tui_pty.py`, `tests/acceptance.rs`
- Test: unit tests in `src/tui.rs`; the PTY run in `tests/acceptance.rs`

**Interfaces:**
- Consumes: Task 6's `PendingTrust`, `Mode::ConfirmTrust`, `UiAction::Trust`, `Job::Trust`.

Behaviour:
- `Finished` with `UnknownHostKey { report, key }` sets `state.trust` (remembering the current mode as `previous`) and switches to `Mode::ConfirmTrust`. The right pane, titled `Trust host key`, shows `{host} presents an {TYPE} key with fingerprint {fingerprint}` per fingerprint, then `Compare it with the fingerprint from the server's administrator or console.`, then `Trust it and test again? y/N`.
- `y` → `Trust(true)`: restore `previous`, `tester.start(origin, Job::Trust { target, key })`. `n` or `Esc` → `Trust(false)`: restore `previous`, record the result as `failed — host key not trusted`, status `Host key not trusted`.
- A changed key needs no prompt: it is an ordinary failure whose status line also carries its hint.
- The footer in `ConfirmTrust` reads `y trust and test again  n/Esc keep it untrusted`.

- [x] **Step 1: Write the failing tests**

```rust
#[test] fn an_unknown_key_opens_the_trust_prompt_with_the_fingerprint()
#[test] fn y_trusts_and_retests_and_returns_to_the_previous_mode() // from EditForm → back in EditForm, UiAction::Trust(true)
#[test] fn n_and_esc_decline_and_mark_the_host_untrusted()
#[test] fn a_changed_key_shows_its_warning_without_a_prompt()
```

- [x] **Step 2: Run them** — FAIL. **Step 3: Implement.** **Step 4: Run** `cargo test --lib tui::` — PASS.
- [x] **Step 5: PTY run.** Add `tui_connection_test_runs_on_a_real_pty` to `tests/acceptance.rs` (pattern of `tui_paste_and_terminal_lifecycle_work_on_a_real_pty`) with a fake `ssh` that, for a non-`-G` call, sleeps 1 s and prints the success line. Add a scenario to `tui_pty.py` that presses `t`, waits for `Testing dev… (Esc to cancel)` on the screen, then for `✓ dev`, then quits. Run `cargo test --test acceptance tui_connection_test` → PASS.
- [x] **Step 6: Commit** — `feat: trust a new host key from the TUI`.

---

### Task 8: Real OpenSSH check, docs and roadmap

**Files:**
- Create: `tests/real_openssh.rs`
- Modify: `README.md`, `ROADMAP.md`, `docs/superpowers/specs/2026-10-17-connection-test-design.md` (status line), `tests/docs.rs` if a guard needs the new command

- [x] **Step 1: Write the ignored end-to-end test** `#[ignore] fn portico_test_against_a_real_sshd()`: skip with a printed note when `/usr/bin/sshd` is missing. Start a user `sshd` on a free high port inside the test's temp dir (setup as in Task 1, Step 1). Write the env's `~/.ssh/config` with `Include ~/.ssh/portico/config` and an external `Host real` block using absolute paths (`HostName 127.0.0.1`, `Port`, `IdentityFile`, `IdentitiesOnly yes`, `IdentityAgent none`, `UserKnownHostsFile <env home>/.ssh/known_hosts`). OpenSSH resolves `~` from the password database, not `$HOME`, so install an `ssh` wrapper in the env's `bin` that runs `/usr/bin/ssh -F <env home>/.ssh/real-config "$@"`, where `real-config` is the entry with `~/.ssh/portico/config` replaced by the absolute managed path (as `ordinary_openssh_resolves_a_portico_host` does), and an `ssh-keygen` wrapper that execs `/usr/bin/ssh-keygen`. Assert: `portico test real` without the flag exits 1 and prints a `SHA256:` fingerprint; with `--accept-host-key <that fingerprint>` exits 0 with `authenticated`; the env's `known_hosts` now holds the key and the real user's `~/.ssh/known_hosts` is untouched (compare its bytes, or its absence, before and after); with the authorized key removed it exits 1 with `authentication refused`. Kill the sshd in a drop guard.
- [x] **Step 2: Run it** — `cargo test --test real_openssh -- --ignored` → PASS (locally, as a regular user).
- [x] **Step 3: Docs.** README: the `portico test` command with `--accept-host-key`, the `t` and `Ctrl+T` keys, what a pass means (login succeeds, no shell opened), and that keys with a passphrase must be in an agent. ROADMAP: remove the "Connection test (manual)" slice and renumber (a delivered slice leaves the roadmap); keep "Rich error messages". Spec: mark it implemented.
- [x] **Step 4: Full checks** — `scripts/ci-checks.sh` as a regular user → all green.
- [x] **Step 5: Commit** — `docs: document the connection test`.
- [x] **Step 6: Open the PR** titled `feat: add a manual connection test` (the title release-plz reads), body summarising the feature and how it was verified, with no attribution line.
