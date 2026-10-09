use super::swap_order::delegate_setup;
use super::*;
use crate::{
    BroadcasterFeePolicyStatus, DesktopPrivateSpendAuthorization, ExecutorPrivateFeeLimitExceeded,
    PublicBroadcasterCandidate, SwapPairPreparation, SwapPairSide, SwapSetupStatus, SwapUseClaim,
    is_swap_destination_record, prepare_swap_pair, submit_swap_pair_setups_with, swap_setup_status,
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
            source_setup_fee: None,
        },
        price_verified: Some(false),
        price_acknowledged: true,
        delivery,
        tokens: Some(crate::vault::SwapApprovalTokens { sell, buy }),
        accounts: None,
    }
}

/// Terms of a private Bridge swap to the destination chain, before its destination stealth
/// account is derived, with a setup fee limit for each chain.
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
    approval.bounds.source_setup_fee = Some(U256::from(2_000));
    approval
}

/// A set-up account at `executor` on `store`'s chain with nothing unfinished: its setup won
/// nonce 0 and it signed nothing else.
pub(super) fn existing_account(
    store: &ExecutorStore,
    profile: crate::settings::ExecutorProfile,
    executor: Address,
) -> ExecutorOperationId {
    let operation = ExecutorOperationId::random().unwrap();
    store
        .reserve(operation, profile.delegate(), Some("Private swap"), &[])
        .unwrap();
    store.bind_address(operation, executor).unwrap();
    delegate_setup(store, operation, profile);
    operation
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
async fn private_bridge_setup_claims_both_accounts_first_and_each_setup_stands_alone() {
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
    let preparation = |candidate, approval| SwapPairPreparation {
        use_id: SwapUseId::first(operation),
        source: SwapAccountChoice::New(operation),
        destination: Some(SwapAccountChoice::New(destination_operation)),
        candidate: Some(candidate),
        destination_candidate: Some(destination_candidate.clone()),
        approval,
        authorization: &authorization,
        destination_authorization: Some(&destination_authorization),
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
            prepare_swap_pair(&origin, Some(&destination), request)
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

    // Both accounts are claimed for the swap's use before either is inspected, so a preparation
    // paused in a chain read holds its whole pair, and a setup that fails leaves both claimed.
    let swap_use = SwapUseId::first(operation);
    // What an unrelated swap gets when it names either account of the pair.
    let unrelated_claims = |origin: &ExecutorOwner, destination: &ExecutorOwner| {
        [
            origin.claim_swap_use(
                None,
                SwapUseClaim {
                    id: SwapUseId::random().unwrap(),
                    source: SwapAccountChoice::Existing(operation),
                    approval: setup_approval(WETH, USDC, SwapDelivery::Reshield),
                    destination: None,
                },
            ),
            origin.claim_swap_use(
                Some(destination),
                SwapUseClaim {
                    id: SwapUseId::random().unwrap(),
                    source: SwapAccountChoice::New(ExecutorOperationId::random().unwrap()),
                    approval: private_bridge_approval(),
                    destination: Some(SwapAccountChoice::Existing(destination_operation)),
                },
            ),
        ]
        .map(|claim| {
            matches!(
                claim.unwrap_err().downcast_ref::<ExecutorStoreError>(),
                Some(ExecutorStoreError::SwapUseActive)
            )
        })
    };
    // The pair as its records hold it.
    let pair = |origin: &ExecutorOwner, destination: &ExecutorOwner| {
        let (source, account) = (
            record(origin, operation),
            record(destination, destination_operation),
        );
        (
            (source.active_swap_use(), account.active_swap_use()),
            source.swap_approval().cloned(),
            source.destination_operation(),
            account.swap_destination(),
            (source.is_retired(), account.is_retired()),
        )
    };
    let mut unavailable = broadcaster(profile.delegate());
    unavailable.available_wallets = 0;
    let request = preparation(unavailable, private_bridge_approval());
    rpc.hold.send_replace(Some(multicall()));
    {
        let preparing = prepare_swap_pair(&origin, Some(&destination), request);
        tokio::pin!(preparing);
        tokio::select! {
            result = &mut preparing => panic!("destination inspection should be blocked: {}", result.is_ok()),
            () = rpc.wait_for(|requests| requests.iter().any(|request| request["method"] == "eth_call")) => {},
        }
        let held = (
            record(&origin, operation),
            record(&destination, destination_operation),
        );
        assert_eq!(
            (held.0.active_swap_use(), held.1.active_swap_use()),
            (Some(swap_use), Some(swap_use))
        );
        assert_eq!(held.0.destination_operation(), Some(destination_operation));
        assert!(is_swap_destination_record(&held.1));
        // An unrelated swap takes neither account, and its own new account is not allocated.
        assert_eq!(unrelated_claims(&origin, &destination), [true, true]);
        // Finishing an older swap sweeps this chain while the pair is still being inspected,
        // and so does a restart's load. Neither retires or releases an account of the pair.
        assert!(!destination.reconcile_swap_destinations().unwrap());
        assert!(
            !ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN)
                .unwrap()
                .reconcile_swap_destinations_on_load()
                .unwrap()
        );
        assert_eq!(
            (origin.records().unwrap(), destination.records().unwrap()),
            (vec![held.0], vec![held.1])
        );
        rpc.hold.send_replace(None);
        assert!(preparing.await.is_err());
    }
    let reserved = record(&destination, destination_operation);
    assert!(is_swap_destination_record(&reserved) && !crate::is_swap_record(&reserved));
    assert!(crate::is_swap_record(&record(&origin, operation)));

    // A restart before anything is signed restores the pair with its approval and its claim,
    // and still gives neither account to another swap.
    let claimed = pair(&origin, &destination);
    assert_eq!(claimed.0, (Some(swap_use), Some(swap_use)));
    origin.shutdown().await;
    destination.shutdown().await;
    drop((origin, destination));
    let [origin, destination] = owners(1);
    assert_eq!(pair(&origin, &destination), claimed);
    assert_eq!(unrelated_claims(&origin, &destination), [true, true]);
    assert!(record(&origin, operation).issued().is_empty() && reserved.issued().is_empty());

    // The retry resumes both accounts, and the approval saved with the swap names the
    // destination account.
    let request = preparation(broadcaster(profile.delegate()), private_bridge_approval());
    let prepared = prepare_swap_pair(&origin, Some(&destination), request)
        .await
        .unwrap();
    let (SwapPairSide::Setup(origin_setup), Some(SwapPairSide::Setup(destination_setup))) =
        (&prepared.origin, &prepared.destination)
    else {
        panic!("both fresh accounts need setup");
    };
    let executor = destination_setup.context().executor;
    assert_eq!(reserved.address(), Some(executor));
    assert_ne!(origin_setup.context().executor, executor);
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

    // A duplicate Public address rejects the handoff without stopping either fresh account.
    let duplicate = {
        let (_, signer) = vault
            .executor_spend_signers_for_session(
                &mut vault.create_spend_grant(TEST_PASSWORD).unwrap(),
                &view,
                None,
                1,
                saved.index(),
            )
            .unwrap();
        let private_key = Zeroizing::new(alloy::hex::encode(signer.to_bytes()));
        vault
            .import_public_account(TEST_PASSWORD, &view, &private_key, None, false)
            .unwrap()
    };
    assert_eq!(duplicate.address, origin_setup.context().executor);
    let before = (
        origin.records().unwrap(),
        destination.records().unwrap(),
        vault.list_public_accounts_for_session(&view, true).unwrap(),
    );
    let error = origin
        .register_public_account(operation, &authorization)
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<ExecutorStoreError>(),
        Some(ExecutorStoreError::Vault(
            VaultError::DuplicatePublicAccountAddress
        ))
    ));
    assert_eq!(
        (
            origin.records().unwrap(),
            destination.records().unwrap(),
            vault.list_public_accounts_for_session(&view, true).unwrap(),
        ),
        before
    );
    vault
        .delete_imported_public_account(&view, &duplicate.public_account_uuid)
        .unwrap();

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

    // The swap's setup nonce is read as consumed and the destination's as unconsumed.
    let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
    let settle = |chain_id: u64,
                  operation: ExecutorOperationId,
                  delegate: Address,
                  consumed: bool| {
        let store = ExecutorStore::new(db.clone(), view.clone(), chain_id).unwrap();
        let signed_at =
            ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
        store.record_account_read(operation, signed_at).unwrap();
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
        store
            .record_account_read(
                operation,
                ExecutorNonceObservation::new(confirmed, U256::from(u8::from(consumed))),
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
    let delegated = settle(1, operation, profile.delegate(), true);
    let pending = settle(
        DESTINATION_CHAIN,
        destination_operation,
        destination_profile.delegate(),
        false,
    );
    assert!(matches!(
        status(&delegated, profile),
        SwapSetupStatus::Delegated(_)
    ));
    assert_eq!(
        status(&pending, destination_profile),
        SwapSetupStatus::Pending
    );

    // A retry prepares only the pending setup, with the account it reserved. The confirmed
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
    let [origin, destination] = owners(2);
    assert_eq!(
        record(&origin, operation).swap_approval(),
        Some(&prepared.approval)
    );
    let restored = record(&destination, destination_operation);
    assert!(!restored.is_retired());
    assert_eq!(restored.swap_destination(), Some(serves));
    let resumed = destination
        .resume_swap_setup(
            destination_operation,
            destination_candidate.clone(),
            &destination_authorization,
        )
        .await
        .unwrap();
    assert_eq!(resumed.context().executor, executor);
    let before = (origin.records().unwrap(), destination.records().unwrap());
    let mut another = prepared.approval.clone();
    let SwapDelivery::Bridge(delivery) = &mut another.delivery else {
        panic!("the approval keeps its Bridge delivery");
    };
    delivery.destination_token = USDC;
    let error = prepare_swap_pair(
        &origin,
        Some(&destination),
        preparation(broadcaster(profile.delegate()), another),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(
        error.downcast_ref::<ExecutorStoreError>(),
        Some(ExecutorStoreError::OperationMismatch)
    ));
    assert_eq!(
        (origin.records().unwrap(), destination.records().unwrap()),
        before
    );
    // A stopped destination account takes no further setup.
    destination.stop_swap_setup(destination_operation).unwrap();
    assert!(
        destination
            .resume_swap_setup(
                destination_operation,
                destination_candidate,
                &destination_authorization,
            )
            .await
            .is_err()
    );
    // Registering the swap's account in Public stops its use before the account is handed off.
    let live = record(&origin, operation);
    assert!(!live.swap_use(swap_use).unwrap().is_stopped());
    origin
        .register_public_account(operation, &authorization)
        .await
        .unwrap();
    let registered = record(&origin, operation);
    assert!(registered.swap_use(swap_use).unwrap().is_stopped());
    assert_eq!(registered.issued(), live.issued());
    origin.shutdown().await;
    destination.shutdown().await;
    drop((origin, destination));
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// Each side of a pair is a new account, which the swap sets up, or an existing one, which takes
// no setup and no setup fee. Only new sides are prepared and submitted, each with its own
// chain's broadcaster and approved fee limit, and two new sides are submitted at the same time.
#[tokio::test]
async fn swap_pair_sets_up_only_its_new_accounts() {
    for (source_new, destination_new) in
        [(true, true), (false, true), (true, false), (false, false)]
    {
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
        let candidate = broadcaster(profile.delegate());
        let mut destination_candidate = broadcaster(destination_profile.delegate());
        destination_candidate.chain_id = DESTINATION_CHAIN;
        let [origin, destination] = [origin_chain, destination_chain].map(|chain| {
            ExecutorOwner::new(
                0,
                db.clone(),
                view.clone(),
                chain,
                HttpContext::direct_for_tests(),
            )
            .unwrap()
        });
        let record = |owner: &ExecutorOwner, operation| {
            owner
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap()
        };
        let existing = (Address::repeat_byte(0xa1), Address::repeat_byte(0xa2));
        let source = if source_new {
            SwapAccountChoice::New(ExecutorOperationId::random().unwrap())
        } else {
            let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
            SwapAccountChoice::Existing(existing_account(&store, profile, existing.0))
        };
        let destination_account = if destination_new {
            SwapAccountChoice::New(ExecutorOperationId::random().unwrap())
        } else {
            let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
            SwapAccountChoice::Existing(existing_account(&store, destination_profile, existing.1))
        };
        let other_destination = if !source_new && !destination_new {
            let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
            let address = Address::repeat_byte(0xa3);
            existing_account(&store, destination_profile, address);
            Some(address)
        } else {
            None
        };
        let before = (origin.records().unwrap(), destination.records().unwrap());
        let use_id = SwapUseId::random().unwrap();
        let authorization = password();
        let destination_authorization = authorization.for_destination().unwrap();
        // The approval binds a fee limit only for a chain whose account needs setup.
        let mut approval = private_bridge_approval();
        approval.bounds.source_setup_fee = source_new.then_some(U256::from(2_000));
        approval.bounds.destination_setup_fee = destination_new.then_some(U256::from(1_000));
        let preparation =
            |source, approval, candidate, destination_candidate| SwapPairPreparation {
                use_id,
                source,
                destination: Some(destination_account),
                approval,
                candidate,
                destination_candidate,
                authorization: &authorization,
                destination_authorization: Some(&destination_authorization),
            };
        // The broadcaster of each side that needs setup.
        let source_setup = source_new.then(|| candidate.clone());
        let destination_setup = destination_new.then(|| destination_candidate.clone());

        // A broadcaster on a side that isn't new, a new side without one, and a new side
        // without its fee limit each reserve nothing.
        let mut unbounded = approval.clone();
        unbounded.bounds.source_setup_fee = None;
        unbounded.bounds.destination_setup_fee = None;
        let mut refused = vec![
            (
                approval.clone(),
                (!source_new).then(|| candidate.clone()),
                destination_setup.clone(),
            ),
            (
                approval.clone(),
                source_setup.clone(),
                (!destination_new).then(|| destination_candidate.clone()),
            ),
        ];
        if source_new || destination_new {
            refused.push((unbounded, source_setup.clone(), destination_setup.clone()));
        }
        if let Some(other_destination) = other_destination {
            // Software authorization cannot substitute the selected destination for the
            // account explicitly approved, or change whether either side needs setup.
            let mut bound = approval.clone();
            bound.accounts = Some(SwapApprovedAccounts {
                source: SwapApprovedAccount {
                    address: Some(existing.0),
                    setup: false,
                },
                destination: Some(SwapApprovedAccount {
                    address: Some(other_destination),
                    setup: false,
                }),
            });
            refused.push((bound.clone(), None, None));
            let accounts = bound.accounts.as_mut().unwrap();
            accounts.destination.as_mut().unwrap().address = Some(existing.1);
            accounts.source.setup = true;
            refused.push((bound.clone(), None, None));
            let accounts = bound.accounts.as_mut().unwrap();
            accounts.source.setup = false;
            accounts.destination = None;
            refused.push((bound, None, None));
        }
        for (approval, candidate, destination_candidate) in refused {
            let request = preparation(source, approval, candidate, destination_candidate);
            assert!(
                prepare_swap_pair(&origin, Some(&destination), request)
                    .await
                    .is_err()
            );
            assert_eq!(
                (origin.records().unwrap(), destination.records().unwrap()),
                before
            );
        }

        let request = preparation(
            source,
            approval.clone(),
            source_setup.clone(),
            destination_setup.clone(),
        );
        let prepared = prepare_swap_pair(&origin, Some(&destination), request)
            .await
            .unwrap();
        let prepared_destination = prepared.destination.as_ref().unwrap();
        assert_eq!(
            (
                prepared.origin.requires_setup(),
                prepared_destination.requires_setup()
            ),
            (source_new, destination_new)
        );

        // A new side is prepared for its own chain and its own broadcaster. An existing side
        // keeps its address and what it issued, and its use is not a fresh one.
        for (owner, side, chain_id, own, other, address) in [
            (
                &origin,
                &prepared.origin,
                1,
                &candidate,
                &destination_candidate,
                existing.0,
            ),
            (
                &destination,
                prepared_destination,
                DESTINATION_CHAIN,
                &destination_candidate,
                &candidate,
                existing.1,
            ),
        ] {
            let saved = record(owner, side.operation());
            assert_eq!(saved.active_swap_use(), Some(use_id));
            assert_eq!(saved.address(), Some(side.executor()));
            match side {
                SwapPairSide::Setup(setup) => {
                    assert_eq!(setup.context().chain_id, chain_id);
                    setup.require_broadcaster(own).unwrap();
                    assert!(setup.require_broadcaster(other).is_err());
                    assert!(saved.swap_use(use_id).unwrap().is_fresh());
                    assert!(saved.issued().is_empty());
                }
                SwapPairSide::Existing { executor, .. } => {
                    assert_eq!(*executor, address);
                    assert!(!saved.swap_use(use_id).unwrap().is_fresh());
                    assert_eq!(saved.issued().len(), 1, "only its earlier setup");
                }
            }
        }

        // The saved approval binds both addresses, each side's setup need and the destination
        // account as receiver.
        let bound = crate::vault::SwapApprovedAccounts {
            source: crate::vault::SwapApprovedAccount {
                address: Some(prepared.origin.executor()),
                setup: source_new,
            },
            destination: Some(crate::vault::SwapApprovedAccount {
                address: Some(prepared_destination.executor()),
                setup: destination_new,
            }),
        };
        assert_eq!(prepared.approval.accounts, Some(bound));
        let SwapDelivery::Bridge(delivery) = prepared.approval.delivery else {
            panic!("the approval keeps its Bridge delivery");
        };
        assert_eq!(delivery.receiver, prepared_destination.executor());
        let saved = record(&origin, source.operation());
        assert_eq!(saved.swap_approval(), Some(&prepared.approval));
        if let Some(other_destination) = other_destination {
            // An idempotent claim must still enforce the supplied account binding.
            let before = (origin.records().unwrap(), destination.records().unwrap());
            let mut replaced = prepared.approval.clone();
            replaced
                .accounts
                .as_mut()
                .unwrap()
                .destination
                .as_mut()
                .unwrap()
                .address = Some(other_destination);
            let request = preparation(source, replaced, None, None);
            assert!(
                prepare_swap_pair(&origin, Some(&destination), request)
                    .await
                    .is_err()
            );
            assert_eq!(
                (origin.records().unwrap(), destination.records().unwrap()),
                before
            );
        }

        // Each new side's fee limit is the one approved for its own chain.
        let exceeds = |result: eyre::Result<()>| {
            let error = result.unwrap_err();
            let exceeded = error
                .downcast_ref::<ExecutorPrivateFeeLimitExceeded>()
                .unwrap();
            (exceeded.maximum(), exceeded.required())
        };
        if source_new {
            let (operation, token) = (source.operation(), candidate.token);
            origin
                .require_swap_source_setup_fee(operation, token, U256::from(2_000))
                .unwrap();
            assert_eq!(
                exceeds(origin.require_swap_source_setup_fee(operation, token, U256::from(2_001))),
                (U256::from(2_000), U256::from(2_001))
            );
        }
        if destination_new {
            let (operation, token) = (destination_account.operation(), destination_candidate.token);
            destination
                .require_swap_destination_setup_fee(operation, token, U256::from(1_000))
                .unwrap();
            assert_eq!(
                exceeds(destination.require_swap_destination_setup_fee(
                    operation,
                    token,
                    U256::from(1_001)
                )),
                (U256::from(1_000), U256::from(1_001))
            );
        }

        // Only new sides are submitted. Each submission waits for the others, so two new sides
        // finish only when both are in flight at once.
        let new_sides = usize::from(source_new) + usize::from(destination_new);
        let barrier = tokio::sync::Barrier::new(new_sides.max(1));
        let submitted = Mutex::new(Vec::new());
        let results = tokio::time::timeout(
            Duration::from_secs(2),
            submit_swap_pair_setups_with(
                (&origin, &prepared.origin, Some(U256::from(2_000))),
                Some((&destination, prepared_destination, Some(U256::from(1_000)))),
                |_, setup, maximum_private_fee| {
                    let (barrier, submitted) = (&barrier, &submitted);
                    async move {
                        barrier.wait().await;
                        let sent = (setup.context().chain_id, setup.operation());
                        submitted.lock().unwrap().push((sent, maximum_private_fee));
                        Ok::<_, eyre::Report>(sent)
                    }
                },
            ),
        )
        .await
        .expect("new sides are submitted at the same time");
        let sent_origin = source_new.then(|| ((1, source.operation()), U256::from(2_000)));
        let sent_destination = destination_new.then(|| {
            (
                (DESTINATION_CHAIN, destination_account.operation()),
                U256::from(1_000),
            )
        });
        assert_eq!(
            (results.0.map(Result::unwrap), results.1.map(Result::unwrap)),
            (
                sent_origin.map(|(sent, _)| sent),
                sent_destination.map(|(sent, _)| sent)
            )
        );
        let mut submitted = submitted.into_inner().unwrap();
        submitted.sort_by_key(|((chain_id, _), _)| *chain_id);
        assert_eq!(
            submitted,
            sent_origin
                .into_iter()
                .chain(sent_destination)
                .collect::<Vec<_>>()
        );

        // A retry prepares a new side alone, with the account it reserved. An existing side
        // takes no setup, and its record stays as it is.
        for (owner, side, candidate, authorization) in [
            (&origin, &prepared.origin, &candidate, &authorization),
            (
                &destination,
                prepared_destination,
                &destination_candidate,
                &destination_authorization,
            ),
        ] {
            let saved = record(owner, side.operation());
            let retried = owner
                .resume_swap_setup(side.operation(), candidate.clone(), authorization)
                .await;
            if side.requires_setup() {
                assert_eq!(retried.unwrap().context().executor, side.executor());
            } else {
                let error = retried.err().unwrap();
                assert!(error.to_string().contains("takes no setup"), "{error:#}");
                assert_eq!(record(owner, side.operation()), saved);
            }
        }

        // The same request resumes the pair. One that flips the source's setup need for the
        // same use is refused, and the saved approval stays.
        let request = preparation(
            source,
            prepared.approval.clone(),
            source_setup,
            destination_setup.clone(),
        );
        let resumed = prepare_swap_pair(&origin, Some(&destination), request)
            .await
            .unwrap();
        assert_eq!(resumed.approval, prepared.approval);
        let flipped = if source_new {
            SwapAccountChoice::Existing(source.operation())
        } else {
            SwapAccountChoice::New(source.operation())
        };
        let mut rebound = approval;
        rebound.bounds.source_setup_fee = Some(U256::from(2_000));
        let request = preparation(
            flipped,
            rebound,
            (!source_new).then(|| candidate.clone()),
            destination_setup,
        );
        let error = prepare_swap_pair(&origin, Some(&destination), request)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("differ"), "{error:#}");
        assert_eq!(
            record(&origin, source.operation()).swap_approval(),
            Some(&prepared.approval)
        );

        origin.shutdown().await;
        destination.shutdown().await;
        drop((origin, destination));
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn a_swap_use_cancelled_during_inspection_refuses_its_late_preparation() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let (origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
    let profile = origin_chain.accepted_executor_profile().unwrap();
    let mut destination_candidate = broadcaster(
        destination_chain
            .accepted_executor_profile()
            .unwrap()
            .delegate(),
    );
    destination_candidate.chain_id = DESTINATION_CHAIN;
    let [origin, destination] = [origin_chain, destination_chain].map(|chain| {
        ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            chain,
            HttpContext::direct_for_tests(),
        )
        .unwrap()
    });
    let (operation, destination_operation) = (
        ExecutorOperationId::random().unwrap(),
        ExecutorOperationId::random().unwrap(),
    );
    let authorization = password();
    let destination_authorization = authorization.for_destination().unwrap();
    let preparation = || SwapPairPreparation {
        use_id: SwapUseId::first(operation),
        source: SwapAccountChoice::New(operation),
        destination: Some(SwapAccountChoice::New(destination_operation)),
        candidate: Some(broadcaster(profile.delegate())),
        destination_candidate: Some(destination_candidate.clone()),
        approval: private_bridge_approval(),
        authorization: &authorization,
        destination_authorization: Some(&destination_authorization),
    };

    // The user cancels while the destination account is still being inspected. Neither fresh
    // account issued anything, so both claims are released, and the inspection's result
    // prepares nothing.
    rpc.hold.send_replace(Some(multicall()));
    {
        let preparing = prepare_swap_pair(&origin, Some(&destination), preparation());
        tokio::pin!(preparing);
        tokio::select! {
            result = &mut preparing => panic!("destination inspection should be blocked: {}", result.is_ok()),
            () = rpc.wait_for(|requests| requests.iter().any(|request| request["method"] == "eth_call")) => {},
        }
        assert_eq!(
            origin
                .cancel_swap_use(Some(&destination), operation, SwapUseId::first(operation))
                .unwrap(),
            SwapUseCancellation {
                source: SwapUseRelease::Released,
                destination: Some(SwapUseRelease::Released),
            }
        );
        rpc.hold.send_replace(None);
        assert!(preparing.await.is_err());
    }
    // Starting the same swap again resumes its stopped claim, which prepares nothing either.
    assert!(
        prepare_swap_pair(&origin, Some(&destination), preparation())
            .await
            .is_err()
    );
    for (owner, operation) in [(&origin, operation), (&destination, destination_operation)] {
        let records = owner.records().unwrap();
        let [record] = records.as_slice() else {
            panic!("the cancelled swap keeps its one account on each chain");
        };
        assert_eq!(record.operation(), operation);
        assert!(record.is_swap_setup_stopped() && record.issued().is_empty());
        assert!(record.swap_uses().iter().all(SwapUseRecord::is_stopped));
    }
    origin.shutdown().await;
    destination.shutdown().await;
    drop((origin, destination));
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// Each pass of a setup wait is one read of the account's code and execution nonce at the
// confirmed tip, and that state alone decides the setup.
#[tokio::test]
async fn swap_setup_completes_only_with_observed_delegation_and_consumed_nonce() {
    use railgun_wallet::WalletUtxo;
    use sync_service::{WalletCurrentSnapshot, WalletPendingOverlay};

    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let profile = chain.accepted_executor_profile().unwrap();
    let delegate = profile.delegate();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let prepared = owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            None,
            &password(),
        )
        .await
        .unwrap();
    let executor = prepared.context().executor;
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
            _nonce: prepared.context().execution_nonce,
            _signature: Bytes::new(),
        }
        .abi_encode()
        .into(),
    };
    let setup = owner
        .issue_operation(&prepared, &call, std::slice::from_ref(&input), &password())
        .await
        .unwrap()
        .payload_hash();
    store
        .record_submission(operation, setup, B256::repeat_byte(5))
        .unwrap();
    let record = || {
        owner
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };
    // One pass at the confirmed block below `head`, with the requests it made.
    let pass = async |head: u64| {
        rpc.state.head.store(head, Ordering::Relaxed);
        let before = rpc.state.requests.lock().unwrap().len();
        let status = owner.observe_swap_setup(operation).await.unwrap();
        let requests = rpc.state.requests.lock().unwrap()[before..].to_vec();
        (status, requests)
    };
    let reads_account = |requests: &[Value]| {
        requests.iter().any(|request| {
            request["method"] == "eth_getCode" && request["params"][0] == json!(executor)
        })
    };

    // The nonce is unconsumed at the confirmed block: not yet included, or included with
    // its authorization skipped. The setup stays pending and the next pass reads again,
    // without whole blocks or logs.
    for head in [20, 21] {
        let (status, requests) = pass(head).await;
        assert_eq!(status, SwapSetupStatus::Pending);
        assert!(reads_account(&requests));
        assert!(requests.iter().all(|request| {
            request["method"] != "eth_getLogs"
                && (request["method"] != "eth_getBlockByNumber"
                    || request["params"][1] != json!(true))
        }));
        assert_eq!(
            record().nonce_observation().unwrap().block().number,
            head - 1
        );
    }

    // Another writer changes the record while the pass reads. The pass keeps waiting.
    rpc.hold.send_replace(Some(executor));
    let held = rpc.state.requests.lock().unwrap().len();
    let (status, ()) = tokio::join!(owner.observe_swap_setup(operation), async {
        rpc.wait_for(|requests| reads_account(&requests[held..]))
            .await;
        owner.set_hidden(operation, true).unwrap();
        rpc.hold.send_replace(None);
    });
    assert_eq!(status.unwrap(), SwapSetupStatus::Pending);
    owner.set_hidden(operation, false).unwrap();

    // The user releases the setup's notes and a transaction that is not recorded for the
    // setup spends them. That is no evidence about the setup: it stays pending and a retry
    // is admitted with the same account.
    owner.release_input_lock(operation).unwrap();
    let mut spent = WalletUtxo::new(input);
    spent.spent = Some(UtxoSource {
        tx_hash: B256::repeat_byte(6),
        block_number: 14,
        block_timestamp: 1,
    });
    let synced = WalletCurrentSnapshot::new(21, 0, 0, vec![spent], WalletPendingOverlay::default());
    owner
        .confirm_synced_record(&synced, &|| Some(synced.clone()), Some(21), record())
        .await
        .unwrap();
    assert_eq!(
        record().payload_state(setup),
        Some(ExecutorPayloadState::Pending)
    );
    assert_eq!(pass(22).await.0, SwapSetupStatus::Pending);
    let retried = owner
        .resume_swap_setup(operation, broadcaster(delegate), &password())
        .await
        .unwrap();
    assert_eq!(retried.context().executor, executor);

    // The nonce is past the setup and the account has no code: the delegation is missing.
    rpc.state.used.lock().unwrap().insert(executor);
    assert_eq!(pass(23).await.0, SwapSetupStatus::MissingDelegation);

    // The nonce is past the setup under the accepted delegation: the account is set up.
    rpc.set_delegated_account(executor, delegate, U256::ONE);
    let (SwapSetupStatus::Delegated(handle), _) = pass(24).await else {
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
    // Code read at any block other than the nonce observation is not evidence.
    let code = [
        EIP7702_DELEGATION_DESIGNATOR.as_slice(),
        delegate.as_slice(),
    ]
    .concat();
    assert_eq!(
        swap_setup_status(
            &record(),
            BlockNumHash::new(22, B256::repeat_byte(22)),
            &code,
            profile
        ),
        SwapSetupStatus::Pending
    );

    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
