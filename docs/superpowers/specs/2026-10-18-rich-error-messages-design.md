# Rich Error Messages Design

**Status:** Approved; implemented

## Purpose

When `ssh` fails, Portico today passes on a terse reason: the connection test has nine one-line results with a short hint, connecting reports only `ssh exited with 255` (and the TUI redraws over ssh's own output), and saving passes on `ssh -G`'s stderr, which names a temporary file. This slice makes every failure explain itself in plain words: what went wrong, the facts behind it (which user, which keys were offered, which file and line), and the next step. The same explanations appear in the CLI and the TUI, for the connection test, connecting, and OpenSSH validation when saving. The raw `ssh` output stays available for anything Portico does not recognise.

It implements the "Rich error messages" slice of `ROADMAP.md` and builds on the connection test (`2026-10-17-connection-test-design.md`), whose behaviour it keeps except where stated here.

## Decisions

### Explanations

A new pure module, `src/explain.rs`, turns a diagnosis into an explanation. It owns all the wording, so the three operations and the two interfaces never phrase the same cause differently.

```rust
pub struct Explanation {
    pub headline: String,                  // "the server refused the login"
    pub facts: Vec<(Label, Vec<String>)>,  // user:, offered:, file:, jump: …
    pub next: Vec<String>,                 // concrete steps; commands in backticks
    pub raw: Vec<String>,                  // the ssh lines it was derived from
}
```

Its input is the `Diagnosis` produced by `probe::classify` plus context Portico already has: the alias, the operation (test, connect or save), and, for test and connect, the host as `ssh -G` resolves it (user, host name, port, `ProxyJump`/`ProxyCommand`, identity files and whether each exists). `Failure::hint()` is removed; its role moves here.

### What is recognised

| Cause | Recognised from | Facts, then next step |
|---|---|---|
| Key needs a passphrase | `Server accepts key: <path> …` followed by no login | the key → `ssh-add <path>` (a test cannot type a passphrase) |
| No key offered | no `Offering public key` line while the server accepts `publickey` | the identity files tried and which do not exist → set IdentityFile, or load a key in an agent |
| Login refused | `Permission denied (<methods>)` after keys were offered | `user@host:port`; each key offered, with its source (file or agent) → check User, and that the public key is in the server's `authorized_keys` |
| Password-only server | `Permission denied (<methods>)` where the methods do not include `publickey` | the methods → the test cannot type a password; `portico connect` will prompt for it |
| Name does not resolve | `Could not resolve hostname <name>` | the name; when no HostName is configured, that ssh looked up the alias itself → check HostName |
| Connection refused | `Connection refused` | address and port → check the port, and that an SSH server runs there |
| Unreachable | `No route to host`, `Network is unreachable` | address → check the address and your network |
| Timed out | `Connection timed out`, `Operation timed out`, or Portico's deadline | address and port; which limit fired (the 5 s connect timeout or the 15 s test deadline) → check the address, port and firewall |
| Jump host failed | as today | the jump host and its own diagnosis, explained by the same rules → `portico test <jump>` when the jump is a known alias |
| Host key unknown | as today | test: the trust prompt, unchanged; connect: that the key was declined → `portico test <alias>` to review and trust it |
| Host key changed | the `REMOTE HOST IDENTIFICATION HAS CHANGED` banner and `Offending … key in <file>:<line>` | known-hosts file and line, and that this can mean an attack → once the new key is confirmed with the server's administrator, `ssh-keygen -R <host>`; Portico never runs it |
| Config rejected | `<file> line <n>: <message>` (or `<file>: line <n>: …`) | the managed host and field, or the user's own file and line (see Saving) |
| Connection lost | after a session, `Broken pipe`, `Connection … closed by remote host`, `Timeout, server … not responding` | the line → connect only; no further step beyond reconnecting |
| Other | anything else | the raw lines, always shown |

- A wrong user and a wrong key are indistinguishable to OpenSSH, so "login refused" always names the user it tried.
- Key facts need OpenSSH's `DEBUG1` log level (see the connection test below); at the default level, as when connecting, OpenSSH names neither the offered keys nor an accepted but unusable key. An explanation built without them says so and points to `portico test <alias>` for the full breakdown.
- The classifier stays a pure function of the collected stderr, the route and whether the deadline fired.

### Layout

The CLI prints a failure as a headline followed by aligned, labelled lines:

```
dev: failed — the server refused the login
  user:    deploy@10.0.0.5:22
  offered: ~/.ssh/id_ed25519 (agent)
           ~/.ssh/work (needs a passphrase; not in an agent)
  next:    load the key with `ssh-add ~/.ssh/work`,
           or check User and IdentityFile for dev
```

- Paths under `$HOME` are shown with `~`.
- Raw output is shown below the explanation for "Other" always, and for every other cause on request: `portico test --raw` in the CLI, and in the TUI a folded "ssh output" section that `o` unfolds in the details pane and in a form's result area.
- In the TUI the status line carries the headline; the details pane and the form's result area carry the facts and next steps.

### Connection test

- `LogLevel=VERBOSE` becomes `LogLevel=DEBUG1`. Every other option, the success line, the deadline, cancellation and the trust flow are unchanged.
- OpenSSH does not pass `-o LogLevel=DEBUG1` on to the `ssh` it runs for a `ProxyJump` hop (verified in the `jump-target-key-refused` fixture), so the debug key lines on a proxied route are the target's. Only when the user has configured the jump host itself verbose does the hop's `ssh` also log a direct-form `Authenticated to <host> ([<address>]:<port>)` line; key facts are then taken only after that line. The absence of that line never by itself means "jump host failed".
- The stderr kept for classification is capped at 256 KiB, keeping the newest lines.
- The CLI keeps the success line and exit codes; a failure prints the layout above. `--raw` adds the ssh lines.

### Connecting

**In the current terminal** (`portico connect`, `Enter` in the TUI):

- `ssh` starts as today with stdin and stdout on the terminal; only stderr is piped. A copier thread writes every byte to Portico's stderr as soon as it is read, so partial lines and the `\r\n` ssh writes in raw mode pass through unchanged. Host-key and password prompts are unaffected: ssh reads them from `/dev/tty`. The thread keeps the newest 64 KiB for diagnosis.
- Portico's exit code is always ssh's.
- An exit code other than 255 (a remote command's status, or a normal logout) never gets an explanation.
- On exit 255 Portico resolves the host with `ssh -G` (only after a failure, so connecting is never slowed) and classifies the captured output. A recognised cause prints a blank line and the explanation; "Other" prints nothing extra, because ssh's output is already on screen.
- In the TUI, the status line shows the headline, and the details pane gains a "Last connection" section with the explanation and the folded raw output. It is cleared when the host is edited or removed and on refresh, like test results.

**In a new terminal** (`-n`, `Ctrl+Enter`, `n`):

- Portico launches `xdg-terminal-exec <portico> connect --hold <alias>`, where `<portico>` is the running executable (`std::env::current_exe`), instead of bare `ssh`. `--hold` is a hidden flag.
- In that window the connection behaves as in the current terminal. On a recognised failure it also prints `Press Enter to close` and waits; on success or a normal logout the window closes as today.
- If the executable's path cannot be determined, Portico launches bare `ssh` as today.
- Nothing new is reported back to the CLI or TUI that launched it: "launch requested" remains the only signal there.

### Saving

Applies to add, edit and `portico init`, and to a test or connect that fails because OpenSSH refuses the configuration.

- `TransactionError::Validation` and `InitError::Validation` carry ssh's stderr plus the paths of the staged managed file and validation entry, instead of a string.
- A new pure module, `src/config_error.rs`, parses each `<file> line <n>: <message>` line:
  - **The staged managed file:** Portico reads the staged content, finds the `Host` block and directive at that line, and names the host and field, for example `Port for dev: OpenSSH rejects '99999'`. In the TUI the error appears on that field of the form.
  - **The staged validation entry:** the entry is a copy of the user's `~/.ssh/config` with Portico's Include line swapped one line for one line (and, for a test, `Match all` and an Include of the system configuration appended). A rejected line Portico wrote there (the swapped Include or an appended line) is reported as an internal Portico error with the raw text. Any other line is the user's own: it is reported against the real `~/.ssh/config` at the same line number.
  - **Any other file:** the user's own configuration, for example `line 12 of ~/.ssh/config (not written by Portico): Bad configuration option: bogus`, with the next step `nothing was saved; fix that line, then save again` (for `init`: `then rerun portico init`).
  - `ConfigFiles.entry` is Portico's staged validation wrapper only. `portico init`, saved-host tests and `connect` treat the user's entry file (`~/.ssh/config`) as the user's own file.
- Output that does not parse falls back to today's message with the raw lines.

## Code layout

- `src/probe.rs`: `classify` returns the richer `Diagnosis` (offered keys and their source, accepted but unusable keys, password-only methods, the known-hosts file and line, the jump host's own diagnosis, the rejected config line, connection lost). Line parsing stays here, pure.
- `src/explain.rs` (new, pure): `Diagnosis` plus context to `Explanation`.
- `src/config_error.rs` (new, pure): `ssh -G` error lines to a managed host and field or a user file and line.
- `src/openssh.rs`: the stderr-mirroring connect (returning the exit status and the captured tail), the `--hold` launch command, and `LogLevel=DEBUG1` for tests.
- `src/managed.rs`: the structured validation errors.
- `src/app.rs`: connect outcomes and test results carry an `Explanation` when there is one.
- `src/cli.rs`: one renderer for the layout, `test --raw`, and the hidden `connect --hold`.
- `src/tui.rs`: explanations in the details pane and forms, the "Last connection" section, field-attached save errors, and the `o` toggle (advertised in the footer when there is raw output to show).
- `README.md`: a short "When something fails" section; `ROADMAP.md`: slice 1 removed. `CHANGELOG.md` comes from the `feat:` commit.

## Out of scope

- The three connection-test edge cases (roadmap slice 3).
- Explaining failures of a session in a new terminal back to the Portico that launched it.
- Changing anything for the user: Portico explains and suggests, and never edits `known_hosts`, loads keys or rewrites the user's configuration on its own (trusting an unknown key stays the existing, confirmed flow).
- Distinguishing a wrong user from a wrong key, which OpenSSH does not reveal.

## Testing

- **Classifier and explanations (unit):** every fixture under `tests/fixtures/ssh/` is re-captured at `DEBUG1` from a real OpenSSH talking to a throwaway `sshd`, run as a normal user on a high port. New fixtures: a passphrase key, a wrong user, a missing IdentityFile, a password-only server, a refused key through a jump host, a changed key with its `Offending` line, and a lost connection. One golden test per cause checks the exact rendered text. The `sshd` setup is recorded in the plan so fixtures can be regenerated.
- **Config errors (unit):** a line in the staged managed file maps to its host and field; another file is reported as the user's; a user line of the entry wrapper maps to the real `~/.ssh/config` and Portico's own lines there are internal; unparseable output falls back.
- **Connect mirroring (unit, fake `ssh` scripts):** bytes arrive in order and unchanged, partial lines included; exit 255 with a recognised cause prints the explanation; other exit codes and "Other" print nothing extra; the exit code passes through; the tail is capped.
- **`--hold` (unit):** waits for Enter only after a recognised failure; the launch uses the current executable and falls back to `ssh` when it is unavailable.
- **CLI (integration, `tests/cli.rs`):** the test failure layout and `--raw`; connect explanations; save errors naming the field or the user's file.
- **TUI (unit):** a save error lands on its field; the "Last connection" section appears and is cleared on edit, removal and refresh; `o` folds and unfolds the raw output and the footer advertises it only when there is some.
- **Real OpenSSH (ignored by default):** `tests/real_openssh.rs` adds a passphrase key and a wrong key and checks the `offered` and `ssh-add` lines.
