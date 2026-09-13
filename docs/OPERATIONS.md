# Operating Byssus

This guide covers installing, configuring, running and troubleshooting
Byssus. The paths below (`/srv/example/...`) are placeholders; substitute your
own layout. For the design and security contract, see [DESIGN.md](DESIGN.md).

## Contents

- [Requirements](#requirements)
- [Install the binaries](#install-the-binaries)
- [Create the service user](#create-the-service-user)
- [Lay out directories and permissions](#lay-out-directories-and-permissions)
- [Create the propagation anchor](#create-the-propagation-anchor)
- [Configure groups](#configure-groups)
- [Check the deployment](#check-the-deployment)
- [Run under systemd](#run-under-systemd)
- [Run without systemd](#run-without-systemd)
- [Attach containers](#attach-containers)
- [Integrate an application](#integrate-an-application)
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

## Install the binaries

From a source checkout:

```bash
rustup target add x86_64-unknown-linux-musl     # or aarch64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl

install -o root -g root -m 0755 target/x86_64-unknown-linux-musl/release/byssusd /usr/local/bin/
install -o root -g root -m 0755 target/x86_64-unknown-linux-musl/release/byssus  /usr/local/bin/
```

Both binaries are statically linked. Do not add file capabilities when using
systemd.

## Create the service user

With systemd-sysusers:

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

## Create the propagation anchor

Consumers see new mounts through **shared** mount propagation. The target root
must lie on a mount marked shared. If it is an ordinary directory, bind-mount
an ancestor onto itself and mark it shared.

Persistently, with the example systemd mount unit:

```bash
install -m 0644 contrib/systemd/srv-example-groups.mount /etc/systemd/system/
# The file name must match the mount point: systemd-escape --path /srv/example/groups
systemctl daemon-reload
systemctl enable --now srv-example-groups.mount
```

Or with `/etc/fstab`:

```
/srv/example/groups  /srv/example/groups  none  bind,shared  0 0
```

Check it:

```bash
findmnt -o TARGET,PROPAGATION /srv/example/groups
```

## Configure groups

Copy and edit the examples:

```bash
install -o root -g root -m 0644 examples/byssus.toml /etc/byssus/byssus.toml
install -o root -g root -m 0644 examples/conf.d/research.toml /etc/byssus/conf.d/research.toml
```

Configuration files and their directories must be owned by root and not group-
or world-writable; `byssusd` refuses to start otherwise. See
[DESIGN.md](DESIGN.md#configuration) for every option.

## Check the deployment

Run a full validation without mounting anything:

```bash
sudo byssus dry-run
```

As root with `daemon.user` configured, `dry-run` switches to that user and
drops every capability first, so permission problems show up exactly as the
daemon would experience them. It reports kernel features, `/proc`,
configuration paths, state, propagation, and per group: members, rejected
membership entries, sources that resolve or not, existing mounts, what a
reconcile would create or remove, and conflicts. It exits 1 if anything is an
error.

`--format json` produces the same report as JSON.

## Run under systemd

```bash
install -m 0644 contrib/systemd/byssusd.service /etc/systemd/system/byssusd.service
systemctl daemon-reload
systemctl enable --now byssusd
journalctl -u byssusd -f
```

The shipped unit:

- runs as `byssus` with only `CAP_SYS_ADMIN` (ambient) and `NoNewPrivileges=yes`;
- creates `/var/lib/byssus` (`StateDirectory=`, mode `0750`);
- applies a system call filter, address-family, namespace, realtime, SUID,
  personality and W^X restrictions, and private network and IPC namespaces;
- starts before Docker, containerd and Podman so containers see every member.

**Do not add options that create a mount namespace** — `ProtectSystem=`,
`PrivateTmp=`, `ReadWritePaths=` and many others (the full list is at the top
of the unit file). They trap every mount inside the service. `byssusd` detects
the resulting slave-only propagation at startup and refuses to run, rather
than silently doing nothing useful. If you need to customize the unit, use a
drop-in (`systemctl edit byssusd`) and keep to non-namespace options.

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

## Attach containers

Mount the group's view into containers **read-only with `rslave`
propagation**. New members then appear without restarting the container.

Docker:

```bash
docker run \
  --mount type=bind,source=/srv/example/groups/research/view,target=/group,readonly,bind-propagation=rslave \
  ...
```

Compose:

```yaml
services:
  agent:
    volumes:
      - type: bind
        source: /srv/example/groups/research/view
        target: /group
        read_only: true
        bind:
          propagation: rslave
```

Inside the container, `/group/<member>` is each member's source directory,
read-only, with `nosuid`, `nodev` and (by default) `noexec`. Filesystems
mounted *inside* a member's source directory are never exposed.

## Integrate an application

Membership is file presence:

```bash
# Add a member
touch /srv/example/membership/research/libcurl
# Remove a member
rm /srv/example/membership/research/libcurl
```

Rules:

- Names use only `A–Z a–z 0–9 . _ -`, are at most 255 bytes, and must not
  start with `.`.
- A member file must be an **empty regular file**. Symlinks, directories and
  non-empty files are rejected (and a member whose file becomes non-empty is
  removed).
- Files whose names start with `.` are ignored, so an application can prepare
  a file under a hidden name and `rename(2)` it into place.
- Changes take effect about 150 ms after the last change in a burst.
- Adding a member before its source directory exists is fine: it is mounted
  at the next periodic resync after the source appears (default 60 s), or
  immediately on any other membership change.

Write access to a membership directory is the authority to change what the
group sees. Restrict it to the application that manages membership.

To add or remove a whole group, change the configuration and reload:

```bash
systemctl reload byssusd      # sends SIGHUP
```

## Day-to-day operation

| Task | Command |
|------|---------|
| Show members and their mount state | `byssus status` (add `--format json` for tooling) |
| Validate everything without changes | `sudo byssus dry-run` |
| Reload configuration | `systemctl reload byssusd` |
| Follow logs | `journalctl -u byssusd -f` |
| One-off reconcile (daemon stopped) | `sudo byssus reconcile --user byssus` |

`byssus status` exits 1 if any member is in an error state, so it can be used
for monitoring. If the state file cannot be read (for example, the caller is
not in the `byssus` group) it says so and shows what it can determine from the
kernel alone.

**Logs** are `key=value` lines. Every mount operation includes `op`, `group`,
`name`, `source`, `target`, `trigger` and `result`. Useful filters:

```bash
journalctl -u byssusd | grep -E 'op=(conflict|reject|degrade)|result=failed'
```

**Reloads are transactional.** An invalid configuration is logged and ignored;
the daemon keeps running on the previous one. Changing `daemon.state_dir`
requires a restart; so does changing `daemon.user`.

**Restarts and upgrades** do not disturb consumers: `byssusd` never unmounts on
exit, and on start it reconciles against the existing mounts and state file.

**Reboots** clear bind mounts; `byssusd` recreates them at startup, before
container runtimes start.

## Troubleshooting

| Symptom | Likely cause and fix |
|---------|----------------------|
| `op=reject ... reason="name fails allowlist"` | Membership file name has disallowed characters or starts with `.`. Rename it. |
| `op=reject ... not a regular file` / `file is not empty` | Membership entries must be empty regular files. |
| `op=skip ... No such file or directory` | The member's source does not exist yet. It is mounted once it appears. |
| `op=skip ... Permission denied` (source) | `byssus` lacks search permission on some directory in the source path. Grant `setfacl -m u:byssus:x` on each directory; confirm with `sudo byssus dry-run`. |
| `op=skip ... Too many levels of symbolic links` | A symlink is in the source or target path beneath the root. Byssus never follows symlinks there; use real directories. |
| `op=conflict ... not recorded in state` | Something else is mounted at the member's target. Byssus never touches it. Unmount it or change the target template. |
| `op=conflict ... not the recorded mount` | Byssus's mount was replaced by another. Resolve manually; Byssus will re-create its mount once the target is free. |
| `op=conflict ... several members resolve to the same target` | Two members (possibly in different groups) map to one target. Adjust templates. |
| `private (mounts will not reach containers via rslave)` | The target root is not on a shared mount. See [Create the propagation anchor](#create-the-propagation-anchor). |
| `slave-only mounts; refusing to start` | `byssusd` is running in its own mount namespace — usually a systemd option such as `ProtectSystem=` or `PrivateTmp=`. Remove it. |
| `op=degrade` | A membership directory was moved, deleted or unmounted. Mounts are kept. Restore the directory and `systemctl reload byssusd`. |
| `state file is corrupt` | The corrupt file was renamed to `state.json.corrupt-<time>`. Existing Byssus mounts now show as conflicts; unmount them manually (or reboot), then let Byssus re-create them. |
| `another Byssus process holds the state lock` | `byssusd` is running; use `systemctl reload byssusd` instead of `byssus reconcile`. |
| `refusing to run as root` | Configure `daemon.user` (or `--user`), or run under the shipped systemd unit. |
| `CAP_SYS_ADMIN is not permitted` | Use the systemd unit, or `setcap cap_sys_admin+ep` on the binary. |
| `kernel is missing required features` | Upgrade to Linux 5.12 or newer. |

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

3. Remove the binaries, unit, configuration, `/var/lib/byssus` and the
   propagation anchor as desired.

Stopping `byssusd` without step 1 leaves its mounts in place until they are
unmounted manually or the host reboots.
