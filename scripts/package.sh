#!/usr/bin/env bash
# Builds .deb and .rpm packages from static musl binaries.
#
# Usage: scripts/package.sh [TARGET...]
#   TARGET defaults to x86_64-unknown-linux-musl. Packages are written to dist/.
# Requires: cargo-deb and cargo-generate-rpm (cargo install --locked cargo-deb cargo-generate-rpm)
set -euo pipefail

cd "$(dirname "$0")/.."

targets=("$@")
if [[ ${#targets[@]} -eq 0 ]]; then
    targets=(x86_64-unknown-linux-musl)
fi

for tool in cargo-deb cargo-generate-rpm; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "error: $tool is required: cargo install --locked $tool" >&2
        exit 1
    fi
done

# Packages install the binaries in /usr/bin; the unit in contrib/ uses
# /usr/local/bin for manual installs.
mkdir -p target/package dist
sed 's|/usr/local/bin/byssusd|/usr/bin/byssusd|' contrib/systemd/byssusd.service \
    > target/package/byssusd.service
if grep -q /usr/local/bin target/package/byssusd.service; then
    echo "error: packaged unit still refers to /usr/local/bin" >&2
    exit 1
fi

# cargo-deb may warn "Failed to find dependency specification": the binaries
# are statically linked, so there are no shared-library dependencies and the
# package correctly has no Depends field.
for target in "${targets[@]}"; do
    cargo build --locked --release --target "$target"
    cargo deb --no-build --target "$target" --output dist/
    cargo generate-rpm --target "$target" --output dist/
done

ls -l dist/*.deb dist/*.rpm
