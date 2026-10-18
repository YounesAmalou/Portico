# Portico Publish Foundation Design

**Status:** Approved; implemented (released as v0.1.0)

## Purpose

Publish Portico v0.1 as a public, installable, continuously verified open-source project and lay the base for production-grade releases. This is sub-project 1 of the production-readiness effort; each later sub-project gets its own design review.

## Decisions

- **License:** dual `MIT OR Apache-2.0`, copyright Younes A. Amalou (the legal name; the public handle Yousen may appear in `authors` and README credits). SPDX expression `MIT OR Apache-2.0`. Files `LICENSE-MIT` and `LICENSE-APACHE`; contributions are accepted under the same terms.
- **Names:** GitHub repository `Portico`; crates.io package `portico-ssh` (the name `portico` is taken by an unrelated crate); installed binary `portico` via an explicit `[[bin]]` entry; library crate name unchanged unless the package rename requires it.
- **Targets for this slice:** Linux x86_64, verified on Arch Linux (Omarchy's base). Other targets are roadmap items.
- **Version:** first public release is `0.1.0`, under Semantic Versioning, pre-1.0 so minor versions may break.

## Scope

### Included

1. **Repository hygiene:** license files, complete Cargo metadata (description, license, repository, homepage, readme, keywords, categories, `rust-version`, `exclude` for non-package files), `CHANGELOG.md` (Keep a Changelog), `CONTRIBUTING.md`, `SECURITY.md` (private reporting channel, scope: Portico edits `~/.ssh/config`), issue and PR templates.
2. **CI** on every push and pull request, on Arch Linux x86_64 in a container, as an unprivileged user (permission tests cannot pass as root):
   - `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets` including PTY acceptance tests (requires OpenSSH and Python 3 in the image).
   - Minimum supported Rust version check at the declared `rust-version`.
   - `cargo audit` and `cargo deny` (licenses limited to those compatible with MIT/Apache-2.0, advisories, duplicate sources).
   - A job that runs `cargo package` to prove the crate is publishable.
3. **Release workflow** triggered by a `vX.Y.Z` tag that matches the Cargo version: re-run the CI checks, build a stripped release binary, package `portico-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` with `LICENSE-*` and README, produce a SHA-256 checksum file, create a GitHub Release with notes extracted from `CHANGELOG.md`, then publish to crates.io using a repository secret. Publishing to crates.io is the last step so a failed build never publishes.
4. **Install documentation:** README sections for the release binary (with checksum verification), `cargo install portico-ssh`, and building from source; supported-platform statement; link to the roadmap.
5. **Roadmap document** (`ROADMAP.md`) retaining every follow-up slice already recorded in the v1 design spec (safe adoption, identity awareness, tunnels, desktop surface, tool integration, TUI refresh key) and adding: end-to-end tests with real OpenSSH and a real terminal in CI, aarch64 Linux, macOS, Windows, AUR package, Homebrew formula. Each item states its prerequisite; none is dropped by being scheduled later.

### Excluded (own designs later)

End-to-end test suite, cross-OS/architecture support and the process-control port that requires, AUR/Homebrew packaging, signed releases and SBOM/provenance attestation.

## Constraints and risks

- The release workflow's crates.io token and any publish step are configured by the maintainer; Claude does not create secrets or publish. The first tag is pushed by the maintainer after reviewing a manual release rehearsal.
- The package rename changes `Cargo.toml` and any references to the crate name in docs and tests; behaviour is unchanged and the existing test suite must still pass.
- CI must be deterministic: tests that depend on a real terminal or `xdg-terminal-exec` use the existing fakes; the PTY harness must pass in a container without a display.
- Branch protection and required checks are recommended in `CONTRIBUTING.md` but are repository settings applied by the maintainer.

## Verification

- All CI jobs pass on a pull request branch before merge.
- `cargo package --list` shows only intended files and `cargo publish --dry-run` succeeds.
- The release workflow is exercised by a manual run (`workflow_dispatch`), which verifies, runs CI and builds the tarball but never creates a release or publishes because those jobs require a pushed tag; artifact contents and checksum are inspected.
- A clean-machine smoke test installs from the tarball and from `cargo install --path .`, then runs `portico --version` and `portico init` in an isolated `HOME`.
