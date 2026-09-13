# RailOxide Flatpak

RailOxide publishes two Linux Flatpak variants from the same source and signed update repository.

Both variants include the browser extension. Use **Browser pairing** in the app to
[download and install it](browser-extension.md#install-from-the-desktop-app).

| Variant | Branch | Hardware feature | Raw USB |
| --- | --- | --- | --- |
| Default | `stable` | Enabled | Enabled |
| No hardware | `stable-no-hardware` | Disabled | None |

## Install

Install Flatpak through the Linux distribution's package manager if it is not already available.

Default build with Ledger and Trezor support:

```bash
flatpak install --user https://triamazikamno.github.io/railoxide/flatpak/RailOxide.flatpakref
```

The default build requires Flatpak 1.16 or newer for USB-only device-class access. This still exposes all raw USB devices, not only Ledger and Trezor devices.

Restricted build without hardware-wallet support:

```bash
flatpak install --user https://triamazikamno.github.io/railoxide/flatpak/RailOxide-NoHardware.flatpakref
```

Install only one variant at a time. Both branches use the application ID `io.github.triamazikamno.RailOxide`, export the same desktop launcher, and share the same private application data directory.

GitHub Releases also provide architecture-specific `.flatpak` bundles. Install a downloaded bundle with:

```bash
flatpak install --user ./RailOxide-VERSION-ARCH.flatpak
```

The bundle configures the same signed update repository as the `.flatpakref` installation.

## Run And Update

Launch RailOxide from the desktop menu or run:

```bash
flatpak run io.github.triamazikamno.RailOxide
```

Install available updates with:

```bash
flatpak update
```

To inspect the effective sandbox permissions:

```bash
flatpak info --show-permissions io.github.triamazikamno.RailOxide
```

## Permissions

Both variants receive network, GPU, Wayland, and fallback X11 access. They do not receive home-directory or host-filesystem access.

Network access is required for Tor bootstrap, RPC calls, Waku, artifacts, and WalletConnect. Flatpak cannot restrict a process to only its built-in Tor client, so this sandbox limits access to unrelated host data but cannot protect wallet material from a malicious RailOxide process.

The default build additionally receives `--device=usb`. This exposes raw USB devices for direct Ledger and Trezor access. Host udev rules must still allow the desktop user to open those devices. Use the current [Ledger Linux guidance](https://support.ledger.com/article/115005165269-zd) and [Trezor udev rules](https://trezor.io/learn/a/udev-rules) rather than granting world-writable access to HID devices.

The no-hardware build does not include hardware-wallet integration. Hardware-wallet controls, direct USB access, and Trezor Bridge support are absent from its UI and binary.

Direct X11 access weakens desktop isolation because X11 clients share a trusted display server. Wayland provides the stronger desktop boundary and is preferred when available.

## Wallet Data

Flatpak stores wallet data under:

```text
~/.var/app/io.github.triamazikamno.RailOxide/data/RailOxide
```

This is intentionally separate from the native installation at `~/.local/share/RailOxide`. RailOxide receives no permission to read the native directory.

To migrate an existing native installation, close every RailOxide process, back up the source directory, and copy it from the host:

```bash
mkdir -p ~/.var/app/io.github.triamazikamno.RailOxide/data
cp -a ~/.local/share/RailOxide ~/.var/app/io.github.triamazikamno.RailOxide/data/
```

Do not run native and Flatpak installations against the same live database directory.

Uninstalling the application normally preserves its private data. Delete it only when intentionally removing the wallet and after confirming recovery backups:

```bash
flatpak uninstall --user --delete-data io.github.triamazikamno.RailOxide
```

## Switch Variants

Uninstall the current branch without `--delete-data`, then install the other `.flatpakref`. The shared application ID preserves the private wallet directory.

List installed branches with:

```bash
flatpak list --app --columns=application,branch | grep io.github.triamazikamno.RailOxide
```
