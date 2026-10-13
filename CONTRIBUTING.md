# Contributing

Thanks for helping improve Portico.

## Development checks

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

The tests use temporary SSH homes and never connect to remote machines. The terminal tests need `python3` and OpenSSH installed, and the suite must run as a regular user: several tests prove that Portico refuses unreadable or unwritable SSH files, which root can always access. `scripts/ci-container.sh <check>` runs a CI check as an unprivileged user from inside a root container. Continuous integration runs these same checks, plus `cargo audit`, `cargo deny`, a minimum-Rust-version check and `cargo package`.

## Continuous integration and releases

Two workflows live in `.github/workflows/`:

| Workflow | File | Runs when |
|---|---|---|
| CI | `ci.yml` | on every pull request, on every push to `main`, and when called by the release workflow |
| Release | `release.yml` | on pushing a `vX.Y.Z` tag, and on a manual run |

CI runs the seven jobs (`fmt`, `clippy`, `test`, `msrv`, `audit`, `deny`, `package`) in an Arch Linux container, as an unprivileged user. A pull request, a merge to `main` and a release all go through the same checks, so a green pull request is what a release will see.

Release is not triggered by pull requests or pushes to branches. It has two modes:

- **Pushing a `vX.Y.Z` tag** verifies that the tag matches the Cargo version and that `CHANGELOG.md` has a section for it, re-runs CI, builds the Linux x86_64 tarball and checksum, creates the GitHub Release, and then publishes in two independent jobs, each starting automatically and each failing or being re-run without affecting the other: `crates` sends `portico-ssh` to crates.io (environment `crates`, which holds the crates.io token), and `aur` updates the AUR package `portico` (environment `aur`, which holds the AUR SSH key). Both run only for tags. `v*` tags are protected: they can only be created by an administrator and can never be moved or deleted. Publishing cannot be undone.
- **A manual run** (Actions, Release, Run workflow, or `gh workflow run release.yml --ref <branch>`) is a rehearsal: it verifies, runs CI and builds the tarball, but never creates a release or publishes. Use it to test workflow changes from a branch.

CodeQL code scanning is managed in the repository's GitHub settings and appears in the Actions tab; it is not defined by files in this repository. Dependabot (`.github/dependabot.yml`) opens weekly pull requests for GitHub Actions and Cargo dependencies. Every action in the workflows is pinned to commit SHAs, with the version in a trailing comment, and the repository refuses to run an action that is not; update them through Dependabot, or look up the SHA of a release tag.

### Changelog

`CHANGELOG.md` is written by hand, in [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format: user-visible changes go under `Unreleased` in the pull request that makes them, grouped as Added, Changed, Fixed or Security. Nothing generates or edits it automatically, and CI does not check that a pull request touched it (the pull request template only reminds you). At release time `Unreleased` becomes the version's section, and the release workflow extracts the section for that version with `scripts/changelog-notes.sh` and uses it as the GitHub Release notes. A missing section aborts the release before anything is built.

### Cutting a release

1. In a pull request, set `version` in `Cargo.toml`, run `cargo update -p portico-ssh` so `Cargo.lock` carries the same version (the release builds use `--locked` and fail otherwise), and turn `Unreleased` in `CHANGELOG.md` into a `## [X.Y.Z] - date` section above a fresh empty `Unreleased`.
2. After it merges, tag the merge commit on `main`: `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. Nothing else is needed: the run verifies, checks, builds, creates the release and publishes to crates.io and the AUR by itself. Watch it with `gh run watch`. If only `crates` or `aur` fails, use "Re-run failed jobs": re-running the whole workflow fails at release creation because the release already exists.
4. Verify the GitHub Release and `cargo install portico-ssh` afterwards.
5. The `aur` job runs `scripts/aur-publish.sh`, which pins the tag archive's SHA-256 with `scripts/aur-update.sh`, proves the package builds with `makepkg`, and pushes `PKGBUILD` and `.SRCINFO` to `ssh://aur@aur.archlinux.org/portico.git`. It is independent of `crates` and safe to re-run (a version already on the AUR changes nothing), so if it fails, for example because the key is missing, fix the cause and use "Re-run failed jobs". The copy in `packaging/aur/` is the template: it is only edited when the packaging itself changes, and the version and checksum in it are rewritten at release time.

## Pull requests

- Keep changes focused and include tests for behaviour changes.
- Update `CHANGELOG.md` under `Unreleased`.
- Larger features follow the project's spec, plan, implementation flow in `docs/superpowers/`.

## Licensing

Portico is dual-licensed under MIT OR Apache-2.0. Unless you state otherwise, any contribution you submit is licensed under those same terms, without additional conditions.

## Repository settings (maintainers)

These GitHub settings are not stored in the repository. They are listed here so they can be recreated or audited:

- **`main` branch protection:** pull request required; the seven CI jobs (`fmt`, `clippy`, `test`, `msrv`, `audit`, `deny`, `package`) must pass on an up-to-date branch; conversations must be resolved; force pushes and deletion are blocked; the rules apply to administrators too.
- **Tag rulesets:** `release tags are immutable` blocks updating and deleting `v*` tags; `only admins create release tags` restricts creating them to the admin role.
- **`crates` environment:** accepts deployments only from `v*` tags (which only administrators can create), and holds `CARGO_REGISTRY_TOKEN` as an environment secret (there is no repository-level copy). Prefer a crates.io token scoped to the `portico-ssh` crate with the `publish-update` scope.
- **`aur` environment:** the same `v*` tag restriction, holding `AUR_SSH_PRIVATE_KEY`, the private half of a dedicated key registered on the maintainer's AUR account. Keeping it apart from `crates` means the AUR job can never read the crates.io token. The AUR host key the job trusts is pinned in `packaging/aur/known_hosts`.
- **Actions:** read-only default permissions, actions must be pinned to commit SHAs, and first-time contributors' workflows need approval.
- **Security features:** secret scanning with push protection, Dependabot security updates, and private vulnerability reporting (the channel named in `SECURITY.md`).

Signed commits are not required.
