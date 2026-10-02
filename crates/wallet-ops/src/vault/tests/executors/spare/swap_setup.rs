use super::*;
use crate::{
    BroadcasterFeePolicyStatus, DesktopPrivateSpendAuthorization, ExecutorPrivateFeeLimitExceeded,
    PrivateBridgeSetupPreparation, PublicBroadcasterCandidate, SwapSetupStatus,
    is_swap_destination_record, prepare_private_bridge_setup, swap_setup_status,
};
use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use alloy::primitives::address;
use broadcaster_core::crypto::railgun::AddressData;

pub(super) const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
pub(super) const USDC: Address = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
pub(super) const DESTINATION_CHAIN: u64 = 137;
pub(super) const DESTINATION_TOKEN: Address = address!("3c499c542cef5e3811e1192ce70d8cc03d5c3359");

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
            gas_share_bps: None,
            gas_estimate: None,
            gas_allowance: None,
            gas_price_wei: None,
            valid_for_secs: None,
            destination_shield_fee_bps: None,
            delivery_allowance: None,
            destination_setup_fee: None,
        },
        price_verified: Some(false),
        price_acknowledged: true,
        delivery,
        tokens: Some(crate::vault::SwapApprovalTokens { sell, buy }),
    }
}

/// Terms of a private Bridge swap to the destination chain, before its destination stealth
/// account is derived.
pub(super) fn private_bridge_approval() -> SwapApproval {
    let mut approval = setup_approval(
        WETH,
        USDC,
        SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: DESTINATION_CHAIN,
            receiver: Address::ZERO,
            destination_token: DESTINATION_TOKEN,
            surplus: BridgeSurplus::Reshield,
            private: Some(BridgePrivateDelivery {
                on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
            }),
        }),
    );
    approval.bounds.destination_setup_fee = Some(U256::from(1_000));
    approval
}

/// The destination chain's configuration over the shared mock, which answers as chain 1. A
/// relay answers `eth_chainId` for the destination chain and passes every other request on.
pub(super) async fn destination_chain_config(rpc: &Rpc) -> crate::settings::EffectiveChainConfig {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url: url::Url = format!("http://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let upstream = format!("127.0.0.1:{}", rpc.url.port().unwrap());
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(relay(stream, upstream.clone()));
        }
    });
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(DESTINATION_CHAIN)
            .cloned()
            .unwrap();
    chain.finality_depth = 1;
    chain.rpc_route =
        crate::RpcChainRoute::new(DESTINATION_CHAIN, vec![url]).with_multicall(multicall());
    chain
}

async fn relay(stream: tokio::net::TcpStream, upstream: String) -> std::io::Result<()> {
    let mut stream = BufReader::new(stream);
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = length.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body).await?;
    let request: Value = serde_json::from_slice(&body).unwrap();
    let response = if request["method"] == "eth_chainId" {
        let body = json!({
            "jsonrpc": "2.0", "id": request["id"], "result": format!("0x{DESTINATION_CHAIN:x}")
        })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    } else {
        let mut upstream = tokio::net::TcpStream::connect(upstream).await?;
        upstream
            .write_all(
                format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
            )
            .await?;
        upstream.write_all(&body).await?;
        let mut response = Vec::new();
        upstream.read_to_end(&mut response).await?;
        response
    };
    stream.get_mut().write_all(&response).await
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
            None,
            &password(),
        )
        .await
        .unwrap();
    let second = owner
        .prepare_swap_setup(
            ExecutorOperationId::random().unwrap(),
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            None,
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
                None,
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

#[tokio::test]
async fn private_bridge_setup_reserves_the_destination_first_and_each_setup_stands_alone() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let (origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
    let profile = origin_chain.accepted_executor_profile().unwrap();
    let destination_profile = destination_chain.accepted_executor_profile().unwrap();
    let mut destination_candidate = broadcaster(destination_profile.delegate());
    destination_candidate.chain_id = DESTINATION_CHAIN;
    let owners = |generation| {
        [origin_chain.clone(), destination_chain.clone()].map(|chain| {
            ExecutorOwner::new(
                generation,
                db.clone(),
                view.clone(),
                chain,
                HttpContext::direct_for_tests(),
            )
            .unwrap()
        })
    };
    let [origin, destination] = owners(0);
    let record = |owner: &ExecutorOwner, operation| {
        owner
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };
    let (operation, destination_operation) = (
        ExecutorOperationId::random().unwrap(),
        ExecutorOperationId::random().unwrap(),
    );
    // One software authorization covers both chains.
    let authorization = password();
    let destination_authorization = authorization.for_destination().unwrap();
    let preparation = |candidate, approval| PrivateBridgeSetupPreparation {
        operation,
        destination_operation,
        candidate,
        destination_candidate: destination_candidate.clone(),
        approval,
        authorization: &authorization,
        destination_authorization: &destination_authorization,
    };

    // Nothing is reserved for terms that are not a private Bridge delivery with a destination
    // setup fee limit, or for a swap whose link disagrees with its delivery.
    let mut unbounded = private_bridge_approval();
    unbounded.bounds.destination_setup_fee = None;
    for refused in [
        setup_approval(WETH, USDC, SwapDelivery::Reshield),
        unbounded,
    ] {
        let request = preparation(broadcaster(profile.delegate()), refused);
        assert!(
            prepare_private_bridge_setup(&origin, &destination, request)
                .await
                .is_err()
        );
    }
    for (approval, link) in [
        (private_bridge_approval(), None),
        (
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            Some(destination_operation),
        ),
    ] {
        let candidate = broadcaster(profile.delegate());
        assert!(
            origin
                .prepare_swap_setup(operation, candidate, approval, link, &authorization)
                .await
                .is_err()
        );
    }
    assert!(origin.records().unwrap().is_empty() && destination.records().unwrap().is_empty());

    // The destination account is reserved first, so a swap setup that fails leaves it reserved
    // with no swap naming it.
    let mut unavailable = broadcaster(profile.delegate());
    unavailable.available_wallets = 0;
    let request = preparation(unavailable, private_bridge_approval());
    rpc.hold.send_replace(Some(multicall()));
    {
        let preparing = prepare_private_bridge_setup(&origin, &destination, request);
        tokio::pin!(preparing);
        tokio::select! {
            result = &mut preparing => panic!("destination inspection should be blocked: {}", result.is_ok()),
            () = rpc.wait_for(|requests| requests.iter().any(|request| request["method"] == "eth_call")) => {},
        }
        assert!(origin.records().unwrap().is_empty());
        assert!(is_swap_destination_record(&record(
            &destination,
            destination_operation
        )));
        // Finishing an older swap sweeps this chain while the new destination is still
        // being inspected. Its unlinked reservation must remain available to preparation.
        assert!(!destination.reconcile_swap_destinations().unwrap());
        rpc.hold.send_replace(None);
        assert!(preparing.await.is_err());
    }
    assert!(origin.records().unwrap().is_empty());
    let reserved = record(&destination, destination_operation);
    assert!(is_swap_destination_record(&reserved) && !crate::is_swap_record(&reserved));

    // The retry resumes that account, and the approval saved with the swap names it.
    let request = preparation(broadcaster(profile.delegate()), private_bridge_approval());
    let prepared = prepare_private_bridge_setup(&origin, &destination, request)
        .await
        .unwrap();
    let executor = prepared.destination.context().executor;
    assert_eq!(reserved.address(), Some(executor));
    assert_ne!(prepared.origin.context().executor, executor);
    let SwapDelivery::Bridge(delivery) = prepared.approval.delivery else {
        panic!("the approval keeps its Bridge delivery");
    };
    assert_eq!(delivery.receiver, executor);
    let saved = record(&origin, operation);
    assert_eq!(saved.swap_approval(), Some(&prepared.approval));
    assert_eq!(saved.destination_operation(), Some(destination_operation));
    let serves = SwapDestinationRecord {
        origin_chain: 1,
        origin_operation: operation,
        destination_token: DESTINATION_TOKEN,
        outcome: None,
    };
    assert_eq!(
        record(&destination, destination_operation).swap_destination(),
        Some(serves)
    );

    // The destination setup's fee ceiling is the one approved with the swap, read from the
    // swap's record on its own chain.
    let fee_token = destination_candidate.token;
    destination
        .require_swap_destination_setup_fee(destination_operation, fee_token, U256::from(1_000))
        .unwrap();
    let error = destination
        .require_swap_destination_setup_fee(destination_operation, fee_token, U256::from(1_001))
        .unwrap_err();
    let exceeded = error
        .downcast_ref::<ExecutorPrivateFeeLimitExceeded>()
        .unwrap();
    assert_eq!(
        (exceeded.maximum(), exceeded.required()),
        (U256::from(1_000), U256::from(1_001))
    );
    assert!(
        origin
            .require_swap_destination_setup_fee(operation, fee_token, U256::ZERO)
            .is_err()
    );

    // The swap's setup is confirmed and the destination's reverted.
    let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
    let settle = |chain_id: u64,
                  operation: ExecutorOperationId,
                  delegate: Address,
                  result: ExecutorExecutionResult| {
        let store = ExecutorStore::new(db.clone(), view.clone(), chain_id).unwrap();
        let signed_at =
            ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
        store.reconcile(operation, signed_at, &[]).unwrap();
        let setup = B256::repeat_byte(3);
        store
            .record_issued(
                operation,
                IssuedExecutorPayload::new(
                    U256::ZERO,
                    delegate,
                    setup,
                    ExecutorPayloadPurpose::Operation,
                    ExecutorPayloadContext::new(
                        Bytes::from_static(b"setup"),
                        signed_at,
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        let nonce = u8::from(result == ExecutorExecutionResult::Executed);
        store
            .reconcile(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::from(nonce)),
                &[(
                    setup,
                    ExecutorPayloadInclusion::new(
                        BlockNumHash::new(11, B256::repeat_byte(11)),
                        B256::repeat_byte(4),
                        result,
                    ),
                )],
            )
            .unwrap()
    };
    let status = |record: &ExecutorRecord, profile: crate::settings::ExecutorProfile| {
        let code = [
            EIP7702_DELEGATION_DESIGNATOR.as_slice(),
            profile.delegate().as_slice(),
        ]
        .concat();
        swap_setup_status(record, confirmed, &code, profile)
    };
    let delegated = settle(
        1,
        operation,
        profile.delegate(),
        ExecutorExecutionResult::Executed,
    );
    let failed = settle(
        DESTINATION_CHAIN,
        destination_operation,
        destination_profile.delegate(),
        ExecutorExecutionResult::Reverted,
    );
    assert!(matches!(
        status(&delegated, profile),
        SwapSetupStatus::Delegated(_)
    ));
    assert_eq!(
        status(&failed, destination_profile),
        SwapSetupStatus::Failed
    );

    // A retry prepares only the failed setup, with the account it reserved. The confirmed
    // setup takes no retry and its record is untouched.
    let retried = destination
        .resume_swap_setup(
            destination_operation,
            destination_candidate.clone(),
            &destination_authorization,
        )
        .await
        .unwrap();
    assert_eq!(retried.context().executor, executor);
    assert!(
        origin
            .resume_swap_setup(operation, broadcaster(profile.delegate()), &authorization)
            .await
            .is_err()
    );
    assert_eq!(record(&origin, operation), delegated);

    // A restart restores both accounts and the saved approval, with the destination setup
    // still to retry for the same swap and token only.
    origin.shutdown().await;
    destination.shutdown().await;
    drop((origin, destination));
    let [origin, destination] = owners(1);
    assert_eq!(
        record(&origin, operation).swap_approval(),
        Some(&prepared.approval)
    );
    let restored = record(&destination, destination_operation);
    assert!(!restored.is_retired());
    assert_eq!(restored.swap_destination(), Some(serves));
    let resumed = destination
        .prepare_swap_destination_setup(
            destination_operation,
            destination_candidate.clone(),
            serves,
            &destination_authorization,
        )
        .await
        .unwrap();
    assert_eq!(resumed.context().executor, executor);
    let another = SwapDestinationRecord {
        destination_token: USDC,
        ..serves
    };
    assert!(
        destination
            .prepare_swap_destination_setup(
                destination_operation,
                destination_candidate.clone(),
                another,
                &destination_authorization,
            )
            .await
            .is_err()
    );
    // A stopped destination account takes no further setup.
    destination.stop_swap_setup(destination_operation).unwrap();
    assert!(
        destination
            .prepare_swap_destination_setup(
                destination_operation,
                destination_candidate,
                serves,
                &destination_authorization,
            )
            .await
            .is_err()
    );
    origin.shutdown().await;
    destination.shutdown().await;
    drop((origin, destination));
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
