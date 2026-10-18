# Rich Error Messages Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every `ssh` failure in Portico (connection test, connect, save validation) explains its cause, the facts behind it and the next step, identically in the CLI and the TUI.

**Architecture:** `probe::classify` grows from a nine-way result into a richer `Failure` with key facts read from `LogLevel=DEBUG1` output. A new pure `config_error` module maps `ssh -G` error lines to a managed host and field or the user's own file. A new pure `explain` module turns either into an `Explanation` (headline, labelled facts, next steps, raw lines) and renders it as text lines, which the CLI prints and the TUI shows. Connecting pipes ssh's stderr through a copier thread that mirrors it live and keeps a tail for classification.

**Tech Stack:** Rust (edition and MSRV as in `Cargo.toml`), clap, ratatui/crossterm, OpenSSH 10.x. No new crates.

**Spec:** `docs/superpowers/specs/2026-10-18-rich-error-messages-design.md`

## Global Constraints

- No new dependencies.
- Portico never edits `known_hosts`, loads keys or rewrites the user's configuration on its own; it only explains and suggests. The existing confirmed trust flow is unchanged.
- Connect's exit code is always ssh's exit code. Only exit 255 can get an explanation, and only for a recognised cause.
- Mirrored stderr bytes reach the terminal unchanged and in order, partial lines included.
- Tail kept while connecting: newest 64 KiB. Stderr kept while testing: newest 256 KiB.
- Paths under `$HOME` are displayed with `~`.
- The test runs with `LogLevel=DEBUG1`; every other test option is unchanged.
- Commit messages follow Conventional Commits and carry no AI attribution or co-author trailer (`CLAUDE.md`). Run `scripts/ci-checks.sh` before each commit, as a regular user.
- Fixtures are captured in the session scratchpad, never inside the repository, and only the captured stderr is copied in.

## Review Focus

1. A `debug1:` line that names the jump host (for example `Setting implicit ProxyCommand from ProxyJump: …`) must not turn a target failure into "jump host failed". Pinned in Task 1.
2. A partial line with no newline (a prompt-like `Password:` written to stderr) must appear on the terminal before ssh writes anything else. Pinned in Task 6.
3. An established session that drops (exit 255 with `Broken pipe` or `closed by remote host`) must say "connection lost", never "the server refused the login". Pinned in Tasks 2 and 6.
4. A save error in an included file whose path has spaces, or lies outside `$HOME`, must name that file correctly; only `$HOME` paths get `~`. Pinned in Task 3.
5. When the `ssh -G` that follows a failed connect itself fails for a reason other than a rejected configuration, connect must still return ssh's exit code and print no explanation rather than an error. Pinned in Task 6.

---

### Task 1: DEBUG1 test runs and re-captured fixtures

**Files:**
- Modify: `src/probe.rs` (`base_args`, `classify_lines`, `classify`, tests)
- Replace: every file under `tests/fixtures/ssh/`; add new ones (below); update `tests/fixtures/ssh/VERSION`

**Interfaces:**
- Produces: `probe::test_args` with `LogLevel=DEBUG1`; `fn is_debug(line: &str) -> bool` (private) used by all later classification; the fixture set later tasks test against.

- [ ] **Step 1: Capture fixtures.** Follow Task 1 Step 1 of `docs/superpowers/plans/2026-10-17-connection-test.md`, with two changes: the client uses `-o LogLevel=DEBUG1` instead of `VERBOSE`, and the server runs as `/usr/bin/sshd -d -f sshd_config` (one connection per start, restarted before each capture; in the Claude sandbox the daemon mode resets every connection, `-d` works). Re-capture every existing fixture under its current name, then add:
  - `passphrase-key.txt` (re-captured; it now holds `Server accepts key:` for the passphrase key)
  - `wrong-user.txt`: good key, `nobodyhere@127.0.0.1`
  - `missing-identity.txt`: `-o IdentitiesOnly=yes -i <scratch>/doesnotexist`
  - `password-only.txt`: a second `sshd_config` with `PubkeyAuthentication no`, `PasswordAuthentication yes`
  - `host-key-changed.txt` (re-captured; must contain an `Offending … key in <file>:<line>` line)
  - `jump-target-key-refused.txt`: two `sshd -d` (ports 22999 and 22997, same host key, both in the known-hosts file) and an `-F` client config with `Host jump` (port 22999, good key) and `Host target` (port 22997, `ProxyJump jump`, wrong key), each with `IdentitiesOnly yes`, `IdentityAgent none`, `UserKnownHostsFile`
  - `connection-lost.txt`: `ssh -N` with the good key in the background, then kill the `sshd -d` session process after one second; it must end with `Connection to 127.0.0.1 closed by remote host.`

  Replace the scratchpad path in every fixture with `/tmp/portico-fixture` (`sed`), so fixtures do not depend on the machine. Write the version from `ssh -V` into `VERSION`.

- [ ] **Step 2: Update the argument test and run the suite.** In `test_arguments_are_exact` change `"LogLevel=VERBOSE"` to `"LogLevel=DEBUG1"`. Add:

```rust
#[test]
fn debug_lines_never_name_the_jump_host() {
    let text = "debug1: Setting implicit ProxyCommand from ProxyJump: ssh -l u -p 22 -W '[%h]:%p' bastion\n\
                user@target: Permission denied (publickey).\n";
    assert!(matches!(classify(text, false, Some("bastion")), Failure::AuthRefused { .. }));
}
```

Run: `cargo test --lib probe`
Expected: FAIL (`LogLevel` mismatch, the new test, and any fixture test the debug lines now confuse).

- [ ] **Step 3: Implement.** In `base_args` use `LogLevel=DEBUG1`. Add `is_debug(line)` (`line.starts_with("debug")`) and drop such lines before `classify_lines` and before the jump host `named`/`marked` checks in `classify` (the key facts in Task 2 read them separately). Every existing test must pass against the re-captured fixtures without changing its expected values.

- [ ] **Step 4: Run** `cargo test --lib probe` — Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -m "test: capture the ssh fixtures at debug level"` (fixtures, `probe.rs`).

---

### Task 2: A richer diagnosis

**Files:**
- Modify: `src/probe.rs`, `src/openssh.rs` (stderr cap), `src/app.rs`, `src/cli.rs`, `src/tui.rs` (only the call sites that match on the changed variants)

**Interfaces:**
- Produces (in `probe`):

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeySource { File, Agent }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfferedKey { pub path: String, pub fingerprint: String, pub source: KeySource }

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Failure {
    HostKeyUnknown,
    HostKeyChanged { known_hosts: Option<(String, u32)> },
    AuthRefused { methods: String, offered: Vec<OfferedKey>, unusable: Vec<String> },
    NameUnresolved { host: String },
    ConnectionRefused,
    Unreachable,
    TimedOut { deadline: bool },
    JumpHostFailed { jump: String, cause: Box<Failure> },
    ConfigRejected(crate::config_error::ConfigRejection),
    ConnectionLost { line: String },
    Other(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnosis { pub failure: Failure, pub raw: Vec<String> }
pub fn diagnose(stderr: &str, deadline_reached: bool, route: Route<'_>) -> Diagnosis;
pub const TEST_STDERR_LIMIT: usize = 256 * 1024;
```

  `classify(stderr, deadline_reached, jump) -> Failure` stays, now `diagnose(..).failure` for callers that only need the failure. `Diagnosis::raw` is every non-empty stderr line that is not a `debug` line. `Failure::hint()` is deleted; `Display` stays and gives the short reason used on one line (`jump host bastion failed: connection refused`, `host key changed`, `timed out`, …). `ConfigRejected` is added in Task 3; until then omit the variant.

- [ ] **Step 1: Write the failing tests** in `probe::tests`:

```rust
#[test]
fn offered_keys_and_their_source_are_read_from_debug_lines() {
    let Failure::AuthRefused { offered, unusable, methods } =
        classify(&fixture("auth-refused"), false, None) else { panic!() };
    assert_eq!(methods, "publickey");
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0].source, KeySource::File);
    assert!(offered[0].fingerprint.starts_with("SHA256:"));
    assert!(unusable.is_empty());
    let line = "debug1: Offering public key: me@laptop ED25519 SHA256:abc agent";
    assert_eq!(offered_key(line).unwrap().source, KeySource::Agent);
}

#[test]
fn a_key_the_server_accepts_but_ssh_cannot_use_is_unusable() {
    let Failure::AuthRefused { unusable, .. } =
        classify(&fixture("passphrase-key"), false, None) else { panic!() };
    assert_eq!(unusable, ["/tmp/portico-fixture/locked"]);
}

#[test]
fn password_only_and_missing_identity_are_refusals_with_their_facts() {
    assert!(matches!(classify(&fixture("password-only"), false, None),
        Failure::AuthRefused { ref methods, .. } if !methods.contains("publickey")));
    assert!(matches!(classify(&fixture("missing-identity"), false, None),
        Failure::AuthRefused { ref offered, .. } if offered.is_empty()));
}

#[test]
fn a_changed_key_names_the_offending_known_hosts_line() {
    let Failure::HostKeyChanged { known_hosts: Some((file, line)) } =
        classify(&fixture("host-key-changed"), false, None) else { panic!() };
    assert!(file.ends_with("kh") && line >= 1);
}

#[test]
fn a_jump_failure_carries_the_jump_hosts_own_cause() {
    let jump = Route::Proxied { jump: Some("jump"), hostname: "127.0.0.1", port: 22997 };
    let failure = diagnose(&fixture("jump-target-key-refused"), false, jump).failure;
    // The jump host logged in; the target refused the key, so this is the target's refusal,
    // and only keys offered after the jump's own `Authenticated to` line are listed.
    let Failure::AuthRefused { offered, .. } = failure else { panic!("{failure:?}") };
    assert!(offered.iter().all(|key| key.path.ends_with("wrong")));
    assert_eq!(
        classify(&fixture("jump-connection-refused"), false, Some("127.0.0.1:22998")),
        Failure::JumpHostFailed { jump: "127.0.0.1:22998".into(), cause: Box::new(Failure::ConnectionRefused) }
    );
}

#[test]
fn a_dropped_session_is_lost_not_refused() {
    assert!(matches!(classify(&fixture("connection-lost"), false, None), Failure::ConnectionLost { .. }));
    for line in ["client_loop: send disconnect: Broken pipe",
                 "Timeout, server 10.0.0.1 not responding."] {
        assert!(matches!(classify(line, false, None), Failure::ConnectionLost { .. }), "{line}");
    }
}

#[test]
fn raw_lines_leave_out_debug_output() {
    let raw = diagnose(&fixture("auth-refused"), false, Route::Direct).raw;
    assert!(raw.iter().all(|line| !line.starts_with("debug")));
    assert!(raw.last().unwrap().contains("Permission denied"));
}
```

  Update the existing tests to the new shapes (`JumpHostFailed { cause }`, `TimedOut { deadline }`, `HostKeyChanged { .. }`, `AuthRefused { .. }` via `matches!`) and delete `reasons_and_hints_match_the_spec`'s hint assertions; keep its `Display` assertions.

- [ ] **Step 2: Run** `cargo test --lib probe` — Expected: compile FAIL.

- [ ] **Step 3: Implement** in `src/probe.rs`:
  - `fn offered_key(line: &str) -> Option<OfferedKey>` parses `Offering public key: <path> <type> <fingerprint> <flags…>`; `source` is `Agent` when the flags contain `agent`, otherwise `File`.
  - `unusable`: paths from `Server accepts key: <path> …` lines when no `Authenticated to` line follows (the passphrase case).
  - On `Route::Proxied`, key facts come only from lines after the last direct-form `Authenticated to … ([addr]:port)` line; if there is none, key facts belong to the jump host's cause.
  - `known_hosts` from `Offending <type> key in <file>:<line>`.
  - `ConnectionLost` is recognised from `Broken pipe`, `closed by remote host`, `not responding` — checked after the pre-login causes so a refused login is never mistaken for it.
  - `TimedOut { deadline }` is `true` only when Portico's deadline fired.
  - In `openssh.rs` `watch_ssh`, keep only the newest `TEST_STDERR_LIMIT` bytes (drop whole lines from the front).
  - Fix the call sites in `app.rs`, `cli.rs` and `tui.rs` mechanically: replace `failure.hint()` with `None` for now (Tasks 5 and 7 render explanations) and match the new variant shapes.

- [ ] **Step 4: Run** `cargo test` — Expected: PASS (CLI tests asserting a `hint:` line are updated in Task 5; mark them `#[ignore = "rendered in Task 5"]` here only if they fail, and remove the ignore in Task 5).

- [ ] **Step 5: Commit** — `git commit -m "refactor: read offered keys and causes from debug output"`

---

### Task 3: Mapping configuration errors

**Files:**
- Create: `src/config_error.rs` (register in `src/lib.rs`)
- Modify: `src/probe.rs` (add `Failure::ConfigRejected`)

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigRejection {
    /// A line Portico wrote, mapped to the host and field it belongs to.
    Managed { alias: String, field: Option<HostField>, directive: String, message: String },
    /// A line in the user's own configuration.
    UserFile { path: PathBuf, line: u32, message: String },
    /// The validation entry Portico wrote; never expected.
    Internal { raw: String },
    /// Output in no recognised shape.
    Unrecognised { raw: String },
}

pub struct ConfigFiles<'a> { pub managed: &'a Path, pub managed_text: &'a str, pub entry: &'a Path }

pub fn interpret(stderr: &str, files: &ConfigFiles<'_>) -> ConfigRejection;
```

  `Display` for `ConfigRejection` gives one line: `Port for dev: OpenSSH rejects '99999'` (Managed: `<Directive> for <alias>: <message>`), `line 12 of ~/.ssh/config: Bad configuration option: bogus` (UserFile, `~` applied through `$HOME`), the raw text otherwise.

- [ ] **Step 1: Write the failing tests** in `config_error::tests`:

```rust
const MANAGED: &str = "Host dev\n  HostName 10.0.0.5\n  Port 99999\n";

fn files<'a>(managed: &'a Path, entry: &'a Path) -> ConfigFiles<'a> {
    ConfigFiles { managed, managed_text: MANAGED, entry }
}

#[test]
fn a_managed_line_names_its_host_and_field() {
    let (m, e) = (Path::new("/tmp/stage.m"), Path::new("/tmp/stage.e"));
    let rejection = interpret("/tmp/stage.m line 3: Bad port '99999'.\n/tmp/stage.m: terminating, 1 bad configuration options\n", &files(m, e));
    assert_eq!(rejection, ConfigRejection::Managed {
        alias: "dev".into(), field: Some(HostField::Port), directive: "Port".into(),
        message: "Bad port '99999'.".into() });
}

#[test]
fn both_line_forms_and_paths_with_spaces_are_read() {
    let (m, e) = (Path::new("/tmp/m"), Path::new("/tmp/e"));
    for (text, path) in [
        ("/home/u/my configs/extra.conf: line 7: Bad configuration option: bogus\n", "/home/u/my configs/extra.conf"),
        ("/etc/ssh/ssh_config.d/x.conf line 7: Bad configuration option: bogus\n", "/etc/ssh/ssh_config.d/x.conf"),
    ] {
        assert_eq!(interpret(text, &files(m, e)), ConfigRejection::UserFile {
            path: path.into(), line: 7, message: "Bad configuration option: bogus".into() }, "{text}");
    }
}

#[test]
fn the_entry_wrapper_is_internal_and_anything_else_unrecognised() {
    let (m, e) = (Path::new("/tmp/m"), Path::new("/tmp/e"));
    assert!(matches!(interpret("/tmp/e line 2: Bad thing\n", &files(m, e)), ConfigRejection::Internal { .. }));
    assert!(matches!(interpret("something odd\n", &files(m, e)), ConfigRejection::Unrecognised { .. }));
}

#[test]
fn only_home_paths_are_shortened() {
    let rejection = ConfigRejection::UserFile { path: "/etc/ssh/ssh_config".into(), line: 1, message: "x".into() };
    assert_eq!(rejection.display_with_home(Path::new("/home/u")), "line 1 of /etc/ssh/ssh_config: x");
    let rejection = ConfigRejection::UserFile { path: "/home/u/.ssh/config".into(), line: 12, message: "x".into() };
    assert_eq!(rejection.display_with_home(Path::new("/home/u")), "line 12 of ~/.ssh/config: x");
}
```

- [ ] **Step 2: Run** `cargo test --lib config_error` — Expected: compile FAIL.

- [ ] **Step 3: Implement** `interpret`: take the first line matching `<path>: line <n>: <msg>` or `<path> line <n>: <msg>` (split on the last ` line ` / `: line ` before `<n>:`, so paths with spaces survive), skipping `terminating, N bad configuration options`. Map by path equality: `managed` → walk `managed_text` to the `Host` block containing line `n` and the directive on it; `field` from the directive name, ignoring ASCII case (`host`→`Alias`, `hostname`→`Hostname`, `user`→`User`, `port`→`Port`, `identityfile`→`IdentityFile`, `proxyjump`→`ProxyJump`, else `None`). Add `fn display_with_home(&self, home: &Path) -> String`; `Display` uses `$HOME` from the environment. Add `Failure::ConfigRejected(ConfigRejection)` to `probe`.

- [ ] **Step 4: Run** `cargo test --lib config_error` — Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat: map OpenSSH configuration errors to the host and field"`

---

### Task 4: Structured validation errors when saving

**Files:**
- Modify: `src/managed.rs` (`TransactionError::Validation`, `InitError::Validation`, `transact_inner`, `apply_init`, `preview`), `src/app.rs` (validate closures, `run_test`), `src/tui.rs` (`report_error`, `may_have_changed_inventory`)
- Test: `src/managed.rs` tests, `src/tui.rs` tests, `tests/cli.rs`

**Interfaces:**
- Consumes: `config_error::{interpret, ConfigFiles, ConfigRejection}`.
- Produces: `TransactionError::Validation(ConfigRejection)`, `InitError::Validation(ConfigRejection)`. The `validate` closure type stays `Fn(&Path, &[String]) -> Result<(), String>`, where the `String` is now ssh's raw stderr (`OpenSshError::Failed { stderr, .. }` → `stderr`; any other error → its `to_string()`, which interprets as `Unrecognised`). `preview`'s `use_entry` closure receives `ConfigFiles<'_>` instead of `&Path` (its `entry` is the staged entry), so a form test can map config errors too.
- Error text: `TransactionError::Validation` displays `configuration validation failed: {rejection}; nothing was saved` followed, for `UserFile`, by `; fix that line, then save again`. `InitError::Validation` keeps its current recovery text but uses `{rejection}` and ends `then rerun portico init`.

- [ ] **Step 1: Write the failing tests.**
  - `managed.rs`: `a_rejected_managed_line_reports_its_host_and_field` — a `transact` that inserts host `dev` with port 22, and whose `validate` closure reads the staged entry it is given, takes the candidate path from its `Include <path>` line, finds the line number of `Port 22` in that candidate, and returns `Err(format!("{candidate} line {n}: Bad port '22'."))`; assert `Err(TransactionError::Validation(ConfigRejection::Managed { field: Some(HostField::Port), .. }))`. Adapt the existing `Validation("ssh failed")` test to `Validation(ConfigRejection::Unrecognised { raw })` with `raw == "ssh failed"`.
  - `tui.rs`: `a_rejected_field_is_focused_in_the_form` — in `Mode::EditForm`, `report_error(&AppError::Transaction(TransactionError::Validation(ConfigRejection::Managed { alias: "dev".into(), field: Some(HostField::Port), directive: "Port".into(), message: "Bad port '99999'.".into() })))` sets `form.active` to the Port row and `form.error` to `Some("Port for dev: Bad port '99999'.")`.
  - `tests/cli.rs`: `a_save_rejected_by_the_users_own_config_names_that_file` — an env whose `~/.ssh/config` has `Bogus yes` on line 3 below the Include; `portico add x -H h` exits 1 and stderr contains `line 3 of ~/.ssh/config: Bad configuration option: bogus` and `nothing was saved`.

- [ ] **Step 2: Run** `cargo test` — Expected: compile FAIL.

- [ ] **Step 3: Implement.** In `transact_inner`, map the closure's `Err(stderr)` with `interpret(&stderr, &ConfigFiles { managed: candidate.path(), managed_text: <candidate bytes as UTF-8>, entry: validation_file.path() })` before the staged files are dropped; in `apply_init`, with the real managed and entry paths. In `tui::report_error`, when in a form and the rejection is `Managed { field: Some(field), .. }`, focus that row (as `report_form_error` does) and set `form.error` to the rejection's `Display`.

- [ ] **Step 4: Run** `cargo test` — Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat: say which host, field or file OpenSSH rejected when saving"`

---

### Task 5: Explanations, and the test rendering them in the CLI

**Files:**
- Create: `src/explain.rs` (register in `src/lib.rs`)
- Modify: `src/app.rs` (`TestReport`, `test_entry`), `src/cli.rs` (`print_report`, `Command::Test { raw }`)
- Test: `src/explain.rs` tests, `tests/cli.rs`

**Interfaces:**
- Consumes: `probe::{Failure, Diagnosis, OfferedKey, KeySource}`, `config_error::ConfigRejection`.
- Produces:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation { Test, Connect, Save }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Label { User, Address, Offered, Key, Name, Methods, File, Jump, Line, Next }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Explanation {
    pub headline: String,
    pub facts: Vec<(Label, Vec<String>)>,
    pub next: Vec<String>,
    pub raw: Vec<String>,
}

pub struct Context<'a> {
    pub alias: &'a str,
    pub operation: Operation,
    pub user: Option<&'a str>,
    pub hostname: Option<&'a str>,
    pub port: Option<u16>,
    /// Each configured identity file and whether it exists.
    pub identity_files: &'a [(String, bool)],
    /// Whether the jump host is an alias Portico knows, so `portico test <jump>` works.
    pub jump_is_known: bool,
    pub home: &'a Path,
}

pub fn explain(diagnosis: &Diagnosis, context: &Context<'_>) -> Explanation;

impl Explanation {
    /// `<alias>: failed — <headline>` then the aligned labelled lines (label column padded to
    /// the longest label plus one space, continuation lines indented to the value column), then
    /// the raw lines under `  ssh said:` when `raw` is true.
    pub fn render(&self, alias: &str, raw: bool) -> Vec<String>;
}
```

  `TestReport` gains `pub explanation: Option<Explanation>` (`Some` exactly when the result is `Failed`), built in `test_entry` from the `Diagnosis` and the resolved config (`identity_files` existence checked with `Path::exists` after `~/` expansion against the store's home). When `resolve_with` fails with `OpenSshError::Failed { stderr, .. }` whose `interpret` (with the preview's `ConfigFiles` for a form, or the real managed file and entry for a saved host) is not `Unrecognised`, `test_entry` returns a `Done` report with `ProbeResult::Failed(Failure::ConfigRejected(rejection))` instead of an error. `TestReport::summary()` stays the one-line form. `key_fetch_failure` stays and, when present, is added as an extra `next` line: `could not fetch the host key: {failure}`.

- [ ] **Step 1: Write the failing tests.** One golden test per cause in `explain::tests`, each building a `Diagnosis` from a fixture (or by hand where noted) and asserting the full `render("dev", false)` output. The headlines and next steps are fixed copy:

| Cause | Headline | Next |
|---|---|---|
| unusable key | `the key needs a passphrase` | ``load it with `ssh-add <path>` `` (`~` applied) |
| no key offered | `no key was offered` | `set IdentityFile for <alias>, or load a key in an agent`; a missing file adds fact `key: <path> (does not exist)` |
| refused | `the server refused the login` | `check User and IdentityFile for <alias>, and that the public key is in the server's authorized_keys` |
| password only | `the server accepts only a password` | test: `` the test cannot type a password; `portico connect <alias>` will ask for it ``; connect: none |
| name | `the host name does not resolve` | `check HostName for <alias>`; when `hostname == alias`, fact `name: <alias> (no HostName is set, so ssh looked up the alias)` |
| refused port | `nothing accepted the connection` | `check the port, and that an SSH server runs there` |
| unreachable | `the host is unreachable` | `check the address and your network` |
| timeout | `the host did not answer in time` | `check the address, the port and any firewall`; fact `line: no answer within 5 s` or, for the deadline, `the test gave up after 15 s` |
| jump | `jump host <jump> failed: <cause's headline>` | the cause's facts under `jump:`; `` `portico test <jump>` `` first when `jump_is_known` |
| key unknown | `the host key is not trusted` | test: none (the trust prompt follows); connect: `` the key was declined; review it with `portico test <alias>` `` |
| key changed | `the host key changed, which can mean an attack` | fact `file: <known_hosts>:<line>`; `` once the new key is confirmed with the server's administrator, remove the old one with `ssh-keygen -R <hostname>` `` |
| config | `OpenSSH rejected the configuration` | fact `file:` / `line:` from the rejection; `fix that line, then try again` |
| lost | `the connection was lost` | none |
| other | the raw last line | none; `render` always appends the raw lines for this cause |

  Example golden (refused, from `auth-refused`):

```rust
assert_eq!(explain(&diagnosis, &context).render("dev", false), [
    "dev: failed — the server refused the login",
    "  user:    user@127.0.0.1:22999",
    "  offered: /tmp/portico-fixture/wrong (file)",
    "  next:    check User and IdentityFile for dev, and that the public key is in the",
    "           server's authorized_keys",
]);
```

  Wrap `next` and fact values at 72 columns on word boundaries. When the operation is `Connect` and the cause is a refusal with no offered keys known (the default log level), the facts say `offered: not reported at this log level` and `next` adds `` run `portico test <alias>` to see which keys were offered ``.

  In `tests/cli.rs`: update the existing failure tests to the new layout; add `test_raw_prints_what_ssh_said` (`portico test x --raw` with a fake ssh printing `user@h: Permission denied (publickey).` shows `  ssh said:` and that line).

- [ ] **Step 2: Run** `cargo test` — Expected: compile FAIL.

- [ ] **Step 3: Implement** `explain`, `render`, the `TestReport` field, `--raw` (`#[arg(long)] raw: bool` on `Test`), and `print_report` printing `report.summary()` for a pass and `explanation.render(alias, raw)` for a failure. Remove any `#[ignore]` added in Task 2.

- [ ] **Step 4: Run** `cargo test` — Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat: explain connection test failures with their facts and next step"`

---

### Task 6: Mirrored stderr and explanations when connecting

**Files:**
- Modify: `src/openssh.rs` (`connect`, `connect_cancellable`), `src/app.rs` (`connect`, `connect_cancellable`), `src/cli.rs` (`execute` gets the error writer; the `Connect` arm)
- Test: `src/openssh.rs` tests, `tests/cli.rs`

**Interfaces:**
- Produces:

```rust
pub const CONNECT_TAIL_LIMIT: usize = 64 * 1024;

pub struct Mirrored { pub status: ExitStatus, pub tail: Vec<u8> }

impl OpenSsh {
    /// Runs `ssh -- <alias>` with stdin and stdout inherited and stderr piped through a copier
    /// thread that writes every chunk to `sink` as soon as it is read and keeps the newest
    /// `CONNECT_TAIL_LIMIT` bytes.
    pub(crate) fn connect_mirrored(
        &self, alias: &str, sink: Box<dyn Write + Send>, cancelled: Option<&AtomicBool>,
    ) -> Result<Mirrored, OpenSshError>;
}

pub enum ConnectionOutcome {
    Exited { status: ExitStatus, explanation: Option<Explanation> },
    LaunchRequested,
    LaunchFailed(ExitStatus),
}
```

  `App::connect` and `App::connect_cancellable` both return `ConnectionOutcome` and use `connect_mirrored` with `io::stderr()`. On `status.code() == Some(255)` they run `resolve_with(None, alias, None)`. If that fails with `OpenSshError::Failed { stderr, .. }` that `interpret` (real managed file and entry) maps to anything but `Unrecognised`, the explanation is that config rejection; any other resolve failure gives `None`. Otherwise they `diagnose` the lossy-UTF-8 tail with the resolved route and explain it with `Operation::Connect`; `Failure::Other` gives `None`. Every other status gives `None`.

- [ ] **Step 1: Write the failing tests** in `openssh::tests`, with fake `ssh` scripts and a shared `Arc<Mutex<Vec<u8>>>` sink:
  - `mirrored_bytes_arrive_unchanged_and_in_order` — the script runs `printf 'Password:'; sleep 0.3; printf '\377\r\nbye\n' >&2; exit 0` (the first `printf` to stderr too); assert the sink holds `Password:` before the script exits (poll the sink while the child runs) and finally equals the exact bytes, including `\xff` and `\r\n`.
  - `the_tail_keeps_the_newest_bytes` — the script writes 100 KiB of `a` then `END\n`; `tail.len() == CONNECT_TAIL_LIMIT` and it ends with `END\n`.
  - `the_exit_status_passes_through` — `exit 3` gives code 3.
  In `tests/cli.rs`:
  - `connect_explains_a_refused_login` — a fake `ssh` that, for `-G`, prints a resolved config and, otherwise, writes `user@h: Permission denied (publickey).` to stderr and exits 255; `portico connect x` exits 255 and stderr holds the ssh line, a blank line, then `x: failed — the server refused the login`.
  - `connect_adds_nothing_for_other_exit_codes_or_unknown_output` — exit 1 with any output, and exit 255 with `weird thing`: stderr is exactly what ssh wrote.
  - `connect_keeps_the_exit_code_when_resolving_fails` — exit 255 with `Permission denied (publickey).`, and `-G` exits 1 printing `oops`: Portico exits 255 and prints no explanation and no `error:`.
  - `connect_explains_a_rejected_configuration` — exit 255, and `-G` exits 255 printing `<env home>/.ssh/config line 3: Bad configuration option: bogus`: the explanation headline is `OpenSSH rejected the configuration` with `line 3 of ~/.ssh/config`.
  - `a_lost_session_says_so` — exit 255 with `Connection to h closed by remote host.`: explanation headline `the connection was lost`.

- [ ] **Step 2: Run** `cargo test` — Expected: compile FAIL.

- [ ] **Step 3: Implement.** The copier reads into an 8 KiB buffer and writes and flushes each chunk before the next read; the tail is a `VecDeque<u8>` trimmed from the front. Join the thread after the child exits. Keep the cancellation and process-tree handling of the current `connect_cancellable`. The CLI writes the explanation to its error writer: an empty line, then `render(alias, false)`, and exits with ssh's code as today. In `tui.rs`, adapt the status line to `ConnectionOutcome::Exited { status, explanation }` (`explanation.headline` when present, otherwise today's text); the details section is Task 7.

- [ ] **Step 4: Run** `cargo test` — Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat: explain why a connection failed, keeping ssh's own output"`

---

### Task 7: Explanations in the TUI

**Files:**
- Modify: `src/tui.rs` (`TestMark`, `FormTestResult`, `show_result`, test result handling, connect handling, details rendering, form rendering, key handling, footer)
- Test: `src/tui.rs` tests

**Interfaces:**
- Consumes: `TestReport::explanation`, `ConnectionOutcome::Exited { explanation }`, `Explanation::render`.
- Produces: `TestMark` and `FormTestResult` gain `explanation: Option<Explanation>` (the `hint` field goes); `UiState` gains `connections`: the last connection explanation per host, kept, matched and cleared exactly like `results` (edit, removal, refresh), and `show_raw: bool`, toggled by `o`, reset when the selection changes or a form opens or closes.

- [ ] **Step 1: Write the failing tests:**
  - `a_failed_test_shows_its_explanation_in_the_details` — after a `Done` report with an explanation, the details text contains `Connection test: failed — the server refused the login at <time>` followed by the rendered fact and `next` lines (without the `dev: failed — ` prefix line).
  - `o_unfolds_the_ssh_output_and_the_footer_offers_it_only_when_there_is_some` — the footer contains `o ssh output` only when the selected host's test or connection explanation has raw lines; after `o` the details contain `ssh said:` and the raw line; a second `o` hides it; moving the selection hides it.
  - `a_failed_connection_adds_a_last_connection_section` — after the connect handling records `Exited { status: 255, explanation: Some(..) }`, the details contain `Last connection: failed — <headline>`; the status line is the headline; edit, removal and refresh clear it (one assertion each, mirroring the existing test-mark clearing tests).
  - `a_form_test_failure_shows_its_explanation` — the form area shows `Connection test: failed — <headline>` and the next lines; `o` inside a form is text, so in a form the raw output is unfolded with `Ctrl+O` instead, and the form footer advertises `Ctrl+O ssh output` only when there is raw output.
  Update the existing tests that asserted hint lines.

- [ ] **Step 2: Run** `cargo test --lib tui` — Expected: FAIL.

- [ ] **Step 3: Implement** as specified; the status line for a failed test keeps today's `<alias>: failed — <reason>` form, using the headline.

- [ ] **Step 4: Run** `cargo test --lib tui` — Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat: show failure explanations and ssh output in the TUI"`

---

### Task 8: Explained failures in a new terminal

**Files:**
- Modify: `src/openssh.rs` (`OpenSsh` gains `portico: Option<PathBuf>`, `launch_in_new_terminal`), `src/cli.rs` (`Command::Connect { hold }`), `src/main.rs` if `OpenSsh::system()` is built there
- Test: `src/openssh.rs` tests, `tests/cli.rs`

**Interfaces:**
- Produces: `OpenSsh::with_portico(self, path: Option<PathBuf>) -> Self`; `OpenSsh::system()` sets it from `std::env::current_exe().ok()`. `Connect` gains `#[arg(long, hide = true)] hold: bool`.

- [ ] **Step 1: Write the failing tests:**
  - `openssh`: `a_new_terminal_runs_portico_when_its_path_is_known` — a fake terminal script records its arguments; with `with_portico(Some("/opt/portico"))` they are exactly `/opt/portico connect --hold -- dev`; with `None` they are `<ssh> -- dev` as today.
  - `tests/cli.rs`: `hold_waits_for_enter_only_after_an_explained_failure` — `portico connect x --hold` with the refusing fake ssh and stdin `\n`: stderr ends with `Press Enter to close`, and the process exits 255 after reading the line; with an `exit 0` fake ssh and empty stdin it exits 0 without printing the prompt; with `exit 255` and output `weird thing` it does not prompt.

- [ ] **Step 2: Run** `cargo test` — Expected: FAIL.

- [ ] **Step 3: Implement.** When `hold` is set and the outcome carries an explanation, write `Press Enter to close` to the error writer and read one line from `input` (EOF also ends the wait).

- [ ] **Step 4: Run** `cargo test` — Expected: PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat: keep a new terminal open to explain a failed connection"`

---

### Task 9: Real OpenSSH check and documentation

**Files:**
- Modify: `tests/real_openssh.rs`, `README.md`, `ROADMAP.md`

- [ ] **Step 1: Extend the ignored end-to-end test.** Add a passphrase-protected key (`-N pw`) to `authorized_keys` and a second `Host locked` block using it; assert `portico test locked` exits 1 and prints `the key needs a passphrase` and `` `ssh-add `` with the key's path. With the authorized key removed (existing step), assert the output contains `offered:` and the key's path.

- [ ] **Step 2: Run** `cargo test --test real_openssh -- --ignored` — Expected: PASS (needs `/usr/bin/sshd`; outside the Claude sandbox, or with the sandbox disabled for this command).

- [ ] **Step 3: Documentation.** In `README.md`: replace the test section's "prints the reason" sentence with the new layout and `--raw`; add a short "When something fails" section after the test section covering: test, connect (ssh's output unchanged, then an explanation for a recognised cause; new terminals stay open on such a failure until Enter), save (names the field, or the line of your own file), `o` / `Ctrl+O` in the TUI; add `o` to the key table. In `ROADMAP.md` remove slice 1 and renumber.

- [ ] **Step 4: Run** `scripts/ci-checks.sh` — Expected: every check passes (including `tests/docs.rs`).

- [ ] **Step 5: Commit** — `git commit -m "docs: describe how Portico explains failures"`
