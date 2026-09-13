# Byssus Reference

Configuration, naming rules, command-line interface and log format. For how
these fit into the design and security contract, see [DESIGN.md](DESIGN.md).

## Contents

- [Configuration](#configuration)
- [Names and templates](#names-and-templates)
- [CLI](#cli)
- [Logging](#logging)

---

## Configuration

Configuration is TOML. The main file is `/etc/byssus/byssus.toml`; drop-in
fragments in `/etc/byssus/conf.d/*.toml` are read in lexicographic order of
file name. Files not ending in `.toml`, and hidden files, are ignored.

```toml
# /etc/byssus/byssus.toml

[daemon]
# Service user to switch to when started as root. Optional.
user = "byssus"
# Directory holding state.json and the lock file. Default shown.
state_dir = "/var/lib/byssus"
# Full reconcile of every group at this interval, in seconds; 0 disables.
# Catches mounts removed out-of-band and sources that appear after their
# membership file. Default shown.
resync_interval_secs = 60

[groups.research]
source_root = "/srv/example/projects"
source      = "{name}/workspace"
target_root = "/srv/example/groups/research/view"
target      = "{name}"
membership  = "/srv/example/membership/research"
read_only   = true    # default true  — MOUNT_ATTR_RDONLY
noexec      = true    # default true  — MOUNT_ATTR_NOEXEC
nosymfollow = false   # default false — MOUNT_ATTR_NOSYMFOLLOW

[group_sets.projects]
membership_root = "/srv/example/membership/projects"
source_root     = "/srv/example/orgs"
source          = "{group}/projects/{name}/workspace"
target_root     = "/srv/example/orgs"
target          = "{group}/groups/{subgroup}/view/{name}"
read_only       = true
noexec          = true
```

### Fields

**`[daemon]`** — only allowed in the main file.

| Field | Default | Description |
|-------|---------|-------------|
| `user` | none | The service user. `byssusd` and `byssus reconcile` switch to it when started as root; `byssus check` and `byssus dry-run` run as root check access as this user. Without it, those checks run as root, miss access problems the service user would hit, and warn about it. The packaged configuration sets `user = "byssus"`. |
| `state_dir` | `/var/lib/byssus` | Absolute path of the state directory. |
| `resync_interval_secs` | `60` | Periodic full reconcile interval; `0` disables. |

**`[groups.<group-name>]`** — allowed in the main file and in fragments. A group
name may be defined only once across all files.

| Field | Required | Description |
|-------|----------|-------------|
| `source_root` | yes | Absolute path of the trusted root beneath which sources are resolved. |
| `source` | yes | Relative template beneath `source_root`; must contain `{name}`. |
| `target_root` | yes | Absolute path of the trusted root beneath which views are mounted. |
| `target` | yes | Relative template beneath `target_root`; must contain `{name}`. |
| `membership` | yes | Absolute path of the membership directory. |
| `read_only` | no | Default `true`. When `false`, every consumer of the group can create, modify and delete files in every member's source directory, subject to ordinary file permissions for the consumer's own user. Only this setting restricts writes: binding the view read-only in a container does not make member mounts read-only. |
| `noexec` | no | Default `true`. Set `false` only if exposed files must be executable. |
| `nosymfollow` | no | Default `false`. When `true`, symlinks inside views are not followed. |

`MOUNT_ATTR_NOSUID` and `MOUNT_ATTR_NODEV` are always applied and cannot be
disabled.

**`[group_sets.<set-name>]`** — allowed in the main file and in fragments. A
group set defines any number of groups that are created and removed at runtime
by creating and removing directories, without changing configuration or
reloading. A set name may be defined only once across all files.

| Field | Required | Description |
|-------|----------|-------------|
| `membership_root` | yes | Absolute path of the directory holding the groups. With one level, each subdirectory is a group; with two, each subdirectory of a subdirectory is. A group directory's files are its members. |
| `source_root` | yes | As for groups. |
| `source` | yes | Relative template beneath `source_root`; must contain `{name}`, and may contain `{group}` and `{subgroup}`. |
| `target_root` | yes | As for groups. |
| `target` | yes | Relative template beneath `target_root`; must contain `{name}` and every directory level the set uses. |
| `read_only`, `noexec`, `nosymfollow` | no | As for groups; they apply to every group in the set. |

The set's **depth** is the number of directory levels it uses: two if either
template contains `{subgroup}`, otherwise one. The groups are then:

| Depth | Group directory | Group identity | `{group}` | `{subgroup}` |
|-------|-----------------|----------------|-----------|--------------|
| 1 | `<membership_root>/<group>/` | `<set>/<group>` | `<group>` | — |
| 2 | `<membership_root>/<group>/<subgroup>/` | `<set>/<group>/<subgroup>` | `<group>` | `<subgroup>` |

For example, with the set above the file
`/srv/example/membership/projects/acme/research/webapp` makes `webapp` a member
of the group `projects/acme/research`, mounting
`/srv/example/orgs/acme/projects/webapp/workspace` at
`/srv/example/orgs/acme/groups/research/view/webapp`.

Because `target` must contain every level, every group in a set has its own
view. Using `{group}` in `source` confines a group's members to that group's
own part of `source_root`; see
[INTEGRATION.md](INTEGRATION.md#multiple-groups-and-tenants).

Entries in a set's directory tree:

- names beginning with `.` are ignored, at every level;
- a directory with a valid [name](#names) is a group (or, at the first of two
  levels, contains groups);
- anything else — a file, symlink or other type, or a directory whose name
  fails the rules — is rejected, logged once (`op=reject group=<set>/*`) and
  listed by `byssus status`, `dry-run` and `check`;
- the membership files inside a group directory follow the ordinary
  [membership rules](DESIGN.md#membership-files).

How group directories come and go is described in
[DESIGN.md](DESIGN.md#group-sets).

**Why a root plus a relative template?** `openat2()` with `RESOLVE_BENEATH`
guarantees resolution stays beneath a directory descriptor and rejects
absolute paths. Splitting each location into a trusted root (opened once) and
a relative template (interpolated per member) makes the security boundary
explicit in the schema: roots are trusted configuration; the relative part is
derived from untrusted membership file names.

### Validation

Configuration is rejected — at startup (exit status 1) or on reload (old
configuration retained) — if any of the following fail:

- The file parses as TOML and contains no unknown keys.
- Every configuration file, and the directories containing them, are owned by
  UID 0 and are not group- or world-writable. Only root may change where
  mounts point. (`byssus status` and `byssus dry-run`, which never mount,
  report a violation as a warning so configurations can be checked before
  installation.)
- Group and group set names satisfy the [name rules](#names).
- Group set templates use only the placeholders `{name}`, `{group}` and
  `{subgroup}`, and `target` contains every level the set uses.
- `source_root`, `target_root`, `membership` and `state_dir` are absolute,
  contain no `.` or `..` components, and exist as directories.
- Templates satisfy the [template rules](#templates).
- No membership directory or `membership_root` lies at or beneath any group's
  or set's `target_root`.
- No membership directory or `membership_root` lies at, beneath or above a
  group set's `membership_root` (its subdirectories are groups).

Two members (in the same or different groups) that resolve to the same target
are detected during reconciliation and reported as conflicts; neither is
mounted.

## Names and templates

### Names

Member names come from membership file names; group and group set names come
from configuration, and group names in a set from directory names. All must:

- consist only of `A–Z`, `a–z`, `0–9`, `.`, `_`, `-`;
- be 1–255 bytes long;
- not begin with `.` (this excludes `.`, `..`, hidden files and editor
  temporary files such as `.name.swp`).

Membership entries whose names begin with `.` are ignored silently (see
[Membership files](DESIGN.md#membership-files)); any other name that fails these rules
is rejected.

A group is identified in logs, `byssus status` and the state file as its
configured name (`research`) for a statically configured group, or as
`<set>/<group>` or `<set>/<group>/<subgroup>` (`projects/acme/research`) for a
group in a set. Names cannot contain `/`, so the forms never collide. Problems
with a set as a whole, or with an entry that is not a group, use `<set>/*`.

### Templates

A template is a relative path of `/`-separated components.

- `{name}` is the member name. In a group set, `{group}` and `{subgroup}` are
  the group's directory names; statically configured groups accept only
  `{name}`. Placeholders may appear any number of times, including within a
  component (`{name}-ro`).
- Any other `{` or `}` is an error.
- The template must contain `{name}` at least once.
- Components must be non-empty and must not be `.` or `..`; no leading or
  trailing `/`.
- After interpolation the path must not exceed `PATH_MAX`.

Because names cannot contain `/` and cannot be `.` or `..`, interpolation can
never introduce new components or traversal. `RESOLVE_BENEATH` enforces the
same property again at resolution time.

## CLI

The packages install manual pages for each command (`man byssusd`,
`man byssus`, `man byssus-check`, …); they are generated from the same
definitions as `--help`.

### `byssusd`

```
byssusd [OPTIONS]
  --config <FILE>          Main config file   [default: /etc/byssus/byssus.toml]
  --config-dir <DIR>       Drop-in directory  [default: /etc/byssus/conf.d]
  --log-level <LEVEL>      error|warn|info|debug|trace [default: info]
  --user <USER>            Service user to switch to when started as root
  --allow-root             Permit running as UID 0 (development/testing)
  --allow-slave-namespace  Permit a target root on a slave mount
```

Signals: `SIGHUP` reloads; `SIGTERM`/`SIGINT` shut down cleanly.

Exit status: `0` clean shutdown; `1` startup failure (configuration, kernel,
privileges, lock).

### `byssus`

```
byssus status    [--config <FILE>] [--config-dir <DIR>] [--format text|json]
byssus dry-run   [--config <FILE>] [--config-dir <DIR>] [--format text|json]
byssus check     [--config <FILE>] [--config-dir <DIR>] [--add <FILE>]... [--remove <NAME>]...
                 [--allow-slave-namespace] [--format text|json]
byssus reconcile [--config <FILE>] [--config-dir <DIR>] [--user <USER>] [--allow-root]
                 [--allow-slave-namespace]
byssus version
```

- **`status`** — for each configured group, each group currently discovered
  in a group set, and each state record: member,
  target, and whether its mount is present and matches (`ok`), missing,
  conflicting, or has changed source; plus rejected membership entries with
  their reasons and the number of ignored hidden entries. Exits 1 only for
  error states (conflicts, unavailable targets or groups); rejected entries
  and unavailable sources are warnings. Run as root with `daemon.user`
  configured, it switches to that user and drops every capability first, so
  it shows what the daemon can see (`checked_as_uid`): a group whose
  membership directory the service user cannot read is listed as
  `<group>/*  state=group_unavailable` with the reason (an error), rather
  than its members appearing as `would_mount`. If the state file cannot be read, it
  explains why (for example, "join the `byssus` group") and falls back to a
  view derived from the kernel alone, labeled *ownership unknown*.
- **`dry-run`** — a full validation pass without mounting: configuration,
  kernel features, `/proc`, propagation, membership files, per-member source
  resolution and permission diagnostics, and the actions a reconcile would
  take. Exit status `0` if no errors (warnings allowed), `1` otherwise.
- **`check`** — predicts whether a `SIGHUP` reload would accept the
  configuration, without installing anything or signaling the daemon. With
  `--add FILE`, a candidate fragment is treated as installed in the drop-in
  directory under its file name (replacing a fragment of the same name); with
  `--remove NAME`, an installed fragment is left out. It runs the reload's
  validation: parsing, ownership and modes (for a candidate, only the file
  itself; its future directory is the drop-in directory), cross-group rules,
  paths, opening every root and membership directory, discovering the groups
  currently in each group set, creating watches, and
  propagation (a slave target root is an error unless `--allow-slave-namespace`). Run
  as root with `daemon.user` configured, it checks access as that user.
  Rejected set entries and group directories that cannot be opened are
  warnings, since they are runtime data rather than configuration; an
  unreadable `membership_root` is an error.
  Exit status `0` if a reload would accept the configuration, `1` otherwise.
  It does not compare against the running daemon's configuration, so it
  cannot report a changed `daemon.state_dir` (which fragments cannot set).
- **`reconcile`** — one reconcile pass of every group (discovering the groups
  of every set), then exit. Requires
  `CAP_SYS_ADMIN`; refuses to run while the daemon holds the state lock.
- **`version`** — print the version.

Example `dry-run` output:

```
kernel.open_tree       = ok
kernel.openat2         = ok
kernel.mount_setattr   = ok
kernel.statx_mnt_id_unique = ok
procfs                 = ok

group=research  status=ok
  membership_dir   = /srv/example/membership/research (readable)
  propagation      = shared
  members          = 12
  sources_valid    = 12
  sources_missing  = 0
  existing_mounts  = 4
  would_create     = 8
  would_remove     = 0
  conflicts        = 0

group=builds    status=warn
  membership_dir   = /srv/example/membership/builds (readable)
  propagation      = private (mounts will not reach containers)
  ...
```

## Logging

Logs are structured `key=value` (logfmt) lines on stderr. When stderr is
connected to the systemd journal (`JOURNAL_STREAM` matches), timestamps are
omitted because the journal records them.

Every mount-affecting operation logs: group, member name, source, target,
operation (`mount`, `unmount`, `drop_record`, `skip`, `reject`, `reject_cleared`, `conflict`, `degrade`),
trigger (`startup`, `inotify`, `resync`, `reload`, `cli`) and result.

```
ts=2026-09-12T14:30:01Z level=info op=mount group=research name=libcurl source=/srv/example/projects/libcurl/workspace target=/srv/example/groups/research/view/libcurl trigger=inotify result=ok
ts=2026-09-12T14:30:02Z level=warn op=reject group=research name=bad:name reason="name fails allowlist: name contains disallowed byte 0x3a at offset 3" trigger=inotify
ts=2026-09-12T14:30:03Z level=warn op=conflict group=research name=mystery target=/srv/example/groups/research/view/mystery reason="mount present at target but not recorded in state; not touching foreign mount"
```

Paths in log output are for humans only; they are never fed back into
syscalls.
