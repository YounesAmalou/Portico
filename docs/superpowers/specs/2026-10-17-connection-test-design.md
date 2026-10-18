# Connection Test Design

**Status:** Approved; implemented

## Purpose

Saving a host proves only that OpenSSH can parse it. Whether the host can actually be logged into (right address, port, user, key, jump host and a trusted host key) is found out later, on the first real connection. This slice adds a manual connection test that answers that question on demand, from the CLI and the TUI, including for the values typed into an add or edit form before they are saved. It implemented the connection-test slice of `ROADMAP.md`, which left the roadmap when it shipped, and changes nothing else in the v1 design (`2026-09-13-portico-host-management-design.md`).

A test passes only when OpenSSH completes authentication. A host that answers but refuses the login, or whose host key is not trusted, fails. A test never opens a shell or runs a remote command, so it works the same for hosts that only allow port forwarding (a key restricted to forwarding, a `ForceCommand`, or no shell).

## Decisions

### What a test runs

- One `ssh` process per test, with these options on the command line so they override the user's configuration:

  ```
  ssh -N -n -o BatchMode=yes -o ConnectTimeout=5 -o LogLevel=VERBOSE
      -o ClearAllForwardings=yes -o ControlMaster=no -o ControlPath=none
      -o RemoteCommand=none -o PermitLocalCommand=no
      -o ForwardAgent=no -o ForwardX11=no [-F <staged entry>] -- <alias>
  ```

  - `-N` (no session) and `RemoteCommand=none`: nothing is run on the server, so forwarding-only hosts pass.
  - `BatchMode=yes`: OpenSSH never prompts. A key that needs a typed passphrase cannot be used, exactly as for any non-interactive use; keys in an agent (including 1Password's) work.
  - `ClearAllForwardings=yes`: the host's configured `LocalForward`, `RemoteForward` and `DynamicForward` are not bound, so a test cannot collide with a tunnel already running.
  - `ControlMaster=no`, `ControlPath=none`: a fresh connection every time, so an existing multiplexed session cannot hide a broken configuration.
  - `ForwardAgent=no`, `ForwardX11=no`, `PermitLocalCommand=no`: the test gives the server nothing beyond the login itself and runs nothing locally.
  - `LogLevel=VERBOSE`: makes OpenSSH log `Authenticated to <host> ([<address>]:<port>) using "<method>".` when the login succeeds, and keeps the error lines needed for classification.
- Everything else comes from the user's configuration, including `StrictHostKeyChecking`. A user who set `accept-new` gets the same behaviour as `portico connect`; Portico's trust prompt (below) appears only with OpenSSH's default, `ask`.
- Portico reads stderr line by line as it arrives. The `Authenticated to` line ends the test as a success: Portico terminates the `ssh` process group and reports the user, address, port and method from that line. If `ssh` exits first, its output is classified.
- Deadline: `ConnectTimeout=5` bounds each TCP connection; Portico additionally kills the test after 15 seconds in total (slow jump hosts, servers that stall during key exchange or authentication) and reports it as timed out.
- Cancellation: the test checks a cancel flag like the existing cancellable `ssh -G` calls and kills the process group when it is set.

### Testing a saved host and testing a form

- **Saved host** (CLI, host list, details pane): `ssh … -- <alias>` with the user's normal configuration. Managed and external hosts can both be tested; testing changes nothing. The alias is matched ignoring ASCII case, as `connect` does, and an unknown alias is the usual unknown-host error.
- **Form values** (add and edit forms): the test checks exactly the configuration that saving would activate. The save transaction in `ManagedStore` is split into two steps:
  - `stage`: under the lock, read the managed file, apply the add or edit mutation (with the same validation and alias-collision checks as a save), write the candidate managed file and the validation entry config that includes it, release the lock, and return both temporary files.
  - Saving stages, validates with `ssh -G`, then activates as today; its behaviour is unchanged.
  - Testing stages, runs the test with `-F <staged entry>` and the (possibly renamed) alias, then deletes both files without activating anything.
  - `-F` makes OpenSSH skip the system configuration. For tests only, the staged entry therefore ends with `Match all` followed by `Include /etc/ssh/ssh_config` when that file exists, so system settings still apply after the user's, in OpenSSH's usual order.
  - A form error raised while staging (invalid field, alias collision) is reported on its field, as on save.

### Results

A test ends with one of:

| Result | Recognised by |
|---|---|
| Authenticated (user, address, port, method) | the `Authenticated to … using "…"` line |
| Host key not trusted | `Host key verification failed.` without the changed-key banner; confirmed by fetching the key (below) |
| Host key changed | `REMOTE HOST IDENTIFICATION HAS CHANGED` |
| Authentication refused (with the methods the server accepts) | `Permission denied (<methods>)` |
| Name does not resolve | `Could not resolve hostname` |
| Connection refused | `Connection refused` |
| Unreachable | `No route to host` or `Network is unreachable` |
| Timed out | `Connection timed out`, `Operation timed out`, or Portico's deadline |
| Jump host failed (and which side) | a failure line naming the configured `ProxyJump` host, or `channel … open failed` |
| Failed (raw) | anything else: the last non-empty line `ssh` printed |

- OpenSSH reports a key that needs a passphrase and a wrong key identically (`Permission denied (publickey)`), so there is no separate passphrase result. The "authentication refused" hint says to check the user and key, and that a key with a passphrase must be loaded in an agent.
- Each failure carries a one-line hint where there is a useful next step. Fuller explanations are the "Rich error messages" roadmap slice; this slice keeps one line per result.
- The classifier is a pure function from the collected stderr (and whether the deadline fired) to a result, so it is tested with recorded OpenSSH output.

### Trusting a new host key

Most hosts are unknown the first time they are tested, so Portico offers to trust the key itself instead of sending the user to `connect` and back. The user always sees the fingerprint and decides before anything is written.

1. **Fetch without logging in.** Portico runs `ssh` again with every authentication method off (`PubkeyAuthentication`, `PasswordAuthentication`, `KbdInteractiveAuthentication`, `GSSAPIAuthentication` and `HostbasedAuthentication` set to `no`), `UserKnownHostsFile=<new empty temporary file>` and `StrictHostKeyChecking=accept-new`, plus the test's other options and the same `-F` for a form. OpenSSH connects, records the server's key in the temporary file and fails authentication on purpose; no credential is offered. Because it is real `ssh`, the host's port, `ProxyJump`, `ProxyCommand`, `HostKeyAlias` and `HashKnownHosts` are honoured and the entry is written exactly as OpenSSH would write it. Portico never parses or builds known-hosts lines. If the temporary file stays empty, the result stays "host key not trusted" with the fetch's own failure as the reason.
2. **Show the fingerprint.** `ssh-keygen -l -f <temporary file>` gives the key type and SHA256 fingerprint, shown with the host and port, and a reminder to compare it with a fingerprint obtained from the server's administrator or console.
3. **On confirmation,** Portico appends the temporary file's lines to the first `UserKnownHostsFile` reported by `ssh -G` (with the same `-F` for a form), normally `~/.ssh/known_hosts`. The file is created with mode `0600` if missing, a newline is written first if it does not end with one, and the parent directory must already exist. The test then runs again at once and normally proceeds to authentication. Declining writes nothing.
4. **A changed key is never offered for trust.** It is a hard failure with a warning that it may indicate an attack; resolving it is left to the user.
5. **Jump hosts.** The fetch's options reach only the final hop. If the jump host's own key is unknown, the result is "jump host failed" naming its host-key problem; the user tests the jump host first.

`ssh-keygen` ships with OpenSSH and is run like `ssh`, from `PATH`; no new crate is needed.

### CLI

- `portico test <alias> [--accept-host-key <SHA256:…>]`.
- Success prints one line, for example `dev: authenticated as deploy@10.0.0.5:22 (publickey)`, and exits 0.
- Failure prints `dev: failed — <reason>`, followed by the hint on its own line when there is one, and exits 1.
- Unknown host key, interactive terminal: prints the host, key type and fingerprint and asks `Trust this host key? [y/N]`; on `y` it trusts the key and tests again.
- Unknown host key, non-interactive: never prompts and never trusts. It fails and prints the fingerprint. `--accept-host-key SHA256:…` trusts the key only if the fetched fingerprint matches exactly, then tests again; a mismatch fails without writing anything. There is no option that trusts an unseen key.

### TUI

- **Host list and details pane:** `t` tests the selected host, and the browse footer advertises `t test`.
- **Forms:** `Ctrl+T` tests the values currently typed (a plain `t` is text). The form footer advertises it. The result appears where field errors go today. Saving neither waits for nor triggers a test.
- **In flight:** the status line shows `Testing <alias>… (Esc to cancel)`. Navigation, filtering and the details pane keep working. Only one test runs at a time: starting another cancels the first. `Esc` cancels a running test; in a form, `Esc` cancels the test first and a second `Esc` leaves the form as usual.
- **Result display for saved hosts:** a `✓` or `✗` next to the alias in the list, a "Connection test" line in the details pane with the result and the time it finished, and the outcome on the status line. Results last for the session: a host's result is cleared when it is edited or removed, and every result is cleared on refresh, so a stale mark never outlives the configuration it tested. Form results are not kept after the form closes.
- **Trust prompt:** when a test finds an unknown host key, a confirmation like the remove confirmation shows the host, port, key type and fingerprint with the reminder to compare it, and `y` trusts and re-tests while `n` or `Esc` declines. A changed key shows the warning without a prompt.
- **Worker:** tests run on a worker thread modelled on the existing `ssh -G` `Resolver` (requests carry a generation and a cancel flag, so a superseded result is never mistaken for the current one). It lives in `src/tui/tester.rs`; `tui.rs` keeps the keys, marks, rendering and the prompt.

### Code layout

- `src/probe.rs` (new): result and failure types, hints, ssh argument building for the test and the key fetch, and the classifier. Pure.
- `src/openssh.rs`: running a test (streaming stderr, deadline, cancellation, process-group kill), fetching a key, reading its fingerprint, and appending to the known-hosts file.
- `src/managed.rs`: `stage` extracted from `transact_inner`, and a `preview` that stages, hands the staged entry to a closure and discards it.
- `src/app.rs`: `test(alias)`, `test_add(host)`, `test_edit(alias, changes)`, plus fetching and trusting a host key for a saved host or a form, each with a cancellable variant.
- `src/cli.rs`: the `test` command.
- `src/tui.rs` and `src/tui/tester.rs`: as above.
- `README.md`: the new command and keys. `CHANGELOG.md` is generated from the `feat:` commit by release-plz.

## Out of scope

- Fuller error explanations across connect, save and test (the "Rich error messages" slice).
- Testing whether a particular forward is permitted (the tunnels slice).
- Automatic or background tests (not planned).
- Resolving a changed host key.

## Testing

- **Classifier (unit):** one test per result, fed recorded OpenSSH output stored under `tests/fixtures/ssh/`. The fixtures are captured from a real OpenSSH (10.5p1 at the time of writing) talking to a throwaway `sshd` run as a normal user on a high port: success, unknown key, changed key, refused key, passphrase-protected key, connection refused, unresolvable name, jump host failures, and an unrecognised failure. The `sshd` setup used is recorded in the plan so the fixtures can be regenerated.
- **Process handling (unit, with fake `ssh` scripts as in `tests/support`):** success is detected mid-stream and the process group is terminated; the deadline produces "timed out" and kills it; cancellation kills it and reports cancelled; the exact option list is passed; the key fetch uses an empty temporary known-hosts file and removes it; trusting appends with a separating newline, creates a missing file with mode `0600`, and never writes on a fingerprint mismatch.
- **Staging (unit):** a preview leaves the managed file, the backup and the entry config byte-for-byte unchanged and removes its temporary files, also when the closure fails; the staged entry ends with the system include only when that file exists; saving behaves exactly as before (existing transaction tests keep passing).
- **CLI (integration, `tests/cli.rs`):** output and exit codes for success and each failure kind; an unknown host; non-interactive runs never prompt; `--accept-host-key` with a matching and a mismatching fingerprint.
- **TUI (unit):** `t` starts a test only in browse mode, `Ctrl+T` only in forms; `Esc` cancels before it leaves a form; a second test supersedes the first; marks appear, and are cleared on edit, removal and refresh; the trust prompt accepts with `y` and declines with `n` and `Esc`; the footers advertise the keys.
- **PTY:** one run on a real terminal where `t` on a host shows the in-flight status and then the result, with a fake `ssh`.
- **Real OpenSSH (ignored by default):** an integration test that starts a throwaway `sshd` and runs `portico test` end to end for success, an unknown key (trusted through `--accept-host-key`) and a refused key. It runs with `cargo test -- --ignored` where `sshd` is installed; running it in CI belongs to the end-to-end roadmap item.
