# End-to-End Tests Design

**Status:** Approved; implemented

## Purpose

Portico's tests run the real binary, but the OpenSSH side of almost every test is a fake `ssh` script, and the terminal tests drive the TUI through a Python harness with a hand-written screen parser. The one test against a real `sshd` (`tests/real_openssh.rs`) is opt-in and never runs in CI. So a change in how OpenSSH behaves, or in how Portico talks to it, can pass every check.

This slice adds an end-to-end suite that runs the real `portico` binary in a real pseudo-terminal against a real, unprivileged `sshd` with throwaway keys and a throwaway home, and runs it in CI as its own job. It implements the "Automated end-to-end tests with a real OpenSSH and a real terminal in CI" item of `ROADMAP.md`, and it is the safety net the macOS and Windows ports will rely on. Product code does not change.

## Decisions

### Where the server runs

- Each test starts its own `/usr/bin/sshd` as the current user, listening on a random port on `127.0.0.1`, with every file in a temporary directory: host key, `authorized_keys`, `sshd_config` (`StrictModes no`, `UsePAM no`, `PasswordAuthentication no` unless a test needs otherwise, `LogLevel ERROR`), PID file. An unprivileged `sshd` can only log in as the user running it, which is all the suite needs.
- No Docker: CI jobs already run inside an Arch Linux container as an unprivileged user, the container already installs `openssh`, and GitHub's macOS runners have no Docker. The same suite therefore runs unchanged locally and in CI.
- Interactive sessions never reach the user's login shell: `sshd_config` sets `ForceCommand` to a script in the temporary directory that prints a banner (`portico-e2e ready`), reads one line, prints `portico-e2e got <line>` and exits. `portico test` is unaffected because a test never opens a session.
- The server is killed when its fixture is dropped, however the test ends. The port is chosen by binding port 0 and releasing it; if `sshd` fails to listen (the port was taken in between), the fixture retries with a new port, up to three times.

### Isolating the client

OpenSSH resolves `~` from the password database rather than `$HOME`, so a temporary `$HOME` alone would still read and write the real `~/.ssh`. Each test gets:

- A temporary home, used as `HOME` for Portico, holding `~/.ssh/config` and `~/.ssh/portico/config` as Portico writes them.
- A wrapper `ssh` first on `PATH`. When Portico passes no `-F` (connect, test, resolving a saved host), the wrapper writes a flattened copy of `$HOME/.ssh/config` with every `~/` replaced by `$HOME/` (so the `Include` of the managed file points into the temporary home), appends a final `Host *` block (`UserKnownHostsFile $HOME/.ssh/known_hosts`, `GlobalKnownHostsFile /dev/null`, `IdentityAgent none`, `IdentitiesOnly yes`), and runs `/usr/bin/ssh -F <flattened> "$@"`. Options go in the config rather than on the command line so that Portico's own `-o` options, such as the temporary `UserKnownHostsFile` used to fetch a host key, still take precedence. When Portico passes `-F <entry>` itself (validating a candidate), the wrapper runs `/usr/bin/ssh` unchanged.
- A wrapper `ssh-keygen` that runs the real one, so tests use the real tool through the same `PATH`.
- Hosts are added with absolute `IdentityFile` paths, never `~`.
- Every test ends by asserting that the real user's `~/.ssh/known_hosts` is byte-for-byte unchanged.

### Driving the terminal

- A `Terminal` helper in the test crate spawns the binary in a pseudo-terminal with `portable-pty` (Linux, macOS and Windows ConPTY, so the ports can reuse it) at 120×24, and feeds everything it prints to a `vt100` parser that holds the screen as a real terminal would.
- A reader thread moves output into the parser; the test thread waits on it with deadlines. Helpers:
  - `type_text(&str)`, `key(Key)` (Enter, Tab, Esc, Up/Down, Ctrl+C), `paste(&str)` (bracketed paste);
  - `wait_for(text, timeout)`: waits until `text` is on the screen; on timeout it fails with the full screen contents and the last raw bytes;
  - `wait_gone(text, timeout)`, `screen() -> String`, `wait_exit(timeout) -> ExitStatus`.
- crossterm asks the terminal for its keyboard protocol and device attributes at start-up. The helper answers that query like a terminal without the kitty protocol (device attributes only), which is the common case; the kitty-protocol paths stay covered by the existing Python harness.
- Default deadline 10 s per wait, generous for a loaded CI runner; a passing test never waits that long.
- The child is killed when the helper is dropped.

### Flows covered

TUI, each a single test from a fresh home:

1. **First run, add and connect.** Start `portico` in an uninitialized home; accept the init prompt with `y`. Add a host through the form: alias, host name `127.0.0.1`, the current user, the server's port, the absolute key path; save and see it listed. Press Enter: the TUI leaves the alternate screen, `ssh` asks whether to trust the unknown host key, the test types `yes`, sees the banner, types a line and sees it echoed. The session ends and the TUI is back with the host selected. The managed config holds the host block, and the temporary `known_hosts` holds the server's key.
2. **Test, edit and remove.** Hosts are created with the CLI first. `t` on a host whose key is not trusted yet shows the untrusted-key explanation; trusting it from the TUI's review then `t` shows the success mark. `e` changes the port to a closed one and saves; `t` shows the explanation for a refused connection. `x` and confirming remove the host from the screen and from the managed config.

CLI:

3. **`portico connect`** in the pseudo-terminal against a trusted host: banner, echoed line, exit status 0.
4. **`portico test`** (moved from `tests/real_openssh.rs`, which is deleted): an unknown host key is reported with its fingerprint and nothing is trusted; `--accept-host-key <fingerprint>` trusts it and the login passes; a login through a jump host (`ProxyJump`) is recognised well before the test deadline; a key with a passphrase and no agent says so and names the `ssh-add` command; a key the server no longer accepts is a refused login listing the offered key.
5. **Error explanations** with `portico test`, each from its real cause:
   - host key changed: the server's host key is replaced after it was trusted;
   - refused connection: a port nothing listens on;
   - password-only server: a second `sshd` with `PubkeyAuthentication no` and `PasswordAuthentication yes`;
   - failing jump host: a `ProxyJump` through a host on a closed port names the jump host and its cause.

Unresolvable names and timeouts depend on DNS and the network, so they stay covered by the unit tests only. Launching a host in a new terminal window needs a terminal emulator and stays covered by the fake launcher in `tests/acceptance.rs`.

### Running the suite

- The suite is one test target, `tests/e2e.rs`, with its modules under `tests/e2e/` (`sshd.rs`, `client.rs`, `terminal.rs`). Every test is `#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]`, so `cargo test --all-targets` (and the `test` job) compiles the suite but does not run it.
- `scripts/ci-checks.sh e2e` runs `cargo test --test e2e -- --ignored`. Locally it needs `/usr/bin/sshd` and runs as a regular user; there is no skip: a missing `sshd` fails the test with a message saying what to install, so CI cannot pass without running the suite.
- New dev-dependencies: `portable-pty` and `vt100`. Both must pass `cargo deny` with the existing license list; if one does not, the plan picks an equivalent before writing tests on it.

### CI

- A new `e2e` job in `.github/workflows/ci.yml`, a copy of the `test` job's steps (container, packages, toolchain, checkout, build cache) ending in `scripts/ci-container.sh e2e`. It runs when the change touches anything but documentation (`needs.changes.outputs.rust != 'false'`).
- It runs in parallel with the other jobs. Target: the workflow's wall-clock stays where it is (about a minute), because the suite itself takes a few seconds and the job's build is about as long as the `test` job's.
- `CONTRIBUTING.md` lists the new job and how to run the suite locally.
- Making `e2e` a required check is a repository setting the maintainer changes after the job has proved stable; this slice does not change it.

## Testing the tests

- Each flow is first run against a deliberately broken expectation (a wrong banner, a wrong explanation text) to see it fail with a readable screen dump, then fixed.
- The suite is run several times in a row locally and once in CI before merging, to catch timing flakiness.

## Out of scope

- macOS and Windows runners (their own designs; the harness is chosen so they can reuse it).
- Replacing the Python PTY harness and its fake-`ssh` scenarios, which test terminal lifecycle details (signals, kitty keyboard flags, process cleanup) this suite does not repeat.
- Password and keyboard-interactive logins that actually succeed (an unprivileged `sshd` without PAM cannot check passwords).
