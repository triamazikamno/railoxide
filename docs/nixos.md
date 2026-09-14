This guide covers installing the RailOxide desktop wallet using [Nix](https://nixos.org/) and the provided flake.

The flake supports x86_64-linux, aarch64-linux, x86_64-darwin, and aarch64-darwin.
The package builds and embeds the browser extension automatically.

## Imperative Install

For release installs, pin the flake input to a release tag or commit. Replace `<tag-or-commit>` with the source revision you want to build:

```bash
nix profile install github:triamazikamno/railoxide/<tag-or-commit>
```

Using `github:triamazikamno/railoxide` without a ref tracks the repository default branch and is only recommended when you intentionally want the latest `main`.

The `railoxide` binary will be available on your `PATH`. Run it with:

```bash
railoxide
```

## Declarative Install (NixOS / home-manager)

Add RailOxide as a pinned flake input in your system or home-manager configuration:

```nix
{
  inputs.railoxide.url = "github:triamazikamno/railoxide/<tag-or-commit>";
  # ...
}
```

Then reference the package in your configuration:

```nix
# NixOS system configuration
environment.systemPackages = [
  inputs.railoxide.packages.${pkgs.system}.default
];

# home-manager configuration
home.packages = [
  inputs.railoxide.packages.${pkgs.system}.default
];
```

Rebuild your system or home-manager to install:

```bash
# NixOS
sudo nixos-rebuild switch --flake .

# home-manager
home-manager switch --flake .
```

## Development Shell

The flake provides a development shell with the Rust toolchain, its WASM target, Python 3, and the pinned wasm-bindgen helper:

```bash
nix develop
```

This drops you into a shell with `rustc`, `cargo`, `clang`, and all required system libraries. You can then build from source as usual:

```bash
scripts/build-browser-extension
RAILOXIDE_EXTENSION_BUNDLE="$PWD/target/browser-extension.zip" cargo build --release -p wallet
```

## Runtime Notes

On NixOS, the `wallet` binary is wrapped with the necessary `LD_LIBRARY_PATH` entries for Vulkan GPU rendering, Wayland/X11 display, and audio. No additional system configuration is required for the default build.

### Hardware Wallets

The flake builds with hardware wallet support (Ledger/Trezor) enabled by default.

Hardware wallet USB access on NixOS requires udev rules for Ledger and Trezor devices. See the [Ubuntu build guide](build-wallet-ubuntu.md#hardware-wallet-usb-access) for the udev rule files.

## Troubleshooting

If the wallet fails to open a window, ensure you are running from a desktop session with X11 or Wayland available. Headless/SSH sessions without display forwarding will not work.

If GPU rendering fails, check that a Vulkan-capable driver is installed:

```bash
nix-shell -p vulkan-tools --run vulkaninfo
```
