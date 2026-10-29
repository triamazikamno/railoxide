# Install RailOxide on Windows

Prebuilt RailOxide releases support x64 Windows 11. Other Windows versions and architectures can [install from source](install-from-source.md).

The release is a portable ZIP holding `wallet.exe`, which needs no Visual C++ Redistributable or other runtime. It stores wallet profiles in:

```text
%LOCALAPPDATA%\RailOxide
```

## Scoop

The project publishes a [Scoop](https://scoop.sh) manifest in its own bucket, [`triamazikamno/scoop-bucket`](https://github.com/triamazikamno/scoop-bucket). The manifest downloads the ZIP from the RailOxide GitHub Release and checks its SHA-256 checksum before installing. Scoop installs it for your user account only and needs no administrator rights.

Adding a bucket needs Git. If `scoop bucket add` reports that Git is missing, run `scoop install git` first.

Install RailOxide:

```powershell
scoop bucket add railoxide https://github.com/triamazikamno/scoop-bucket
scoop install railoxide/railoxide
```

> RailOxide's Windows binaries are not code-signed. Installing through this bucket means trusting our published binaries and release process. Scoop verifies the download checksum; it does not certify the app's safety.

Scoop adds a RailOxide shortcut under **Scoop Apps** in the Start Menu. It puts no command on `PATH`. The manifest runs no install, update or uninstall scripts, and it does not change SmartScreen, Defender or PowerShell execution-policy settings.

Upgrade to the latest published release:

```powershell
scoop update
scoop update railoxide
```

RailOxide does not check for updates itself. New versions reach you only when you run `scoop update`.

Uninstall:

```powershell
scoop uninstall railoxide
```

Uninstalling removes the app and its shortcut and keeps your wallet profiles. The manifest declares no persisted data, so `scoop uninstall --purge railoxide` also leaves them in place. Delete `%LOCALAPPDATA%\RailOxide` yourself only when you mean to remove the wallet, and only after confirming your recovery backups.

## Download the ZIP manually

Each GitHub Release has three Windows files:

- `RailOxide-v<VERSION>-windows-x64.zip`
- `RailOxide-v<VERSION>-windows-SHA256SUMS`
- `RailOxide-v<VERSION>-windows-SHA256SUMS.asc`

Download all three from the [Releases page](https://github.com/triamazikamno/railoxide/releases) into one directory. With [GnuPG](https://gnupg.org/download/) installed, for example through Gpg4win, open PowerShell in that directory and verify the signature and checksum:

```powershell
curl.exe -fsSLO https://triamazikamno.github.io/railoxide/flatpak/flatpak-release-public.gpg
gpg --import flatpak-release-public.gpg
gpg --verify RailOxide-v<VERSION>-windows-SHA256SUMS.asc RailOxide-v<VERSION>-windows-SHA256SUMS
$expected = (Get-Content RailOxide-v<VERSION>-windows-SHA256SUMS).Split(' ')[0]
$actual = (Get-FileHash -Algorithm SHA256 RailOxide-v<VERSION>-windows-x64.zip).Hash
if ($actual -eq $expected) { 'checksum OK' } else { 'checksum MISMATCH: do not use this file' }
```

`RailOxide-v<VERSION>-windows-SHA256SUMS.asc` signs `RailOxide-v<VERSION>-windows-SHA256SUMS`, which lists the ZIP. The macOS and Flatpak files on the same release have their own checksum files and signatures. All of them use the same release key.

> RailOxide's Windows binaries are not code-signed. Installing from the ZIP means trusting our published binaries and release process. A valid checksum and signature show that the file is the one we published; they do not certify the app's safety.

A ZIP downloaded through a browser is marked as coming from the internet, and Windows passes that mark on to the files you extract. Because `wallet.exe` is not code-signed, SmartScreen may warn on its first launch. If it does, allow this one file in either of these ways:

- Before extracting, right-click the ZIP, choose **Properties**, tick **Unblock** on the General tab and click **OK**. Then extract it.
- Or extract it as it is, and when SmartScreen shows "Windows protected your PC" on the first launch of `wallet.exe`, click **More info** and then **Run anyway**.

Extract the ZIP somewhere you keep programs, such as `%LOCALAPPDATA%\Programs`. Explorer's **Extract All** creates a folder named after the ZIP, with a `RailOxide-v<VERSION>-windows-x64` folder inside it. Run `wallet.exe` from that inner folder. Do not turn off SmartScreen or Defender.

## Privacy

The bucket and the ZIPs come only from GitHub: `github.com` and the redirects GitHub uses to deliver release assets. Every user of a release downloads the same file from the same public URL. The project runs no server, download intermediary, per-user URL or install callback. The wallet itself makes no update checks and has no telemetry.

GitHub can see request metadata such as your IP address. Maintainers get only GitHub's aggregate download counts and repository traffic statistics. Those include no downloader IP addresses and no record of individual downloads.

Scoop has no analytics. It clones the bucket from GitHub with Git and downloads the ZIP from GitHub directly. RailOxide's built-in Tor applies to the wallet's own traffic; it does not route Scoop's or Git's requests.

## Switch from a source install

The [source installer](install-from-source.md) puts `wallet.exe` in `%LOCALAPPDATA%\RailOxide\bin`, its source checkout in `%LOCALAPPDATA%\RailOxide\src`, and a `RailOxide.lnk` shortcut in the Start Menu. Scoop never upgrades that copy. If you passed `-InstallDir` or `-SourceDir`, use those paths instead.

To switch to Scoop, quit RailOxide, remove the source build, its checkout and its shortcut, then install through Scoop:

```powershell
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\RailOxide\bin", "$env:LOCALAPPDATA\RailOxide\src"
Remove-Item -Force "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\RailOxide.lnk"
scoop bucket add railoxide https://github.com/triamazikamno/scoop-bucket
scoop install railoxide/railoxide
```

> RailOxide's Windows binaries are not code-signed. Installing through this bucket means trusting our published binaries and release process. Scoop verifies the download checksum; it does not certify the app's safety.

Do not delete `%LOCALAPPDATA%\RailOxide` itself. It is your wallet profile, and the `bin` and `src` directories only sit inside it. The Scoop app opens the same profile.

If you keep both copies, they share one wallet profile, so the older source build can open data written by a newer release. Each copy has its own Start Menu shortcut. The source installer warns when Scoop has RailOxide installed, removes nothing and continues.
