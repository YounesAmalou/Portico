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

- **Pushing a `vX.Y.Z` tag** verifies that the tag matches the Cargo version and that `CHANGELOG.md` has a section for it, re-runs CI, builds the Linux x86_64 tarball and checksum, creates the GitHub Release, and publishes `portico-ssh` to crates.io. The publish job runs in the `release` environment, which holds the crates.io token and requires the maintainer's approval before it starts, and only for tags. `v*` tags are protected: they can only be created by an administrator and can never be moved or deleted. Publishing cannot be undone.
- **A manual run** (Actions, Release, Run workflow, or `gh workflow run release.yml --ref <branch>`) is a rehearsal: it verifies, runs CI and builds the tarball, but never creates a release or publishes. Use it to test workflow changes from a branch.

CodeQL code scanning is managed in the repository's GitHub settings and appears in the Actions tab; it is not defined by files in this repository. Dependabot (`.github/dependabot.yml`) opens weekly pull requests for GitHub Actions and Cargo dependencies. Every action in the workflows is pinned to commit SHAs, with the version in a trailing comment, and the repository refuses to run an action that is not; update them through Dependabot, or look up the SHA of a release tag.

### Cutting a release

1. In a pull request, set `version` in `Cargo.toml` and turn `Unreleased` in `CHANGELOG.md` into a `## [X.Y.Z] - date` section.
2. After it merges, tag the merge commit on `main`: `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. Watch the run (`gh run watch`). If only `publish` fails, use "Re-run failed jobs": re-running the whole workflow fails at release creation because the release already exists.
4. Verify the GitHub Release and `cargo install portico-ssh` afterwards.

## Pull requests

- Keep changes focused and include tests for behaviour changes.
- Update `CHANGELOG.md` under `Unreleased`.
- Larger features follow the project's spec, plan, implementation flow in `docs/superpowers/`.

## Licensing

Portico is dual-licensed under MIT OR Apache-2.0. Unless you state otherwise, any contribution you submit is licensed under those same terms, without additional conditions.

## Repository settings (maintainers)

Protect `main` and require the CI jobs `fmt`, `clippy`, `test`, `msrv`, `audit`, `deny` and `package` before merging.
