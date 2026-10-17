# Release automation with release-plz

**Status:** Draft, awaiting review
**Date:** 2026-10-16

## Purpose

A release today needs three hand steps: edit `Cargo.toml` and `Cargo.lock`, turn `Unreleased` into a dated changelog section, and tag the merge commit. The tag step burned `v0.2.0`, `v0.2.1` and `v0.2.2`, because tags are immutable and the wrong commit was tagged. This slice removes all three: a bot keeps a release pull request open, and merging it is the whole release.

It implements the "Release automation" item of `ROADMAP.md`. Linux artifacts, signing and Trusted Publishing are separate items and change nothing here.

## Decisions

- **Tool:** release-plz, run from a new `release-plz.yml` workflow on every push to `main`, with two jobs: `release-plz release-pr` and `release-plz release`.
- **The release pull request** carries the version bump in `Cargo.toml` and `Cargo.lock` and the new changelog section, and is merged like any other pull request: it needs the seven required CI checks and the protection rules on `main` stay as they are. The release decision remains a human merge.
- **What `release-plz release` does:** only when the release pull request has merged (`release_always = false`), it creates the tag `v{{ version }}` on the merge commit. Nothing else.
- **What it deliberately does not do:** it does not publish to crates.io (`publish = false`) and does not create the GitHub Release (`git_release_enable = false`). The existing `release.yml`, started by the pushed tag, keeps verifying, running CI, building the tarball and checksum, creating the release with notes from `CHANGELOG.md`, and publishing through the `crates` and `aur` jobs with their own environments and secrets. The crates.io token therefore never enters the new workflow.
- **Token:** the workflow uses a short-lived token minted for a dedicated GitHub App, not the default workflow token, because pull requests and tags created with the default token start no workflows: only an App token lets the release pull request run CI and lets the tag start `release.yml`. The App's credentials are stored in repository settings, never in the repository.
- **Tag protection:** the App is allowed to create `v*` tags; tags stay immutable, so no one can move or delete one.
- **Versions:** commit messages already follow `feat:`, `fix:`, `docs:`, `ci:`, `chore:`, `test:`. Before 1.0 release-plz follows Cargo's rules: a breaking change (`!` or a `BREAKING CHANGE` footer) bumps the minor version, and `feat` and `fix` bump the patch. Pull-request titles become the squash commit subjects, so they must follow the convention; a pull request check validating the title is part of this slice.
- **Changelog:** generated from commit messages by release-plz (git-cliff) into `CHANGELOG.md`, in Keep a Changelog groups (Added from `feat`, Fixed from `fix`, Changed, Security) with `ci`, `docs`, `test` and `chore` excluded. Existing history is kept unchanged. The hand-maintained `Unreleased` section stops being the source: its entries are rewritten as commit messages or moved into the first generated section. The heading stays `## [X.Y.Z]` followed by a link and date, which `scripts/changelog-notes.sh` already matches by prefix, so release notes keep working.
- **`scripts/tag-release.sh`** is a local maintainer helper in this repository, not part of any workflow. It is deleted from the repository (and from `CONTRIBUTING.md`) after the first automated release succeeds, since no manual tagging remains. `CONTRIBUTING.md` documents the new flow, including that a failed `crates` or `aur` job is re-run, never re-tagged.

## Not changing

Branch protection, the required checks, tag immutability, the `crates` and `aur` environments, the tarball build, and the rule that nothing publishes except from a `v*` tag.

## Verification

Rehearsed on GitHub before a real release relies on it, and not before the next version is due:

1. The release pull request opens against `main` and its seven required checks run (proving the App token triggers CI).
2. Its diff changes only the version, the lockfile entry and the changelog, and `scripts/changelog-notes.sh` extracts the new section from it.
3. After merging it, the `v*` tag appears on the merge commit and `release.yml` starts from it (proving the App may create the tag and that the tag starts the release).
4. `release.yml` completes: GitHub Release, tarball, checksum, and `crates` publish; `aur` stays red while AUR publishing is suspended.

If step 3 fails because the App may not create the tag, fix the App's access in the repository settings, never the tag.

## Risks

- A mis-typed pull-request title yields a wrong bump or a missing changelog line; the title check and review of the release pull request are the guard.
- The App's private key is the one new long-lived secret. It can be rotated from the App's settings without any repository change.
