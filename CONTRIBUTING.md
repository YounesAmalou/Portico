# Contributing

Thanks for helping improve Portico.

## Development checks

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
scripts/ci-checks.sh e2e
```

The tests use temporary SSH homes and never connect to remote machines. `scripts/ci-checks.sh e2e` runs the end-to-end suite (`tests/e2e.rs`, ignored by a plain `cargo test`): the real binary, through a real pseudo-terminal where a terminal is involved, against a throwaway `sshd` that each test starts on `127.0.0.1` as the current user, so it needs OpenSSH's server (`/usr/bin/sshd`) installed. The terminal tests need `python3` and OpenSSH installed, and the suite must run as a regular user: several tests prove that Portico refuses unreadable or unwritable SSH files, which root can always access. `scripts/ci-container.sh <check>` runs a CI check as an unprivileged user from inside a root container. Continuous integration runs these same checks, plus `cargo audit`, `cargo deny`, a minimum-Rust-version check and `cargo package`.

## Continuous integration and releases

Four workflows live in `.github/workflows/`:

| Workflow | File | Runs when |
|---|---|---|
| CI | `ci.yml` | on every pull request, on every push to `main`, once a day, and when called by the release workflow |
| Release PR | `release-plz.yml` | on every push to `main` |
| Pull request title | `pr-title.yml` | when a pull request is opened or its title changes |
| Release | `release.yml` | on pushing a `vX.Y.Z` tag, and on a manual run |

CI runs the eight jobs (`fmt`, `clippy`, `test`, `e2e`, `msrv`, `audit`, `deny`, `package`) in an Arch Linux container, as an unprivileged user. A pull request, a merge to `main` and a release all go through the same checks, so a green pull request is what a release will see. A first `changes` job decides which checks a change needs, and a job with nothing to check is skipped, which still counts as passing: a change that touches only Markdown files or `docs/` runs just the doc guards (`cargo test --test docs`), and `audit` and `deny` run only when `Cargo.toml`, `Cargo.lock`, `deny.toml` or the CI files change, plus in the daily run, because advisories are published independently of the code. Builds are cached per job: runs on `main` save the cache and pull requests restore it.

Release is not triggered by pull requests or pushes to branches. It has two modes:

- **Pushing a `vX.Y.Z` tag** verifies that the tag matches the Cargo version and that `CHANGELOG.md` has a section for it, re-runs CI unless that commit already passed every CI job on `main` (as the commit release-plz tags has: its merge commit carries the same tree), builds the Linux x86_64 tarball and checksum, creates the GitHub Release, and then publishes in two independent jobs, each starting automatically and each failing or being re-run without affecting the other: `crates` sends `portico-ssh` to crates.io (environment `crates`, which holds the crates.io token), and `aur` updates the AUR package `portico` (environment `aur`, which holds the AUR SSH key). Both run only for tags. `v*` tags are protected: they can only be created by an administrator and can never be moved or deleted. Publishing cannot be undone.
- **A manual run** (Actions, Release, Run workflow, or `gh workflow run release.yml --ref <branch>`) is a rehearsal: it verifies, runs CI and builds the tarball, but never creates a release or publishes. Use it to test workflow changes from a branch.

CodeQL code scanning is managed in the repository's GitHub settings and appears in the Actions tab; it is not defined by files in this repository. Dependabot (`.github/dependabot.yml`) opens weekly pull requests for GitHub Actions and Cargo dependencies. Every action in the workflows is pinned to commit SHAs, with the version in a trailing comment, and the repository refuses to run an action that is not; update them through Dependabot, or look up the SHA of a release tag.

### Commit messages and the changelog

Versions and `CHANGELOG.md` come from commit messages, which follow [Conventional Commits](https://www.conventionalcommits.org/): `feat:` (listed under Added), `fix:` (Fixed), `changed:`, `refactor:` and `perf:` (Changed) and `security:` (Security), plus `docs:`, `test:`, `ci:`, `build:`, `chore:`, `style:` and `revert:`, which never appear in the changelog. Add `!` after the type (`feat!:`) or a `BREAKING CHANGE:` footer for a breaking change. Only `feat`, `fix`, `security`, `perf`, `refactor` and `changed` commits cause a release. A squash merge uses the pull request title, and the `Pull request title` check rejects titles that do not follow the convention, so write the title as the changelog line you want users to read.

Nothing edits `CHANGELOG.md` by hand any more: release-plz writes each version's section when the release pull request is prepared, and the `Unreleased` heading stays as the marker where new sections are inserted. The release workflow extracts the section for that version with `scripts/changelog-notes.sh` and uses it as the GitHub Release notes; a missing section aborts the release before anything is built.

### Cutting a release

1. After a user-visible change merges to `main`, the `Release PR` workflow opens (or updates) a release pull request that sets the version in `Cargo.toml` and `Cargo.lock` and adds the version's changelog section. Before 1.0, a breaking change raises the minor version and anything else raises the patch version.
2. Review it like any pull request. Merging it is the release decision: the same workflow then tags the release pull request's commit `vX.Y.Z` (it is in `main`'s history, with the same tree as the merge commit).
3. The tag starts the release run, which verifies that the tag matches the Cargo version and that the changelog has a section for it, re-runs CI unless that commit already passed every CI job on `main` (as the commit release-plz tags has: its merge commit carries the same tree), builds the Linux x86_64 tarball and checksum, creates the GitHub Release, and publishes in two independent jobs, each starting automatically and each failing or being re-run without affecting the other: `crates` sends `portico-ssh` to crates.io and `aur` updates the AUR package `portico`. Both run only for tags. `v*` tags are protected: they can only be created by an administrator or the release automation and can never be moved or deleted. Publishing cannot be undone. Watch the run with `gh run watch`; if only `crates` or `aur` fails, use "Re-run failed jobs" (re-running the whole workflow fails at release creation because the release already exists), and never re-tag.
4. Verify the GitHub Release and `cargo install portico-ssh` afterwards.
5. The `aur` job runs `scripts/aur-publish.sh`, which pins the tag archive's SHA-256 with `scripts/aur-update.sh`, proves the package builds with `makepkg`, and pushes `PKGBUILD` and `.SRCINFO` to `ssh://aur@aur.archlinux.org/portico.git`. It is independent of `crates` and safe to re-run (a version already on the AUR changes nothing). The copy in `packaging/aur/` is the template: it is only edited when the packaging itself changes, and the version and checksum in it are rewritten at release time.

If the release automation is unavailable, `scripts/tag-release.sh X.Y.Z` is the manual fallback: in a pull request set the version in `Cargo.toml`, run `cargo update -p portico-ssh` so `Cargo.lock` carries the same version (release builds use `--locked`), and add a `## [X.Y.Z] - date` changelog section; merge it, run `git checkout main && git pull`, then the script. It refuses unless `HEAD` is the tip of `origin/main`, `Cargo.toml` and `CHANGELOG.md` carry the version and the tag is new. Never tag by hand: a tag on the wrong commit burns that version.

## Pull requests

- Keep changes focused and include tests for behaviour changes.
- Give the pull request a conventional title (see above); the changelog is generated from it.
- Name the branch after the same type, then a short kebab-case description of the change: `feat/jump-host-suggestions`, `fix/rich-error-followups`, `docs/release-plz-spec`, `ci/leaner-ci`. Avoid generic or tool-generated names (`patch-1`, `claude/...`, `worktree-...`); rename the branch before pushing. Only the release automation's `release-plz-*` branches use another scheme.
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
