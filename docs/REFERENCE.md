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

Membership entries whose names begin with `.` are ignored silently (see
[Membership files](DESIGN.md#membership-files)); any other name that fails these rules
is rejected.

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
  conflicting, or has changed source; plus rejected membership entries with
  their reasons and the number of ignored hidden entries. Exits 1 only for
  error states (conflicts, unavailable targets or groups); rejected entries
  and unavailable sources are warnings. If the state file cannot be read, it
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
ts=2026-09-12T14:30:02Z level=warn op=reject group=research name=bad:name reason="name fails allowlist: name contains disallowed byte 0x3a at offset 3" trigger=inotify
ts=2026-09-12T14:30:03Z level=warn op=conflict group=research name=mystery target=/srv/example/groups/research/view/mystery reason="mount present at target but not recorded in state; not touching foreign mount"
```

Paths in log output are for humans only; they are never fed back into
syscalls.
