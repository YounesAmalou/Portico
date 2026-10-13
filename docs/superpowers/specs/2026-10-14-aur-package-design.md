# Portico AUR Package Design

## Purpose

Let Arch Linux users install Portico with `yay -S portico`, without having Rust installed beforehand. This is a follow-up slice of the production-readiness effort (see `ROADMAP.md`).

## Decisions

- **One package, built from source, named `portico`.** AUR convention reserves the `-bin` suffix for prebuilt binaries, so a plain `portico` builds from source. `cargo` is a build-time dependency, which `yay` and `makepkg` install and remove, so users need nothing beforehand. The build takes about a minute.
- **Source:** the GitHub archive of the release tag (`v$pkgver`), pinned by SHA-256. It carries `Cargo.lock`, so `cargo fetch --locked` and `cargo build --frozen` reproduce the released dependency set.
- **Runtime dependencies:** `openssh` (Portico shells out to `ssh`), plus `glibc` and `gcc-libs`.
- **Architecture:** `x86_64` only, matching the supported target. aarch64 joins when it is supported and tested.
- **Verification in `check()`:** the library unit tests. The PTY and integration suites need a configured SSH environment and run in CI instead.
- **Debug package:** disabled (`!debug`); the release profile already strips the binary.
- **Prebuilt `portico-bin`:** deferred (roadmap). It can be added later without renaming this package.
- **Other platforms:** Homebrew, winget/scoop and `.deb` are separate packaging channels with their own designs.

## Components

1. `packaging/aur/PKGBUILD` and `.SRCINFO`, excluded from the crates.io package.
2. `scripts/aur-update.sh <version>`: downloads the tag archive, pins its checksum, resets `pkgrel`, regenerates `.SRCINFO`. It runs after the tag exists, because the archive does not exist before.
3. Documentation: README install section, CONTRIBUTING release step 5, CHANGELOG, ROADMAP (the AUR package moves out; `portico-bin` and automatic TUI refresh are recorded).
4. Tests in `tests/docs.rs` guard the PKGBUILD's shape and the documented steps.

## Maintainer actions

The first push to `ssh://aur@aur.archlinux.org/portico.git` needs the maintainer's AUR account and SSH key. Later releases follow CONTRIBUTING step 5. Automating the push from the release workflow, with an AUR SSH key as an environment secret, is a possible later slice.

## Verification

`makepkg --nodeps` builds the package from the real v0.1.0 archive, runs `check()`, and the resulting package installs `/usr/bin/portico` (which reports its version), the licenses and the docs. `scripts/ci-checks.sh` still passes.
