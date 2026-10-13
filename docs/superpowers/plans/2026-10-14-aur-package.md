# AUR Package Implementation Plan

Spec: `docs/superpowers/specs/2026-10-14-aur-package-design.md`.

1. Write `packaging/aur/PKGBUILD` for `portico` 0.1.0 and `scripts/aur-update.sh`; run the script to pin the real v0.1.0 checksum and generate `.SRCINFO`.
2. Build with `makepkg --nodeps` in scratch `BUILDDIR`/`PKGDEST`/`SRCDEST` directories; inspect the package contents and run the packaged binary.
3. Exclude `packaging/` from the crates.io package and extend `tests/package.rs`.
4. Document: README, CONTRIBUTING release step 5, CHANGELOG, ROADMAP.
5. Add the PKGBUILD and documentation tests to `tests/docs.rs`.
6. Add `scripts/aur-publish.sh`, the pinned AUR host key, the `aur` environment and the `aur` release job; rehearse the script in an Arch container against a local bare repository (first publish, then an idempotent re-run).
7. Run `scripts/ci-checks.sh` checks as a regular user and open a pull request.

## Deviations from this plan

- The first build produced an empty `portico-debug` package and a `gdb-add-index` error, because the binary is already stripped by the release profile; `options=('!debug')` fixes it.
