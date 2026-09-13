# Releasing Byssus

Releases are cut from `main` by a maintainer.

1. **Prepare**
   - Ensure CI is green on `main`.
   - Run the privileged integration tests locally:
     `scripts/integration-tests.sh`.
   - Update `version` in `Cargo.toml` (and `Cargo.lock` via `cargo check`).
   - Move the *Unreleased* entries in `CHANGELOG.md` under a new version
     heading with today's date.
   - Open a pull request with these changes and merge it.

2. **Tag**

   ```bash
   git switch main && git pull
   git tag -s vX.Y.Z -m "Byssus vX.Y.Z"
   git push origin vX.Y.Z
   ```

   The `Release` workflow checks that the tag matches the crate version,
   builds static `x86_64` and `aarch64` musl binaries, smoke-tests them,
   packages them as archives (with the documentation and `contrib/` files) and
   as `.deb` and `.rpm` packages, tests the `x86_64` packages in Ubuntu and
   Fedora containers and end to end on the runner's systemd, generates
   `SHA256SUMS`, attests build provenance and creates a **draft** GitHub
   release.

3. **Review and publish**
   - Review the draft release notes and artifacts, then publish the release.
   - Publish the crate:

     ```bash
     cargo publish --locked
     ```

4. **Verify**
   - `gh attestation verify byssus-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo ElasticLoom/byssus`
   - `sha256sum -c SHA256SUMS`
