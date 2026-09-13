# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `byssusd` daemon: watches membership directories with inotify and
  reconciles read-only bind mounts; debounced change handling, periodic
  resync, transactional `SIGHUP` reload, clean shutdown that preserves mounts,
  and degraded handling of lost membership directories.
- `byssus` CLI: `status`, `dry-run`, `check`, `reconcile` and `version`, with
  text and JSON output. `check` validates candidate drop-in fragments exactly
  as a reload would, before they are installed.
- Descriptor-confined path handling (`openat2` with `RESOLVE_BENEATH`,
  `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS`), non-recursive clones with
  restrictions applied before attachment, mount identity verification via
  `statx`, and unmounting through pinned descriptors.
- Privilege normalization to exactly `CAP_SYS_ADMIN`, with optional switch to
  a service user, locked securebits and `no_new_privs`.
- TOML configuration with drop-in fragments and ownership checks; versioned,
  atomically written state file.
- Hardened systemd unit, propagation anchor example, sysusers configuration,
  example configuration, operations guide and design document.
- Namespace-isolated integration test suite (`scripts/integration-tests.sh`).
- CI (format, lints, tests, docs, MSRV, static builds with smoke tests,
  cargo-deny, packaging) and a release workflow with provenance attestation.
