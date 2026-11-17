use super::*;

#[test]
fn wallet_assets_embed_shared_icon_sets() {
    let assets = WalletAssets;

    for path in [
        ui::icons::refresh_ccw_icon_path(),
        ui::icons::tor_status_icon_path(),
        ui::icons::arrow_right_left_icon_path(),
    ] {
        assert!(assets.load(path).expect("load ui icon").is_some(), "{path}");
    }
    assert!(
        assets
            .load("railgun-ui/chains/ethereum.svg")
            .expect("load chain icon")
            .is_some()
    );
    for network in wallet_ops::bridge::NonEvmNetwork::ALL {
        let path = network.icon_asset_path();
        assert!(
            assets.load(path).expect("load network icon").is_some(),
            "{path}"
        );
    }
    assert!(
        assets
            .load("railgun-ui/tokens/1-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png")
            .expect("load token icon")
            .is_some()
    );
    // A non-EVM token takes the icon of a known token with its exact symbol, such as USDT
    // on Tron and USDC on Solana; a symbol no known token has, or a look-alike, gets none.
    for symbol in ["USDT", "USDC"] {
        let path = railgun_ui::symbol_token_icon_asset_path(symbol).expect(symbol);
        assert!(
            assets.load(&path).expect("load token icon").is_some(),
            "{path}"
        );
    }
    for symbol in ["BONK", "sUSDC"] {
        assert_eq!(railgun_ui::symbol_token_icon_asset_path(symbol), None);
    }
    assert!(
        assets
            .load("railgun/icons/clock.svg")
            .expect("load clock icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/book-user.svg")
            .expect("load book user icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/landmark.svg")
            .expect("load landmark icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/save.svg")
            .expect("load save icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/square-asterisk.svg")
            .expect("load square asterisk icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/wrench.svg")
            .expect("load wrench icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/file-pen-line.svg")
            .expect("load file pen line icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/folder-cog.svg")
            .expect("load folder cog icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/import.svg")
            .expect("load import icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/list.svg")
            .expect("load list icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/group.svg")
            .expect("load group icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/users.svg")
            .expect("load users icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/piggy-bank.svg")
            .expect("load piggy bank icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/chevrons-down.svg")
            .expect("load chevrons down icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/ledger-logo-short-white.svg")
            .expect("load ledger icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/trezor-symbol-white-rgb.svg")
            .expect("load trezor icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/walletconnect.svg")
            .expect("load walletconnect icon")
            .is_some()
    );
    assert!(
        assets
            .load("railgun/icons/telegram.svg")
            .expect("load telegram icon")
            .is_some()
    );
}
