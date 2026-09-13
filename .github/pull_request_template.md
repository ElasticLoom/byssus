## Summary

<!-- What does this change and why? Link related issues. -->

## Checklist

- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] `scripts/integration-tests.sh` passes (for changes touching mounts, privileges, the daemon or the CLI)
- [ ] `docs/DESIGN.md` updated for behavior or security-contract changes
- [ ] `docs/TODO.md` updated for anything left incomplete or deferred
- [ ] `CHANGELOG.md` updated under *Unreleased* for user-visible changes
- [ ] No secrets, private hostnames or private paths in code, tests or examples
