# Contributing

Thanks for helping improve Portico.

## Development checks

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

The tests use temporary SSH homes and never connect to remote machines. The terminal tests need `python3` and OpenSSH installed. Continuous integration runs these same checks, plus `cargo audit`, `cargo deny`, a minimum-Rust-version check and `cargo package`.

## Pull requests

- Keep changes focused and include tests for behaviour changes.
- Update `CHANGELOG.md` under `Unreleased`.
- Larger features follow the project's spec, plan, implementation flow in `docs/superpowers/`.

## Licensing

Portico is dual-licensed under MIT OR Apache-2.0. Unless you state otherwise, any contribution you submit is licensed under those same terms, without additional conditions.

## Repository settings (maintainers)

Protect `main` and require the CI jobs `fmt`, `clippy`, `test`, `msrv`, `audit`, `deny` and `package` before merging.
