# Operating Byssus

This guide covers installing, configuring, running and troubleshooting
Byssus. The paths below (`/srv/example/...`) are placeholders; substitute your
own layout. For the design and security contract, see [DESIGN.md](DESIGN.md);
for attaching containers and integrating an application, see
[INTEGRATION.md](INTEGRATION.md); for every option, see
[REFERENCE.md](REFERENCE.md).

## Contents

- [Requirements](#requirements)
- [Install](#install)
  - [From a package](#from-a-package)
  - [From source](#from-source)
- [Lay out directories and permissions](#lay-out-directories-and-permissions)
- [Check mount propagation](#check-mount-propagation)
- [Configure groups](#configure-groups)
- [Check the deployment](#check-the-deployment)
- [Run under systemd](#run-under-systemd)
- [Run without systemd](#run-without-systemd)
- [Day-to-day operation](#day-to-day-operation)
- [Troubleshooting](#troubleshooting)
- [Removing Byssus](#removing-byssus)

## Requirements

- Linux 5.12 or newer (`uname -r`). `byssus dry-run` verifies every kernel
  feature Byssus uses.
- `CAP_SYS_ADMIN` for `byssusd`, granted by systemd or `setcap`.
- POSIX ACL support on the filesystems holding source directories (or an
  equivalent group-permission scheme).
- Containers must run in the host's mount namespace hierarchy — Docker Desktop
  on macOS or Windows is not supported.

## Install

### From a package

Releases on GitHub provide `.deb` and `.rpm` packages for `x86_64`/`amd64`
and `aarch64`/`arm64`, plus `SHA256SUMS`. Verify and install:

```bash
sha256sum -c --ignore-missing SHA256SUMS
gh attestation verify byssus_<version>-1_amd64.deb --repo ElasticLoom/byssus   # optional

sudo apt install ./byssus_<version>-1_amd64.deb     # Debian, Ubuntu
sudo dnf install ./byssus-<version>-1.x86_64.rpm    # Fedora, RHEL
```

The package:

- installs `/usr/bin/byssusd` and `/usr/bin/byssus` (static binaries, no
  dependencies);
- installs the hardened unit as `/usr/lib/systemd/system/byssusd.service`;
- creates the `byssus` system user and group, `/var/lib/byssus` (mode
  `0750`) and `/etc/byssus/conf.d`;
- installs `/etc/byssus/byssus.toml` with `user = "byssus"` and no groups,
  kept across upgrades if
  you edit it;
- installs documentation and examples under `/usr/share/doc/byssus/`;
- does **not** enable or start the daemon.

Upgrading a package restarts a running daemon; mounts are preserved.
Removing it stops and disables the daemon but leaves its mounts in place (see
[Removing Byssus](#removing-byssus)). Purging a `.deb` also removes
`/var/lib/byssus` if the state file records no mounts.

With a package installed, skip to
[Lay out directories and permissions](#lay-out-directories-and-permissions).

### From source

```bash
rustup target add x86_64-unknown-linux-musl     # or aarch64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl

install -o root -g root -m 0755 target/x86_64-unknown-linux-musl/release/byssusd /usr/local/bin/
install -o root -g root -m 0755 target/x86_64-unknown-linux-musl/release/byssus  /usr/local/bin/
```

Both binaries are statically linked. Do not add file capabilities when using
systemd. To build packages instead, run `scripts/package.sh`.

Then create the service user, with systemd-sysusers:

```bash
install -m 0644 contrib/sysusers.d/byssus.conf /usr/lib/sysusers.d/byssus.conf
systemd-sysusers
```

Or manually:

```bash
useradd --system --no-create-home --shell /usr/sbin/nologin byssus
```

Operators and monitoring tools that should read Byssus state (for
`byssus status`) can be added to the `byssus` group. **Never** give the
`byssus` group write access to membership directories.

## Lay out directories and permissions

`byssusd` runs as `byssus` with no ability to bypass file permissions, so grant
exactly what it needs.

```bash
# State directory (systemd's StateDirectory= creates this automatically).
install -d -o byssus -g byssus -m 0750 /var/lib/byssus

# Configuration (root-owned, not group- or world-writable).
install -d -o root -g root -m 0755 /etc/byssus /etc/byssus/conf.d

# Target root: where views are mounted. Owned by byssus.
install -d -o root   -g root   -m 0755 /srv/example/groups
install -d -o byssus -g byssus -m 0755 /srv/example/groups/research
install -d -o byssus -g byssus -m 0755 /srv/example/groups/research/view

# Membership directory: written by your application, readable by byssus.
install -d -o app -g app -m 0750 /srv/example/membership/research
setfacl -m u:byssus:rx /srv/example/membership/research
```

### Group set membership

For a [group set](REFERENCE.md#configuration), the application creates group
directories at runtime beneath `membership_root`. Give `byssus` read and
search access to the root and, through a default ACL, to every directory
created beneath it:

```bash
install -d -o app -g app -m 0750 /srv/example/membership/projects
setfacl    -m u:byssus:rx /srv/example/membership/projects
setfacl -d -m u:byssus:rx /srv/example/membership/projects
```

The target root of a set usually spans many views (for example
`/srv/example/orgs`, with views at `<org>/groups/<group>/view`). `byssus` needs
write access to each view to create its member directories, or to a parent
if views should be created on demand. The application can grant it when it
creates a view (`setfacl -m u:byssus:rwx <view>`), or a default ACL on a parent
can.

### Source directories

`byssus` needs **search** (`x`) permission on every directory from `/` down to
each member's source directory, and nothing more. It never needs to read file
contents; consumers read through the mount with their own credentials.

```bash
# Existing directories on the path to the sources.
setfacl -m u:byssus:x /srv/example /srv/example/projects

# Existing members.
find /srv/example/projects -mindepth 1 -maxdepth 2 -type d -exec setfacl -m u:byssus:x {} +

# Future members: default ACLs are inherited by newly created directories.
setfacl -d -m u:byssus:x /srv/example/projects
```

Default ACLs are inherited only when the creating process does not strip them
(for example with an explicit `chmod` that lowers the ACL mask). If your
application creates source directories, run `byssus dry-run` after it does, or
grant the ACL explicitly as part of creating a project.

Parents created for nested targets (for example `target = "{name}/ro"`) are
made with mode `0755`.

## Check mount propagation

Consumers see new mounts through **shared** mount propagation: the mount that
contains each target root must be shared. The target root itself can be an
ordinary directory.

On hosts booted with systemd this is already the case — systemd makes every
mount shared at boot — and there is nothing to set up. (systemd skips this when
it runs inside a container.) Check it:

```bash
findmnt -o TARGET,PROPAGATION -T /srv/example/groups/research/view   # expect: shared
```

`byssus check` and `byssus dry-run` report the same (`propagation = shared`),
and `byssusd` checks it at startup and on reload.

Only if it reports `private` (for example on a host without systemd, inside a
container, or where a mount was made private deliberately), create a shared mount at or above the
target roots, and make it persistent. With the example systemd mount unit:

```bash
install -m 0644 contrib/systemd/srv-example-groups.mount /etc/systemd/system/
# The file name must match the mount point: systemd-escape --path /srv/example/groups
systemctl daemon-reload
systemctl enable --now srv-example-groups.mount
```

Or with `/etc/fstab`:

```
/srv/example/groups  /srv/example/groups  none  rbind,rshared  0 0
```

`rbind` keeps any mounts already beneath the directory visible. Create it before
starting containers that bind views beneath it: a container attached earlier
holds a view from beneath the old mount and does not see new members.

If it reports a `slave` propagation, `byssusd` is running in its own mount
namespace; see [Troubleshooting](#troubleshooting).

## Configure groups

Copy and edit the examples:

```bash
install -o root -g root -m 0644 examples/byssus.toml /etc/byssus/byssus.toml
install -o root -g root -m 0644 examples/conf.d/research.toml /etc/byssus/conf.d/research.toml
```

For groups that your application creates at runtime, start from
`examples/conf.d/projects.toml` (a group set) instead; see
[INTEGRATION.md](INTEGRATION.md#groups-created-at-runtime-group-sets).

Configuration files and their directories must be owned by root and not group-
or world-writable; `byssusd` refuses to start otherwise. See
[REFERENCE.md](REFERENCE.md#configuration) for every option.

## Check the deployment

Run a full validation without mounting anything:

```bash
sudo byssus dry-run
```

As root with `daemon.user` configured, `dry-run` switches to that user and
drops every capability first, so permission problems show up exactly as the
daemon would experience them. It reports kernel features, `/proc`,
configuration paths, state, propagation, and per group: members, rejected
membership entries (by name, with the reason), ignored hidden entries, sources that resolve or not, existing mounts, what a
reconcile would create or remove, and conflicts. It exits 1 if anything is an
error.

`--format json` produces the same report as JSON.

## Run under systemd

Packages install the unit already. For a source install:

```bash
install -m 0644 contrib/systemd/byssusd.service /etc/systemd/system/byssusd.service
systemctl daemon-reload
```

Then:

```bash
systemctl enable --now byssusd
journalctl -u byssusd -f
```

The shipped unit:

- runs as `byssus` with only `CAP_SYS_ADMIN` (ambient) and `NoNewPrivileges=yes`;
- creates `/var/lib/byssus` (`StateDirectory=`, mode `0750`);
- applies a system call filter, address-family, namespace, realtime,
  personality and W^X restrictions, and denies all IP traffic;
- uses `Type=notify`: `byssusd` reports ready only after its startup
  reconcile, and the unit starts before Docker, containerd and Podman, so
  containers see every member from the start;
- reports group and mount counts, degraded groups, groups whose directories
  it cannot read, and rejected reloads in `systemctl status byssusd` (or
  `systemctl show -p StatusText byssusd`), for example
  `3 group(s), 5 mount(s); unreadable: projects/acme/research (see byssus status)`.

**Do not add options that create a mount namespace** — `ProtectSystem=`,
`PrivateTmp=`, `ReadWritePaths=` and many others (the full list is at the top
of the unit file). They trap every mount inside the service. `byssusd` detects
the resulting slave propagation at startup and refuses to run, rather
than silently doing nothing useful. Do not add `RestrictSUIDSGID=` either:
systemd implements it by blocking `openat2`, which Byssus requires. If you
need to customize the unit, use a drop-in (`systemctl edit byssusd`) and keep
to non-namespace options.

## Run without systemd

Either grant the capability to the binary and run it as the service user:

```bash
setcap cap_sys_admin+ep /usr/local/bin/byssusd
sudo -u byssus /usr/local/bin/byssusd
```

(Do not set `no_new_privs` before exec here: it prevents file capabilities from
applying. `byssusd` sets it itself after starting.)

Or start it as root with a service user configured (`daemon.user` or
`--user byssus`); it switches user and drops every other capability itself:

```bash
/usr/local/bin/byssusd --user byssus
```

`byssusd` refuses to run as root without a service user unless `--allow-root`
is given, which is intended only for development and tests.

## Day-to-day operation

| Task | Command |
|------|---------|
| Show members and their mount state | `byssus status` (add `--format json` for tooling) |
| Validate everything without changes | `sudo byssus dry-run` |
| Check a configuration change before applying it | `sudo byssus check --add new.toml` |
| Reload configuration | `systemctl reload byssusd` |
| Follow logs | `journalctl -u byssusd -f` |
| One-off reconcile (daemon stopped) | `sudo byssus reconcile --user byssus` |

`byssus status` exits 1 if any member is in an error state (conflicts,
unavailable targets or groups), so it can be used for monitoring. Rejected
membership entries and missing sources are listed as warnings and do not
change the exit status. If the state file cannot be read (for example, the caller is
not in the `byssus` group) it says so and shows what it can determine from the
kernel alone.

**Logs** are `key=value` lines. Every mount operation includes `op`, `group`,
`name`, `source`, `target`, `trigger` and `result`. Useful filters:

```bash
journalctl -u byssusd | grep -E 'op=(conflict|reject|degrade)|result=failed'
```

**Reloads are transactional.** An invalid configuration is logged and ignored;
the daemon keeps running on the previous one, and its status line says so
until a reload succeeds. Changing `daemon.state_dir`
requires a restart; so does changing `daemon.user`.

**Restarts and upgrades** do not disturb consumers: `byssusd` never unmounts on
exit, and on start it reconciles against the existing mounts and state file.

**Reboots** clear bind mounts; `byssusd` recreates them at startup, before
container runtimes start.

## Troubleshooting

| Symptom | Likely cause and fix |
|---------|----------------------|
| `op=reject ... reason="name fails allowlist"` | Membership file name has disallowed characters. Rename it. Each rejection is logged once; `byssus status` lists current ones. |
| A member starting with `.` is never mounted, with no warning | Hidden names are ignored by design. Rename the file without the leading `.`. |
| `op=reject ... not a regular file` / `file is not empty` | Membership entries must be empty regular files. |
| `op=skip ... No such file or directory` | The member's source does not exist yet. It is mounted once it appears. |
| `op=skip ... Permission denied` (source) | `byssus` lacks search permission on some directory in the source path. Grant `setfacl -m u:byssus:x` on each directory; confirm with `sudo byssus dry-run`. |
| `op=skip ... Too many levels of symbolic links` | A symlink is in the source or target path beneath the root. Byssus never follows symlinks there; use real directories. |
| `op=conflict ... not recorded in state` | Something else is mounted at the member's target. Byssus never touches it. Unmount it or change the target template. |
| `op=conflict ... not the recorded mount` | Byssus's mount was replaced by another. Resolve manually; Byssus will re-create its mount once the target is free. |
| `op=conflict ... several members resolve to the same target` | Two members (possibly in different groups) map to one target. Adjust templates. |
| `private (mounts will not reach containers via rslave)` | The mount containing the target root is private. See [Check mount propagation](#check-mount-propagation). |
| `are slave mounts ... refusing to start` | `byssusd` is running in its own mount namespace — usually a systemd option such as `ProtectSystem=`, `PrivateTmp=`, `PrivateNetwork=` or `PrivateIPC=`. Remove it. |
| `op=degrade` | A membership directory, or a group set's `membership_root` (`group=<set>/*`), was moved, deleted or unmounted. Mounts are kept. Restore the directory and `systemctl reload byssusd`. |
| `op=reject group=<set>/* ...` | An entry in a group set's directory tree is not a directory with a valid name. Rename or remove it. |
| `state=group_unavailable` in `byssus status`, `unreadable:` in the systemd status, or `op=scan ... membership directory unreadable` in the log | `byssus` lacks read or search access to that membership directory (for a group set, often a group directory created without the inherited ACL, or moved in from elsewhere). The group is left unchanged until it can be read; fixing the permissions is picked up automatically. For a group set: `setfacl -R -m u:byssus:rX <membership_root>` and `setfacl -R -d -m u:byssus:rX <membership_root>`. |
| Members of a newly created group fail with `Permission denied` on the target | `byssus` cannot create the member directory in the view. Grant it write access to the view (or create the view with an ACL for it). |
| `state file is corrupt` | The corrupt file was renamed to `state.json.corrupt-<time>`. Existing Byssus mounts now show as conflicts; unmount them manually (or reboot), then let Byssus re-create them. |
| `another Byssus process holds the state lock` | `byssusd` is running; use `systemctl reload byssusd` instead of `byssus reconcile`. |
| `refusing to run as root` | Configure `daemon.user` (or `--user`), or run under the shipped systemd unit. |
| `CAP_SYS_ADMIN is not permitted` | Use the systemd unit, or `setcap cap_sys_admin+ep` on the binary. |
| `required kernel features are unavailable` | Upgrade to Linux 5.12 or newer. If the kernel is new enough, a seccomp filter is blocking the syscall — under systemd, remove `RestrictSUIDSGID=` or custom `SystemCallFilter=` changes. |

## Removing Byssus

1. Remove all membership files (or remove every group from the configuration)
   and reload, so Byssus unmounts everything it created:

   ```bash
   systemctl reload byssusd
   byssus status        # should list no members
   ```

2. Stop and disable the service:

   ```bash
   systemctl disable --now byssusd
   ```

3. Remove the binaries, unit, configuration, `/var/lib/byssus` and any shared
   mount you created for propagation, as desired.

Stopping `byssusd` without step 1 leaves its mounts in place until they are
unmounted manually or the host reboots.
