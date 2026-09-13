# Design Decisions

Decisions that refined the original design draft or were made during
implementation, with rationale. The current behavior is specified in
[DESIGN.md](DESIGN.md) and [REFERENCE.md](REFERENCE.md).

Decisions that refined the original design draft, with rationale:

1. **No mount-namespace sandboxing in the systemd unit.** Namespace-creating
   options trap mounts in a slave namespace. Hardening uses non-namespace
   options, AppArmor, and a runtime self-check that refuses slave
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
12. **Hidden membership entries are ignored, not rejected.** Names beginning
    with `.` are skipped with a debug log only, supporting write-then-rename
    and avoiding noise from tool artifacts.
13. **Rejections are logged once** (and again only when their reason changes,
    plus once when cleared) rather than on every pass, and are visible on
    demand through `status` and `dry-run`.
14. **Mount attributes are never changed in place.** Mount attributes do not
    propagate, so `mount_setattr` on the host's mount would not reach copies
    already propagated into running containers (verified by an integration
    test). Any change of configured attributes, or drift, re-creates the
    mount, which does propagate. Attributes are also only ever set on a new
    clone, never cleared, so a view is never more permissive than its source.
15. **Read-only is enforced by Byssus, not by the consumer's bind.** A
    read-only bind of the view does not apply to member mounts beneath it, so
    the group's `read_only` setting is the only control over member
    writability, and read-write groups are documented as trusting every
    consumer with every member's files.
