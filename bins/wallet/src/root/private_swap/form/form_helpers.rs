#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn select_index(
    items: &[PrivateActionAssetSelectItem],
    token: Address,
) -> Option<gpui_component::IndexPath> {
    items
        .iter()
        .position(|item| item.token == token)
        .map(|index| gpui_component::IndexPath::default().row(index))
}

/// Whether two setup route candidate lists show the same: the same offers, in order, with the
/// labels and fee checks the form and the broadcaster picker show.
pub(super) fn same_shown_candidates(
    left: &[PublicBroadcasterCandidate],
    right: &[PublicBroadcasterCandidate],
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            same_offer(left, right)
                && left.identifier == right.identifier
                && left.version == right.version
                && left.fee_policy_status == right.fee_policy_status
        })
}

pub(super) fn same_chain_buy_items(items: Vec<PrivateActionAssetSelectItem>) -> Vec<SwapBuyItem> {
    items
        .into_iter()
        .map(|asset| SwapBuyItem {
            asset,
            near_only: false,
            contract: None,
        })
        .collect()
}

/// Where `token` sorts in an asset list: the native asset, then the chain's `wrapped` native
/// token, then every other token.
pub(super) fn native_rank(token: Address, wrapped: Option<Address>) -> u8 {
    if token == Address::ZERO {
        0
    } else if Some(token) == wrapped {
        1
    } else {
        2
    }
}

pub(super) fn destination_for(
    list: &[BridgeDestination],
    token: Address,
) -> Option<&BridgeDestination> {
    list.iter()
        .find(|destination| destination.destination_token == token)
}

/// The wrapped native token Across delivers on `chain` as the native asset: the `SpokePool`
/// unwraps the chain's configured wrapped native token for a receiver without code. A chain
/// without that setting has none, and its tokens are shown as themselves.
pub(super) const fn across_unwrapped_token(chain: &EffectiveChainConfig) -> Option<Address> {
    chain.wrapped_native_token
}

/// The surplus choice a provider takes: Across's `chosen` one; NEAR Intents converts it all.
pub(super) const fn bridge_surplus(
    provider: BridgeProvider,
    chosen: BridgeSurplus,
) -> BridgeSurplus {
    match provider {
        BridgeProvider::Across => chosen,
        BridgeProvider::NearIntents => BridgeSurplus::BridgedByProvider,
    }
}

/// A Public account as the swap form and its reviews name it: its label.
pub(in crate::root::private_swap) fn public_source_label(
    account: &PublicAccountMetadata,
) -> String {
    public_account_display_label(account).unwrap_or_else(|| "Public account".into())
}

/// A Pay from source's avatar: a Public account's identicon, and for Private balance the
/// same space left empty, so the labels line up.
pub(super) fn source_avatar(address: Option<Address>) -> gpui::Div {
    let avatar = div().size_4().flex_none();
    let Some(address) = address else {
        return avatar;
    };
    let color = ui::public_address::public_account_identicon_color(address.as_ref());
    let pattern = ui::public_address::public_account_identicon_pattern(address.as_ref());
    avatar.flex().flex_col().children(
        pattern
            .as_chunks::<{ ui::public_address::PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE }>()
            .0
            .iter()
            .map(|row| {
                div().flex_1().flex().children(row.iter().map(|active| {
                    // The identicon's colour is address-derived data, as on the Public tab.
                    let cell = div().flex_1();
                    if *active { cell.bg(rgb(color)) } else { cell }
                }))
            }),
    )
}

/// Whether `receive_to` can deliver on `chain`, another chain the picker lists. A Public
/// address needs only its RPC endpoints. Private balance also needs an accepted swap
/// profile, the wallet's private sync ready in this session, and either private funds for the
/// destination stealth account's setup or, with `reusable`, a set-up account there to select.
/// While it isn't known whether the funds held there can pay for the setup, the setup fee
/// reads as being checked.
pub(super) fn destination_availability(
    root: &WalletRoot,
    receive_to: ReceiveTo,
    chain: &EffectiveChainConfig,
    reusable: bool,
) -> NetworkAvailability {
    let rpc = resolve_effective_chain_rpc_route(chain.chain_id, chain).is_ok();
    match receive_to {
        ReceiveTo::PublicAddress => public_network_availability(rpc),
        ReceiveTo::PrivateBalance => private_network_availability(PrivateNetworkFacts {
            rpc,
            swap_profile: chain.swap_profile().is_some(),
            sync: network_sync(root.chain_states.get(&chain.chain_id)),
            // Debug UI fixture: a wallet without private funds there picks the network too.
            funds: if crate::root::private_swap::ui_fixture::active() {
                SetupFunds::Funded
            } else {
                destination_setup_funds(root, chain)
            },
            reusable,
        }),
    }
}

/// Where the wallet's private sync of a chain stands. A chain without a session yet reads as
/// loading: opening the Buy picker starts it. A synced chain taking in newer blocks stays
/// ready, so its quote isn't dropped for each catch-up.
const fn network_sync(state: Option<&ChainUtxoState>) -> NetworkSync {
    match state {
        Some(
            ChainUtxoState::Ready { .. }
            | ChainUtxoState::Syncing {
                catching_up: true, ..
            },
        ) => NetworkSync::Ready,
        Some(ChainUtxoState::Error { .. }) => NetworkSync::Failed,
        Some(state) => NetworkSync::Loading(state.sync_percent()),
        None => NetworkSync::Loading(None),
    }
}

/// How long after the fee subscription goes live a missing offer may still arrive. Offers
/// arrived up to about 30 s after it in testing; the margin covers slower broadcasters.
const FEE_OFFERS_SETTLE: Duration = Duration::from_mins(1);

/// Whether the wallet could pay a broadcaster for a stealth account's setup on `chain`: some
/// POI-verified, spendable private balance there is in a token that a broadcaster compatible
/// with the chain's executor profile accepts, under the fee policy and trust filter a new
/// setup route starts with. The picker then checks that balance against the setup estimate.
/// Without such a balance, notes held there leave it unknown while their POI status is being
/// refreshed, or while no broadcaster offer on the chain has arrived.
fn destination_setup_funds(root: &WalletRoot, chain: &EffectiveChainConfig) -> SetupFunds {
    let chain_id = chain.chain_id;
    let Some(profile) = chain.accepted_executor_profile() else {
        return SetupFunds::Unfunded;
    };
    let Some(state) = root.chain_states.get(&chain_id) else {
        return SetupFunds::Unfunded;
    };
    let Some(snapshot) = state.snapshot() else {
        return SetupFunds::Unfunded;
    };
    let rows = root.monitor_fee_rows();
    let funded = public_broadcaster_fee_token_options_from_snapshot(
        snapshot,
        &rows,
        Some(profile),
        root.public_broadcaster_fee_policy(false),
        &root.public_broadcaster_trust_filter(false),
        Some(&root.effective_token_registry),
        |token| {
            root.public_broadcaster_anchor_cache
                .cached_rate(chain_id, token)
        },
    )
    .iter()
    .any(|option| option.eligible_broadcaster_count > 0);
    // A POI refresh or a first offer on the chain can still make the held notes pay for it, and
    // so can an offer from a broadcaster that hasn't announced since the fee subscription went
    // live, as after a new Tor session.
    let settling = root
        .monitor_state
        .read()
        .fee_subscription_started()
        .is_some_and(|started| started.elapsed() < FEE_OFFERS_SETTLE);
    let pending =
        state.poi_refreshing() || settling || !rows.iter().any(|row| row.chain_id == chain_id);
    if funded {
        SetupFunds::Funded
    } else if snapshot.unspent_count > 0 && pending {
        SetupFunds::Unknown
    } else {
        SetupFunds::Unfunded
    }
}

/// The non-EVM delivery of a Bridge `review` whose bridge leg is quoted, whose strip amounts are
/// in the delivery's token and decimals.
pub(super) fn non_evm_bridge_review(review: &SwapReview) -> Option<NonEvmDelivery> {
    match (review.plan().delivery(), review.bridge()) {
        (SwapDelivery::NonEvmBridge(delivery), Some(_)) => Some(delivery),
        _ => None,
    }
}

/// 1Click's USD value of `amount` of a non-EVM delivery's token, at the rate its `quote` values
/// the expected output, labelled as 1Click's: "≈ $12.50 · 1Click estimate". For display only.
/// `None` when the quote states no USD value.
pub(super) fn one_click_usd_estimate(quote: &SwapBridgeQuote, amount: U256) -> Option<String> {
    let usd = amount
        .checked_mul(quote.expected_output_usd?)?
        .checked_div(quote.expected_output)?;
    Some(format!(
        "≈ {} · 1Click estimate",
        railgun_ui::format_usd_micro_value(usd)
    ))
}

/// What a NEAR Intents delivery to `network` makes public, for What becomes public.
pub(super) fn near_intents_disclosure(network: &str) -> String {
    format!(
        "The order names a 1Click deposit address, and the settlement pays it in the same transaction that unshields from Railgun. 1Click learns the receiver when the order is signed, so the receiver's funds on {network} can be traced to this swap."
    )
}

/// The review's warning for an unverified price. A non-EVM bridge leg without an anchor says
/// that no independent price is known for the token it delivers.
pub(super) fn unverified_price_warning(review: &SwapReview) -> String {
    match review.plan().delivery() {
        SwapDelivery::NonEvmBridge(delivery)
            if review
                .bridge()
                .is_some_and(|bridge| bridge.leg == BridgeLegPrice::Unverified) =>
        {
            format!(
                "{UNVERIFIED_PRICE_WARNING} No independent price is known for {} on {}.",
                delivery.symbol,
                delivery.network.display_name()
            )
        }
        _ => UNVERIFIED_PRICE_WARNING.to_owned(),
    }
}

/// A review's row naming a non-EVM token's mint or contract, which it copies. The native asset
/// has none.
pub(super) fn non_evm_contract_row(
    delivery: &NonEvmDelivery,
) -> Option<SpendAuthorizationSummaryRow> {
    let contract = delivery.contract.as_deref()?;
    Some(
        SpendAuthorizationSummaryRow::new(contract_label(delivery.network), contract)
            .with_short_copyable(railgun_ui::short_non_evm(contract)),
    )
}

/// What a non-EVM network calls a token's address: a mint on Solana, a contract elsewhere.
const fn contract_label(network: NonEvmNetwork) -> &'static str {
    match network {
        NonEvmNetwork::Solana => "Mint",
        NonEvmNetwork::Bitcoin | NonEvmNetwork::Tron | NonEvmNetwork::Zcash => "Contract",
    }
}

/// Under the Buy panel, the chosen non-EVM token's mint or contract, shortened, with a control
/// that copies it in full. Look-alike symbols are told apart by it. The native asset has none.
pub(super) fn render_non_evm_contract(form: &SwapForm) -> Option<gpui::Div> {
    let network = form.non_evm?;
    let contract = form.non_evm_token.clone()??;
    let copy_id = SharedString::from(format!("swap-buy-contract-{contract}-copy"));
    Some(
        div()
            .debug_selector(|| "swap-buy-contract".into())
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .child(
                app_muted_text(contract_label(network))
                    .text_xs()
                    .flex_none(),
            )
            .child(
                app_muted_text(railgun_ui::short_non_evm(&contract))
                    .text_xs()
                    .min_w_0()
                    .truncate()
                    .font_family(theme::APP_MONO_FONT_FAMILY),
            )
            .child(
                clipboard_with_toast(copy_id, contract)
                    .tooltip(format!("Copy {}", contract_label(network).to_lowercase())),
            ),
    )
}

/// The Receiver field's placeholder, which names the destination network's address format.
pub(super) const fn receiver_placeholder(network: Option<NonEvmNetwork>) -> &'static str {
    match network {
        None => "0x address",
        Some(NonEvmNetwork::Bitcoin) => "Bitcoin address: bc1…, 1… or 3…",
        Some(NonEvmNetwork::Solana) => "Solana address",
        Some(NonEvmNetwork::Tron) => "Tron address: T…",
        Some(NonEvmNetwork::Zcash) => "Zcash address: u1…, t1… or tex1…",
    }
}

/// Why a receiver can't be used on a non-EVM `network`, naming its format.
pub(super) fn non_evm_receiver_message(
    network: NonEvmNetwork,
    error: NonEvmReceiverError,
) -> String {
    let name = network.display_name();
    match error {
        NonEvmReceiverError::Testnet => {
            "Only mainnet addresses can receive the swap. Enter a mainnet address.".to_owned()
        }
        NonEvmReceiverError::OtherNetwork(_) => {
            format!("The receiver must be a {name} address.")
        }
        NonEvmReceiverError::Invalid => match network {
            NonEvmNetwork::Bitcoin => {
                "This isn't a valid Bitcoin address. Check it, including its checksum.".to_owned()
            }
            NonEvmNetwork::Solana => "This isn't a valid Solana address.".to_owned(),
            NonEvmNetwork::Tron => {
                "This isn't a valid Tron address. Check it, including its checksum.".to_owned()
            }
            NonEvmNetwork::Zcash => {
                "This isn't a valid Zcash address. Check it, including its checksum.".to_owned()
            }
        },
        NonEvmReceiverError::Sapling => {
            "The bridge can't pay a Sapling-only address. Enter a t1… address, or a unified \
             address (u1…) with an Orchard or transparent receiver."
                .to_owned()
        }
    }
}

/// The delivery of `destination`, a token NEAR Intents delivers on `network`, to `receiver`.
pub(super) fn non_evm_delivery(
    network: NonEvmNetwork,
    receiver: String,
    destination: &NonEvmBridgeDestination,
) -> NonEvmDelivery {
    NonEvmDelivery {
        network,
        receiver,
        asset_id: destination.near.destination_asset.clone(),
        contract: destination.contract.clone(),
        symbol: destination.symbol.clone(),
        decimals: destination.near.destination_decimals,
    }
}

/// The token a Bridge review delivers on its network. Only a Bridge review has one.
pub(super) fn review_destination_token(review: &SwapReview) -> Address {
    match review.plan().delivery() {
        SwapDelivery::Bridge(delivery) => delivery.destination_token,
        _ => review.plan().buy_token(),
    }
}

/// Why a Bridge receiver can't be used on `network`.
pub(super) fn bridge_receiver_message(rejection: BridgeReceiverRejection, network: &str) -> String {
    match rejection {
        BridgeReceiverRejection::ZeroAddress => {
            "Tokens sent to the zero address are lost. Enter the receiver's address.".to_owned()
        }
        BridgeReceiverRejection::Railgun => format!(
            "This is the Railgun contract on {network}. Tokens sent to it directly can't be recovered."
        ),
        BridgeReceiverRejection::SpokePool => format!(
            "This is the Across SpokePool on {network}. Tokens sent to it directly can't be recovered."
        ),
    }
}

/// A provider's refusal to quote the amount, for the Provider row. 1Click states its route
/// minimum in its message, which is shown as sent.
pub(super) fn bridge_rejection(error: &eyre::Report) -> Option<String> {
    match error.downcast_ref::<BridgeApiError>()? {
        BridgeApiError::AmountTooLow => {
            Some("The amount is below Across's minimum for this route.".to_owned())
        }
        BridgeApiError::Rejected { api, message, .. } if message.is_empty() => {
            Some(format!("{api} rejected this amount."))
        }
        // 1Click also sends these for pairs it lists, during outages that clear in minutes.
        BridgeApiError::Rejected {
            api: api @ BridgeApi::NearIntents,
            message,
            ..
        } if message == ONE_CLICK_NO_QUOTE => Some(format!(
            "{api} can't quote this pair right now. This is usually temporary. Retry in a few minutes."
        )),
        BridgeApiError::Rejected {
            api: api @ BridgeApi::NearIntents,
            message,
            ..
        } if message == ONE_CLICK_NO_LIQUIDITY => Some(format!(
            "{api} has no liquidity for this pair right now. This is usually temporary. Retry in a few minutes."
        )),
        BridgeApiError::Rejected { api, message, .. } => Some(format!("{api}: {message}")),
        _ => None,
    }
}

/// NEAR Intents' incident status page.
const NEAR_INTENTS_STATUS_URL: &str = "https://partners.near-intents.org/shield/status";
/// 1Click's refusals during an outage, as [`bridge_rejection`] and [`near_intents_status`]
/// match them.
const ONE_CLICK_NO_QUOTE: &str = "Quoting for this pair is not available";
const ONE_CLICK_NO_LIQUIDITY: &str = "No liquidity available";

/// NEAR Intents' status page, for a failure that a NEAR Intents outage can cause.
pub(super) fn near_intents_status(error: &eyre::Report) -> Option<&'static str> {
    match error.downcast_ref::<BridgeApiError>()? {
        BridgeApiError::Unavailable {
            api: BridgeApi::NearIntents,
            ..
        } => Some(NEAR_INTENTS_STATUS_URL),
        BridgeApiError::Rejected {
            api: BridgeApi::NearIntents,
            message,
            ..
        } if message == ONE_CLICK_NO_QUOTE || message == ONE_CLICK_NO_LIQUIDITY => {
            Some(NEAR_INTENTS_STATUS_URL)
        }
        _ => None,
    }
}

/// A provider that couldn't be reached on the swap's route. Retry asks on a fresh one.
pub(super) fn bridge_unreachable(error: &eyre::Report) -> Option<String> {
    match error.downcast_ref::<BridgeApiError>()? {
        BridgeApiError::RateLimited { api } => Some(format!(
            "{api} is limiting requests right now. Retry in a minute."
        )),
        // A 5xx, such as 1Click's "Internal server error": the provider was reached.
        BridgeApiError::Unavailable { api, .. } => Some(format!(
            "{api} is having a temporary problem. Retry in a few minutes."
        )),
        BridgeApiError::Timeout { api, .. } | BridgeApiError::Transport { api, .. } => Some(
            format!("Couldn't reach {api} through the swap's network route."),
        ),
        _ => None,
    }
}
