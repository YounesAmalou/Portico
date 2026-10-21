# Linux Release Artifacts Design

**Status:** Approved; implemented and rehearsed on GitHub: a manual run of the release workflow from the branch (run 39581746766) built, verified, checksummed and attested every artifact, and `gh attestation verify` accepted each downloaded file, the extracted binary and the SBOM attestations; first used by the next tagged release
**Date:** 2026-10-21

## Purpose

A release today attaches one artifact: an x86_64 glibc tarball built on current Arch Linux, which older distributions cannot run (the README sends their users to Cargo). Installing Portico without Rust should work on any Linux distribution and through each family's own package manager, and a user should be able to check that a download is what this repository's release workflow built.

This design delivers two rows of `ROADMAP.md`'s "Production readiness" table as one piece of work, and both rows leave the roadmap with it:

- **Linux release artifacts:** static tarballs for x86_64 and aarch64, plus `.deb`, `.rpm` and Arch Linux `.pkg.tar.zst` packages, all checksummed in one `SHA256SUMS` and attached to the GitHub Release.
- **Signed releases and SBOM/provenance attestations:** every artifact and `SHA256SUMS` get keyless build provenance, and the binary gets an attested SBOM. The row's prerequisite, "a decision on signing keys and attestation format", is settled below: there are no signing keys to manage, because GitHub's artifact attestations sign with a short-lived Sigstore certificate issued to the workflow run through OIDC, and the formats are SLSA build provenance and CycloneDX.

Out of scope: GPG signatures (they matter for an apt or yum repository, and none is planned), hosting an apt or yum repository, `portico-bin` on the AUR, and macOS and Windows.

## What the build and distribution channels already support

Checked before choosing any tool, so that nothing is added that an existing channel already does:

- **Rust (rustup):** `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` are standard targets that ship musl's C runtime, and every Rust toolchain bundles `rust-lld`. Portico's dependencies are pure Rust (no `cc` build), so `rust-lld` links a static aarch64 binary on an x86_64 machine with no cross toolchain. Tried locally: both targets build and link statically, and the whole test suite, end-to-end tests included, passes against the x86_64 musl build.
- **Arch Linux repositories:** the existing release and CI jobs already run in `archlinux:base-devel` and install `cargo-audit` and `cargo-deny` prebuilt with `pacman`. The same repositories package `cargo-deb`, `cargo-generate-rpm` and `cargo-cyclonedx`, and the container has `makepkg`. Every packaging tool is therefore a `pacman -S` away, with nothing compiled in CI.
- **GitHub:** Releases host the files; artifact attestations are native and free for public repositories (signed through Sigstore's public-good instance, stored with the repository, checked with `gh attestation verify`); `ubuntu-24.04-arm` runners are free for public repositories, so aarch64 packages can be installed and run on real aarch64 hardware.
- **release-plz:** it only tags (its GitHub Release creation and publishing are switched off on purpose, see `2026-10-16-release-plz-design.md`) and builds no binaries, so it is unaffected.
- **cargo-dist** was considered and rejected: it builds tarballs and can attest them, but it makes no `.deb`, `.rpm` or Arch package, and it generates and owns its release workflow, which would replace `release.yml`'s tag verification, CI reuse and separated publishing environments.
- **actions/attest:** `actions/attest-build-provenance` and `actions/attest-sbom` are now thin wrappers around `actions/attest` (the SBOM one warns that it is deprecated). The workflow calls `actions/attest` directly for both kinds of attestation, pinned by commit SHA like every other action.

## Settled questions

### Target list

| Artifact | x86_64 | aarch64 |
|---|---|---|
| Tarball (static musl binary, licenses, README) | `portico-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` | `portico-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` |
| Debian package | `portico_X.Y.Z-1_amd64.deb` | `portico_X.Y.Z-1_arm64.deb` |
| RPM package | `portico-X.Y.Z-1.x86_64.rpm` | `portico-X.Y.Z-1.aarch64.rpm` |
| Arch Linux package | `portico-X.Y.Z-1-x86_64.pkg.tar.zst` | none |
| CycloneDX SBOM of the binary | `portico-vX.Y.Z-x86_64-unknown-linux-musl.cdx.json` | `portico-vX.Y.Z-aarch64-unknown-linux-musl.cdx.json` |

Plus one `SHA256SUMS` covering every file above.

- **Both binaries are static musl builds.** One binary per architecture runs on every distribution, Arch Linux included, whatever its glibc. The glibc tarball (`…-x86_64-unknown-linux-gnu.tar.gz`) is no longer built, and the README's note that release binaries need a recent glibc goes away. Musl's allocator is slower than glibc's, which does not matter for an interactive tool that manages a config file.
- **Every package carries the static binary**, so its only dependency is the OpenSSH client (`openssh-client` on Debian, `openssh-clients` on Fedora, `openssh` on Arch). Each installs the same files as the AUR package: `/usr/bin/portico`, the licenses, the README and changelog, the launcher entry and its icon.
- **The package name is `portico`**, matching the binary and the AUR package (the crate's name, `portico-ssh`, exists only because `portico` was taken on crates.io). Installing the release's Arch package over the AUR one is an ordinary upgrade of the same package.
- **No aarch64 Arch Linux package:** Arch Linux supports only x86_64; Arch Linux ARM is a separate project with no official container image to verify a package in. Its users can use the aarch64 tarball.
- **The SBOM is per target** (`cargo cyclonedx --target`), because a dependency can be platform-specific, and leaves out build-only dependencies (`--no-build-deps`). It is a superset of what the binary links: `cargo metadata`, which it reads, unifies features with the dev-dependencies, so it also lists packages such as `termwiz` and `pest` that the release build never compiles (169 components against the 87 that `cargo tree -e normal` shows for v0.6.0, every one of which it includes). A superset is the safe direction for matching advisories. `cargo auditable` with `syft`, also packaged by Arch, was tried as the alternative and reports the same superset, because cargo-auditable reads the same metadata.

### aarch64 is cross-compiled; ARM runners only verify

Building needs no ARM runner: the aarch64 build runs in the same Arch Linux container as x86_64, linked by `rust-lld` (`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld`). The Arch Linux image exists only for x86_64, and the packaging tools come from its repositories, so a native ARM build would need another image and other tool sources for no benefit. Running the aarch64 binary does need aarch64, so the verification of aarch64 artifacts runs on GitHub's `ubuntu-24.04-arm` runner. Locally, the same verification runs under QEMU through binfmt.

### How each artifact is built

`scripts/build-release.sh <version> <target> <outdir>` builds one target, and the release workflow runs it in `archlinux:base-devel` exactly as a local rehearsal does:

1. `cargo build --release --locked --target <target>` (the release profile already strips the binary).
2. The tarball: `scripts/package-release.sh` (now taking the target, and no longer writing a per-file `.sha256`).
3. The `.deb`: `cargo deb --no-build --no-strip --target <target>`, from `[package.metadata.deb]` in `Cargo.toml`.
4. The `.rpm`: `cargo generate-rpm --target <target>`, from `[package.metadata.generate-rpm]` in `Cargo.toml` (automatic library requirements off: the binary links nothing).
5. The SBOM: `cargo cyclonedx --format json --spec-version 1.5 --no-build-deps --target <target>`.
6. For x86_64 only, the Arch package: `scripts/package-arch.sh` stages the binary and its files beside `packaging/arch/PKGBUILD`, sets `pkgver`, and runs `makepkg` as an unprivileged user (makepkg refuses root). The PKGBUILD only copies files; the AUR's source PKGBUILD in `packaging/aur/` is unchanged.

Package metadata lives in `Cargo.toml` because cargo-deb and cargo-generate-rpm read it there, together with the version, description, license and homepage they take from `[package]`; the version therefore cannot drift from the crate's.

### How each artifact is verified

`scripts/verify-packages.sh <version> <dir> [arch]` installs each artifact of one architecture in a clean container of its own distribution and requires `portico --version` to print `portico <version>`:

| Artifact | Container | Install |
|---|---|---|
| Tarball | `alpine` (musl, no glibc: proves the binary is static) | unpack, link onto `PATH` |
| `.deb` | `debian:stable-slim` | `apt-get install ./…deb` (pulls in `openssh-client`) |
| `.rpm` | `fedora` | `dnf install ./…rpm` (pulls in `openssh-clients`) |
| `.pkg.tar.zst` (x86_64) | `archlinux` | `pacman -U` (pulls in `openssh`) |

Each container is pulled fresh and removed afterwards, so nothing carries over from the build or from another check. Installing through the distribution's own package manager also proves that the declared dependency exists under that name.

### How it fits `release.yml` and release-plz

Nothing changes upstream of the build: the tag push (from release-plz) still verifies the tag against the Cargo version and the changelog, reuses `main`'s CI result or runs CI, and only then builds. The `build` job and what follows become:

```
verify ─┬─ checks ─ build (x86_64, aarch64) ─ verify-packages (x86_64, aarch64) ─ checksums ─ release ─┬─ crates
ci-passed ┘                                                                                              └─ aur
```

- **`build`** (matrix over the two targets, x86_64 runner, `archlinux:base-devel`): installs `rustup`, `cargo-deb`, `cargo-generate-rpm` and `cargo-cyclonedx` with `pacman`, runs `scripts/build-release.sh`, attests, and uploads `dist-<target>`.
- **`verify-packages`** (matrix: `ubuntu-latest` for x86_64, `ubuntu-24.04-arm` for aarch64): downloads that target's artifacts and runs `scripts/verify-packages.sh` with the runner's Docker. It needs no permission beyond reading the repository.
- **`checksums`**: downloads every target's artifacts, writes `SHA256SUMS` with `sha256sum`, checks it, attests it, and uploads `release-dist`, the complete set of release files.
- **`release`** (tags only, unchanged permissions): creates the GitHub Release with every file of `release-dist`.
- **`crates` and `aur`** are unchanged and stay independent of each other.

A manual run is still a rehearsal: it builds, verifies, checksums and attests everything, then stops before `release`; nothing is released or published. Its attestations are real, but they are bound to the branch the rehearsal ran from and only to the digests of files that were never released; `--source-ref refs/tags/vX.Y.Z` in the verification command below rejects anything not built from a release tag.

### Attestations

- **Provenance:** in `build`, one `actions/attest` step with every file in that target's `dist` (tarball, packages, SBOM) and the bare binary as subjects; in `checksums`, one step for `SHA256SUMS`. The bare binary is a subject so that an extracted or installed `portico` can be verified too, not only the archive it came in.
- **SBOM:** in `build`, one `actions/attest` step with `sbom-path` set to that target's CycloneDX file and the binary, tarball and packages as subjects.
- **Permissions:** the workflow default stays `contents: read`. Only `build` and `checksums` are granted `id-token: write` and `attestations: write`, each attests only what it produced in the same job, and neither can write to the repository or its releases. `release` keeps `contents: write` alone.

### How users verify a download

Documented in the README's install section:

```sh
sha256sum --check --ignore-missing SHA256SUMS
gh attestation verify portico-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo YounesAmalou/Portico
```

`gh attestation verify` checks that the file's digest was attested by a workflow of this repository, which is the release workflow; adding `--source-ref refs/tags/vX.Y.Z` also requires that it was built from that tag. The same command works for any artifact, for `SHA256SUMS`, and for an installed `/usr/bin/portico`. The SBOM attestation is shown with `--predicate-type https://cyclonedx.org/bom`.

## Not changing

Tag verification, CI reuse, the `crates` and `aur` jobs and their environments, release-plz's configuration, the AUR source package, and the rule that nothing is released or published except from a `v*` tag.

## Tests

- `tests/release_scripts.rs`: the tarball is named after the target and holds the binary, licenses and README.
- `tests/docs.rs`: the release workflow builds both musl targets, verifies on an aarch64 runner, grants `id-token: write` and `attestations: write` only to `build` and `checksums`, and attests in both; the README documents `SHA256SUMS` and `gh attestation verify`; the roadmap no longer plans release artifacts or signing; `Cargo.toml`'s package metadata and the Arch PKGBUILD name the OpenSSH client and install the binary.
- Workflows cannot run locally. `scripts/build-release.sh` and `scripts/verify-packages.sh` are rehearsed locally in Docker (aarch64 under QEMU), then the release workflow is rehearsed on GitHub with a manual run, which must produce every artifact, pass every verification and record every attestation.

## Risks

- **The build job holds an OIDC token while it compiles dependencies.** A malicious build script could request an attestation for a file of its choice, attributed to this workflow. Mitigations: dependencies are locked, `cargo audit` and `cargo deny` gate every change to them, and attestations prove where a file was built, not that its source is trustworthy. Attesting in a separate job would have required passing files between jobs before attesting them, which only moves the trust to the artifact store.
- **The packaging tools are unpinned** (whatever version Arch currently ships), like the toolchain and `cargo-audit` already are. A tool update that changes output names breaks `scripts/verify-packages.sh` before anything is released, which is the intended failure.
- **Musl differs from glibc** in DNS resolution and locale handling. Portico resolves nothing itself (OpenSSH does) and formats only times, through `localtime_r`, which musl implements from `/etc/localtime` as glibc does.
