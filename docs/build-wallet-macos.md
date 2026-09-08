This guide builds the RailOxide desktop wallet binary from source on macOS with Ledger and Trezor support enabled.

You need Apple Command Line Tools, Rust, and Git. The wallet uses Metal GPU acceleration and does not require full Xcode.

## Install Command Line Tools

Install Apple's Command Line Tools first.

```bash
xcode-select --install
```

Verify the tools are active:

```bash
xcode-select -p
git --version
clang --version
```

`xcode-select -p` should report `/Library/Developer/CommandLineTools` or a full Xcode developer directory.

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

## Clone the repository

```bash
git clone https://github.com/triamazikamno/railoxide.git
cd railoxide
```

## Build with hardware wallet support

Build the optimized release binary:

```bash
cargo build --release -p wallet --features hardware
```

The wallet binary is written to:

```bash
target/release/wallet
```

View the available command-line options:

```bash
./target/release/wallet --help
```

Run the wallet:

```bash
./target/release/wallet
```

To store wallet data in a custom location:

```bash
./target/release/wallet --db-path "$HOME/RailOxideData"
```

## Profile Rust heap memory

The optional `heap-profiling` feature replaces the Rust global allocator with a
profiling-enabled jemalloc build. It is intended only for local profiling and is
not enabled by default in release or packaged wallet builds.

Build the unstripped profiling binary with frame pointers:

```bash
RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --profile profiling -p wallet --features heap-profiling
```

Run `target/profiling/wallet`, wait until the wallet reaches the state to
inspect, then press `Command-Option-Shift-H`. The wallet writes a sampled heap
profile to the macOS per-user temporary directory and logs the exact path.
Profiling builds also log each GPUI asset cache miss with its source path,
encoded size, format, and SVG intrinsic dimensions when present. Each heap dump
log also reports jemalloc's live allocated, active, resident, and retained byte
counts for comparison with the process footprint.

Install `jeprof` and the Rust stack-processing tools if needed:

```bash
brew install jemalloc
cargo install rustfilt inferno
```

Generate a Rust-demangled heap flamegraph using the same unstripped executable.
The script rebases wallet addresses from the runtime Mach-O mapping so `jeprof`
does not assign symbols using unadjusted macOS ASLR addresses:

```bash
PROFILE_PATH="dump.heap"
scripts/render-wallet-heap-profile \
  --profile "$PROFILE_PATH" \
  --binary target/profiling/wallet \
  --output /tmp/railoxide-heap.svg
```

## Package a macOS app

Run the packaging script to build the wallet with hardware support and create a macOS app and DMG. The script ad-hoc signs the app by default:

```bash
scripts/package-wallet-macos
```

The packaged app and DMG are written to:

```bash
target/macos/RailOxide.app
target/macos/RailOxide.dmg
```

## Troubleshooting

If `cargo` is not found after installing Rust, load rustup's environment:

```bash
. "$HOME/.cargo/env"
```
