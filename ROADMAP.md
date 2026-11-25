# Roadmap

Scheduling an item here never cancels it. Items keep their requirements until they ship, and each needs its own design review before implementation. A shipped item leaves this file: the product slices delivered so far are listed under "Delivered follow-up slices" in the v1 design (`docs/superpowers/specs/2026-09-13-portico-host-management-design.md`), each design under `docs/superpowers/specs/` records its status, and `CHANGELOG.md` records every release.

## Rules that keep Portico simple

These bind every item below. A design that needs to break one must say so and be reviewed on that ground.

- **No daemon.** Portico runs when the user runs it. Anything long-lived (a tunnel) is an ordinary `ssh` process that Portico starts and later finds again, not a resident Portico service.
- **No database.** State is the OpenSSH config files Portico owns, plus at most small runtime files (for example the process record of a running tunnel) and one small hand-editable preferences file (`~/.config/portico/settings.toml`).
- **No plugin system inside Portico.** Portico itself is not extensible. Desktop surfaces are separate consumers of Portico's CLI.
- **OpenSSH stays authoritative.** `ssh <alias>` keeps working for every managed host, and every change is validated by OpenSSH before it is activated.
- **Credentials stay with their owner.** Portico stores paths and references, never key material, and never grants access to a credential merely because a host is visible.

## Product slices

Each slice keeps the requirements recorded in the v1 design (`docs/superpowers/specs/2026-09-13-portico-host-management-design.md`) and adds what was decided since. Slices are grouped by the release they are planned for, and listed in rough order, because later slices build on earlier ones. Open design questions are settled when a slice starts, in its own design review.

### v4: settings and preferences

1. **Remembered container user.** `portico shell` stores nothing, so the user chosen for a container's shell must be picked again each time. Remember the last user per Compose service (or container) so it becomes the default. It is stored in the preferences file (`portico settings`).

### v5: Windows and the desktop interfaces

Builds on the platform layer (`docs/architecture.md`): each system is a new implementation of it, not a fork of the Linux code, as macOS already is. Each system's design review decides its build, process control and terminal launcher.

2. **Windows support.** A Windows implementation of the platform layer (beside `unix`, `linux` and `macos` in `src/platform/`) on the OpenSSH client that ships with Windows: process control for saved port forwards without Unix signals, a Windows Terminal launcher, and Windows paths. Distributed through winget, with the end-to-end tests running on Windows in CI (ConPTY in place of the Unix PTY harness).
   - Open design question, settled when the slice starts: how Docker Desktop and WSL fit in as targets.
3. **macOS and Windows desktop interfaces.** The counterpart of the Quickshell panel on each system: a menu bar app on macOS and a system tray app on Windows that list hosts, open shells (in hosts and their containers) in a new terminal, and start and stop saved port forwards. Like the Quickshell panel, each is a separate consumer of Portico's CLI and JSON output, not code inside Portico. Each is installed, updated and removed from the desktop integration setting in the TUI's settings on its system.
   - Footprint is a requirement, not a preference: each app stays as light as the TUI in memory, CPU when idle and download size.
   - Open design questions, settled when the slice starts: the toolkit, chosen by measuring the candidates available then against the footprint requirement (native toolkits such as SwiftUI and WinUI, cross-platform shells such as Tauri, and others), and how each is kept in step with the binary, as `portico desktop install` does on Linux.

### v6: share a port from anywhere

Saved port forwards work, but setting one up means knowing the target's address and filling in a separate form first. v6 turns it around: start from the host, container or Proxmox CT you are looking at, bring one of its ports to this machine, and save it only if you want to keep it. It builds on the backend interface (`docs/architecture.md`), so it covers every backend at once.

4. **Quick port forwards.** A "forward a port" action on every row of the hosts and container lists, and a matching CLI command. It asks only what it cannot work out: the remote port, suggested from what the target reports (the ports a container exposes, or the ports listening on a host), and the local port, by default the same number or the next free one, bound to `127.0.0.1`. The forward starts at once and appears in the tunnels tab marked as temporary, with its local address ready to copy or open in a browser. A save action there turns it into a saved port forward that stays listed.
   - The tunnels tab is reworked around this flow: temporary and saved forwards in one list, each showing what it reaches by name (host, container or CT), not only by address.
   - Decided: a temporary forward is an ordinary `ssh` process like a saved one. It keeps running after Portico exits, until it is stopped from Portico or killed.
   - Open design question, settled when the slice starts: how a temporary forward is recorded so Portico finds it again, within the rule of no database.
5. **Forwards to containers and CTs.** A forward whose target is a container or a Proxmox CT, named the way the pickers name it (Compose project and service, container name, or CT ID), not by its IP. Portico looks up the current address through the backend each time the forward starts, so it survives a container being recreated (for example by a Coolify redeploy), and the far port can be any port inside the container, including one the host itself already uses.
   - Decided: an app that listens only on `127.0.0.1` inside its container cannot be reached from the host, and Portico does not force it. The documentation says so, and the forward reports that nothing answers instead of failing silently. This is revisited only if a clean way is found that installs nothing in the container and needs no extra privileges.
   - Open design question, settled when the slice starts: containers on networks the host cannot route to.

### Unscheduled

6. **More OpenSSH directives (optional, undecided).** Managed hosts support only host, user, port, identity file and ProxyJump today. Supporting more directives is wanted but must not overload the tool. Whether this ships as a curated set added one directive at a time, as a free-form "custom directive" field validated by OpenSSH, or both, is an open design question; the answer decides the slice's size. It stays optional: the default form does not grow.
7. **Pin an agent key to a host by writing its public half (optional, undecided).** With 1Password and other agents, choosing one key for a host means saving that key's public half as a `.pub` file and pointing IdentityFile at it (with `IdentitiesOnly yes`). Portico could offer that from `portico identities` or the host form. It writes no secret, but it would be the first key-related file Portico writes, so it needs its own design review on that ground.
8. **External adapters (optional, undecided).** Backends beyond the built-in ones (for example Kubernetes, Incus or distrobox), written and installed by others as separate executables (for example `portico-backend-<name>` on `PATH`) that answer a small, versioned JSON protocol mirroring the backend interface (`docs/architecture.md`): list what can be opened on a target, and give the command that opens a shell in it. No outside code is loaded into Portico, and Portico runs the command it is given the same way it runs its own. It keeps Portico itself closed but makes its list of backends open, so it needs a design review against the rule of no plugin system inside Portico before it is scheduled. Open design questions, settled when the slice starts: how an external adapter is found and trusted, and whether it gets its own tab or joins the container picker.

## Production readiness

| Item | Prerequisite |
|---|---|
| Project website and documentation: a static site that presents Portico (what it is, install, a short tour) and holds the user documentation now living in the README, kept in this repository and published from CI | A choice of generator (a docs-oriented static site tool) and hosting (GitHub Pages is the default candidate); the README stays the short entry point. Whatever is built is verified on GitHub, not only locally |
| AUR package `portico` (source build) | Built and merged; publishing is suspended until an AUR account and key exist. Then set the `aur` environment secret and re-run the failed job |
| AUR package `portico-bin` (prebuilt binary) | Demand for faster installs; the source package ships first |
| Homebrew tap `portico` (prebuilt macOS binaries) | Built and wired into the release; publishing waits until the tap repository `homebrew-tap` and the `homebrew` environment's `HOMEBREW_TAP_TOKEN` exist (see `docs/superpowers/specs/2026-11-24-macos-support-design.md`). Until then the release skips the tap |
| Trusted Publishing on crates.io | Configure the trust relationship on crates.io, then switch the publish job to OIDC and delete the stored token |
| Replace the initial crates.io token with a crate-scoped token | Maintainer action on crates.io (unnecessary once Trusted Publishing is in place) |

Windows support, including winget distribution, is scheduled as a product slice in v5 above.

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
