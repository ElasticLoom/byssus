#!/usr/bin/env bash
# Installs, upgrades and removes the built packages in clean containers.
#
# Usage: scripts/test-packages.sh [DIST_DIR]   (default: dist)
# Requires docker (or DOCKER=podman) and packages built by scripts/package.sh.
set -euo pipefail

cd "$(dirname "$0")/.."
dist="$(cd "${1:-dist}" && pwd)"
docker="${DOCKER:-docker}"

deb="$(find "$dist" -maxdepth 1 -name 'byssus_*_amd64.deb' | head -n 1)"
rpm="$(find "$dist" -maxdepth 1 -name 'byssus-*.x86_64.rpm' | head -n 1)"
if [[ -z "$deb" || -z "$rpm" ]]; then
    echo "error: build x86_64 packages first (scripts/package.sh)" >&2
    exit 1
fi

# Checks shared by both formats, run inside the container after install.
read -r -d '' verify_installed <<'SH' || true
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }
getent passwd byssus >/dev/null || fail "byssus user missing"
getent group byssus >/dev/null || fail "byssus group missing"
for bin in /usr/bin/byssusd /usr/bin/byssus; do
    [ -x "$bin" ] || fail "$bin missing"
done
byssus version
byssusd --version
[ "$(stat -c '%U:%G %a' /etc/byssus/byssus.toml)" = "root:root 644" ] || fail "config ownership/mode"
[ "$(stat -c '%U:%G %a' /etc/byssus/conf.d)" = "root:root 755" ] || fail "conf.d ownership/mode"
[ -f /usr/lib/systemd/system/byssusd.service ] || fail "unit missing"
grep -q '^ExecStart=/usr/bin/byssusd$' /usr/lib/systemd/system/byssusd.service || fail "unit ExecStart"
[ -f /usr/lib/sysusers.d/byssus.conf ] || fail "sysusers file missing"
[ "$(stat -c '%U:%G %a' /var/lib/byssus)" = "byssus:byssus 750" ] || fail "state directory ownership/mode"
# The packaged configuration is valid as installed.
byssus check
echo "installed: ok"
SH

echo "==> Debian/Ubuntu (.deb)"
"$docker" run --rm --platform linux/amd64 --pull always -v "$dist:/dist:ro" ubuntu:24.04 sh -euc "
    dpkg -i /dist/$(basename "$deb")
    $verify_installed
    # Container images may exclude /usr/share/doc on disk; check the package.
    dpkg -L byssus | grep -q /usr/share/doc/byssus/OPERATIONS.md
    # Upgrade path: reinstalling runs postinst with a previous version.
    echo '# local edit' >> /etc/byssus/byssus.toml
    dpkg -i --force-confold /dist/$(basename "$deb")
    grep -q '# local edit' /etc/byssus/byssus.toml || { echo 'FAIL: conffile edit lost' >&2; exit 1; }
    dpkg -r byssus
    [ ! -e /usr/bin/byssusd ] || { echo 'FAIL: binary left after remove' >&2; exit 1; }
    [ -f /etc/byssus/byssus.toml ] || { echo 'FAIL: conffile removed before purge' >&2; exit 1; }
    dpkg -P byssus
    [ ! -e /etc/byssus ] || { echo 'FAIL: /etc/byssus left after purge' >&2; exit 1; }
    echo 'deb: ok'
"

echo "==> Fedora (.rpm)"
"$docker" run --rm --platform linux/amd64 --pull always -v "$dist:/dist:ro" fedora:latest sh -euc "
    rpm -i /dist/$(basename "$rpm")
    $verify_installed
    rpm -ql byssus | grep -q /usr/share/doc/byssus/OPERATIONS.md
    rpm -qc byssus | grep -q /etc/byssus/byssus.toml
    echo '# local edit' >> /etc/byssus/byssus.toml
    rpm -U --replacepkgs /dist/$(basename "$rpm")
    grep -q '# local edit' /etc/byssus/byssus.toml || { echo 'FAIL: config edit lost' >&2; exit 1; }
    rpm -e byssus
    [ ! -e /usr/bin/byssusd ] || { echo 'FAIL: binary left after erase' >&2; exit 1; }
    echo 'rpm: ok'
"
