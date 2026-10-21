# Roadmap

Scheduling an item here never cancels it. Items keep their requirements until they ship, and each needs its own design review before implementation. A shipped item leaves this file: the product slices delivered so far are listed under "Delivered follow-up slices" in the v1 design (`docs/superpowers/specs/2026-09-13-portico-host-management-design.md`), each design under `docs/superpowers/specs/` records its status, and `CHANGELOG.md` records every release.

## Rules that keep Portico simple

These bind every item below. A design that needs to break one must say so and be reviewed on that ground.

- **No daemon.** Portico runs when the user runs it. Anything long-lived (a tunnel) is an ordinary `ssh` process that Portico starts and later finds again, not a resident Portico service.
- **No database.** State is the OpenSSH config files Portico owns, plus at most small runtime files (for example the process record of a running tunnel).
- **No plugin system inside Portico.** Portico itself is not extensible. Desktop surfaces are separate consumers of Portico's CLI.
- **OpenSSH stays authoritative.** `ssh <alias>` keeps working for every managed host, and every change is validated by OpenSSH before it is activated.
- **Credentials stay with their owner.** Portico stores paths and references, never key material, and never grants access to a credential merely because a host is visible.

## Product slices

Each slice keeps the requirements recorded in the v1 design (`docs/superpowers/specs/2026-09-13-portico-host-management-design.md`) and adds what was decided since. Slices are grouped by the release they are planned for, and listed in rough order, because later slices build on earlier ones. Open design questions are settled when a slice starts, in its own design review.

### v4: a modular core, then more places to open a shell

The architecture slice comes first: Proxmox and Podman are the first new adapters built on it, and v5's macOS and Windows slices build on its platform layer.

1. **Modular architecture.** Several files have grown too large to read as one unit (`src/tui.rs` about 6,900 lines, `src/managed.rs` 3,400, `src/cli.rs` 2,900, `src/openssh.rs` 2,500, `src/adopt.rs` 2,100). Split them by responsibility, and give the crate layers that new backends, platforms and interfaces plug into without touching unrelated code:
   - **Core:** hosts, the inventory, the managed configuration, saved port forwards, and their validation. It knows nothing about the terminal UI, the CLI's output format or the operating system.
   - **Backends:** one adapter per way of reaching a shell (Docker today, then Proxmox and Podman), behind one shared interface: list what can be opened on a target, open a shell in it, and report what is missing (a binary, a permission). Docker moves behind it first, with no change in behaviour. Adapters are compiled into Portico and chosen at run time; this is not a plugin system (see the rules above).
   - **Platform:** everything specific to the operating system, behind one interface per concern: process control (today `pidfd` and `/proc`), the terminal launcher (today `xdg-terminal-exec`), configuration and runtime paths, and file watching. Linux is the only implementation in v4; v5 adds macOS and Windows.
   - **Interfaces:** the TUI and the CLI with its JSON output, each a thin layer over the core. Each TUI tab gets its own module for its state, input and rendering.
   - Requirements: no change in behaviour, CLI flags or JSON output. The unit, CLI, docs and end-to-end tests pass unchanged after every step, so the slice lands as a series of small refactors, not one rewrite. Adding a backend touches only its adapter and its registration. The design review sets a size guideline for files and records the module map in a short architecture document linked from `CONTRIBUTING.md`.
   - Open design questions, settled when the slice starts: whether the layers stay modules of one crate or become a Cargo workspace, and the one shape in which a backend reports what it needs so the TUI, the CLI and the desktop panel all show it the same way.
2. **Proxmox container shells.** The same list-and-shell flow for LXC containers on a Proxmox VE node Portico knows: list them with `pct list` over `ssh`, and open a shell with `pct exec <vmid>` as the chosen user, `bash` first and `sh` otherwise. It is an adapter on the backend interface from slice 1, behind the same picker, not a separate feature.
   - Open design questions, settled when the slice starts: `pct` needs root on the node, and whether stopped containers are listed.
3. **Podman.** `portico shell` supports Docker only and never runs `podman`. Add Podman as an adapter on the backend interface from slice 1, behind the same picker, listing and shell. Open design questions, settled when the slice starts: rootless Podman's per-user containers, and whether `podman` and `docker` on one target are both listed.
4. **Settings and preferences.** A settings view in the TUI, with CLI equivalents for scripts, backed by one small Portico-owned preferences file, within the rule of no database. With more backends, not every user wants every tab, and the desktop integration is managed from the CLI only today. The view holds:
   - **Tabs:** the tunnels tab and each container backend (Docker, Podman, Proxmox) can be turned on or off; the hosts tab is always on. Portico never runs a turned-off backend's tool (`docker`, `podman`, `pct`).
   - **Desktop integration**, as a setting of its own: the Quickshell panel's status as `portico desktop status` reports it (installed, out of date, missing pieces) and the actions tied to it (install, update, uninstall), with the same previews and confirmations as `portico desktop install | uninstall`.
   - Later preferences, starting with the remembered container user (slice 5).
   - Requirements: Portico works without the file, and its defaults match today's behaviour. The file is written atomically and stays readable and editable by hand. An invalid file is reported, never a crash. The desktop panel reads the same preferences through the JSON output, so it hides the same tabs.
   - Open design questions, settled when the slice starts: the file's format and location (through the platform layer), whether a turned-off backend's CLI commands are hidden or only its tab, and how a confirmation that edits a compositor file is shown inside the TUI.
5. **Remembered container user.** `portico shell` stores nothing, so the user chosen for a container's shell must be picked again each time. Remember the last user per Compose service (or container) so it becomes the default. It is stored in the preferences file from slice 4.

### v5: macOS and Windows

Builds on the platform layer from v4's modular architecture: each system is a new implementation of it, not a fork of the Linux code. Each system's design review decides its build, process control and terminal launcher.

6. **macOS support.** A macOS implementation of the platform layer: process control without `pidfd` or `/proc`, a terminal launcher (Terminal, iTerm2 and others), and macOS paths. Distributed through Homebrew, with release builds for Apple silicon and Intel and the end-to-end tests running on macOS in CI.
7. **Windows support.** A Windows implementation of the platform layer on the OpenSSH client that ships with Windows: process control for saved port forwards without Unix signals, a Windows Terminal launcher, and Windows paths. Distributed through winget, with the end-to-end tests running on Windows in CI (ConPTY in place of the Unix PTY harness).
   - Open design question, settled when the slice starts: how Docker Desktop and WSL fit in as targets.
8. **macOS and Windows desktop interfaces.** The counterpart of the Quickshell panel on each system: a menu bar app on macOS and a system tray app on Windows that list hosts, open shells (in hosts and their containers) in a new terminal, and start and stop saved port forwards. Like the Quickshell panel, each is a separate consumer of Portico's CLI and JSON output, not code inside Portico. Each is installed, updated and removed from the desktop integration setting (slice 4) on its system.
   - Footprint is a requirement, not a preference: each app stays as light as the TUI in memory, CPU when idle and download size.
   - Open design questions, settled when the slice starts: the toolkit, chosen by measuring the candidates available then against the footprint requirement (native toolkits such as SwiftUI and WinUI, cross-platform shells such as Tauri, and others), and how each is kept in step with the binary, as `portico desktop install` does on Linux.

### v6: share a port from anywhere

Saved port forwards work, but setting one up means knowing the target's address and filling in a separate form first. v6 turns it around: start from the host, container or Proxmox CT you are looking at, bring one of its ports to this machine, and save it only if you want to keep it. It builds on v4's backend interface, so it covers every backend at once.

9. **Quick port forwards.** A "forward a port" action on every row of the hosts and container lists, and a matching CLI command. It asks only what it cannot work out: the remote port, suggested from what the target reports (the ports a container exposes, or the ports listening on a host), and the local port, by default the same number or the next free one, bound to `127.0.0.1`. The forward starts at once and appears in the tunnels tab marked as temporary, with its local address ready to copy or open in a browser. A save action there turns it into a saved port forward that stays listed.
   - The tunnels tab is reworked around this flow: temporary and saved forwards in one list, each showing what it reaches by name (host, container or CT), not only by address.
   - Decided: a temporary forward is an ordinary `ssh` process like a saved one. It keeps running after Portico exits, until it is stopped from Portico or killed.
   - Open design question, settled when the slice starts: how a temporary forward is recorded so Portico finds it again, within the rule of no database.
10. **Forwards to containers and CTs.** A forward whose target is a container or a Proxmox CT, named the way the pickers name it (Compose project and service, container name, or CT ID), not by its IP. Portico looks up the current address through the backend each time the forward starts, so it survives a container being recreated (for example by a Coolify redeploy), and the far port can be any port inside the container, including one the host itself already uses.
   - Decided: an app that listens only on `127.0.0.1` inside its container cannot be reached from the host, and Portico does not force it. The documentation says so, and the forward reports that nothing answers instead of failing silently. This is revisited only if a clean way is found that installs nothing in the container and needs no extra privileges.
   - Open design question, settled when the slice starts: containers on networks the host cannot route to.

### Unscheduled

11. **More OpenSSH directives (optional, undecided).** Managed hosts support only host, user, port, identity file and ProxyJump today. Supporting more directives is wanted but must not overload the tool. Whether this ships as a curated set added one directive at a time, as a free-form "custom directive" field validated by OpenSSH, or both, is an open design question; the answer decides the slice's size. It stays optional: the default form does not grow.
12. **Pin an agent key to a host by writing its public half (optional, undecided).** With 1Password and other agents, choosing one key for a host means saving that key's public half as a `.pub` file and pointing IdentityFile at it (with `IdentitiesOnly yes`). Portico could offer that from `portico identities` or the host form. It writes no secret, but it would be the first key-related file Portico writes, so it needs its own design review on that ground.
13. **External adapters (optional, undecided).** Backends beyond the built-in ones (for example Kubernetes, Incus or distrobox), written and installed by others as separate executables (for example `portico-backend-<name>` on `PATH`) that answer a small, versioned JSON protocol mirroring the backend interface from v4's modular architecture: list what can be opened on a target, and give the command that opens a shell in it. No outside code is loaded into Portico, and Portico runs the command it is given the same way it runs its own. It keeps Portico itself closed but makes its list of backends open, so it needs a design review against the rule of no plugin system inside Portico before it is scheduled. Open design questions, settled when the slice starts: how an external adapter is found and trusted, and whether it gets its own tab or joins the container picker.

## Production readiness

| Item | Prerequisite |
|---|---|
| Project website and documentation: a static site that presents Portico (what it is, install, a short tour) and holds the user documentation now living in the README, kept in this repository and published from CI | A choice of generator (a docs-oriented static site tool) and hosting (GitHub Pages is the default candidate); the README stays the short entry point. Whatever is built is verified on GitHub, not only locally |
| AUR package `portico` (source build) | Built and merged; publishing is suspended until an AUR account and key exist. Then set the `aur` environment secret and re-run the failed job |
| AUR package `portico-bin` (prebuilt binary) | Demand for faster installs; the source package ships first |
| Trusted Publishing on crates.io | Configure the trust relationship on crates.io, then switch the publish job to OIDC and delete the stored token |
| Replace the initial crates.io token with a crate-scoped token | Maintainer action on crates.io (unnecessary once Trusted Publishing is in place) |

macOS and Windows support, including Homebrew and winget distribution, are scheduled as product slices in v5 above.

## Not planned

Recorded so they are not mistaken for forgotten work:

- Connection history and session management.
- Key generation.
- An identity indicator in the host list: the host details and `portico identities` show identities on demand.
- Container management beyond a shell: logs, restarts, Compose `up` and `down`, and starting or stopping Proxmox containers.
- Background reachability polling (`portico test` covers the need on demand).
- Merging small CI jobs (for example `fmt` into `clippy`) to share their setup: the jobs run in parallel, so it saves runner time but no wall-clock time, and it would rename checks that pull requests must pass.
- A daemon, a database, or a plugin system inside Portico (see the rules at the top).
- herdr integration.
