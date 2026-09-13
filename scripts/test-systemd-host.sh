#!/usr/bin/env bash
# End-to-end test of the Debian package and hardened systemd unit on a real
# systemd host, as root. Intended for disposable CI runners and VMs: it
# installs the package, creates /srv/byssus-e2e, mounts, and starts byssusd.
#
# Usage: sudo BYSSUS_E2E_DISPOSABLE_HOST=1 scripts/test-systemd-host.sh [DEB]
set -euo pipefail

if [[ "${BYSSUS_E2E_DISPOSABLE_HOST:-}" != 1 ]]; then
    echo "refusing to run: this modifies the host (set BYSSUS_E2E_DISPOSABLE_HOST=1 on a disposable machine)" >&2
    exit 1
fi
[[ $EUID -eq 0 ]] || { echo "must run as root" >&2; exit 1; }
[[ -d /run/systemd/system ]] || { echo "systemd is not running" >&2; exit 1; }

cd "$(dirname "$0")/.."
deb="${1:-$(find dist -maxdepth 1 -name 'byssus_*_amd64.deb' | head -n 1)}"
[[ -f "$deb" ]] || { echo "no .deb found; run scripts/package.sh" >&2; exit 1; }

root=/srv/byssus-e2e
step() { echo "==> $*"; }
fail() {
    echo "FAIL: $*" >&2
    journalctl -u byssusd --no-pager -n 100 >&2 || true
    exit 1
}
wait_for() {
    local what="$1"; shift
    for _ in $(seq 100); do
        if "$@"; then return 0; fi
        sleep 0.1
    done
    fail "timed out waiting for: $what"
}
cleanup() {
    systemctl stop byssusd >/dev/null 2>&1 || true
    for m in "$root/consumer" "$root/groups"; do
        umount -R -l "$m" >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT

step "install package"
dpkg -i "$deb"
systemd-analyze verify /usr/lib/systemd/system/byssusd.service
[[ "$(systemctl is-enabled byssusd || true)" == disabled ]] || fail "unit enabled by install"
systemctl is-active --quiet byssusd && fail "daemon started by install"
uid="$(id -u byssus)"

step "lay out a deployment"
mkdir -p "$root"/projects/{alpha,beta}/workspace "$root"/groups/demo/view "$root"/membership/demo "$root"/consumer
echo "I am alpha" > "$root/projects/alpha/workspace/README"
echo "I am beta" > "$root/projects/beta/workspace/README"
chown byssus:byssus "$root/groups/demo/view"
chmod 0755 "$root" "$root"/projects "$root"/membership "$root"/membership/demo
mount --bind "$root/groups" "$root/groups"
mount --make-shared "$root/groups"
# A simulated container: the view bound read-only with rslave propagation.
mount --bind "$root/groups/demo/view" "$root/consumer"
mount -o remount,bind,ro "$root/consumer"
mount --make-rslave "$root/consumer"
install -m 0644 /dev/stdin /etc/byssus/conf.d/demo.toml <<CONFIG
[groups.demo]
source_root = "$root/projects"
source      = "{name}/workspace"
target_root = "$root/groups/demo/view"
target      = "{name}"
membership  = "$root/membership/demo"
CONFIG

step "validate before starting"
byssus check || fail "byssus check"
byssus dry-run || fail "byssus dry-run"

step "start the service"
# Membership exists before start: with Type=notify, systemctl start returns
# only after the startup reconcile, so the mount must already be visible.
touch "$root/membership/demo/alpha"
systemctl enable --now byssusd
systemctl is-active --quiet byssusd || fail "service not active after start"
pid="$(systemctl show -p MainPID --value byssusd)"
[[ "$pid" != 0 ]] || fail "no main process"
[[ "$(readlink "/proc/$pid/ns/mnt")" == "$(readlink /proc/1/ns/mnt)" ]] || fail "daemon is not in the host mount namespace"
test -f "$root/consumer/alpha/README" || fail "member not mounted when start returned (readiness sent too early?)"
status_text="$(systemctl show -p StatusText --value byssusd)"
[[ "$status_text" == "1 group(s), 1 mount(s)" ]] || fail "unexpected StatusText: $status_text"
grep -Eq "^Uid:\s+$uid\s+$uid\s+$uid\s+$uid$" "/proc/$pid/status" || fail "not running as byssus"
grep -Eq '^CapEff:\s+0000000000200000$' "/proc/$pid/status" || fail "effective capabilities are not exactly CAP_SYS_ADMIN"
grep -Eq '^NoNewPrivs:\s+1$' "/proc/$pid/status" || fail "no_new_privs not set"
journalctl -u byssusd --no-pager | grep -q 'msg="privileges normalized"' || fail "no privileges log"

step "the member is served to the consumer"
[[ "$(cat "$root/consumer/alpha/README")" == "I am alpha" ]] || fail "wrong content"
if (echo x > "$root/consumer/alpha/new") 2>/dev/null; then fail "consumer could write"; fi
[[ "$(stat -c %a /var/lib/byssus)" == 750 ]] || fail "state directory mode"
[[ "$(stat -c %a /var/lib/byssus/state.json)" == 640 ]] || fail "state file mode"
byssus status | grep -q 'demo/alpha  state=mounted' || fail "status"

step "a second member and a reload"
touch "$root/membership/demo/beta"
wait_for "beta visible" test -f "$root/consumer/beta/README"
systemctl reload byssusd
wait_for "reload logged" sh -c "journalctl -u byssusd --no-pager | grep -q 'configuration reloaded'"
wait_for "status shows two mounts" sh -c "systemctl show -p StatusText --value byssusd | grep -q '1 group(s), 2 mount(s)'"

step "a rejected reload is visible in the status"
echo '[groups.broken' > /etc/byssus/conf.d/zz-broken.toml
systemctl reload byssusd
wait_for "failed reload in status" sh -c "systemctl show -p StatusText --value byssusd | grep -q 'last reload failed'"
systemctl is-active --quiet byssusd || fail "daemon stopped after a rejected reload"
rm /etc/byssus/conf.d/zz-broken.toml
systemctl reload byssusd
wait_for "status recovered" sh -c "! systemctl show -p StatusText --value byssusd | grep -q 'last reload failed'"

step "leaving removes the mount"
rm "$root/membership/demo/beta"
wait_for "beta gone" sh -c "! test -e '$root/consumer/beta/README'"

step "restart preserves mounts"
systemctl restart byssusd
wait_for "service active" systemctl is-active --quiet byssusd
test -f "$root/consumer/alpha/README" || fail "mount lost on restart"

step "stop preserves mounts"
systemctl stop byssusd
test -f "$root/consumer/alpha/README" || fail "mount lost on stop"

step "a namespace-creating unit option is refused"
systemctl stop byssusd
install -d /etc/systemd/system/byssusd.service.d
printf '[Service]\nPrivateNetwork=yes\n' > /etc/systemd/system/byssusd.service.d/private-network.conf
systemctl daemon-reload
if systemctl start byssusd 2>/dev/null; then fail "daemon started in a private mount namespace"; fi
journalctl -u byssusd --no-pager -n 20 | grep -q 'are slave mounts' || fail "no slave-namespace refusal logged"
rm -r /etc/systemd/system/byssusd.service.d
systemctl daemon-reload
systemctl reset-failed byssusd

step "remove the package"
systemctl start byssusd
rm "$root/membership/demo/alpha"
wait_for "alpha gone" sh -c "! test -e '$root/consumer/alpha/README'"
dpkg -r byssus
systemctl is-active --quiet byssusd && fail "daemon still running after remove"
echo "systemd host end-to-end: ok"
