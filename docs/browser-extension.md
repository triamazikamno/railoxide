## Build and load

Use the native development prerequisites, Python 3, `nightly-2026-08-16` with the `wasm32-unknown-unknown` target, and exactly `wasm-bindgen-cli 0.2.126`.
Install missing tools with:

```sh
rustup toolchain install nightly-2026-08-16 --profile minimal --component rustfmt --component clippy --target wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked
scripts/build-browser-extension
```

1. Use desktop Brave based on Chromium 120 or later. In `brave://extensions`, enable Developer mode and select **Load unpacked**.
2. Select the absolute `target/browser-extension` directory printed by the build script.
3. Pin **RailOxide Gateway** extension and open its action popup.

For updates, rebuild, use **Load unpacked** again, extension will reload automatically.

## Pair and connect

Open **Browser pairing** in the desktop sidebar, above the network status pill. Turn on **Enable browser gateway** and choose the **+** action at the right of **Paired browsers**, with the tooltip **Create new pairing**. Enter the displayed 6-digit code in the browser extension.

## Connect a dapp

Choose RailOxide in the dapp's wallet selector. EIP-6963 discovery coexists with Brave Wallet by default and leaves Brave's global wallet setting under your control.

For older dapps, **Use window.ethereum** and **Appear as MetaMask** are separate opt-ins, both off by default. They live with the toolbar mode behind the settings (gear) menu in the popup header. The first attempts to install RailOxide as `window.ethereum`; another wallet can prevent replacement. The second advertises MetaMask compatibility. Reload dapp pages after changing either preference.
