# Publish Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Portico publishable as the `portico-ssh` crate with a dual license, verified by CI on every change and released by a tag-triggered workflow as a Linux x86_64 tarball, GitHub Release and crates.io package.

**Architecture:** Package metadata and license files make the crate publishable without code changes (the library keeps the name `portico`, the binary stays `portico`). Two small shell scripts hold the release logic that can be tested locally (changelog extraction, tarball packaging); the GitHub workflows only orchestrate them. CI and release run in an Arch Linux container, and the same commands are rehearsed locally in a Docker `archlinux` container before anything is pushed.

**Tech Stack:** Cargo, GitHub Actions, Docker (local rehearsal), bash, `cargo-audit`, `cargo-deny`.

**Spec:** `docs/superpowers/specs/2026-10-13-publish-foundation-design.md` (also read `docs/superpowers/specs/2026-09-13-portico-host-management-design.md`, section "Follow-up product slices").

## Deviations from this plan

Recorded after execution; the original text below is kept as written.

- The crates.io environment was later renamed `crates` (beside `aur`) and the required approval removed: release tags can only be created by an administrator, and a pushed tag is the approval.

- **Minimum Rust version:** `rust-version = "1.88"`, not 1.85. The dependency tree requires 1.88, found by rehearsing the MSRV check.
- **Dependencies:** `ratatui` 0.30 and `crossterm` 0.29 replace 0.29 and 0.28. `cargo deny` rejected the unmaintained `paste` and unsound `lru` that 0.29 pulled in. `Terminal::clear` in 0.30 queries the cursor, so the TUI redraws with `redraw_from_scratch` instead.
- **CI:** jobs run through `scripts/ci-container.sh` as a non-root user, because four permission tests cannot pass as root. `rust-cache` was dropped. Hygiene tests that read unpublished files are excluded from the package.
- **Release workflow:** a manual run has no inputs; it always only verifies, checks and builds, and `release` and `publish` require a pushed tag. Release builds are stripped by Cargo, and `publish` runs in the `release` environment.
- **Test fixes found by CI:** the PTY harness treats zombies as dead (containers whose PID 1 never reaps), and the process-tree test waits for descendants to die instead of asserting the instant cancellation returns.
- **Afterwards:** actions were pinned to commit SHAs with Dependabot added, and the repository settings listed in `CONTRIBUTING.md` were applied.

## Global Constraints

- License: `MIT OR Apache-2.0`; files `LICENSE-MIT` and `LICENSE-APACHE`; copyright holder `Younes A. Amalou` (handle `Yousen` allowed in `authors` and README credits only).
- crates.io package `portico-ssh`; installed binary `portico`; library crate name stays `portico` (`use portico::...` in `src/main.rs` and `tests/` must not change).
- Target: Linux x86_64 (`x86_64-unknown-linux-gnu`), verified on Arch Linux. No other targets in this slice.
- First public version `0.1.0`, Semantic Versioning, `CHANGELOG.md` in Keep a Changelog format.
- Release tarball name: `portico-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` with a `.sha256` file; crates.io publish is the last release step.
- Release tag `vX.Y.Z` must equal the Cargo version, or the workflow aborts before building.
- CI: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets` (needs OpenSSH and Python 3), MSRV check at the declared `rust-version`, `cargo audit`, `cargo deny`, `cargo package`.
- No secrets are created or used by Claude; the crates.io token (`CARGO_REGISTRY_TOKEN`) and the first tag push belong to the maintainer. Never run `cargo publish` without `--dry-run`, never push tags, never push to a remote without the maintainer asking.
- Existing behaviour and the existing 180 tests must keep passing unchanged.

## Review Focus

- Tag `v0.1.1` with Cargo version `0.1.0`: release aborts before any build or publish (Task 4, script test).
- `CHANGELOG.md` has no section for the version being released: release aborts with a message naming the missing section (Task 4, script test).
- `cargo install portico-ssh` must produce a binary named `portico`, not `portico-ssh` (Task 1, test).
- The published crate contains `LICENSE-MIT`, `LICENSE-APACHE` and README but not `.superpowers/`, `.worktrees/`, `target/` or `docs/superpowers/` (Task 1, test over `cargo package --list`).
- A tampered tarball must fail `sha256sum -c` (Task 4, script test).
- The declared `rust-version` must actually compile the crate, otherwise the MSRV claim is false (Task 3, local rehearsal).
- PTY tests must pass in a container with no display and no controlling terminal (Task 3, local rehearsal in Docker).

---

### Task 1: Package identity, license and metadata

**Files:**
- Modify: `Cargo.toml`
- Create: `LICENSE-MIT`, `LICENSE-APACHE`
- Test: `tests/package.rs`

**Interfaces:**
- Produces: package `portico-ssh` version `0.1.0`, `[lib] name = "portico"`, `[[bin]] name = "portico"`, `rust-version` (verified in Task 3), license files at the repo root.

- [x] **Step 1: Write the failing test** `tests/package.rs` with tests:
  - `package_is_published_as_portico_ssh_under_a_dual_license`: `assert_eq!(env!("CARGO_PKG_NAME"), "portico-ssh")` and `assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0")`.
  - `installed_binary_is_named_portico`: runs `env!("CARGO_BIN_EXE_portico") --version` and asserts stdout is `portico 0.1.0\n` built from `env!("CARGO_PKG_VERSION")`.
  - `license_files_name_the_copyright_holder`: reads `LICENSE-MIT` and asserts it contains `Younes A. Amalou` and `Permission is hereby granted`; reads `LICENSE-APACHE` and asserts it contains `Apache License` and `Version 2.0`.
  - `published_crate_contains_only_intended_files`: runs `cargo package --list --allow-dirty` and asserts the list contains `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`, `src/main.rs` and contains no line starting with `.superpowers`, `.worktrees`, `target/` or `docs/superpowers`.
- [x] **Step 2: Run** `cargo test --test package` — Expected: FAIL (package name and license missing).
- [x] **Step 3: Edit `Cargo.toml`:** `name = "portico-ssh"`; add `description`, `license = "MIT OR Apache-2.0"`, `repository = "https://github.com/YounesAmalou/Portico"`, `homepage` (same URL), `readme = "README.md"`, `keywords` (at most five: `ssh`, `tui`, `openssh`, `cli`, `config`), `categories = ["command-line-utilities"]`, `authors = ["Younes A. Amalou (Yousen)"]`, `rust-version = "1.85"` (edition 2024 floor; Task 3 confirms), `exclude = [".superpowers/", ".worktrees/", "docs/superpowers/", ".github/"]`; add `[lib] name = "portico"` and `[[bin]] name = "portico" path = "src/main.rs"`.
- [x] **Step 4: Create the license files:** `LICENSE-MIT` is the standard MIT text with `Copyright (c) 2026 Younes A. Amalou`; `LICENSE-APACHE` is the unmodified Apache-2.0 text (copy from a canonical source such as `curl -fsSL https://www.apache.org/licenses/LICENSE-2.0.txt`).
- [x] **Step 5: Run** `cargo test --all-targets` — Expected: PASS, including the four new tests and all 180 existing ones.
- [x] **Step 6: Commit** `git add Cargo.toml Cargo.lock LICENSE-MIT LICENSE-APACHE tests/package.rs && git commit -m "feat: publish as portico-ssh under MIT OR Apache-2.0"`

### Task 2: Project documents and roadmap

**Files:**
- Create: `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`, `ROADMAP.md`, `.github/ISSUE_TEMPLATE/bug_report.md`, `.github/ISSUE_TEMPLATE/feature_request.md`, `.github/pull_request_template.md`
- Test: `tests/docs.rs`

**Interfaces:**
- Produces: `CHANGELOG.md` with an `## [0.1.0] - 2026-10-13` section (consumed by Task 4's extraction script).

- [x] **Step 1: Write the failing test** `tests/docs.rs`:
  - `required_project_documents_exist`: asserts each of the files above exists and is non-empty.
  - `roadmap_keeps_every_follow_up_slice_from_the_v1_spec`: reads the follow-up slices section of `docs/superpowers/specs/2026-09-13-portico-host-management-design.md`, and asserts `ROADMAP.md` contains each slice's bold title (`Safe adoption`, `Identity awareness`, `Tunnels`, `Desktop surface`, `Tool integration`, `TUI refresh`) plus `aarch64`, `macOS`, `Windows`, `AUR`, `Homebrew` and `end-to-end`.
  - `changelog_has_the_current_version_section`: asserts `CHANGELOG.md` contains `## [` followed by `env!("CARGO_PKG_VERSION")` and `]`.
  - `security_policy_names_a_private_reporting_channel`: asserts `SECURITY.md` contains `private` and `~/.ssh/config`.
- [x] **Step 2: Run** `cargo test --test docs` — Expected: FAIL (files missing).
- [x] **Step 3: Write the documents.** `CHANGELOG.md`: Keep a Changelog header, an `Unreleased` section and the `0.1.0` section summarising v1 (host management CLI and TUI, init, safety guarantees, Ctrl+Enter, validation). `CONTRIBUTING.md`: build/test commands (the "Development checks" list in README), the dual-license contribution clause, commit and PR expectations, recommended branch protection with the CI job names from Task 3. `SECURITY.md`: supported versions (latest 0.x), private reporting via GitHub private vulnerability reporting, scope note that Portico edits `~/.ssh/config` and `~/.ssh/portico/`. `ROADMAP.md`: the six slices copied from the v1 spec with their requirements verbatim, plus the new items each with a one-line prerequisite (end-to-end tests before cross-OS work; process-control port before macOS/Windows; first tagged release before AUR/Homebrew; aarch64 via native ARM runners). Issue and PR templates: short, with environment fields (OS, terminal, OpenSSH version, `portico --version`).
- [x] **Step 4: Run** `cargo test --test docs` — Expected: PASS.
- [x] **Step 5: Commit** `git add CHANGELOG.md CONTRIBUTING.md SECURITY.md ROADMAP.md .github tests/docs.rs && git commit -m "docs: add changelog, contributing, security policy and roadmap"`

### Task 3: Continuous integration

**Files:**
- Create: `.github/workflows/ci.yml`, `deny.toml`, `scripts/ci-checks.sh`
- Modify: `Cargo.toml` (`rust-version` only if the MSRV check finds a higher floor), `CONTRIBUTING.md` (job names)
- Test: `scripts/ci-checks.sh` rehearsed in Docker (no Rust test; the script is the unit)

**Interfaces:**
- Produces: `scripts/ci-checks.sh <fmt|clippy|test|msrv|audit|deny|package>` runs one check and exits non-zero on failure; each CI job calls exactly one subcommand, so local rehearsal equals CI.

- [x] **Step 1: Write `scripts/ci-checks.sh`** (bash, `set -euo pipefail`): `fmt` → `cargo fmt --check`; `clippy` → `cargo clippy --all-targets --all-features -- -D warnings`; `test` → `cargo test --all-targets`; `msrv` → read `rust-version` from `Cargo.toml`, `rustup toolchain install <v> --profile minimal`, `cargo +<v> check --all-targets`; `audit` → `cargo audit`; `deny` → `cargo deny check`; `package` → `cargo package --locked`. Unknown subcommand prints usage and exits 2.
- [x] **Step 2: Write `deny.toml`:** allowed licenses `MIT`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Unicode-3.0`, `Zlib`, `MPL-2.0` (add only what `cargo deny check licenses` actually reports for the current tree), advisories deny vulnerabilities, sources only crates.io.
- [x] **Step 3: Rehearse locally in Docker** (the failing-then-passing cycle for CI): `docker run --rm -v "$PWD":/src -w /src archlinux:base-devel bash -c 'pacman -Syu --noconfirm rustup openssh python git && rustup default stable && cargo install --locked cargo-audit cargo-deny && for c in fmt clippy test msrv audit deny package; do scripts/ci-checks.sh $c; done'`. Expected: each check passes; fix real findings (an MSRV failure means raise `rust-version` to the lowest version that passes and update the Task 1 test expectations and spec text; a licence failure means extend `deny.toml` only for licences compatible with MIT/Apache-2.0).
- [x] **Step 4: Write `.github/workflows/ci.yml`:** triggers `push` to `main` and `pull_request`; `permissions: contents: read`; `concurrency` cancels superseded runs; one job per check named `fmt`, `clippy`, `test`, `msrv`, `audit`, `deny`, `package`, each `runs-on: ubuntu-latest` with `container: archlinux:base-devel`, a first step installing `rustup openssh python git` with pacman and `rustup default stable`, `actions/checkout` pinned to a major version, `Swatinem/rust-cache` for cargo caching, then `scripts/ci-checks.sh <check>`; `audit` and `deny` install their tool with `cargo install --locked`.
- [x] **Step 5: Validate the workflow syntax:** `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml'))"` and, if `actionlint` is available, `actionlint`. Expected: no errors.
- [x] **Step 6: Commit** `git add .github/workflows/ci.yml deny.toml scripts/ci-checks.sh Cargo.toml CONTRIBUTING.md && git commit -m "ci: verify format, lints, tests, MSRV, advisories and packaging"`

### Task 4: Release workflow and install documentation

**Files:**
- Create: `scripts/changelog-notes.sh`, `scripts/package-release.sh`, `.github/workflows/release.yml`
- Modify: `README.md` (install section, supported platforms, link to roadmap), `.github/workflows/ci.yml` (add the `workflow_call` trigger)
- Test: `tests/release_scripts.rs`

**Interfaces:**
- Produces: `scripts/changelog-notes.sh <version> [changelog]` prints the notes of `## [<version>]` up to the next `## [` heading, exits 1 with `missing changelog section for <version>` otherwise. `scripts/package-release.sh <version> <binary> <outdir>` writes `portico-v<version>-x86_64-unknown-linux-gnu.tar.gz` (containing a `portico-v<version>-x86_64-unknown-linux-gnu/` directory with `portico`, `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`) and `<name>.tar.gz.sha256` in `<outdir>`, printing the tarball path.

- [x] **Step 1: Write the failing tests** in `tests/release_scripts.rs` (invoking the scripts with `std::process::Command` against temp dirs):
  - `changelog_notes_extracts_only_the_requested_section`: a changelog with sections 0.2.0 and 0.1.0 yields exactly the 0.1.0 body.
  - `changelog_notes_fails_for_a_missing_version`: exit status non-zero, stderr contains `missing changelog section for 0.9.0`.
  - `package_release_builds_a_tarball_with_license_files_and_checksum`: unpack the tarball, assert the four files exist with `portico` executable, and `sha256sum -c` succeeds in `<outdir>`.
  - `package_release_checksum_detects_tampering`: append a byte to the tarball, `sha256sum -c` fails.
- [x] **Step 2: Run** `cargo test --test release_scripts` — Expected: FAIL (scripts missing).
- [x] **Step 3: Implement the two scripts** (bash, `set -euo pipefail`, readable errors) to the interfaces above.
- [x] **Step 4: Run** `cargo test --test release_scripts` — Expected: PASS.
- [x] **Step 5: Write `.github/workflows/release.yml`:** triggers `push` of tags `v*.*.*` and `workflow_dispatch` with a boolean `dry_run` (default `true`) and a `version` input; first job `verify` reads the Cargo version, fails when the tag differs (`refs/tags/vX.Y.Z` vs `X.Y.Z`) and runs `scripts/changelog-notes.sh` to fail on a missing section; then the same seven checks as CI (reusable via `workflow_call` in `ci.yml`, which gains that trigger); job `build` (needs `verify` and all checks) runs `cargo build --release --locked`, strips, runs `scripts/package-release.sh`, uploads the artefacts; job `release` (needs `build`, skipped when `dry_run`) creates the GitHub Release with `gh release create` using the notes and the artefacts; job `publish` (needs `release`, skipped when `dry_run`) runs `cargo publish --locked` with `CARGO_REGISTRY_TOKEN`; `permissions: contents: write` only on `release`.
- [x] **Step 6: Document installation in `README.md`:** a section with release binary download plus `sha256sum -c`, `cargo install portico-ssh`, build from source, a "Supported platforms" line (Linux x86_64 verified on Arch; others on the roadmap) and a link to `ROADMAP.md`; update the "Run from the repository" and "Development checks" sections to match `scripts/ci-checks.sh`.
- [x] **Step 7: Rehearse the release locally:** `cargo build --release --locked && scripts/package-release.sh 0.1.0 target/release/portico /tmp/claude-1000/-home-yousen-Dev-Portico/2e2f0b75-83d5-4ce5-ae5a-4fa16180b618/scratchpad/release && cd <outdir> && sha256sum -c *.sha256`. Expected: `OK`. Then validate `release.yml` syntax as in Task 3.
- [x] **Step 8: Commit** `git add scripts .github/workflows README.md tests/release_scripts.rs && git commit -m "ci: release Linux x86_64 binaries and publish to crates.io on tags"`

### Task 5: Final verification and hand-off

**Files:**
- Modify: none expected (fixes only if a check fails)

- [x] **Step 1: Run the full local gate:** `scripts/ci-checks.sh fmt && scripts/ci-checks.sh clippy && scripts/ci-checks.sh test && scripts/ci-checks.sh package` and `cargo publish --dry-run --locked` — Expected: all succeed, dry-run reports `warning: aborting upload due to dry run`.
- [x] **Step 2: Clean-machine smoke test in Docker:** unpack the Task 4 tarball inside `archlinux:base-devel` with `openssh` installed, run `HOME=$(mktemp -d) ./portico --version` then `./portico init -y` and `./portico add dev -H dev.example` and `./portico hosts`. Expected: version `portico 0.1.0`, init succeeds, host listed. Repeat with `cargo install --path . --root <tmp>` and assert the installed file is `bin/portico`.
- [x] **Step 3: Confirm the license metadata:** `cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; p=json.load(sys.stdin)["packages"][0]; print(p["name"], p["license"])'` — Expected: `portico-ssh MIT OR Apache-2.0`.
- [x] **Step 4: Report to the maintainer** with the checklist of actions that remain theirs: push the branch and open the PR, enable branch protection with the seven CI job names, enable GitHub private vulnerability reporting, add the `CARGO_REGISTRY_TOKEN` secret, run the release workflow with `dry_run` first, then push the `v0.1.0` tag.
