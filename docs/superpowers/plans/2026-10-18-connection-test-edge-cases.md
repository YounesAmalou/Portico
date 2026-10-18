# Connection Test Edge Cases Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `portico test` finds the target's login line through a multi-hop jump and through a fd-passing `ProxyCommand`, and a late signal never cancels the retest after trusting a key.

**Architecture:** `probe::authenticated` gains a host check and a fd-passing line form, fed by a new `ResolvedConfig::proxy_use_fdpass` read from `ssh -G`. `cli::Interrupts::during` resets the cancel flag per step and reports a signal that landed after a step finished; the `test` command stops before the trust prompt on one.

**Tech Stack:** Rust, `signal-hook`, fake `ssh` scripts (`crate::test_executable::write_executable`), recorded OpenSSH fixtures.

**Spec:** `docs/superpowers/specs/2026-10-18-connection-test-edge-cases-design.md`

## Global Constraints

- Run the suite as a regular user, never root; `scripts/ci-checks.sh` must pass before each commit.
- Conventional Commits; no AI attribution lines anywhere.
- Fixtures are verbatim OpenSSH 10.6p1 stderr: `tests/fixtures/ssh/authenticated-via-two-jumps.txt` and `tests/fixtures/ssh/authenticated-via-fdpass.txt` (already captured; the target is `127.0.0.3`, port `22999`, hop `b` is `127.0.0.2`).
- Do not edit `ROADMAP.md`.

## Review Focus

1. A target `HostName` typed in mixed case (`Db.Example`) against a lowercase line: must still succeed. Test in Task 1.
2. A multi-hop run where the target fails after hop `b` logged success: must be a failure, not a success, and hop `b`'s line is not lost from the raw output. Test in Task 1.
3. A fd-passing route that receives a `(via proxy)` line (a verbose inner hop): must not count as success. Test in Task 2.
4. `ssh -G` output without a `proxyusefdpass` line (older OpenSSH, fakes): `fd_passing` is false. Test in Task 2.
5. A late signal after a first test that ended in an ordinary result (success or failure, no key prompt): the result prints with its usual exit code, not `cancelled`. Test in Task 3.

---

### Task 1: The target's line names the target's host

**Files:**
- Modify: `src/probe.rs` (`authenticated`, its doc comment, tests near `a_proxied_success_is_recognised_only_through_a_proxy`)
- Modify: `src/openssh.rs` tests (`a_proxied_probe_stops_at_the_targets_line_not_the_jump_hosts`)
- Add: `tests/fixtures/ssh/authenticated-via-two-jumps.txt`

**Interfaces:**
- Produces: `authenticated(line: &str, route: Route<'_>) -> Option<Authenticated>` (signature unchanged); on `Route::Proxied { hostname, .. }` it returns `None` unless the line's host equals `hostname` ignoring ASCII case.

- [x] **Step 1: Write the failing tests** in `src/probe.rs`:

```rust
#[test]
fn a_later_hops_proxied_login_is_not_the_targets() {
    let route = Route::Proxied { jump: Some("hopa,hopb"), hostname: "127.0.0.3", port: 22999 };
    let text = fixture("authenticated-via-two-jumps");
    let hop = text.lines().find(|l| l.contains("127.0.0.2 (via proxy)")).unwrap();
    assert_eq!(authenticated(hop, route), None);
    let success = text.lines().find_map(|l| authenticated(l, route)).unwrap();
    assert_eq!(success.host, "127.0.0.3");
    assert_eq!(success.via.as_deref(), Some("hopa,hopb"));
}

#[test]
fn the_targets_host_is_matched_ignoring_case() {
    let route = Route::Proxied { jump: None, hostname: "Db.Example", port: 22 };
    assert!(authenticated("Authenticated to db.example (via proxy) using \"publickey\".", route).is_some());
}
```

In `src/openssh.rs`, add `a_multi_hop_probe_waits_for_the_targets_line`: `fake_hanging_ssh(Some("authenticated-via-two-jumps"))`, route as above; the result is `ProbeResult::Authenticated` with `host == "127.0.0.3"`, and the recorded pid dies. Add `a_hops_success_before_the_targets_failure_is_a_failure`: `fake_failure` printing `Authenticated to 127.0.0.2 (via proxy) using "publickey".\n` then the `auth-refused` fixture's lines; route hostname `127.0.0.3`; the result is `ProbeResult::Failed(_)` and the raw output contains `127.0.0.2 (via proxy)`.

- [x] **Step 2: Run them and confirm they fail**

Run: `cargo test a_later_hops_proxied_login the_targets_host_is_matched a_multi_hop_probe a_hops_success_before`
Expected: the hop line is accepted (`Some(..)` / wrong host `127.0.0.2`).

- [x] **Step 3: Implement.** In the `Route::Proxied` arm, after splitting off the host, return `None` unless `host.eq_ignore_ascii_case(hostname)`. Extend the doc comment with the known limit from the spec (a later verbose hop with the target's `HostName`). Update the existing tests whose `hostname` does not match their fixture's host: `a_proxied_success_is_recognised_only_through_a_proxy` and `a_proxied_probe_stops_at_the_targets_line_not_the_jump_hosts` use `hostname: "127.0.0.1"` (and the expected `address` follows).

- [x] **Step 4: Run** `cargo test` — all pass.

- [x] **Step 5: Commit**

```bash
git add src/probe.rs src/openssh.rs tests/fixtures/ssh/authenticated-via-two-jumps.txt
git commit -m "fix: tell a later jump host's login from the target's"
```

### Task 2: Accept the direct line form with `ProxyUseFdpass`

**Files:**
- Modify: `src/probe.rs` (`Route::Proxied`, `authenticated`, `classify`)
- Modify: `src/openssh.rs` (`ResolvedConfig`, its parser, `route()`, tests)
- Modify: `src/tui.rs:2677` test fixture (new field)
- Add: `tests/fixtures/ssh/authenticated-via-fdpass.txt`

**Interfaces:**
- Consumes: Task 1's host check.
- Produces: `Route::Proxied { jump: Option<&str>, hostname: &str, port: u16, fd_passing: bool }`; `ResolvedConfig { proxy_use_fdpass: bool, .. }`. `route()` sets `fd_passing` from `proxy_use_fdpass` only (OpenSSH already reports `no` under `ProxyJump`). `classify` builds its route with `fd_passing: false`.

- [x] **Step 1: Write the failing tests.** In `src/probe.rs`, `a_fd_passing_proxy_logs_the_direct_form`:

```rust
let route = Route::Proxied { jump: None, hostname: "127.0.0.3", port: 2222, fd_passing: true };
let success = fixture("authenticated-via-fdpass").lines().find_map(|l| authenticated(l, route)).unwrap();
assert_eq!(success, Authenticated { host: "127.0.0.3".into(), address: "127.0.0.3".into(),
    port: 22999, method: "publickey".into(), via: Some("proxy".into()) });
assert_eq!(authenticated("Authenticated to 127.0.0.3 (via proxy) using \"publickey\".", route), None);
let plain = Route::Proxied { fd_passing: false, ..route };   // or spell the fields out
assert_eq!(fixture("authenticated-via-fdpass").lines().find_map(|l| authenticated(l, plain)), None);
```

In `src/openssh.rs`, extend `a_proxy_command_or_none_decides_the_route`: `"hostname h\nproxycommand nc %h %p\nproxyusefdpass yes\n"` gives `fd_passing: true`; the existing outputs (no `proxyusefdpass` line) give `fd_passing: false`. Add `a_fd_passing_probe_succeeds_instead_of_timing_out`: `fake_hanging_ssh(Some("authenticated-via-fdpass"))` with the fd-passing route returns `Authenticated` within 2 s.

- [x] **Step 2: Run them and confirm they fail** — `cargo test` does not compile (no `fd_passing`), which is the expected failure.

- [x] **Step 3: Implement.** Add the field and parse `proxyusefdpass` (`yes`, ASCII case ignored, is true). In `authenticated`, the line form is chosen by `Route::Direct | Route::Proxied { fd_passing: true, .. }` → direct form, otherwise `(via proxy)` form; the fd-passing case takes address and port from the line, keeps `via` from `jump.unwrap_or("proxy")`, and applies Task 1's host check. Add `fd_passing: false` to every other `Route::Proxied` literal and `proxy_use_fdpass: false` to the `tui.rs` fixture.

- [x] **Step 4: Run** `cargo test` — all pass.

- [x] **Step 5: Commit**

```bash
git add src/probe.rs src/openssh.rs src/tui.rs tests/fixtures/ssh/authenticated-via-fdpass.txt
git commit -m "fix: recognise a login through a fd-passing ProxyCommand"
```

### Task 3: A late signal never cancels the next step

**Files:**
- Modify: `src/cli.rs` (`Interrupts`, the `Command::Test` arm, tests)

**Interfaces:**
- Produces:
  - `enum Step<T> { Done(T), Late(T) }` — `Late` when the flag was set once the step returned and the step itself was not cancelled.
  - `Interrupts::during<T>(&self, step: impl FnOnce(&AtomicBool) -> T) -> Step<T>`: stores `false` in `cancelled` before the step, reads and clears it after restoring the default action.
  - `#[cfg(test)] fn Interrupts::signalled_after(step: usize) -> Interrupts`: no handlers; `during` sets the flag just after the step with that 1-based index returns, as a signal in the window would. Backed by a `#[cfg(test)]` counter field.
  - `fn test_command<I: BufRead, O: Write>(app: &App, alias: String, accept_host_key: Option<String>, raw: bool, input: &mut I, output: &mut O, interactive: bool, interrupts: &Interrupts) -> Result<ExitCode, String>`: the current `Command::Test` arm moved out of `execute`, which now installs `Interrupts` and calls it.

- [x] **Step 1: Write the failing tests** in `src/cli.rs` `mod tests`:

```rust
#[test]
fn each_step_starts_with_a_clear_flag_and_a_late_flag_is_reported() {
    let interrupts = Interrupts::install(false).unwrap();
    interrupts.cancelled.store(true, Ordering::SeqCst);
    assert!(matches!(interrupts.during(|c| c.load(Ordering::SeqCst)), Step::Done(false)));
    assert!(matches!(interrupts.during(|c| c.store(true, Ordering::SeqCst)), Step::Late(())));
    assert!(matches!(interrupts.during(|c| c.load(Ordering::SeqCst)), Step::Done(false)));
}
```

`a_late_signal_after_an_unknown_key_cancels_before_the_prompt`: a test app whose fake `ssh` reports an unknown key (as `unknown_key_env` in `tests/cli.rs`: `-G` prints `hostname h`, `user deploy`, `port 22`, `userknownhostsfile <home>/.ssh/known_hosts`; a key fetch writes a line to the `UserKnownHostsFile=` it is given and exits 255; a probe prints the `host-key-unknown` fixture and exits 255) and fake `ssh-keygen` printing `256 SHA256:abc h (ED25519)`; `test_command(.., interactive: true, &Interrupts::signalled_after(1))` with input `y\n` returns exit code 130; the output contains `SHA256:abc` and ends with `dev: cancelled\n`, does not contain `Trust this host key?`, and `known_hosts` does not exist.

`a_late_signal_after_the_retest_keeps_its_result`: same app, but the probe prints `Authenticated to h ([10.0.0.5]:22) using "publickey".` once `known_hosts` exists; `--accept-host-key SHA256:abc` with `Interrupts::signalled_after(2)` exits 0 and prints `authenticated`.

`a_late_signal_after_an_ordinary_result_keeps_it`: probe prints the success line from the start; `signalled_after(1)` exits 0 with `authenticated`.

- [x] **Step 2: Run them and confirm they fail** — compile errors (`Step`, `test_command`, `signalled_after`).

- [x] **Step 3: Implement.** In `test_command`: a first step that is `Late(Ok(TestOutcome::UnknownHostKey { .. }))` prints the untrusted key as today, then calls `print_cancelled`; any other `Late(x)` is handled as `Done(x)`. The retest treats `Late` as `Done`. Update the `Interrupts` doc comment to say a signal after a step ends is acted on at the next step.

- [x] **Step 4: Run** `cargo test` — all pass, including `interrupting_a_test_stops_ssh_and_exits_130` in `tests/cli.rs`.

- [x] **Step 5: Run** `scripts/ci-checks.sh` as the regular user — passes.

- [x] **Step 6: Commit**

```bash
git add src/cli.rs
git commit -m "fix: keep a late signal from cancelling the retest after trusting a key"
```
