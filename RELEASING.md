# Releasing flashcp

## Prerequisites

- Push access to the repository.
- `main` is green in CI.

## Steps

1. On a branch, bump `version` in `Cargo.toml`, then run `cargo check` to refresh `Cargo.lock`.
   Bump the GUI to the same version too: `version` in `gui/src-tauri/tauri.conf.json`, `gui/src-tauri/Cargo.toml` and `gui/helper/Cargo.toml`, then run `cargo check --manifest-path gui/Cargo.toml` to refresh `gui/Cargo.lock`. The release fails if any of them differs from `Cargo.toml`.
2. Commit as `Release vX.Y.Z` and open a PR to `main`. Merge it once CI passes.
3. Optional: dry run the release workflow on `main`. This builds, smoke-tests and packages everything but publishes nothing.

   ```sh
   gh workflow run release.yml --ref main -f dry_run=true
   gh run watch
   ```

   A PR that changes `.github/workflows/release.yml` or `.github/actions/gui-deb/` gets the same dry run automatically. (`workflow_dispatch` only works once the workflow is on `main`.)

4. Tag the merged commit on `main` and push the tag:

   ```sh
   git switch main && git pull
   git tag -a vX.Y.Z -m "flashcp vX.Y.Z"
   git push origin vX.Y.Z
   ```

The tag push starts the Release workflow. It checks that the tag matches the Cargo version (and that the GUI versions match it), and that the tagged commit is reachable from `origin/main` (tags on unmerged branches are rejected), builds static musl binaries for x86_64 and aarch64, smoke-tests each one (builds an exFAT image and runs `fsck.exfat`), installs the `.deb` and checks it, builds the GUI `.deb` (amd64) with the same steps as `gui.yml` (shared in `.github/actions/gui-deb`), installs it together with the CLI `.deb` and checks the helpers (root-owned, 0755, not setuid) and polkit policies, then publishes the GitHub release with the assets and `SHA256SUMS`.

## Verify

```sh
gh release view vX.Y.Z
```

The release should contain these assets:

- `flashcp-X.Y.Z-x86_64-unknown-linux-musl.tar.gz`
- `flashcp-X.Y.Z-aarch64-unknown-linux-musl.tar.gz`
- `flashcp_X.Y.Z_amd64.deb`
- `flashcp_X.Y.Z_arm64.deb`
- `flashcp-gui_X.Y.Z_amd64.deb` (desktop app; depends on `flashcp`, so install both: `sudo apt install ./flashcp_X.Y.Z_amd64.deb ./flashcp-gui_X.Y.Z_amd64.deb`)
- `SHA256SUMS`

Check the checksums:

```sh
gh release download vX.Y.Z -D /tmp/rel && cd /tmp/rel && sha256sum -c SHA256SUMS
```

## Name clash with mtd-utils

The package installs `/usr/bin/flashcp` and does not conflict with `mtd-utils`. If `mtd-utils` is installed, `flashcp` may run its unrelated MTD flash tool (`/usr/sbin/flashcp`, which comes first in PATH) instead of this one. Check with `command -v flashcp`, or `flashcp --help`: ours starts with "Fast file transfer to USB sticks and external drives". To be sure, call `/usr/bin/flashcp`. The .deb long description carries the same note.

## If something fails

Fix the problem on `main` through a PR. Then remove the tag and any draft or partial release, and tag again:

```sh
git push --delete origin vX.Y.Z
git tag -d vX.Y.Z
gh release delete vX.Y.Z --yes   # only if a release or draft exists
git tag -a vX.Y.Z -m "flashcp vX.Y.Z"
git push origin vX.Y.Z
```
