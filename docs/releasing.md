# Releasing RailOxide

Publishing a GitHub release starts two workflows from the release tag:

- [`flatpak-release.yml`](../.github/workflows/flatpak-release.yml) builds and signs the Flatpak bundles, attaches them to the release, deploys the Flatpak repository to GitHub Pages and sends the release announcement.
- [`macos-release.yml`](../.github/workflows/macos-release.yml) builds the Apple Silicon DMG on `macos-15`, signs its checksum file, attaches the macOS files to the release and produces the Homebrew cask for promotion.

Both workflows reject a tag that does not match `v` plus the wallet version in `bins/wallet/Cargo.toml`, or whose commit is not on `main`. The macOS workflow also rejects local Cargo path dependencies or patches that the tagged checkout cannot resolve. Publishing a release is the only way to run them, so never publish a release or tag to test something.

Both publication jobs run in the `flatpak-release` environment and sign with its release key. That environment's protection rules gate macOS publication as well as Flatpak publication. Pull-request builds never receive the key or release-write permission.

## Release assets

Each publisher owns a fixed set of asset names:

| Publisher | Assets |
| --- | --- |
| Flatpak | `RailOxide-<TAG>-x86_64.flatpak`, `RailOxide-<TAG>-no-hardware-x86_64.flatpak`, `RailOxide-<TAG>-aarch64.flatpak`, `RailOxide-<TAG>-no-hardware-aarch64.flatpak`, `RailOxide-<TAG>-build-provenance.tar.gz`, `SHA256SUMS`, `SHA256SUMS.asc` |
| macOS | `RailOxide-<TAG>-macos-arm64.dmg`, `RailOxide-<TAG>-macos-SHA256SUMS`, `RailOxide-<TAG>-macos-SHA256SUMS.asc` |

`SHA256SUMS.asc` signs `SHA256SUMS`, which covers the Flatpak assets. `RailOxide-<TAG>-macos-SHA256SUMS.asc` signs `RailOxide-<TAG>-macos-SHA256SUMS`, which covers the DMG. The public key is published at `https://triamazikamno.github.io/railoxide/flatpak/flatpak-release-public.gpg`.

Each publisher uploads, checks and verifies only its own names. It never replaces or deletes the other platform's assets, so the two workflows can finish in either order. A release can briefly carry one platform's files before the other's arrive.

The macOS build summary records the version, tag, source commit, Rust version, architecture, deployment target, DMG checksum and runner image. The same `build-provenance.json` travels in the `macos-release` workflow artifact.

## Retrying a failed publication

Rerun the failed workflow from the GitHub Actions page.

A macOS rerun accepts an existing macOS asset only when its bytes match the file being published, and an existing `.asc` only when it verifies against the intended checksum file. Any other existing macOS asset fails the run without replacing anything. Missing macOS assets are uploaded, and every macOS asset is then downloaded through its public URL and checked again.

The Flatpak publisher still fails if any Flatpak-owned name already exists on the release. After a partial Flatpak upload, inspect the release and remove the incomplete Flatpak assets by hand before rerunning. Leave the macOS assets alone.

## Promoting the Homebrew cask

The cask lives in [`triamazikamno/homebrew-tap`](https://github.com/triamazikamno/homebrew-tap) as `Casks/railoxide.rb`. railoxide CI holds no write credentials for the tap; a maintainer promotes each release by hand.

The macOS publish job renders the cask only after it has published and verified the macOS assets. The `railoxide-cask` artifact exists only on runs where that verification completed. Pull-request runs produce `railoxide-cask-unverified`, which must never be promoted.

To promote a release:

1. If `triamazikamno/homebrew-tap` does not exist, create it as a public repository with a `Casks/` directory.
2. Download the `railoxide-cask` artifact from the release's `macOS release` run.
3. Without signing in, download the published DMG, checksum file and `.asc` from the release, then verify them:

   ```bash
   curl -fsSLO https://triamazikamno.github.io/railoxide/flatpak/flatpak-release-public.gpg
   gpg --import flatpak-release-public.gpg
   gpg --verify RailOxide-<TAG>-macos-SHA256SUMS.asc RailOxide-<TAG>-macos-SHA256SUMS
   shasum -a 256 -c RailOxide-<TAG>-macos-SHA256SUMS
   ```

4. Confirm that the cask's `sha256` equals the DMG checksum in the published checksum file, and that its `url` is the fixed release URL `https://github.com/triamazikamno/railoxide/releases/download/v#{version}/RailOxide-v#{version}-macos-arm64.dmg`.
5. Confirm that the release is the latest published release and that the cask's `version` is newer than the version currently in the tap. Reject a stale or backwards promotion, such as a cask from an older release whose run finished late.
6. Copy the cask into your local tap checkout and lint it:

   ```bash
   brew tap triamazikamno/tap
   cp railoxide.rb "$(brew --repository triamazikamno/tap)/Casks/railoxide.rb"
   brew style --cask triamazikamno/tap/railoxide
   brew audit --cask triamazikamno/tap/railoxide
   ```

7. Commit `Casks/railoxide.rb` in that checkout and push it to the tap.

If any step fails, leave the tap unchanged. Users keep the previous cask until the problem is fixed.

To withdraw a version, stop advertising it and document how affected users recover. Never overwrite a published binary, and do not move the tap to an older version, since an older build would then open wallet data written by the newer one.
