use super::*;

/// An order of Address 1 for Address 2 delivered as `delivery`, placed from a set-up stealth
/// account at `Address::repeat_byte(3)`, with nothing observed yet. A Bridge order deposits its
/// 99 bought for [`BRIDGE_MINIMUM`] on the destination network, and a NEAR Intents one pays
/// [`NEAR_DEPOSIT_ADDRESS`]. Returns its UID and the account's observation.
pub(super) fn placed_swap(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delivery: wallet_ops::vault::SwapDelivery,
) -> (
    broadcaster_core::contracts::cow::OrderUid,
    wallet_ops::vault::ExecutorNonceObservation,
) {
    placed_swap_with(executors, operation, delivery, |_| {})
}

pub(super) struct PlacedPrivateBridge {
    pub(super) operation: ExecutorOperationId,
    pub(super) destination_operation: ExecutorOperationId,
    pub(super) delivery: BridgeDelivery,
    pub(super) uid: broadcaster_core::contracts::cow::OrderUid,
    pub(super) origin_observed: wallet_ops::vault::ExecutorNonceObservation,
    pub(super) destination_observed: wallet_ops::vault::ExecutorNonceObservation,
}

/// Linked Ethereum and Polygon accounts with a private Bridge order and its destination
/// shield signed. Neither the order's delivery nor the shield's execution is observed yet.
pub(super) fn placed_private_bridge(
    origin: &ExecutorStore,
    destination: &ExecutorStore,
    origin_delegate: Address,
    destination_delegate: Address,
    destination_token: Address,
) -> PlacedPrivateBridge {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload, SwapDestinationRecord,
    };

    let operation = ExecutorOperationId::random().unwrap();
    let destination_operation = ExecutorOperationId::random().unwrap();
    let receiver = Address::repeat_byte(0x51);
    origin
        .reserve_with_swap_approval(
            operation,
            origin_delegate,
            Some("Private swap"),
            &[],
            None,
            Some(destination_operation),
        )
        .unwrap();
    destination
        .reserve_swap_destination(
            destination_operation,
            destination_delegate,
            SwapDestinationRecord {
                origin_chain: 1,
                origin_operation: operation,
                destination_token,
                outcome: None,
            },
        )
        .unwrap();
    destination
        .bind_address(destination_operation, receiver)
        .unwrap();
    // The destination account's setup won its first nonce, and its shield is signed
    // at the next one.
    let setup_hash = B256::repeat_byte(69);
    record_setup(
        destination,
        destination_operation,
        destination_delegate,
        setup_hash,
        Vec::new(),
    );
    let destination_observed =
        ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
    confirm_setup(destination, destination_operation, destination_observed);
    destination
        .record_issued(
            destination_operation,
            IssuedExecutorPayload::new(
                U256::ONE,
                destination_delegate,
                B256::repeat_byte(70),
                ExecutorPayloadPurpose::SwapDestinationShield,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"shield"),
                    destination_observed,
                    Vec::new(),
                ),
            ),
        )
        .unwrap();
    let delivery = BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: 137,
        receiver,
        destination_token,
        surplus: BridgeSurplus::Reshield,
        private: Some(BridgePrivateDelivery {
            on_shield_failure: BridgeShieldFailure::KeepOnDestination,
        }),
    };
    let (uid, origin_observed) = placed_swap(origin, operation, SwapDelivery::Bridge(delivery));
    PlacedPrivateBridge {
        operation,
        destination_operation,
        delivery,
        uid,
        origin_observed,
        destination_observed,
    }
}

/// [`placed_swap`] with `edit` applied to the order's approved bounds.
pub(super) fn placed_swap_with(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delivery: wallet_ops::vault::SwapDelivery,
    edit: impl FnOnce(&mut wallet_ops::vault::SwapApprovedBounds),
) -> (
    broadcaster_core::contracts::cow::OrderUid,
    wallet_ops::vault::ExecutorNonceObservation,
) {
    let order = PlacedOrder {
        uid_hash: 0x11,
        hook_hash: 7,
        valid_to: 20,
        speeds_up: None,
    };
    placed_order(executors, operation, delivery, order, edit)
}

/// One order of [`placed_order`]: the byte of its ID's hash, the byte of its pre-hook's hash,
/// which its post-hook's follows, its expiry, and the open order it speeds up.
#[derive(Clone, Copy)]
pub(super) struct PlacedOrder {
    pub(super) uid_hash: u8,
    pub(super) hook_hash: u8,
    pub(super) valid_to: u32,
    pub(super) speeds_up: Option<broadcaster_core::contracts::cow::OrderUid>,
}

/// [`placed_swap_with`] for the order `placed`. A speed-up is a second order of the account
/// its open order set up: it shares that order's proof, input and pre-hook nonce, and names
/// it as the order its pre-hook invalidates.
pub(super) fn placed_order(
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    delivery: wallet_ops::vault::SwapDelivery,
    placed: PlacedOrder,
    edit: impl FnOnce(&mut wallet_ops::vault::SwapApprovedBounds),
) -> (
    broadcaster_core::contracts::cow::OrderUid,
    wallet_ops::vault::ExecutorNonceObservation,
) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use broadcaster_core::contracts::cow::OrderUid;
    use wallet_ops::vault::{
        AcrossOrderTerms, BridgeOrderTerms, BridgeProvider, ExecutorInputIdentity,
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload, NearIntentsOrderTerms, SwapAttempt, SwapDelivery, SwapProof,
        SwapRecipient, SwapTerms,
    };
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(30, B256::repeat_byte(30)), U256::ONE);
    let setup = if placed.speeds_up.is_some() {
        executors
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    } else {
        let setup = pending_setup(executors, operation);
        executors.record_account_read(operation, observed).unwrap();
        setup
    };
    let setup_hash = setup.issued()[0].hash();
    let input: ExecutorInputIdentity = serde_json::from_value(serde_json::json!({
        "tree": 4, "position": 16198, "commitment": "0x2"
    }))
    .unwrap();
    let uid = OrderUid::new(
        B256::repeat_byte(placed.uid_hash),
        Address::repeat_byte(3),
        placed.valid_to,
    );
    let hook = |nonce, hash, purpose, inputs| {
        IssuedExecutorPayload::new(
            U256::from(nonce),
            setup.delegate(),
            B256::repeat_byte(hash),
            purpose,
            ExecutorPayloadContext::new(Bytes::from_static(b"hook"), observed, inputs),
        )
    };
    let mut bounds = test_approval().bounds;
    let post_hook = hook(
        2_u64,
        placed.hook_hash + 1,
        ExecutorPayloadPurpose::SwapPostHook,
        Vec::new(),
    );
    // A Private delivery shields the output with a post-hook, and an Across one deposits it.
    let (post_hook, bridge) = match delivery {
        SwapDelivery::Reshield => (Some(post_hook), None),
        SwapDelivery::External { .. } => {
            bounds.post_hook_gas_limit = None;
            (None, None)
        }
        SwapDelivery::Bridge(delivery) => {
            bounds.destination_minimum = Some(BRIDGE_MINIMUM);
            match delivery.provider {
                BridgeProvider::Across => (
                    Some(post_hook),
                    Some(BridgeOrderTerms::Across(AcrossOrderTerms {
                        spoke_pool: Address::repeat_byte(0x55),
                        input_token: Address::repeat_byte(2),
                        output_token: delivery.destination_token,
                        input_amount: bounds.buy_amount,
                        output_amount: BRIDGE_MINIMUM,
                        quote_timestamp: 1_790_000_000,
                        fill_deadline: 1_790_007_200,
                        exclusive_relayer: Address::ZERO,
                        exclusivity_parameter: 0,
                        recipient: delivery.is_private().then_some(Address::repeat_byte(0x56)),
                        message_hash: delivery.is_private().then_some(B256::repeat_byte(0x57)),
                    })),
                ),
                BridgeProvider::NearIntents => {
                    bounds.post_hook_gas_limit = None;
                    (
                        None,
                        Some(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
                            deposit_address: NEAR_DEPOSIT_ADDRESS,
                            min_amount_out: BRIDGE_MINIMUM,
                            amount_out: BRIDGE_MINIMUM,
                            deadline: "2026-09-30T01:00:00.000Z".into(),
                            signed_quote: "{}".into(),
                        })),
                    )
                }
            }
        }
        // 1Click converts the whole payout, which needs no post-hook.
        SwapDelivery::NonEvmBridge(_) => {
            bounds.destination_minimum = Some(BRIDGE_MINIMUM);
            bounds.post_hook_gas_limit = None;
            (
                None,
                Some(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
                    deposit_address: NEAR_DEPOSIT_ADDRESS,
                    min_amount_out: BRIDGE_MINIMUM,
                    amount_out: BRIDGE_MINIMUM,
                    deadline: "2026-09-30T01:00:00.000Z".into(),
                    signed_quote: "{}".into(),
                })),
            )
        }
    };
    edit(&mut bounds);
    executors
        .record_swap_attempt(
            operation,
            SwapAttempt {
                use_id: wallet_ops::vault::SwapUseId::first(operation),
                terms: SwapTerms::new(
                    Address::repeat_byte(1),
                    Address::repeat_byte(2),
                    SwapRecipient::new(U256::ONE, [7; 32]),
                    setup_hash,
                ),
                proof: SwapProof::new(B256::repeat_byte(6), vec![input.clone()]),
                uid,
                submission: None,
                delivery,
                bounds,
                invalidates: placed.speeds_up,
                pre_hook: hook(
                    1_u64,
                    placed.hook_hash,
                    ExecutorPayloadPurpose::SwapPreHook,
                    vec![input],
                ),
                post_hook,
                bridge,
                speed_up: placed.speeds_up,
            },
        )
        .unwrap();
    (uid, observed)
}
