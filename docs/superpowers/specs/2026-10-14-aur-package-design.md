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
3. Documentation: README install section, CONTRIBUTING release step 5, CHANGELOG, ROADMAP (the AUR package moves out; `portico-bin` and live inventory reload are recorded).
4. Tests in `tests/docs.rs` guard the PKGBUILD's shape and the documented steps.

## Publishing

The release workflow gains an `aur` job beside `publish`; both depend only on `release`, so neither blocks or re-runs the other. It runs in its own `aur` environment (approval, `v*` tags only) so the AUR key and the crates.io token are never in the same job. `scripts/aur-publish.sh` pins the tag archive, builds the package with `makepkg` as a gate, and pushes `PKGBUILD` and `.SRCINFO` to the AUR; a version already on the AUR is a no-op, so a failed job can simply be re-run, for example after the maintainer's AUR account exists. The AUR host key is pinned in the repository rather than trusted on first use.

The maintainer registers an AUR account, adds a dedicated SSH key, stores its private half as `AUR_SSH_PRIVATE_KEY` in the `aur` environment, and makes the first push by hand (or re-runs the first failed `aur` job).

## Verification

`makepkg --nodeps` builds the package from the real v0.1.0 archive, runs `check()`, and the resulting package installs `/usr/bin/portico` (which reports its version), the licenses and the docs. `scripts/ci-checks.sh` still passes.
