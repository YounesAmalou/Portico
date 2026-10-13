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

1. **Connection test.** When a host is added or edited, and again after it is saved, run a quick non-interactive check that the connection works, with a short timeout, and show the result next to the host (reachable, host key unknown, authentication refused, timed out). It must never prompt, never hang the TUI, and never send credentials Portico does not already use for a normal connection. Also available from the CLI. Open design questions: which exact check (`ssh` with `BatchMode` and a command-less handshake, or a plain TCP probe first), how to treat a first-time host key, and whether a failed test blocks saving (it should not; it warns).
2. **Safe adoption:** losslessly migrate a concrete external block only after showing an exact preview, creating a backup, and providing rollback.
3. **Tunnels (saved forwards, run on demand).** Model local, remote and dynamic forwards, save them, and start and stop them on demand. Requirements:
   - Saved tunnels live in Portico-owned configuration, are listed, edited, added and removed from both the CLI and the TUI, and the TUI treats them as a first-class screen: obvious add and edit forms with field validation, one key to start or stop, and a status column (stopped, running, failed, port in use).
   - Local conflicts are detected before starting (the local port is already bound, or two saved tunnels collide).
   - A started tunnel is an `ssh` process Portico can find again after Portico exits, without a resident Portico process. Where its state lives, and how it is stopped, are the central design questions.
   - Explicitly started only: nothing starts automatically.
4. **Machine-readable output.** The smallest `--json` contract (hosts, one host's details, tunnels and their status) that the desktop surface needs, defined by that consumer's requirements and carrying no credentials.
5. **Desktop surface (Quickshell).** A Quickshell component for quick connection and tunnel control: a short list of hosts to connect to, and the saved tunnels with their status and a start or stop action. It reads from the CLI contract above and does not duplicate management: adding, editing and adopting stay in the TUI and CLI.
   - **Quickshell first, Omarchy second.** Omarchy's shell is a Quickshell configuration, so one Quickshell component can serve both. On a plain Quickshell setup it runs as its own panel; on Omarchy it can additionally be packaged as an Omarchy plugin (a `plugin.json` with a bar widget and a panel entry point, as other plugins in `~/.config/omarchy/plugins` do) and offered through the Omarchy menu. The shared part is the panel; only the thin entry points differ.
   - Open design questions: bar widget versus launcher-style popup, how connecting opens a terminal (reuse the `xdg-terminal-exec` path), and how the panel learns of status changes without a daemon (poll the CLI on open).
6. **Identity awareness:** discover file references and agent/provider identities, including 1Password, while keeping private keys with their provider. Identity knowledge must remain separate from credential possession. Discovery and display only: Portico does not generate keys.
7. **More OpenSSH directives (optional, undecided).** Managed hosts support only host, user, port, identity file and ProxyJump today. Supporting more directives is wanted but must not overload the tool. Whether this ships as a curated set added one directive at a time, as a free-form "custom directive" field validated by OpenSSH, or both, is an open design question; the answer decides the slice's size. It stays optional: the default form does not grow.

## Production readiness

| Item | Prerequisite |
|---|---|
| Release guard: a release that cannot be tagged from the wrong commit (the `v0.2.0` and `v0.2.1` tags both landed on a commit whose version had not been bumped) | A check that the tagged commit is on `main`, carries the matching Cargo version and has a changelog section, ideally before the tag can be pushed; extends the release automation item below |
| Release automation (a release pull request that bumps the version and changelog) | A commit-message or label convention to derive the changelog from |
| Automated end-to-end tests with a real OpenSSH and a real terminal in CI | Its own design |
| AUR package `portico` (source build) | Built and merged; publishing is suspended until an AUR account and key exist. Then set the `aur` environment secret and re-run the failed job |
| AUR package `portico-bin` (prebuilt binary) | Demand for faster installs; the source package ships first |
| Live inventory reload when the config changes | Its own design: watch `~/.ssh/config` and every included file (or poll modification times), debounce editors that save by renaming, and never reshuffle the list while a form is open; the manual refresh key already ships |
| aarch64 Linux builds | First check whether the build and distribution channels support the architecture natively; add native ARM runners in CI if needed, and decide the fallback only if they do not |
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
- Background reachability polling (the connection test above covers the need on demand).
- A daemon, a database, or a plugin system inside Portico (see the rules at the top).
- herdr integration.
