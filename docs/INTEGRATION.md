# Integrating with Byssus

For platforms that build on Byssus: attaching containers to group views and
managing membership from an application. Installation and operation are
covered in [OPERATIONS.md](OPERATIONS.md).

## Contents

- [Attach containers](#attach-containers)
- [Integrate an application](#integrate-an-application)
- [Multiple groups and tenants](#multiple-groups-and-tenants)

---

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
- Files whose names start with `.` are ignored without a warning, so an
  application can prepare a file under a hidden name and `rename(2)` it into
  place, and files like `.gitkeep` are harmless.
- Creating a membership file never fails because of Byssus: the daemon picks
  changes up asynchronously. Check `byssus status` (or its JSON output) to see
  whether an entry was accepted or rejected, and why.
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

## Multiple groups and tenants

One `byssusd` serves any number of groups, including groups belonging to
different tenants (organizations, customers, teams). Running one daemon per
tenant adds little: every instance needs `CAP_SYS_ADMIN`, so a compromised
daemon is equally dangerous whichever tenant it serves, while the extra units,
state directories and failure points are real costs. The boundaries between
groups come from how you lay out and wire the configuration, described below.

### What keeps groups apart

1. **Which view a consumer receives.** Byssus only creates mounts beneath each
   group's `target_root`. A container sees a group's files only if your
   platform bind-mounts *that group's* view into it. Give every group its own
   `target_root`, and attach to each container only the views of the groups
   its project belongs to. This wiring is your platform's responsibility and
   is the primary boundary.

2. **Where a member name can resolve.** A membership file name is resolved
   only beneath its own group's `source_root`: names cannot contain `/` or
   `..`, and the kernel enforces `RESOLVE_BENEATH`. If a tenant's groups use
   that tenant's own `source_root`, no membership entry can expose another
   tenant's files — a name that does not exist beneath the root simply fails
   to resolve.

3. **Who can change membership.** Write access to a membership directory is
   the authority to expose any source beneath that group's `source_root` to
   the group. Keep membership directories writable only by the component that
   manages membership.

4. **Who can change where things point.** Configuration is root-owned and
   re-read only on reload, so only the privileged step that writes it can
   change roots and templates.

### Rules

- **Give each tenant its own `source_root`**, for example
  `/data/tenants/<tenant>/projects`. Never point several tenants' groups at a
  shared parent such as `/data/tenants`.
- **Give each group its own `target_root`**, for example
  `/data/tenants/<tenant>/groups/<group>/view`. Distinct roots also avoid
  target collisions (collision detection compares configured paths and does
  not yet detect two different paths that reach the same directory).
- **Never mount membership directories into containers.** A consumer that can
  write one could add any project of that tenant to its group.
- **Never give consumers write access to views** (Byssus mounts are read-only
  regardless, but the view directory itself should not be writable either).
- **Prefix group names with the tenant**, for example `acme-research`. Group
  names are global to the daemon.
- **Keep the `byssus` group small.** Members can read the state file and run
  `byssus status`, which shows group and member names for every tenant.
- **One project may belong to several groups.** Each group gets its own
  independent mount.

The `byssus` service user needs search permission into every tenant's source
tree (see [OPERATIONS.md](OPERATIONS.md#source-directories)). It never needs
read permission, and this is the same whether one daemon or several run.

### Layout: one fragment per tenant

```toml
# /etc/byssus/conf.d/acme.toml — owned by root, mode 0644
[groups.acme-research]
source_root = "/data/tenants/acme/projects"
source      = "{name}/workspace"
target_root = "/data/tenants/acme/groups/research/view"
target      = "{name}"
membership  = "/var/lib/platform/membership/acme-research"

[groups.acme-builds]
source_root = "/data/tenants/acme/projects"
source      = "{name}/workspace"
target_root = "/data/tenants/acme/groups/builds/view"
target      = "{name}"
membership  = "/var/lib/platform/membership/acme-builds"
```

Keeping each tenant in its own fragment means adding, changing or removing a
tenant touches one file.

### Changing a tenant's configuration safely

Reloads are all-or-nothing: if **any** fragment is invalid, the reload is
rejected and the daemon keeps its previous configuration for **every**
tenant. Existing groups keep working — including membership changes — but new
or changed groups wait until the problem is fixed. Validate before you
install:

```bash
# 1. Write the candidate somewhere private, owned by root, mode 0644.
install -o root -g root -m 0644 /dev/stdin /run/platform/acme.toml < generated-acme.toml

# 2. Check the configuration a reload would see, with the candidate in place.
#    Run as root: access is checked as the service user.
byssus check --add /run/platform/acme.toml || exit 1

# 3. Install atomically. Hidden names are ignored by the daemon, so write
#    under a hidden name in the drop-in directory and rename into place.
install -o root -g root -m 0644 /run/platform/acme.toml /etc/byssus/conf.d/.acme.toml.tmp
mv /etc/byssus/conf.d/.acme.toml.tmp /etc/byssus/conf.d/acme.toml

# 4. Apply.
systemctl reload byssusd
```

To remove a tenant, check with `byssus check --remove acme.toml`, delete the
fragment and reload; the tenant's mounts are removed. `byssus check` also
verifies that the membership and root directories exist and can be opened by
the service user, and that target roots are not on slave-only mounts.

Create a tenant's directories — source root, target roots on a shared mount,
membership directories — **before** installing its fragment; a fragment that
refers to missing directories fails the check.

A reload's outcome is reported in the daemon's log (`msg="configuration
reloaded"` or `msg="reload failed; keeping previous configuration"`). A
successful `byssus check` immediately beforehand makes failure unlikely, but
it is not atomic with the reload: something changing in between (for example
a directory being removed) can still make the reload fail.
