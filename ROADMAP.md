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
| Automated end-to-end tests with a real OpenSSH and a real terminal in CI | First tagged release |
| aarch64 Linux builds | Native ARM runners in CI |
| macOS support | The end-to-end tests, and a portable replacement for the Linux-specific process control (`pidfd`, `/proc`) and terminal launcher |
| Windows support | The same portability work, plus a Windows terminal launcher |
| AUR package | First tagged release with a published tarball |
| Homebrew formula | macOS support |
