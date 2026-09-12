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
- [x] GitHub Actions CI: fmt, clippy, tests, docs, MSRV, musl builds (x86_64 + aarch64), cargo-deny
- [x] Dependabot for Cargo and GitHub Actions
- [x] `cargo publish --dry-run` packaging check in CI
- [x] Cross-linking of static musl binaries with `rust-lld` (`.cargo/config.toml`)
- [ ] Enable GitHub private vulnerability reporting (repository settings; manual)
- [ ] Branch protection on `main` requiring CI (repository settings; manual)
- [ ] `CODE_OF_CONDUCT.md` (decide on text, e.g. Contributor Covenant)
- [ ] Issue and pull request templates

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
- [ ] `state`: move state I/O onto a state-directory descriptor (`openat`/`renameat`) once the kernel layer exists; it currently uses path-based `std::fs` within the trusted, `byssus`-owned state directory
- [ ] `membership`: entry classification (name, file type, size) with reject reasons
- [ ] `reconcile::plan`: pure planner covering all 14 decision-table rows
- [ ] `reconcile::plan`: template/root moves, removed groups, target collisions, unmount-before-mount ordering
- [ ] `privileges::plan`: pure capability-normalization decision logic

## M2 — Kernel layer

- [ ] `sys`: `mount_setattr` raw syscall wrapper (the only mount-related `unsafe`)
- [ ] `sys`: signal masking + `signalfd` wrapper
- [ ] `probe`: kernel feature probes (`open_tree`, `move_mount`, `openat2`, `mount_setattr`, `statx` mount fields, `STATX_MNT_ID_UNIQUE`), `ENOSYS` vs `EPERM` reporting
- [ ] `probe`: `/proc` verification via `fstatfs`
- [ ] `probe`: propagation check via `statx` mount ID + mountinfo; private → warn, slave-only → error
- [ ] `probe`: best-effort `/proc/1/ns/mnt` comparison
- [ ] `fsops`: confined `openat2` resolution helpers
- [ ] `fsops`: target directory creation walk (`mkdirat` + `openat2`)
- [ ] `fsops`: membership directory listing via descriptor + `statx(AT_SYMLINK_NOFOLLOW)`
- [ ] `mount`: identity read (`statx` mnt_id, mnt_id_unique, dev, ino, `STATX_ATTR_MOUNT_ROOT`)
- [ ] `mount`: create (`open_tree` clone, `mount_setattr`, `move_mount`, identity)
- [ ] `mount`: attribute verification (`fstatvfs`) and in-place re-apply
- [ ] `mount`: unmount via pinned descriptor + `/proc/self/fd`, leaf directory removal
- [ ] `privileges`: apply normalization (capget/capset, user switch with keepcaps, bounding set, securebits, ambient clear, `no_new_privs`)
- [ ] `lock`: state directory `flock`
- [ ] Integration test harness: `scripts/integration-tests.sh` running tests in `unshare --user --map-root-user --mount`
- [ ] Integration tests for each kernel-layer operation (including no-recursive-submount and attribute enforcement)

## M3 — Reconciler and CLI

- [ ] `reconcile::observe`: gather membership, state and kernel observations per group
- [ ] `reconcile::execute`: apply planned actions, per-member error isolation, state writes
- [ ] Structured logfmt logging (journal detection, all required fields)
- [ ] `byssus version`
- [ ] `byssus status` (text + JSON; permission-error fallback to kernel-only view)
- [ ] `byssus dry-run` (full validation report, exit codes; switch to service user when root)
- [ ] `byssus reconcile` (privilege normalization, lock, one pass)
- [ ] Integration tests: end-to-end reconcile, conflicts, identity mismatch, source removed/recreated, rejects

## M4 — Daemon

- [ ] `byssusd` argument parsing
- [ ] Startup sequence (privileges → /proc → probes → config → lock → descriptors → propagation → state → watches/signals → reconcile)
- [ ] `epoll` event loop: inotify, signalfd, resync timeout
- [ ] inotify event handling: drain, per-group coalescing, overflow → full reconcile
- [ ] Degraded groups on membership directory deletion/move
- [ ] Transactional `SIGHUP` reload
- [ ] Clean shutdown on `SIGTERM`/`SIGINT` (state write, no unmount)
- [ ] Integration tests: live membership changes, propagation into simulated container, reload scenarios, restart no-op, rapid churn

## M5 — Deployment artifacts and documentation

- [ ] `contrib/byssusd.service` (no namespace-creating options; verify every option on current systemd)
- [ ] Verify `StateDirectory=` does not create a mount namespace; use it if safe
- [ ] Persistent shared anchor mount example (systemd `.mount` unit + `make-shared`)
- [ ] `contrib/byssus.apparmor` profile
- [ ] `docs/OPERATIONS.md`: install, users, ACL recipe, anchor mount, container configuration (`rslave`), upgrades, troubleshooting
- [ ] Example configuration in `examples/`
- [ ] Man pages or `--help` completeness review

## M6 — Release engineering and publication

- [ ] Release workflow: tagged builds of static musl binaries (x86_64, aarch64), checksums, GitHub release
- [ ] Build provenance / artifact attestation for release binaries
- [ ] Smoke-test the aarch64 binary (e.g. under qemu-user) in CI
- [ ] Real-root test tier (`sudo`) for service-user switching
- [ ] Decide on and run integration tests in CI (userns on GitHub runners)
- [ ] Security review of the full codebase before first release
- [ ] Make repository public; publish 0.1.0 to crates.io

## Deferred / future (not scheduled)

- [ ] Daemon-written status snapshot `/run/byssus/status.json` (only if a use case appears)
- [ ] JSON log format
- [ ] Selective exposure of source subdirectories
- [ ] Daemon-applied seccomp filter
