# Linux release artifacts: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Implements `docs/superpowers/specs/2026-10-21-linux-release-artifacts-design.md`.

1. Make the musl build warning-free: `src/tunnel.rs` stops naming `libc::time_t`, which is deprecated on musl; the type is inferred from `localtime_r`.
2. Packaging metadata: `[package.metadata.deb]` and `[package.metadata.generate-rpm]` in `Cargo.toml` (package `portico`, OpenSSH client dependency, the AUR package's file list); `packaging/arch/PKGBUILD`, which repackages the prebuilt binary.
3. Scripts:
   - `scripts/package-release.sh <version> <target> <binary> <outdir>`: the tarball named after the target, without a per-file `.sha256`.
   - `scripts/package-arch.sh <version> <binary> <outdir>`: stages the files beside the PKGBUILD, sets `pkgver`, runs `makepkg` as an unprivileged user when started as root.
   - `scripts/build-release.sh <version> <target> <outdir>`: build (aarch64 linked with `rust-lld`), tarball, `.deb`, `.rpm`, CycloneDX SBOM, and for x86_64 the Arch package.
   - `scripts/verify-packages.sh <version> <dir> [arch]`: each artifact installed in a fresh container of its distribution (Alpine, Debian, Fedora, Arch Linux), `portico --version` compared with the version.
4. `release.yml`: `build` becomes a matrix over the two musl targets in `archlinux:base-devel` with `id-token: write` and `attestations: write`, attesting provenance and the SBOM; new `verify-packages` matrix (`ubuntu-latest`, `ubuntu-24.04-arm`); new `checksums` job writing and attesting `SHA256SUMS` and uploading `release-dist`; `release` needs `checksums`. `actions/attest` pinned by commit SHA.
5. Documentation: README install section (packages, tarballs, `SHA256SUMS`, `gh attestation verify`); `CONTRIBUTING.md`'s release description; both production-readiness rows removed from `ROADMAP.md`.
6. Tests: `tests/release_scripts.rs` for the renamed tarball; `tests/docs.rs` for the workflow's targets, runners, permissions and attestations, the README's verification commands, the package metadata, and the roadmap rows that left.
7. Rehearse locally in Docker (`scripts/build-release.sh` for both targets, `scripts/verify-packages.sh` for both architectures, aarch64 under QEMU), run `scripts/ci-checks.sh` as a regular user, then rehearse on GitHub with `gh workflow run release.yml --ref <branch>` and check that the run lists every artifact and attestation (`gh attestation verify` against the downloaded files).
