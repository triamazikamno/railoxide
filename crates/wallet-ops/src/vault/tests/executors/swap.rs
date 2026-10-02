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

    // The delegation-only setup wins nonce 0, so the swap's pre-hook uses k = 1.
    let before_setup =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.reconcile(operation, before_setup, &[]).unwrap();
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
    let setup_won = (
        setup,
        ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(4),
            ExecutorExecutionResult::Executed,
        ),
    );
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.reconcile(operation, observed, &[setup_won]).unwrap();

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
        anchors: vec![SwapAnchorObservation {
            source: Address::repeat_byte(10),
            block: observed.block(),
            block_timestamp: 1_700_000_000,
            updated_at: Some(1_699_990_000),
        }],
        destination_minimum: None,
        gas_share_bps: None,
        gas_estimate: None,
        gas_allowance: None,
        gas_price_wei: None,
        valid_for_secs: None,
        destination_shield_fee_bps: None,
        delivery_allowance: None,
        destination_setup_fee: None,
    };
    let attempt = |digest: u8, valid_to: u32, hooks: u8, post_nonce: u64| SwapAttempt {
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
    store
        .reserve_swap_destination(
            destination,
            delegate,
            SwapDestinationRecord {
                origin_chain: 137,
                origin_operation: ExecutorOperationId::random().unwrap(),
                destination_token: Address::repeat_byte(6),
                outcome: None,
            },
        )
        .unwrap();
    set_up(&store, destination, delegate, destination_executor);
    let mut unrelated = first.clone();
    unrelated.uid = OrderUid::new(B256::repeat_byte(30), destination_executor, 1_000);
    assert!(matches!(
        store.record_swap_attempt(destination, unrelated),
        Err(ExecutorStoreError::OperationMismatch)
    ));
    store.record_swap_attempt(operation, first.clone()).unwrap();

    // The pre-hook reserves its inputs against every other operation.
    let other = ExecutorOperationId::random().unwrap();
    store.reserve(other, delegate, None, &[]).unwrap();
    store.bind_address(other, Address::repeat_byte(40)).unwrap();
    store.reconcile(other, observed, &[]).unwrap();
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

    // The exception does not extend to other payloads: an ordinary operation
    // still cannot follow a pre-hook whose outcome is unknown.
    let consumed =
        ExecutorNonceObservation::new(BlockNumHash::new(13, B256::repeat_byte(13)), U256::from(2));
    // The spent pre-hook nonce resolves the pre-hook, but the post-hook can still run.
    assert!(
        store
            .reconcile(operation, consumed, &[setup_won])
            .unwrap()
            .has_unresolved_issued_work()
    );
    assert!(matches!(
        store.record_issued(
            operation,
            hook(
                ExecutorPayloadPurpose::Operation,
                2,
                42,
                delegate,
                consumed,
                Vec::new()
            )
        ),
        Err(ExecutorStoreError::OutstandingNonce)
    ));

    // After expiry the nonce is still k, and a retry waits for recorded death.
    store.reconcile(operation, observed, &[setup_won]).unwrap();
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
    store
        .reconcile(operation, after_fill, &[setup_won])
        .unwrap();
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
    // Without a fresh nonce, recorded outcomes can't resolve a hook.
    assert!(
        store
            .invalidate_observation(operation)
            .unwrap()
            .has_recorded_unresolved_issued_work()
    );
    store
        .reconcile(operation, after_fill, &[setup_won])
        .unwrap();
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
    store.reconcile(operation, before_setup, &[]).unwrap();
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
    let setup_won = (
        setup,
        ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(4),
            ExecutorExecutionResult::Executed,
        ),
    );
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.reconcile(operation, observed, &[setup_won]).unwrap();

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
            sell_amount: U256::from(9_975),
            unshield_amount: Some(U256::from(10_000)),
            unshield_fee_bps: U256::from(25),
            buy_amount: U256::from(9_975),
            private_minimum: U256::from(9_975),
            shield_fee_bps: U256::from(25),
            slippage_bps: 50,
            pre_hook_gas_limit: 900_000,
            post_hook_gas_limit: None,
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
    store.reconcile(operation, before_setup, &[]).unwrap();
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
    let setup_won = (
        setup,
        ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(4),
            ExecutorExecutionResult::Executed,
        ),
    );
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.reconcile(operation, observed, &[setup_won]).unwrap();

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
                    sell_amount: U256::from(9_975),
                    unshield_amount: Some(U256::from(10_000)),
                    unshield_fee_bps: U256::from(25),
                    buy_amount: U256::from(9_975),
                    private_minimum: U256::from(9_975),
                    shield_fee_bps: U256::ZERO,
                    slippage_bps: 50,
                    pre_hook_gas_limit: 900_000,
                    post_hook_gas_limit: post_hook.then_some(400_000),
                    hook_cost: Some(U256::ZERO),
                    anchors: Vec::new(),
                    destination_minimum: Some(U256::from(9_900)),
                    gas_share_bps: None,
                    gas_estimate: None,
                    gas_allowance: None,
                    gas_price_wei: None,
                    valid_for_secs: None,
                    destination_shield_fee_bps: None,
                    delivery_allowance: None,
                    destination_setup_fee: None,
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
    store.reconcile(operation, before_setup, &[]).unwrap();
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
    let setup_won = (
        B256::repeat_byte(3),
        ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(4),
            ExecutorExecutionResult::Executed,
        ),
    );
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    store.reconcile(operation, observed, &[setup_won]).unwrap();
    observed
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
    let delegate = Address::repeat_byte(1);
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
    let uid = OrderUid::new(B256::repeat_byte(30), executor, 1_000);
    origin_store
        .record_swap_attempt(
            origin,
            SwapAttempt {
                terms: SwapTerms::new(
                    sell,
                    buy,
                    SwapRecipient::new(U256::from(7), [8; 32]),
                    B256::repeat_byte(3),
                ),
                proof: SwapProof::new(B256::repeat_byte(20), inputs.clone()),
                uid,
                submission: None,
                delivery: SwapDelivery::Bridge(BridgeDelivery {
                    provider: BridgeProvider::Across,
                    destination_chain: 137,
                    receiver: destination_executor,
                    destination_token: token,
                    surplus: BridgeSurplus::KeepInAccount,
                    private: Some(BridgePrivateDelivery {
                        on_shield_failure: BridgeShieldFailure::KeepOnDestination,
                    }),
                }),
                bounds: SwapApprovedBounds {
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
                    gas_share_bps: None,
                    gas_estimate: None,
                    gas_allowance: None,
                    gas_price_wei: None,
                    valid_for_secs: None,
                    destination_shield_fee_bps: Some(U256::from(25)),
                    delivery_allowance: Some(U256::from(120)),
                    destination_setup_fee: Some(U256::from(3)),
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
            },
        )
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
    // Canonical nonce consumption, rather than a provider outcome, resolves the signature.
    let setup = &held.issued()[0];
    let consumed = store
        .reconcile(
            destination,
            ExecutorNonceObservation::new(block, U256::from(2)),
            &[(setup.hash(), setup.inclusion().unwrap())],
        )
        .unwrap();
    assert!(!consumed.is_outstanding_at(&consumed.issued()[1], U256::from(2)));
    assert!(!consumed.has_competing_payloads());
    assert!(!consumed.has_unresolved_issued_work());
    drop(origin_store);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
