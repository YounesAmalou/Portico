# Modular Architecture Design

**Status:** Approved; implemented (see "As built" at the end, and `docs/architecture.md`)

## Purpose

Several source files have grown too large to read as one unit: `src/tui.rs` (6,914 lines, about half of them tests), `src/managed.rs` (3,366), `src/cli.rs` (2,894), `src/openssh.rs` (2,517) and `src/adopt.rs` (2,141). Operating-system details (`pidfd`, `/proc`, `flock`, `xdg-terminal-exec`, XDG paths) are spread across `openssh.rs`, `managed.rs`, `tunnel/runtime.rs`, `tui/` and `desktop/`, and Docker is the only backend, written directly into `container.rs`.

This slice is v4 slice 1 of `ROADMAP.md`. It reorganizes the crate into layers that new backends (Proxmox, Podman), new platforms (macOS, Windows in v5) and the interfaces plug into without touching unrelated code. It changes no behaviour, no CLI flag and no JSON output.

## Settled questions

- **One crate, not a workspace.** The layers are modules of the single `portico-ssh` crate. A workspace would make the compiler enforce the boundaries, but crates.io cannot publish a crate that depends on unpublished ones, so every layer would become a published, versioned crate with its own release-plz changelog and tag. Nothing outside Portico uses the core as a library today (the desktop panel consumes the CLI and its JSON output), so that cost buys nothing yet. The boundaries are kept by the dependency rule below, checked in review. Revisit only if an outside consumer needs the core as a library.
- **No file-size guideline.** A file is split when it has grown hard to work with or a change needs it, not when it crosses a number. The architecture document records the module map only.
- **One shape for what a backend needs.** Today's Docker report (`DockerProblem`, `Access`, `DockerIssue`) becomes the backend-neutral `BackendIssue` described below. The TUI, the CLI and the desktop panel show any backend's issue the same way, without knowing which backend produced it.
- **Platform selection at compile time.** Each platform concern is a module whose functions are implemented once per operating system and chosen with `#[cfg(target_os = ...)]`, the way the standard library does it. No trait objects: only one platform exists in a given binary. v4 has only the Linux implementation; v5 adds `macos` and `windows` beside it.
- **Backends chosen at run time.** Adapters are compiled in. The registry is the `BackendId` enum and the dispatch functions in `src/backend/mod.rs` that match on it; registering a backend is one variant and one arm per function. This is not a plugin system: nothing is loaded from outside the binary.
- **The app runs commands, the adapter decides them.** Running a command on this machine or through `ssh` on a host (with batch mode, cancellation and ssh's own failures explained) stays in `app`, shared by every backend. An adapter says which commands to run and what their output means: the listing, the probe, a rerun inside a group, and the issue to report.
- **The library is not a supported API.** Module paths inside the crate change in this slice. The supported interfaces stay the CLI and its JSON output (`docs/json-output.md`).

## Decisions

### Layers and the dependency rule

```
interfaces   src/tui/, src/cli/            (and src/desktop/, used by the CLI)
    |
   app       src/app/                      the operations both interfaces call
    |
 domain  +  backend                        src/domain/, src/backend/
    |
 platform                                  src/platform/
```

A layer may use the layers below it, never the ones above. In particular:

- `domain` knows nothing about the terminal UI, the CLI's output format or the operating system; it reaches files and processes only through `platform`.
- `backend` adapters use `platform` (to run commands) and the domain's host types (to name a target), never `app` or an interface.
- `app` is the only entry point the interfaces use for anything that reads or changes state.
- An interface never calls another interface: the TUI does not call into `cli`, and `cli` does not call into `tui`.

The layer is called `domain`, not `core`: a crate-root module named `core` is ambiguous with the built-in `core` crate in `use` paths.

### Module map (target)

| Layer | Module | Holds | Comes from |
|---|---|---|---|
| platform | `platform::process` | spawning, `pidfd` open and signal, killing a process tree, finding a process again | `openssh.rs`, `tunnel/runtime.rs`, `tui/lister.rs`, `tui/tester.rs`, `app/tunnels.rs` |
| platform | `platform::launcher` | opening a command in a new terminal (`xdg-terminal-exec`) | `openssh.rs`, `app/identities.rs`, `desktop/launcher.rs` |
| platform | `platform::paths` | the home directory, the runtime directory, the system `ssh_config` path, `~` display | `home.rs`, `main.rs`, `RuntimeDir::locate` in `tunnel/runtime.rs`, `ConfigPaths::system_config` |
| platform | `platform::fs` | private directories, lock files, atomic write, directory sync, lossy reads, the file identity (`Stamp`) the config watch compares | the `pub(crate)` helpers of `managed.rs`, `Stamp` in `tui/watch.rs` |
| domain | `domain::host`, `domain::discovery` | hosts and the inventory read | `host.rs`, `discovery.rs` |
| domain | `domain::managed` | the managed file: store and transactions, parse, render, initialization | `managed.rs`, split along those four parts |
| domain | `domain::openssh` | `ssh -G` resolution, `known_hosts`, key fetch, connection outcomes | `openssh.rs`, minus its process helpers |
| domain | `domain::adopt` | adoption and export plans | `adopt.rs`, `adopt/` |
| domain | `domain::tunnel` | saved port forwards and their store | `tunnel.rs`, `tunnel/store.rs` |
| domain | `domain::identity`, `suggest`, `fuzzy`, `explain`, `config_error` | as today | same files |
| backend | `backend` | the `Backend` interface, `BackendId`, `Target`, `BackendIssue`, the registry | new, from the shared parts of `container.rs` |
| backend | `backend::docker` | the Docker adapter | `container.rs`, `probe.rs` (its Docker part) |
| app | `app` | operations over the domain and the backends, and running a backend's commands locally or over `ssh` | `app.rs`, `app/` (unchanged role) |
| interface | `cli` | arguments, one module per command group, JSON documents, pickers | `cli.rs`, `cli/`, `picker.rs` |
| interface | `tui` | event loop, terminal session, shared widgets (text input, form, dropdown), one module per tab (hosts, tunnels, containers) for its state, input and rendering | `tui.rs`, `tui/` |
| interface | `desktop` | `portico desktop` plans (Omarchy panel, launcher entry, keybinding) | unchanged, reaches the OS through `platform` |

The config watch (`tui/watch.rs`) stays in the TUI: it follows discovery's `Include` inputs, which are domain types, so only its file identity moves down to `platform::fs`.

The map is the target, not a contract: a step may settle a module's exact name or split it further when the code shows a better seam. The architecture document records what was built.

### The backend interface

```rust
pub enum Target { Local, Host(String) }       // from app/containers.rs
pub enum BackendId { Docker }                 // podman and proxmox join in their slices
pub enum GroupTool { Sg }                     // from container.rs; `--via sg` is unchanged

/// How running one command failed, as the app reports it to the adapter.
pub enum RunFailure<E> { Cancelled(E), Failed(E) }
/// Why a listing produced no list.
pub enum ListFailure<E> { Run(E), Issue(Box<BackendIssue>) }

pub trait Backend {
    fn id(&self) -> BackendId;
    fn list<E>(
        &self,
        target: &Target,
        run: &mut dyn FnMut(&[String]) -> Result<Output, RunFailure<E>>,
    ) -> Result<ContainerList, ListFailure<E>>;
    fn shell_words(&self, id: &str, user: Option<&str>, via: Option<GroupTool>) -> Vec<String>;
    fn plain_issue(&self, target: &Target, problem: Problem, stderr: String) -> BackendIssue;
}
```

`run` is the app's transport: it runs the words on the target and returns the captured output. A `Cancelled` failure always stops the listing; a `Failed` one stops it, except where the adapter treats it as "no answer" (Docker's probe does). `list` is generic, so the trait is not used as a trait object; the registry's dispatch functions call the adapter directly.

What the picker shows stays today's `Container` row and `Listing`, and `ContainerList` (target, listing, `via`, note) moves to `backend`. Proxmox and Podman extend the row in their own slices if they need to.

### What a backend reports: `BackendIssue`

```rust
pub struct BackendIssue {
    pub backend: BackendId,
    pub target: Target,
    pub problem: Problem,       // NotInstalled | ServiceDown | PermissionDenied | Other
    pub headline: String,       // one-line summary
    pub next: Vec<String>,      // steps the user can take themselves
    pub offer: Option<Offer>,   // a fix Portico can run after a confirmation
    pub stderr: String,         // what the tool wrote to stderr
}

pub struct Offer {
    pub key: char,           // the TUI key that runs it: 'g', 's'
    pub hint: String,        // the TUI footer hint: "add to docker group"
    pub prompt: String,      // the CLI question, without " [y/N] "
    pub words: Vec<String>,  // the command, run on the target after a yes
}
```

- Each adapter turns its own fixes into generic offers. Docker's two offers keep their exact keys, hints, prompts and commands: `g` / "add to docker group" / "Add {user} to the docker group on {target}? Members of the docker group have root-equivalent access. This runs: …", and `s` / "start docker" / "Start the Docker service on {target}? This runs: …". The interfaces show, confirm and run an offer without knowing the backend.
- What is not an offer stays inside the adapter: a session that predates the user's docker group is still rerun through `sg` automatically (the listing's `via`), and a missing `sg` is still a `next` line asking for a new login.
- The JSON failure keeps its shape: `kind` is the backend id and `problem` keeps today's strings. Docker still emits `"kind": "docker"` with `not-installed`, `daemon-down`, `permission-denied` or `other` (`ServiceDown` maps to `daemon-down`), and `next` still ends with the "Run … in a terminal to be offered this fix." line when an offer exists. Podman and Proxmox emit their own `kind` with the same fields.
- A requirement such as Proxmox's `pct` needing root is `PermissionDenied` with `next` lines (connect as root, or configure sudo). No new problem kind is needed for it.

### Steps

Each step is one pull request of moves and small changes, and leaves the unit, CLI, docs and end-to-end tests passing unchanged (`scripts/ci-checks.sh`). Tests move with the code they test.

1. **Platform.** Create `src/platform/` with its Linux implementation and move process control, the launcher, paths, file helpers and the watch's file identity into it. Callers switch to `platform::…`.
2. **Backend.** Create `src/backend/` with the interface, `BackendIssue`, `Offer` and the registry, and move Docker behind it as `backend::docker`. `DockerIssue`, `DockerProblem` and `Access` are replaced; the JSON output is checked byte for byte by the existing tests.
3. **Domain.** Create `src/domain/`; split `managed.rs` into store, parse, render and init, split `openssh.rs` along resolution, `known_hosts`, keys and connection outcomes, and move `adopt`, `tunnel`, `host`, `discovery` and the small shared modules in.
4. **CLI.** Split `cli.rs` into argument definitions and one module per command group; move `picker.rs` under `cli`.
5. **TUI.** Split `tui.rs` into the event loop, the terminal session and signals, the shared widgets, and one module per tab, joining the existing `tui/containers.rs`, `tui/tunnels.rs` and their siblings.
6. **Architecture document.** Add `docs/architecture.md` with the layers, the dependency rule and the module map as built, and link it from `CONTRIBUTING.md`.

Steps 1 and 2 come first because Proxmox, Podman and v5 build on them; steps 3 to 5 can land in any order after them.

## Not in this slice

- Proxmox, Podman, settings and the remembered container user (v4 slices 2 to 5).
- Any macOS or Windows implementation of `platform` (v5).
- A compiler- or CI-enforced layer check. Review keeps the dependency rule; a check can be added later if violations appear.

## As built

The slice shipped as specified, with these differences. `docs/architecture.md` describes the code as it is.

- `ListFailure::Issue` holds `Box<BackendIssue>`, and `Backend::list` takes `run: &mut dyn FnMut(&[String]) -> RunResult<E>` with `type RunResult<E> = Result<Output, RunFailure<E>>`; both satisfy clippy's `result_large_err` and `type_complexity`.
- The listing row types (`Container`, `Listing`, `group`, `display_name`, `resolve`) live in `backend::listing`, shared by every adapter; Docker keeps only the parsing of `docker ps`.
- `remote_command` and `quote` live in `backend` (the app's transport uses them); `Access` and `decide` are private to the Docker adapter.
- `picker.rs` stays at the crate root: the CLI and the TUI both use it, and it imports `TextInput` from the TUI. With no subcommand, the CLI starts the TUI. Both are recorded as the exceptions to "an interface never calls another".
- `Proc`'s tunnel-specific methods stay in `domain::tunnel::runtime`; the `/proc` readers are in `platform::process`.
- A few direct `libc` calls remain in `domain` (`tunnel::runtime`, `tunnel`, `adopt`); a macOS or Windows port must revisit them.
