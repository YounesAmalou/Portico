# Tunnels Design

**Status:** Approved; implemented

## Purpose

Opening a port forward today means remembering an `ssh -N -L 5432:localhost:5432 prod-db` line, keeping a terminal open for it, and finding out by trial why it did not start. This slice lets Portico save forwards as named tunnels, list them with their status, and start and stop them on demand, from the CLI and from a tunnels screen in the TUI. A started tunnel is an ordinary `ssh` process that outlives Portico; Portico finds it again later from a small runtime record, and never kills a process it cannot prove is that tunnel.

It implements v3 item 1 of `ROADMAP.md`, "Tunnels (saved forwards, run on demand)", and the "Tunnels" follow-up of the v1 design: local, remote and dynamic forwards, local conflict detection, and supervision of explicitly started tunnels. The status model is shaped so that the next v3 slice (`--json`) can expose it unchanged.

## Settled questions

- **One forward per tunnel.** A tunnel is a name, a host and one forward. Its status, its conflicts and its start or stop are then about one port, and the form stays small. Several forwards through one host are several tunnels (several `ssh` processes), which can be started and stopped one by one. Grouping them is not part of this slice (decided in review).
- **TCP forwards only.** Local (`-L`), remote (`-R`) and dynamic (`-D`) forwards of TCP ports. Unix-socket forwards (`-L /path:host:port`) and remote dynamic forwards (`-R port` without a destination) are rejected: the first has no port to check for conflicts and neither was asked for.
- **Tunnel definitions live in their own Portico-owned file,** `~/.ssh/portico/tunnels`, beside the managed config but never `Include`d by OpenSSH. Putting `LocalForward` into a managed `Host` block would open the forward on every `ssh <alias>`, which breaks "nothing starts automatically" and the meaning of `ssh <alias>`. The file is plain text in an `ssh_config`-like syntax (below), so no new dependency is needed and a reader recognises it.
- **Runtime state is one small record per started tunnel** under `$XDG_RUNTIME_DIR/portico/tunnels/` (a per-user tmpfs, emptied at logout and reboot, which is exactly the lifetime of the processes it describes), or `~/.ssh/portico/run/tunnels/` when `XDG_RUNTIME_DIR` is unset or not a usable directory. No daemon and no database: the record is written when a tunnel starts and removed when it is stopped.
- **A tunnel is found by verified identity, never by PID alone.** The record holds the PID together with the kernel boot ID and the process start time from `/proc/<pid>/stat`. A process counts as the tunnel only when all three match, it belongs to the current user, and its command line still names the tunnel's log file. Stopping opens a pidfd first, verifies second, and signals through the pidfd, so a recycled PID is never signalled (see "Finding and stopping a tunnel").
- **"Established" is OpenSSH's own definition.** Portico starts `ssh -f -N -o ExitOnForwardFailure=yes …`: ssh puts itself in the background only after authentication and after every forward is set up, remote forwards included, and exits non-zero otherwise. Portico waits for that foreground ssh, so a start either reports a working tunnel or ssh's own reason for failing. No port polling, and no guess about remote forwards.
- **Started tunnels never prompt.** They run in the background with no terminal, so Portico passes `BatchMode=yes`. An agent (including a provider agent such as 1Password's) still works; a password, a passphrase prompt or an unknown host key fails with an explanation that points at `ssh-add` or at `portico test <alias>`, which already offers to trust a new host key.
- **Each tunnel is its own connection.** Portico passes `ControlMaster=no` and `ControlPath=none`. Through a multiplexing master, the forward would live in the master and outlive the `ssh` process Portico can stop.
- **Nothing starts or stops automatically.** No tunnel starts when Portico or the TUI starts, none restarts after it fails, and quitting the TUI leaves running tunnels running, without a prompt (decided in review); the tunnels screen says so while any tunnel runs. "Supervision" in the v1 design means status and explanations, not restarts.
- **A running tunnel is not edited or removed.** Editing or removing it is refused with "stop it first", because the running process would no longer match what is saved and nothing restarts on its own.
- **Tunnels follow their host by alias, without cascading.** A tunnel names a host alias (managed or external). If that alias later disappears or is renamed, the tunnel stays saved and is shown with a warning (`host prod-db is not in the inventory`); starting it fails with the same message. Host edits do not rewrite the tunnels file, which keeps the host transaction unchanged and the two files independent.
- **A lost record is rebuilt.** The runtime directory can vanish while tunnels survive (logout with lingering, a cleared tmpfs), and a crash can fall between the launch and the record write. A tunnel without a record whose process is still there is found by the same `/proc` scan a start uses, and its record is written again (see "Finding and stopping a tunnel").
- **Linux only,** as the rest of Portico's process control (`pidfd`, `/proc`). The macOS and Windows rows of `ROADMAP.md` already list this as their prerequisite.

## Decisions

### Tunnel model

```text
name          required, [A-Za-z0-9][A-Za-z0-9._-]*, unique case-insensitively among tunnels
host          required, a host alias as `portico connect` accepts it
kind          local | remote | dynamic
bind address  optional; local and dynamic: the local listen address, remote: the server-side one
bind port     required, 1 to 65535
target host   required for local and remote, absent for dynamic
target port   required for local and remote, absent for dynamic
```

- Names share the alias syntax, so a name is always a safe file name (it starts with a letter or digit, so never `.` or `..`).
- The bind address is a host name, an IPv4 address, an IPv6 address (written in brackets in specs, `[::1]:5432`), or `*` for every address. Omitted, ssh's own default applies: loopback, unless the host's configuration sets `GatewayPorts yes`.
- The target host is any single value without whitespace or control characters, as ssh resolves it on the remote side (`localhost` there means the server).
- Every value rejects whitespace and control characters, as host fields do, so every argument passed to ssh is one word and no shell is involved.
- Specs are parsed from and printed in OpenSSH's own syntax, the one users already know: `[bind_address:]port:host:hostport` for `-L` and `-R`, `[bind_address:]port` for `-D`. Saving a tunnel also runs `ssh -G -o RemoteCommand=none <forward> -- <alias>`: OpenSSH validates the spec and the host together, and a refusal is shown with ssh's message ("Bad local forwarding specification …").

The pure model and its parsing live in `src/tunnel.rs`.

### The tunnels file

`~/.ssh/portico/tunnels`, mode `0600`, beside `config`, written only by Portico:

```text
# Portico tunnels. Managed by Portico: edit with `portico tunnel` or the TUI.
# This file is not read by OpenSSH.

Tunnel db
    Host prod-db
    LocalForward 127.0.0.1:5432 localhost:5432

Tunnel socks
    Host bastion
    DynamicForward 1080

Tunnel preview
    Host staging
    RemoteForward 8080 localhost:3000
```

- One `Tunnel <name>` block per tunnel, ordered case-insensitively by name, with `Host` and exactly one forward line whose directive and value follow OpenSSH's `LocalForward`, `RemoteForward` and `DynamicForward`. Rendering is deterministic, as for the managed config.
- Parsing is strict: an unknown directive, a missing `Host` or forward, or a second forward is an error that names the line and points at `tunnels.bak`. Comments and blank lines are not preserved (the file is machine-owned, like the managed config's body).
- A missing file means no tunnels; it is created on the first `add`. Tunnels do not require `portico init`'s Include, only a reachable host (which, for a managed host, does).
- **Mutations** reuse the managed store's discipline: an exclusive `tunnels.lock`, re-read and strict parse, apply in memory, validate (name uniqueness, the model's rules, and the `ssh -G` check of the changed tunnel), write a `0600` temporary file in `~/.ssh/portico/`, sync it, copy the previous file to `tunnels.bak`, rename into place, and sync the directory. A failure before the rename leaves the active file unchanged. This is a smaller transaction than the host one, since OpenSSH never reads this file and no entry config has to be validated.

### Runtime records

The runtime directory is `$XDG_RUNTIME_DIR/portico/tunnels/` when `XDG_RUNTIME_DIR` is set to an absolute path of a directory owned by the current user, otherwise `~/.ssh/portico/run/tunnels/`. Portico creates the directories it needs with mode `0700`, and files with `0600`. Per tunnel:

```text
<name>.run    the record, replaced atomically (temporary file and rename)
<name>.log    ssh's log for this run (`-E`), truncated at each start
start.lock    held while a start checks conflicts and launches, for all tunnels
```

A record has one of two shapes, as `key value` lines:

```text
state running            state failed
pid 41237                reason Address already in use: 127.0.0.1:5432
start 9218734            exit 255
boot 3f0d…-…             finished 1781861043
started 1781860921
host prod-db
forward -L 127.0.0.1:5432:localhost:5432
```

- `start` is field 22 of `/proc/<pid>/stat` (clock ticks since boot), `boot` is `/proc/sys/kernel/random/boot_id`, `started` and `finished` are Unix seconds.
- A record is written only for a tunnel that is or was started, and holds nothing that is not safe to print.
- `host` and `forward` are what was actually started, so `stop` and the status view work even if the saved tunnel was later removed or the tunnels file is unreadable. They also let `--json` report what is running rather than what is saved.
- No credential, environment or key material is written. The log holds only what ssh prints at its configured `LogLevel`.

### Status

Status is computed on demand from the saved tunnel, its record and the system; reading it writes nothing, except that a record from another boot is deleted as stale.

```rust
pub enum TunnelStatus {
    Stopped,
    Running { pid: u32, started: u64 },                 // Unix seconds
    Failed { reason: String, finished: Option<u64> },   // Unix seconds
    PortInUse { address: String, by: Option<String> },
}
```

Rules, first match wins:

1. **Running:** a `running` record whose process passes the identity check below, or no record and a process found by the rediscovery scan (its record is then written again).
2. **Port in use:** the tunnel is not running and one of its local listen addresses cannot be bound now (a test `bind` and immediate close, with `SO_REUSEADDR` as ssh uses). Without a bind address, ssh listens on both `127.0.0.1` and `::1` (when IPv6 is available), so both are tested; `*` tests `0.0.0.0` and `::`, and a name tests every address it resolves to. IPv6 addresses are test-bound with `IPV6_V6ONLY` (through `libc`, already a dependency), as ssh binds them, so `::` is not reported busy because of an IPv4 listener; for the same reason `::` overlaps only IPv6 addresses when collisions are computed. `by` names the saved tunnel that is running on an overlapping address, when there is one; otherwise the holder is unnamed. Remote forwards have no local address and never get this status.
3. **Failed:** a `failed` record (the start failed), or a `running` record whose process is gone (the tunnel died, or was killed outside Portico). The reason is the record's, or the last meaningful line of `<name>.log` ("Timeout, server not responding", "Connection closed by …"), or "exited" when the log says nothing.
4. **Stopped:** otherwise.

"Port in use" outranks "failed" because it is what blocks the next start; the failure reason stays visible in the details.

`TunnelStatus` has a stable lowercase name for each variant (`stopped`, `running`, `failed`, `port-in-use`), used by the CLI's status column and intended verbatim for `--json`. Its fields and those of `TunnelReport` are named for that serialization (`pid`, `started`, `reason`, `finished`, `address`, `by`, `warnings`) and stay stable. They hold nothing that is unsafe to print: no environment, no credential, and from the log only the one reason line, with terminal escape sequences and control characters removed (a `ProxyCommand` may print anything); the full log is read only by `show` and the TUI's details. The CLI list, the TUI and the later JSON output all render one `TunnelReport { tunnel, status, warnings }` built by the same function in `src/app/tunnels.rs`; warnings are the dangling host alias and the saved tunnels it collides with.

The list does not run ssh: the "port in use" check uses the saved bind address with ssh's loopback default. A start does the authoritative check with `ssh -G` (next section).

### Conflicts

Checked before every start, under `start.lock`, so two concurrent starts cannot both pass:

- **Ports bound now.** Portico asks OpenSSH what this ssh would bind: `ssh -G -o RemoteCommand=none <forward> -- <alias>` and its `localforward` and `dynamicforward` lines, which include forwards the host's own configuration adds and reflect its `GatewayPorts`. Each address is test-bound; the first that fails stops the start with `port 5432 on 127.0.0.1 is in use` (and `by tunnel db` only when a running saved tunnel listens on an overlapping address and that port). A port below 1024 that needs privileges is reported as such.
- **Two saved tunnels collide** when both are local or dynamic, their ports are equal and their addresses overlap (equal after the default is applied, or either is `*`, `0.0.0.0` or `::`); or when both are remote forwards on the same host with the same remote port. Saving such a pair is allowed, since alternatives on one port are normal (a staging and a production database on 5432), and each shows a warning naming the other. Starting one while the other runs is refused with the other's name.
- Remote-side conflicts (a remote port the server already uses) are reported by ssh itself, through `ExitOnForwardFailure`.

### Starting

Under `start.lock`: refuse if the tunnel is already running (a no-op that succeeds, so a desktop panel can call `start` blindly), check the host is in the inventory, check conflicts, truncate the log, then run:

```text
ssh -f -N -o ExitOnForwardFailure=yes -o BatchMode=yes
    -o ControlMaster=no -o ControlPath=none -o RemoteCommand=none
    -E <runtime>/<name>.log
    -L|-R|-D <spec> -- <alias>
```

- Arguments go straight to the process, never through a shell, with stdin null, stdout null and stderr written to `<name>.err` in the runtime directory, read once the foreground ssh exits and then unlinked. It is a file, not a pipe: a `ProxyJump` or `ProxyCommand` child (`ssh -W`, `nc`) inherits ssh's stderr and lives as long as the tunnel, so a pipe would never reach end of file and the start would never return.
- The foreground ssh is waited for. The CLI lets Ctrl+C reach it (it is in Portico's process group until it backgrounds itself); the TUI runs the start on a worker thread, as connection tests do, and Esc kills it together with its descendants (a proxy child included), as a cancelled connection test does. A successful start kills nothing.
- **Exit 0:** ssh has forked into its own session (`daemon()`), so it survives Portico, the TUI and the terminal. Portico finds the background process by scanning `/proc` for processes of the current user started since the launch whose command line contains `-E <runtime>/<name>.log` (unique per tunnel; matching that pair rather than the whole line keeps it working when `ssh` on `PATH` is a wrapper that adds arguments). It reads its start time and writes the `running` record. If it is already gone, the start is reported as failed with the log's reason.
- **Non-zero exit:** Portico writes a `failed` record with the exit status and the explained reason from the log and stderr, and reports it.
- Explanations reuse `src/explain.rs` for connection failures (authentication, host key, unreachable host, jump hosts), plus three tunnel cases: a local bind failure (`Address already in use`, `Permission denied` for a privileged port), a refused remote forward (`remote port forwarding failed for listen port`), and a host that needs a password or passphrase in batch mode.

ssh's keep-alive and timeout settings come from the user's configuration: OpenSSH stays authoritative for connection behaviour. A tunnel whose connection dies is reported as failed with ssh's last log line.

### Finding and stopping a tunnel

Identity check, used by status and stop:

1. The record's `boot` equals the current boot ID (otherwise the record is stale and deleted).
2. `/proc/<pid>/stat` exists, its start time equals `start`, and its state is not zombie or dead.
3. `/proc/<pid>/status` shows the current user's real UID.
4. `/proc/<pid>/cmdline` still contains `-E <log path>`.

Boot ID, PID and start time identify one process for the lifetime of the system; the UID and command-line checks guard against a corrupt or hand-edited record.

**Rediscovery.** When a tunnel has no record, status and stop run the scan a start uses: a process of the current user, not a zombie, whose command line contains `-E <that tunnel's log path>`. If one is found, it is the tunnel: Portico writes its `running` record from `/proc` (PID, start time, current boot ID, `started` from the start time and the boot time, and `host` and `forward` from the command line), and status and stop then proceed as usual. Because the log path sits in the runtime directory and holds the tunnel's name, only a process Portico started for that tunnel can match. Only a process that leads its own session counts (its session ID, field 6 of `/proc/<pid>/stat`, equals its PID), because `ssh -f` calls `setsid` when it backgrounds itself: a foreground ssh that is still connecting is never taken for the tunnel. A status read rebuilds nothing while a start holds `start.lock` (it tries the lock without waiting), since that start writes the record itself; and a record whose process is gone is checked against the same scan before the tunnel is called failed.

Stop:

1. Open a pidfd for the PID (`pidfd_open`).
2. Run the identity check. Because the pidfd was opened first and the process verified afterwards started before the record was written, the pidfd refers to that same process: a PID is never reused while its process lives. If the check fails, nothing is signalled; the record is turned into stopped (removed) and the user is told the tunnel had already exited.
3. Send `SIGTERM` through the pidfd (ssh then also ends its `ProxyJump`/`ProxyCommand` child), wait up to 3 seconds for the pidfd to become readable (the process exited), then send `SIGKILL` through it.
4. Remove the record and the log.

`stop` on a failed tunnel clears its record (the failure was seen), and on a stopped tunnel succeeds with "not running". The pidfd helpers already in `src/openssh.rs` are made `pub(crate)` and reused rather than duplicated; moving them is left for later so this slice keeps its `openssh.rs` change to two lines.

### CLI

```text
portico tunnels
portico tunnel show [<name>]
portico tunnel add <name> --on <alias> (-L <spec> | -R <spec> | -D <spec>)
portico tunnel edit [<name>] [--name <new>] [--on <alias>] [-L <spec> | -R <spec> | -D <spec>]
portico tunnel remove [<name>] [--force/-f]
portico tunnel start [<name>...]
portico tunnel stop [<name>...] [--all/-A]
```

- **`portico tunnels`** prints aligned columns: name, host, forward (`127.0.0.1:5432 → localhost:5432`, `socks 1080`, `remote 8080 → localhost:3000`), status, and the detail (the PID and age for running, the short reason for failed, the address and holder for port in use). Warnings follow the table. With no tunnels it prints how to add one.
- **`show`** prints one tunnel's fields, the exact ssh command line it runs, its status with the full explanation, and for a failed tunnel the last lines of its log.
- **`add` and `edit`** take the forward in ssh's own spec syntax. `--on` without an alias opens the host picker. `edit` changes only what is given; changing the kind is giving another of `-L`/`-R`/`-D`.
- **`remove`** prompts on a terminal and otherwise needs `--force`, as host removal does.
- **Names are optional** where the fuzzy picker of `2026-10-19-fuzzy-pickers-design.md` can ask: `show`, `edit` and `remove` pick from every tunnel, `start` from those not running, `stop` from those running (or failed). Several names start or stop several tunnels in order, each reported on its own line.
- **Exit status:** 0 when every named tunnel ended in the asked state (including "already running" and "not running"), 1 when any failed or was refused, 130 when a picker was cancelled.
- No command reads a terminal except the pickers and `remove`'s prompt, so a desktop panel can run `portico tunnel start db` and `portico tunnel stop db` directly.

### TUI

The tunnels screen is a full screen, like the adopt and export screen, so the hosts screen and its forms do not change beyond one key.

- **Opening.** `T` on the hosts screen opens it; `T` or `Esc` (when nothing is being edited) returns to the hosts. The hosts footer advertises `T tunnels`. Terminals report Shift+T as `T` with or without the Shift modifier, so `T` matches either way, and the test key `t` matches only a lowercase `t`.
- **Layout.** A table on the left with the columns name, host, forward and status, the status coloured (running green, failed red, port in use yellow, stopped dimmed). The right pane shows the selected tunnel's details: the fields, the ssh command line, the status with its PID and age, the explanation of a failure, the last log lines, and its collision warnings.
- **Keys:**

  | Key | Effect |
  |---|---|
  | Up, Down, j, k | Move the selection |
  | `/` | Filter tunnels by name, host or forward, with the shared fuzzy matcher |
  | `s` | Start the selected tunnel, or stop it when it is running (one key, as asked) |
  | `a` | Add a tunnel |
  | `e` | Edit the selected tunnel (refused while it runs) |
  | `x` | Remove the selected tunnel, after a confirmation (refused while it runs) |
  | `r` | Re-read the tunnels file and every status |
  | `o` | Unfold or fold the full log of a failed tunnel |
  | `T`, `Esc` | Back to the hosts |
  | `q` | Quit (running tunnels keep running) |

  While any tunnel runs, the table's bottom border reads `running tunnels keep running after q` (the footer is already long enough to wrap).

- **The form** replaces the right pane, as the host forms do: Name, Host (the known aliases matching what is typed are listed under it, and Tab completes the best one, then moves on; the jump host dropdown and `src/suggest.rs` are left alone, since the identity slice changes them), Kind (Space or ←/→ cycles local, remote, dynamic), Bind address (placeholder `localhost`), Bind port, and Target host and Target port (hidden for dynamic). Fields validate as they are typed where the model can say so (port ranges, characters), Enter saves through the same transaction as the CLI, and a refusal (a duplicate name, an unknown host, ssh's spec error) appears under the field it concerns. A collision with another saved tunnel shows as a warning under Bind port without blocking the save.
- **Starting** runs on a worker thread, shows `starting…` in the status column, and Esc cancels it. **Stopping** is quick and runs in place. The result goes to the status line, as test results do.
- **Status refresh.** On opening the screen, on `r`, after a start, stop or save, and every 2 seconds while the screen is shown. A refresh reads records and `/proc` and test-binds the local ports of tunnels that are not running; it never runs ssh or touches the network. Nothing refreshes while the hosts screen is shown.

## Code layout

Kept in their own modules, so the hosts screen, the host forms and the host details pane (which the identity awareness slice also changes) are untouched apart from the `T` key and the footer.

- **`src/tunnel.rs` (new, pure, unit-tested):** `Tunnel`, `Forward` and its kinds, name and field validation, spec parsing and printing, the tunnels file parser and renderer, `TunnelStatus` and its names, collision detection between saved tunnels, and the ssh argument list.
- **`src/tunnel/store.rs` (new):** the tunnels file transaction.
- **`src/tunnel/runtime.rs` (new):** the runtime directory, records, the identity check against a `/proc` root passed as a path (as `process_children_in` does, so tests use a fake one), the background-process scan, the bind test, start and stop.
- **`src/openssh.rs`:** `open_pidfd` and `signal_pidfd` become `pub(crate)`; nothing else changes.
- **`src/app/tunnels.rs` (new):** `tunnels()` returning `TunnelReport`s, `add`, `edit`, `remove`, `start` (with a cancellable variant for the TUI) and `stop`. `src/app.rs` gains only the module and the paths.
- **`src/cli.rs`:** the `tunnels` and `tunnel` commands; their printing goes in `src/cli/tunnels.rs` if `cli.rs` would otherwise grow by more than the argument definitions and dispatch.
- **`src/tui/tunnels.rs` (new):** the screen's state, keys, form, start worker (modelled on `tester.rs`), refresh timer and rendering. `src/tui.rs` gains a `Mode::Tunnels`, the `T` key, the draw and key dispatch, and the footer entry.
- **`README.md`:** the commands, the screen's keys, where the files live, and what a started tunnel is (an `ssh` process that keeps running after Portico exits).
- **Docs in the same pull request:** this spec becomes "Approved; implemented", item 1 leaves `ROADMAP.md` v3 (the rest renumbered), and the v1 design's "Delivered follow-up slices" gains a "Tunnels" bullet.

## Out of scope

- Several forwards in one tunnel, Unix-socket forwards and remote dynamic forwards.
- Starting tunnels automatically (at login, with the TUI, or on demand by a connection), restarting failed ones, and keep-alive settings of Portico's own.
- Tunnels in `ssh_config` (`LocalForward` on a host) managed by Portico: forwards in the user's own configuration still apply to `ssh <alias>` as before, and to a tunnel on that host (they are part of its conflict check).
- Naming the process that holds a busy port when it is not a saved tunnel.
- Machine-readable output (the next v3 slice exposes `TunnelReport`).
- macOS and Windows.

## Testing

- **Unit (`src/tunnel.rs`):** names and every field's validation; spec parsing and printing for each kind, with and without a bind address, IPv6 in brackets, `*`, and every rejected form (socket paths, remote dynamic, port 0 and 65536, whitespace); the file's round trip, ordering and every strict-parse error; collisions (same port and address, wildcard against specific, different ports, local against remote, remote on two hosts); status precedence; the ssh argument list.
- **Unit (`src/tunnel/runtime.rs`, fake `/proc`):** a matching process is running; a tunnel without a record whose process is found by the scan is running and its record is written again; a different start time (a recycled PID), another boot ID, another UID, a zombie and a missing command-line argument are each not the tunnel; a stale record from another boot is deleted; record round trip; the background-process scan picks the process started after the launch and ignores older or foreign ones; the runtime directory fallback when `XDG_RUNTIME_DIR` is unset, relative or not ours.
- **Unit (store):** a failed validation leaves the file unchanged; a concurrent add is not lost; an unreadable file names `tunnels.bak`.
- **CLI (fake executables, as in the existing tests):** add, edit and remove, including the refusals (a duplicate name, an unknown host, a running tunnel, `ssh -G` rejecting a spec); `remove` without `--force` off a terminal; the exact ssh command line of `start`; `start` of a running tunnel and `stop` of a stopped one succeeding; a failed start writing a failed record and explaining it; pickers for each command.
- **TUI (unit, as in `tui.rs`):** `T` in and out, with and without the Shift modifier, and `t` still testing; the table and the details; `s` producing a start or a stop by status; `e` and `x` refused while running; the form's kind cycling, hidden target fields for dynamic, field errors and the collision warning; the footer.
- **End to end (`tests/e2e/tunnels.rs`, real `ssh` against the throwaway `sshd`):**
  - a local forward to a small TCP echo server on `127.0.0.1` carries data, survives the `portico` process that started it, shows as running in a new `portico tunnels`, and is gone (process and port) after `stop`;
  - a dynamic forward answers a minimal SOCKS5 connect to the echo server;
  - a remote forward reaches the echo server through the server-side port, and a second remote forward on that busy port fails with ssh's reason;
  - a busy local port shows "port in use" and refuses the start, and a running tunnel blocks starting a colliding one, naming it;
  - a tunnel killed outside Portico shows as failed with the log's reason;
  - a running tunnel whose record was deleted still shows as running in `portico tunnels`, and `stop` stops it;
  - a record whose PID now belongs to another process (a `sleep` given the record's PID and a wrong start time) is not signalled by `stop`;
  - a start that needs an unknown host key fails in batch mode with the `portico test` hint.

## Decided in review

1. **One forward per tunnel.** Kept; no grouping in this slice.
2. **Quitting the TUI** leaves running tunnels running, with no prompt. The tunnels screen's footer says so while any tunnel runs.
