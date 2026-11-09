# Releasing flashcp

## Prerequisites

- Push access to the repository.
- `main` is green in CI.

## Steps

1. On a branch, bump `version` in `Cargo.toml`, then run `cargo check` to refresh `Cargo.lock`.
2. Commit as `Release vX.Y.Z` and open a PR to `main`. Merge it once CI passes.
3. Optional: dry run the release workflow on `main`. This builds, smoke-tests and packages everything but publishes nothing.

   ```sh
   gh workflow run release.yml --ref main -f dry_run=true
   gh run watch
   ```

4. Tag the merged commit on `main` and push the tag:

   ```sh
   git switch main && git pull
   git tag -a vX.Y.Z -m "flashcp vX.Y.Z"
   git push origin vX.Y.Z
   ```

The tag push starts the Release workflow. It checks that the tag matches the Cargo version, builds static musl binaries for x86_64 and aarch64, smoke-tests each one (builds an exFAT image and runs `fsck.exfat`), installs the `.deb` and checks it, then publishes the GitHub release with the assets and `SHA256SUMS`.

## Verify

```sh
gh release view vX.Y.Z
```

The release should contain these assets:

- `flashcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz`
- `flashcp-X.Y.Z-aarch64-unknown-linux-musl.tar.gz`
- `flashcp_X.Y.Z_amd64.deb`
- `flashcp_X.Y.Z_arm64.deb`
- `SHA256SUMS`

Check the checksums:

```sh
gh release download vX.Y.Z -D /tmp/rel && cd /tmp/rel && sha256sum -c SHA256SUMS
```

## If something fails

Fix the problem on `main` through a PR. Then remove the tag and any draft or partial release, and tag again:

```sh
git push --delete origin vX.Y.Z
git tag -d vX.Y.Z
gh release delete vX.Y.Z --yes   # only if a release or draft exists
git tag -a vX.Y.Z -m "flashcp vX.Y.Z"
git push origin vX.Y.Z
```
