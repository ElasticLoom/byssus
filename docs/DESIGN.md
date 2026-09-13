# Byssus Design

> *Byssus creates live filesystem attachments between isolated workspaces.*

This document is the authoritative design and security contract for Byssus.
Implementation progress is tracked in [TODO.md](TODO.md). Where the code and
this document disagree, one of them has a bug — please open an issue.

## Contents

- [Problem](#problem)
- [Principles](#principles)
- [Concepts](#concepts)
- [Architecture](#architecture)
- [Binaries and privilege model](#binaries-and-privilege-model)
- [Configuration](#configuration)
- [Names and templates](#names-and-templates)
- [Membership files](#membership-files)
- [Mount creation](#mount-creation)
- [Mount identity](#mount-identity)
- [Unmounting](#unmounting)
- [State file](#state-file)
- [Reconciliation](#reconciliation)
- [Daemon lifecycle](#daemon-lifecycle)
- [Propagation and mount namespaces](#propagation-and-mount-namespaces)
- [Kernel requirements](#kernel-requirements)
- [CLI](#cli)
- [Logging](#logging)
- [Deployment](#deployment)
- [Security contract](#security-contract)
- [Testing strategy](#testing-strategy)
- [Limitations](#limitations)
- [Future work](#future-work)
- [Design decisions log](#design-decisions-log)

---

## Problem

Systems that run many isolated workspaces — one container per project, per
tenant, per agent — sometimes need a *group* of those workspaces to see each
other's files. Typical examples: analysis agents that should read findings
produced by peer projects, or build sandboxes that should see a shared set of
sibling checkouts.

The obvious approaches all have drawbacks:

- **Copying or syncing** creates derived data that diverges from its source.
- **An API or query service** forces every consumer to learn a new interface,
  when the consumers already know how to read files.
- **Static bind mounts in container definitions** require restarting
  containers whenever group membership changes.

Byssus gives each member of a group a live, read-only view of the other
members' directories, and lets membership change at runtime without restarting
anything.

## Principles

1. **The source directory is the source of truth.** No copies, no sync, no
   derived stores that can diverge.
2. **Consumers read files, not APIs.** A directory tree is the interface.
3. **Live membership changes.** Adding or removing a member takes effect in
   already-running containers.
4. **Isolation.** Consumers outside a group see nothing. Views are read-only,
   so a consumer cannot modify another member's files through Byssus.
5. **Declarative membership.** The presence of a file in a membership
   directory is the desired state. The daemon continuously reconciles the
   kernel's mount table toward it.
6. **Least privilege, verified by the binary.** The daemon holds only the
   capability it needs, touches only paths beneath configured roots, and never
   acts on a mount it cannot prove it created.

## Concepts

| Term | Meaning |
|------|---------|
| **Group** | A named set of members sharing one view. Defined in configuration. |
| **Member** | A name (for example `libcurl`) whose source directory is exposed in the group's view. |
| **Membership directory** | A directory whose regular, empty files name the group's members. Written by the integrating application; watched by `byssusd`. |
| **Source root / source** | A trusted absolute directory, plus a relative template (for example `{name}/workspace`) locating a member's source directory beneath it. |
| **Target root / target** | A trusted absolute directory, plus a relative template (for example `{name}`) locating where a member's view is mounted beneath it. |
| **View** | The target root as seen by consumers — typically bind-mounted into containers with `rslave` propagation. |

## Architecture

```
 Application                byssusd                        Containers
 ───────────                ───────                        ──────────
 touch membership file  →   inotify event
                            validate name + file
                            openat2(RESOLVE_BENEATH)
                            open_tree(OPEN_TREE_CLONE)
                            mount_setattr(RDONLY|...)
                            move_mount → target       →    rslave propagation:
                            statx → state.json             files visible
                                                           immediately

 rm membership file     →   inotify event
                            verify mount identity
                            umount2(MNT_DETACH)       →    files disappear
```

A single-threaded event loop multiplexes three sources with `epoll`: an
inotify descriptor watching every membership directory, a `signalfd` for
`SIGHUP`/`SIGTERM`/`SIGINT`, and a timeout for periodic resynchronization. No
async runtime is used.

## Binaries and privilege model

The package builds two binaries from one library crate:

- **`byssusd`** — the daemon. Requires `CAP_SYS_ADMIN` for mount operations.
- **`byssus`** — the CLI. `status`, `dry-run` and `version` need no
  capabilities. `reconcile` performs mounts and needs `CAP_SYS_ADMIN`.

All paths (config, state) use the project name `byssus`, not `byssusd`.

Release binaries are statically linked against musl, so there is no dynamic
loader and no `LD_PRELOAD`/`LD_LIBRARY_PATH` surface.

### Filesystem permissions (DAC)

`CAP_SYS_ADMIN` authorizes mounting; it does **not** bypass ordinary
discretionary access control. `byssusd` runs as a dedicated unprivileged user
(conventionally `byssus`) and is granted **no** DAC-bypassing capabilities
(`CAP_DAC_READ_SEARCH`, `CAP_DAC_OVERRIDE`). A compromised daemon therefore
cannot read arbitrary files on the host — not even the contents of the
directories it exposes.

The deployer grants exactly the access needed:

| Path | Access required by the `byssus` user |
|------|--------------------------------------|
| Every directory from `/` down to and including each resolved source directory | search (`x`) |
| `target_root` (and any nested target parents) | read, write, search — normally owned by `byssus` |
| Each membership directory | read + search (the *application* holds write access) |
| State directory (`/var/lib/byssus`) | owned by `byssus`, mode `0750` |
| Configuration (`/etc/byssus`) | read |

Search-only access to source trees is typically granted with POSIX ACLs,
including *default* ACLs so newly created members inherit it — see
[Deployment](#deployment). `byssus dry-run` and the daemon's logs report
missing permissions per member with an actionable message.

**Write access to a membership directory is authorization** to change that
group's exposure. Byssus does not decide who may write membership files; the
deployer does, through permissions on the membership directory. The `byssus`
group (which may read state) must never confer write access to membership
directories.

### Capability normalization

However the process was launched, `byssusd` and `byssus reconcile` bring
themselves to a known minimal state immediately after parsing configuration
(which is root-owned and needed to learn the service user) and before
touching anything else:

1. Read the current credentials. If `CAP_SYS_ADMIN` is not permitted, exit
   with a diagnostic explaining the systemd and `setcap` options.
2. Decide whether to switch user. If any of the real, effective or saved UIDs
   is 0:
   - with a service user configured (`daemon.user` or `--user`), switch to it
     (a service user with UID 0 is rejected);
   - otherwise refuse to start, unless `--allow-root` is given (intended for
     development and tests), in which case stay UID 0 with a warning.

   If not running as root, a configured service user is not applied (a
   warning is logged if the current UID differs from it).
3. Clear the ambient set, and raise into the effective set any capability the
   following steps use (`CAP_SETPCAP`, `CAP_SETUID`, `CAP_SETGID`).
4. If `CAP_SETPCAP` is permitted (only when started as root): drop every
   capability except `CAP_SYS_ADMIN` from the bounding set, then set and lock
   the securebits `NOROOT`, `NO_SETUID_FIXUP`, `KEEP_CAPS` (cleared) and
   `NO_CAP_AMBIENT_RAISE`. Without `CAP_SETPCAP` (systemd or `setcap`
   launches) the bounding set cannot be changed; `PR_SET_NO_NEW_PRIVS` below
   makes it irrelevant.
5. If switching user: `setgroups` (primary plus supplementary groups),
   `setresgid`, `setresuid`. `NO_SETUID_FIXUP` preserves capabilities across
   the switch; in the unusual case that `CAP_SETPCAP` was unavailable,
   `PR_SET_KEEPCAPS` is used for the switch instead.
6. Set effective and permitted sets to exactly `CAP_SYS_ADMIN`, and the
   inheritable set to empty.
7. Set `PR_SET_NO_NEW_PRIVS`, so no later `execve()` can gain privilege.
8. Re-read credentials and verify they match the plan exactly; exit if not.
9. Log the resulting state, for example
   `level=info msg="privileges normalized" uid=991 caps=cap_sys_admin`.

The daemon is single-threaded at this point, so per-thread credential syscalls
apply to the whole process.

Supported launch methods:

- **systemd (recommended):** `User=byssus`, `AmbientCapabilities=CAP_SYS_ADMIN`,
  `CapabilityBoundingSet=CAP_SYS_ADMIN`, `NoNewPrivileges=yes`. The binary has
  no file capabilities.
- **Standalone, unprivileged user:** `setcap cap_sys_admin+ep` on the binary.
  Do not set `no_new_privs` *before* exec — it prevents file capabilities from
  applying. (The daemon sets it itself after exec, which is safe.)
- **Standalone, started as root:** configure `daemon.user`; the daemon switches
  user and drops privileges itself.

`byssus dry-run`, when started as root with a service user configured, switches
to that user and drops *all* capabilities before checking access, so its
permission diagnostics reflect what the daemon will actually experience.

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
```

### Fields

**`[daemon]`** — only allowed in the main file.

| Field | Default | Description |
|-------|---------|-------------|
| `user` | none | User to switch to when started as root. |
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
| `read_only` | no | Default `true`. |
| `noexec` | no | Default `true`. Set `false` only if exposed files must be executable. |
| `nosymfollow` | no | Default `false`. When `true`, symlinks inside views are not followed. |

`MOUNT_ATTR_NOSUID` and `MOUNT_ATTR_NODEV` are always applied and cannot be
disabled.

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
- Group names satisfy the [name rules](#names).
- `source_root`, `target_root`, `membership` and `state_dir` are absolute,
  contain no `.` or `..` components, and exist as directories.
- Templates satisfy the [template rules](#templates).
- No membership directory lies at or beneath any group's `target_root`.

Two members (in the same or different groups) that resolve to the same target
are detected during reconciliation and reported as conflicts; neither is
mounted.

## Names and templates

### Names

Member names come from membership file names; group names come from
configuration. Both must:

- consist only of `A–Z`, `a–z`, `0–9`, `.`, `_`, `-`;
- be 1–255 bytes long;
- not begin with `.` (this excludes `.`, `..`, hidden files and editor
  temporary files such as `.name.swp`).

A membership file whose name fails these rules is ignored and logged at
`warn` as `op=reject`.

### Templates

A template is a relative path of `/`-separated components.

- Only the placeholder `{name}` is recognized; it may appear any number of
  times, including within a component (`{name}-ro`).
- Any other `{` or `}` is an error.
- The template must contain `{name}` at least once.
- Components must be non-empty and must not be `.` or `..`; no leading or
  trailing `/`.
- After interpolation the path must not exceed `PATH_MAX`.

Because names cannot contain `/` and cannot be `.` or `..`, interpolation can
never introduce new components or traversal. `RESOLVE_BENEATH` enforces the
same property again at resolution time.

## Membership files

During reconciliation `byssusd` lists the membership directory through a
directory descriptor and inspects each entry with
`statx(dirfd, name, AT_SYMLINK_NOFOLLOW)`. An entry makes its name a member
only if:

- the name satisfies the [name rules](#names);
- it is a regular file (symlinks, directories, FIFOs, sockets and devices are
  rejected);
- its size is 0.

Each rejected entry is logged at `warn` with `op=reject` and a reason. A
previously valid membership file that becomes invalid (for example, data is
written into it) removes that member.

## Mount creation

Every path operation is relative to descriptors for the trusted roots. No
string-constructed path reaches a privileged syscall.

```
     source_root_fd                        target_root_fd
          │                                      │
   openat2(source,                       mkdirat() per component
     O_PATH|O_DIRECTORY,                 openat2(target,
     RESOLVE_BENEATH|                      O_PATH|O_DIRECTORY,
     RESOLVE_NO_SYMLINKS|                  RESOLVE_BENEATH|
     RESOLVE_NO_MAGICLINKS)                RESOLVE_NO_SYMLINKS|
          │                                RESOLVE_NO_MAGICLINKS)
          │                                      │
   open_tree(source_fd, "",                      │
     AT_EMPTY_PATH|OPEN_TREE_CLONE)              │
          │                                      │
   mount_setattr(tree_fd, "", AT_EMPTY_PATH,     │
     RDONLY|NOSUID|NODEV|NOEXEC|...)             │
          │                                      │
   move_mount(tree_fd, "", target_fd, "", ◄──────┘
     MOVE_MOUNT_F_EMPTY_PATH|MOVE_MOUNT_T_EMPTY_PATH)
          │
   statx(tree_fd) → identity → state.json
```

1. **Resolve the source**: `openat2(source_root_fd, source, O_PATH |
   O_DIRECTORY | O_CLOEXEC, RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
   RESOLVE_NO_MAGICLINKS)`. Failure skips the member with a warning (for
   example, the source does not exist yet; periodic resync retries it).
   `RESOLVE_NO_MAGICLINKS` is passed explicitly even though `RESOLVE_BENEATH`
   currently implies it, because the kernel documentation says not to rely on
   that.
2. **Create the target directory**: walk the target components from
   `target_root_fd`, using `mkdirat(parent_fd, component, 0o755)` (tolerating
   `EEXIST`) followed by `openat2(parent_fd, component, O_PATH | O_DIRECTORY,
   RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)` for each.
3. **Inspect the target**: if it is already a mount root, reconciliation
   treats it as described in [Reconciliation](#reconciliation); creation
   proceeds only when nothing is mounted there.
4. **Clone the source**: `open_tree(source_fd, "", AT_EMPTY_PATH |
   OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC)` — deliberately **without**
   `AT_RECURSIVE` (see below). The result is a detached mount, invisible to
   everyone.
5. **Set attributes**: `mount_setattr(tree_fd, "", AT_EMPTY_PATH, …)` with
   `MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV`, plus `MOUNT_ATTR_RDONLY`,
   `MOUNT_ATTR_NOEXEC` and `MOUNT_ATTR_NOSYMFOLLOW` as configured.
   Attributes are only ever *set*, never cleared: a clone inherits its source
   mount's restrictions (for example, a read-only source filesystem stays
   read-only), and a view is never more permissive than its source.
6. **Attach**: `move_mount(tree_fd, "", target_fd, "", MOVE_MOUNT_F_EMPTY_PATH
   | MOVE_MOUNT_T_EMPTY_PATH)`. Both flags are required because both paths are
   empty.
7. **Record identity**: `statx` on `tree_fd`, which now refers to the attached
   mount, yields its [identity](#mount-identity). Write the state record.

The mount is never visible in a writable or otherwise under-restricted state:
attributes are applied while it is still detached.

### No recursive submounts

`OPEN_TREE_CLONE` is used without `AT_RECURSIVE`. If a source directory has
other filesystems mounted inside it, those submounts are **not** exposed
through the view; only files that genuinely live in the source directory are
visible. This is a deliberate security invariant: someone may graft sensitive
content into a member's directory, and it must not be exported to every peer
automatically. Integration tests verify it.

## Mount identity

To avoid acting on a mount it did not create, Byssus identifies mounts by
properties read from descriptors with `statx`, never by searching
`/proc/self/mountinfo` for a path (which races with stacked mounts, requires
unescaping, and is easy to misread).

A mount's identity consists of:

| Field | Source | Notes |
|-------|--------|-------|
| `mnt_id` | `statx` `STATX_MNT_ID` (5.8+) | Unique while mounted; the kernel may reuse it afterwards. |
| `mnt_id_unique` | `statx` `STATX_MNT_ID_UNIQUE` (6.8+) | Never reused. Recorded when the kernel supports it (probed at startup). |
| `root_dev` | `stx_dev_major`, `stx_dev_minor` of the mount root | Device of the exposed directory. |
| `root_ino` | `stx_ino` of the mount root | A bind mount's root has the same device and inode as its source directory. |

**Is something mounted at the target?** Open the target with `O_PATH` beneath
`target_root_fd` and `statx` it: `STATX_ATTR_MOUNT_ROOT` (5.8+) is set exactly
when the descriptor refers to the root of a mount — the top-most mount at that
path.

**Is it ours?** It matches a state record when:

- the record has `mnt_id_unique` and the kernel reports one: the unique IDs are
  equal, and `root_dev`/`root_ino` are equal; otherwise
- `mnt_id`, `root_dev` and `root_ino` are all equal.

**Has the source changed?** The member's source currently resolves to a
directory whose device and inode differ from `root_dev`/`root_ino` (for
example, the source directory was deleted and recreated).

`/proc/self/mountinfo` is used only for propagation checks (looked up by mount
ID, not path) and for human-readable output in `byssus status`.

## Unmounting

`umount2()` accepts only a path, and no kernel offers unmount-by-descriptor.
Byssus closes the resulting race by pinning the mount first:

1. `openat2(target_root_fd, target, O_PATH | O_DIRECTORY | O_CLOEXEC,
   RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS)` — the
   descriptor now references one specific mount.
2. Verify its identity against the state record.
3. `umount2("/proc/self/fd/<N>", MNT_DETACH)`. The kernel resolves the
   descriptor's magic link to exactly the mount that was verified; nothing can
   be substituted between check and use. `MNT_DETACH` performs a lazy unmount
   so busy files do not block removal.
4. Remove the state record and write the state file.
5. Remove the leaf target directory with `unlinkat(parent_fd, leaf,
   AT_REMOVEDIR)` if it is empty. Parents created for nested targets are left
   in place.

At startup `byssusd` opens `/proc` and verifies with `fstatfs` that it is a
genuine procfs (`PROC_SUPER_MAGIC`) before relying on `/proc/self/fd`.

## State file

`<state_dir>/state.json` records the mounts Byssus created. It is the
daemon's *ownership claim*; the kernel remains the authority on what exists.

```json
{
  "version": 1,
  "mounts": [
    {
      "group": "research",
      "name": "libcurl",
      "source_root": "/srv/example/projects",
      "source": "libcurl/workspace",
      "target_root": "/srv/example/groups/research/view",
      "target": "libcurl",
      "mnt_id": 4132,
      "mnt_id_unique": 2147487780,
      "root_dev_major": 8,
      "root_dev_minor": 1,
      "root_ino": 1842211,
      "read_only": true,
      "noexec": true,
      "nosymfollow": false,
      "created_at": "2026-09-12T14:30:01Z"
    }
  ]
}
```

`mnt_id_unique` is omitted when the kernel does not support it. `read_only`,
`noexec` and `nosymfollow` record the configurable attributes that were
applied.

**Three sources of truth:**

- **Membership files** — desired state: what should exist.
- **State file** — ownership claim: what Byssus created.
- **Kernel** (via `statx`) — reality: what exists.

**Writes** happen after every successful mount or unmount and on clean
shutdown. They are atomic: write `state.json.tmp` (mode `0640`), `fsync`,
`renameat` over `state.json`, `fsync` the directory.

**Permissions:** the state directory is `0750 byssus:byssus` and the state file
`0640`. Operators and monitoring tools that need `byssus status` join the
`byssus` group. The daemon sets its umask and file modes explicitly.

**Missing state file:** start with empty state. Any existing mounts at
configured targets are then foreign and reported as conflicts — the safe
default, requiring explicit operator action rather than guessing.

**Unreadable or corrupt state file:** rename it aside to
`state.json.corrupt-<timestamp>` (preserving it for the operator), log an
error, and proceed as if missing.

**Unknown `version`:** refuse to start. A newer format is never silently
overwritten by an older binary.

**Lock:** `byssusd` holds an exclusive `flock` on `<state_dir>/lock` for its
lifetime. `byssus reconcile` takes the same lock and exits with an explanation
if the daemon holds it, so two writers never race.

## Reconciliation

Each reconciliation pass covers every configured, non-degraded group (so
cross-group target collisions are always visible) and compares, for every
member name that is either desired or recorded:

- **M** — a valid membership file exists (and the group is in configuration);
- **S** — a state record exists for `(group, name)` whose recorded roots and
  resolved paths equal the currently desired ones;
- **T** — something is mounted at the target, and whether its identity
  **matches** the record;
- **Src** — whether the source resolves, and whether its device/inode equal the
  record's.

A state record whose recorded roots or resolved paths differ from the desired
ones (because a template or root changed) is treated as a record of a
*non-member* at the old location, and the member as having *no* record at the
new location. The old mount is removed before the new one is created.

| # | M | S | Mount at target | Identity | Source | Action |
|---|---|---|-----------------|----------|--------|--------|
| 1 | yes | no | no | — | resolves | **Create**; record identity. |
| 2 | yes | no | no | — | fails | Skip, `warn` (source missing or inaccessible). Retried on resync. |
| 3 | yes | no | yes | — | — | **Conflict** (foreign mount at target). Log, do not touch. |
| 4 | yes | yes | yes | match | same | Ours. If configuration *removed* a restriction the record shows was applied: **unmount, then create**. Otherwise, if configuration added a restriction or the mount lacks a required one: **add the missing restrictions** in place with `mount_setattr` and update the record. |
| 5 | yes | yes | yes | match | changed | Source replaced: **unmount, then create**. |
| 6 | yes | yes | yes | match | fails | Source gone: **unmount**, remove record, `warn`. |
| 7 | yes | yes | yes | mismatch | — | **Conflict** (target replaced). Log, do not touch; keep record. |
| 8 | yes | yes | no | — | resolves | Removed externally (reboot, manual unmount): **re-create**, update record. |
| 9 | yes | yes | no | — | fails | Remove record, `warn` (as #2). |
| 10 | no | yes | yes | match | — | Member removed: **unmount**, remove record. |
| 11 | no | yes | yes | mismatch | — | Remove stale record; foreign mount, do not touch. |
| 12 | no | yes | no | — | — | Remove stale record. |
| 13 | no | no | yes | — | — | Not ours; ignore. |
| 14 | no | no | no | — | — | Nothing to do. |

Additional rules:

- **Target collisions:** if several desired members (in any groups) resolve to
  the same target, a collision is reported. If one of them is already mounted
  there as a verified Byssus mount, it is kept; otherwise none is mounted.
  Members excluded by a collision are treated as non-members, so their mounts
  elsewhere are removed.
- **Relocation:** when a member's source or target location changes, its old
  mount is removed before the new one is created. If the old mount cannot be
  inspected, the move is blocked (and reported) rather than losing track of
  the old mount.
- **Freed targets:** a member may mount at a target currently occupied by a
  Byssus mount that the same pass removes; the mount waits for that unmount.
- **Dependencies:** a mount that depends on an unmount (source changed,
  relocation, freed target) is skipped if that unmount fails.
- **Degraded groups:** records of degraded groups are left untouched.
- **Ordering:** within a pass, unmounts run before mounts.
- **Failures are local:** an error on one member is logged and does not abort
  reconciliation of other members or groups.
- **Planning is pure:** the decision logic takes observations as input and
  returns a list of actions, so every row above is unit-tested without
  privileges. A separate executor performs the actions.

## Daemon lifecycle

### Startup

1. Parse arguments; initialize logging.
2. Parse and validate configuration syntax and ownership; exit 1 on error.
3. [Normalize privileges](#capability-normalization).
4. Open and verify `/proc`.
5. Probe kernel features (see [Kernel requirements](#kernel-requirements));
   exit 1 with a diagnostic if a required feature is missing.
6. Check configured paths as the service user; exit 1 on error.
7. Take the state lock; exit 1 if another instance holds it.
8. Open root and membership directory descriptors for every group.
9. Check propagation for every group's target root (see
   [Propagation](#propagation-and-mount-namespaces)).
10. Load the state file.
11. Install inotify watches, block the handled signals and create the
    `signalfd` — **before** the initial reconcile, so no membership change
    during startup is missed.
12. Full reconcile of every group, including cleanup of state records belonging
    to groups no longer configured.
13. Enter the event loop.

### Event loop

- **inotify readable:** drain all pending events. Membership changes schedule
  a reconciliation pass after a short quiet period (150 ms, never postponed by
  more than 1 s in total), so a burst of changes — including a membership
  directory being deleted file by file — is handled as a whole. Watched
  events: `IN_CREATE`, `IN_DELETE`, `IN_MOVED_FROM`, `IN_MOVED_TO`,
  `IN_CLOSE_WRITE`, `IN_MODIFY`, `IN_ATTRIB`, `IN_DELETE_SELF`,
  `IN_MOVE_SELF`. Reconciliation always re-reads the directory, so bursts
  converge regardless of event ordering.
- **`IN_Q_OVERFLOW`:** schedule a full reconcile.
- **Membership directory lost** — moved or unmounted (`IN_MOVE_SELF`,
  `IN_UNMOUNT`, `IN_IGNORED`), or deleted (detected before every scan by the
  directory descriptor's link count reaching zero, since the daemon's open
  descriptor keeps the inode alive and suppresses `IN_DELETE_SELF`): mark the
  group *degraded* — keep its existing mounts, log an error, and make no
  further changes to it until a successful reload re-opens the directory.
  Accidentally moving or deleting a membership directory must not
  mass-unmount a group; groups are removed through configuration.
- **Resync timeout:** full reconcile of every non-degraded group. The timer
  restarts after every pass.
- **`SIGHUP`:** transactional reload.
- **`SIGTERM` / `SIGINT`:** write the state file and exit 0. Mounts are **not**
  removed.

### Reload (`SIGHUP`)

1. Parse all configuration files.
2. Fully validate, including checking paths, opening every root and
   membership directory, checking propagation and creating new inotify
   watches. Changing `daemon.state_dir` is rejected (it requires a restart);
   a changed `daemon.user` is logged and takes effect on restart.
3. On any failure: log the error, discard the new configuration, keep running
   unchanged.
4. On success: atomically replace the active configuration, descriptors and
   watches, and clear degraded groups.
5. Reconcile every group. Groups that were removed have all their recorded
   mounts unmounted (the "not a member" rows); new groups are reconciled from
   scratch; groups with changed roots or templates move their mounts; groups
   with changed attributes have them re-applied in place.

### Shutdown and restarts

`byssusd` never unmounts on exit. Mount lifetime is governed by membership, not
by daemon liveness: consumers keep working across daemon restarts and upgrades,
and on restart reconciliation against the kernel and state file is a no-op.

## Propagation and mount namespaces

Consumers see views through **mount propagation**. The target root must lie on
a mount with `shared` propagation in the host's mount namespace; containers
bind-mount the view with `rslave` propagation, so each new Byssus mount appears
inside them, and nothing propagates back.

```
Host:       /srv/example/groups                 ← shared mount (propagation anchor)
            └── research/view                   ← target_root
                ├── libcurl                     ← Byssus bind mount
                └── openssl
Container:  /group                              ← bind of research/view, rslave
```

`shared`, `private` and `slave` are properties of mounts, not directories. If
the target root is an ordinary directory, make an anchor mount once at install
time:

```bash
mount --bind /srv/example/groups /srv/example/groups
mount --make-shared /srv/example/groups
```

(See [Deployment](#deployment) for making this persistent.)

### Startup check

For each group, `byssusd` takes the mount ID of `target_root_fd` from `statx`,
finds that mount in `/proc/self/mountinfo`, and inspects its optional fields:

| Propagation of target root's mount | Meaning | Behavior |
|-----------------------------------|---------|-----------|
| `shared:N` | Correct. | OK. |
| no propagation fields (`private`) | Mounts work on the host but will not reach containers. Valid for host-only use. | `warn`, continue. |
| `master:N` without `shared:N` (`slave`) | The daemon is almost certainly running in a **non-host mount namespace** — for example a systemd unit using `ProtectSystem=`, `PrivateTmp=` or `ReadWritePaths=`. Mounts would be invisible outside the daemon. | `error`, refuse to start unless `--allow-slave-namespace` is given. |

When the daemon can read `/proc/1/ns/mnt` (typically only when started as
root), it additionally compares its mount namespace with PID 1's and logs an
error if they differ.

### systemd sandboxing and mount namespaces

Many systemd hardening options silently place a service in a private mount
namespace with `slave` propagation, which traps every mount the daemon makes.
The unit shipped in `contrib/` therefore uses **none** of: `ProtectSystem=`,
`ProtectHome=`, `PrivateTmp=`, `PrivateDevices=`, `PrivateMounts=`,
`ReadWritePaths=`, `ReadOnlyPaths=`, `InaccessiblePaths=`, `ExecPaths=`,
`NoExecPaths=`, `TemporaryFileSystem=`, `BindPaths=`, `BindReadOnlyPaths=`,
`MountAPIVFS=`, `ProtectKernelTunables=`, `ProtectKernelModules=`,
`ProtectKernelLogs=`, `ProtectControlGroups=`, `ProtectProc=`, `ProcSubset=`,
`RootDirectory=`, `RootImage=`, `LogNamespace=`, `PrivateUsers=`,
`DynamicUser=`, `MountFlags=`. Hardening is achieved instead with capability
bounding, `NoNewPrivileges=`, locked `noroot` securebits, a system call filter
(`@system-service @mount`, minus `@privileged` and `@resources`, plus
`capset`), namespace, address-family, realtime, SUID and W^X restrictions,
private network and IPC namespaces (which do not affect mounts), and
`DevicePolicy=closed`.
`MountFlags=shared` is explicitly not used: it would propagate systemd's own
sandbox remounts back to the host.

## Kernel requirements

**Linux 5.12 or newer**, required for `mount_setattr()`, which makes read-only
attachment atomic. Other features used:

| Feature | Since | Use |
|---------|-------|-----|
| `open_tree`, `move_mount` | 5.2 | Clone and attach mounts via descriptors |
| `openat2` + `RESOLVE_*` | 5.6 | Confined path resolution |
| `statx` `STATX_MNT_ID`, `STATX_ATTR_MOUNT_ROOT` | 5.8 | Mount identity and detection |
| `MOUNT_ATTR_NOSYMFOLLOW` | 5.10 | Optional attribute |
| `mount_setattr` | 5.12 | Atomic mount attributes |
| `statx` `STATX_MNT_ID_UNIQUE` | 6.8 | Non-recycled mount IDs (optional; used when present) |

Startup probes call each syscall with deliberately invalid arguments and
distinguish `ENOSYS` (missing) from any other error (present). Some syscalls
check privileges before arguments, so `EPERM` also means present; a seccomp
filter returning `EPERM` is therefore indistinguishable at probe time and
surfaces as an error when the syscall is first used. System call numbers come from the
`libc`/`rustix` crates, so every Linux architecture those crates support is
supported.

## CLI

### `byssusd`

```
byssusd [OPTIONS]
  --config <FILE>          Main config file   [default: /etc/byssus/byssus.toml]
  --config-dir <DIR>       Drop-in directory  [default: /etc/byssus/conf.d]
  --log-level <LEVEL>      error|warn|info|debug|trace [default: info]
  --user <USER>            Service user to switch to when started as root
  --allow-root             Permit running as UID 0 (development/testing)
  --allow-slave-namespace  Permit a slave-only target root mount
```

Signals: `SIGHUP` reloads; `SIGTERM`/`SIGINT` shut down cleanly.

Exit status: `0` clean shutdown; `1` startup failure (configuration, kernel,
privileges, lock).

### `byssus`

```
byssus status    [--config <FILE>] [--config-dir <DIR>] [--format text|json]
byssus dry-run   [--config <FILE>] [--config-dir <DIR>] [--format text|json]
byssus reconcile [--config <FILE>] [--config-dir <DIR>] [--user <USER>] [--allow-root]
byssus version
```

- **`status`** — for each configured group and each state record: member,
  target, and whether its mount is present and matches (`ok`), missing,
  conflicting, or has changed source. If the state file cannot be read, it
  explains why (for example, "join the `byssus` group") and falls back to a
  view derived from the kernel alone, labeled *ownership unknown*.
- **`dry-run`** — a full validation pass without mounting: configuration,
  kernel features, `/proc`, propagation, membership files, per-member source
  resolution and permission diagnostics, and the actions a reconcile would
  take. Exit status `0` if no errors (warnings allowed), `1` otherwise.
- **`reconcile`** — one reconcile pass of every group, then exit. Requires
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
operation (`mount`, `unmount`, `reattr`, `skip`, `reject`, `conflict`),
trigger (`startup`, `inotify`, `resync`, `reload`, `cli`) and result.

```
ts=2026-09-12T14:30:01Z level=info op=mount group=research name=libcurl source=/srv/example/projects/libcurl/workspace target=/srv/example/groups/research/view/libcurl trigger=inotify result=ok
ts=2026-09-12T14:30:02Z level=warn op=reject group=research name=.hidden reason="name fails allowlist" trigger=inotify
ts=2026-09-12T14:30:03Z level=warn op=conflict group=research name=mystery target=/srv/example/groups/research/view/mystery reason="mount present at target but not recorded in state; not touching foreign mount"
```

Paths in log output are for humans only; they are never fed back into
syscalls.

## Deployment

Installation, permissions, the propagation anchor, the systemd unit, container
configuration and troubleshooting are covered in
[OPERATIONS.md](OPERATIONS.md). Deployment artifacts ship in `contrib/`:

- `contrib/systemd/byssusd.service` — hardened unit without mount namespacing;
- `contrib/systemd/srv-example-groups.mount` — example shared propagation
  anchor;
- `contrib/sysusers.d/byssus.conf` — service user.

Example configuration is in `examples/`.

## Security contract

- **No privileged action on behalf of the caller.** The application only
  creates and removes empty files. A separate long-running daemon does all
  privileged work.
- **Untrusted input is a single name**, validated against a strict allowlist
  and interpolated into root-owned templates.
- **Descriptor-based resolution.** All path operations use `openat2()` with
  `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS` relative to
  trusted root descriptors; target directories are created with `mkdirat()`
  beneath them. No string-constructed path reaches a privileged syscall; the
  one path-taking call, `umount2`, receives a `/proc/self/fd` link to a
  verified, pinned descriptor.
- **Strict membership files.** Only empty regular files with valid names
  count; everything else is rejected and logged.
- **No recursive submounts.** Nested mounts inside a source are never exposed.
- **Atomic restriction.** Attributes are applied to the detached clone before
  it is attached; a view is never writable, even briefly.
- **Root-owned configuration**, enforced by ownership and mode checks, reloaded
  only on `SIGHUP` and only if fully valid.
- **Ownership verification.** Byssus never unmounts or modifies a mount unless
  its kernel identity matches a record Byssus wrote. Foreign mounts are
  reported, never replaced.
- **Least privilege.** Only `CAP_SYS_ADMIN`; no DAC bypass; capabilities
  normalized by the binary itself; `no_new_privs`; dedicated user.
- **Membership directory permissions are part of the security boundary** and
  are the deployer's responsibility.
- **Memory safety.** Rust, with `unsafe` confined to a small, documented
  syscall layer (`mount_setattr`, `signalfd` and signal masking); everything
  else goes through `rustix`.
- **Static binaries** with no dynamic loader.

## Testing strategy

**Unit tests** (no privileges; run in CI):

- Configuration parsing, defaults, unknown keys, fragments and duplicate groups
- Ownership and mode checks on configuration files
- Name allowlist (accept, reject, boundaries)
- Template parsing and interpolation edge cases
- Membership file classification
- `/proc/self/mountinfo` parsing, including escaping and propagation fields
- State file serialization, versioning, corrupt-file handling, atomic write
- Every row of the reconciliation table, target collisions, template moves and
  removed groups
- Capability-normalization decision logic
- Transactional reload (old configuration retained on failure)
- `dry-run` and `status` formatting

**Integration tests** (need mount privileges; local-only for now, run by
`scripts/integration-tests.sh`):

The harness runs the test binary inside a fresh **user and mount namespace**
(`unshare --user --map-root-user --mount --propagation private`). Inside it the
tests hold `CAP_SYS_ADMIN` over their own mount namespace, so they exercise the
real syscalls without host root and cannot leak mounts onto the host; teardown
is automatic when the namespace exits. Tests that simulate a container create a
further child mount namespace with `rslave` propagation.

- End to end: create membership file → mount appears in a simulated container
  → remove file → mount disappears
- Startup reconcile with pre-existing state; with missing state (conflicts)
- Reload: add group, remove group, change template, change attributes, invalid
  new configuration retained old
- Rejection of symlinks, non-empty files, directories, FIFOs, bad names
- Source removed after mount → unmount; source recreated → remount
- Rapid create/delete convergence
- **No recursive submounts** exposed
- Foreign mount at target → conflict, untouched
- **Mount replaced** at target (identity mismatch) → conflict
- Daemon restart → no-op reconcile
- Propagation warning (private) and refusal (slave)
- Read-only, nosuid, nodev, noexec actually enforced

Tests requiring real host root (switching to a real service user) are
additionally gated and run with `sudo`.

## Limitations

- **Linux 5.12+ only.** Docker Desktop on macOS and Windows runs containers in
  a VM with its own mount namespace; propagation from the host does not reach
  them.
- **Host reboots clear bind mounts.** The daemon recreates them at startup;
  order it before the container runtime.
- **`CAP_SYS_ADMIN` is broad.** Mitigated by a small, audited code surface,
  capability normalization, `no_new_privs`, no DAC bypass, a syscall filter,
  static linking and optional AppArmor confinement.
- **Mount ID reuse on kernels before 6.8.** Identity additionally requires
  matching device and inode, which makes accidental matches implausible but
  not impossible (for example, a manual re-bind of the same source to the same
  target that happens to receive a recycled mount ID).
- **Large groups.** Hundreds of mounts in one view are fine for the kernel but
  make recursive listings noisy for consumers.

## Future work

- A daemon-written status snapshot (`/run/byssus/status.json`) with conflicts,
  rejections and last-reconcile time, if a use case emerges.
- Selective exposure of subdirectories of a source.
- JSON log output.
- Running the integration suite in CI.
- Seccomp filter applied by the daemon itself, in addition to systemd's.

## Design decisions log

Decisions that refined the original design draft, with rationale:

1. **No mount-namespace sandboxing in the systemd unit.** Namespace-creating
   options trap mounts in a slave namespace. Hardening uses non-namespace
   options, AppArmor, and a runtime self-check that refuses slave-only
   propagation.
2. **No DAC-bypassing capabilities.** The daemon runs as an unprivileged user
   with only `CAP_SYS_ADMIN`; deployers grant search-only ACLs on sources.
   A compromise cannot read arbitrary host files.
3. **Unmount through a pinned, verified descriptor** via `/proc/self/fd`,
   eliminating the check-then-unmount race.
4. **Identity via `statx`** (`mnt_id`, `mnt_id_unique` where available, root
   device and inode) instead of path lookups in `mountinfo`.
5. **Capability normalization in the binary**, independent of how it was
   launched, including refusing UID 0 without a configured service user.
6. **Group-readable state** (`0640`) rather than world-readable; `status` falls
   back gracefully when it cannot read it.
7. **TOML configuration** instead of YAML.
8. **`rustix` for syscalls**, with raw `libc` only where `rustix` has no
   wrapper.
9. **Membership directories are not created by the daemon.** The daemon has no
   write access to them; a missing directory is a configuration error.
10. **Degraded groups on membership directory loss** rather than
    mass-unmounting.
11. **Periodic resync** so sources that appear after their membership file,
    and mounts removed out-of-band, converge without an event.
12. **Names may not begin with `.`**, excluding hidden and temporary files.
