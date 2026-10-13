# Roadmap

Scheduling an item here never cancels it. Items keep their requirements until they ship, and each needs its own design review before implementation.

## Product slices

Carried over from the v1 design (`docs/superpowers/specs/2026-09-13-portico-host-management-design.md`):

1. **Safe adoption:** losslessly migrate a concrete external block only after showing an exact preview, creating a backup, and providing rollback.
2. **Identity awareness:** discover file references and agent/provider identities, including 1Password, while keeping private keys with their provider. Identity knowledge must remain separate from credential possession.
3. **Tunnels:** model local, remote, and dynamic forwards; detect local conflicts; and supervise explicitly started tunnels.
4. **Desktop surface:** add a lightweight Quickshell launcher/status view rather than duplicating management UI.
5. **Tool integration:** expose the minimum machine-readable contract required by herdr without granting credential access merely because a host is visible.
6. **TUI refresh:** add a refresh key that discards cached host details and re-reads the inventory, so edits made outside Portico (including changes to external included configs) appear without restarting. v1 caches details until Portico itself changes the inventory.

## Production readiness

| Item | Prerequisite |
|---|---|
| Automated end-to-end tests with a real OpenSSH and a real terminal in CI | Its own design; v0.1.0 is released |
| aarch64 Linux builds | Native ARM runners in CI |
| macOS support | The end-to-end tests, and a portable replacement for the Linux-specific process control (`pidfd`, `/proc`) and terminal launcher |
| Windows support | The same portability work, plus a Windows terminal launcher |
| AUR package | A published release tarball (available since v0.1.0) |
| Homebrew formula | macOS support |
| Signed releases and SBOM/provenance attestations | A decision on signing keys and attestation format |
| Trusted Publishing on crates.io | Configure the trust relationship on crates.io, then switch the publish job to OIDC and delete the stored token |
| Release automation (a release pull request that bumps the version and changelog) | A commit-message or label convention to derive the changelog from |
| Replace the initial crates.io token with a crate-scoped token | Maintainer action on crates.io (unnecessary once Trusted Publishing is in place) |
