<p align="center">
  <img height="192" src="bins/wallet/packaging/icons/railoxide-icon.svg" alt="RailOxide">
</p>

<h1 align="center">
  <img width="420" src="bins/wallet/assets/icons/hero-wordmark.svg#gh-dark-mode-only" alt="RailOxide">
  <img width="420" src="bins/wallet/assets/icons/hero-wordmark-light.svg#gh-light-mode-only" alt="RailOxide">
</h1>

<p align="center">Desktop wallet for RAILGUN private transactions.</p>

<p align="center">
  <a href="https://github.com/triamazikamno/railoxide/releases">Releases</a> ·
  <a href="#flatpak">Install</a> ·
  <a href="#build">Build</a> ·
  <a href="#privacy-model">Privacy Model</a>
</p>

---

## Status

RailOxide is under active development. APIs, wallet storage formats, and UI flows may change before a stable release.

## Features

- Fully open source
- Zero telemetry, zero home calls
- First-class integrated Tor support
- Indexed POI tree support, prevents UTXO spend intent leaking to the poi proxy operator
- Hardware-derived wallets:
  - public accounts have full hardware wallet support.
  - 0zk accounts are derived deterministically by signing a hash with a hardware device, private keys are **not** stored in app, but for signing they are briefly exposed in memory.
    Full on-device 0zk signing support is to be added as soon as hardware wallet vendors add railgun-specific cryptography functions.
- Aggressive request batching to reduce rpc throttling
- Resilient public broadcaster network connection management
- Block-builder sponsored self-broadcasting mode (Mainnet only). This is a reliable and permissionless way to send private transactions using an empty/underfunded EOA without public broadcasters 
- Decentralized and leak-free pricing discovery via on-chain chainlink oracles. Used both for display and suspicious public broadcaster filtering
- WalletConnect support for DeFi exposure

## Flatpak

Flatpak is the recommended Linux installation when a packaged release is available. The default package includes hardware-wallet support and raw USB access:

```bash
flatpak install --user https://triamazikamno.github.io/railoxide/flatpak/RailOxide.flatpakref
```

A restricted build omits hardware-wallet support and does not receive USB access:

```bash
flatpak install --user https://triamazikamno.github.io/railoxide/flatpak/RailOxide-NoHardware.flatpakref
```

Both builds otherwise use the same sandbox and update repository. Install only one variant at a time. See the [`Flatpak guide`](docs/flatpak.md) for permissions, updates, and data migration.

## Install From Source

RailOxide is alpha software and installs by building from source. The installer prompts for the source to build and defaults to the latest published GitHub release.

macOS/Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/triamazikamno/railoxide/main/scripts/install-wallet | bash
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/triamazikamno/railoxide/main/scripts/install-wallet.ps1 | iex
```

To inspect the installer first:

macOS/Linux:

```bash
curl -fsSLO https://raw.githubusercontent.com/triamazikamno/railoxide/main/scripts/install-wallet
less install-wallet
bash install-wallet
```

Windows PowerShell:

```powershell
iwr https://raw.githubusercontent.com/triamazikamno/railoxide/main/scripts/install-wallet.ps1 -OutFile install-wallet.ps1
notepad .\install-wallet.ps1
powershell -ExecutionPolicy Bypass -File .\install-wallet.ps1
```

The installers support macOS, Ubuntu/Debian Linux, and Windows. See [`Install from source`](docs/install-from-source.md) for options and platform notes.

## Build

Native dependencies include Rust 1.97.1 or newer and platform-specific C/C++ build dependencies.

```bash
cargo check -p wallet
cargo check -p wallet --features hardware
cargo build --release -p wallet --features hardware
```

For complete wallet build guides see

- [`Ubuntu`](docs/build-wallet-ubuntu.md)
- [`NixOS`](docs/nixos.md)
- [`macOS`](docs/build-wallet-macos.md)
- [`Windows`](docs/build-wallet-windows.md)

## Hardware Wallets

Ledger and Trezor support is available behind the `hardware` feature:

```bash
cargo run --bin wallet --features hardware
```

Current hardware-wallet support is hardware-derived software custody, not native RAILGUN hardware signing. The desktop app asks the device to derive profile material, then uses derived wallet material in desktop memory to prepare and sign RAILGUN spends. Treat this as hardware-assisted recovery/authorization for a software wallet, not as a guarantee that private transaction signing remains inside the hardware device.

## Privacy Model

RailOxide is privacy-oriented, but metadata privacy depends on mode and infrastructure choices.

The recommended default posture is built-in Tor with indexed POI artifacts. Built-in Tor routes wallet HTTP/RPC traffic through the bundled Tor client. Indexed POI artifacts avoid sending wallet blinded commitments for normal POI status and proof reads.

Direct mode is explicit and privacy-degraded. Proxy mode routes wallet HTTP/RPC traffic through the configured proxy, but embedded Waku libp2p transports are disabled in proxy mode to avoid proxy bypass. POI proxy mode is less private because it sends blinded commitment hashes associated with UTXOs being received or prepared for spend.

For details, see [`docs/privacy-model.md`](docs/privacy-model.md).

## LLM Use in Development

RailOxide is developed with heavy LLM assistance, designed and maintained by a human developer who reads and understands every line that ships.

Where LLMs are used:

- **Code generation.** Parts of the codebase are LLM-generated — notably GUI code and 100% of the unit tests. Generated code is reviewed and reworked by hand before merge; tests are additionally checked for what they assert, not just that they pass.
- **Refactoring.** Large mechanical refactors are LLM-assisted.
- **Code review and auditing.** Every change goes through LLM-assisted review cycles before merge, in addition to manual review. This regularly catches subtle bugs that human eyes alone would miss.

Security-sensitive paths — key material handling, transaction construction, proof generation — receive the strictest scrutiny regardless of how a first draft originated.

## Shared Crates

RailOxide depends on shared RAILGUN Rust crates from [`railgun-rust`](https://github.com/triamazikamno/railgun-rust).

## Disclaimer

RailOxide is free and open-source software distributed under the [MIT License](LICENSE). It is provided **"as is", without warranty of any kind**, express or implied. To the maximum extent permitted by law, the authors and contributors accept no liability for any claim, damages, or loss — including loss of funds — arising from the use of, or inability to use, this software.

This is alpha software that interacts with real cryptocurrency networks. You are solely responsible for your keys, your backups, and every transaction you sign. Verify transaction details before signing, keep offline backups of your recovery material, and do not commit funds you cannot afford to lose.
