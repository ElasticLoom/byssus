#!/usr/bin/env bash
# Runs the privileged integration tests inside a throwaway user and mount
# namespace. Inside it the tests hold CAP_SYS_ADMIN over their own mount
# namespace, so real mount syscalls are exercised without host root, and every
# mount disappears when the namespace exits.
#
# Usage: scripts/integration-tests.sh [test-binary arguments...]
#   e.g. scripts/integration-tests.sh --nocapture mount::
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v unshare >/dev/null 2>&1; then
    echo "error: 'unshare' (util-linux) is required" >&2
    exit 1
fi

if ! unshare --user --map-root-user --mount true 2>/dev/null; then
    cat >&2 <<'MSG'
error: cannot create an unprivileged user namespace.
  On Ubuntu 24.04+ this is restricted by AppArmor; for a local run you can use:
    sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0
  or check: sysctl user.max_user_namespaces
MSG
    exit 1
fi

# Build test and daemon binaries as the invoking user. Locate them from
# cargo's output, which honors CARGO_TARGET_DIR and other build settings.
executables() {
    grep -o '"executable":"[^"]*"' | cut -d'"' -f4
}
binary="$(
    cargo test --test privileged --no-run --message-format=json 2>/dev/null |
        executables | grep '/privileged-[^/]*$' | tail -n 1
)"
if [[ -z "$binary" || ! -x "$binary" ]]; then
    echo "error: could not locate the privileged test binary" >&2
    exit 1
fi
mapfile -t bins < <(cargo build --bins --message-format=json 2>/dev/null | executables)
if [[ ${#bins[@]} -ne 2 ]]; then
    echo "error: expected the byssus and byssusd binaries, found: ${bins[*]:-none}" >&2
    exit 1
fi

# Stage the binaries the tests execute in a directory every user in the
# namespace can reach, as an installed package would be. Tests run them as
# unprivileged users too, and the build directory may not be traversable by
# other users (for example beneath a CI runner's home directory).
bin_dir="$(mktemp -d)"
trap 'rm -rf "$bin_dir"' EXIT
chmod 0755 "$bin_dir"
install -m 0755 "${bins[@]}" "$bin_dir/"
for name in byssus byssusd; do
    [[ -x "$bin_dir/$name" ]] || { echo "error: $name was not built" >&2; exit 1; }
done

export BYSSUS_TEST_NAMESPACE=1
export BYSSUS_TEST_BIN_DIR="$bin_dir"

# Map a range of subordinate IDs as well as root when possible, so tests can
# switch to an unprivileged service user inside the namespace. This needs
# newuidmap/newgidmap and entries in /etc/subuid and /etc/subgid.
map_args=(--map-root-user)
skip_args=()
if unshare --user --map-root-user --map-auto --mount true 2>/dev/null; then
    map_args=(--map-root-user --map-auto)
    export BYSSUS_TEST_SUBIDS=1
elif [[ "${BYSSUS_REQUIRE_SUBIDS:-0}" == 1 ]]; then
    echo "error: subordinate UIDs are required (BYSSUS_REQUIRE_SUBIDS=1) but unavailable" >&2
    exit 1
else
    echo "warning: subordinate UIDs unavailable; skipping service_user:: tests" >&2
    echo "         (configure /etc/subuid and /etc/subgid for $(id -un) to run them)" >&2
    skip_args=(--skip service_user::)
fi

# Not exec: the trap removes the staged binaries afterwards.
unshare --user "${map_args[@]}" --mount --propagation private -- \
    "$binary" --ignored --test-threads=1 "${skip_args[@]}" "$@"
