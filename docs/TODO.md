# Byssus TODO and Milestones

This file tracks all planned work. **Every known gap, stub, shortcut or
deferred item must be listed here** so nothing is silently forgotten. When code
contains a deliberate gap, reference the item here from a code comment.

Status: `[ ]` not started · `[~]` in progress · `[x]` done

See [DESIGN.md](DESIGN.md) for the specification each item implements.

---

## M0 — Repository foundation

- [x] Cargo package with `byssusd` and `byssus` binaries, library crate
- [x] crates.io-ready metadata (license, repository, keywords, categories, `include`)
- [x] Apache-2.0 `LICENSE`
- [x] `README.md`, `SECURITY.md`, `CONTRIBUTING.md`, `CHANGELOG.md`
- [x] Public design document (`docs/DESIGN.md`)
- [x] Lint configuration (rustc + clippy pedantic, unsafe hygiene lints)
- [x] `rustfmt.toml`, `.gitignore`, `.editorconfig`
- [x] `cargo-deny` configuration (licenses, advisories, bans, sources)
- [x] GitHub Actions CI: fmt, clippy, tests, integration tests, docs, MSRV, musl builds (x86_64 + aarch64), cargo-deny
- [x] Dependabot for Cargo and GitHub Actions
- [x] `cargo publish --dry-run` packaging check in CI
- [x] Cross-linking of static musl binaries with `rust-lld` (`.cargo/config.toml`)
- [ ] Enable GitHub private vulnerability reporting (repository settings; manual)
- [ ] Branch protection on `main` requiring CI (repository settings; manual)
- [ ] `CODE_OF_CONDUCT.md` (decide on text, e.g. Contributor Covenant)
- [x] Issue and pull request templates
- [x] `scripts/playground.sh`: interactive namespace playground

## M1 — Unprivileged core (pure logic, fully unit-tested)

- [x] `name`: name allowlist validation
- [x] `template`: template parsing, validation and interpolation
- [x] `config`: TOML schema, defaults, `deny_unknown_fields`
- [x] `config`: main file + `conf.d` fragments, ordering, duplicate group detection, `[daemon]` only in main file
- [x] `config`: path validation (absolute, no `.`/`..`), membership-not-beneath-target check
- [x] `config`: ownership/mode checks on files and containing directories (error for daemon/reconcile, warning for status/dry-run)
- [x] `mountinfo`: parser (escaping, optional fields, propagation classification, lookup by mount ID)
- [x] `state`: schema v1, serde, version check, missing/corrupt handling (rename aside)
- [x] `state`: atomic write (tmp + fsync + renameat + dir fsync), explicit modes
- [x] `identity`: mount identity matching (unique ID authoritative, fallback to reusable ID + root dev/ino)
- [x] `membership`: entry classification (name, file type, size) with reject reasons
- [x] `reconcile::plan`: pure planner covering all 14 decision-table rows
- [x] `reconcile::plan`: template/root moves, removed groups, target collisions, unmount-before-mount ordering
- [x] `privileges::plan`: pure capability-normalization decision logic

## M2 — Kernel layer

- [x] `sys`: `mount_setattr` raw syscall wrapper (add-only)
- [x] `sys`: signal masking + `signalfd` wrapper
- [x] `probe`: kernel feature probes (`open_tree`, `move_mount`, `openat2`, `mount_setattr`, `statx` mount fields, `STATX_MNT_ID_UNIQUE`); only `ENOSYS` means missing
- [x] `probe`: `/proc` verification via `fstatfs`
- [x] `probe`: propagation check via `statx` mount ID + mountinfo; private → warn, slave-only → error
- [x] `probe`: best-effort `/proc/1/ns/mnt` comparison
- [x] `fsops`: confined `openat2` resolution helpers
- [x] `fsops`: target directory creation walk (`mkdirat` + `openat2`)
- [x] `fsops`: membership directory listing via descriptor + `statx(AT_SYMLINK_NOFOLLOW)`
- [x] `mount`: identity read (`statx` mnt_id, mnt_id_unique, dev, ino, `STATX_ATTR_MOUNT_ROOT`)
- [x] `mount`: create (`open_tree` clone, `mount_setattr`, `move_mount`, identity)
- [x] `mount`: attribute observation (`fstatvfs`) and verified in-place addition of restrictions
- [x] `mount`: unmount via pinned, verified descriptor + `/proc/self/fd`
- [x] `fsops`: empty leaf target directory removal
- [x] `privileges`: apply normalization plan (capget/capset, bounding set, securebits, user switch, ambient clear, `no_new_privs`) and verify the result
- [x] `privileges`: resolve service user from `/etc/passwd` and `/etc/group` (static musl has no NSS)
- [x] `config`: expose path checks separately so they run after privilege normalization (startup step order in DESIGN.md)
- [x] `lock`: state directory `flock`
- [x] `state`: state I/O through a state-directory descriptor (`openat`/`renameat`/`fsync`)
- [x] Integration test harness: `scripts/integration-tests.sh` running tests in `unshare --user --map-root-user --mount`
- [x] Integration tests for each kernel-layer operation (including no-recursive-submount and attribute enforcement)

- [x] Integration test for service-user switching (via `--map-auto` subordinate UIDs)

## M3 — Reconciler and CLI

- [x] `reconcile::observe`: gather membership, state and kernel observations for all groups
- [ ] `reconcile::observe`: alias-aware target collision detection (compare target roots by device/inode, not only by configured path string)
- [x] `reconcile::execute`: apply planned actions, per-member error isolation, state writes, rollback of unrecordable mounts
- [ ] Unit tests for the executor's state bookkeeping with a fake kernel backend (currently covered only by namespace integration tests)
- [x] Structured logfmt logging (journal detection, all required fields)
- [x] `byssus version`
- [x] `byssus status` (text + JSON; permission-error fallback to kernel-only view)
- [x] `byssus dry-run` (full validation report, exit codes; switch to service user when root)
- [x] `byssus reconcile` (privilege normalization, lock, one pass)
- [x] Integration tests: end-to-end reconcile, conflicts, identity mismatch, source removed/recreated, rejects

## M4 — Daemon

- [x] `byssusd` argument parsing
- [x] Startup sequence (config → privileges → probes/procfs → paths → lock/state → descriptors → propagation → signals/watches → reconcile)
- [x] `epoll` event loop: inotify, signalfd, resync timeout
- [x] inotify event handling: drain, debounce into one pass, overflow → full reconcile
- [x] Degraded groups on membership directory deletion/move
- [x] Transactional `SIGHUP` reload
- [x] Clean shutdown on `SIGTERM`/`SIGINT` (state write, no unmount)
- [x] Integration tests: live membership changes, propagation into simulated container, reload scenarios, restart no-op, rapid churn

- [ ] systemd readiness notification (`Type=notify`, `sd_notify` READY/RELOADING/STOPPING) so dependent units start after the initial reconcile
- [ ] Automatic recovery of degraded groups when the membership directory reappears (currently requires SIGHUP)

## M5 — Deployment artifacts and documentation

- [x] `contrib/systemd/byssusd.service` (no namespace-creating options; each option checked against systemd 255 `systemd.exec(5)`; `systemd-analyze security` exposure 2.6; seccomp filter exercised by running the unit tests under it via `systemd-run --user`)
- [ ] Validate the unit end to end on a real systemd host as root (start, mount, reload, stop)
- [x] Verify `StateDirectory=` does not create a mount namespace; use it if safe
- [x] Persistent shared anchor mount example (systemd `.mount` unit with `bind,shared`; fstab equivalent documented)
- [x] `contrib/sysusers.d/byssus.conf`
- [ ] `contrib/byssus.apparmor` profile — deferred until it can be written and validated against the new mount API (`open_tree`/`move_mount`/`mount_setattr`) on a root-capable AppArmor host; an unvalidated profile risks breaking the daemon
- [x] `docs/OPERATIONS.md`: install, users, ACL recipe, anchor mount, container configuration (`rslave`), upgrades, troubleshooting
- [x] Example configuration in `examples/` (validated by a unit test)
- [x] `--help` completeness review
- [ ] Man pages (e.g. generated from the clap definitions)

## M6 — Release engineering and publication

- [x] Release workflow: tagged builds of static musl binaries (x86_64, aarch64), version/tag check, smoke tests, archives with docs and contrib, SHA256SUMS, draft GitHub release
- [ ] Exercise the release workflow once (e.g. with a pre-release tag) after the repository is public
- [x] Build provenance / artifact attestation for release archives
- [x] Smoke-test binaries in CI (aarch64 under qemu-user)
- [x] Service-user switching tests using subordinate UIDs in the test namespace (`service_user::`), no host root required
- [x] Run integration tests in CI (user namespaces on GitHub runners; subordinate IDs required)
- [ ] Confirm the CI integration job passes on GitHub's runners (first push)
- [ ] Security review of the full codebase before first release
- [x] Document the release process in `RELEASING.md`
- [ ] Make repository public; publish 0.1.0 to crates.io

## Deferred / future (not scheduled)

- [ ] Daemon-written status snapshot `/run/byssus/status.json` (only if a use case appears; would also let integrations confirm that the latest reload was accepted without reading logs)
- [ ] JSON log format
- [ ] Selective exposure of source subdirectories
- [ ] Daemon-applied seccomp filter
