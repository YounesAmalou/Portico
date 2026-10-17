# Roadmap

Scheduling an item here never cancels it. Items keep their requirements until they ship, and each needs its own design review before implementation.

## Rules that keep Portico simple

These bind every item below. A design that needs to break one must say so and be reviewed on that ground.

- **No daemon.** Portico runs when the user runs it. Anything long-lived (a tunnel) is an ordinary `ssh` process that Portico starts and later finds again, not a resident Portico service.
- **No database.** State is the OpenSSH config files Portico owns, plus at most small runtime files (for example the process record of a running tunnel).
- **No plugin system inside Portico.** Portico itself is not extensible. Desktop surfaces are separate consumers of Portico's CLI.
- **OpenSSH stays authoritative.** `ssh <alias>` keeps working for every managed host, and every change is validated by OpenSSH before it is activated.
- **Credentials stay with their owner.** Portico stores paths and references, never key material, and never grants access to a credential merely because a host is visible.

## Product slices

Each slice keeps the requirements recorded in the v1 design (`docs/superpowers/specs/2026-09-13-portico-host-management-design.md`) and adds what was decided since. Rough order, because later slices build on earlier ones:

1. **Connection test (manual).** A command the user runs on demand to check that a host's connection works: `portico test <alias>` on the CLI and a single key (`t`) on the selected host in the TUI, offered from the host list, the details pane and the add and edit forms (testing the values currently typed). A test passes only when the login succeeds, without opening a shell, so hosts that only allow port forwarding pass too. A first-time host key is shown with its fingerprint and trusted only when the user confirms. It is never triggered automatically, not on save and not on selection. Design: `docs/superpowers/specs/2026-10-17-connection-test-design.md`.
2. **Rich error messages.** When `ssh` fails, Portico explains why in plain words instead of passing on its last line of output. Each failure names its cause and, where one exists, the next step: wrong user or rejected credentials (and which keys were offered), a key that needs a passphrase or an agent, a host name that does not resolve, an unreachable host or closed port, a timeout, a failing jump host, an unknown host key, or a host key that has changed. The same explanations appear in the CLI and the TUI, for the connection test, connecting, and OpenSSH validation errors when saving. The raw `ssh` output stays available for anything Portico does not recognise.
3. **Safe adoption:** losslessly migrate a concrete external block only after showing an exact preview, creating a backup, and providing rollback.
4. **Tunnels (saved forwards, run on demand).** Model local, remote and dynamic forwards, save them, and start and stop them on demand. Requirements:
   - Saved tunnels live in Portico-owned configuration, are listed, edited, added and removed from both the CLI and the TUI, and the TUI treats them as a first-class screen: obvious add and edit forms with field validation, one key to start or stop, and a status column (stopped, running, failed, port in use).
   - Local conflicts are detected before starting (the local port is already bound, or two saved tunnels collide).
   - A started tunnel is an `ssh` process Portico can find again after Portico exits, without a resident Portico process. Where its state lives, and how it is stopped, are the central design questions.
   - Explicitly started only: nothing starts automatically.
5. **Machine-readable output.** The smallest `--json` contract (hosts, one host's details, tunnels and their status) that the desktop surface needs, defined by that consumer's requirements and carrying no credentials.
6. **Desktop surface (Quickshell).** A Quickshell component for quick connection and tunnel control: a short list of hosts to connect to, and the saved tunnels with their status and a start or stop action. It reads from the CLI contract above and does not duplicate management: adding, editing and adopting stay in the TUI and CLI.
   - **Quickshell first, Omarchy second.** Omarchy's shell is a Quickshell configuration, so one Quickshell component can serve both. On a plain Quickshell setup it runs as its own panel; on Omarchy it can additionally be packaged as an Omarchy plugin (a `plugin.json` with a bar widget and a panel entry point, as other plugins in `~/.config/omarchy/plugins` do) and offered through the Omarchy menu. The shared part is the panel; only the thin entry points differ.
   - Open design questions: bar widget versus launcher-style popup, how connecting opens a terminal (reuse the `xdg-terminal-exec` path), and how the panel learns of status changes without a daemon (poll the CLI on open).
7. **Identity awareness:** discover file references and agent/provider identities, including 1Password, while keeping private keys with their provider. Identity knowledge must remain separate from credential possession. Discovery and display only: Portico does not generate keys.
8. **More OpenSSH directives (optional, undecided).** Managed hosts support only host, user, port, identity file and ProxyJump today. Supporting more directives is wanted but must not overload the tool. Whether this ships as a curated set added one directive at a time, as a free-form "custom directive" field validated by OpenSSH, or both, is an open design question; the answer decides the slice's size. It stays optional: the default form does not grow.

## Production readiness

| Item | Prerequisite |
|---|---|
| Release automation with release-plz: a release pull request kept open on `main` that bumps the version and changelog, and that tags, releases and publishes to crates.io when merged | A GitHub App (the default workflow token neither triggers CI on its pull requests nor starts the release on its tags) added to the tag ruleset's bypass list; a decision on the changelog format (generated from commit messages) and on how `release.yml` keeps building the release tarballs. The pull-request rule on `main` stays. Rehearsed on GitHub before a real release relies on it |
| Project website and documentation: a static site that presents Portico (what it is, install, a short tour) and holds the user documentation now living in the README, kept in this repository and published from CI | A choice of generator (a docs-oriented static site tool) and hosting (GitHub Pages is the default candidate); the README stays the short entry point. Whatever is built is verified on GitHub, not only locally |
| Automated end-to-end tests with a real OpenSSH and a real terminal in CI | Its own design |
| AUR package `portico` (source build) | Built and merged; publishing is suspended until an AUR account and key exist. Then set the `aur` environment secret and re-run the failed job |
| AUR package `portico-bin` (prebuilt binary) | Demand for faster installs; the source package ships first |
| Live inventory reload when the config changes | Its own design: watch `~/.ssh/config` and every included file (or poll modification times), debounce editors that save by renaming, and never reshuffle the list while a form is open; the manual refresh key already ships |
| Linux release artifacts, ready to use without installing Rust: prebuilt tarballs (a static x86_64 musl build that runs on any distribution including Arch Linux, and an aarch64 build) and distribution packages (`.deb`, `.rpm` and an Arch Linux `.pkg.tar.zst`), each checksummed and attached to the GitHub Release | Its own design: the final target list, whether the aarch64 build needs native ARM runners in CI, how each package is built and verified in a clean container of its distribution, and how this fits the release-plz workflow; first check what the build and distribution channels already support natively |
| macOS support, distributed through Homebrew | The end-to-end tests, and a portable replacement for the Linux-specific process control (`pidfd`, `/proc`) and terminal launcher |
| Windows support, distributed through winget | The same portability work, plus a Windows terminal launcher |
| Signed releases and SBOM/provenance attestations | A decision on signing keys and attestation format |
| Trusted Publishing on crates.io | Configure the trust relationship on crates.io, then switch the publish job to OIDC and delete the stored token |
| Replace the initial crates.io token with a crate-scoped token | Maintainer action on crates.io (unnecessary once Trusted Publishing is in place) |

Cross-platform support has no agreed design yet: Homebrew and winget are the intended channels, and each platform's spec decides its own build, process-control and terminal-launcher approach.

## Not planned

Recorded so they are not mistaken for forgotten work:

- Connection history and session management.
- Key generation.
- Background reachability polling (the manual connection test above covers the need on demand).
- A daemon, a database, or a plugin system inside Portico (see the rules at the top).
- herdr integration.
