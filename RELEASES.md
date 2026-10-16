# Releases

wakterm is published as prebuilt binary archives and system packages on GitHub Releases from annotated `v*` tags by `.github/workflows/publish.yml`, with manifests synchronized afterward to the [Homebrew](https://brew.sh/) tap (`wakamex/tap`, backed by `wakamex/homebrew-tap`) and [Scoop](https://scoop.sh/) bucket (`wakamex/scoop-bucket`).

## Version source and targets

`VERSION` at the repository root is the single version source. Each release tag is annotated and named `vX.Y.Z`, matching `VERSION`.

| Platform | Runner / container | Rust target(s) | Published asset(s) |
|---|---|---|---|
| Ubuntu / Debian x86-64 | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` | `wakterm-vX.Y.Z.Ubuntu24.04.deb`, `wakterm-vX.Y.Z.Ubuntu24.04.tar.xz` |
| Fedora / RPM x86-64 | `ubuntu-24.04` (`fedora:43` container) | `x86_64-unknown-linux-gnu` | `wakterm-X.Y.Z-1.fedora43.x86_64.rpm` |
| macOS Universal | `macos-15` | `aarch64-apple-darwin`, `x86_64-apple-darwin` | `wakterm-macos-vX.Y.Z.zip` |
| Windows x86-64 | `windows-2025` | `x86_64-pc-windows-msvc` | `wakterm-vX.Y.Z-setup.exe`, `wakterm-windows-vX.Y.Z.zip` |

All release builds use the stable Rust toolchain with `--locked` against the committed `Cargo.lock` on clean runners without restored build caches. Each distributed archive and package includes `LICENSE.md` and the bundled font license files (`assets/fonts/LICENSE_*.txt`).

To build the release binaries and package for the current host platform locally:

```sh
cargo build --release --locked \
  -p wakterm \
  -p wakterm-gui \
  -p wakterm-mux-server \
  -p strip-ansi-escapes
TAG_NAME="v$(cat VERSION)" bash ci/deploy.sh
```

## Validation and release eligibility

`.github/workflows/validation.yml` builds all four platform targets on clean runners, runs the workspace test suite with `cargo nextest`, runs the headless GUI smoke check and derived-file check on Linux, and packages the platform artifacts with `ci/deploy.sh`. Its `validate` gate job (`wakamex/release-actions/validate-gate@stable`) runs with `if: always()` and succeeds only when every target build and test job succeeded.

`.github/workflows/release-eligibility.yml` calls `validation.yml` on pushes to `main` under the `release-eligible` job name, producing the `release-eligible / validate` status check required by the `Validated release tags` repository ruleset.

## Releasing vX.Y.Z

1. Update `VERSION` to `X.Y.Z` and write `release-notes/vX.Y.Z.md`.
2. Commit only `VERSION`, `Cargo.lock` (if changed), and `release-notes/vX.Y.Z.md` as `Release vX.Y.Z`, and push `main` without tags.
3. Wait for `release-eligible / validate` to succeed on that commit, and confirm that remote `main` still equals that commit.
4. Create an annotated tag `git tag -a vX.Y.Z -m "vX.Y.Z"` on that commit and push only the tag (`git push origin vX.Y.Z`).
5. `.github/workflows/publish.yml` reruns `validation.yml`, verifies the annotated tag against `VERSION` and `release-notes/vX.Y.Z.md` (`wakamex/release-actions/verify-release-tag@stable`), builds all release assets on clean runners, and hands the combined `release-assets` artifact to `wakamex/release-actions/.github/workflows/binary-release.yml@stable`, which writes `SHA256SUMS`, attests and verifies every asset, and publishes the immutable GitHub Release.
6. Once the GitHub Release is published, render and push the Homebrew cask and Scoop manifest from the tagged commit:

```sh
./ci/sync-package-repos.sh vX.Y.Z --push
```

## Verifying a download

Release attestations are signed by the shared release workflow. Verify any downloaded release asset with [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify ASSET --repo wakamex/wakterm \
  --signer-workflow wakamex/release-actions/.github/workflows/binary-release.yml
gh release verify-asset vX.Y.Z ASSET --repo wakamex/wakterm
```
