# Contributing to Byssus

Thanks for your interest in Byssus! Contributions of all kinds are welcome.

## Before you start

- Read [docs/DESIGN.md](docs/DESIGN.md). Byssus is security-sensitive; changes
  must preserve the [security contract](docs/DESIGN.md#security-contract).
- Check [docs/TODO.md](docs/TODO.md) and open issues to avoid duplicate work.
- For anything beyond a small fix, open an issue to discuss the approach first.
- Report security issues privately — see [SECURITY.md](SECURITY.md).

## Development

Requirements: Linux, Rust (latest stable; the minimum supported version is set
by `rust-version` in `Cargo.toml`).

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Static release builds:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

### Integration tests

Integration tests exercise real mount syscalls. They are not run by plain
`cargo test`. The harness runs them inside a throwaway user and mount namespace,
so they need no host root and cannot leave mounts behind:

```bash
scripts/integration-tests.sh
```

Extra arguments are passed to the test binary, for example
`scripts/integration-tests.sh --nocapture kernel::`. If unprivileged user
namespaces are disabled (for example by AppArmor on Ubuntu 24.04), the script
explains how to enable them for a local run.

### Playground

To try Byssus by hand, start an interactive shell with a sample deployment, a
simulated container and `byssusd` running — no root needed, and everything is
torn down on exit:

```bash
scripts/playground.sh
```

Type `help` inside for the layout, helper commands and things to try.

## Guidelines

- **No silent gaps.** If a change leaves something incomplete, stubbed or
  deferred, add an entry to `docs/TODO.md` and reference it from a code comment.
- **Keep `unsafe` minimal.** Prefer `rustix`. Every `unsafe` block needs a
  `// SAFETY:` comment; clippy enforces this.
- **Keep the design document current.** Behavior changes update
  `docs/DESIGN.md` in the same pull request.
- **Test the decision logic without privileges.** Keep planning pure and cover
  it with unit tests; use integration tests for syscall behavior.
- **Dependencies** must be actively maintained, use their latest versions, and
  pass `cargo deny check`.
- Update `CHANGELOG.md` under *Unreleased* for user-visible changes.
- Do not include private information, credentials, or internal hostnames in
  code, tests, examples, or commit messages.

## License

By contributing, you agree that your contributions will be licensed under the
[Apache License, Version 2.0](LICENSE).
