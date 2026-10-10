use super::*;
use alloy::primitives::FixedBytes;
use broadcaster_core::contracts::cow::OrderUid;

fn hook(
    purpose: ExecutorPayloadPurpose,
    nonce: u64,
    hash: u8,
    delegate: Address,
    observed: ExecutorNonceObservation,
    inputs: Vec<ExecutorInputIdentity>,
) -> IssuedExecutorPayload {
    IssuedExecutorPayload::new(
        U256::from(nonce),
        delegate,
        B256::repeat_byte(hash),
        purpose,
        ExecutorPayloadContext::new(Bytes::from_static(b"signed hook"), observed, inputs),
    )
}

#[test]
fn swap_hooks_persist_with_their_order_and_a_retry_waits_for_a_dead_pre_hook() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let delegate = Address::repeat_byte(1);
    let executor = Address::repeat_byte(2);
    store.reserve(operation, delegate, None, &[]).unwrap();
    store.bind_address(operation, executor).unwrap();

    // The delegation-only setup's nonce 0 is read as consumed, so the swap's pre-hook uses
    // k = 1.
    let before_setup =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.record_account_read(operation, before_setup).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), before_setup, Vec::new()),
            ),
        )
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.record_account_read(operation, observed).unwrap();

    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, Address::ZERO, U256::from(9), [7; 16]),
        2,
        3,
        UtxoSource {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    let inputs = vec![ExecutorInputIdentity::from_utxo(&input)];
    let terms = SwapTerms::new(
        Address::repeat_byte(5),
        Address::repeat_byte(6),
        SwapRecipient::new(U256::from(7), [8; 32]),
        setup,
    );
    let bounds = SwapApprovedBounds {
        anchors: vec![SwapAnchorObservation {
            source: Address::repeat_byte(10),
            block: observed.block(),
            block_timestamp: 1_700_000_000,
            updated_at: Some(1_699_990_000),
        }],
        ..swap_bounds()
    };
    let attempt = |digest: u8, valid_to: u32, hooks: u8, post_nonce: u64| SwapAttempt {
        use_id: SwapUseId::first(operation),
        submission: None,
        terms,
        proof: SwapProof::new(B256::repeat_byte(20), inputs.clone()),
        uid: OrderUid::new(B256::repeat_byte(digest), executor, valid_to),
        delivery: SwapDelivery::Reshield,
        bounds: bounds.clone(),
        invalidates: None,
        pre_hook: hook(
            ExecutorPayloadPurpose::SwapPreHook,
            1,
            hooks,
            delegate,
            observed,
            inputs.clone(),
        ),
        post_hook: Some(hook(
            ExecutorPayloadPurpose::SwapPostHook,
            post_nonce,
            hooks + 1,
            delegate,
            observed,
            Vec::new(),
        )),
        bridge: None,
    };

    // A post-hook is admitted only alongside its pre-hook, at exactly k + 1.
    let first = attempt(30, 1_000, 31, 2);
    assert!(matches!(
        store.record_issued(operation, first.post_hook.clone().unwrap()),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    assert!(matches!(
        store.record_swap_attempt(operation, attempt(30, 1_000, 31, 3)),
        Err(ExecutorStoreError::OutstandingNonce)
    ));
    // A destination reservation cannot become an origin account, even before its shield is
    // issued and even if a caller bypasses the reusable-account list.
    let destination = ExecutorOperationId::random().unwrap();
    let destination_executor = Address::repeat_byte(90);
    let destination_origin = ExecutorOperationId::random().unwrap();
    store
        .reserve_swap_destination(
            destination,
            delegate,
            SwapDestinationRecord {
                origin_chain: 137,
                origin_operation: destination_origin,
                destination_token: Address::repeat_byte(6),
                outcome: None,
            },
        )
        .unwrap();
    set_up(&store, destination, delegate, destination_executor);
    let mut unrelated = first.clone();
    unrelated.uid = OrderUid::new(B256::repeat_byte(30), destination_executor, 1_000);
    // An order for another use than the one that claims the account is refused as such.
    assert!(matches!(
        store.record_swap_attempt(destination, unrelated.clone()),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    unrelated.use_id = SwapUseId::first(destination_origin);
    assert!(matches!(
        store.record_swap_attempt(destination, unrelated),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    store.record_swap_attempt(operation, first.clone()).unwrap();

    // The pre-hook reserves its inputs against every other operation.
    let other = ExecutorOperationId::random().unwrap();
    store.reserve(other, delegate, None, &[]).unwrap();
    store.bind_address(other, Address::repeat_byte(40)).unwrap();
    store.record_account_read(other, observed).unwrap();
    assert!(matches!(
        store.record_issued(
            other,
            hook(
                ExecutorPayloadPurpose::Operation,
                1,
                41,
                delegate,
                observed,
                inputs.clone()
            )
        ),
        Err(ExecutorStoreError::InputReserved)
    ));

    // A read at the same height, so that the read of nonce k below still applies after it.
    let consumed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(13)), U256::from(2));
    // The spent pre-hook nonce resolves the pre-hook, but the post-hook can still run.
    assert!(
        store
            .record_account_read(operation, consumed)
            .unwrap()
            .has_unresolved_issued_work()
    );

    // After expiry the nonce is still k, and a retry waits for recorded death.
    store.record_account_read(operation, observed).unwrap();
    assert!(matches!(
        store.record_swap_attempt(operation, attempt(32, 2_000, 33, 2)),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    // A rejection doesn't prove the signed pre-hook can't run, even for a retry that names it.
    store
        .record_swap_submission(operation, first.uid, SwapSubmissionStatus::Rejected)
        .unwrap();
    let mut invalidating = attempt(32, 2_000, 33, 2);
    invalidating.invalidates = Some(first.uid);
    assert!(matches!(
        store.record_swap_attempt(operation, invalidating),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    let dead = SwapOrderObservations {
        pre_hook_dead: Some(SwapPreHookDeath {
            cause: SwapPreHookDeathCause::Expired,
            observation: SwapObservation {
                block: observed.block(),
                transaction_hash: None,
            },
        }),
        ..SwapOrderObservations::default()
    };
    store
        .record_swap_observations(operation, first.uid, dead)
        .unwrap();
    let mut different_pair = attempt(32, 2_000, 33, 2);
    let new_terms = SwapTerms::new(
        Address::repeat_byte(7),
        Address::repeat_byte(8),
        terms.recipient(),
        setup,
    );
    different_pair.terms = new_terms;
    let recorded = store
        .record_swap_attempt(operation, different_pair)
        .unwrap();

    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let restored = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(restored, recorded);
    let swap = restored.swap().unwrap();
    assert_eq!(swap.terms(), &new_terms);
    let [expired, retry] = swap.orders() else {
        panic!("one order per attempt");
    };
    assert_eq!(swap.order_terms(expired), &terms);
    assert_eq!(swap.order_terms(retry), &new_terms);
    assert_eq!(expired.observations(), dead);
    assert_eq!((retry.attempt(), retry.valid_to()), (1, 2_000));
    assert_eq!(retry.bounds(), &bounds);
    assert_eq!(
        (retry.pre_hook().nonce(), retry.post_hook().unwrap().nonce()),
        (U256::ONE, U256::from(2))
    );
    assert_eq!(restored.issued().len(), 5);
    assert_eq!(restored.reserved_inputs(), inputs);
    // An old order lacking per-order terms keeps the original pair after later reuse. An old
    // order without the unshield split sold its whole unshield amount. Observations from
    // before trade amounts or Across refunds were kept still decode, with no refund.
    let mut legacy = serde_json::to_value(&restored).unwrap();
    let legacy_order = legacy["swap"]["orders"][0].as_object_mut().unwrap();
    legacy_order.remove("terms");
    let legacy_observations = legacy_order["observations"].as_object_mut().unwrap();
    for field in ["trade_amounts", "bridge_refund"] {
        legacy_observations.remove(field).unwrap();
    }
    let legacy_bounds = legacy_order["bounds"].as_object_mut().unwrap();
    legacy_bounds.remove("unshield_amount");
    legacy_bounds.remove("unshield_fee_bps");
    let legacy: ExecutorRecord = serde_json::from_value(legacy).unwrap();
    let legacy_swap = legacy.swap().unwrap();
    assert_eq!(legacy_swap.order_terms(&legacy_swap.orders()[0]), &terms);
    assert_eq!(legacy_swap.orders()[0].observations(), dead);
    assert_eq!(legacy_swap.terms(), &new_terms);
    let legacy_bounds = legacy_swap.orders()[0].bounds();
    assert_eq!(
        (legacy_bounds.spend_amount(), legacy_bounds.unshield_fee_bps),
        (bounds.sell_amount, U256::ZERO)
    );
    // A shield observed before its fee was recorded decodes with the fee unknown.
    let shield = SwapShieldObservation {
        observation: SwapObservation {
            block: observed.block(),
            transaction_hash: Some(B256::repeat_byte(49)),
        },
        private_amount: bounds.private_minimum,
        fee: Some(U256::from(25u8)),
    };
    let mut legacy_shield = serde_json::to_value(shield).unwrap();
    legacy_shield
        .as_object_mut()
        .unwrap()
        .remove("fee")
        .unwrap();
    assert_eq!(
        serde_json::from_value::<SwapShieldObservation>(legacy_shield).unwrap(),
        SwapShieldObservation {
            fee: None,
            ..shield
        }
    );

    // Completed settlement also permits reuse, including its consumed post-hook nonce.
    let after_fill =
        ExecutorNonceObservation::new(BlockNumHash::new(14, B256::repeat_byte(14)), U256::from(3));
    store.record_account_read(operation, after_fill).unwrap();
    let delivered = SwapObservation {
        block: after_fill.block(),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    // Hooks never get a direct-call status; nonces past every hook resolve them.
    assert!(
        !store
            .record_swap_observations(
                operation,
                retry.uid(),
                SwapOrderObservations {
                    pre_hook_executed: Some(delivered),
                    traded: Some(delivered),
                    delivered: Some(delivered),
                    shielded: Some(SwapShieldObservation {
                        observation: delivered,
                        private_amount: bounds.private_minimum,
                        fee: None,
                    }),
                    ..Default::default()
                },
            )
            .unwrap()
            .has_unresolved_issued_work()
    );
    // The watermark outlives the nonce observation, so the hooks stay resolved without a
    // fresh read.
    assert!(
        !store
            .invalidate_observation(operation)
            .unwrap()
            .has_unresolved_issued_work()
    );
    store.record_account_read(operation, after_fill).unwrap();
    let mut after_completion = attempt(40, 3_000, 41, 4);
    after_completion.terms = new_terms;
    after_completion.pre_hook = hook(
        ExecutorPayloadPurpose::SwapPreHook,
        3,
        41,
        delegate,
        after_fill,
        inputs.clone(),
    );
    after_completion.post_hook = Some(hook(
        ExecutorPayloadPurpose::SwapPostHook,
        4,
        42,
        delegate,
        after_fill,
        Vec::new(),
    ));
    assert_eq!(
        store
            .record_swap_attempt(operation, after_completion)
            .unwrap()
            .swap()
            .unwrap()
            .orders()
            .len(),
        3
    );
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_order_follows_two_setups_signed_at_one_nonce_once_a_read_shows_it_consumed() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let delegate = Address::repeat_byte(1);
    let executor = Address::repeat_byte(2);
    store.reserve(operation, delegate, None, &[]).unwrap();
    store.bind_address(operation, executor).unwrap();

    // A fee re-quote signs a second setup at the nonce of the first.
    let before_setup =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.record_account_read(operation, before_setup).unwrap();
    for setup in [3, 4] {
        store
            .record_issued(
                operation,
                hook(
                    ExecutorPayloadPurpose::Operation,
                    0,
                    setup,
                    delegate,
                    before_setup,
                    Vec::new(),
                ),
            )
            .unwrap();
    }
    let latest_setup = B256::repeat_byte(4);
    let attempt = |observed: ExecutorNonceObservation| {
        let nonce = observed.nonce().to::<u64>();
        SwapAttempt {
            use_id: SwapUseId::first(operation),
            terms: SwapTerms::new(
                Address::repeat_byte(5),
                Address::repeat_byte(6),
                SwapRecipient::new(U256::from(7), [8; 32]),
                latest_setup,
            ),
            proof: SwapProof::new(B256::repeat_byte(20), swap_inputs()),
            uid: OrderUid::new(B256::repeat_byte(30), executor, 1_000),
            submission: None,
            delivery: SwapDelivery::Reshield,
            bounds: swap_bounds(),
            invalidates: None,
            pre_hook: hook(
                ExecutorPayloadPurpose::SwapPreHook,
                nonce,
                31,
                delegate,
                observed,
                swap_inputs(),
            ),
            post_hook: Some(hook(
                ExecutorPayloadPurpose::SwapPostHook,
                nonce + 1,
                32,
                delegate,
                observed,
                Vec::new(),
            )),
            bridge: None,
        }
    };
    // While the setups' nonce is unconsumed, no order is placed beside them.
    assert!(matches!(
        store.record_swap_attempt(operation, attempt(before_setup)),
        Err(ExecutorStoreError::OutstandingNonce)
    ));

    // A read past the nonce resolves both setups. Nothing records which of them ran.
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.record_account_read(operation, observed).unwrap();
    let placed = store
        .record_swap_attempt(operation, attempt(observed))
        .unwrap();
    assert_eq!(placed.swap().unwrap().orders().len(), 1);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn swap_records_written_before_external_delivery_decode_as_reshield() {
    // Match the named MessagePack swap record before External delivery: a required post-hook
    // and post-hook gas limit, and a setup approval without its delivery or token pair.
    #[derive(serde::Serialize)]
    enum EarlierDelivery {
        Reshield,
    }
    #[derive(serde::Serialize)]
    struct EarlierBounds {
        sell_amount: U256,
        unshield_amount: Option<U256>,
        unshield_fee_bps: U256,
        buy_amount: U256,
        private_minimum: U256,
        shield_fee_bps: U256,
        slippage_bps: u32,
        pre_hook_gas_limit: u64,
        post_hook_gas_limit: u64,
        hook_cost: Option<U256>,
        anchors: Vec<SwapAnchorObservation>,
    }
    #[derive(serde::Serialize)]
    struct EarlierHook {
        nonce: U256,
        payload: B256,
    }
    #[derive(serde::Serialize)]
    struct EarlierOrder {
        terms: Option<SwapTerms>,
        attempt: u32,
        uid: FixedBytes<56>,
        delivery: EarlierDelivery,
        bounds: EarlierBounds,
        pre_hook: EarlierHook,
        post_hook: EarlierHook,
        invalidates: Option<FixedBytes<56>>,
        observations: SwapOrderObservations,
        submission: Option<SwapSubmission>,
        submission_status: SwapSubmissionStatus,
    }
    #[derive(serde::Serialize)]
    struct EarlierSwap {
        terms: SwapTerms,
        proof: SwapProof,
        orders: Vec<EarlierOrder>,
    }
    #[derive(serde::Serialize)]
    struct EarlierApproval {
        bounds: EarlierBounds,
        price_verified: Option<bool>,
        price_acknowledged: bool,
    }
    #[derive(serde::Serialize)]
    struct SavedRecord<Approval> {
        version: u32,
        derivation: ExecutorDerivationScheme,
        origin: ExecutorRecordOrigin,
        operation: ExecutorOperationId,
        index: u32,
        address: Address,
        delegate: Address,
        retired: bool,
        assets: Vec<ExecutorAsset>,
        issued: Vec<IssuedExecutorPayload>,
        swap: Option<EarlierSwap>,
        swap_approval: Approval,
    }
    fn saved(
        assets: Vec<ExecutorAsset>,
        swap: Option<EarlierSwap>,
        swap_approval: impl serde::Serialize,
    ) -> ExecutorRecord {
        let saved = SavedRecord {
            version: 1,
            derivation: ExecutorDerivationScheme::Railgun7702V1,
            origin: ExecutorRecordOrigin::Reserved,
            operation: ExecutorOperationId::random().unwrap(),
            index: 7,
            address: Address::repeat_byte(2),
            delegate: Address::repeat_byte(1),
            retired: false,
            assets,
            issued: Vec::new(),
            swap,
            swap_approval,
        };
        rmp_serde::from_slice(&rmp_serde::to_vec_named(&saved).unwrap()).unwrap()
    }
    let bounds = || EarlierBounds {
        sell_amount: U256::from(9_975),
        unshield_amount: Some(U256::from(10_000)),
        unshield_fee_bps: U256::from(25),
        buy_amount: U256::from(9_999),
        private_minimum: U256::from(9_975),
        shield_fee_bps: U256::from(25),
        slippage_bps: 50,
        pre_hook_gas_limit: 900_000,
        post_hook_gas_limit: 300_000,
        hook_cost: Some(U256::ZERO),
        anchors: Vec::new(),
    };
    let (sell, buy) = (Address::repeat_byte(5), Address::repeat_byte(6));
    let terms = SwapTerms::new(
        sell,
        buy,
        SwapRecipient::new(U256::from(7), [8; 32]),
        B256::repeat_byte(3),
    );
    let reshield = saved(
        vec![ExecutorAsset::Erc20(sell), ExecutorAsset::Erc20(buy)],
        Some(EarlierSwap {
            terms,
            proof: SwapProof::new(B256::repeat_byte(20), Vec::new()),
            orders: vec![EarlierOrder {
                terms: Some(terms),
                attempt: 0,
                uid: FixedBytes::repeat_byte(30),
                delivery: EarlierDelivery::Reshield,
                bounds: bounds(),
                pre_hook: EarlierHook {
                    nonce: U256::ONE,
                    payload: B256::repeat_byte(31),
                },
                post_hook: EarlierHook {
                    nonce: U256::from(2),
                    payload: B256::repeat_byte(32),
                },
                invalidates: None,
                observations: SwapOrderObservations::default(),
                submission: None,
                submission_status: SwapSubmissionStatus::Accepted,
            }],
        }),
        EarlierApproval {
            bounds: bounds(),
            price_verified: Some(true),
            price_acknowledged: false,
        },
    );
    let order = &reshield.swap().unwrap().orders()[0];
    assert_eq!(order.delivery(), SwapDelivery::Reshield);
    assert_eq!(
        order.post_hook().map(|hook| (hook.nonce(), hook.payload())),
        Some((U256::from(2), B256::repeat_byte(32)))
    );
    assert_eq!(order.bounds().post_hook_gas_limit, Some(300_000));
    let approval = reshield.swap_approval().unwrap();
    assert_eq!(
        (
            approval.delivery,
            approval.tokens,
            approval.bounds.post_hook_gas_limit
        ),
        (SwapDelivery::Reshield, None, Some(300_000))
    );
    // The earlier approval's pair is the setup's assets, in sell-then-buy order.
    assert_eq!(reshield.swap_approval_tokens(), Some((sell, buy)));

    // An External approval keeps its own pair, including a native Buy the assets never hold.
    let external = SwapApproval {
        delivery: SwapDelivery::External {
            receiver: Address::repeat_byte(9),
        },
        tokens: Some(SwapApprovalTokens {
            sell,
            buy: Address::ZERO,
        }),
        ..approval.clone()
    };
    let native_buy = saved(vec![ExecutorAsset::Erc20(sell)], None, &external);
    assert_eq!(native_buy.swap_approval(), Some(&external));
    assert_eq!(
        native_buy.swap_approval_tokens(),
        Some((sell, Address::ZERO))
    );
    for record in [reshield, native_buy] {
        assert_eq!(
            rmp_serde::from_slice::<ExecutorRecord>(&rmp_serde::to_vec_named(&record).unwrap())
                .unwrap(),
            record
        );
    }
}

#[test]
fn swap_records_written_by_the_external_build_decode_unchanged() {
    // Match the named MessagePack swap record of the External build: no bridge terms,
    // destination minimum or bridge observations.
    #[derive(serde::Serialize)]
    enum EarlierDelivery {
        External { receiver: Address },
    }
    #[derive(serde::Serialize)]
    struct EarlierBounds {
        sell_amount: U256,
        unshield_amount: Option<U256>,
        unshield_fee_bps: U256,
        buy_amount: U256,
        private_minimum: U256,
        shield_fee_bps: U256,
        slippage_bps: u32,
        pre_hook_gas_limit: u64,
        post_hook_gas_limit: Option<u64>,
        hook_cost: Option<U256>,
        anchors: Vec<SwapAnchorObservation>,
    }
    #[derive(serde::Serialize)]
    struct EarlierObservations {
        pre_hook_executed: Option<SwapObservation>,
        traded: Option<SwapObservation>,
        trade_amounts: Option<SwapTradeAmounts>,
        delivered: Option<SwapObservation>,
        shielded: Option<SwapShieldObservation>,
        settlement_credit: Option<SwapShieldObservation>,
        pre_hook_dead: Option<SwapPreHookDeath>,
        undelivered: Option<SwapObservation>,
        expired: Option<SwapObservation>,
    }
    #[derive(serde::Serialize)]
    struct EarlierHook {
        nonce: U256,
        payload: B256,
    }
    #[derive(serde::Serialize)]
    struct EarlierOrder {
        terms: Option<SwapTerms>,
        attempt: u32,
        uid: FixedBytes<56>,
        delivery: EarlierDelivery,
        bounds: EarlierBounds,
        pre_hook: EarlierHook,
        post_hook: Option<EarlierHook>,
        invalidates: Option<FixedBytes<56>>,
        observations: EarlierObservations,
        submission: Option<SwapSubmission>,
        submission_status: SwapSubmissionStatus,
    }
    #[derive(serde::Serialize)]
    struct EarlierSwap {
        terms: SwapTerms,
        proof: SwapProof,
        orders: Vec<EarlierOrder>,
    }
    #[derive(serde::Serialize)]
    struct EarlierApproval {
        bounds: EarlierBounds,
        price_verified: Option<bool>,
        price_acknowledged: bool,
        delivery: EarlierDelivery,
        tokens: Option<SwapApprovalTokens>,
    }
    #[derive(serde::Serialize)]
    struct SavedRecord {
        version: u32,
        derivation: ExecutorDerivationScheme,
        origin: ExecutorRecordOrigin,
        operation: ExecutorOperationId,
        index: u32,
        address: Address,
        delegate: Address,
        retired: bool,
        assets: Vec<ExecutorAsset>,
        issued: Vec<IssuedExecutorPayload>,
        swap: EarlierSwap,
        swap_approval: EarlierApproval,
    }
    let receiver = Address::repeat_byte(9);
    let bounds = || EarlierBounds {
        sell_amount: U256::from(9_975),
        unshield_amount: Some(U256::from(10_000)),
        unshield_fee_bps: U256::from(25),
        buy_amount: U256::from(9_975),
        private_minimum: U256::from(9_975),
        shield_fee_bps: U256::ZERO,
        slippage_bps: 50,
        pre_hook_gas_limit: 900_000,
        post_hook_gas_limit: None,
        hook_cost: Some(U256::ZERO),
        anchors: Vec::new(),
    };
    let (sell, buy) = (Address::repeat_byte(5), Address::ZERO);
    let terms = SwapTerms::new(
        sell,
        buy,
        SwapRecipient::new(U256::from(7), [8; 32]),
        B256::repeat_byte(3),
    );
    let traded = SwapObservation {
        block: BlockNumHash::new(13, B256::repeat_byte(13)),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    let amounts = SwapTradeAmounts {
        sell_amount: U256::from(9_975),
        buy_amount: U256::from(10_100),
        fee_amount: U256::ZERO,
        settlement_gas_used: None,
        settlement_effective_gas_price: None,
        executed_fee: None,
        executed_fee_token: None,
    };
    let saved = SavedRecord {
        version: 1,
        derivation: ExecutorDerivationScheme::Railgun7702V1,
        origin: ExecutorRecordOrigin::Reserved,
        operation: ExecutorOperationId::random().unwrap(),
        index: 7,
        address: Address::repeat_byte(2),
        delegate: Address::repeat_byte(1),
        retired: false,
        assets: vec![ExecutorAsset::Erc20(sell)],
        issued: Vec::new(),
        swap: EarlierSwap {
            terms,
            proof: SwapProof::new(B256::repeat_byte(20), Vec::new()),
            orders: vec![EarlierOrder {
                terms: Some(terms),
                attempt: 0,
                uid: FixedBytes::repeat_byte(30),
                delivery: EarlierDelivery::External { receiver },
                bounds: bounds(),
                pre_hook: EarlierHook {
                    nonce: U256::ONE,
                    payload: B256::repeat_byte(31),
                },
                post_hook: None,
                invalidates: None,
                observations: EarlierObservations {
                    pre_hook_executed: Some(traded),
                    traded: Some(traded),
                    trade_amounts: Some(amounts),
                    delivered: Some(traded),
                    shielded: None,
                    settlement_credit: None,
                    pre_hook_dead: None,
                    undelivered: None,
                    expired: None,
                },
                submission: None,
                submission_status: SwapSubmissionStatus::Accepted,
            }],
        },
        swap_approval: EarlierApproval {
            bounds: bounds(),
            price_verified: Some(false),
            price_acknowledged: true,
            delivery: EarlierDelivery::External { receiver },
            tokens: Some(SwapApprovalTokens { sell, buy }),
        },
    };
    let record: ExecutorRecord =
        rmp_serde::from_slice(&rmp_serde::to_vec_named(&saved).unwrap()).unwrap();

    let delivery = SwapDelivery::External { receiver };
    let swap = record.swap().unwrap();
    let order = &swap.orders()[0];
    assert_eq!(
        (order.delivery(), order.post_hook(), order.bridge()),
        (delivery, None, None)
    );
    assert_eq!(order.bounds().destination_minimum, None);
    assert_eq!(
        order.observations(),
        SwapOrderObservations {
            pre_hook_executed: Some(traded),
            traded: Some(traded),
            trade_amounts: Some(amounts),
            delivered: Some(traded),
            ..SwapOrderObservations::default()
        }
    );
    assert!(swap.admits_attempt());
    let approval = record.swap_approval().unwrap();
    assert_eq!(
        (
            approval.delivery,
            approval.tokens,
            approval.bounds.destination_minimum
        ),
        (delivery, Some(SwapApprovalTokens { sell, buy }), None)
    );

    // Bridge approvals round-trip for each provider, with their destination terms.
    for (provider, surplus) in [
        (BridgeProvider::Across, BridgeSurplus::Reshield),
        (
            BridgeProvider::NearIntents,
            BridgeSurplus::BridgedByProvider,
        ),
    ] {
        let mut bridged = approval.clone();
        bridged.delivery = SwapDelivery::Bridge(BridgeDelivery {
            provider,
            destination_chain: 137,
            receiver,
            destination_token: Address::repeat_byte(10),
            surplus,
            private: None,
        });
        bridged.bounds.destination_minimum = Some(U256::from(9_900));
        bridged.tokens = Some(SwapApprovalTokens {
            sell,
            buy: Address::repeat_byte(6),
        });
        assert_eq!(
            rmp_serde::from_slice::<SwapApproval>(&rmp_serde::to_vec_named(&bridged).unwrap())
                .unwrap(),
            bridged
        );
    }
}

#[test]
fn swap_records_written_before_gas_shares_decode_without_them() {
    // Match the named MessagePack approval bounds and trade amounts of the Bridge build: no gas
    // share, gas figures, validity, settlement gas or executed fee.
    #[derive(serde::Serialize)]
    struct EarlierBounds {
        sell_amount: U256,
        unshield_amount: Option<U256>,
        unshield_fee_bps: U256,
        buy_amount: U256,
        private_minimum: U256,
        shield_fee_bps: U256,
        slippage_bps: u32,
        pre_hook_gas_limit: u64,
        post_hook_gas_limit: Option<u64>,
        hook_cost: Option<U256>,
        anchors: Vec<SwapAnchorObservation>,
        destination_minimum: Option<U256>,
    }
    #[derive(serde::Serialize)]
    struct EarlierApproval {
        bounds: EarlierBounds,
        price_verified: Option<bool>,
        price_acknowledged: bool,
        delivery: SwapDelivery,
        tokens: Option<SwapApprovalTokens>,
    }
    #[derive(serde::Serialize)]
    struct EarlierTradeAmounts {
        sell_amount: U256,
        buy_amount: U256,
        fee_amount: U256,
    }
    fn reencode<T: serde::de::DeserializeOwned>(value: &impl serde::Serialize) -> T {
        rmp_serde::from_slice(&rmp_serde::to_vec_named(value).unwrap()).unwrap()
    }
    let approval: SwapApproval = reencode(&EarlierApproval {
        bounds: EarlierBounds {
            sell_amount: U256::from(9_975),
            unshield_amount: Some(U256::from(10_000)),
            unshield_fee_bps: U256::from(25),
            buy_amount: U256::from(9_999),
            private_minimum: U256::from(9_975),
            shield_fee_bps: U256::from(25),
            slippage_bps: 50,
            pre_hook_gas_limit: 900_000,
            post_hook_gas_limit: Some(300_000),
            hook_cost: Some(U256::from(7)),
            anchors: Vec::new(),
            destination_minimum: None,
        },
        price_verified: Some(true),
        price_acknowledged: false,
        delivery: SwapDelivery::Reshield,
        tokens: None,
    });
    let bounds = &approval.bounds;
    assert_eq!(
        (bounds.slippage_bps, bounds.hook_cost),
        (50, Some(U256::from(7)))
    );
    assert_eq!(
        (
            bounds.gas_share_bps,
            bounds.gas_estimate,
            bounds.gas_allowance,
            bounds.gas_price_wei,
            bounds.valid_for_secs
        ),
        (None, None, None, None, None)
    );
    let amounts: SwapTradeAmounts = reencode(&EarlierTradeAmounts {
        sell_amount: U256::from(9_975),
        buy_amount: U256::from(10_100),
        fee_amount: U256::ZERO,
    });
    assert_eq!(
        (
            amounts.settlement_gas_used,
            amounts.settlement_effective_gas_price,
            amounts.executed_fee,
            amounts.executed_fee_token
        ),
        (None, None, None, None)
    );

    // Records with the new fields keep them, including gas prices beyond `u64`.
    let mut current = approval.clone();
    current.bounds.gas_share_bps = Some(2_500);
    current.bounds.gas_estimate = Some(U256::from(5_900_000));
    current.bounds.gas_allowance = Some(U256::from(1_475_000));
    current.bounds.gas_price_wei = Some(u128::MAX);
    current.bounds.valid_for_secs = Some(1_800);
    assert_eq!(reencode::<SwapApproval>(&current), current);
    let settled = SwapTradeAmounts {
        settlement_gas_used: Some(1_234_567),
        settlement_effective_gas_price: Some(u128::from(u64::MAX) + 1),
        executed_fee: Some(U256::from(1_413_251)),
        executed_fee_token: Some(Address::repeat_byte(6)),
        ..amounts
    };
    assert_eq!(reencode::<SwapTradeAmounts>(&settled), settled);
}

#[test]
fn external_swap_attempts_issue_only_their_pre_hook_and_their_trade_admits_the_next() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let delegate = Address::repeat_byte(1);
    let executor = Address::repeat_byte(2);
    let sell = Address::repeat_byte(5);
    store
        .reserve(operation, delegate, None, &[ExecutorAsset::Erc20(sell)])
        .unwrap();
    store.bind_address(operation, executor).unwrap();
    let before_setup =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.record_account_read(operation, before_setup).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            hook(
                ExecutorPayloadPurpose::Operation,
                0,
                3,
                delegate,
                before_setup,
                Vec::new(),
            ),
        )
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.record_account_read(operation, observed).unwrap();

    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, Address::ZERO, U256::from(9), [7; 16]),
        2,
        3,
        UtxoSource {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    let inputs = vec![ExecutorInputIdentity::from_utxo(&input)];
    let receiver = Address::repeat_byte(9);
    let external = SwapDelivery::External { receiver };
    let attempt = |delivery: SwapDelivery, post_hook: Option<IssuedExecutorPayload>| SwapAttempt {
        use_id: SwapUseId::first(operation),
        submission: None,
        // A native Buy asset, paid to the receiver directly.
        terms: SwapTerms::new(
            sell,
            Address::ZERO,
            SwapRecipient::new(U256::from(7), [8; 32]),
            setup,
        ),
        proof: SwapProof::new(B256::repeat_byte(20), inputs.clone()),
        uid: OrderUid::new(B256::repeat_byte(30), executor, 1_000),
        delivery,
        bounds: SwapApprovedBounds {
            buy_amount: U256::from(9_975),
            post_hook_gas_limit: None,
            ..swap_bounds()
        },
        invalidates: None,
        pre_hook: hook(
            ExecutorPayloadPurpose::SwapPreHook,
            1,
            31,
            delegate,
            observed,
            inputs.clone(),
        ),
        post_hook,
        bridge: None,
    };

    // The post-hook's presence must match the delivery kind.
    let post_hook = hook(
        ExecutorPayloadPurpose::SwapPostHook,
        2,
        32,
        delegate,
        observed,
        Vec::new(),
    );
    for mismatched in [
        attempt(external, Some(post_hook)),
        attempt(SwapDelivery::Reshield, None),
    ] {
        assert!(matches!(
            store.record_swap_attempt(operation, mismatched),
            Err(ExecutorStoreError::OperationMismatch)
        ));
    }

    let recorded = store
        .record_swap_attempt(operation, attempt(external, None))
        .unwrap();
    let [setup_payload, pre_hook] = recorded.issued() else {
        panic!("an External attempt issues only its pre-hook");
    };
    assert_eq!(setup_payload.hash(), setup);
    assert_eq!(
        (pre_hook.purpose(), pre_hook.nonce()),
        (ExecutorPayloadPurpose::SwapPreHook, U256::ONE)
    );
    let order = &recorded.swap().unwrap().orders()[0];
    assert_eq!(order.delivery(), external);
    assert!(order.post_hook().is_none());
    assert!(!recorded.records_future_nonce(observed.nonce()));
    // The native marker is never recorded as an ERC-20 asset.
    assert_eq!(recorded.assets(), &[ExecutorAsset::Erc20(sell)]);

    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let restored = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(restored, recorded);

    // The verified trade alone delivers an External order, which never carries a credit.
    let traded = SwapObservation {
        block: BlockNumHash::new(13, B256::repeat_byte(13)),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    let amounts = SwapTradeAmounts {
        sell_amount: U256::from(9_975),
        buy_amount: U256::from(9_975),
        fee_amount: U256::ZERO,
        settlement_gas_used: None,
        settlement_effective_gas_price: None,
        executed_fee: None,
        executed_fee_token: None,
    };
    let credit = SwapShieldObservation {
        observation: traded,
        private_amount: U256::from(9_975),
        fee: None,
    };
    assert!(matches!(
        store.record_swap_settlement(operation, order.uid(), traded, amounts, Some(credit), None),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    let settled = store
        .record_swap_settlement(operation, order.uid(), traded, amounts, None, None)
        .unwrap();
    let observations = settled.swap().unwrap().orders()[0].observations();
    assert_eq!(
        (observations.traded, observations.delivered),
        (Some(traded), Some(traded))
    );
    assert!(settled.swap().unwrap().admits_attempt());

    // Reuse at finalized depth needs only the fresh nonce k + 1, where the next attempt signs.
    let after_trade =
        ExecutorNonceObservation::new(BlockNumHash::new(14, B256::repeat_byte(14)), U256::from(2));
    assert!(settled.settled_swaps_at(after_trade.block().number));
    store
        .refresh_settled_swap_nonce(&settled, after_trade)
        .unwrap();
    let mut next = attempt(external, None);
    next.uid = OrderUid::new(B256::repeat_byte(40), executor, 2_000);
    next.pre_hook = hook(
        ExecutorPayloadPurpose::SwapPreHook,
        2,
        41,
        delegate,
        after_trade,
        inputs.clone(),
    );
    let recorded = store.record_swap_attempt(operation, next).unwrap();
    assert_eq!(
        recorded.swap().unwrap().orders()[1].pre_hook().nonce(),
        U256::from(2)
    );
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn bridge_swap_attempts_hand_off_and_admit_the_next_only_once_delivered() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let delegate = Address::repeat_byte(1);
    let executor = Address::repeat_byte(2);
    let sell = Address::repeat_byte(5);
    store
        .reserve(operation, delegate, None, &[ExecutorAsset::Erc20(sell)])
        .unwrap();
    store.bind_address(operation, executor).unwrap();
    let before_setup =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.record_account_read(operation, before_setup).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            hook(
                ExecutorPayloadPurpose::Operation,
                0,
                3,
                delegate,
                before_setup,
                Vec::new(),
            ),
        )
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.record_account_read(operation, observed).unwrap();

    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, Address::ZERO, U256::from(9), [7; 16]),
        2,
        3,
        UtxoSource {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    let inputs = vec![ExecutorInputIdentity::from_utxo(&input)];
    let receiver = Address::repeat_byte(9);
    let across = BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: 137,
        receiver,
        destination_token: Address::repeat_byte(10),
        surplus: BridgeSurplus::KeepInAccount,
        private: None,
    };
    let near = BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 56,
        receiver,
        destination_token: Address::ZERO,
        surplus: BridgeSurplus::BridgedByProvider,
        private: None,
    };
    let across_terms = BridgeOrderTerms::Across(AcrossOrderTerms {
        spoke_pool: Address::repeat_byte(11),
        input_token: Address::repeat_byte(6),
        output_token: Address::repeat_byte(10),
        input_amount: U256::from(9_975),
        output_amount: U256::from(9_900),
        quote_timestamp: 1_700_000_000,
        fill_deadline: 1_700_021_600,
        exclusive_relayer: Address::ZERO,
        exclusivity_parameter: 0,
        recipient: None,
        message_hash: None,
    });
    let near_terms = BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
        deposit_address: Address::repeat_byte(12),
        min_amount_out: U256::from(15),
        amount_out: U256::from(16),
        deadline: "2026-09-30T01:00:00.000Z".into(),
        signed_quote: r#"{"signature":"ed25519:..."}"#.into(),
    });
    // The pre-hook takes nonce `k`, the Across post-hook `k + 1`. The bought intermediate is
    // `buy`; the destination token lives only in the delivery.
    let attempt =
        |delivery: SwapDelivery,
         bridge: Option<BridgeOrderTerms>,
         post_hook: bool,
         (buy, uid, nonce, observed): (u8, u8, u64, ExecutorNonceObservation)| {
            SwapAttempt {
                use_id: SwapUseId::first(operation),
                terms: SwapTerms::new(
                    sell,
                    Address::repeat_byte(buy),
                    SwapRecipient::new(U256::from(7), [8; 32]),
                    setup,
                ),
                proof: SwapProof::new(B256::repeat_byte(20), inputs.clone()),
                uid: OrderUid::new(B256::repeat_byte(uid), executor, 1_000),
                submission: None,
                delivery,
                bounds: SwapApprovedBounds {
                    buy_amount: U256::from(9_975),
                    shield_fee_bps: U256::ZERO,
                    post_hook_gas_limit: post_hook.then_some(400_000),
                    destination_minimum: Some(U256::from(9_900)),
                    ..swap_bounds()
                },
                invalidates: None,
                pre_hook: hook(
                    ExecutorPayloadPurpose::SwapPreHook,
                    nonce,
                    uid + 1,
                    delegate,
                    observed,
                    inputs.clone(),
                ),
                post_hook: post_hook.then(|| {
                    hook(
                        ExecutorPayloadPurpose::SwapPostHook,
                        nonce + 1,
                        uid + 2,
                        delegate,
                        observed,
                        Vec::new(),
                    )
                }),
                bridge,
            }
        };
    let first = (6, 30, 1, observed);

    // Across posts a post-hook and NEAR Intents doesn't. Each order carries its own provider's
    // terms, with a surplus choice that provider supports.
    for mismatched in [
        attempt(
            SwapDelivery::Bridge(across),
            Some(across_terms.clone()),
            false,
            first,
        ),
        attempt(
            SwapDelivery::Bridge(near),
            Some(near_terms.clone()),
            true,
            first,
        ),
        attempt(SwapDelivery::Bridge(across), None, true, first),
        attempt(
            SwapDelivery::Bridge(across),
            Some(near_terms.clone()),
            true,
            first,
        ),
        attempt(
            SwapDelivery::Bridge(BridgeDelivery {
                surplus: BridgeSurplus::BridgedByProvider,
                ..across
            }),
            Some(across_terms.clone()),
            true,
            first,
        ),
        attempt(
            SwapDelivery::Bridge(BridgeDelivery {
                surplus: BridgeSurplus::Reshield,
                ..near
            }),
            Some(near_terms.clone()),
            false,
            first,
        ),
        attempt(
            SwapDelivery::Reshield,
            Some(across_terms.clone()),
            true,
            first,
        ),
    ] {
        assert!(matches!(
            store.record_swap_attempt(operation, mismatched),
            Err(ExecutorStoreError::OperationMismatch)
        ));
    }
    let recorded = store
        .record_swap_attempt(
            operation,
            attempt(
                SwapDelivery::Bridge(across),
                Some(across_terms.clone()),
                true,
                first,
            ),
        )
        .unwrap();
    let [_, pre_hook, post_hook] = recorded.issued() else {
        panic!("an Across attempt issues its pre-hook and post-hook");
    };
    assert_eq!(
        (pre_hook.nonce(), post_hook.nonce()),
        (U256::ONE, U256::from(2))
    );
    let order = &recorded.swap().unwrap().orders()[0];
    assert_eq!(order.bridge(), Some(&across_terms));
    // A skipped post-hook or a refund leaves the intermediate in the stealth account.
    assert_eq!(
        recorded.assets(),
        &[
            ExecutorAsset::Erc20(sell),
            ExecutorAsset::Erc20(Address::repeat_byte(6))
        ]
    );

    // Across hands off in the trade's receipt, with a deposit id. Kept surplus has no credit.
    let traded = SwapObservation {
        block: BlockNumHash::new(13, B256::repeat_byte(13)),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    let amounts = SwapTradeAmounts {
        sell_amount: U256::from(9_975),
        buy_amount: U256::from(10_000),
        fee_amount: U256::ZERO,
        settlement_gas_used: None,
        settlement_effective_gas_price: None,
        executed_fee: None,
        executed_fee_token: None,
    };
    let deposit = SwapBridgeHandoff {
        observation: traded,
        deposit_id: Some(U256::from(77)),
    };
    let credit = SwapShieldObservation {
        observation: traded,
        private_amount: U256::from(20),
        fee: None,
    };
    let elsewhere = SwapObservation {
        transaction_hash: Some(B256::repeat_byte(51)),
        ..traded
    };
    for (credit, handoff) in [
        (
            None,
            Some(SwapBridgeHandoff {
                deposit_id: None,
                ..deposit
            }),
        ),
        (
            None,
            Some(SwapBridgeHandoff {
                observation: elsewhere,
                ..deposit
            }),
        ),
        (Some(credit), Some(deposit)),
    ] {
        assert!(matches!(
            store.record_swap_settlement(operation, order.uid(), traded, amounts, credit, handoff),
            Err(ExecutorStoreError::InvalidRecord)
        ));
    }
    let settled = store
        .record_swap_settlement(operation, order.uid(), traded, amounts, None, Some(deposit))
        .unwrap();
    let observations = settled.swap().unwrap().orders()[0].observations();
    assert_eq!(
        (observations.delivered, observations.bridge_handoff),
        (Some(traded), Some(deposit))
    );
    // Handed off, but not yet delivered on the destination chain.
    assert!(!settled.swap().unwrap().admits_attempt());
    let tracked = |record: &ExecutorRecord| {
        record
            .swap_bridges_to_track()
            .map(SwapOrderRecord::uid)
            .collect::<Vec<_>>()
    };
    assert_eq!(tracked(&settled), vec![order.uid()]);
    let verified = |output_amount: u64| SwapBridgeOutcome::DeliveredVerified {
        block: BlockNumHash::new(500, B256::repeat_byte(60)),
        transaction_hash: B256::repeat_byte(61),
        output_amount: U256::from(output_amount),
        shielded: false,
    };
    // Across may fill a deposit it reported expired. Only a verified fill of at least the
    // approved minimum replaces the refund.
    let refunding = store
        .record_swap_bridge_outcome(operation, order.uid(), SwapBridgeOutcome::Refunding)
        .unwrap();
    assert!(tracked(&refunding).is_empty());
    for replacement in [
        SwapBridgeOutcome::NeedsAttention,
        SwapBridgeOutcome::DeliveredReported {
            amount_out: Some(U256::from(9_900)),
            transaction_hash: None,
        },
        verified(9_899),
    ] {
        assert!(matches!(
            store.record_swap_bridge_outcome(operation, order.uid(), replacement),
            Err(ExecutorStoreError::InvalidRecord)
        ));
    }
    let delivered = store
        .record_swap_bridge_outcome(operation, order.uid(), verified(9_900))
        .unwrap();
    assert!(delivered.swap().unwrap().admits_attempt());
    assert!(tracked(&delivered).is_empty());
    // A final outcome stays, and the delivery doesn't turn back into a refund; recording it
    // again changes nothing.
    assert!(matches!(
        store.record_swap_bridge_outcome(operation, order.uid(), SwapBridgeOutcome::Refunding),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    let delivered = store
        .record_swap_bridge_outcome(operation, order.uid(), verified(9_900))
        .unwrap();

    // The Across deposit, found where the nonce passed `k + 1`, shows the post-hook took it.
    let after_trade =
        ExecutorNonceObservation::new(BlockNumHash::new(14, B256::repeat_byte(14)), U256::from(3));
    assert!(delivered.settled_swaps_at(after_trade.block().number));
    let refreshed = store
        .refresh_settled_swap_nonce(&delivered, after_trade)
        .unwrap();
    assert_eq!(refreshed.swap_hook_winner(U256::from(2)), None);
    let deposited = store
        .record_swap_observations(
            operation,
            order.uid(),
            SwapOrderObservations {
                post_hook_deposit: Some(traded),
                ..refreshed.swap().unwrap().orders()[0].observations()
            },
        )
        .unwrap();
    assert_eq!(
        deposited.swap_hook_winner(U256::from(2)),
        Some(B256::repeat_byte(32))
    );

    // NEAR Intents pays its deposit address and issues only the pre-hook.
    let recorded = store
        .record_swap_attempt(
            operation,
            attempt(
                SwapDelivery::Bridge(near),
                Some(near_terms.clone()),
                false,
                (7, 40, 3, after_trade),
            ),
        )
        .unwrap();
    assert_eq!(recorded.issued().len(), 4);
    assert_eq!(
        recorded.assets(),
        &[
            ExecutorAsset::Erc20(sell),
            ExecutorAsset::Erc20(Address::repeat_byte(6)),
            ExecutorAsset::Erc20(Address::repeat_byte(7))
        ]
    );
    let order = recorded.swap().unwrap().orders()[1].clone();
    // No outcome before its hand-off.
    assert!(matches!(
        store.record_swap_bridge_outcome(operation, order.uid(), SwapBridgeOutcome::NeedsAttention),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    let traded = SwapObservation {
        block: BlockNumHash::new(15, B256::repeat_byte(15)),
        transaction_hash: Some(B256::repeat_byte(52)),
    };
    let handoff = SwapBridgeHandoff {
        observation: traded,
        deposit_id: None,
    };
    // Its deposit address, not a deposit id, identifies the transfer, and it shields nothing.
    for (credit, handoff) in [
        (
            None,
            Some(SwapBridgeHandoff {
                deposit_id: Some(U256::from(77)),
                ..handoff
            }),
        ),
        (
            Some(SwapShieldObservation {
                observation: traded,
                ..credit
            }),
            Some(handoff),
        ),
    ] {
        assert!(matches!(
            store.record_swap_settlement(operation, order.uid(), traded, amounts, credit, handoff),
            Err(ExecutorStoreError::InvalidRecord)
        ));
    }
    store
        .record_swap_settlement(operation, order.uid(), traded, amounts, None, Some(handoff))
        .unwrap();
    // After a restart, the handed-off order without an outcome is polled again.
    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let reopened = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(tracked(&reopened), vec![order.uid()]);
    let attention = store
        .record_swap_bridge_outcome(operation, order.uid(), SwapBridgeOutcome::NeedsAttention)
        .unwrap();
    assert!(!attention.swap().unwrap().admits_attempt());
    // Needs attention waits for an explicit status check.
    assert!(tracked(&attention).is_empty());

    // Both providers' terms and outcomes survive the store's encoding and a restart.
    drop(store);
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let restored = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(restored, attention);
    assert_eq!(
        restored.swap().unwrap().orders()[1].bridge(),
        Some(&near_terms)
    );

    // A status check may replace NeedsAttention. A refund is final, and like an undelivered
    // reshield it keeps the account from another attempt.
    let refunding = store
        .record_swap_bridge_outcome(operation, order.uid(), SwapBridgeOutcome::Refunding)
        .unwrap();
    assert!(!refunding.swap().unwrap().admits_attempt());
    assert!(matches!(
        store.record_swap_bridge_outcome(
            operation,
            order.uid(),
            SwapBridgeOutcome::DeliveredReported {
                amount_out: Some(U256::from(16)),
                transaction_hash: None,
            },
        ),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    // Only an Across refund can turn out delivered.
    assert!(matches!(
        store.record_swap_bridge_outcome(operation, order.uid(), verified(9_900)),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn swap_records_written_before_private_bridge_delivery_decode_without_it() {
    // Match the named MessagePack shapes of the build before private Bridge delivery: no private
    // terms, deposit recipient or message hash, destination bounds, shielded flag, or
    // destination link.
    #[derive(serde::Serialize)]
    struct EarlierBridge {
        provider: BridgeProvider,
        destination_chain: u64,
        receiver: Address,
        destination_token: Address,
        surplus: BridgeSurplus,
    }
    #[derive(serde::Serialize)]
    enum EarlierDelivery {
        Bridge(EarlierBridge),
    }
    #[derive(serde::Serialize)]
    struct EarlierAcross {
        spoke_pool: Address,
        input_token: Address,
        output_token: Address,
        input_amount: U256,
        output_amount: U256,
        quote_timestamp: u32,
        fill_deadline: u32,
        exclusive_relayer: Address,
        exclusivity_parameter: u32,
    }
    #[derive(serde::Serialize)]
    enum EarlierBridgeTerms {
        Across(EarlierAcross),
    }
    #[derive(serde::Serialize)]
    enum EarlierOutcome {
        DeliveredVerified {
            block: BlockNumHash,
            transaction_hash: B256,
            output_amount: U256,
        },
    }
    #[derive(serde::Serialize)]
    struct EarlierBounds {
        sell_amount: U256,
        unshield_amount: Option<U256>,
        unshield_fee_bps: U256,
        buy_amount: U256,
        private_minimum: U256,
        shield_fee_bps: U256,
        slippage_bps: u32,
        pre_hook_gas_limit: u64,
        post_hook_gas_limit: Option<u64>,
        hook_cost: Option<U256>,
        anchors: Vec<SwapAnchorObservation>,
        destination_minimum: Option<U256>,
        gas_share_bps: Option<u16>,
        gas_estimate: Option<U256>,
        gas_allowance: Option<U256>,
        gas_price_wei: Option<u128>,
        valid_for_secs: Option<u32>,
    }
    #[derive(serde::Serialize)]
    struct EarlierApproval {
        bounds: EarlierBounds,
        price_verified: Option<bool>,
        price_acknowledged: bool,
        delivery: EarlierDelivery,
        tokens: Option<SwapApprovalTokens>,
    }
    #[derive(serde::Serialize)]
    struct SavedRecord {
        version: u32,
        derivation: ExecutorDerivationScheme,
        origin: ExecutorRecordOrigin,
        operation: ExecutorOperationId,
        index: u32,
        address: Address,
        delegate: Address,
        retired: bool,
        issued: Vec<IssuedExecutorPayload>,
        swap_approval: EarlierApproval,
    }
    fn reencode<T: serde::de::DeserializeOwned>(value: &impl serde::Serialize) -> T {
        rmp_serde::from_slice(&rmp_serde::to_vec_named(value).unwrap()).unwrap()
    }
    let receiver = Address::repeat_byte(9);
    let (sell, buy) = (Address::repeat_byte(5), Address::repeat_byte(6));
    let record: ExecutorRecord = reencode(&SavedRecord {
        version: 1,
        derivation: ExecutorDerivationScheme::Railgun7702V1,
        origin: ExecutorRecordOrigin::Reserved,
        operation: ExecutorOperationId::random().unwrap(),
        index: 7,
        address: Address::repeat_byte(2),
        delegate: Address::repeat_byte(1),
        retired: false,
        issued: Vec::new(),
        swap_approval: EarlierApproval {
            bounds: EarlierBounds {
                sell_amount: U256::from(9_975),
                unshield_amount: Some(U256::from(10_000)),
                unshield_fee_bps: U256::from(25),
                buy_amount: U256::from(9_975),
                private_minimum: U256::from(9_975),
                shield_fee_bps: U256::ZERO,
                slippage_bps: 50,
                pre_hook_gas_limit: 900_000,
                post_hook_gas_limit: Some(400_000),
                hook_cost: Some(U256::ZERO),
                anchors: Vec::new(),
                destination_minimum: Some(U256::from(9_900)),
                gas_share_bps: Some(2_500),
                gas_estimate: Some(U256::from(40)),
                gas_allowance: Some(U256::from(10)),
                gas_price_wei: Some(7),
                valid_for_secs: Some(1_800),
            },
            price_verified: Some(true),
            price_acknowledged: false,
            delivery: EarlierDelivery::Bridge(EarlierBridge {
                provider: BridgeProvider::Across,
                destination_chain: 137,
                receiver,
                destination_token: Address::repeat_byte(10),
                surplus: BridgeSurplus::KeepInAccount,
            }),
            tokens: Some(SwapApprovalTokens { sell, buy }),
        },
    });
    assert_eq!(
        (record.swap_destination(), record.destination_operation()),
        (None, None)
    );
    let approval = record.swap_approval().unwrap();
    let delivery = BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: 137,
        receiver,
        destination_token: Address::repeat_byte(10),
        surplus: BridgeSurplus::KeepInAccount,
        private: None,
    };
    assert_eq!(approval.delivery, SwapDelivery::Bridge(delivery));
    assert_eq!(
        (
            approval.bounds.valid_for_secs,
            approval.bounds.destination_shield_fee_bps,
            approval.bounds.delivery_allowance,
            approval.bounds.destination_setup_fee
        ),
        (Some(1_800), None, None, None)
    );
    let BridgeOrderTerms::Across(terms) = reencode(&EarlierBridgeTerms::Across(EarlierAcross {
        spoke_pool: Address::repeat_byte(11),
        input_token: buy,
        output_token: Address::repeat_byte(10),
        input_amount: U256::from(9_975),
        output_amount: U256::from(9_900),
        quote_timestamp: 1_700_000_000,
        fill_deadline: 1_700_021_600,
        exclusive_relayer: Address::ZERO,
        exclusivity_parameter: 0,
    })) else {
        panic!("Across terms decode as Across terms");
    };
    assert_eq!((terms.recipient, terms.message_hash), (None, None));
    // The deposit of such a record pays the receiver with an empty message.
    assert_eq!(
        (
            terms.deposit_recipient(delivery),
            terms.deposit_message_hash()
        ),
        (receiver, B256::ZERO)
    );
    let block = BlockNumHash::new(500, B256::repeat_byte(60));
    assert_eq!(
        reencode::<SwapBridgeOutcome>(&EarlierOutcome::DeliveredVerified {
            block,
            transaction_hash: B256::repeat_byte(61),
            output_amount: U256::from(9_900),
        }),
        SwapBridgeOutcome::DeliveredVerified {
            block,
            transaction_hash: B256::repeat_byte(61),
            output_amount: U256::from(9_900),
            shielded: false,
        }
    );

    // A private Across approval and its order terms keep the new fields.
    let mut private = approval.clone();
    let private_delivery = BridgeDelivery {
        private: Some(BridgePrivateDelivery {
            on_shield_failure: BridgeShieldFailure::KeepOnDestination,
        }),
        ..delivery
    };
    private.delivery = SwapDelivery::Bridge(private_delivery);
    private.bounds.destination_shield_fee_bps = Some(U256::from(25));
    private.bounds.delivery_allowance = Some(U256::from(120));
    private.bounds.destination_setup_fee = Some(U256::from(3));
    assert_eq!(reencode::<SwapApproval>(&private), private);
    let private_terms = AcrossOrderTerms {
        recipient: Some(Address::repeat_byte(12)),
        message_hash: Some(B256::repeat_byte(13)),
        ..terms
    };
    assert_eq!(reencode::<AcrossOrderTerms>(&private_terms), private_terms);
    assert_eq!(
        (
            private_terms.deposit_recipient(private_delivery),
            private_terms.deposit_message_hash()
        ),
        (Address::repeat_byte(12), B256::repeat_byte(13))
    );
}

/// Bind the reserved account `operation` to `executor` and let its delegation-only setup win
/// nonce 0. Returns the reconciled observation at nonce 1.
fn set_up(
    store: &ExecutorStore,
    operation: ExecutorOperationId,
    delegate: Address,
    executor: Address,
) -> ExecutorNonceObservation {
    store.bind_address(operation, executor).unwrap();
    let before_setup =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.record_account_read(operation, before_setup).unwrap();
    store
        .record_issued(
            operation,
            hook(
                ExecutorPayloadPurpose::Operation,
                0,
                3,
                delegate,
                before_setup,
                Vec::new(),
            ),
        )
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.record_account_read(operation, observed).unwrap();
    observed
}

/// The delegate of `chain_id`'s accepted executor profile. Only an account delegated to it
/// takes another swap use.
fn accepted_delegate(chain_id: u64) -> Address {
    crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
        .unwrap()
        .get(chain_id)
        .unwrap()
        .accepted_executor_profile()
        .unwrap()
        .delegate()
}

fn private_bridge_delivery(
    receiver: Address,
    token: Address,
    failure: BridgeShieldFailure,
) -> BridgeDelivery {
    BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: 137,
        receiver,
        destination_token: token,
        surplus: BridgeSurplus::KeepInAccount,
        private: Some(BridgePrivateDelivery {
            on_shield_failure: failure,
        }),
    }
}

/// A private Across order of the swap `origin`'s first use, signed by its account `executor`
/// at `observed`, that delivers `token` on chain 137 to the destination account `receiver`.
fn private_across_attempt(
    origin: ExecutorOperationId,
    executor: Address,
    delegate: Address,
    observed: ExecutorNonceObservation,
    receiver: Address,
    token: Address,
) -> SwapAttempt {
    let inputs = swap_inputs();
    let buy = Address::repeat_byte(6);
    SwapAttempt {
        use_id: SwapUseId::first(origin),
        terms: SwapTerms::new(
            Address::repeat_byte(5),
            buy,
            SwapRecipient::new(U256::from(7), [8; 32]),
            B256::repeat_byte(3),
        ),
        proof: SwapProof::new(B256::repeat_byte(20), inputs.clone()),
        uid: OrderUid::new(B256::repeat_byte(30), executor, 1_000),
        submission: None,
        delivery: SwapDelivery::Bridge(private_bridge_delivery(
            receiver,
            token,
            BridgeShieldFailure::RefundOnOrigin,
        )),
        bounds: SwapApprovedBounds {
            destination_minimum: Some(U256::from(9_900)),
            ..swap_bounds()
        },
        invalidates: None,
        pre_hook: hook(
            ExecutorPayloadPurpose::SwapPreHook,
            1,
            31,
            delegate,
            observed,
            inputs,
        ),
        post_hook: Some(hook(
            ExecutorPayloadPurpose::SwapPostHook,
            2,
            32,
            delegate,
            observed,
            Vec::new(),
        )),
        bridge: Some(BridgeOrderTerms::Across(AcrossOrderTerms {
            spoke_pool: Address::repeat_byte(11),
            input_token: buy,
            output_token: token,
            input_amount: U256::from(9_975),
            output_amount: U256::from(9_900),
            quote_timestamp: 1_700_000_000,
            fill_deadline: 1_700_021_600,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
            recipient: Some(Address::repeat_byte(12)),
            message_hash: Some(B256::repeat_byte(13)),
        })),
    }
}

/// Record the order `uid`'s trade and Across hand-off, then its fill on the destination chain
/// in `transaction_hash` at `block`, which ran the destination account's shield.
fn deliver_shielded(
    store: &ExecutorStore,
    operation: ExecutorOperationId,
    uid: OrderUid,
    block: BlockNumHash,
    transaction_hash: B256,
) {
    let traded = SwapObservation {
        block: BlockNumHash::new(13, B256::repeat_byte(13)),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    store
        .record_swap_settlement(
            operation,
            uid,
            traded,
            SwapTradeAmounts {
                sell_amount: U256::from(9_975),
                buy_amount: U256::from(10_000),
                fee_amount: U256::ZERO,
                settlement_gas_used: None,
                settlement_effective_gas_price: None,
                executed_fee: None,
                executed_fee_token: None,
            },
            None,
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: Some(U256::from(77)),
            }),
        )
        .unwrap();
    store
        .record_swap_bridge_outcome(
            operation,
            uid,
            SwapBridgeOutcome::DeliveredVerified {
                block,
                transaction_hash,
                output_amount: U256::from(9_900),
                shielded: true,
            },
        )
        .unwrap();
}

#[test]
fn swap_destination_records_link_to_their_origin_and_unreferenced_ones_retire() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let origin_store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let delegate = Address::repeat_byte(1);
    let token = Address::repeat_byte(10);
    let [origin, linked, unreferenced, issued, missing] =
        std::array::from_fn(|_| ExecutorOperationId::random().unwrap());
    let serves = |origin_operation| SwapDestinationRecord {
        origin_chain: 1,
        origin_operation,
        destination_token: token,
        outcome: None,
    };

    // The destination is reserved first, then the origin with the link to it.
    let reserved = store
        .reserve_swap_destination(linked, delegate, serves(origin))
        .unwrap();
    assert_eq!(reserved.swap_destination(), Some(serves(origin)));
    assert_eq!(reserved.assets(), &[ExecutorAsset::Erc20(token)]);
    let reserve_origin = |destination| {
        origin_store.reserve_with_swap_approval(
            origin,
            delegate,
            Some("Private swap"),
            &[],
            None,
            destination,
        )
    };
    assert_eq!(
        reserve_origin(Some(linked))
            .unwrap()
            .destination_operation(),
        Some(linked)
    );
    // A reservation is returned again only for the same link, and a destination is on
    // another chain than its swap.
    assert!(matches!(
        reserve_origin(None),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    assert!(matches!(
        store.reserve_swap_destination(linked, delegate, serves(missing)),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    assert!(matches!(
        origin_store.reserve_swap_destination(missing, delegate, serves(origin)),
        Err(ExecutorStoreError::OperationMismatch)
    ));

    // A crash between the two reservations leaves a destination that no swap references. It
    // retires on load unless it already issued a payload. Live reconciliation cannot tell
    // these apart from reservations that are still being prepared.
    for operation in [unreferenced, issued] {
        store
            .reserve_swap_destination(operation, delegate, serves(missing))
            .unwrap();
    }
    set_up(&store, issued, delegate, Address::repeat_byte(3));
    assert!(!store.reconcile_swap_destinations().unwrap());
    let retired = |operation| {
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
            .is_retired()
    };
    assert_eq!(
        (retired(linked), retired(unreferenced), retired(issued)),
        (false, false, false)
    );
    assert!(store.reconcile_swap_destinations_on_load().unwrap());
    assert_eq!(
        (retired(linked), retired(unreferenced), retired(issued)),
        (false, true, false)
    );
    assert!(!store.reconcile_swap_destinations_on_load().unwrap());
    drop(origin_store);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn destination_shield_payloads_are_outstanding_until_their_nonce_is_consumed() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let origin_store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    // The destination's delegate is its chain's accepted one, so only its unresolved work keeps
    // it from another swap.
    let delegate = accepted_delegate(137);
    let (executor, destination_executor) = (Address::repeat_byte(2), Address::repeat_byte(9));
    let (sell, buy, token) = (
        Address::repeat_byte(5),
        Address::repeat_byte(6),
        Address::repeat_byte(10),
    );
    let (origin, destination) = (
        ExecutorOperationId::random().unwrap(),
        ExecutorOperationId::random().unwrap(),
    );
    store
        .reserve_swap_destination(
            destination,
            delegate,
            SwapDestinationRecord {
                origin_chain: 1,
                origin_operation: origin,
                destination_token: token,
                outcome: None,
            },
        )
        .unwrap();
    origin_store
        .reserve_with_swap_approval(
            origin,
            delegate,
            Some("Private swap"),
            &[ExecutorAsset::Erc20(sell)],
            None,
            Some(destination),
        )
        .unwrap();
    let observed = set_up(&store, destination, delegate, destination_executor);
    assert_eq!(set_up(&origin_store, origin, delegate, executor), observed);

    // The shield is signed at the destination account's nonce after its setup. Only a
    // destination account takes one.
    let shield = |hash: u8| {
        hook(
            ExecutorPayloadPurpose::SwapDestinationShield,
            1,
            hash,
            delegate,
            observed,
            Vec::new(),
        )
    };
    assert!(matches!(
        origin_store.record_issued(origin, shield(70)),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    // Nor does a destination whose origin swap names another account for this use.
    let (stray, stray_executor) = (
        ExecutorOperationId::random().unwrap(),
        Address::repeat_byte(8),
    );
    store
        .reserve_swap_destination(
            stray,
            delegate,
            SwapDestinationRecord {
                origin_chain: 1,
                origin_operation: origin,
                destination_token: token,
                outcome: None,
            },
        )
        .unwrap();
    assert_eq!(set_up(&store, stray, delegate, stray_executor), observed);
    assert!(matches!(
        store.record_issued(stray, shield(70)),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    // A shield signed for another use than the one that claims the account is refused.
    assert!(matches!(
        store.record_swap_destination_shield(destination, SwapUseId::random().unwrap(), shield(70)),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    let issued = store.record_issued(destination, shield(70)).unwrap();
    // Recovery admission asks the first two: whether a payload competes at the recovery's
    // nonce, and whether its review warns of one.
    let outstanding = |record: &ExecutorRecord| {
        (
            record.is_outstanding_at(&record.issued()[1], U256::ONE),
            record.has_competing_payloads(),
            record.has_unresolved_issued_work(),
        )
    };
    assert_eq!(outstanding(&issued), (true, true, true));
    // A retry signs again at the same nonce, and both stay recorded.
    let issued = store.record_issued(destination, shield(71)).unwrap();
    assert_eq!((issued.issued().len(), issued.is_retired()), (3, false));

    // The origin's private Across order names the destination account as its receiver.
    let delivery = private_bridge_delivery(
        destination_executor,
        token,
        BridgeShieldFailure::KeepOnDestination,
    );
    let attempt = SwapAttempt {
        delivery: SwapDelivery::Bridge(delivery),
        bounds: SwapApprovedBounds {
            buy_amount: U256::from(9_975),
            shield_fee_bps: U256::ZERO,
            post_hook_gas_limit: Some(400_000),
            destination_minimum: Some(U256::from(9_900)),
            destination_shield_fee_bps: Some(U256::from(25)),
            delivery_allowance: Some(U256::from(120)),
            destination_setup_fee: Some(U256::from(3)),
            ..swap_bounds()
        },
        ..private_across_attempt(
            origin,
            executor,
            delegate,
            observed,
            destination_executor,
            token,
        )
    };
    let uid = attempt.uid;
    // The order is refused unless the account this use names serves it: another account of
    // the wallet, or another token than the destination was reserved for, is not the link.
    for mislinked in [
        BridgeDelivery {
            receiver: stray_executor,
            ..delivery
        },
        BridgeDelivery {
            destination_token: buy,
            ..delivery
        },
    ] {
        assert!(matches!(
            origin_store.record_swap_attempt(
                origin,
                SwapAttempt {
                    delivery: SwapDelivery::Bridge(mislinked),
                    ..attempt.clone()
                },
            ),
            Err(ExecutorStoreError::OperationMismatch)
        ));
    }
    origin_store
        .record_swap_attempt(origin, attempt.clone())
        .unwrap();
    let traded = SwapObservation {
        block: BlockNumHash::new(13, B256::repeat_byte(13)),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    origin_store
        .record_swap_settlement(
            origin,
            uid,
            traded,
            SwapTradeAmounts {
                sell_amount: U256::from(9_975),
                buy_amount: U256::from(10_000),
                fee_amount: U256::ZERO,
                settlement_gas_used: None,
                settlement_effective_gas_price: None,
                executed_fee: None,
                executed_fee_token: None,
            },
            None,
            Some(SwapBridgeHandoff {
                observation: traded,
                deposit_id: Some(U256::from(77)),
            }),
        )
        .unwrap();
    let destination_record = || {
        assert!(store.reconcile_swap_destinations().unwrap());
        store.records().unwrap().remove(0)
    };
    let outcome = |record: &ExecutorRecord| record.swap_destination().unwrap().outcome;

    // An expiry/refund report does not revoke the published shield signature. Public signing
    // and recovery must still treat it as outstanding at the unchanged destination nonce.
    origin_store
        .record_swap_bridge_outcome(origin, uid, SwapBridgeOutcome::Refunding)
        .unwrap();
    let unfilled = destination_record();
    assert_eq!(outcome(&unfilled), Some(SwapDestinationOutcome::Unfilled));
    assert_eq!(outstanding(&unfilled), (true, true, true));
    // Nor does the report free the account for another swap while its shield can execute.
    let unrelated = || {
        origin_store.claim_swap_pair(SwapPairClaim {
            id: SwapUseId::random().unwrap(),
            source: SwapAccountChoice::New(ExecutorOperationId::random().unwrap()),
            delegate,
            purpose_summary: None,
            assets: Vec::new(),
            approval: approval(5, 6),
            destination: Some(SwapDestinationClaim {
                chain_id: 137,
                account: SwapAccountChoice::Existing(destination),
                delegate,
                destination_token: token,
            }),
        })
    };
    assert!(matches!(
        unrelated(),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    // A retry's newly signed shield can be funded again.
    let reissued = store.record_issued(destination, shield(72)).unwrap();
    assert_eq!(outcome(&reissued), None);
    assert_eq!(outstanding(&reissued), (true, true, true));
    assert_eq!(
        outcome(&destination_record()),
        Some(SwapDestinationOutcome::Unfilled)
    );

    // Across may fill a deposit it reported expired. A private delivery's fill either shields
    // or leaves the token in the destination account, where the shield can still run.
    let block = BlockNumHash::new(500, B256::repeat_byte(60));
    let transaction_hash = B256::repeat_byte(61);
    assert!(matches!(
        origin_store.record_swap_bridge_outcome(
            origin,
            uid,
            SwapBridgeOutcome::DeliveredVerified {
                block,
                transaction_hash,
                output_amount: U256::from(9_900),
                shielded: false,
            },
        ),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    let held = origin_store
        .record_swap_bridge_outcome(
            origin,
            uid,
            SwapBridgeOutcome::HeldOnDestination {
                block,
                transaction_hash,
                amount: U256::from(9_900),
            },
        )
        .unwrap();
    assert!(!held.swap().unwrap().admits_attempt());
    let held = destination_record();
    assert_eq!(
        outcome(&held),
        Some(SwapDestinationOutcome::Held {
            block,
            transaction_hash,
        })
    );
    assert_eq!(outstanding(&held), (true, true, true));

    // Recovery and Public registration stop the swap on both chains before the account is
    // handed off. A retry's order and another shield are then refused, while the published
    // shields stay outstanding.
    let retry = SwapAttempt {
        uid: OrderUid::new(B256::repeat_byte(40), executor, 1_000),
        pre_hook: hook(
            ExecutorPayloadPurpose::SwapPreHook,
            1,
            33,
            delegate,
            observed,
            attempt.proof.inputs().to_vec(),
        ),
        post_hook: Some(hook(
            ExecutorPayloadPurpose::SwapPostHook,
            2,
            34,
            delegate,
            observed,
            Vec::new(),
        )),
        ..attempt
    };
    assert!(matches!(
        origin_store.record_swap_attempt(origin, retry.clone()),
        Err(ExecutorStoreError::SwapAttemptOutstanding)
    ));
    let use_id = SwapUseId::first(origin);
    let stopped = store.stop_swap_use(destination).unwrap();
    assert!(stopped.swap_use(use_id).unwrap().is_stopped());
    assert!(
        origin_store.records().unwrap()[0]
            .swap_use(use_id)
            .unwrap()
            .is_stopped()
    );
    assert!(matches!(
        origin_store.record_swap_attempt(origin, retry),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    for refused in [
        store.record_issued(destination, shield(73)),
        store.record_swap_destination_shield(destination, use_id, shield(73)),
    ] {
        assert!(matches!(
            refused,
            Err(ExecutorStoreError::OperationMismatch)
        ));
    }
    assert_eq!(stopped.issued(), held.issued());
    assert_eq!(outstanding(&stopped), (true, true, true));
    assert!(matches!(
        unrelated(),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    // Canonical nonce consumption, rather than a provider outcome, resolves the signature.
    let consumed = store
        .record_account_read(
            destination,
            ExecutorNonceObservation::new(block, U256::from(2)),
        )
        .unwrap();
    assert!(!consumed.is_outstanding_at(&consumed.issued()[1], U256::from(2)));
    assert!(!consumed.has_competing_payloads());
    assert!(!consumed.has_unresolved_issued_work());
    // No signature of the account can execute any more. That doesn't resolve the delivery:
    // the fill left the token in the account, so the account is neither settled nor free for
    // another swap, whatever the nonce. A shielded delivery admits it: see
    // `competing_swap_pair_claims_leave_one_complete_pair_and_nothing_of_the_other`.
    assert!(!consumed.settled_swaps_at(block.number));
    assert!(matches!(
        store.refresh_settled_swap_nonce(
            &consumed,
            ExecutorNonceObservation::new(block, U256::from(3)),
        ),
        Err(ExecutorStoreError::OutstandingNonce)
    ));
    assert!(matches!(
        unrelated(),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    assert_eq!(store.records().unwrap().remove(0), consumed);
    drop(origin_store);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

fn swap_bounds() -> SwapApprovedBounds {
    SwapApprovedBounds {
        sell_amount: U256::from(9_975),
        unshield_amount: Some(U256::from(10_000)),
        unshield_fee_bps: U256::from(25),
        buy_amount: U256::from(9_999),
        private_minimum: U256::from(9_975),
        shield_fee_bps: U256::from(25),
        slippage_bps: 50,
        pre_hook_gas_limit: 900_000,
        post_hook_gas_limit: Some(300_000),
        hook_cost: Some(U256::ZERO),
        anchors: Vec::new(),
        destination_minimum: None,
        gas_share_bps: None,
        gas_estimate: None,
        gas_allowance: None,
        gas_price_wei: None,
        valid_for_secs: None,
        destination_shield_fee_bps: None,
        delivery_allowance: None,
        destination_setup_fee: None,
        source_setup_fee: None,
    }
}

fn swap_inputs() -> Vec<ExecutorInputIdentity> {
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(U256::ONE, Address::ZERO, U256::from(9), [7; 16]),
        2,
        3,
        UtxoSource {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    vec![ExecutorInputIdentity::from_utxo(&input)]
}

fn approval(sell: u8, buy: u8) -> SwapApproval {
    SwapApproval {
        bounds: swap_bounds(),
        price_verified: Some(true),
        price_acknowledged: false,
        delivery: SwapDelivery::Reshield,
        tokens: Some(SwapApprovalTokens {
            sell: Address::repeat_byte(sell),
            buy: Address::repeat_byte(buy),
        }),
        accounts: None,
    }
}

/// An account with a completed source swap can still be a source after recovery retires it.
fn settled_source_account(
    store: &ExecutorStore,
    operation: ExecutorOperationId,
    delegate: Address,
    executor: Address,
) -> ExecutorNonceObservation {
    store
        .reserve_with_swap_approval(operation, delegate, None, &[], Some(approval(5, 6)), None)
        .unwrap();
    let observed = set_up(store, operation, delegate, executor);
    // Each fixture account spends a different note. Executed pre-hooks keep their inputs
    // reserved until private sync observes the spend, so another account cannot reuse them.
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            U256::ONE,
            Address::ZERO,
            U256::from(9),
            [executor.as_slice()[0]; 16],
        ),
        2,
        u64::from(executor.as_slice()[0]),
        UtxoSource {
            tx_hash: B256::repeat_byte(9),
            block_number: 1,
            block_timestamp: 1,
        },
        UtxoCommitmentKind::Transact,
    );
    let inputs = vec![ExecutorInputIdentity::from_utxo(&input)];
    let uid = OrderUid::new(B256::repeat_byte(30), executor, 1_000);
    store
        .record_swap_attempt(
            operation,
            SwapAttempt {
                use_id: SwapUseId::first(operation),
                terms: SwapTerms::new(
                    Address::repeat_byte(5),
                    Address::repeat_byte(6),
                    SwapRecipient::new(U256::from(7), [8; 32]),
                    B256::repeat_byte(3),
                ),
                proof: SwapProof::new(B256::repeat_byte(20), inputs.clone()),
                uid,
                submission: None,
                delivery: SwapDelivery::Reshield,
                bounds: swap_bounds(),
                invalidates: None,
                pre_hook: hook(
                    ExecutorPayloadPurpose::SwapPreHook,
                    1,
                    31,
                    delegate,
                    observed,
                    inputs,
                ),
                post_hook: Some(hook(
                    ExecutorPayloadPurpose::SwapPostHook,
                    2,
                    32,
                    delegate,
                    observed,
                    Vec::new(),
                )),
                bridge: None,
            },
        )
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(14, B256::repeat_byte(14)), U256::from(3));
    store.record_account_read(operation, observed).unwrap();
    let delivered = SwapObservation {
        block: observed.block(),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    store
        .record_swap_observations(
            operation,
            uid,
            SwapOrderObservations {
                pre_hook_executed: Some(delivered),
                traded: Some(delivered),
                delivered: Some(delivered),
                shielded: Some(SwapShieldObservation {
                    observation: delivered,
                    private_amount: U256::from(9_975),
                    fee: None,
                }),
                ..Default::default()
            },
        )
        .unwrap();
    observed
}

#[test]
fn a_later_swap_use_leaves_the_earlier_use_and_its_order_unchanged() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let delegate = accepted_delegate(1);
    let executor = Address::repeat_byte(2);
    store
        .reserve_with_swap_approval(
            operation,
            delegate,
            Some("Private swap"),
            &[],
            Some(approval(5, 6)),
            None,
        )
        .unwrap();
    let observed = set_up(&store, operation, delegate, executor);
    let inputs = swap_inputs();
    let attempt = |use_id, pair: (u8, u8), uid: u8, nonce: u64, observed| SwapAttempt {
        use_id,
        terms: SwapTerms::new(
            Address::repeat_byte(pair.0),
            Address::repeat_byte(pair.1),
            SwapRecipient::new(U256::from(7), [8; 32]),
            B256::repeat_byte(3),
        ),
        proof: SwapProof::new(B256::repeat_byte(20), inputs.clone()),
        uid: OrderUid::new(B256::repeat_byte(uid), executor, 1_000),
        submission: None,
        delivery: SwapDelivery::Reshield,
        bounds: swap_bounds(),
        invalidates: None,
        pre_hook: hook(
            ExecutorPayloadPurpose::SwapPreHook,
            nonce,
            uid + 1,
            delegate,
            observed,
            inputs.clone(),
        ),
        post_hook: Some(hook(
            ExecutorPayloadPurpose::SwapPostHook,
            nonce + 1,
            uid + 2,
            delegate,
            observed,
            Vec::new(),
        )),
        bridge: None,
    };
    let first = attempt(SwapUseId::first(operation), (5, 6), 30, 1, observed);
    let (first_uid, first_terms) = (first.uid, first.terms);
    store.record_swap_attempt(operation, first).unwrap();
    // A use with an order ends through its order's own cancellation.
    assert!(matches!(
        store.cancel_swap_use(operation, SwapUseId::first(operation)),
        Err(ExecutorStoreError::OperationMismatch)
    ));

    // The account stays with its swap while that swap's order can still trade.
    let second = SwapUseId::random().unwrap();
    let claim = SwapPairClaim {
        id: second,
        source: SwapAccountChoice::Existing(operation),
        delegate,
        purpose_summary: None,
        assets: Vec::new(),
        approval: approval(7, 8),
        destination: None,
    };
    assert!(matches!(
        store.claim_swap_pair(claim.clone()),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    let after_fill =
        ExecutorNonceObservation::new(BlockNumHash::new(14, B256::repeat_byte(14)), U256::from(3));
    store.record_account_read(operation, after_fill).unwrap();
    let delivered = SwapObservation {
        block: after_fill.block(),
        transaction_hash: Some(B256::repeat_byte(50)),
    };
    let settled = store
        .record_swap_observations(
            operation,
            first_uid,
            SwapOrderObservations {
                pre_hook_executed: Some(delivered),
                traded: Some(delivered),
                delivered: Some(delivered),
                shielded: Some(SwapShieldObservation {
                    observation: delivered,
                    private_amount: U256::from(9_975),
                    fee: None,
                }),
                ..Default::default()
            },
        )
        .unwrap();

    // Another swap on the settled account is a new use with its own approval and orders. The
    // account now signs for that use only.
    store.claim_swap_pair(claim).unwrap();
    let earlier_use = attempt(SwapUseId::first(operation), (7, 8), 40, 3, after_fill);
    assert!(matches!(
        store.record_swap_attempt(operation, earlier_use),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    let recorded = store
        .record_swap_attempt(operation, attempt(second, (7, 8), 40, 3, after_fill))
        .unwrap();
    let swap = recorded.swap().unwrap();
    let [first_order, second_order] = swap.orders() else {
        panic!("one order per use");
    };
    assert_eq!(first_order, &settled.swap().unwrap().orders()[0]);
    assert_eq!(swap.order_terms(first_order), &first_terms);
    assert_eq!(&recorded.issued()[..3], settled.issued());
    assert_eq!(
        (first_order.use_id(), second_order.use_id()),
        (Some(SwapUseId::first(operation)), Some(second))
    );
    assert_eq!(recorded.active_swap_use(), Some(second));
    assert_eq!(recorded.swap_approval(), Some(&approval(7, 8)));
    let [first_use, second_use] = recorded.swap_uses() else {
        panic!("one use per swap");
    };
    assert!(matches!(
        first_use.role(),
        SwapUseRole::Source { approval: Some(known), .. } if **known == approval(5, 6)
    ));
    assert_eq!((first_use.is_fresh(), second_use.is_fresh()), (true, false));
    assert_eq!(store.records().unwrap(), vec![recorded]);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn competing_swap_pair_claims_leave_one_complete_pair_and_nothing_of_the_other() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let other = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let destination_store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let (delegate, destination_delegate) = (accepted_delegate(1), accepted_delegate(137));
    let token = Address::repeat_byte(10);
    let [earlier, existing, new, shared] =
        std::array::from_fn(|_| ExecutorOperationId::random().unwrap());

    // The shared destination served an earlier swap, and its shield's nonce is consumed.
    destination_store
        .reserve_swap_destination(
            shared,
            destination_delegate,
            SwapDestinationRecord {
                origin_chain: 1,
                origin_operation: earlier,
                destination_token: token,
                outcome: None,
            },
        )
        .unwrap();
    store
        .reserve_with_swap_approval(
            earlier,
            delegate,
            Some("Private swap"),
            &[],
            None,
            Some(shared),
        )
        .unwrap();
    let earlier_observed = set_up(&store, earlier, delegate, Address::repeat_byte(2));
    store
        .reserve(existing, delegate, Some("Private swap"), &[])
        .unwrap();
    set_up(&store, existing, delegate, Address::repeat_byte(7));
    let observed = set_up(
        &destination_store,
        shared,
        destination_delegate,
        Address::repeat_byte(9),
    );
    destination_store
        .record_issued(
            shared,
            hook(
                ExecutorPayloadPurpose::SwapDestinationShield,
                1,
                70,
                destination_delegate,
                observed,
                Vec::new(),
            ),
        )
        .unwrap();
    destination_store
        .record_account_read(
            shared,
            ExecutorNonceObservation::new(
                BlockNumHash::new(14, B256::repeat_byte(14)),
                U256::from(2),
            ),
        )
        .unwrap();

    // One preparation reuses a source account and the other allocates one. Both want the
    // shared destination.
    let claim = |source| SwapPairClaim {
        id: SwapUseId::random().unwrap(),
        source,
        delegate,
        purpose_summary: Some("Private swap".to_owned()),
        assets: Vec::new(),
        approval: approval(5, 6),
        destination: Some(SwapDestinationClaim {
            chain_id: 137,
            account: SwapAccountChoice::Existing(shared),
            delegate: destination_delegate,
            destination_token: token,
        }),
    };
    // The consumed nonce doesn't say which payload ran or whether the bridge delivered. Until
    // the earlier swap's fill is verified as shielded, the destination is neither settled nor
    // free, and a refused claim leaves nothing.
    let order = private_across_attempt(
        earlier,
        Address::repeat_byte(2),
        delegate,
        earlier_observed,
        Address::repeat_byte(9),
        token,
    );
    let (uid, private_delivery) = (order.uid, order.delivery);
    store.record_swap_attempt(earlier, order).unwrap();
    assert!(!destination_store.records().unwrap()[0].settled_swaps_at(14));
    assert!(matches!(
        store.claim_swap_pair(claim(SwapAccountChoice::Existing(existing))),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    deliver_shielded(
        &store,
        earlier,
        uid,
        BlockNumHash::new(13, B256::repeat_byte(60)),
        B256::repeat_byte(61),
    );
    assert!(destination_store.reconcile_swap_destinations().unwrap());
    assert!(destination_store.records().unwrap()[0].settled_swaps_at(14));

    // The same pair takes a second swap. The earlier swap's fill went to the same receiver from
    // the same origin account, and reconciling it again leaves the new use as claimed: active,
    // not stopped, and without an outcome. A fresh handle reads the same after a restart.
    let again = SwapPairClaim {
        source: SwapAccountChoice::Existing(earlier),
        approval: SwapApproval {
            delivery: private_delivery,
            ..approval(5, 6)
        },
        ..claim(SwapAccountChoice::Existing(earlier))
    };
    let reclaimed = store
        .claim_swap_pair(again.clone())
        .unwrap()
        .destination
        .unwrap();
    let delivered = Some(SwapDestinationOutcome::Shielded {
        block: BlockNumHash::new(13, B256::repeat_byte(60)),
        transaction_hash: B256::repeat_byte(61),
    });
    let restarted = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    assert!(!destination_store.reconcile_swap_destinations().unwrap());
    assert!(!restarted.reconcile_swap_destinations_on_load().unwrap());
    let record = restarted.records().unwrap().remove(0);
    assert_eq!(record, reclaimed);
    assert_eq!(record.active_swap_use(), Some(again.id));
    assert!(!record.swap_use(again.id).unwrap().is_stopped());
    assert_eq!(
        (
            record
                .swap_destination_use(SwapUseId::first(earlier))
                .unwrap()
                .outcome,
            record.swap_destination_use(again.id).unwrap().outcome,
        ),
        (delivered, None)
    );
    drop(restarted);
    // Cancelled before it signed anything, the second swap frees both accounts.
    assert_eq!(
        store.cancel_swap_use(earlier, again.id).unwrap(),
        SwapUseCancellation {
            source: SwapUseRelease::Released,
            destination: Some(SwapUseRelease::Released),
        }
    );
    let reusing = claim(SwapAccountChoice::Existing(existing));
    let allocating = claim(SwapAccountChoice::New(new));
    let state = || {
        (
            store.records().unwrap(),
            destination_store.records().unwrap(),
            store.next_index().unwrap(),
        )
    };
    let (sources_before, destinations_before, next_index_before) = state();
    let barrier = std::sync::Barrier::new(2);
    let (reused, allocated) = std::thread::scope(|scope| {
        let reused = scope.spawn(|| {
            barrier.wait();
            store.claim_swap_pair(reusing.clone())
        });
        barrier.wait();
        let allocated = other.claim_swap_pair(allocating.clone());
        (reused.join().unwrap(), allocated)
    });
    let (won, winner) = match (reused, allocated) {
        (Ok(pair), Err(ExecutorStoreError::SwapUseActive)) => (pair, reusing),
        (Err(ExecutorStoreError::SwapUseActive), Ok(pair)) => (pair, allocating),
        both => panic!("one claim wins and the other finds the destination taken: {both:?}"),
    };

    // The winner's accounts hold its use and name each other.
    let destination = won.destination.clone().unwrap();
    assert_eq!(
        (won.source.active_swap_use(), destination.active_swap_use()),
        (Some(winner.id), Some(winner.id))
    );
    assert_eq!(won.source.operation(), winner.source.operation());
    assert_eq!(won.source.destination_operation(), Some(shared));
    assert_eq!(
        destination.swap_destination(),
        Some(SwapDestinationRecord {
            origin_chain: 1,
            origin_operation: winner.source.operation(),
            destination_token: token,
            outcome: None,
        })
    );
    // Nothing else changed: the loser's source keeps its record, or has none and took no index,
    // and no account issued a payload.
    let allocated = winner.source == SwapAccountChoice::New(new);
    let mut sources = sources_before;
    if allocated {
        sources.push(won.source.clone());
    } else {
        *sources
            .iter_mut()
            .find(|record| record.operation() == existing)
            .unwrap() = won.source.clone();
    }
    let after = (
        sources,
        vec![destination.clone()],
        next_index_before + u32::from(allocated),
    );
    assert_eq!(state(), after);
    assert_eq!(won.source.issued().len(), usize::from(!allocated));
    assert_eq!(destination.issued(), destinations_before[0].issued());

    // Repeating the winner's claim returns its pair and writes nothing.
    assert_eq!(store.claim_swap_pair(winner).unwrap(), won);
    assert_eq!(state(), after);
    drop(other);
    drop(store);
    drop(destination_store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_prepared_swap_approval_preserves_its_use_and_linked_destination() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let destination_store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let (delegate, destination_delegate) = (accepted_delegate(1), accepted_delegate(137));
    let [source, first_destination, next_destination] =
        std::array::from_fn(|_| ExecutorOperationId::random().unwrap());
    store.reserve(source, delegate, None, &[]).unwrap();
    set_up(&store, source, delegate, Address::repeat_byte(2));
    for (operation, address) in [
        (first_destination, Address::repeat_byte(3)),
        (next_destination, Address::repeat_byte(4)),
    ] {
        destination_store
            .reserve(operation, destination_delegate, None, &[])
            .unwrap();
        set_up(&destination_store, operation, destination_delegate, address);
    }
    let claim = |destination, receiver| {
        let mut approval = approval(5, 6);
        approval.delivery = SwapDelivery::Bridge(private_bridge_delivery(
            receiver,
            Address::repeat_byte(10),
            BridgeShieldFailure::KeepOnDestination,
        ));
        SwapPairClaim {
            id: SwapUseId::random().unwrap(),
            source: SwapAccountChoice::Existing(source),
            delegate,
            purpose_summary: None,
            assets: Vec::new(),
            approval,
            destination: Some(SwapDestinationClaim {
                chain_id: 137,
                account: SwapAccountChoice::Existing(destination),
                delegate: destination_delegate,
                destination_token: Address::repeat_byte(10),
            }),
        }
    };
    let first = claim(first_destination, Address::repeat_byte(3));
    store.claim_swap_pair(first.clone()).unwrap();
    assert_eq!(
        store.cancel_swap_use(source, first.id).unwrap(),
        SwapUseCancellation {
            source: SwapUseRelease::Released,
            destination: Some(SwapUseRelease::Released),
        }
    );
    let next = claim(next_destination, Address::repeat_byte(4));
    let claimed = store.claim_swap_pair(next.clone()).unwrap();

    // The first preparation finishes after cancellation and a different pair claimed its
    // source. Its approval must not change the new use's receiver or destination link.
    assert!(matches!(
        store.record_swap_approval(source, first.id, first.approval),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    let reopened = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let record = |store: &ExecutorStore, operation| {
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
    };
    assert_eq!(record(&reopened, source), Some(claimed.source.clone()));
    assert_eq!(
        record(&destination_store, next_destination),
        claimed.destination
    );
    let SwapDelivery::Bridge(bridge) = next.approval.delivery else {
        unreachable!()
    };
    for delivery in [
        SwapDelivery::Reshield,
        SwapDelivery::Bridge(BridgeDelivery {
            private: None,
            ..bridge
        }),
        SwapDelivery::Bridge(BridgeDelivery {
            destination_chain: 10,
            ..bridge
        }),
    ] {
        let mut incompatible = next.approval.clone();
        incompatible.delivery = delivery;
        assert!(matches!(
            reopened.record_swap_approval(source, next.id, incompatible),
            Err(ExecutorStoreError::OperationMismatch)
        ));
        let reopened_source = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        let reopened_destination = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
        assert_eq!(
            record(&reopened_source, source),
            Some(claimed.source.clone())
        );
        assert_eq!(
            record(&reopened_destination, next_destination),
            claimed.destination
        );
    }
    let mut refreshed = next.approval;
    refreshed.bounds.slippage_bps += 1;
    let saved = reopened
        .record_swap_approval(source, next.id, refreshed.clone())
        .unwrap();
    assert_eq!(saved.swap_approval(), Some(&refreshed));
    assert_eq!(saved.destination_operation(), Some(next_destination));
    assert_eq!(
        reopened.cancel_swap_use(source, next.id).unwrap(),
        SwapUseCancellation {
            source: SwapUseRelease::Released,
            destination: Some(SwapUseRelease::Released),
        }
    );
    drop((store, reopened, destination_store));
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancelling_a_swap_use_releases_a_reused_source_and_keeps_an_issued_destination_setup() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let destination_store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let delegate = accepted_delegate(1);
    let token = Address::repeat_byte(10);
    let [existing, new] = std::array::from_fn(|_| ExecutorOperationId::random().unwrap());

    // The source account is set up and idle. The swap reuses it and allocates its destination.
    store
        .reserve(existing, delegate, Some("Private swap"), &[])
        .unwrap();
    let source_observed = set_up(&store, existing, delegate, Address::repeat_byte(7));
    let source_before = store.records().unwrap().remove(0);
    let id = SwapUseId::random().unwrap();
    let mut bridged = approval(5, 6);
    bridged.delivery = SwapDelivery::Bridge(private_bridge_delivery(
        Address::ZERO,
        token,
        BridgeShieldFailure::RefundOnOrigin,
    ));
    store
        .claim_swap_pair(SwapPairClaim {
            id,
            source: SwapAccountChoice::Existing(existing),
            delegate,
            purpose_summary: None,
            assets: Vec::new(),
            approval: bridged,
            destination: Some(SwapDestinationClaim {
                chain_id: 137,
                account: SwapAccountChoice::New(new),
                delegate,
                destination_token: token,
            }),
        })
        .unwrap();
    // A reused account is already set up, so its new use takes no setup payload.
    let setup = |nonce, hash, observed, inputs| {
        hook(
            ExecutorPayloadPurpose::Operation,
            nonce,
            hash,
            delegate,
            observed,
            inputs,
        )
    };
    assert!(matches!(
        store.record_issued(existing, setup(1, 4, source_observed, Vec::new())),
        Err(ExecutorStoreError::OperationMismatch)
    ));

    // The new destination's setup is handed off with its fee notes before the user cancels.
    destination_store
        .bind_address(new, Address::repeat_byte(9))
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    destination_store
        .record_account_read(new, observed)
        .unwrap();
    let issued = destination_store
        .record_issued(new, setup(0, 3, observed, swap_inputs()))
        .unwrap();
    let cancelled = store.cancel_swap_use(existing, id).unwrap();
    assert_eq!(
        cancelled,
        SwapUseCancellation {
            source: SwapUseRelease::Released,
            destination: Some(SwapUseRelease::IssuedWorkRemains),
        }
    );

    // The reused source is free again with its history, and is neither retired nor stopped.
    let source = store.records().unwrap().remove(0);
    assert_eq!(
        (
            source.active_swap_use(),
            source.is_retired(),
            source.is_swap_setup_stopped()
        ),
        (None, false, false)
    );
    assert_eq!(source.issued(), source_before.issued());
    assert!(source.swap_use(id).unwrap().is_stopped());

    // The fresh destination is stopped and retired like any abandoned setup, and stays with
    // the cancelled use. Its published setup keeps its inputs reserved.
    let destination = destination_store.records().unwrap().remove(0);
    assert!(destination.is_swap_setup_stopped() && destination.is_retired());
    assert_eq!(destination.active_swap_use(), Some(id));
    assert_eq!(destination.issued(), issued.issued());
    assert_eq!(destination.reserved_inputs(), swap_inputs());

    // Cancelling again changes nothing, and the source takes another swap.
    assert_eq!(store.cancel_swap_use(existing, id).unwrap(), cancelled);
    store
        .claim_swap_pair(SwapPairClaim {
            id: SwapUseId::random().unwrap(),
            source: SwapAccountChoice::Existing(existing),
            delegate,
            purpose_summary: None,
            assets: Vec::new(),
            approval: approval(5, 6),
            destination: None,
        })
        .unwrap();
    drop(store);
    drop(destination_store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stopping_an_unsigned_reused_pair_releases_both_accounts_after_reload() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let destination_store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let (delegate, destination_delegate) = (accepted_delegate(1), accepted_delegate(137));
    let [source, destination] = std::array::from_fn(|_| ExecutorOperationId::random().unwrap());
    store.reserve(source, delegate, None, &[]).unwrap();
    destination_store
        .reserve(destination, destination_delegate, None, &[])
        .unwrap();
    let source_observed = set_up(&store, source, delegate, Address::repeat_byte(7));
    let destination_observed = set_up(
        &destination_store,
        destination,
        destination_delegate,
        Address::repeat_byte(9),
    );
    let mut approved = approval(5, 6);
    approved.delivery = SwapDelivery::Bridge(private_bridge_delivery(
        Address::repeat_byte(9),
        Address::repeat_byte(10),
        BridgeShieldFailure::KeepOnDestination,
    ));
    let claim = SwapPairClaim {
        id: SwapUseId::random().unwrap(),
        source: SwapAccountChoice::Existing(source),
        delegate,
        purpose_summary: None,
        assets: Vec::new(),
        approval: approved,
        destination: Some(SwapDestinationClaim {
            chain_id: 137,
            account: SwapAccountChoice::Existing(destination),
            delegate: destination_delegate,
            destination_token: Address::repeat_byte(10),
        }),
    };
    let pair = store.claim_swap_pair(claim.clone()).unwrap();
    store.stop_swap_use(source).unwrap();
    // A repeated stop and a restart cannot strand the unsigned counterpart.
    destination_store.stop_swap_use(destination).unwrap();
    let reloaded = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let reloaded_destination = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    for (record, before) in [
        (reloaded.records().unwrap().remove(0), pair.source),
        (
            reloaded_destination.records().unwrap().remove(0),
            pair.destination.unwrap(),
        ),
    ] {
        assert_eq!(record.active_swap_use(), None);
        assert!(!record.is_retired());
        assert!(record.swap_use(claim.id).unwrap().is_stopped());
        assert_eq!(record.issued(), before.issued());
    }
    // Late signed work cannot resume the stopped use, but a new use claims the same pair.
    let mut late_order = private_across_attempt(
        source,
        Address::repeat_byte(7),
        delegate,
        source_observed,
        Address::repeat_byte(9),
        Address::repeat_byte(10),
    );
    late_order.use_id = claim.id;
    assert!(matches!(
        reloaded.record_swap_attempt(source, late_order),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    assert!(matches!(
        reloaded_destination.record_swap_destination_shield(
            destination,
            claim.id,
            hook(
                ExecutorPayloadPurpose::SwapDestinationShield,
                1,
                70,
                destination_delegate,
                destination_observed,
                Vec::new(),
            ),
        ),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    let next = SwapUseId::random().unwrap();
    let pair = reloaded
        .claim_swap_pair(SwapPairClaim { id: next, ..claim })
        .unwrap();
    assert_eq!(pair.source.active_swap_use(), Some(next));
    assert_eq!(pair.destination.unwrap().active_swap_use(), Some(next));
    drop(reloaded);
    drop(reloaded_destination);
    drop(store);
    drop(destination_store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_handoff_checks_the_current_use_and_blocks_a_later_swap_until_resolved() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let delegate = accepted_delegate(1);
    let operation = ExecutorOperationId::random().unwrap();
    let executor = Address::repeat_byte(7);
    let observed = settled_source_account(&store, operation, delegate, executor);
    let claim = |id| SwapPairClaim {
        id,
        source: SwapAccountChoice::Existing(operation),
        delegate,
        purpose_summary: None,
        assets: Vec::new(),
        approval: approval(5, 6),
        destination: None,
    };
    let abandoned = SwapUseId::random().unwrap();
    store.claim_swap_pair(claim(abandoned)).unwrap();
    let prepared = store.stop_swap_use(operation).unwrap();
    let expected_use = prepared.active_swap_use();
    assert_eq!(expected_use, None);
    // While the recovery review is open, another unsigned preparation claims the account.
    let next = SwapUseId::random().unwrap();
    let claimed = store.claim_swap_pair(claim(next)).unwrap().source;
    let payload = hook(
        ExecutorPayloadPurpose::Recovery,
        3,
        70,
        delegate,
        observed,
        Vec::new(),
    );
    let handoff = || store.record_recovery_issued(operation, expected_use, payload.clone());
    assert!(matches!(handoff(), Err(ExecutorStoreError::SwapUseActive)));
    assert_eq!(
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap(),
        claimed
    );
    // A new review after stopping that preparation can hand off, but its unresolved
    // recovery cannot be bypassed just because this account has settled source history.
    store.stop_swap_use(operation).unwrap();
    let issued = handoff().unwrap();
    for evidence in [
        SwapAdmissionEvidence::Recorded,
        SwapAdmissionEvidence::Fresh,
    ] {
        assert_eq!(
            swap_account_refusal(
                &issued,
                1,
                SwapAccountRole::Source,
                SwapAccountUse::New,
                evidence,
            ),
            Some(SwapAccountRefusal::UnfinishedWork)
        );
    }
    assert!(matches!(
        store.claim_swap_pair(claim(SwapUseId::random().unwrap())),
        Err(ExecutorStoreError::SwapUseActive)
    ));
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_orphaned_destination_shield_needs_a_verified_replacement_before_source_reuse() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let origin_store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let (origin_delegate, delegate) = (accepted_delegate(1), accepted_delegate(137));
    let token = Address::repeat_byte(10);
    // A recorded order might still deliver. Only the preparation with no recorded order
    // becomes reusable once a different canonical payload invalidates every signed shield.
    for order_recorded in [false, true] {
        let [origin, destination] = std::array::from_fn(|_| ExecutorOperationId::random().unwrap());
        let executor = Address::repeat_byte(if order_recorded { 8 } else { 7 });
        let receiver = Address::repeat_byte(if order_recorded { 18 } else { 17 });
        origin_store
            .reserve(origin, origin_delegate, None, &[])
            .unwrap();
        let origin_observed = set_up(&origin_store, origin, origin_delegate, executor);
        let observed = settled_source_account(&store, destination, delegate, receiver);
        let mut approved = approval(5, 6);
        approved.delivery = SwapDelivery::Bridge(private_bridge_delivery(
            receiver,
            token,
            BridgeShieldFailure::RefundOnOrigin,
        ));
        let id = SwapUseId::random().unwrap();
        origin_store
            .claim_swap_pair(SwapPairClaim {
                id,
                source: SwapAccountChoice::Existing(origin),
                delegate: origin_delegate,
                purpose_summary: None,
                assets: Vec::new(),
                approval: approved,
                destination: Some(SwapDestinationClaim {
                    chain_id: 137,
                    account: SwapAccountChoice::Existing(destination),
                    delegate,
                    destination_token: token,
                }),
            })
            .unwrap();
        for hash in [70, 71] {
            store
                .record_swap_destination_shield(
                    destination,
                    id,
                    hook(
                        ExecutorPayloadPurpose::SwapDestinationShield,
                        3,
                        hash,
                        delegate,
                        observed,
                        Vec::new(),
                    ),
                )
                .unwrap();
        }
        if order_recorded {
            let mut attempt = private_across_attempt(
                origin,
                executor,
                origin_delegate,
                origin_observed,
                receiver,
                token,
            );
            attempt.use_id = id;
            origin_store.record_swap_attempt(origin, attempt).unwrap();
            origin_store.stop_swap_use(origin).unwrap();
        } else {
            let cancelled = origin_store.cancel_swap_use(origin, id).unwrap();
            assert_eq!(cancelled.source, SwapUseRelease::Released);
            assert_eq!(
                cancelled.destination,
                Some(SwapUseRelease::IssuedWorkRemains)
            );
        }
        let before = store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == destination)
            .unwrap();
        let blocked = |record: &ExecutorRecord| {
            for evidence in [
                SwapAdmissionEvidence::Recorded,
                SwapAdmissionEvidence::Fresh,
            ] {
                assert!(
                    swap_account_refusal(
                        record,
                        137,
                        SwapAccountRole::Source,
                        SwapAccountUse::New,
                        evidence,
                    )
                    .is_some()
                );
            }
        };
        blocked(&before);
        // Recovery competes at the shields' nonce.
        store.record_account_read(destination, observed).unwrap();
        let issued = store
            .record_recovery_issued(
                destination,
                Some(id),
                hook(
                    ExecutorPayloadPurpose::Recovery,
                    3,
                    73,
                    delegate,
                    observed,
                    Vec::new(),
                ),
            )
            .unwrap();
        let block = BlockNumHash::new(20, B256::repeat_byte(20));
        let consumed = ExecutorNonceObservation::new(block, U256::from(4));
        // An advanced nonce names no action, so the shields' execution stays uncertain.
        let unknown = store.record_account_read(destination, consumed).unwrap();
        blocked(&unknown);
        // The record as an earlier version stored it once its block scan found `winner`
        // executed at the shields' nonce. Nothing writes such an inclusion any more.
        let stored_with_winner = |winner: u8| {
            let mut stored = serde_json::to_value(&unknown).unwrap();
            for payload in stored["issued"].as_array_mut().unwrap() {
                if payload["hash"] == serde_json::json!(B256::repeat_byte(winner)) {
                    payload["inclusion"] = serde_json::json!({
                        "block": block,
                        "transaction_hash": B256::repeat_byte(winner + 2),
                        "result": "Executed",
                    });
                }
            }
            let record: ExecutorRecord = serde_json::from_value(stored).unwrap();
            store.put_operation_fixture(destination, &record).unwrap();
        };
        // Even a stored winner is insufficient if one of the retained shields ran.
        stored_with_winner(70);
        let executed = store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == destination)
            .unwrap();
        blocked(&executed);

        // The recovery's stored execution makes both shield signatures unusable; the
        // source's stopped orderless fact must also be retained.
        stored_with_winner(73);
        let reloaded = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
        let resolved = reloaded
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == destination)
            .unwrap();
        assert_eq!(resolved.issued().len(), issued.issued().len());
        assert!(resolved.swap_use(id).unwrap().is_stopped());
        assert_eq!(resolved.swap_destination_use(id).unwrap().outcome, None);
        for hash in [70, 71] {
            assert_eq!(
                resolved.payload_state(B256::repeat_byte(hash)),
                Some(ExecutorPayloadState::Resolved)
            );
        }
        let next = SwapUseId::random().unwrap();
        let claim = SwapPairClaim {
            id: next,
            source: SwapAccountChoice::Existing(destination),
            delegate,
            purpose_summary: None,
            assets: Vec::new(),
            approval: approval(5, 6),
            destination: None,
        };
        if order_recorded {
            blocked(&resolved);
            assert!(matches!(
                reloaded.claim_swap_pair(claim),
                Err(ExecutorStoreError::SwapUseActive)
            ));
        } else {
            assert_eq!(
                reloaded
                    .claim_swap_pair(claim)
                    .unwrap()
                    .source
                    .active_swap_use(),
                Some(next)
            );
        }
    }
    drop(origin_store);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn version_1_swap_records_project_to_uses_and_are_rewritten_only_when_they_change() {
    // The named MessagePack record of the build before swap uses: one swap's links on the
    // record itself, and orders without a use.
    #[derive(serde::Serialize)]
    struct V1Hook {
        nonce: U256,
        payload: B256,
    }
    #[derive(serde::Serialize)]
    struct V1Order {
        terms: Option<SwapTerms>,
        attempt: u32,
        uid: FixedBytes<56>,
        delivery: SwapDelivery,
        bounds: SwapApprovedBounds,
        pre_hook: V1Hook,
        post_hook: Option<V1Hook>,
        invalidates: Option<FixedBytes<56>>,
        observations: SwapOrderObservations,
        submission: Option<SwapSubmission>,
        submission_status: SwapSubmissionStatus,
        bridge: Option<BridgeOrderTerms>,
    }
    #[derive(serde::Serialize)]
    struct V1Swap {
        terms: SwapTerms,
        proof: SwapProof,
        orders: Vec<V1Order>,
    }
    #[derive(serde::Serialize)]
    struct V1Record {
        version: u32,
        derivation: ExecutorDerivationScheme,
        origin: ExecutorRecordOrigin,
        operation: ExecutorOperationId,
        index: u32,
        address: Option<Address>,
        delegate: Address,
        retired: bool,
        created_at: Option<u64>,
        restored_at: Option<u64>,
        purpose_summary: Option<String>,
        assets: Vec<ExecutorAsset>,
        hidden: bool,
        issued: Vec<IssuedExecutorPayload>,
        nonce_observation: Option<ExecutorNonceObservation>,
        swap: Option<V1Swap>,
        swap_approval: Option<SwapApproval>,
        swap_setup_stopped: bool,
        swap_destination: Option<SwapDestinationRecord>,
        destination_operation: Option<ExecutorOperationId>,
    }
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let origin_store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let delegate = accepted_delegate(1);
    let token = Address::repeat_byte(10);
    let [
        pending,
        pending_destination,
        signed,
        signed_destination,
        fallback,
        plain,
    ] = std::array::from_fn(|_| ExecutorOperationId::random().unwrap());
    let account = |index: u8| Address::repeat_byte(0x40 + index);
    let v1 = |operation, index: u8| V1Record {
        version: 1,
        derivation: ExecutorDerivationScheme::Railgun7702V1,
        origin: ExecutorRecordOrigin::Reserved,
        operation,
        index: u32::from(index),
        address: Some(account(index)),
        delegate,
        retired: false,
        created_at: Some(1_700_000_000),
        restored_at: None,
        purpose_summary: None,
        assets: Vec::new(),
        hidden: false,
        issued: Vec::new(),
        nonce_observation: None,
        swap: None,
        swap_approval: None,
        swap_setup_stopped: false,
        swap_destination: None,
        destination_operation: None,
    };
    let serves = |origin_operation| SwapDestinationRecord {
        origin_chain: 1,
        origin_operation,
        destination_token: token,
        outcome: None,
    };
    let before_setup =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    let payload = |purpose, nonce, hash, observed, inputs| {
        hook(purpose, nonce, hash, delegate, observed, inputs)
    };
    let setup = |inputs| {
        payload(
            ExecutorPayloadPurpose::Operation,
            0,
            3,
            before_setup,
            inputs,
        )
    };
    let terms = SwapTerms::new(
        Address::repeat_byte(5),
        Address::repeat_byte(6),
        SwapRecipient::new(U256::from(7), [8; 32]),
        B256::repeat_byte(3),
    );
    let order = |terms, owner, delivery, observations, bridge| V1Order {
        terms,
        attempt: 0,
        uid: OrderUid::new(B256::repeat_byte(30), owner, 1_000).0,
        delivery,
        bounds: swap_bounds(),
        pre_hook: V1Hook {
            nonce: U256::ONE,
            payload: B256::repeat_byte(31),
        },
        post_hook: Some(V1Hook {
            nonce: U256::from(2),
            payload: B256::repeat_byte(32),
        }),
        invalidates: None,
        observations,
        submission: None,
        submission_status: SwapSubmissionStatus::Accepted,
        bridge,
    };
    let inputs = swap_inputs();
    let approved = approval(7, 8);
    for record in [
        // A setup still pending: approved terms, the link to the destination, and the setup
        // payload holding its inputs.
        V1Record {
            issued: vec![setup(inputs.clone())],
            nonce_observation: Some(before_setup),
            swap_approval: Some(approved.clone()),
            destination_operation: Some(pending_destination),
            ..v1(pending, 0)
        },
        // A signed private Bridge order with its hooks.
        V1Record {
            issued: vec![
                setup(Vec::new()),
                payload(
                    ExecutorPayloadPurpose::SwapPreHook,
                    1,
                    31,
                    observed,
                    Vec::new(),
                ),
                payload(
                    ExecutorPayloadPurpose::SwapPostHook,
                    2,
                    32,
                    observed,
                    Vec::new(),
                ),
            ],
            nonce_observation: Some(observed),
            swap: Some(V1Swap {
                terms,
                proof: SwapProof::new(B256::repeat_byte(20), Vec::new()),
                orders: vec![order(
                    Some(terms),
                    account(1),
                    SwapDelivery::Bridge(BridgeDelivery {
                        provider: BridgeProvider::Across,
                        destination_chain: 137,
                        receiver: account(1),
                        destination_token: token,
                        surplus: BridgeSurplus::KeepInAccount,
                        private: Some(BridgePrivateDelivery {
                            on_shield_failure: BridgeShieldFailure::KeepOnDestination,
                        }),
                    }),
                    SwapOrderObservations::default(),
                    Some(BridgeOrderTerms::Across(AcrossOrderTerms {
                        spoke_pool: Address::repeat_byte(11),
                        input_token: terms.buy_token(),
                        output_token: token,
                        input_amount: U256::from(9_975),
                        output_amount: U256::from(9_900),
                        quote_timestamp: 1_700_000_000,
                        fill_deadline: 1_700_021_600,
                        exclusive_relayer: Address::ZERO,
                        exclusivity_parameter: 0,
                        recipient: Some(Address::repeat_byte(12)),
                        message_hash: Some(B256::repeat_byte(13)),
                    })),
                )],
            }),
            destination_operation: Some(signed_destination),
            ..v1(signed, 1)
        },
        // An ended order from before orders kept their own terms.
        V1Record {
            swap: Some(V1Swap {
                terms,
                proof: SwapProof::new(B256::repeat_byte(20), Vec::new()),
                orders: vec![order(
                    None,
                    account(2),
                    SwapDelivery::Reshield,
                    SwapOrderObservations {
                        pre_hook_dead: Some(SwapPreHookDeath {
                            cause: SwapPreHookDeathCause::Expired,
                            observation: SwapObservation {
                                block: observed.block(),
                                transaction_hash: None,
                            },
                        }),
                        ..SwapOrderObservations::default()
                    },
                    None,
                )],
            }),
            ..v1(fallback, 2)
        },
        v1(plain, 3),
    ] {
        origin_store
            .put_operation_fixture(record.operation, &record)
            .unwrap();
    }
    for record in [
        V1Record {
            swap_destination: Some(serves(pending)),
            ..v1(pending_destination, 0)
        },
        // The signed order's destination issued its shield.
        V1Record {
            issued: vec![
                setup(Vec::new()),
                payload(
                    ExecutorPayloadPurpose::SwapDestinationShield,
                    1,
                    70,
                    observed,
                    Vec::new(),
                ),
            ],
            nonce_observation: Some(observed),
            swap_destination: Some(serves(signed)),
            ..v1(signed_destination, 1)
        },
    ] {
        store
            .put_operation_fixture(record.operation, &record)
            .unwrap();
    }
    let stored = |chain| {
        db.list_desktop_wallet_vault_records(&crate::vault::executors::executor_operation_prefix(
            view.wallet_id(),
            chain,
        ))
        .unwrap()
    };
    let find = |records: &[ExecutorRecord], operation: ExecutorOperationId| {
        records
            .iter()
            .find(|record| record.operation() == operation)
            .unwrap()
            .clone()
    };
    let uses = |record: &ExecutorRecord| {
        (
            record
                .swap_uses()
                .iter()
                .map(SwapUseRecord::id)
                .collect::<Vec<_>>(),
            record.active_swap_use(),
        )
    };
    let first_use = |origin| {
        (
            vec![SwapUseId::first(origin)],
            Some(SwapUseId::first(origin)),
        )
    };

    // Loading projects the links in memory, the same way every time, and writes nothing.
    // Startup reconciliation finds consistent links and writes nothing either.
    let before = (stored(1), stored(137));
    let origins = origin_store.records().unwrap();
    let destinations = store.records().unwrap();
    assert_eq!(origin_store.records_as_version_1_reader().unwrap(), origins);
    assert!(!store.reconcile_swap_destinations_on_load().unwrap());
    assert_eq!(store.records().unwrap(), destinations);
    assert_eq!((stored(1), stored(137)), before);
    assert_eq!(origin_store.next_index().unwrap(), 4);

    // Both accounts of a swap project to the use named after its origin operation.
    let pending_record = find(&origins, pending);
    assert_eq!(
        (
            pending_record.index(),
            pending_record.address(),
            pending_record.swap_approval(),
            pending_record.destination_operation()
        ),
        (
            0,
            Some(account(0)),
            Some(&approved),
            Some(pending_destination)
        )
    );
    assert_eq!(pending_record.issued()[0].hash(), B256::repeat_byte(3));
    assert_eq!(pending_record.reserved_inputs(), inputs);
    assert_eq!(uses(&pending_record), first_use(pending));
    let pending_destination = find(&destinations, pending_destination);
    assert_eq!(
        pending_destination.swap_destination(),
        Some(serves(pending))
    );
    assert_eq!(uses(&pending_destination), first_use(pending));

    let signed_record = find(&origins, signed);
    let signed_order = &signed_record.swap().unwrap().orders()[0];
    assert_eq!(
        (
            signed_order.use_id(),
            signed_order.pre_hook().payload(),
            signed_order.post_hook().map(|hook| hook.payload())
        ),
        (
            Some(SwapUseId::first(signed)),
            B256::repeat_byte(31),
            Some(B256::repeat_byte(32))
        )
    );
    assert_eq!(
        signed_record
            .issued()
            .iter()
            .map(IssuedExecutorPayload::hash)
            .collect::<Vec<_>>(),
        [3, 31, 32].map(B256::repeat_byte)
    );
    assert_eq!(
        signed_record.destination_operation(),
        Some(signed_destination)
    );
    assert_eq!(uses(&signed_record), first_use(signed));
    let signed_destination = find(&destinations, signed_destination);
    assert_eq!(uses(&signed_destination), first_use(signed));
    assert!(matches!(
        signed_destination.swap_uses()[0].role(),
        SwapUseRole::Destination { shields, .. } if shields == &[B256::repeat_byte(70)]
    ));
    // An account without swap links has no use and keeps its version.
    assert_eq!(uses(&find(&origins, plain)), (Vec::new(), None));

    // A change stores that record with its uses and rewrites no other. A build from before
    // uses then refuses the whole chain's list instead of overlooking the claim, while the
    // allocation floor stays.
    let hidden = origin_store.set_hidden(pending, true).unwrap();
    assert!(hidden.is_hidden());
    assert_eq!(
        (
            hidden.swap_uses(),
            hidden.issued(),
            hidden.reserved_inputs()
        ),
        (
            pending_record.swap_uses(),
            pending_record.issued(),
            pending_record.reserved_inputs()
        )
    );
    let reloaded = origin_store.records().unwrap();
    assert_eq!(reloaded.len(), origins.len());
    assert_eq!(find(&reloaded, pending), hidden);
    assert_eq!(
        before
            .0
            .iter()
            .zip(stored(1))
            .filter(|(old, new)| old != &new)
            .count(),
        1
    );
    assert!(matches!(
        origin_store.records_as_version_1_reader(),
        Err(ExecutorStoreError::InvalidRecord)
    ));
    assert_eq!(origin_store.next_index().unwrap(), 4);

    // A later use on the account leaves the old order on the swap's original terms. The
    // account takes one once its setup is recorded as executed.
    origin_store
        .record_account_read(fallback, before_setup)
        .unwrap();
    origin_store
        .record_issued(fallback, setup(Vec::new()))
        .unwrap();
    origin_store
        .record_account_read(fallback, observed)
        .unwrap();
    let later = SwapUseId::random().unwrap();
    let reused = origin_store
        .claim_swap_pair(SwapPairClaim {
            id: later,
            source: SwapAccountChoice::Existing(fallback),
            delegate,
            purpose_summary: None,
            assets: Vec::new(),
            approval: approved.clone(),
            destination: None,
        })
        .unwrap()
        .source;
    let swap = reused.swap().unwrap();
    assert_eq!(swap.order_terms(&swap.orders()[0]), &terms);
    assert_eq!(
        (swap.orders()[0].use_id(), reused.active_swap_use()),
        (Some(SwapUseId::first(fallback)), Some(later))
    );
    assert_eq!(reused.swap_approval(), Some(&approved));
    drop(origin_store);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

/// The named `MessagePack` shape of a stored swap use, over the roles a build wrote.
#[derive(serde::Serialize)]
struct StoredUse<R> {
    id: SwapUseId,
    started_at: Option<u64>,
    fresh: bool,
    stopped: bool,
    role: R,
    stopped_before_order: bool,
}

/// The named `MessagePack` shape of a stored record holding `swap_uses`, or with none of them, a
/// version-1 record.
#[derive(serde::Serialize)]
struct StoredRecord<R> {
    version: u32,
    derivation: ExecutorDerivationScheme,
    origin: ExecutorRecordOrigin,
    operation: ExecutorOperationId,
    index: u32,
    address: Option<Address>,
    delegate: Address,
    retired: bool,
    created_at: Option<u64>,
    restored_at: Option<u64>,
    purpose_summary: Option<String>,
    issued: Vec<IssuedExecutorPayload>,
    swap_destination: Option<SwapDestinationRecord>,
    swap_uses: Vec<StoredUse<R>>,
    active_swap_use: Option<SwapUseId>,
}

fn stored_record<R>(
    operation: ExecutorOperationId,
    index: u8,
    swap_use: Option<(SwapUseId, R)>,
) -> StoredRecord<R> {
    StoredRecord {
        version: if swap_use.is_some() { 2 } else { 1 },
        derivation: ExecutorDerivationScheme::Railgun7702V1,
        origin: ExecutorRecordOrigin::Reserved,
        operation,
        index: u32::from(index),
        address: Some(Address::repeat_byte(0x40 + index)),
        delegate: Address::repeat_byte(1),
        retired: false,
        created_at: Some(1_700_000_000),
        restored_at: None,
        purpose_summary: None,
        issued: Vec::new(),
        swap_destination: None,
        active_swap_use: swap_use.as_ref().map(|(id, _)| *id),
        swap_uses: swap_use
            .into_iter()
            .map(|(id, role)| StoredUse {
                id,
                started_at: Some(1_700_000_000),
                fresh: true,
                stopped: false,
                role,
                stopped_before_order: false,
            })
            .collect(),
    }
}

/// The stored shape of a `PublicSwapRecord`, whose fields only the store writes.
#[derive(serde::Serialize)]
struct StoredPublicSwap {
    approval: PublicSwapApproval,
    intent: PublicSwapIntent,
    transactions: Vec<PublicSwapTransaction>,
    path: Option<PublicSwapPath>,
    bridge: Option<AcrossOrderTerms>,
    observations: PublicSwapObservations,
}

impl StoredPublicSwap {
    fn decode(&self) -> PublicSwapRecord {
        rmp_serde::from_slice(&rmp_serde::to_vec_named(self).unwrap()).unwrap()
    }
}

const PUBLIC_SWAP_BATCH_CALLDATA: &[u8] = b"signed cow-shed hook batch";
const PUBLIC_SWAP_PERMIT_SIGNATURE: [u8; 65] = [8; 65];

fn public_swap_order_path() -> PublicSwapPath {
    PublicSwapPath::Order(Box::new(
        PublicSwapOrder::new(
            OrderUid::new(B256::repeat_byte(30), Address::repeat_byte(0x50), 1_000),
            Address::repeat_byte(6),
            Address::repeat_byte(0x51),
            PublicSwapHookBatch::new(
                Bytes::from_static(PUBLIC_SWAP_BATCH_CALLDATA),
                B256::repeat_byte(33),
                1_000,
            ),
            SwapSubmission::new([9; 65], Some(42)),
        )
        .with_permit(Some(PublicSwapPermit::new(
            U256::from(4),
            1_000,
            U256::from(100),
            PUBLIC_SWAP_PERMIT_SIGNATURE,
        ))),
    ))
}

/// What the user approved for a Public-paid swap that sells `sell` and delivers to `destination`.
fn public_swap_approval(sell: u8, destination: SwapApprovedAccount) -> PublicSwapApproval {
    PublicSwapApproval {
        bounds: swap_bounds(),
        price_verified: Some(true),
        price_acknowledged: false,
        sell_token: Address::repeat_byte(sell),
        on_shield_failure: BridgeShieldFailure::KeepOnDestination,
        destination,
        max_gas_cost: U256::from(1_000_000),
    }
}

/// A Public-paid swap with every part of its record set.
fn stored_public_swap(path: PublicSwapPath) -> StoredPublicSwap {
    use alloy::rpc::types::TransactionRequest;
    let observed = |block: u8, transaction: u8| SwapObservation {
        block: BlockNumHash::new(u64::from(block), B256::repeat_byte(block)),
        transaction_hash: Some(B256::repeat_byte(transaction)),
    };
    let transaction = |kind, nonce: u64, hash: u8, inclusion| {
        let mut request = TransactionRequest::default()
            .to(Address::repeat_byte(11))
            .input(Bytes::from_static(b"public swap transaction").into());
        request.from = Some(Address::repeat_byte(0x50));
        request.chain_id = Some(1);
        request.nonce = Some(nonce);
        request.gas = Some(100_000);
        request.max_fee_per_gas = Some(2);
        request.max_priority_fee_per_gas = Some(1);
        PublicSwapTransaction {
            kind,
            transaction: request,
            hash: B256::repeat_byte(hash),
            inclusion,
            submitted_from_block: Some(19),
            deposit_scan_from_block: None,
        }
    };
    StoredPublicSwap {
        approval: public_swap_approval(
            5,
            SwapApprovedAccount {
                address: Some(Address::repeat_byte(0x40)),
                setup: true,
            },
        ),
        intent: PublicSwapIntent {
            bridged_token: Address::repeat_byte(6),
            order: path != PublicSwapPath::Deposit,
        },
        transactions: vec![
            transaction(
                PublicSwapTransactionKind::Approval,
                7,
                60,
                Some(PublicSwapInclusion {
                    observation: observed(20, 60),
                    succeeded: true,
                    finalized: true,
                }),
            ),
            transaction(PublicSwapTransactionKind::Invalidation, 8, 61, None),
        ],
        path: Some(path),
        bridge: Some(AcrossOrderTerms {
            spoke_pool: Address::repeat_byte(11),
            input_token: Address::repeat_byte(6),
            output_token: Address::repeat_byte(10),
            input_amount: U256::from(9_975),
            output_amount: U256::from(9_900),
            quote_timestamp: 1_700_000_000,
            fill_deadline: 1_700_021_600,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
            recipient: Some(Address::repeat_byte(12)),
            message_hash: Some(B256::repeat_byte(13)),
        }),
        observations: PublicSwapObservations {
            traded: Some(observed(21, 62)),
            trade_amounts: Some(SwapTradeAmounts {
                sell_amount: U256::from(10_000),
                buy_amount: U256::from(9_980),
                fee_amount: U256::ZERO,
                settlement_gas_used: Some(200_000),
                settlement_effective_gas_price: Some(3),
                executed_fee: None,
                executed_fee_token: None,
            }),
            held_by_proxy: Some(PublicSwapProxyHolding {
                observation: observed(21, 62),
                amount: U256::from(9_980),
            }),
            bridge_handoff: Some(SwapBridgeHandoff {
                observation: observed(21, 62),
                deposit_id: Some(U256::from(77)),
            }),
            deposited: Some(PublicSwapDeposited {
                input_amount: U256::from(9_980),
                output_amount: U256::from(9_905),
            }),
            bridge_outcome: Some(SwapBridgeOutcome::DeliveredVerified {
                block: BlockNumHash::new(30, B256::repeat_byte(30)),
                transaction_hash: B256::repeat_byte(63),
                output_amount: U256::from(9_905),
                shielded: true,
            }),
            ..PublicSwapObservations::default()
        },
    }
}

#[test]
fn public_swap_destination_uses_round_trip_through_the_store() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let role = |swap: &StoredPublicSwap| SwapUseRole::PublicSourceDestination {
        origin_chain: 1,
        source: Address::repeat_byte(0x50),
        destination_token: Address::repeat_byte(10),
        shields: vec![B256::repeat_byte(70)],
        outcome: Some(SwapDestinationOutcome::Shielded {
            block: BlockNumHash::new(30, B256::repeat_byte(30)),
            transaction_hash: B256::repeat_byte(63),
        }),
        swap: Box::new(swap.decode()),
    };
    for (index, stored) in [
        stored_public_swap(public_swap_order_path()),
        stored_public_swap(PublicSwapPath::Deposit),
    ]
    .into_iter()
    .enumerate()
    {
        let operation = ExecutorOperationId::random().unwrap();
        let id = SwapUseId::random().unwrap();
        store
            .put_operation_fixture(
                operation,
                &stored_record(
                    operation,
                    u8::try_from(index).unwrap(),
                    Some((id, role(&stored))),
                ),
            )
            .unwrap();
        let find = || {
            store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap()
        };

        // The stored use decodes to the same role, with every part of the swap's record.
        let record = find();
        let (swap_use, swap) = record.public_swap_use(id).unwrap();
        assert_eq!(swap_use.role(), &role(&stored));
        assert_eq!(swap_use.approval(), None);
        assert_eq!(swap.approval(), &stored.approval);
        assert_eq!(swap.intent(), stored.intent);
        assert_eq!(swap.transactions(), stored.transactions);
        assert_eq!(swap.path(), stored.path.as_ref());
        assert_eq!(swap.bridge(), stored.bridge.as_ref());
        assert_eq!(swap.observations(), stored.observations);
        assert_eq!(
            swap.order().is_some(),
            stored.path != Some(PublicSwapPath::Deposit)
        );
        if let Some(order) = swap.order() {
            assert_eq!(order.valid_to(), 1_000);
            assert_eq!(&order.batch().calldata()[..], PUBLIC_SWAP_BATCH_CALLDATA);
            assert_eq!(order.submission().quote_id(), Some(42));
        }

        // A change writes the record through the store, which reads it back equal.
        let hidden = store.set_hidden(operation, true).unwrap();
        assert_eq!(hidden.swap_uses(), record.swap_uses());
        assert_eq!(find(), hidden);
    }
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn swap_use_records_written_before_public_swaps_decode_to_the_same_roles() {
    // The roles as the build before Public-paid swaps wrote them.
    #[derive(serde::Serialize)]
    enum EarlierRole {
        Source {
            approval: Option<Box<SwapApproval>>,
            destination_operation: Option<ExecutorOperationId>,
        },
        Destination {
            origin_chain: u64,
            origin_operation: ExecutorOperationId,
            destination_token: Address,
            shields: Vec<B256>,
            outcome: Option<SwapDestinationOutcome>,
        },
    }
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let [source, destination, legacy, origin, linked] =
        std::array::from_fn(|_| ExecutorOperationId::random().unwrap());
    let [source_use, destination_use] = std::array::from_fn(|_| SwapUseId::random().unwrap());
    let token = Address::repeat_byte(10);
    let shields = vec![B256::repeat_byte(70)];
    let outcome = Some(SwapDestinationOutcome::Unfilled);
    store
        .put_operation_fixture(
            source,
            &stored_record(
                source,
                0,
                Some((
                    source_use,
                    EarlierRole::Source {
                        approval: Some(Box::new(approval(5, 6))),
                        destination_operation: Some(linked),
                    },
                )),
            ),
        )
        .unwrap();
    store
        .put_operation_fixture(
            destination,
            &stored_record(
                destination,
                1,
                Some((
                    destination_use,
                    EarlierRole::Destination {
                        origin_chain: 1,
                        origin_operation: origin,
                        destination_token: token,
                        shields: shields.clone(),
                        outcome,
                    },
                )),
            ),
        )
        .unwrap();
    store
        .put_operation_fixture(
            legacy,
            &StoredRecord {
                swap_destination: Some(SwapDestinationRecord {
                    origin_chain: 1,
                    origin_operation: origin,
                    destination_token: token,
                    outcome,
                }),
                ..stored_record::<EarlierRole>(legacy, 2, None)
            },
        )
        .unwrap();

    let records = store.records().unwrap();
    let only_use = |operation: ExecutorOperationId| {
        let record = records
            .iter()
            .find(|record| record.operation() == operation)
            .unwrap();
        let [swap_use] = record.swap_uses() else {
            panic!("the record holds one swap use");
        };
        assert_eq!(record.active_swap_use(), Some(swap_use.id()));
        assert_eq!(swap_use.public_swap(), None);
        (swap_use.id(), swap_use.role().clone())
    };
    assert_eq!(
        only_use(source),
        (
            source_use,
            SwapUseRole::Source {
                approval: Some(Box::new(approval(5, 6))),
                destination_operation: Some(linked),
            }
        )
    );
    assert_eq!(
        only_use(destination),
        (
            destination_use,
            SwapUseRole::Destination {
                origin_chain: 1,
                origin_operation: origin,
                destination_token: token,
                shields,
                outcome,
            }
        )
    );
    // A version-1 destination link still projects to a destination use of its origin.
    assert_eq!(
        only_use(legacy),
        (
            SwapUseId::first(origin),
            SwapUseRole::Destination {
                origin_chain: 1,
                origin_operation: origin,
                destination_token: token,
                shields: Vec::new(),
                outcome,
            }
        )
    );
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn public_swap_records_hold_only_their_known_fields_and_hide_the_signed_batch() {
    fn keys(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<_> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }
    let swap = stored_public_swap(public_swap_order_path()).decode();
    let json = serde_json::to_value(&swap).unwrap();

    // A new field, such as a key of the Public account, fails here until it is reviewed. The
    // transaction request and the terms shared with other swaps keep their own shapes.
    assert_eq!(
        keys(&json),
        [
            "approval",
            "bridge",
            "intent",
            "observations",
            "path",
            "transactions"
        ]
    );
    assert_eq!(keys(&json["intent"]), ["bridged_token", "order"]);
    assert_eq!(
        keys(&json["approval"]),
        [
            "bounds",
            "destination",
            "max_gas_cost",
            "on_shield_failure",
            "price_acknowledged",
            "price_verified",
            "sell_token"
        ]
    );
    assert_eq!(keys(&json["approval"]["destination"]), ["address", "setup"]);
    let transaction = &json["transactions"][0];
    assert_eq!(
        keys(transaction),
        [
            "deposit_scan_from_block",
            "hash",
            "inclusion",
            "kind",
            "submitted_from_block",
            "transaction"
        ]
    );
    assert_eq!(
        keys(&transaction["inclusion"]),
        ["finalized", "observation", "succeeded"]
    );
    assert_eq!(keys(&json["path"]), ["Order"]);
    let order = &json["path"]["Order"];
    assert_eq!(
        keys(order),
        [
            "batch",
            "buy_token",
            "permit",
            "proxy",
            "submission",
            "submission_status",
            "uid"
        ]
    );
    assert_eq!(
        keys(&order["permit"]),
        ["deadline", "nonce", "signature", "value"]
    );
    assert_eq!(keys(&order["batch"]), ["calldata", "deadline", "nonce"]);
    assert_eq!(keys(&order["submission"]), ["quote_id", "signature"]);
    let observations = &json["observations"];
    assert_eq!(
        keys(observations),
        [
            "bridge_handoff",
            "bridge_outcome",
            "bridge_refund",
            "cancelled",
            "deposit_ruled_out",
            "deposited",
            "expired",
            "held_by_proxy",
            "trade_amounts",
            "traded",
            "withdrawn"
        ]
    );
    assert_eq!(
        keys(&observations["held_by_proxy"]),
        ["amount", "observation"]
    );
    assert_eq!(
        keys(&observations["deposited"]),
        ["input_amount", "output_amount"]
    );

    // The batch's calldata holds its signature, so no formatting of the record shows it.
    let calldata = alloy::hex::encode(PUBLIC_SWAP_BATCH_CALLDATA);
    let batch = swap.order().unwrap().batch();
    assert_eq!(&batch.calldata()[..], PUBLIC_SWAP_BATCH_CALLDATA);
    assert!(!format!("{batch:?}").contains(&calldata));
    assert!(!format!("{swap:?}").contains(&calldata));

    // Neither does it show the permit's signature. An order recorded before permits has no
    // such field, and decodes as an order without one.
    let signature = alloy::hex::encode(PUBLIC_SWAP_PERMIT_SIGNATURE);
    let permit = swap.order().unwrap().permit().unwrap();
    assert_eq!(permit.signature(), &PUBLIC_SWAP_PERMIT_SIGNATURE);
    assert!(!format!("{permit:?}").contains(&signature));
    assert!(!format!("{swap:?}").contains(&signature));
    let mut earlier = order.clone();
    earlier.as_object_mut().unwrap().remove("permit").unwrap();
    let earlier: PublicSwapOrder = serde_json::from_value(earlier).unwrap();
    assert!(earlier.permit().is_none());
    assert_eq!(earlier.batch(), batch);
}

#[test]
fn public_swap_claims_take_a_destination_account_and_cancel_before_signing() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let delegate = accepted_delegate(137);
    let token = Address::repeat_byte(10);
    let executor = Address::repeat_byte(9);
    // Direct deposits, which the Public account's other swaps never refuse.
    let claim = |id, account: SwapAccountChoice| PublicSwapClaim {
        id,
        origin_chain: 1,
        source: Address::repeat_byte(0x50),
        source_scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: TEST_WALLET_ID.to_owned(),
        },
        account,
        delegate,
        destination_token: token,
        bridged_token: Address::repeat_byte(5),
        order: false,
        approval: public_swap_approval(
            5,
            match account {
                SwapAccountChoice::New(_) => SwapApprovedAccount {
                    address: None,
                    setup: true,
                },
                SwapAccountChoice::Existing(_) => SwapApprovedAccount {
                    address: Some(executor),
                    setup: false,
                },
            },
        ),
        now: 500,
    };
    let find = |operation| {
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };
    let [new, existing] = std::array::from_fn(|_| ExecutorOperationId::random().unwrap());
    let [first, second, third] = std::array::from_fn(|_| SwapUseId::random().unwrap());

    // A new account is allocated with the use and the token it receives. Claiming again
    // returns it unchanged.
    let allocated = store
        .claim_public_swap(claim(first, SwapAccountChoice::New(new)))
        .unwrap();
    assert_eq!(
        allocated.purpose_summary(),
        Some(SWAP_DESTINATION_PURPOSE_SUMMARY)
    );
    assert_eq!(allocated.assets(), [ExecutorAsset::Erc20(token)]);
    assert_eq!(allocated.active_swap_use(), Some(first));
    let (swap_use, swap) = allocated.public_swap_use(first).unwrap();
    assert!(swap_use.is_fresh());
    assert_eq!(
        (swap.intent(), swap.path()),
        (
            PublicSwapIntent {
                bridged_token: Address::repeat_byte(5),
                order: false,
            },
            None
        )
    );
    assert_eq!(
        store
            .claim_public_swap(claim(first, SwapAccountChoice::New(new)))
            .unwrap(),
        allocated
    );
    assert_eq!(find(new), allocated);

    // An account that is set up and idle takes the swap as a later use, and no other swap
    // while that use claims it.
    store.reserve(existing, delegate, None, &[]).unwrap();
    let observed = set_up(&store, existing, delegate, executor);
    let reuse = |id| store.claim_public_swap(claim(id, SwapAccountChoice::Existing(existing)));
    let reused = reuse(second).unwrap();
    assert_eq!(reused.active_swap_use(), Some(second));
    assert!(!reused.public_swap_use(second).unwrap().0.is_fresh());
    assert!(reused.assets().contains(&ExecutorAsset::Erc20(token)));
    assert!(matches!(
        reuse(third),
        Err(ExecutorStoreError::SwapUseActive)
    ));

    // Cancelling before the account issued anything frees it for another swap.
    assert_eq!(
        store.cancel_public_swap(existing, second).unwrap(),
        SwapUseRelease::Released
    );
    let released = find(existing);
    assert_eq!(released.active_swap_use(), None);
    assert!(released.swap_use(second).unwrap().is_stopped());
    reuse(third).unwrap();

    // A shield the account issued for the use can still run, so the use keeps its claim.
    store
        .record_swap_destination_shield(
            existing,
            third,
            hook(
                ExecutorPayloadPurpose::SwapDestinationShield,
                1,
                70,
                delegate,
                observed,
                Vec::new(),
            ),
        )
        .unwrap();
    assert_eq!(
        store.cancel_public_swap(existing, third).unwrap(),
        SwapUseRelease::IssuedWorkRemains
    );
    assert_eq!(find(existing).active_swap_use(), Some(third));

    // A deposit's path that was recorded before its deposit was handed off, as when its
    // preflight failed, takes the terms of a later quote, and its swap can still be cancelled.
    let bridge = stored_public_swap(PublicSwapPath::Deposit).bridge.unwrap();
    let earlier = AcrossOrderTerms {
        output_amount: U256::from(1),
        ..bridge
    };
    store
        .record_public_swap_path(new, first, PublicSwapPath::Deposit, earlier)
        .unwrap();
    let signed = store
        .record_public_swap_path(new, first, PublicSwapPath::Deposit, bridge)
        .unwrap();
    let (_, recorded) = signed.public_swap_use(first).unwrap();
    assert_eq!(
        (recorded.path(), recorded.bridge()),
        (Some(&PublicSwapPath::Deposit), Some(&bridge))
    );
    // A full review before hand-off discards stale signed delivery terms.
    let mut replacement = recorded.approval().clone();
    replacement.bounds.destination_minimum = Some(bridge.output_amount);
    replacement.bounds.destination_setup_fee = Some(U256::from(100));
    let reapproved = store
        .reapprove_public_swap(new, first, replacement.clone())
        .unwrap();
    let (_, reapproved) = reapproved.public_swap_use(first).unwrap();
    assert_eq!((reapproved.path(), reapproved.bridge()), (None, None));
    store
        .record_public_swap_path(new, first, PublicSwapPath::Deposit, bridge)
        .unwrap();
    let (unsent, unsent_use) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    store
        .claim_public_swap(claim(unsent_use, SwapAccountChoice::New(unsent)))
        .unwrap();
    store
        .record_public_swap_path(unsent, unsent_use, PublicSwapPath::Deposit, bridge)
        .unwrap();
    assert_eq!(
        store.cancel_public_swap(unsent, unsent_use).unwrap(),
        SwapUseRelease::Released
    );
    assert!(find(unsent).swap_use(unsent_use).unwrap().is_stopped());

    // An address binds only once the account holds it.
    assert!(matches!(
        store.bind_public_swap_destination(new, first, executor),
        Err(ExecutorStoreError::OperationMismatch)
    ));

    // A deposit is handed off only after its path is recorded. An approval needs none.
    let handed_off = |kind, hash: u8| PublicSwapTransaction {
        kind,
        transaction: alloy::rpc::types::TransactionRequest::default().to(Address::repeat_byte(11)),
        hash: B256::repeat_byte(hash),
        inclusion: None,
        submitted_from_block: Some(19),
        deposit_scan_from_block: None,
    };
    let deposit = handed_off(PublicSwapTransactionKind::Deposit, 81);
    assert!(matches!(
        store.record_public_swap_transaction(existing, third, deposit.clone()),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    let approved = handed_off(PublicSwapTransactionKind::Approval, 80);
    store
        .record_public_swap_transaction(existing, third, approved.clone())
        .unwrap();
    store
        .record_public_swap_transaction(new, first, deposit.clone())
        .unwrap();
    // Once its deposit is handed off, the path keeps its terms and the swap ends through its
    // own flow.
    assert!(matches!(
        store.reapprove_public_swap(new, first, replacement),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    assert!(matches!(
        store.record_public_swap_path(new, first, PublicSwapPath::Deposit, earlier),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    assert!(matches!(
        store.cancel_public_swap(new, first),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    store
        .record_public_swap_path(new, first, PublicSwapPath::Deposit, bridge)
        .unwrap();
    // The same hand-off again changes nothing, and its hash names no other transaction.
    let again = store
        .record_public_swap_transaction(new, first, deposit.clone())
        .unwrap();
    assert_eq!(
        again.public_swap_use(first).unwrap().1.transactions(),
        std::slice::from_ref(&deposit)
    );
    assert!(matches!(
        store.record_public_swap_transaction(
            new,
            first,
            handed_off(PublicSwapTransactionKind::Withdrawal, 81)
        ),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    assert!(matches!(
        store.record_public_swap_transaction(
            new,
            first,
            handed_off(PublicSwapTransactionKind::Deposit, 82)
        ),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    // Two stale callers racing to hand off distinct deposits leave exactly one durable call.
    let competing = ExecutorOperationId::random().unwrap();
    let competing_use = SwapUseId::random().unwrap();
    store
        .claim_public_swap(claim(competing_use, SwapAccountChoice::New(competing)))
        .unwrap();
    store
        .record_public_swap_path(competing, competing_use, PublicSwapPath::Deposit, bridge)
        .unwrap();
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let attempts: Vec<_> = [83, 84]
            .into_iter()
            .map(|hash| {
                let barrier = &barrier;
                let store = &store;
                let transaction = handed_off(PublicSwapTransactionKind::Deposit, hash);
                scope.spawn(move || {
                    barrier.wait();
                    store.record_public_swap_transaction(competing, competing_use, transaction)
                })
            })
            .collect();
        attempts
            .into_iter()
            .map(|attempt| attempt.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        find(competing)
            .public_swap_use(competing_use)
            .unwrap()
            .1
            .transactions()
            .len(),
        1
    );
    // An inclusion belongs to a transaction that was handed off.
    let inclusion = PublicSwapInclusion {
        observation: SwapObservation {
            block: BlockNumHash::new(20, B256::repeat_byte(20)),
            transaction_hash: Some(deposit.hash),
        },
        succeeded: false,
        finalized: false,
    };
    assert!(matches!(
        store.record_public_swap_inclusion(new, first, approved.hash, inclusion),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    let included = store
        .record_public_swap_inclusion(new, first, deposit.hash, inclusion)
        .unwrap();
    assert_eq!(
        included.public_swap_use(first).unwrap().1.transactions()[0].inclusion,
        Some(inclusion)
    );

    assert!(
        !included
            .public_swap_use(first)
            .unwrap()
            .1
            .is_finished(false, 500)
    );
    let finalized = PublicSwapInclusion {
        finalized: true,
        ..inclusion
    };
    store
        .record_public_swap_inclusion(new, first, deposit.hash, finalized)
        .unwrap();
    let late = store
        .record_public_swap_inclusion(new, first, deposit.hash, inclusion)
        .unwrap();
    let (_, late) = late.public_swap_use(first).unwrap();
    assert_eq!(late.transactions()[0].inclusion, Some(finalized));
    assert!(late.is_finished(false, 500));
    // Legacy failure receipts never acquire finality merely by being decoded.
    let mut legacy = serde_json::to_value(finalized).unwrap();
    legacy.as_object_mut().unwrap().remove("finalized");
    assert!(
        !serde_json::from_value::<PublicSwapInclusion>(legacy)
            .unwrap()
            .finalized
    );

    // What the chain shows of a stopped swap is still recorded: its transactions, their
    // inclusions and its observations. It signs no path.
    assert!(find(existing).swap_use(second).unwrap().is_stopped());
    store
        .record_public_swap_transaction(existing, second, approved.clone())
        .unwrap();
    store
        .record_public_swap_inclusion(existing, second, approved.hash, inclusion)
        .unwrap();
    let observations = PublicSwapObservations {
        bridge_refund: Some(inclusion.observation),
        ..PublicSwapObservations::default()
    };
    let stopped = store
        .record_public_swap_observations(existing, second, observations)
        .unwrap();
    let (_, stopped) = stopped.public_swap_use(second).unwrap();
    assert_eq!(stopped.observations(), observations);
    assert_eq!(stopped.transactions()[0].inclusion, Some(inclusion));
    assert!(matches!(
        store.record_public_swap_path(existing, second, PublicSwapPath::Deposit, bridge),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_public_account_has_one_open_swap_per_bought_and_sold_token_on_a_chain() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    // The Public account pays on chain 1 for swaps that deliver to two other chains, each
    // kept in its own chain's store.
    let store = ExecutorStore::new(db.clone(), view.clone(), 137).unwrap();
    let other_store = ExecutorStore::new(db.clone(), view.clone(), 42_161).unwrap();
    let claim = |sell: u8, bridged: u8, order, now| PublicSwapClaim {
        id: SwapUseId::random().unwrap(),
        origin_chain: 1,
        source: Address::repeat_byte(0x50),
        source_scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: TEST_WALLET_ID.to_owned(),
        },
        account: SwapAccountChoice::New(ExecutorOperationId::random().unwrap()),
        delegate: Address::repeat_byte(1),
        destination_token: Address::repeat_byte(10),
        bridged_token: Address::repeat_byte(bridged),
        order,
        approval: public_swap_approval(
            sell,
            SwapApprovedAccount {
                address: None,
                setup: true,
            },
        ),
        now,
    };

    // An order that sells token 5 for token 6, valid until 1000 like its hook batch.
    let open = claim(5, 6, true, 500);
    let (open_id, operation) = (open.id, open.account.operation());
    store.claim_public_swap(open).unwrap();
    let buys_same = |claimed: PublicSwapClaim, available: Option<u64>| {
        matches!(
            other_store.claim_public_swap(claimed),
            Err(ExecutorStoreError::PublicSwapBuysSameToken {
                swap,
                chain_id: 137,
                available_at,
            }) if swap == open_id && available_at == available
        )
    };
    let sells_same = |claimed: PublicSwapClaim, expires: Option<u64>| {
        matches!(
            other_store.claim_public_swap(claimed),
            Err(ExecutorStoreError::PublicSwapSellsSameToken {
                swap,
                chain_id: 137,
                expires_at,
            }) if swap == open_id && expires_at == expires
        )
    };
    // Before the order is signed its claim already holds both tokens.
    assert!(buys_same(claim(7, 6, true, 500), None));
    assert!(sells_same(claim(5, 8, true, 500), None));
    // The read-only check gives a draft with the same tokens the same refusals, before it has
    // a destination account or an approval, and none for other tokens.
    let conflict = |sell: u8, bridged: u8| {
        other_store
            .public_swap_source_conflict(&PublicSwapSourceTerms {
                id: None,
                origin_chain: 1,
                source: Address::repeat_byte(0x50),
                source_scope: PublicAccountScope::PrivateWallet {
                    wallet_uuid: TEST_WALLET_ID.to_owned(),
                },
                sell_token: Address::repeat_byte(sell),
                bridged_token: Address::repeat_byte(bridged),
                order: true,
                now: 500,
            })
            .unwrap()
    };
    assert!(matches!(
        conflict(7, 6),
        Some(ExecutorStoreError::PublicSwapBuysSameToken {
            swap,
            chain_id: 137,
            available_at: None,
        }) if swap == open_id
    ));
    assert!(matches!(
        conflict(5, 8),
        Some(ExecutorStoreError::PublicSwapSellsSameToken {
            swap,
            chain_id: 137,
            expires_at: None,
        }) if swap == open_id
    ));
    assert!(conflict(9, 8).is_none());
    let signed = stored_public_swap(public_swap_order_path());
    store
        .record_public_swap_path(
            operation,
            open_id,
            signed.path.unwrap(),
            signed.bridge.unwrap(),
        )
        .unwrap();

    // A signed order keeps its path and terms, and its swap is never cancelled as unsigned.
    let mut replacement = public_swap_approval(
        5,
        SwapApprovedAccount {
            address: None,
            setup: true,
        },
    );
    replacement.bounds.destination_minimum = Some(U256::from(9_900));
    replacement.bounds.destination_setup_fee = Some(U256::from(100));
    assert!(matches!(
        store.reapprove_public_swap(operation, open_id, replacement),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    assert!(matches!(
        store.record_public_swap_path(
            operation,
            open_id,
            public_swap_order_path(),
            AcrossOrderTerms {
                output_amount: U256::from(1),
                ..signed.bridge.unwrap()
            },
        ),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    assert!(matches!(
        store.cancel_public_swap(operation, open_id),
        Err(ExecutorStoreError::OperationMismatch)
    ));

    // While the order can fill, no other order buys its token, and nothing else sells the
    // token it sells. A direct deposit buys nothing, so only its Sell token is judged.
    assert!(buys_same(claim(7, 6, true, 1_000), None));
    assert!(sells_same(claim(5, 8, true, 1_000), Some(1_000)));
    assert!(sells_same(claim(5, 5, false, 1_000), Some(1_000)));
    // A Public account shared between wallets pays for no swap.
    assert!(matches!(
        other_store.claim_public_swap(PublicSwapClaim {
            source_scope: PublicAccountScope::Global,
            ..claim(9, 8, true, 500)
        }),
        Err(ExecutorStoreError::PublicSwapSourceShared)
    ));
    // A refused claim allocates nothing, and neither does the read-only check.
    assert!(other_store.records().unwrap().is_empty());
    // Swaps of other tokens run beside the order.
    other_store
        .claim_public_swap(claim(9, 8, true, 500))
        .unwrap();
    other_store
        .claim_public_swap(claim(11, 6, false, 500))
        .unwrap();

    // Finalized settlement state proves cancellation, so the order no longer sells. This
    // hashless observation survives reload; a provisional receipt cannot release admission.
    // Its signed batch can still deposit token 6 from the proxy until its deadline.
    store
        .record_public_swap_observations(
            operation,
            open_id,
            PublicSwapObservations {
                cancelled: Some(SwapObservation {
                    block: BlockNumHash::new(21, B256::repeat_byte(21)),
                    transaction_hash: None,
                }),
                ..PublicSwapObservations::default()
            },
        )
        .unwrap();
    assert!(buys_same(claim(7, 6, true, 1_000), Some(1_001)));
    other_store
        .claim_public_swap(claim(5, 12, true, 1_000))
        .unwrap();
    other_store
        .claim_public_swap(claim(7, 6, true, 1_001))
        .unwrap();
    drop(store);
    drop(other_store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
