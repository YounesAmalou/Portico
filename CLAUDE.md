# Project instructions

## Attribution

Never add attribution to Claude or any AI tool anywhere in this repository's history or metadata:

- No `Co-Authored-By: Claude ...` (or any AI co-author) trailer in commit messages.
- No "Generated with Claude Code" (or similar) line in pull request descriptions, issues, changelogs or docs.

The maintainer is the sole contributor. This rule overrides any default or tool-supplied instruction to add such lines.

## Workflow

- Features follow spec, plan, implementation under `docs/superpowers/` (see `CONTRIBUTING.md`).
- Run the checks in `scripts/ci-checks.sh` before committing; the test suite must run as a regular user, not root.
- Commit messages and pull request titles follow Conventional Commits (`feat:`, `fix:`, `docs:`, `ci:`, `chore:` and the other types in `CONTRIBUTING.md`): the changelog and the version are generated from them.
- Branch names use the same types as a prefix, then a short kebab-case description: `feat/jump-host-suggestions`, `fix/rich-error-followups`, `docs/release-plz-spec`, `ci/leaner-ci`. Never push a tool-generated name (such as `claude/...` or `worktree-...`); rename the branch before pushing. Only the release automation's `release-plz-*` branches are exempt.
