#!/usr/bin/env bash
# Runs the same checks as CI's fast jobs, so problems are caught before pushing.
#
# Usage: scripts/check.sh [--full]
#   --full also runs the privileged integration tests and, if docker and the
#   packaging tools are available, builds and tests the packages.
set -euo pipefail

cd "$(dirname "$0")/.."

step() { echo "==> $*"; }

step "format"
cargo fmt --all --check

step "clippy"
RUSTFLAGS="-D warnings" cargo clippy --locked --all-targets --all-features

step "unit tests"
cargo test --locked --all-features

step "docs"
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --document-private-items

msrv="$(grep -m1 '^rust-version' Cargo.toml | cut -d'"' -f2)"
if rustup toolchain list | grep -q "^$msrv"; then
    step "MSRV ($msrv)"
    RUSTFLAGS="" cargo "+$msrv" check --locked --all-targets --all-features
else
    echo "skipping MSRV check: install with 'rustup toolchain install $msrv --profile minimal'"
fi

if command -v cargo-deny >/dev/null 2>&1; then
    step "cargo-deny"
    cargo deny check
else
    echo "skipping cargo-deny: install with 'cargo install --locked cargo-deny'"
fi

step "shell scripts"
if command -v shellcheck >/dev/null 2>&1; then
    shellcheck scripts/*.sh
    shellcheck -s sh packaging/deb/* packaging/rpm/*.sh
else
    echo "skipping shellcheck: not installed"
fi

if [[ "${1:-}" == "--full" ]]; then
    step "integration tests"
    scripts/integration-tests.sh
    if command -v docker >/dev/null 2>&1 && command -v cargo-deb >/dev/null 2>&1 \
        && command -v cargo-generate-rpm >/dev/null 2>&1; then
        step "packages"
        scripts/package.sh
        scripts/test-packages.sh
    else
        echo "skipping packages: needs docker, cargo-deb and cargo-generate-rpm"
    fi
fi

echo "all checks passed"
