---
name: release
description: Cut a new Aggrega release by tagging vX.Y.Z, watching the Release build workflow and fetching the Arch Linux tarball and macOS app. Use when asked to release, tag, ship or publish a new version.
---

# Release Aggrega

Pushing a tag matching `v[0-9]+.[0-9]+.[0-9]+*` triggers `.github/workflows/release.yml`. Both of its jobs rewrite the `[package]` version in `Cargo.toml` from the tag (that's what the sidebar shows), and each uploads a workflow **artifact**:

- `build-arch` builds in an `archlinux` container and uploads `aggrega-<version>-x86_64.tar.gz`.
- `build-macos` runs `packaging/macos/bundle.sh` on `macos-latest` and uploads `aggrega-<version>-macos-arm64.dmg` (an Apple silicon only, ad-hoc signed `Aggrega.app` in a disk image).

It does not create a GitHub Release.

## 1. Pick the version

- List existing tags with `git tag --sort=-v:refname | head`. The new version must be higher than the latest one.
- If the user didn't give a version, propose one from the commits since the last tag (`git log <last-tag>..main --oneline`): patch for fixes, minor for features. Wait for them to confirm.

## 2. Check `main`

- Be on `main`, up to date with `origin/main`, with a clean working tree.
- Run the `precheck` skill. Don't tag if it fails.
- Check that the latest CI run on `main` passed: `gh run list --workflow CI --branch main --limit 1`.

## 3. Don't bump the version in the repo

The Git tag is the only source of truth for release versions. Leave the `version` in `Cargo.toml`, `Cargo.lock` and `pkgver` in `packaging/arch/PKGBUILD` alone. They intentionally lag behind the tags, and the workflow sets the real version inside CI. No release commit is needed.

## 4. Tag and push (confirm first)

Pushing a tag is outward-facing and triggers a public build. Ask the user before running:

```bash
git tag vX.Y.Z
git push origin vX.Y.Z
```

## 5. Watch the build and fetch the artifact

```bash
gh run list --workflow "Release build" --limit 1          # get the run id
gh run watch <run-id> --exit-status
gh run download <run-id> --dir <scratch dir>
```

Report the result. On success, give the run URL and both artifact names. The jobs are independent, so one platform can fail while the other succeeds. On failure, show the failing step's log (`gh run view <run-id> --log-failed`). Don't delete or move the tag without asking the user.
