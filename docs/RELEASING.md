# Releasing Byssus

Pushing a version tag releases Byssus: the `Release` workflow builds and tests
everything, publishes the GitHub release and publishes the crate to crates.io.

1. **Prepare**
   - Update `version` in `Cargo.toml` (and `Cargo.lock` via `cargo check`).
   - Regenerate the man pages, which show the version:
     `BYSSUS_UPDATE_MAN=1 cargo test --lib cli::`.
   - Move the *Unreleased* entries in `CHANGELOG.md` under a new version
     heading with today's date, and update the links at the bottom.
   - Run the privileged integration tests locally:
     `scripts/integration-tests.sh`.
   - Commit, push to `main` and wait for CI to pass.

2. **Tag**

   ```bash
   git tag -s vX.Y.Z -m "Byssus vX.Y.Z"
   git push origin vX.Y.Z
   ```

   The `Release` workflow checks that the tag matches the crate version,
   builds static `x86_64` and `aarch64` musl binaries, smoke-tests them,
   packages them as archives and as `.deb` and `.rpm` packages, tests the
   `x86_64` packages in containers and on the runner's systemd, generates
   `SHA256SUMS`, attests build provenance, publishes the GitHub release and
   then publishes the crate.

   If the workflow fails before the GitHub release is created, nothing has
   been published: delete the tag (`git push --delete origin vX.Y.Z` and
   `git tag -d vX.Y.Z`), fix the problem and tag again. If only the crates.io
   step fails, re-run that job.

3. **Verify**
   - `gh attestation verify byssus-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo ElasticLoom/byssus`
   - `sha256sum -c SHA256SUMS`
   - The new version appears on <https://crates.io/crates/byssus>.

## One-time setup

The crates.io step uses
[trusted publishing](https://crates.io/docs/trusted-publishing), so no
registry token is stored anywhere:

- On crates.io, the `byssus` crate's trusted publisher is the GitHub
  repository `ElasticLoom/byssus`, workflow `release.yml`, environment
  `release`.
- In the repository settings, the `release` environment only allows
  deployments from tags matching `v*`.
