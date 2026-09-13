# Byssus Design

> *Byssus creates live filesystem attachments between isolated workspaces.*

This document is the authoritative design and security contract for Byssus.
Implementation progress is tracked in [TODO.md](TODO.md). Where the code and
this document disagree, one of them has a bug — please open an issue.

Related documents: [REFERENCE.md](REFERENCE.md) (configuration, CLI, logs),
[DECISIONS.md](DECISIONS.md) (why things are the way they are),
[OPERATIONS.md](OPERATIONS.md) and [INTEGRATION.md](INTEGRATION.md).

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

Configuration is TOML: a main file plus drop-in fragments, root-owned and
validated all-or-nothing. Every field and validation rule is documented in
[REFERENCE.md](REFERENCE.md#configuration).

## Names and templates

Member names come from untrusted membership file names and are interpolated
into root-owned path templates. The exact rules are in
[REFERENCE.md](REFERENCE.md#names-and-templates); the security property they
guarantee is that interpolation can never introduce new path components or
traversal.

## Membership files

During reconciliation `byssusd` lists the membership directory through a
directory descriptor and inspects each entry with
`statx(dirfd, name, AT_SYMLINK_NOFOLLOW)`. An entry makes its name a member
only if:

- the name satisfies the [name rules](REFERENCE.md#names);
- it is a regular file (symlinks, directories, FIFOs, sockets and devices are
  rejected);
- its size is 0.

**Hidden entries** (names beginning with `.`) are ignored without a warning
and logged only at `debug` (`op=ignore`). This lets applications create a file
under a hidden name and `rename(2)` it into place, and keeps tool artifacts
such as `.gitkeep`, editor swap files and NFS or rsync temporary files from
producing noise. `byssus status` and `byssus dry-run` show how many hidden
entries each group has.

Every **other** entry that is not a member is *rejected*: logged at `warn`
with `op=reject` and a reason, and listed by name with its reason in
`byssus status` and `byssus dry-run`. The daemon logs a rejection once, when
it first appears or its reason changes, and logs `op=reject_cleared` when the
entry is fixed or removed — not on every reconciliation pass. The same applies
to a membership directory that cannot be read.

A previously valid membership file that becomes invalid (for example, data is
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

   Attributes are also never changed on an existing mount. Mount attributes
   do not propagate: each consumer holds its own copy of a propagated mount,
   and `mount_setattr` on the host's mount leaves those copies unchanged. A
   change of configured attributes, or a mount found not to enforce them, is
   therefore applied by unmounting and creating the mount again — both of
   which do propagate.
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
| 4 | yes | yes | yes | match | same | Ours. If the configured attributes differ from those recorded, or the mount does not enforce a configured restriction: **unmount, then create**, so the change reaches every consumer. |
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
13. Notify the service manager that the daemon is ready (`READY=1` over
    `NOTIFY_SOCKET` when started with `Type=notify`), so units ordered after
    `byssusd` start only once views are populated.
14. Enter the event loop.

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
- **`SIGHUP`:** transactional reload, bracketed by `RELOADING=1` and
  `READY=1` notifications; the reload's reconcile pass runs before `READY=1`.
  The systemd status line (`STATUS=`) reports group and mount counts,
  degraded groups and the most recent rejected reload.
- **`SIGTERM` / `SIGINT`:** notify `STOPPING=1`, write the state file and exit 0. Mounts are **not**
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
   with changed attributes have their mounts re-created.

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

(See [OPERATIONS.md](OPERATIONS.md#create-the-propagation-anchor) for making this persistent.)

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
`capset`), namespace, address-family, realtime and W^X restrictions,
private network and IPC namespaces (which do not affect mounts), and
`DevicePolicy=closed`. `RestrictSUIDSGID=` is not used: systemd cannot filter
`openat2()`'s mode argument and therefore blocks the syscall entirely, which
would disable every confined path lookup.
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

Command-line options, commands and exit statuses for `byssusd` and `byssus`
are documented in [REFERENCE.md](REFERENCE.md#cli).

## Logging

Logs are structured `key=value` lines on stderr; every mount-affecting
operation is logged with its group, member, paths, trigger and result. The
format and every `op=` value are documented in
[REFERENCE.md](REFERENCE.md#logging).

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

**Integration tests** (run by `scripts/integration-tests.sh`, locally and in
CI; no host root required):

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

Service-user switching tests map subordinate UIDs into the namespace
(`unshare --map-auto`); CI requires them, local runs skip them with a warning
if `/etc/subuid` is not configured.

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
- Seccomp filter applied by the daemon itself, in addition to systemd's.
