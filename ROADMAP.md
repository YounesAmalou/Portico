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

### v4: more places to open a shell

1. **Proxmox container shells.** The same list-and-shell flow for LXC containers on a Proxmox VE node Portico knows: list them with `pct list` over `ssh`, and open a shell with `pct exec <vmid>` as the chosen user, `bash` first and `sh` otherwise. It is a second backend behind the container shells picker, not a separate feature.
   - Open design questions, settled when the slice starts: `pct` needs root on the node, and whether stopped containers are listed.
2. **Podman.** `portico shell` supports Docker only and never run `podman`. Add Podman as a second engine behind the same picker, listing and shell. Open design questions, settled when the slice starts: rootless Podman's per-user containers, and whether `podman` and `docker` on one target are both listed.
3. **Remembered container user.** `portico shell` stores nothing, so the user chosen for a container's shell must be picked again each time. Remember the last user per Compose service (or container) so it becomes the default. It needs its own design for a small Portico-owned preferences file, within the rule of no database.

### Unscheduled

4. **More OpenSSH directives (optional, undecided).** Managed hosts support only host, user, port, identity file and ProxyJump today. Supporting more directives is wanted but must not overload the tool. Whether this ships as a curated set added one directive at a time, as a free-form "custom directive" field validated by OpenSSH, or both, is an open design question; the answer decides the slice's size. It stays optional: the default form does not grow.
5. **Pin an agent key to a host by writing its public half (optional, undecided).** With 1Password and other agents, choosing one key for a host means saving that key's public half as a `.pub` file and pointing IdentityFile at it (with `IdentitiesOnly yes`). Portico could offer that from `portico identities` or the host form. It writes no secret, but it would be the first key-related file Portico writes, so it needs its own design review on that ground.
6. **TUI settings panel (desktop integration and other preferences from the TUI).** `portico desktop install | uninstall | status` is CLI-only. A settings view in the TUI would show and change the same desktop integration (panel, launcher entry, keybinding) and later preferences, with the same previews and confirmations. Open design questions, settled when the slice starts: which preferences exist besides the desktop integration, and how a confirmation that edits a compositor file is shown inside the TUI.

## Production readiness

| Item | Prerequisite |
|---|---|
| Project website and documentation: a static site that presents Portico (what it is, install, a short tour) and holds the user documentation now living in the README, kept in this repository and published from CI | A choice of generator (a docs-oriented static site tool) and hosting (GitHub Pages is the default candidate); the README stays the short entry point. Whatever is built is verified on GitHub, not only locally |
| Leaner CI and CD, remainder: merge small jobs (for example `fmt` into `clippy`) so they share the setup each job pays (about 30 s: starting the container, installing packages and the toolchain) | Measured and left out: it saves runner time but no wall-clock time, because the jobs run in parallel and the slowest one sets the pace, and it renames checks that pull requests must pass. Revisit if runner time starts to matter |
| AUR package `portico` (source build) | Built and merged; publishing is suspended until an AUR account and key exist. Then set the `aur` environment secret and re-run the failed job |
| AUR package `portico-bin` (prebuilt binary) | Demand for faster installs; the source package ships first |
| macOS support, distributed through Homebrew | A portable replacement for the Linux-specific process control (`pidfd`, `/proc`) and terminal launcher, with the end-to-end tests running on macOS |
| Windows support, distributed through winget | The same portability work, plus a Windows terminal launcher |
| Trusted Publishing on crates.io | Configure the trust relationship on crates.io, then switch the publish job to OIDC and delete the stored token |
| Replace the initial crates.io token with a crate-scoped token | Maintainer action on crates.io (unnecessary once Trusted Publishing is in place) |

Cross-platform support has no agreed design yet: Homebrew and winget are the intended channels, and each platform's spec decides its own build, process-control and terminal-launcher approach.

## Not planned

Recorded so they are not mistaken for forgotten work:

- Connection history and session management.
- Key generation.
- An identity indicator in the host list: the host details and `portico identities` show identities on demand.
- Container management beyond a shell: logs, restarts, Compose `up` and `down`, and starting or stopping Proxmox containers.
- Background reachability polling (`portico test` covers the need on demand).
- A daemon, a database, or a plugin system inside Portico (see the rules at the top).
- herdr integration.
