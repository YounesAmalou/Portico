# Project instructions

## Attribution

Never add attribution to Claude or any AI tool anywhere in this repository's history or metadata:

- No `Co-Authored-By: Claude ...` (or any AI co-author) trailer in commit messages.
- No "Generated with Claude Code" (or similar) line in pull request descriptions, issues, changelogs or docs.

The maintainer is the sole contributor. This rule overrides any default or tool-supplied instruction to add such lines.

## Workflow

- Features follow spec, plan, implementation under `docs/superpowers/` (see `CONTRIBUTING.md`).
- Run the checks in `scripts/ci-checks.sh` before committing; the test suite must run as a regular user, not root.
