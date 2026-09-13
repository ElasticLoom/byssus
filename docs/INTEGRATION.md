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

Mount the group's view into containers **with `rslave` propagation**. New
members then appear without restarting the container. Binding the view
`readonly` is recommended so consumers cannot create files next to the member
directories, but note what it does *not* do (below).

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

Inside the container, `/group/<member>` is each member's source directory, with
`nosuid`, `nodev`, and by default read-only and `noexec`. Filesystems mounted
*inside* a member's source directory are never exposed.

**Only the group's `read_only` setting controls whether members are
writable.** Each member is its own mount, and a read-only flag on the view's
bind in the container does not apply to mounts beneath it. With
`read_only = false`, consumers can write into members even through a
`readonly` view bind.

**Read-write groups** (`read_only = false`) let every consumer of the group
create, modify and delete files in every member's source directory, as the
consumer's own user and subject to ordinary file permissions. Use them only
when every consumer of the group is trusted to change every member.

**Changing a group's attributes** (for example switching `read_only`) is
applied by re-creating each member's mount, because mount attributes do not
propagate into running containers. Members briefly disappear and reappear in
consumers; processes with files already open keep using them.

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

If your application creates groups at runtime, use a
[group set](#groups-created-at-runtime-group-sets): creating a group is then
creating a directory. Statically configured groups are added and removed by
changing the configuration and reloading, which needs root:

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
   group's target. A container sees a group's files only if your platform
   bind-mounts *that group's* view into it. Give every group its own view, and
   attach to each container only the views of the groups its project belongs
   to. This wiring is your platform's responsibility and is the primary
   boundary.

2. **Where a member name can resolve.** A membership file name is resolved
   only beneath its group's source: names cannot contain `/` or `..`, and the
   kernel enforces `RESOLVE_BENEATH`. In a group set whose `source` starts
   with `{group}/`, a name can only resolve inside that group's own directory
   (for example `/data/orgs/acme/projects/`); a name that exists only in
   another tenant's directory simply fails to resolve. For static groups, give
   each tenant its own `source_root` for the same effect.

3. **Who can change membership.** Write access to a membership directory is
   the authority to expose any source the group's templates can reach. For a
   group set, write access to `membership_root` (or to a tenant's directory
   in it) is also the authority to create groups there. Keep membership
   writable only by the component that manages membership.

4. **Who can change where things point.** Configuration is root-owned and
   re-read only on reload, so only the privileged step that writes it can
   change roots and templates. Group and member names chosen at runtime can
   only fill in the placeholders root put in the templates.

Byssus enforces 2 and the separation of views; which projects join which group
(3) and which containers receive which views (1) are your platform's
decisions.

### Groups created at runtime: group sets

When groups are created by users, define them once as a group set instead of
writing configuration per group. Root installs one fragment, once:

```toml
# /etc/byssus/conf.d/platform.toml — owned by root, mode 0644
[group_sets.projects]
membership_root = "/var/lib/platform/membership"
source_root     = "/data/orgs"
source          = "{group}/projects/{name}/workspace"
target_root     = "/data/orgs"
target          = "{group}/groups/{subgroup}/view/{name}"
read_only       = true
noexec          = true
```

Here `{group}` is the organization and `{subgroup}` a user-created group in
it. The application then manages everything with ordinary directories and
files, with no root and no reload:

```bash
m=/var/lib/platform/membership

# Create group "research" in org "acme", with its view.
mkdir -p /data/orgs/acme/groups/research/view
mkdir -p "$m/acme/research"

# Add and remove members.
touch "$m/acme/research/webapp"
rm    "$m/acme/research/webapp"

# Remove the group (all its mounts are removed), then its view.
rm -r "$m/acme/research"
rmdir /data/orgs/acme/groups/research/view   # once byssus status shows no members
```

The group is `projects/acme/research` in logs and `byssus status`. Its view is
`/data/orgs/acme/groups/research/view`; a member `webapp` mounts
`/data/orgs/acme/projects/webapp/workspace` there, and a member name that only
exists in another org is reported as an unavailable source.

Things to know:

- **Create the view before the group directory**, or make sure the `byssus`
  user can create it (it creates missing target directories with mode `0755`
  where it has write access). Otherwise mounts fail with a logged permission
  error and are retried at the next change or resync.
- **Group directories follow the name rules** (`A–Z a–z 0–9 . _ -`, not
  starting with `.`). Hidden entries are ignored at every level, so a group
  can be prepared under a hidden name and renamed into place. Files, symlinks
  and invalidly named directories are rejected and listed by `byssus status`.
- **Deleting a group directory removes the group.** Deleting or moving
  `membership_root` itself instead freezes the whole set: mounts are kept and
  nothing changes until the directory is restored and the daemon reloaded.
- **The `byssus` user needs read and search access to every level** of the
  membership tree; a default ACL on `membership_root` gives new directories
  that access (see [OPERATIONS.md](OPERATIONS.md#group-set-membership)).
- For a single level (groups without organizations), leave out `{subgroup}`,
  for example `target = "{group}/view/{name}"`; groups are then the
  directories directly in `membership_root`.
- Several sets may coexist (for example one per kind of group), each with its
  own `membership_root`, alongside statically configured groups.

### Rules

- **Confine each tenant's sources**: start a set's `source` with `{group}/`,
  or give each tenant's static groups their own `source_root` such as
  `/data/tenants/<tenant>/projects`. Never let one tenant's groups resolve
  names beneath a parent shared with other tenants.
- **Give each group its own view.** Group sets guarantee this (`target` must
  use every level). For static groups, give each its own `target_root`, for
  example `/data/tenants/<tenant>/groups/<group>/view`. Distinct targets also
  avoid collisions (collision detection compares configured paths and does not
  yet detect two different paths that reach the same directory).
- **Never mount membership directories into containers.** A consumer that can
  write one could add any project of that tenant to its group, or create
  groups.
- **Bind views into containers `readonly`** so consumers cannot create files
  beside member directories. This does not make members read-only; the
  group's `read_only` setting does.
- **Prefix static group names with the tenant**, for example
  `acme-research`. Static group names are global to the daemon; groups in a
  set are already qualified by their directories.
- **Keep the `byssus` group small.** Members can read the state file and run
  `byssus status`, which shows group and member names for every tenant.
- **One project may belong to several groups.** Each group gets its own
  independent mount.

The `byssus` service user needs search permission into every tenant's source
tree (see [OPERATIONS.md](OPERATIONS.md#source-directories)). It never needs
read permission, and this is the same whether one daemon or several run.

### Static groups: one fragment per tenant

If groups change rarely and are managed by a privileged deployment step,
static groups work too:

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

To move a tenant from static groups to a group set, create the set's group
directories with the same member files, then remove the static fragment and
reload; mounts move to the set's groups during that reload. If the set's
targets are the same paths as the static groups', the members are reported as
colliding (and the static mounts kept) until the reload.

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
the service user, and that target roots are not on slave mounts.

Create a tenant's directories — source root, target roots on a shared mount,
membership directories — **before** installing its fragment; a fragment that
refers to missing directories fails the check.

A reload's outcome is reported in the daemon's log (`msg="configuration
reloaded"` or `msg="reload failed; keeping previous configuration"`) and in
its systemd status line: after a rejected reload,
`systemctl show -p StatusText --value byssusd` includes
`last reload failed, running previous configuration: <reason>` until a later
reload succeeds. A
successful `byssus check` immediately beforehand makes failure unlikely, but
it is not atomic with the reload: something changing in between (for example
a directory being removed) can still make the reload fail.
