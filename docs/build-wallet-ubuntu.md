This guide builds the RailOxide desktop wallet binary from source on Ubuntu with Ledger and Trezor support enabled.

The commands below target Ubuntu 24.04.

## Install System Dependencies

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  clang \
  cmake \
  curl \
  elfutils \
  gettext-base \
  git \
  jq \
  lld \
  llvm \
  pkg-config \
  libasound2-dev \
  libfontconfig-dev \
  libglib2.0-dev \
  libsqlite3-dev \
  libssl-dev \
  libstdc++-14-dev \
  libudev-dev \
  libusb-1.0-0-dev \
  libva-dev \
  libvulkan1 \
  libwayland-dev \
  libx11-xcb-dev \
  libxkbcommon-x11-dev \
  libzstd-dev \
  pipewire \
  xdg-desktop-portal
```

These packages cover the Rust native build chain, OpenSSL, GPUI's Linux desktop dependencies, and the USB libraries used by hardware-wallet support.

## Install Rust 1.97.1

If Rust 1.97.1+ is not installed, install it with `rustup`:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
  sh -s -- -y --default-toolchain 1.97.1
. "$HOME/.cargo/env"
rustc --version
cargo --version
```

Both version commands should report `1.97.1` or higher.

## Clone The Repository

```bash
git clone https://github.com/triamazikamno/railoxide.git
cd railoxide
```

## Build With Hardware Wallet Support

Run a check build first:

```bash
cargo check -p wallet --features hardware
```

Build the optimized release binary:

```bash
cargo build --release -p wallet --features hardware
```

The wallet binary is written to:

```bash
target/release/wallet
```

Verify the binary starts far enough to parse command-line options:

```bash
./target/release/wallet --help
```

Run the wallet:

```bash
./target/release/wallet
```

To store wallet data in a custom location:

```bash
./target/release/wallet --db-path "$HOME/.local/share/RailOxide"
```

## Hardware Wallet USB Access

The `hardware` feature enables Ledger and Trezor integration through `coins-ledger`, `hidapi-rusb`, and `trezor-client`.

If the app builds but cannot open a connected device and logs `Access denied (insufficient permissions)`, install udev rules so the desktop user can open the USB and HID device nodes.

Create the Ledger rules at `/etc/udev/rules.d/20-hw1.rules`:

```bash
sudo tee /etc/udev/rules.d/20-hw1.rules >/dev/null <<'EOF'
# HW.1, Nano
SUBSYSTEMS=="usb", ATTRS{idVendor}=="2581", ATTRS{idProduct}=="1b7c|2b7c|3b7c|4b7c", TAG+="uaccess", TAG+="udev-acl"

# Blue, Nano S, Nano X, Nano S Plus, Stax, Flex, Ledger test devices
SUBSYSTEMS=="usb", ATTRS{idVendor}=="2c97", TAG+="uaccess", TAG+="udev-acl"

# hidraw access for hidraw-based libraries
KERNEL=="hidraw*", ATTRS{idVendor}=="2c97", MODE="0666"
EOF
```

Create the Trezor rules at `/etc/udev/rules.d/51-trezor.rules`:

```bash
sudo tee /etc/udev/rules.d/51-trezor.rules >/dev/null <<'EOF'
# Trezor
SUBSYSTEM=="usb", ATTR{idVendor}=="534c", ATTR{idProduct}=="0001", MODE="0660", GROUP="plugdev", TAG+="uaccess", TAG+="udev-acl", SYMLINK+="trezor%n"
KERNEL=="hidraw*", ATTRS{idVendor}=="534c", ATTRS{idProduct}=="0001", MODE="0660", GROUP="plugdev", TAG+="uaccess", TAG+="udev-acl"

# Trezor v2
SUBSYSTEM=="usb", ATTR{idVendor}=="1209", ATTR{idProduct}=="53c0", MODE="0660", GROUP="plugdev", TAG+="uaccess", TAG+="udev-acl", SYMLINK+="trezor%n"
SUBSYSTEM=="usb", ATTR{idVendor}=="1209", ATTR{idProduct}=="53c1", MODE="0660", GROUP="plugdev", TAG+="uaccess", TAG+="udev-acl", SYMLINK+="trezor%n"
KERNEL=="hidraw*", ATTRS{idVendor}=="1209", ATTRS{idProduct}=="53c1", MODE="0660", GROUP="plugdev", TAG+="uaccess", TAG+="udev-acl"
EOF
```

Create the `plugdev` group if it does not exist, add your user to it, reload udev, and reconnect the hardware wallet:

```bash
sudo groupadd -f plugdev
sudo usermod -aG plugdev "$USER"
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Log out and back in after `usermod` so your session gets the new group membership. Do not run the wallet with `sudo`; fix USB permissions instead.

Current hardware-wallet support is hardware-derived software custody. The device is used to derive profile material, while RAILGUN transaction preparation and signing still happen in desktop memory.

## Troubleshooting

If `cargo` is not found after installing Rust, load rustup's environment:

```bash
. "$HOME/.cargo/env"
```

If `openssl-sys` fails, install `pkg-config` and `libssl-dev`.

If `hidapi-rusb`, `rusb`, Ledger, or Trezor crates fail to build, install `libusb-1.0-0-dev` and `libudev-dev`.

If GPUI/X11/Wayland dependencies fail to build, confirm the GUI packages from the dependency install command are present.

If running over SSH or in a headless environment, the GUI may fail to open even though the binary built correctly. Run it from a desktop session with X11 or Wayland available.
