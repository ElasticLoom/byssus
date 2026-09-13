#!/usr/bin/env bash
# Interactive Byssus playground.
#
# Starts a shell inside a throwaway user and mount namespace (no root needed)
# with a sample deployment on a tmpfs, a simulated container, and byssusd
# running. Everything, including every mount, disappears when you exit.
#
# Usage:
#   scripts/playground.sh              # interactive shell
#   scripts/playground.sh -c 'CMDS'    # run commands non-interactively, then exit
set -euo pipefail

cd "$(dirname "$0")/.."
repo="$PWD"

command=""
if [[ "${1:-}" == "-c" ]]; then
    command="${2:?-c requires a command string}"
elif [[ $# -gt 0 ]]; then
    sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi

if ! unshare --user --map-root-user --mount true 2>/dev/null; then
    echo "error: cannot create an unprivileged user namespace." >&2
    echo "  On Ubuntu 24.04+: sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0" >&2
    exit 1
fi

cargo build --bins --quiet

setup="$(mktemp)"
trap 'rm -f "$setup"' EXIT
cat > "$setup" <<SETUP
set -e
umask 022
BIN="$repo/target/debug"
PG="\$(mktemp -d)"
mount -t tmpfs byssus-playground "\$PG"
cd "\$PG"

mkdir -p etc/conf.d state members groups container orgs set-members container-research
for m in libcurl openssl zlib; do
    mkdir -p "src/\$m/workspace"
    echo "Hello from \$m" > "src/\$m/workspace/README"
done
for p in acme/webapp acme/api beta/secret; do
    mkdir -p "orgs/\${p%/*}/projects/\${p#*/}/workspace"
    echo "Hello from \$p" > "orgs/\${p%/*}/projects/\${p#*/}/workspace/README"
done

# Shared propagation anchor holding the view.
mount --bind groups groups
mount --make-shared groups
mkdir groups/view
mount --bind orgs orgs
mount --make-shared orgs
mkdir -p orgs/acme/groups/research/view

cat > etc/byssus.toml <<CONFIG
[daemon]
state_dir = "\$PG/state"
resync_interval_secs = 10

[groups.demo]
source_root = "\$PG/src"
source      = "{name}/workspace"
target_root = "\$PG/groups/view"
target      = "{name}"
membership  = "\$PG/members"

# Groups created at runtime: set-members/<org>/<group>/ is a group.
[group_sets.projects]
membership_root = "\$PG/set-members"
source_root     = "\$PG/orgs"
source          = "{group}/projects/{name}/workspace"
target_root     = "\$PG/orgs"
target          = "{group}/groups/{subgroup}/view/{name}"
CONFIG

# A simulated container: the view bound with rslave propagation.
mount --bind groups/view container
mount --make-rslave container
mount --bind orgs/acme/groups/research/view container-research
mount --make-rslave container-research

"\$BIN/byssusd" --config etc/byssus.toml --config-dir etc/conf.d --allow-root 2>daemon.log &
BYSSUSD_PID=\$!
trap 'kill \$BYSSUSD_PID 2>/dev/null; wait \$BYSSUSD_PID 2>/dev/null; cd /; umount -l "\$PG"; rmdir "\$PG"' EXIT
for _ in \$(seq 50); do
    grep -q 'trigger=startup' daemon.log 2>/dev/null && break
    sleep 0.1
done
set +e

byssus()  { "\$BIN/byssus" --config "\$PG/etc/byssus.toml" --config-dir "\$PG/etc/conf.d" "\$@"; }
join()    { touch "\$PG/members/\$1" && sleep 0.3 && ls "\$PG/container"; }
leave()   { rm -f "\$PG/members/\$1" && sleep 0.3 && ls "\$PG/container"; }
view()    { echo "\$PG/orgs/\${1%/*}/groups/\${1#*/}/view"; }
mkgroup() { mkdir -p "\$(view "\$1")" "\$PG/set-members/\$1"; }
rmgroup() { rm -r "\$PG/set-members/\$1" && sleep 0.3 && ls "\$(view "\$1")"; }
gjoin()   { touch "\$PG/set-members/\$1/\$2" && sleep 0.3 && ls "\$(view "\$1")"; }
gleave()  { rm -f "\$PG/set-members/\$1/\$2" && sleep 0.3 && ls "\$(view "\$1")"; }
logs()    { tail -n 50 -f "\$PG/daemon.log"; }
reload()  { kill -HUP \$BYSSUSD_PID; }
restart() { kill \$BYSSUSD_PID; wait \$BYSSUSD_PID; "\$BIN/byssusd" --config "\$PG/etc/byssus.toml" --config-dir "\$PG/etc/conf.d" --allow-root 2>>"\$PG/daemon.log" & BYSSUSD_PID=\$!; }
help() {
    cat <<HELP
Byssus playground in \$PG (a tmpfs; nothing touches the host)

  src/<name>/workspace   sources: libcurl, openssl, zlib
  members/               membership directory (touch/rm files here)
  groups/view/           target root (shared propagation)
  container/             simulated container view (rslave)
  orgs/<org>/projects/<name>/workspace
                         group set sources: acme/webapp, acme/api, beta/secret
  set-members/<org>/<group>/
                         group set membership: mkdir a group, touch members
  orgs/<org>/groups/<group>/view/
                         each group's view
  container-research/    simulated container for acme/research's view
  etc/byssus.toml        configuration (edit, then: reload)
  daemon.log             byssusd log

Helpers:
  join NAME / leave NAME   add or remove a member, then list the container view
  mkgroup ORG/GROUP        create a group in the set (view + directory)
  rmgroup ORG/GROUP        remove a group (its mounts go too)
  gjoin ORG/GROUP NAME     add a member to a set group, then list its view
  gleave ORG/GROUP NAME    remove a member from a set group
  byssus status|dry-run    run the CLI against this deployment
  logs                     follow the daemon log (Ctrl-C to stop)
  reload                   send SIGHUP to byssusd
  restart                  restart byssusd (mounts are preserved)
  help                     show this again
  exit                     tear everything down

Things to try:
  join libcurl; cat container/libcurl/README
  echo x > container/libcurl/new           # read-only file system
  ln -s libcurl members/link; byssus status  # rejected entry
  touch members/.tmp; byssus status          # ignored hidden entry
  mkdir src/libcurl/workspace/sub; mount -t tmpfs t src/libcurl/workspace/sub
  touch src/libcurl/workspace/sub/secret; ls container/libcurl/sub   # not exposed

Group sets (groups created at runtime, no reload):
  mkgroup acme/research; gjoin acme/research webapp; cat container-research/webapp/README
  gjoin acme/research secret; byssus status   # beta's project: source_unavailable
  mkgroup acme/monitoring; gjoin acme/monitoring api; ls container-research   # separate view
  rmgroup acme/research; ls container-research                                 # gone
HELP
}
PS1='(byssus-playground) \w\\$ '
SETUP

ns=(unshare --user --map-root-user --mount --propagation private --)
if [[ -n "$command" ]]; then
    "${ns[@]}" bash -c "source '$setup'; $command"
else
    "${ns[@]}" bash --noprofile --rcfile <(cat "$setup"; echo help) -i
fi
