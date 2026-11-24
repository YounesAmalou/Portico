# Architecture

Portico is one crate, `portico-ssh`, whose library is `portico`. Its modules form layers. The supported interfaces are the CLI and its JSON output (`docs/json-output.md`); module paths inside the crate are not a stable API.

## Layers

```
interfaces   src/tui/, src/cli/            (and src/desktop/, used by the CLI)
    |
   app       src/app/                      the operations both interfaces call
    |
 domain  +  backend                        src/domain/, src/backend/
    |
 platform                                  src/platform/
```

A layer uses only the layers below it, never the ones above:

- `domain` knows nothing about the terminal UI or the CLI's output format. It reaches files and processes through `platform` and plain `std`; it never calls `app`, `cli` or `tui`.
- `backend` adapters use `platform` and the domain's host types, never `app` or an interface.
- `app` is the entry point the interfaces use for anything that reads or changes state.
- An interface does not call another interface, with two deliberate cases. `portico` with no subcommand starts the TUI: `cli::run` calls `crate::tui::run` (`src/cli/mod.rs`). And `src/picker.rs`, the inline pickers and the fuzzy-ranked rows both interfaces use, sits at the crate root: the CLI and the TUI's containers, tunnels and migrate screens use it. It is not clean, though: it imports `TextInput` and `display_width` from `crate::tui`, which is a known exception to the rule.

The layer is called `domain`, not `core`: a crate-root module named `core` is ambiguous with the built-in `core` crate in `use` paths.

The boundaries are kept in review, not by the compiler. The layers are modules of one crate and not a Cargo workspace, because crates.io cannot publish a crate that depends on unpublished ones.

## Modules

| Module | Holds |
|---|---|
| `platform::process` | Process control: killing process trees through pidfds, reading `/proc`, stopping a recognised process |
| `platform::launcher` | Opening a command in a new terminal window (`xdg-terminal-exec`) |
| `platform::paths` | The home directory, `~` display, and the well-known locations under it |
| `platform::fs` | Locking, atomic writes, directory sync and file identity (`Stamp`, `stamp`) |
| `domain::host`, `domain::discovery` | Hosts, and reading the inventory from the OpenSSH configuration |
| `domain::managed` | The managed host file: store and transactions (`store`), parser (`config`), renderer (`render`), setup of the entry config (`init`) |
| `domain::openssh` | `ssh -G` resolution (`config`), connection outcomes (`connect`), `known_hosts` and key fetch (`keys`), reading a child's output pipes (`pipes`) |
| `domain::adopt` | Adoption and export plans: scanning (`scan`) and previews (`diff`) |
| `domain::tunnel` | Saved port forwards, their store, and the runtime state of started ones (`runtime`, including the `Proc` methods specific to tunnels) |
| `domain::probe` | The connection test: its options and outcomes |
| `domain::identity`, `suggest`, `fuzzy`, `explain`, `config_error` | Identity data, jump-host suggestions, fuzzy matching, failure explanations, configuration errors |
| `backend` | The `Backend` interface, `BackendId`, `Target`, `BackendIssue`, the dispatch functions, and `remote_command` and `quote` (the app's transport uses them) |
| `backend::docker` | The Docker adapter: `docker ps` parsing, grouping by Compose project, and its access decision (`Access`, `decide`, private to this file) |
| `app` | Operations over the domain and the backends, running a backend's commands locally or over `ssh`: `containers`, `tunnels`, `identities` |
| `cli` | Arguments (`mod`), one module per command group (`hosts`, `tunnels`, `containers`, `desktop`, `migrate`) and the JSON documents (`json`) |
| `picker` | The inline pickers the CLI opens when a host or container is missing; also used by the TUI |
| `tui` | The event loop (`mod`, `events`), the terminal session (`terminal`, `hangup`), shared widgets (`text_input`, `dropdown`), the config watch (`watch`, which keeps only its file identity in `platform::fs`), one module per tab (`hosts`, `tunnels`, `containers`, with the hosts tab's form and its tests in `form`), and the workers that keep the interface responsive (`lister`, `tester`, `planner`, `identities`), `init` and `migrate` |
| `desktop` | `portico desktop` plans: the Quickshell panel, the launcher entry and the keybinding |

## The backend interface

`src/backend/mod.rs` defines the `Backend` trait: list the running containers of a target, give the command that opens a shell in one, and explain a problem. The app owns the transport: it runs commands on this machine or through `ssh` and hands each adapter a `run` callback. An adapter says which commands to run and what their output means.

Adapters are compiled in and chosen at run time through the `BackendId` enum. Nothing is loaded from outside the binary.

### Add a backend

1. Add the adapter in `src/backend/<name>.rs`, implementing `Backend`.
2. Add one `BackendId` variant.
3. Add one arm for it in each match in `src/backend/mod.rs`: `BackendId::as_str` and the dispatch functions `list`, `shell_words` and `plain_issue`.
4. Make the app choose it. The app names `BackendId::Docker` itself in `src/app/containers.rs` (listing, opening a shell, and explaining a local spawn failure), and `local_failure` there also checks for the `docker` binary by name. These are the only non-test places that do; a new backend needs its selection added there, or the app never reaches the adapter.

### Add a platform

Each platform concern is a module whose functions exist once per operating system, chosen at compile time with `cfg(target_os = ...)`, without trait objects. Linux is the only implementation today.

1. Add a sibling of `src/platform/linux/` (for example `src/platform/macos/`) with the same four concerns.
2. Add its `cfg` selection in `src/platform/mod.rs` (which currently refuses to compile on other systems) and in each of `src/platform/process.rs`, `launcher.rs`, `paths.rs` and `fs.rs`, which re-export the implementation for the target.
