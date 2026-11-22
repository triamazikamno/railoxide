use std::sync::Arc;

use gpui::{AppContext as _, Render, TestAppContext};
use gpui_kit::test::TestWindowExt as _;
use wallet_ops::settings::{WalletSettings, build_effective_chain_configs};
use wallet_ops::{PrivacyFactor, PrivacyFactorKind, TreeDeposits};

use super::*;
use crate::root::spend_authorization::{
    SpendAuthorizationSummary, spend_authorization_can_use_cached_password,
};

const USDC: Address = Address::repeat_byte(0x55);

/// One bad, one caution and two good factors, the last of which names a tree.
fn mixed_unshield_presentation() -> AmountPrivacyPresentation {
    let mut hints = evaluate_unshield_hints(&UnshieldHintInput {
        statistics: &AmountStatisticsStatus::NotConfigured,
        token: USDC,
        planned: None,
        own_deposits: &[],
        sender: UnshieldSender::Broadcaster,
        recipient_deposited: true,
        route: NetworkRoute::Direct,
        now: 0,
        entered_amount_with_deducted_fee: None,
        max_amount: None,
    });
    hints.factors.push(PrivacyFactor {
        level: PrivacyLevel::Good,
        kind: PrivacyFactorKind::FundingCrowd(TreeDeposits {
            tree: 0,
            deposits: 40,
        }),
    });
    unshield_presentation(&hints, None, "USDC", true)
}

fn mainnet() -> EffectiveChainConfig {
    build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .cloned()
        .unwrap()
}

struct HintsProbe(AmountPrivacyPresentation);

impl Render for HintsProbe {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(render_amount_privacy_hints("probe", &self.0, None, cx))
    }
}

#[gpui_kit::test]
fn factors_list_bad_first_and_name_their_level(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (_root, cx) = cx.add_window_view(|window, cx| {
        let probe = cx.new(|_| HintsProbe(mixed_unshield_presentation()));
        gpui_component::Root::new(probe, window, cx)
    });
    cx.update(|window, cx| {
        window.render_frame(cx);
        let rows = [
            ("recipient-deposited", "Bad: "),
            ("network", "Caution: "),
            ("sender", "Good: "),
        ]
        .map(|(key, level)| {
            let row = window.find(format!("probe-factor-{key}"));
            let label = row.label().unwrap_or_default();
            assert!(label.starts_with(level), "{key} reads {label:?}");
            row.bounds().origin.y
        });
        assert!(
            rows[0] < rows[1] && rows[1] < rows[2],
            "bad, then caution, then good: {rows:?}"
        );
        assert!(
            window
                .try_find("probe-factor-funding-crowd-trees")
                .is_some()
                && window.try_find("probe-factor-sender-trees").is_none(),
            "only a factor that names a tree explains trees"
        );
    });
}

#[gpui::test]
fn choosing_an_amount_fills_the_shield_amount_in_token_units(cx: &mut TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (_host, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(
            directory.path(),
            &runtime,
            window,
            cx,
        );
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    // Ethereum's USDC, which has 6 decimals.
    let usdc = alloy::primitives::address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    root.update_in(cx, |root, window, cx| {
        root.effective_token_registry =
            wallet_ops::settings::build_effective_token_registry(&WalletSettings::default())
                .unwrap();
        root.open_public_action_dialog(
            Arc::from("account-0"),
            PublicAssetId::Erc20(usdc),
            window,
            cx,
        );
        assert_eq!(root.public_form.action_mode, PublicActionMode::Shield);
        root.use_shield_amount(U256::from(1_000_000_000_u64), window, cx);
        assert_eq!(
            root.public_form
                .shield_amount_input
                .read(cx)
                .value()
                .as_ref(),
            "1000"
        );
    });
}

#[test]
fn a_direct_rpc_endpoint_makes_the_route_direct_even_through_tor() {
    let mut chain = mainnet();
    for (name, transports, mode, expected) in [
        (
            "Tor",
            &[RpcTransport::Wallet][..],
            WalletNetworkMode::Tor,
            NetworkRoute::Private,
        ),
        (
            "Tor with a direct endpoint",
            &[RpcTransport::Wallet, RpcTransport::Direct][..],
            WalletNetworkMode::Tor,
            NetworkRoute::Direct,
        ),
        (
            "direct mode",
            &[RpcTransport::Wallet][..],
            WalletNetworkMode::Direct,
            NetworkRoute::Direct,
        ),
    ] {
        chain.rpc_route = wallet_ops::RpcChainRoute::new_with_transports(
            1,
            transports
                .iter()
                .enumerate()
                .map(|(port, &transport)| {
                    let url =
                        reqwest::Url::parse(&format!("http://127.0.0.1:{}", port + 1)).unwrap();
                    (
                        url.into(),
                        transport,
                        wallet_ops::RpcEndpointRoles::default(),
                    )
                })
                .collect(),
        );
        assert_eq!(network_route(Some(&chain), mode), expected, "{name}");
    }
}

#[test]
fn base_token_shield_reads_its_wrapped_token_statistics() {
    let mut chain = mainnet();
    let wrapped = Address::repeat_byte(0xee);
    chain.wrapped_native_token = Some(wrapped);
    assert_eq!(
        shield_statistics_token(Some(&chain), PublicAssetId::Native),
        Some(wrapped)
    );
}

#[test]
fn review_repeats_the_factors_and_a_bad_one_never_forces_it() {
    let form = mixed_unshield_presentation();
    assert!(
        form.factors
            .iter()
            .any(|factor| factor.level == PrivacyLevel::Bad)
    );
    let summary = SpendAuthorizationSummary::new("Private unshield", "", Vec::new())
        .with_privacy_hints(Some(form.clone()));
    let review = summary
        .privacy_hints_for_test()
        .expect("the review shows the hints");
    assert_eq!(review.factors, form.factors);
    assert_eq!(review.notice, form.notice);
    assert!(
        spend_authorization_can_use_cached_password(&summary),
        "a remembered spend authorization still skips the review"
    );
}
