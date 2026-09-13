#!/usr/bin/env bash
# End-to-end test of the Debian package and hardened systemd unit on a real
# systemd host, as root. Intended for disposable CI runners and VMs: it
# installs the package, creates /srv/byssus-e2e and a byssus-e2e-app user,
# mounts consumers, and starts byssusd. It relies on systemd having made
# every mount shared at boot, as on a real host (not systemd in a container).
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
    # Consumers, and any member mounts left by a failed run (deepest first).
    findmnt -rn -o TARGET | grep "^$root/" | sort -r | while read -r m; do
        umount -l "$m" >/dev/null 2>&1 || true
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
# No propagation setup: systemd makes every mount shared at boot, so plain
# directories on it propagate new mounts to rslave consumers.
[[ "$(findmnt -n -o PROPAGATION -T "$root/groups/demo/view")" == shared ]] ||
    fail "the mount containing $root is not shared; is this a systemd host?"
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

step "a group set: groups created at runtime by an unprivileged application"
command -v setfacl >/dev/null || fail "setfacl not found (install the acl package)"
app=byssus-e2e-app
id -u "$app" >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin "$app"
as_app() { runuser -u "$app" -- "$@"; }
m="$root/set-membership"
mkdir -p "$root/orgs" "$root/set-consumer"
chmod 0755 "$root/orgs"
install -d -o "$app" -g "$app" -m 0755 "$root/orgs/acme" "$root/orgs/beta"
install -d -o "$app" -g "$app" -m 0750 "$m"
setfacl    -m u:byssus:rx "$m"
setfacl -d -m u:byssus:rx "$m"
for project in acme/webapp acme/api beta/secret; do
    as_app mkdir -p "$root/orgs/${project%/*}/projects/${project#*/}/workspace"
    echo "I am $project" | as_app tee "$root/orgs/${project%/*}/projects/${project#*/}/workspace/README" >/dev/null
done
install -m 0644 /dev/stdin /etc/byssus/conf.d/orgs.toml <<CONFIG
[group_sets.projects]
membership_root = "$m"
source_root     = "$root/orgs"
source          = "{group}/projects/{name}/workspace"
target_root     = "$root/orgs"
target          = "{group}/groups/{subgroup}/view/{name}"
CONFIG
byssus check || fail "byssus check with a group set"
reloads="$(journalctl -u byssusd --no-pager | grep -c 'configuration reloaded' || true)"
systemctl reload byssusd
wait_for "group set reload logged" sh -c "[ \"\$(journalctl -u byssusd --no-pager | grep -c 'configuration reloaded')\" -gt $reloads ]"

# Everything from here to removal runs as the application, without root.
view="$root/orgs/acme/groups/research/view"
as_app mkdir -p "$view"
as_app setfacl -m u:byssus:rwx "$view"
mount --bind "$view" "$root/set-consumer"
mount -o remount,bind,ro "$root/set-consumer"
mount --make-rslave "$root/set-consumer"
as_app mkdir -p "$m/acme/research"
as_app touch "$m/acme/research/webapp"
wait_for "webapp visible in acme/research" test -f "$root/set-consumer/webapp/README"
[[ "$(cat "$root/set-consumer/webapp/README")" == "I am acme/webapp" ]] || fail "wrong set member content"
if (echo x > "$root/set-consumer/webapp/new") 2>/dev/null; then fail "set consumer could write"; fi

# A member name that exists only in another org is never mounted.
as_app touch "$m/acme/research/secret"
wait_for "secret skipped" sh -c "journalctl -u byssusd --no-pager | grep -q 'op=skip group=projects/acme/research name=secret'"
test -e "$root/set-consumer/secret/README" && fail "another org's project was exposed"
byssus status | grep -q 'projects/acme/research/secret  state=source_unavailable' || fail "status for cross-org member"
as_app rm "$m/acme/research/secret"

# A group prepared under a hidden name and renamed into place, with its own view.
as_app mkdir -p "$root/orgs/acme/groups/monitoring/view"
as_app setfacl -m u:byssus:rwx "$root/orgs/acme/groups/monitoring/view"
as_app mkdir "$m/acme/.monitoring.tmp"
as_app touch "$m/acme/.monitoring.tmp/api"
as_app mv "$m/acme/.monitoring.tmp" "$m/acme/monitoring"
wait_for "api visible in acme/monitoring" test -f "$root/orgs/acme/groups/monitoring/view/api/README"
test -e "$root/set-consumer/api" && fail "groups share a view"
byssus status | grep -q 'projects/acme/research/webapp  state=mounted' || fail "status for set member"
wait_for "status counts set groups" sh -c "systemctl show -p StatusText --value byssusd | grep -q '^3 group(s), 3 mount(s)$'"

# Removing group directories removes the groups and their mounts.
as_app rm -r "$m/acme/research" "$m/acme/monitoring"
wait_for "webapp gone" sh -c "! test -e '$root/set-consumer/webapp/README'"
wait_for "api gone" sh -c "! test -e '$root/orgs/acme/groups/monitoring/view/api/README'"
wait_for "status back to the static group" sh -c "systemctl show -p StatusText --value byssusd | grep -q '^1 group(s), 1 mount(s)$'"
umount "$root/set-consumer"
as_app rmdir "$view" "$root/orgs/acme/groups/monitoring/view"

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
