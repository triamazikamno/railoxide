# Releasing RailOxide

Publishing a GitHub release starts three workflows from the release tag:

- [`flatpak-release.yml`](../.github/workflows/flatpak-release.yml) builds and signs the Flatpak bundles, attaches them to the release, deploys the Flatpak repository to GitHub Pages and sends the release announcement.
- [`macos-release.yml`](../.github/workflows/macos-release.yml) builds the Apple Silicon DMG on `macos-15`, signs its checksum file, attaches the macOS files to the release and produces the Homebrew cask for promotion.
- [`windows-release.yml`](../.github/workflows/windows-release.yml) builds the x64 Windows ZIP on `windows-2022`, signs its checksum file, attaches the Windows files to the release and produces the Scoop manifest for promotion.

All three workflows reject a tag that does not match `v` plus the wallet version in `bins/wallet/Cargo.toml`, or whose commit is not on `main`. The macOS and Windows workflows also reject local Cargo path dependencies or patches that the tagged checkout cannot resolve. Publishing a release is the only way to run them, so never publish a release or tag to test something.

All three publication jobs run in the `flatpak-release` environment and sign with its release key. That environment's protection rules gate macOS and Windows publication as well as Flatpak publication. Pull-request builds never receive the key or release-write permission.

## Release assets

Each publisher owns a fixed set of asset names:

| Publisher | Assets |
| --- | --- |
| Flatpak | `RailOxide-<TAG>-x86_64.flatpak`, `RailOxide-<TAG>-no-hardware-x86_64.flatpak`, `RailOxide-<TAG>-aarch64.flatpak`, `RailOxide-<TAG>-no-hardware-aarch64.flatpak`, `RailOxide-<TAG>-build-provenance.tar.gz`, `SHA256SUMS`, `SHA256SUMS.asc` |
| macOS | `RailOxide-<TAG>-macos-arm64.dmg`, `RailOxide-<TAG>-macos-SHA256SUMS`, `RailOxide-<TAG>-macos-SHA256SUMS.asc` |
| Windows | `RailOxide-<TAG>-windows-x64.zip`, `RailOxide-<TAG>-windows-SHA256SUMS`, `RailOxide-<TAG>-windows-SHA256SUMS.asc` |

`SHA256SUMS.asc` signs `SHA256SUMS`, which covers the Flatpak assets. `RailOxide-<TAG>-macos-SHA256SUMS.asc` signs `RailOxide-<TAG>-macos-SHA256SUMS`, which covers the DMG. `RailOxide-<TAG>-windows-SHA256SUMS.asc` signs `RailOxide-<TAG>-windows-SHA256SUMS`, which covers the Windows ZIP. The public key is published at `https://triamazikamno.github.io/railoxide/flatpak/flatpak-release-public.gpg`.

Each publisher uploads, checks and verifies only its own names. It never replaces or deletes another platform's assets, so the workflows can finish in any order. A release can briefly carry some platforms' files before the others arrive.

The macOS build summary records the version, tag, source commit, Rust version, architecture, deployment target, DMG checksum and runner image. The same `build-provenance.json` travels in the `macos-release` workflow artifact. The Windows build summary records the same fields, with the Rust target in place of the deployment target and the ZIP checksum in place of the DMG checksum, and its `build-provenance.json` travels in the `windows-release` artifact.

## Retrying a failed publication

Rerun the failed workflow from the GitHub Actions page.

A macOS rerun accepts an existing macOS asset only when its bytes match the file being published, and an existing `.asc` only when it verifies against the intended checksum file. Any other existing macOS asset fails the run without replacing anything. Missing macOS assets are uploaded, and every macOS asset is then downloaded through its public URL and checked again.

A Windows rerun behaves the same way for the Windows assets.

The Flatpak publisher still fails if any Flatpak-owned name already exists on the release. After a partial Flatpak upload, inspect the release and remove the incomplete Flatpak assets by hand before rerunning. Leave the macOS and Windows assets alone.

## Promoting the Homebrew cask

The cask lives in [`triamazikamno/homebrew-tap`](https://github.com/triamazikamno/homebrew-tap) as `Casks/railoxide.rb`. railoxide CI holds no write credentials for the tap; a maintainer promotes each release by hand.

The macOS publish job renders the cask only after it has published and verified the macOS assets. The `railoxide-cask` artifact exists only on runs where that verification completed. Pull-request runs produce `railoxide-cask-unverified`, which must never be promoted.

To promote a release:

1. If `triamazikamno/homebrew-tap` does not exist, create it as a public repository with a README, so that it is not empty.
2. On your Mac, with `gh` signed in, `gpg` and Homebrew installed, run:

   ```bash
   scripts/promote-homebrew-cask
   ```

   Pass a tag, such as `scripts/promote-homebrew-cask v0.2.0`, to name the release you expect. The script refuses any tag other than the latest release.

The script uses your own `gh` login and git credentials. It stops at the first failed check and leaves the tap unchanged. It:

- picks the latest published release, counting pre-releases;
- downloads the DMG, checksum file and `.asc` without signing in, verifies the signature against the published public key in a temporary keyring, and checks the DMG against its signed checksum;
- downloads the `railoxide-cask` artifact from the release's successful `macOS release` run, and checks that its `version`, `sha256` and fixed release `url` match the verified release;
- taps `triamazikamno/tap` if needed, requires a clean checkout and fast-forwards it;
- refuses a cask whose `version` is not newer than the one in the tap, and stops without changes if the tap already has this cask;
- copies the cask into `Casks/railoxide.rb`, runs `brew style` and `brew audit`, and shows the change;
- asks before it commits and pushes. If you decline, it restores the tap.

If the push fails, the commit stays in your local tap checkout. Push it by hand.

If any step fails, leave the tap unchanged. Users keep the previous cask until the problem is fixed.

To withdraw a version, stop advertising it and document how affected users recover. Never overwrite a published binary, and do not move the tap to an older version, since an older build would then open wallet data written by the newer one.

## Promoting the Scoop manifest

The manifest lives in [`triamazikamno/scoop-bucket`](https://github.com/triamazikamno/scoop-bucket) as `bucket/railoxide.json`. As with the tap, railoxide CI holds no credentials for the bucket and a maintainer promotes each release by hand.

The Windows publish job renders the manifest only after it has published and verified the Windows assets. The `railoxide-scoop-manifest` artifact exists only on runs where that verification completed. Pull-request runs produce `railoxide-scoop-manifest-unverified`, which must never be promoted.

To promote a release, with `gh` signed in and `gpg` installed:

1. If `triamazikamno/scoop-bucket` does not exist, create it as a public repository with a README and an empty `bucket/` directory.
2. Confirm the tag is the latest published release, counting pre-releases:

   ```bash
   gh release list --repo triamazikamno/railoxide --limit 1
   ```

3. Download the Windows assets without signing in, then check the signature against the published public key in a temporary keyring and the ZIP against its signed checksum:

   ```bash
   tag=v0.2.0
   work="$(mktemp -d)"
   cd "$work"
   for asset in windows-x64.zip windows-SHA256SUMS windows-SHA256SUMS.asc; do
     curl -fLO "https://github.com/triamazikamno/railoxide/releases/download/$tag/RailOxide-$tag-$asset"
   done
   curl -fLo release-public.gpg https://triamazikamno.github.io/railoxide/flatpak/flatpak-release-public.gpg
   export GNUPGHOME="$work/gnupg"
   mkdir -m 700 "$GNUPGHOME"
   gpg --batch --import release-public.gpg
   gpg --batch --verify "RailOxide-$tag-windows-SHA256SUMS.asc" "RailOxide-$tag-windows-SHA256SUMS"
   sha256sum -c "RailOxide-$tag-windows-SHA256SUMS"
   ```

4. Download the `railoxide-scoop-manifest` artifact from that release's successful `Windows release` run, and check that its `version` is the tag without the `v`, its `hash` is the digest in the verified checksum file, and its `url` is the fixed release URL for that tag:

   ```bash
   run="$(gh run list --repo triamazikamno/railoxide --workflow windows-release.yml --event release --branch "$tag" --status success --json databaseId --jq '.[0].databaseId')"
   gh run download "$run" --repo triamazikamno/railoxide --name railoxide-scoop-manifest --dir manifest
   jq -r '.version, .architecture."64bit".hash, .architecture."64bit".url' manifest/railoxide.json
   ```

5. In a clean, up-to-date checkout of the bucket, compare the `version` with the one already in `bucket/railoxide.json`. Stop if the new version is not newer, since that is a stale or backwards promotion. Stop without changes if the bucket already has this manifest.
6. Copy `manifest/railoxide.json` to `bucket/railoxide.json`, review the diff, commit and push.

If any step fails, leave the bucket unchanged. Users keep the previous manifest until the problem is fixed. The withdrawal rules for the cask apply to the bucket too: never overwrite a published ZIP and never move the bucket to an older version.
