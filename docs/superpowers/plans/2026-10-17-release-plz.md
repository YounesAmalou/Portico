# Release automation with release-plz: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Implements `docs/superpowers/specs/2026-10-16-release-plz-design.md`.

1. Add `release-plz.toml`: no publishing, no GitHub Release, tag `v{{ version }}`, tag only after the release pull request merges, release commits limited to user-visible types, changelog header identical to the one in `CHANGELOG.md` and the `## [X.Y.Z] - date` heading `scripts/changelog-notes.sh` matches.
2. Add `.github/workflows/release-plz.yml` (release pull request job and tag job, App token, every action pinned to a commit SHA) and `.github/workflows/pr-title.yml` (conventional title check, title passed through an environment variable).
3. Empty the hand-maintained `Unreleased` section of `CHANGELOG.md`; update `CONTRIBUTING.md` and the pull request template for the new flow; keep `scripts/tag-release.sh` as the documented fallback until one automated release has succeeded, then delete it.
4. Tests in `tests/docs.rs`: the changelog header equals the configured header; the release-plz settings that keep publishing in `release.yml` are present.
5. Not tested locally: the workflows. They are rehearsed on GitHub with the next version, following the spec's Verification section.
