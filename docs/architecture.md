# Architecture

Portico is one crate, `portico-ssh`, whose library is `portico`. Its modules form layers. The supported interfaces are the CLI and its JSON output (`docs/json-output.md`); module paths inside the crate are not a stable API.

## Layers

```
interfaces   src/tui/, src/cli/            (and src/desktop/, used by both)
    |
   app       src/app/                      the operations both interfaces call
    |
 domain  +  backend                        src/domain/, src/backend/
    |
 platform                                  src/platform/
```

A layer uses only the layers below it, never the ones above:

- `domain` knows nothing about the terminal UI or the CLI's output format. It reaches files, processes, sockets and the clock through `platform` and plain `std`, and calls no `libc` or `std::os::unix` itself. One exception is sideways: `domain::settings` imports `backend::BackendId`, a plain enum, to map a backend to its tab setting, and calls nothing in `backend`. `domain` never calls `app`, `cli` or `tui`.
- `backend` adapters use `platform` and the domain's host types, never `app` or an interface.
- `app` is the entry point the interfaces use for anything that reads or changes state.
- An interface does not call another interface, with two deliberate cases. `portico` with no subcommand starts the TUI: `cli::run` calls `crate::tui::run` (`src/cli/mod.rs`). And `src/picker.rs`, the inline pickers and the fuzzy-ranked rows both interfaces use, sits at the crate root: the CLI and the TUI's hosts, containers, tunnels and migrate screens use it. It is not clean, though: it imports `TextInput` and `display_width` from `crate::tui`, which is a known exception to the rule.

The layer is called `domain`, not `core`: a crate-root module named `core` is ambiguous with the built-in `core` crate in `use` paths.

The boundaries are kept in review, not by the compiler. The layers are modules of one crate and not a Cargo workspace, because crates.io cannot publish a crate that depends on unpublished ones.

## Modules

| Module | Holds |
|---|---|
| `platform::process` | Process control: killing process trees, stopping a recognised process, reading the process table (`Proc`); `Uid`, `current_uid` and `effective_uid`, `exit_code`, `own_group`, `detach`, `HANGUP_SIGNALS` and `UNIX_GROUPS` (whether Docker's local access decision applies); `Proc::scan` and `Logged` know how a started tunnel's `ssh` looks (`-E <log>`). Ending a child with its descendants, reaping within a bound and `scan` are portable (`common`); the Unix tree walk and `stop_process` are shared (`unix`); each system pins a process (`Pin`: a pidfd on Linux, a kqueue exit watch on macOS, a process handle on Windows) and reads its process table (`/proc` on Linux, `sysctl` and libproc on macOS, the Toolhelp snapshot and process handles on Windows). Windows sends no signals: it terminates a pinned process |
| `platform::launcher` | Opening a command in a new terminal window: `xdg-terminal-exec` on Linux; on macOS, `open(1)` with the terminal named by `PORTICO_TERMINAL`, else the one Portico runs in (`TERM_PROGRAM`), else Terminal; on Windows, Windows Terminal (`wt.exe`), else a new console window. Running a launcher program and waiting out its grace period is shared (`common`) |
| `platform::paths` | The home directory, `~` display and the well-known locations under it (shared), `openssh_path` (a path as written into an OpenSSH file: `~` and `/` on every system), `config_dir` (shared), `system_ssh_config`, `runtime_dir` and `DEFAULT_AGENT` (the agent ssh uses without `SSH_AUTH_SOCK`; per system) |
| `platform::net` | `bind_probe`: whether a local address can be listened on, as ssh binds it |
| `platform::time` | `local`: a Unix time as local calendar time (`LocalTime`) |
| `platform::fs` | Locking, atomic writes, permissions and ownership checks (`private`, `mode_of`, `set_mode`, `is_executable`, `owner_is`, `has_other_links`, `writable`, `is_read_only_error`), symlinks (`symlink`, `symlink_target`), agent endpoints (`is_agent_endpoint`: a Unix socket, or a named pipe on Windows), directory sync, file identity (`Stamp`, `stamp`) and `nonblocking_pipe`; the Unix parts are shared by Linux and macOS |
| `domain::host`, `domain::discovery` | Hosts, and reading the inventory from the OpenSSH configuration |
| `domain::managed` | The managed host file: store and transactions (`store`), parser (`config`), renderer (`render`), setup of the entry config (`init`) |
| `domain::openssh` | `ssh -G` resolution (`config`), connection outcomes (`connect`), `known_hosts` and key fetch (`keys`), reading a child's output pipes (`pipes`) |
| `domain::adopt` | Adoption and export plans: scanning (`scan`) and previews (`diff`) |
| `domain::tunnel` | Saved port forwards, their store, and the runtime state of started ones (`runtime`, including the `Proc` methods specific to tunnels) |
| `domain::settings` | The preferences file: reading, defaults, the remembered container users (`UserKey`, `remember_user`), keeping comments on write |
| `domain::probe` | The connection test: its options and outcomes |
| `domain::identity`, `suggest`, `fuzzy`, `explain`, `config_error` | Identity data, jump-host suggestions, fuzzy matching, failure explanations, configuration errors |
| `backend` | The `Backend` interface, `BackendId`, `AUTOMATIC` (every backend, in the order they are listed), `Target`, `Via`, `BackendIssue`, the dispatch functions, and `remote_command` and `quote` (the app's transport uses them) |
| `backend::listing` | The container rows every backend produces (`Container`, `Listing`), grouping by Compose project, display names, `remember_key` (the name a container is remembered by), and resolving a container reference |
| `backend::docker` | The Docker adapter: `docker ps` parsing, and its access decision (`Access`, `decide`, private to this file) |
| `backend::podman` | The Podman adapter: the listing script (the ssh user's own containers), `podman ps --format json` parsing, and `podman exec` |
| `backend::proxmox` | The Proxmox VE adapter: the listing script (root or `sudo -n`), `pct list` parsing, and `pct exec` |
| `app` | Operations over the domain and the backends, running a backend's commands locally or over `ssh`: `containers`, `tunnels`, `identities`, and the settings (their file's location, reading and writing them, and `App::remember_container_user`) |
| `cli` | Arguments (`mod`), one module per command group (`hosts`, `tunnels`, `containers`, `desktop`, `migrate`, `settings`) and the JSON documents (`json`) |
| `picker` | The inline pickers the CLI opens when a host or container is missing; also used by the TUI |
| `tui` | The event loop (`mod`, `events`), the terminal session (`terminal`, `hangup`), shared widgets (`text_input`, `dropdown`), the config watch (`watch`, which keeps only its file identity in `platform::fs`), one module per tab (`hosts`, `tunnels`, `containers`, with the hosts tab's form and its tests in `form`), and the workers that keep the interface responsive (`lister`, `tester`, `planner`, `identities`), `init`, `migrate` and the `settings` overlay |
| `desktop` | `portico desktop` plans: the Quickshell panel, the launcher entry and the keybinding; used by the CLI and, on Linux, the TUI's settings overlay. Linux only: on macOS and Windows the command is hidden and refused, the overlay leaves the desktop integration out, and the panel refresh is skipped (`main`) |

## The backend interface

`src/backend/mod.rs` defines the `Backend` trait: list the running containers of a target, give the command that opens a shell in one, and explain a problem. The app owns the transport: it runs commands on this machine or through `ssh` and hands each adapter a `run` callback. An adapter says which commands to run and what their output means.

Adapters are compiled in and chosen at run time through the `BackendId` enum. Nothing is loaded from outside the binary.

### Add a backend

1. Add the adapter in `src/backend/<name>.rs`, implementing `Backend`. It produces the shared `backend::Listing` of `backend::Container` rows (`src/backend/listing.rs`), which grouping, naming and lookup already work on.
2. Add one `BackendId` variant.
3. Add one arm for it in each match in `src/backend/mod.rs`: `BackendId::as_str`, `program`, `title`, `chooses_user` and `ungrouped_heading`, and the dispatch functions `list`, `shell_words` and `plain_issue`.
4. Make the interfaces reach it: add the new id where it belongs in `backend::AUTOMATIC` (Docker, Podman, Proxmox). Every backend is listed in that order: the TUI has one tab per backend (`Tab::ALL` in `src/tui/mod.rs`, which a test keeps in step with `AUTOMATIC`), each with its own panel and listing worker, and the CLI's human listing and `portico shell` walk `AUTOMATIC` themselves. `App::containers` without a backend returns the first one installed, which only the JSON output uses. With `--backend`, the CLI maps its `BackendArg` value to the id (`src/cli/mod.rs`): add a value there. The list records its backend (`ContainerList::backend`), and the shell, the CLI's human and JSON output and the TUI read it from there; the JSON adds a `backend` key for every backend but Docker, whose documents predate it.

### Add a platform

Each platform concern is a module whose functions exist once per operating system, chosen at compile time with `cfg(target_os = ...)` or `cfg(unix)` / `cfg(windows)`, without trait objects. Linux, macOS and Windows are implemented:

```
src/platform/
  mod.rs        selects the system (`os` on Unix); compile_error! for any target that is not Linux, macOS or Windows
  fs.rs, net.rs, paths.rs, process.rs, launcher.rs, time.rs   re-export `common` (fs, paths, process, time, launcher) plus `unix` and the
                system's own module on Unix, or `windows` on Windows
  common/       portable on every system: `~` display and expansion, config_dir, path
                normalisation, locking, PendingFile, Stamp, LocalTime, terminate_process_tree,
                reap_child_bounded, running a launcher program (Launch, run), Proc::scan and Logged, and the test fault hooks
  unix/         shared by Linux and macOS: permissions, symlinks, descriptors, file identity,
                the socket probe (bind_probe), local time, the home directory (HOME), the process
                algorithms (tree walk, stop_process) and Uid, exit codes, process groups
                and hang-up signals
  linux/        Pin (pidfd), children and stopped state (/proc), Proc, xdg-terminal-exec,
                runtime_dir (XDG_RUNTIME_DIR)
  macos/        Pin (kqueue NOTE_EXIT), children and stopped state (libproc), Proc (sysctl),
                open(1) and its terminals, runtime_dir (~/.ssh/portico/run/tunnels)
  windows/      fs, net, paths and time for Windows; launcher (wt.exe, else a new console);
                process: Pin (process handle),
                Proc (Toolhelp, GetProcessTimes, the token's SID, the command line from ntdll,
                the registry's BootId),
                tree walk by creation time and parent, stop_process by TerminateProcess
```

A system provides what `unix` calls (`Pin` with `open`, `signal` and `wait_exit`; `children`; `wait_until_stopped`; `Proc` with `stat`, `uid`, `cmdline`, `pids`, `boot_id` and `started_at`) and the per-system items of `paths` and `launcher`. A record's `start` is whatever the system's `Proc::stat` reports (clock ticks after boot on Linux, microseconds since the epoch on macOS, the creation `FILETIME` on Windows); records never move between systems.

1. Add a sibling of `src/platform/linux/` (for example `src/platform/windows/`). A system that is not a Unix also replaces what `unix` provides, so `fs.rs`, `net.rs`, `paths.rs`, `process.rs`, `launcher.rs` and `time.rs` select it instead; what is portable belongs in `common/`.
2. Add its `cfg` selection in `src/platform/mod.rs`, which refuses to compile on other systems.
3. Revisit the Linux-only `desktop` module.
