## Install from the desktop app

A desktop build serves the extension only if `target/browser-extension.zip`, or the file named by
`RAILOXIDE_EXTENSION_BUNDLE`, existed when that build was made. The packaging scripts in this
repository do not build the extension yet, so run `scripts/build-browser-extension` before
building a desktop app that should carry it.

1. Open **Browser pairing** in the desktop sidebar and turn on **Enable browser gateway**.
2. Choose **Install extension…** under **Paired browsers**.
3. Open the address the dialog shows in the browser you want to pair, on this machine or on another one that can reach the listener.
4. Follow the page: download the zip, extract it, and load the folder through the browser's extension page.

## Build and load

Use the native development prerequisites, Python 3, `nightly-2026-08-16` with the `wasm32-unknown-unknown` target, and exactly `wasm-bindgen-cli 0.2.126`.
Install missing tools with:

```sh
rustup toolchain install nightly-2026-08-16 --profile minimal --component rustfmt --component clippy --target wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked
scripts/build-browser-extension
```

The build script also writes `target/browser-extension.zip`, which the desktop build embeds when it exists and serves from the install page. A desktop build made before the archive existed does not notice it appearing; run `touch bins/wallet/build.rs` (or set `RAILOXIDE_EXTENSION_BUNDLE` to the archive path) before rebuilding. After changing the version in `extensions/railoxide/manifest.json`, run `scripts/build-browser-extension` again before building the desktop, otherwise the desktop reports the new version while serving the previous archive.

1. Use desktop Brave based on Chromium 120 or later. In `brave://extensions`, enable Developer mode and select **Load unpacked**.
2. Select the absolute `target/browser-extension` directory printed by the build script.
3. Pin **RailOxide Gateway** extension and open its action popup.

For updates, rebuild, use **Load unpacked** again, extension will reload automatically.

## Pair and connect

Open **Browser pairing** in the desktop sidebar, above the network status pill. Turn on **Enable browser gateway** and choose the **+** action at the right of **Paired browsers**, with the tooltip **Create new pairing**. Enter the displayed 6-digit code in the browser extension.

## Connect a dapp

Choose RailOxide in the dapp's wallet selector. EIP-6963 discovery coexists with Brave Wallet by default and leaves Brave's global wallet setting under your control.

For older dapps, **Use window.ethereum** and **Appear as MetaMask** are separate opt-ins, both off by default. They live with the toolbar mode behind the settings (gear) menu in the popup header. The first attempts to install RailOxide as `window.ethereum`; another wallet can prevent replacement. The second advertises MetaMask compatibility. Reload dapp pages after changing either preference.
