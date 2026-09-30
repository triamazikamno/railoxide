use super::*;
use crate::{
    BroadcasterFeePolicyStatus, DesktopPrivateSpendAuthorization, PublicBroadcasterCandidate,
    SwapSetupStatus, swap_setup_status,
};
use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use alloy::primitives::address;
use broadcaster_core::crypto::railgun::AddressData;

pub(super) const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
pub(super) const USDC: Address = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");

pub(super) fn broadcaster(delegate: Address) -> PublicBroadcasterCandidate {
    PublicBroadcasterCandidate {
        chain_id: 1,
        railgun_address: "0zk-broadcaster".to_owned(),
        identifier: None,
        token: Address::repeat_byte(0x33),
        fee: U256::from(10),
        fees_id: "fees-id".to_owned(),
        fee_expiration: std::time::SystemTime::now() + Duration::from_mins(1),
        reliability: 0.9,
        available_wallets: 1,
        version: "8.2.3".to_owned(),
        relay_adapt: Address::repeat_byte(0x44),
        relay_adapt_7702: Some(delegate),
        required_poi_list_keys: Vec::new(),
        viewing_public_key: [1; 32],
        address_data: AddressData {
            master_public_key: U256::ONE,
            viewing_public_key: [1; 32],
        },
        fee_policy_status: BroadcasterFeePolicyStatus::UnknownAnchor,
    }
}

pub(super) fn password() -> DesktopPrivateSpendAuthorization {
    DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(TEST_PASSWORD.into()))
}

/// Terms approved with a new swap's setup. Only the pair and delivery bind its first order; a
/// fresh review sets the order's bounds.
pub(super) fn setup_approval(sell: Address, buy: Address, delivery: SwapDelivery) -> SwapApproval {
    SwapApproval {
        bounds: SwapApprovedBounds {
            sell_amount: U256::from(997_500),
            unshield_amount: Some(U256::from(1_000_000)),
            unshield_fee_bps: U256::from(25),
            buy_amount: U256::ONE,
            private_minimum: U256::ONE,
            shield_fee_bps: U256::from(25),
            slippage_bps: 50,
            pre_hook_gas_limit: 1,
            post_hook_gas_limit: Some(1),
            hook_cost: Some(U256::ZERO),
            anchors: Vec::new(),
            destination_minimum: None,
        },
        price_verified: Some(false),
        price_acknowledged: true,
        delivery,
        tokens: Some(crate::vault::SwapApprovalTokens { sell, buy }),
    }
}

#[tokio::test]
async fn each_swap_setup_gets_its_own_executor_and_authorizes_only_that_executor() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let delegate = chain.accepted_executor_profile().unwrap().delegate();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let first = owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            &password(),
        )
        .await
        .unwrap();
    let second = owner
        .prepare_swap_setup(
            ExecutorOperationId::random().unwrap(),
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            &password(),
        )
        .await
        .unwrap();
    assert_ne!(first.context().executor, second.context().executor);
    assert!(
        owner
            .prepare_swap_setup(
                operation,
                broadcaster(delegate),
                setup_approval(WETH, USDC, SwapDelivery::Reshield),
                &password(),
            )
            .await
            .is_err(),
        "a new swap never takes over a recorded swap's executor"
    );
    let resumed = owner
        .resume_swap_setup(operation, broadcaster(delegate), &password())
        .await
        .unwrap();
    assert_eq!(resumed.context().executor, first.context().executor);

    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            Address::repeat_byte(0x33),
            U256::from(9),
            [7; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    let executor = resumed.context().executor;
    // The fee-only setup: one private transaction and no actions.
    let call = railgun_wallet::TransactionCall {
        to: executor,
        data: RelayAdapt7702::executeCall {
            _transactions: vec![Transaction {
                proof: SnarkProof::default(),
                merkleRoot: B256::ZERO,
                nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
                commitments: Vec::new(),
                boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
                unshieldPreimage: CommitmentPreimage::empty(),
            }],
            _actionData: RelayAdapt7702ActionData {
                requireSuccess: true,
                minGasLimit: U256::ZERO,
                calls: Vec::new(),
            },
            _nonce: resumed.context().execution_nonce,
            _signature: Bytes::new(),
        }
        .abi_encode()
        .into(),
    };
    let issued = owner
        .issue_operation(&resumed, &call, std::slice::from_ref(&input), &password())
        .await
        .unwrap();
    let Some([authorization]) = issued.transaction().authorization_list.as_deref() else {
        panic!("the setup carries exactly one delegation authorization");
    };
    assert_eq!(authorization.recover_authority().unwrap(), executor);
    assert_ne!(
        authorization.recover_authority().unwrap(),
        second.context().executor
    );
    assert_eq!(authorization.inner().address, delegate);
    assert_eq!(issued.transaction().transaction_type, Some(4));

    // An unconfirmed setup can select another broadcaster and issue a replacement
    // at the same nonce, reusing its fee note instead of demanding a second note.
    let mut another = broadcaster(delegate);
    another.railgun_address = "0zk-another-broadcaster".into();
    another.fees_id = "new-fees".into();
    let replacement = owner
        .resume_swap_setup(operation, another.clone(), &password())
        .await
        .unwrap();
    replacement.require_broadcaster(&another).unwrap();
    assert_eq!(replacement.context(), resumed.context());
    assert!(
        owner
            .available_inputs(vec![input.clone()])
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        owner
            .inputs_for_preparation(vec![input.clone()], &replacement)
            .unwrap()
            .len(),
        1
    );
    let mut replacement_call = call;
    let mut decoded = RelayAdapt7702::executeCall::abi_decode(&replacement_call.data).unwrap();
    decoded._actionData.minGasLimit = U256::ONE;
    replacement_call.data = decoded.abi_encode().into();
    let replaced = owner
        .issue_operation(
            &replacement,
            &replacement_call,
            std::slice::from_ref(&input),
            &password(),
        )
        .await
        .unwrap();
    assert_ne!(replaced.payload_hash(), issued.payload_hash());
    let record = owner
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(record.issued().len(), 2);
    assert_eq!(
        record.reserved_inputs(),
        vec![ExecutorInputIdentity::from_utxo(&input)]
    );
    owner.stop_swap_setup(operation).unwrap();
    assert!(owner.swap_setup_preview(operation).is_err());
    assert!(
        owner
            .resume_swap_setup(operation, another, &password())
            .await
            .is_err()
    );
    // A preparation retained by an in-flight setup must not issue more payloads after stop.
    decoded._actionData.minGasLimit = U256::from(2);
    replacement_call.data = decoded.abi_encode().into();
    assert!(
        owner
            .issue_operation(&replacement, &replacement_call, &[input], &password())
            .await
            .is_err()
    );
    let stopped = owner
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert!(stopped.is_swap_setup_stopped());
    assert_eq!(stopped.issued(), record.issued());
    assert_eq!(stopped.reserved_inputs(), record.reserved_inputs());
    owner.shutdown().await;
    drop(owner);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn swap_setup_completes_only_with_observed_delegation_and_consumed_nonce() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let profile =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .unwrap()
            .accepted_executor_profile()
            .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let executor = Address::repeat_byte(2);
    store
        .reserve(operation, profile.delegate(), Some("Private swap"), &[])
        .unwrap();
    store.bind_address(operation, executor).unwrap();
    let signed_at =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.reconcile(operation, signed_at, &[]).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                profile.delegate(),
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), signed_at, Vec::new()),
            ),
        )
        .unwrap();
    let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
    let delegated = [
        EIP7702_DELEGATION_DESIGNATOR.as_slice(),
        profile.delegate().as_slice(),
    ]
    .concat();
    let reconcile = |nonce: u64, result: Option<ExecutorExecutionResult>| {
        let inclusions = result
            .map(|result| {
                (
                    setup,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(4),
                        result,
                    ),
                )
            })
            .into_iter()
            .collect::<Vec<_>>();
        store
            .reconcile(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::from(nonce)),
                &inclusions,
            )
            .unwrap()
    };
    let status =
        |record: &ExecutorRecord, code: &[u8]| swap_setup_status(record, confirmed, code, profile);

    // Handed off but not yet included at the confirmed block.
    assert_eq!(
        status(&reconcile(0, None), &delegated),
        SwapSetupStatus::Pending
    );
    // Included and successful, but the authorization was not applied.
    let skipped = reconcile(0, Some(ExecutorExecutionResult::MissingEffects));
    assert_eq!(status(&skipped, &[]), SwapSetupStatus::MissingDelegation);

    let executed = reconcile(1, Some(ExecutorExecutionResult::Executed));
    let SwapSetupStatus::Delegated(handle) = status(&executed, &delegated) else {
        panic!("the confirmed delegation completes the setup");
    };
    assert_eq!(
        (
            handle.operation(),
            handle.executor(),
            handle.setup_payload()
        ),
        (operation, executor, setup)
    );
    assert_eq!(status(&executed, &[]), SwapSetupStatus::MissingDelegation);
    // Code read at any block other than the nonce observation is not evidence.
    assert_eq!(
        swap_setup_status(&executed, signed_at.block(), &delegated, profile),
        SwapSetupStatus::Pending
    );

    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
