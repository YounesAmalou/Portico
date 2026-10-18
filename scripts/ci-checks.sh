#!/usr/bin/env bash
# Runs one continuous-integration check. CI jobs and local rehearsals call the
# same subcommands so a green local run means a green job.
set -euo pipefail

usage() {
    echo "usage: $0 <fmt|clippy|test|e2e|docs|msrv|audit|deny|package>" >&2
    exit 2
}

msrv() {
    local version
    version=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)
    [ -n "$version" ] || { echo "rust-version is not set in Cargo.toml" >&2; exit 1; }
    rustup toolchain install "$version" --profile minimal
    cargo "+$version" check --all-targets --locked
}

# CI installs cargo-audit and cargo-deny prebuilt from the Arch repositories; a local
# rehearsal without them builds the missing one on first use.
ensure_tool() {
    command -v "cargo-$1" >/dev/null || cargo install --locked "cargo-$1"
}

[ $# -eq 1 ] || usage
case "$1" in
    fmt) cargo fmt --check ;;
    clippy) cargo clippy --all-targets --all-features -- -D warnings ;;
    test) cargo test --all-targets ;;
    e2e) cargo test --test e2e -- --ignored ;;
    docs) cargo test --test docs ;;
    msrv) msrv ;;
    audit) ensure_tool audit && cargo audit ;;
    deny) ensure_tool deny && cargo deny check ;;
    package) cargo package --locked ;;
    *) usage ;;
esac
