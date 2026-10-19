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

### v3: long-running work and the desktop

The desktop surface starts and stops saved tunnels and reads everything through one CLI contract, so these ship together.

1. **Machine-readable output.** The smallest `--json` contract (hosts, one host's details, their containers, and tunnels and their status) that the desktop surface needs, defined by that consumer's requirements and carrying no credentials.
2. **Desktop surface (Quickshell).** A Quickshell component for quick connection and tunnel control: a short list of hosts (and their containers) to connect to, and the saved tunnels with their status and a start or stop action. It reads from the CLI contract above and does not duplicate management: adding, editing and adopting stay in the TUI and CLI.
   - **Quickshell first, Omarchy second.** Omarchy's shell is a Quickshell configuration, so one Quickshell component can serve both. On a plain Quickshell setup it runs as its own panel; on Omarchy it can additionally be packaged as an Omarchy plugin (a `plugin.json` with a bar widget and a panel entry point, as other plugins in `~/.config/omarchy/plugins` do) and offered through the Omarchy menu. The shared part is the panel; only the thin entry points differ.
   - Open design questions: bar widget versus launcher-style popup, how connecting opens a terminal (reuse the `xdg-terminal-exec` path), and how the panel learns of status changes without a daemon (poll the CLI on open).

### v4: more places to open a shell

3. **Proxmox container shells.** The same list-and-shell flow for LXC containers on a Proxmox VE node Portico knows: list them with `pct list` over `ssh`, and open a shell with `pct exec <vmid>` as the chosen user, `bash` first and `sh` otherwise. It is a second backend behind the container shells picker, not a separate feature.
   - Open design questions, settled when the slice starts: `pct` needs root on the node, and whether stopped containers are listed.
4. **Podman.** `portico shell` supports Docker only and never run `podman`. Add Podman as a second engine behind the same picker, listing and shell. Open design questions, settled when the slice starts: rootless Podman's per-user containers, and whether `podman` and `docker` on one target are both listed.
5. **Remembered container user.** `portico shell` stores nothing, so the user chosen for a container's shell must be picked again each time. Remember the last user per Compose service (or container) so it becomes the default. It needs its own design for a small Portico-owned preferences file, within the rule of no database.

### Unscheduled

6. **More OpenSSH directives (optional, undecided).** Managed hosts support only host, user, port, identity file and ProxyJump today. Supporting more directives is wanted but must not overload the tool. Whether this ships as a curated set added one directive at a time, as a free-form "custom directive" field validated by OpenSSH, or both, is an open design question; the answer decides the slice's size. It stays optional: the default form does not grow.
7. **Pin an agent key to a host by writing its public half (optional, undecided).** With 1Password and other agents, choosing one key for a host means saving that key's public half as a `.pub` file and pointing IdentityFile at it (with `IdentitiesOnly yes`). Portico could offer that from `portico identities` or the host form. It writes no secret, but it would be the first key-related file Portico writes, so it needs its own design review on that ground.

## Production readiness

| Item | Prerequisite |
|---|---|
| Project website and documentation: a static site that presents Portico (what it is, install, a short tour) and holds the user documentation now living in the README, kept in this repository and published from CI | A choice of generator (a docs-oriented static site tool) and hosting (GitHub Pages is the default candidate); the README stays the short entry point. Whatever is built is verified on GitHub, not only locally |
| Leaner CI and CD, remainder: merge small jobs (for example `fmt` into `clippy`) so they share the setup each job pays (about 30 s: starting the container, installing packages and the toolchain) | Measured and left out: it saves runner time but no wall-clock time, because the jobs run in parallel and the slowest one sets the pace, and it renames checks that pull requests must pass. Revisit if runner time starts to matter |
| AUR package `portico` (source build) | Built and merged; publishing is suspended until an AUR account and key exist. Then set the `aur` environment secret and re-run the failed job |
| AUR package `portico-bin` (prebuilt binary) | Demand for faster installs; the source package ships first |
| Live inventory reload when the config changes | Its own design: watch `~/.ssh/config` and every included file (or poll modification times), debounce editors that save by renaming, and never reshuffle the list while a form is open; the manual refresh key already ships |
| Linux release artifacts, ready to use without installing Rust: prebuilt tarballs (a static x86_64 musl build that runs on any distribution including Arch Linux, and an aarch64 build) and distribution packages (`.deb`, `.rpm` and an Arch Linux `.pkg.tar.zst`), each checksummed and attached to the GitHub Release | Its own design: the final target list, whether the aarch64 build needs native ARM runners in CI, how each package is built and verified in a clean container of its distribution, and how this fits the release-plz workflow; first check what the build and distribution channels already support natively |
| macOS support, distributed through Homebrew | A portable replacement for the Linux-specific process control (`pidfd`, `/proc`) and terminal launcher, with the end-to-end tests running on macOS |
| Windows support, distributed through winget | The same portability work, plus a Windows terminal launcher |
| Signed releases and SBOM/provenance attestations | A decision on signing keys and attestation format |
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
