use super::*;

/// Open the swap's detail and press Recover…, which reveals the account in Stealth accounts
/// behind the dialogs and then asks recovery to open.
pub(super) fn press_swap_recover(
    swaps: &Entity<PrivateSwapsView>,
    operation: ExecutorOperationId,
    stage: SwapStage,
    cx: &mut gpui::VisualTestContext,
) {
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| {
            swaps.reload_records();
            assert_eq!(swaps.stage(swaps.record(operation).unwrap()), stage);
            swaps.show_detail(operation, window, cx);
        });
        window.draw(cx).clear(cx);
    });
    let recover = cx.debug_bounds("swap-progress-recover").unwrap();
    cx.simulate_click(recover.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

/// A reusable set-up stealth account, whose address is `Address::repeat_byte(3)`.
pub(super) fn reusable_account(executors: &ExecutorStore, operation: ExecutorOperationId) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::ExecutorNonceObservation;
    pending_setup(executors, operation);
    confirm_setup(
        executors,
        operation,
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE),
    );
}

/// Record a confirmed read of the account's execution nonce past its setup's.
pub(super) fn confirm_setup(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    observed: wallet_ops::vault::ExecutorNonceObservation,
) -> wallet_ops::vault::ExecutorRecord {
    executors.record_account_read(operation, observed).unwrap()
}

pub(super) fn cold_wallet_entry(address: Address) -> wallet_ops::vault::PublicAddressBookEntry {
    wallet_ops::vault::PublicAddressBookEntry {
        entry_uuid: "cold-wallet".into(),
        label: "Cold wallet".into(),
        address,
        display_order: 0,
    }
}

pub(super) fn set_receiver_text(
    swaps: &mut PrivateSwapsView,
    text: &str,
    window: &mut Window,
    cx: &mut Context<'_, PrivateSwapsView>,
) {
    let input = swaps.form.as_ref().unwrap().receiver_input.clone();
    input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    // Programmatic input changes don't emit InputEvent::Change.
    swaps.receiver_edited(window, cx);
}

/// A reusable set-up stealth account in the fixture wallet's records on Polygon, whose address
/// is `Address::repeat_byte(3)`, with the store that holds it.
pub(super) fn reusable_polygon_account(
    root: &Entity<WalletRoot>,
    cx: &gpui::VisualTestContext,
) -> (ExecutorStore, ExecutorOperationId) {
    let (vault, view, delegate) = root.read_with(cx, |root, _| {
        (
            root.vault_store.clone().unwrap(),
            root.view_session.clone().unwrap(),
            root.effective_chain_configs
                .get(137)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
                .delegate(),
        )
    });
    let executors = ExecutorStore::new(vault.db(), view, 137).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    executors
        .reserve(
            operation,
            delegate,
            Some("Private swap"),
            &[wallet_ops::ExecutorAsset::Erc20(STUB_POLYGON_USDT)],
        )
        .unwrap();
    reusable_account(&executors, operation);
    (executors, operation)
}

/// A setup broadcaster's offer for `fee` of `token` on `chain_id`.
pub(super) fn setup_offer(
    root: &Entity<WalletRoot>,
    chain_id: u64,
    token: Address,
    fee: U256,
    cx: &gpui::VisualTestContext,
) -> PublicBroadcasterCandidate {
    let delegate = root.read_with(cx, |root, _| {
        root.effective_chain_configs
            .get(chain_id)
            .unwrap()
            .accepted_executor_profile()
            .unwrap()
            .delegate()
    });
    PublicBroadcasterCandidate {
        chain_id,
        railgun_address: "setup-offer".into(),
        identifier: None,
        token,
        fee,
        fees_id: "setup-offer".into(),
        fee_expiration: std::time::SystemTime::now() + Duration::from_secs(60),
        reliability: 0.9,
        available_wallets: 1,
        version: "8.2.3".into(),
        relay_adapt: None,
        relay_adapt_7702: Some(delegate),
        required_poi_list_keys: Vec::new(),
        viewing_public_key: [1; 32],
        address_data: broadcaster_core::crypto::railgun::AddressData {
            master_public_key: U256::ONE,
            viewing_public_key: [1; 32],
        },
        fee_policy_status: wallet_ops::BroadcasterFeePolicyStatus::UnknownAnchor,
    }
}

/// The broadcaster a test's setup progress was sent to.
pub(super) fn setup_broadcaster() -> PublicBroadcasterCandidate {
    crate::root::private_swap::setup_progress::stand_in_setup_broadcaster(1, None)
}

/// The wallet's operation client, as the form opens one for each fee estimate.
pub(super) fn operation_client(
    root: &Entity<WalletRoot>,
    runtime: &tokio::runtime::Runtime,
    cx: &gpui::VisualTestContext,
) -> wallet_ops::OperationHttpClient {
    let http = root.read_with(cx, |root, _| root.http.clone());
    runtime.block_on(http.operation_http_client()).unwrap()
}

/// Whether the open picker lists Polygon as a network that can be picked.
pub(super) fn picker_polygon(swaps: &PrivateSwapsView, cx: &App) -> NetworkAvailability {
    let content = swaps.buy_picker_content(swaps.form.as_ref().unwrap(), cx);
    content
        .networks
        .iter()
        .find(|network| network.key == BuyNetworkKey::Chain(137))
        .unwrap()
        .availability
}

/// The destination minimum of the Bridge orders [`placed_swap`] places: 248.71 of a 6-decimal
/// token.
pub(super) const BRIDGE_MINIMUM: U256 = U256::from_limbs([248_710_000, 0, 0, 0]);
/// The 1Click deposit address of the NEAR Intents orders [`placed_swap`] places.
pub(super) const NEAR_DEPOSIT_ADDRESS: Address =
    alloy::primitives::address!("4444444444444444444444444444444444444444");

/// A traded Bridge order's observations at the account's `observed` block: its settlement
/// handed off to the bridge, with an Across `deposit_id` or none for NEAR Intents, and the
/// bridge reported `outcome` so far.
pub(super) fn bridge_observations(
    observed: wallet_ops::vault::ExecutorNonceObservation,
    deposit_id: Option<U256>,
    outcome: Option<wallet_ops::vault::SwapBridgeOutcome>,
) -> wallet_ops::vault::SwapOrderObservations {
    use wallet_ops::vault::{
        SwapBridgeHandoff, SwapObservation, SwapOrderObservations, SwapTradeAmounts,
    };
    let seen = SwapObservation {
        block: observed.block(),
        transaction_hash: Some(alloy::primitives::B256::repeat_byte(40)),
    };
    SwapOrderObservations {
        pre_hook_executed: Some(seen),
        traded: Some(seen),
        trade_amounts: Some(SwapTradeAmounts {
            sell_amount: U256::from(100),
            buy_amount: U256::from(99),
            fee_amount: U256::ZERO,
            settlement_gas_used: None,
            settlement_effective_gas_price: None,
            executed_fee: None,
            executed_fee_token: None,
        }),
        delivered: Some(seen),
        bridge_handoff: Some(SwapBridgeHandoff {
            observation: seen,
            deposit_id,
        }),
        bridge_outcome: outcome,
        ..Default::default()
    }
}

/// Expires the Bridge order `first` untraded with `expired`, then places a retry of its swap
/// use at the same nonces, with `edit` applied to the retry's approved bounds, and records the
/// trade on the retry. Returns the retry's UID.
pub(super) fn retried_and_traded(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    first: broadcaster_core::contracts::cow::OrderUid,
    observed: wallet_ops::vault::ExecutorNonceObservation,
    expired: wallet_ops::vault::SwapPreHookDeath,
    edit: impl FnOnce(&mut wallet_ops::vault::SwapApprovedBounds),
) -> broadcaster_core::contracts::cow::OrderUid {
    use alloy::primitives::{B256, Bytes};
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        ExecutorPayloadContext, ExecutorPayloadPurpose, IssuedExecutorPayload, SwapAttempt,
        SwapOrderObservations,
    };

    executors
        .record_swap_observations(
            operation,
            first,
            SwapOrderObservations {
                pre_hook_dead: Some(expired),
                ..Default::default()
            },
        )
        .unwrap();
    let record = executors
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let delegate = record.issued()[0].delegate();
    let swap = record.swap().unwrap();
    let order = &swap.orders()[0];
    let inputs = swap.proof().inputs().to_vec();
    let hook = |nonce: u64, hash, purpose, inputs| {
        IssuedExecutorPayload::new(
            U256::from(nonce),
            delegate,
            B256::repeat_byte(hash),
            purpose,
            ExecutorPayloadContext::new(Bytes::from_static(b"retry"), observed, inputs),
        )
    };
    let retry = OrderUid::new(B256::repeat_byte(0x12), Address::repeat_byte(3), 20);
    let mut bounds = order.bounds().clone();
    edit(&mut bounds);
    executors
        .record_swap_attempt(
            operation,
            SwapAttempt {
                use_id: order.use_id().unwrap(),
                terms: *swap.terms(),
                proof: swap.proof().clone(),
                uid: retry,
                submission: None,
                delivery: order.delivery(),
                bounds,
                invalidates: None,
                pre_hook: hook(1, 0x21, ExecutorPayloadPurpose::SwapPreHook, inputs),
                post_hook: order
                    .delivery()
                    .has_post_hook()
                    .then(|| hook(2, 0x22, ExecutorPayloadPurpose::SwapPostHook, Vec::new())),
                bridge: order.bridge().cloned(),
                speed_up: None,
            },
        )
        .unwrap();
    let traded = bridge_observations(observed, None, None);
    executors
        .record_swap_observations(
            operation,
            retry,
            SwapOrderObservations {
                traded: traded.traded,
                trade_amounts: traded.trade_amounts,
                ..Default::default()
            },
        )
        .unwrap();
    retry
}

/// `handed_off`, a traded Bridge order's observations, as the attempt that expired untraded
/// holds them once a later attempt's settlement made its deposit.
pub(super) fn deposit_of_a_later_trade(
    handed_off: &wallet_ops::vault::SwapOrderObservations,
    expired: wallet_ops::vault::SwapPreHookDeath,
) -> wallet_ops::vault::SwapOrderObservations {
    wallet_ops::vault::SwapOrderObservations {
        pre_hook_executed: None,
        traded: None,
        trade_amounts: None,
        pre_hook_dead: Some(expired),
        ..handed_off.clone()
    }
}
