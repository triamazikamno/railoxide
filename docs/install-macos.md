# Install RailOxide on macOS

Prebuilt RailOxide releases support Apple Silicon Macs running macOS 15 or later. Intel Macs and older macOS versions can [install from source](install-from-source.md).

The app is installed as `RailOxide.app`. It stores wallet profiles in:

```text
~/Library/Application Support/RailOxide
```

## Homebrew

The project publishes a Homebrew cask in its own tap, [`triamazikamno/homebrew-tap`](https://github.com/triamazikamno/homebrew-tap). The cask downloads the DMG from the RailOxide GitHub Release and checks its SHA-256 checksum before installing.

Homebrew collects its own analytics, separately from RailOxide. To opt out before installing, run:

```bash
brew analytics off
```

This turns off Homebrew's analytics for every `brew` command on this Mac. RailOxide never changes this setting for you.

Install RailOxide:

```bash
brew install --cask triamazikamno/tap/railoxide
```

> RailOxide is ad-hoc signed and is not notarized by Apple. Our Homebrew cask removes the app's quarantine attribute to bypass the usual first-launch Gatekeeper assessment. Installing through this tap means trusting our published binaries and release process. Homebrew verifies the download checksum; it does not certify the app's safety.

The cask removes `com.apple.quarantine` only from the installed `/Applications/RailOxide.app`. It does not change Gatekeeper or System Integrity Protection settings, and it has no other install, upgrade or uninstall hooks.

Upgrade to the latest published release:

```bash
brew upgrade --cask triamazikamno/tap/railoxide
```

RailOxide does not check for updates itself. New versions reach you only when you run `brew upgrade`.

Uninstall:

```bash
brew uninstall --cask triamazikamno/tap/railoxide
```

Uninstalling removes the app and keeps your wallet profiles. The cask has no `zap` stanza, so `brew uninstall --zap` also leaves them in place. Delete `~/Library/Application Support/RailOxide` yourself only when you mean to remove the wallet, and only after confirming your recovery backups.

## Download the DMG manually

Each GitHub Release has three macOS files:

- `RailOxide-v<VERSION>-macos-arm64.dmg`
- `RailOxide-v<VERSION>-macos-SHA256SUMS`
- `RailOxide-v<VERSION>-macos-SHA256SUMS.asc`

Download all three from the [Releases page](https://github.com/triamazikamno/railoxide/releases) into one directory. Then import the release public key and verify the signature and checksum:

```bash
curl -fsSLO https://triamazikamno.github.io/railoxide/flatpak/flatpak-release-public.gpg
gpg --import flatpak-release-public.gpg
gpg --verify RailOxide-v<VERSION>-macos-SHA256SUMS.asc RailOxide-v<VERSION>-macos-SHA256SUMS
shasum -a 256 -c RailOxide-v<VERSION>-macos-SHA256SUMS
```

`RailOxide-v<VERSION>-macos-SHA256SUMS.asc` signs `RailOxide-v<VERSION>-macos-SHA256SUMS`, which lists the DMG. The Flatpak files on the same release have their own `SHA256SUMS`, signed by `SHA256SUMS.asc`. Both signatures use the same release key.

Open the DMG and drag `RailOxide.app` onto the `Applications` folder.

> RailOxide is ad-hoc signed and is not notarized by Apple. Installing from the DMG means trusting our published binaries and release process. A valid checksum and signature show that the file is the one we published; they do not certify the app's safety.

A DMG downloaded through a browser keeps its quarantine attribute, and this path does not run the cask's quarantine removal. macOS therefore blocks the first launch. To open RailOxide anyway, follow [Apple's procedure for apps from unidentified developers](https://support.apple.com/en-us/102445):

<!-- The dialog sequence below is verified at the Mac checkpoint (task 6.2). -->

1. Open `RailOxide.app`. macOS reports that it cannot verify the app and does not open it. Close the dialog.
2. Open System Settings, then Privacy & Security.
3. In the Security section, click "Open Anyway" next to the message about RailOxide.
4. Confirm that you want to open it, and authenticate if asked.

macOS remembers this choice for RailOxide only. Do not disable Gatekeeper or clear quarantine attributes system-wide.

## Privacy

The tap metadata and the DMGs come only from GitHub: `github.com` and the redirects GitHub uses to deliver release assets. Every user of a release downloads the same file from the same public URL. The project runs no server, download intermediary, per-user URL or install callback. The wallet itself makes no update checks and has no telemetry.

GitHub can see request metadata such as your IP address. Maintainers get only GitHub's aggregate download counts and repository traffic statistics. Those include no downloader IP addresses and no record of individual downloads.

Homebrew's analytics are separate from RailOxide, and `brew analytics off` above turns them off. RailOxide's built-in Tor applies to the wallet's own traffic; it does not route Homebrew's downloads.

## Switch from a source install

The [source installer](install-from-source.md) puts the app in `~/Applications/RailOxide.app` and a command wrapper in `~/.local/bin/railoxide-wallet` (`<prefix>/bin/railoxide-wallet` if you passed `--prefix`). Homebrew never upgrades that copy.

To switch to the cask, quit RailOxide, remove the source build and its wrapper, then install the cask. Run `brew analytics off` first if you want to opt out of [Homebrew's analytics](#homebrew).

```bash
rm -rf ~/Applications/RailOxide.app ~/.local/bin/railoxide-wallet
brew install --cask triamazikamno/tap/railoxide
```

> RailOxide is ad-hoc signed and is not notarized by Apple. Our Homebrew cask removes the app's quarantine attribute to bypass the usual first-launch Gatekeeper assessment. Installing through this tap means trusting our published binaries and release process. Homebrew verifies the download checksum; it does not certify the app's safety.

Your wallet profile stays in `~/Library/Application Support/RailOxide`, and the cask app opens it.

If you keep both copies, they share one wallet profile, so the older source build can open data written by a newer release. The source installer warns when `/Applications/RailOxide.app` exists, and the `railoxide-wallet` command keeps launching the source build.
