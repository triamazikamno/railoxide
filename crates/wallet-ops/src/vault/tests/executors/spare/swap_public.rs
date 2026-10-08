//! The destination side of a swap paid from a Public account: its claim, the destination
//! account's setup and delegation check, and the delivery signed before the Public account
//! signs anything. Then the Public account's own side: its approvals and deposit on the chain
//! it pays on, each recorded before it is broadcast, and the hand-off read back from that chain.
//! An order's batch and order are signed as typed data and recorded before the orderbook
//! request, which a restart resends unchanged. What became of the order is read from fabricated
//! settlement receipts: its deposit, proceeds its proxy holds, their withdrawal, and its
//! invalidation. After the hand-off the destination chain's owner tracks the fill on its own
//! chain and the refund on the chain the account pays on, also after a restart.

use super::swap_observation::{EXECUTOR, MockChain, execute, private_logs, private_transaction};
use super::swap_order::{
    across_fee_quote, delegate_setup, read_json_body, spawn_bridge_stub, submitted_order,
};
use super::swap_setup::{
    DESTINATION_CHAIN, DESTINATION_TOKEN, USDC, WETH, broadcaster, destination_chain_config,
    password, setup_approval,
};
use super::*;
use crate::bridge::AcrossClient;
use crate::cow::CowOrderbookClient;
use crate::public_wallet::{PublicErc20, VaultedPublicSigner};
use crate::settings::EffectiveChainConfig;
use crate::signer::SoftwareEvmSigner;
use crate::vault::{
    AcrossOrderTerms, PublicAccountScope, PublicSwapApproval, PublicSwapDeposited,
    PublicSwapInclusion, PublicSwapIntent, PublicSwapObservations, PublicSwapOrder, PublicSwapPath,
    PublicSwapProxyHolding, PublicSwapRecord, PublicSwapTransactionKind, SwapAccountChoice,
    SwapAccountRefusal, SwapAccountRole, SwapAccountUse, SwapAdmissionEvidence,
    SwapApprovedAccount, SwapBridgeHandoff, SwapBridgeOutcome, SwapDestinationOutcome,
    SwapObservation, SwapSubmissionStatus, SwapUseId, SwapUseRole, swap_account_refusal,
};
use crate::{
    ExecutorPrivateFeeLimitExceeded, OperationHttpClient, OperationNetworkIsolation,
    PUBLIC_PROXY_DEPLOYING_WITHDRAWAL_GAS_UNITS, PublicActionGasFeeSelection,
    PublicActionProgressUpdate, PublicSwapDelivery, PublicSwapDeliverySigning,
    PublicSwapOrderOutcome, PublicSwapOrderState, PublicSwapProgress, PublicSwapTracking,
    PublicSwapTransactionOutcome, PublicSwapUseClaim, SwapPairSide, SwapReviewChange,
    SwapSetupStatus, WalletNetworkMode, new_public_swap_batch_nonce, public_swap_batch_terms,
    public_swap_gas_plan, public_swap_order_state, swap_setup_status,
};
use alloy::consensus::{Transaction as _, TxEnvelope};
use alloy::eips::Decodable2718 as _;
use alloy::primitives::keccak256;
use alloy::sol_types::SolEvent as _;
use broadcaster_core::contracts::across::{
    MulticallHandler, SpokePool, V3RelayExecutionEventInfo, address_to_bytes32,
    private_delivery_message,
};
use broadcaster_core::contracts::cow::{
    AppData, ORDER_KIND_SELL, OrderUid, invalidate_order_calldata, order_digest, order_uid,
    recover_order_signer,
};
use broadcaster_core::contracts::cow_shed::{
    COWShedFactory, ExecuteHooks, decode_deposit_hook_calls, decode_withdrawal_calls,
    execute_hooks_digest, proxy_address,
};
use broadcaster_core::contracts::executor::AcrossPrivateDelivery;
use broadcaster_core::contracts::railgun::transferCall;
use broadcaster_core::contracts::shield::build_approve_calldata;
use broadcaster_core::contracts::swap_math::{SWAP_MATH_ADDRESS, SWAP_MATH_CREATION_CODE};

// The settlement's events, as the origin chain's fabricated receipts carry them.
alloy::sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Trade(address indexed owner, address sellToken, address buyToken, uint256 sellAmount, uint256 buyAmount, uint256 feeAmount, bytes orderUid);
}

/// The claim of a direct deposit of `bridged_token` from a Public account on `origin_chain`,
/// delivered as at least 1,000 of `destination_token` to the new account `operation`. A failed
/// shield keeps the tokens in that account.
fn claim(
    id: SwapUseId,
    operation: ExecutorOperationId,
    origin_chain: u64,
    bridged_token: Address,
    destination_token: Address,
) -> PublicSwapUseClaim {
    let mut bounds = setup_approval(bridged_token, WETH, SwapDelivery::Reshield).bounds;
    bounds.destination_minimum = Some(U256::from(1_000));
    bounds.destination_shield_fee_bps = Some(crate::RAILGUN_PROTOCOL_FEE_BPS);
    bounds.destination_setup_fee = Some(U256::from(1_000));
    PublicSwapUseClaim {
        id,
        origin_chain,
        source: Address::repeat_byte(0x50),
        source_scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: TEST_WALLET_ID.to_owned(),
        },
        account: SwapAccountChoice::New(operation),
        destination_token,
        intent: PublicSwapIntent {
            bridged_token,
            order: false,
        },
        approval: PublicSwapApproval {
            bounds,
            price_verified: Some(true),
            price_acknowledged: false,
            sell_token: bridged_token,
            on_shield_failure: BridgeShieldFailure::KeepOnDestination,
            destination: SwapApprovedAccount {
                address: None,
                setup: true,
            },
            max_gas_cost: U256::from(1_000_000),
        },
    }
}

/// The destination account's record as the store holds it, with the swap its use delivers,
/// the shields issued for that use and what became of them.
struct Saved {
    record: ExecutorRecord,
    swap: PublicSwapRecord,
    shields: Vec<B256>,
    outcome: Option<SwapDestinationOutcome>,
}

fn saved(store: &ExecutorStore, operation: ExecutorOperationId, id: SwapUseId) -> Saved {
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let SwapUseRole::PublicSourceDestination {
        shields,
        outcome,
        swap,
        ..
    } = record.swap_use(id).unwrap().role()
    else {
        panic!("the use delivers a swap paid from a Public account");
    };
    let (swap, shields, outcome) = ((**swap).clone(), shields.clone(), *outcome);
    Saved {
        record,
        swap,
        shields,
        outcome,
    }
}

// A destination setup that fails is the swap's only effect: the Public account's side of the
// record stays as it was claimed, and the setup is retried with the same account.
#[tokio::test]
async fn a_failed_destination_setup_leaves_the_public_account_untouched() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let destination_chain = destination_chain_config(&rpc).await;
    let profile = destination_chain.accepted_executor_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        destination_chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
    let mut candidate = broadcaster(profile.delegate());
    candidate.chain_id = DESTINATION_CHAIN;
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let request = || claim(id, operation, 1, USDC, DESTINATION_TOKEN);
    let authorization = password();

    // A claim for the chain it pays on, without a setup fee limit, without a destination
    // minimum or for another setup need than its account has reserves nothing, and neither
    // does one from a shared Public account, whose refusal keeps its type.
    let mut same_chain = request();
    same_chain.origin_chain = DESTINATION_CHAIN;
    let mut unbounded = request();
    unbounded.approval.bounds.destination_setup_fee = None;
    let mut without_minimum = request();
    without_minimum.approval.bounds.destination_minimum = None;
    let mut existing = request();
    existing.approval.destination.setup = false;
    for refused in [same_chain, unbounded, without_minimum, existing] {
        assert!(owner.claim_public_swap(refused).is_err());
    }
    let mut shared = request();
    shared.source_scope = PublicAccountScope::Global;
    assert!(matches!(
        owner
            .claim_public_swap(shared)
            .unwrap_err()
            .downcast_ref::<ExecutorStoreError>(),
        Some(ExecutorStoreError::PublicSwapSourceShared)
    ));
    assert!(store.records().unwrap().is_empty());

    // The claim reserves the account, and its preparation derives it and binds its address
    // into the approval.
    owner.claim_public_swap(request()).unwrap();
    assert!(
        owner
            .prepare_public_swap_destination(operation, id, None, &authorization)
            .await
            .is_err(),
        "a new account needs its setup's broadcaster"
    );
    let SwapPairSide::Setup(prepared) = owner
        .prepare_public_swap_destination(operation, id, Some(candidate.clone()), &authorization)
        .await
        .unwrap()
    else {
        panic!("a new account needs setup");
    };
    let executor = prepared.context().executor;
    let claimed = saved(&store, operation, id);
    assert_eq!(claimed.record.address(), Some(executor));
    assert_eq!(
        claimed.swap.approval().destination,
        SwapApprovedAccount {
            address: Some(executor),
            setup: true,
        }
    );

    // The setup's fee ceiling is the one approved with the swap, read from this account's use.
    let fee_token = candidate.token;
    owner
        .require_swap_destination_setup_fee(operation, fee_token, U256::from(1_000))
        .unwrap();
    let error = owner
        .require_swap_destination_setup_fee(operation, fee_token, U256::from(1_001))
        .unwrap_err();
    let exceeded = error
        .downcast_ref::<ExecutorPrivateFeeLimitExceeded>()
        .unwrap();
    assert_eq!(
        (exceeded.maximum(), exceeded.required()),
        (U256::from(1_000), U256::from(1_001))
    );

    // The setup is handed off and reverts.
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
    let failed = store
        .reconcile(
            operation,
            ExecutorNonceObservation::new(confirmed, U256::ZERO),
            &[(
                setup,
                ExecutorPayloadInclusion::new(
                    BlockNumHash::new(11, B256::repeat_byte(11)),
                    B256::repeat_byte(4),
                    ExecutorExecutionResult::Reverted,
                ),
            )],
        )
        .unwrap();
    assert_eq!(
        swap_setup_status(&failed, confirmed, &[], profile),
        SwapSetupStatus::Failed
    );

    // Nothing of the Public account is recorded: no path, no transaction, no deposit terms and
    // no shield.
    let after = saved(&store, operation, id);
    assert_eq!(after.swap, claimed.swap);
    assert!(after.swap.path().is_none());
    assert!(after.swap.transactions().is_empty());
    assert!(after.swap.bridge().is_none());
    assert!(after.shields.is_empty());

    // The retry prepares the same account for the same use.
    let SwapPairSide::Setup(retried) = owner
        .prepare_public_swap_destination(operation, id, Some(candidate), &authorization)
        .await
        .unwrap()
    else {
        panic!("the failed setup is retried");
    };
    assert_eq!(retried.context().executor, executor);
    assert_eq!(saved(&store, operation, id).swap, claimed.swap);

    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// The delivery is signed against a quote requested with the handler and the real message. A
// quote below the approved destination minimum returns to review with the shield recorded and
// nothing of the Public account signed. A full new review lowers the approved minimum and
// signs a new guarded delivery while retaining the earlier shield.
#[tokio::test]
async fn a_short_signing_time_quote_returns_the_public_swap_to_review() {
    use alloy::sol_types::SolValue as _;

    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let (origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
    let profile = destination_chain.accepted_executor_profile().unwrap();
    let spoke_pool = origin_chain.bridge_origin_profile().unwrap().spoke_pool();
    let handler = destination_chain
        .bridge_profile()
        .unwrap()
        .multicall_handler();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        destination_chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
    let mut candidate = broadcaster(profile.delegate());
    candidate.chain_id = DESTINATION_CHAIN;
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let authorization = password();
    owner
        .claim_public_swap(claim(id, operation, 1, USDC, DESTINATION_TOKEN))
        .unwrap();
    let executor = owner
        .prepare_public_swap_destination(operation, id, Some(candidate), &authorization)
        .await
        .unwrap()
        .executor();
    let delegated = delegate_setup(&store, operation, profile);
    assert_eq!(delegated.executor(), executor);

    // Across answers every quote with the output the test sets.
    let quoted = Arc::new(Mutex::new(U256::from(999)));
    let answer = quoted.clone();
    let (across_url, requests, across_task) = spawn_bridge_stub(move |_| {
        across_fee_quote(spoke_pool, *answer.lock().unwrap(), 3 * 60 * 60)
    })
    .await;
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let across = crate::bridge::AcrossClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        across_url,
    )
    .unwrap();
    let valid_to = u32::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 30 * 60,
    )
    .unwrap();
    let input_amount = U256::from(1_010);
    let sign = || {
        owner.sign_public_swap_delivery(PublicSwapDeliverySigning {
            operation,
            swap_use: id,
            delegated,
            origin: &origin_chain,
            across: &across,
            input_amount,
            valid_to,
            authorization: &authorization,
            notes: None,
        })
    };
    // What the latest quote request carried under `name`.
    let sent = |name: &str| {
        let (path, _) = requests.lock().unwrap().last().cloned().unwrap();
        url::Url::parse(&format!("http://across{path}"))
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap()
    };

    let PublicSwapDelivery::ReviewRequired(change) = sign().await.unwrap() else {
        panic!("a quote below the approved destination minimum signs no delivery");
    };
    assert_eq!(
        change,
        SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(999),
        }
    );

    // The request named the deposit, the handler as recipient and the real message: the drain
    // to the destination account, then that account's shield, with the account as fallback.
    assert_eq!(
        (
            sent("inputToken").parse::<Address>().unwrap(),
            sent("outputToken").parse::<Address>().unwrap(),
            sent("originChainId"),
            sent("destinationChainId"),
            sent("amount"),
        ),
        (
            USDC,
            DESTINATION_TOKEN,
            "1".to_owned(),
            DESTINATION_CHAIN.to_string(),
            input_amount.to_string(),
        )
    );
    assert_eq!(sent("recipient").parse::<Address>().unwrap(), handler);
    let message = sent("message").parse::<Bytes>().unwrap();
    let instructions = MulticallHandler::Instructions::abi_decode(&message).unwrap();
    let [drain, shield] = instructions.calls.as_slice() else {
        panic!("the message drains, then shields");
    };
    assert_eq!((drain.target, shield.target), (handler, executor));
    let drained = MulticallHandler::drainLeftoverTokensCall::abi_decode(&drain.callData).unwrap();
    assert_eq!(
        (drained.token, drained.destination),
        (DESTINATION_TOKEN, executor)
    );
    assert_eq!(instructions.fallbackRecipient, executor);
    assert_eq!(
        message,
        private_delivery_message(
            handler,
            DESTINATION_TOKEN,
            executor,
            shield.callData.clone(),
            Some(executor),
        )
    );

    // That shield is in the account's record under the swap's use, guarded by the approved
    // destination minimum, and the Public account has signed nothing.
    let stopped = saved(&store, operation, id);
    let [shield_hash] = stopped.shields.as_slice() else {
        panic!("one shield is recorded for the use");
    };
    let payload = stopped
        .record
        .issued()
        .iter()
        .find(|payload| payload.hash() == *shield_hash)
        .unwrap();
    assert_eq!(
        (payload.purpose(), payload.context().calldata()),
        (
            ExecutorPayloadPurpose::SwapDestinationShield,
            &shield.callData
        )
    );
    let signed = RelayAdapt7702::multicallCall::abi_decode(&shield.callData).unwrap();
    let guard = transferCall::abi_decode(&signed._calls[0].data).unwrap();
    assert_eq!(
        (guard._transfers[0].to, guard._transfers[0].value),
        (executor, U256::from(1_000))
    );
    assert!(stopped.swap.path().is_none());
    assert!(stopped.swap.transactions().is_empty());
    assert!(stopped.swap.bridge().is_none());

    // The user accepts a new minimum at full review. The destination's bound address and its
    // original setup need cannot be changed by that review.
    let mut approval = stopped.swap.approval().clone();
    approval.bounds.destination_minimum = Some(U256::from(990));
    for destination in [
        SwapApprovedAccount {
            address: None,
            ..approval.destination
        },
        SwapApprovedAccount {
            setup: false,
            ..approval.destination
        },
    ] {
        assert!(
            owner
                .reapprove_public_swap(
                    operation,
                    id,
                    PublicSwapApproval {
                        destination,
                        ..approval.clone()
                    }
                )
                .is_err()
        );
    }
    let reapproved = owner
        .reapprove_public_swap(operation, id, approval.clone())
        .unwrap();
    assert_eq!(
        reapproved.public_swap_use(id).unwrap().1.approval(),
        &approval
    );
    assert_eq!(reapproved.issued(), stopped.record.issued());

    // The unchanged quote now covers the approved minimum and returns fresh terms, paid to
    // the handler with the hash of the new message the request carried.
    let PublicSwapDelivery::Signed {
        delivery,
        message,
        terms,
    } = sign().await.unwrap()
    else {
        panic!("the quote covers the freshly reviewed minimum");
    };
    assert_eq!(sent("message").parse::<Bytes>().unwrap(), message);
    assert_eq!(
        (terms.recipient, terms.message_hash),
        (Some(handler), Some(keccak256(&message)))
    );
    assert_eq!(
        (terms.input_amount, terms.output_amount),
        (input_amount, U256::from(990))
    );
    assert_eq!(
        (terms.spoke_pool, terms.input_token, terms.output_token),
        (spoke_pool, USDC, DESTINATION_TOKEN)
    );
    assert_eq!(
        (
            delivery.handler,
            delivery.destination_executor,
            delivery.fallback
        ),
        (handler, executor, Some(executor))
    );
    // The returned shield is recorded too, and the path is still the Public account's to sign.
    let signed = saved(&store, operation, id);
    assert!(signed.shields.contains(shield_hash));
    assert_ne!(delivery.shield_multicall, shield.callData);
    let new_shield = RelayAdapt7702::multicallCall::abi_decode(&delivery.shield_multicall).unwrap();
    let guard = transferCall::abi_decode(&new_shield._calls[0].data).unwrap();
    assert_eq!(guard._transfers[0].value, U256::from(990));
    assert!(signed.record.issued().iter().any(|payload| {
        signed.shields.contains(&payload.hash())
            && *payload.context().calldata() == delivery.shield_multicall
    }));
    assert!(signed.swap.path().is_none());

    across_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// An order's signing-time quote below the approved destination minimum is taken again for a
// raised buy amount when the raise is within a fifth of the approved allowed gas: the terms
// are for that amount and the approved minimum stands. A larger shortfall returns the swap to
// review with the first quote's minimum, and Across isn't asked again.
#[tokio::test]
async fn a_public_orders_short_signing_time_quote_within_the_cushion_raises_its_buy_amount() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let (origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
    let profile = destination_chain.accepted_executor_profile().unwrap();
    let spoke_pool = origin_chain.bridge_origin_profile().unwrap().spoke_pool();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        destination_chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();
    let mut candidate = broadcaster(profile.delegate());
    candidate.chain_id = DESTINATION_CHAIN;
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let authorization = password();
    // The order buys at least 1,010 for a destination minimum of 1,000, and its approval
    // allows 100 for gas: a cushion of 20.
    let mut request = claim(id, operation, 1, USDC, DESTINATION_TOKEN);
    request.intent.order = true;
    request.approval.sell_token = ORDER_SELL_TOKEN;
    request.approval.bounds.buy_amount = U256::from(1_010);
    request.approval.bounds.gas_allowance = Some(U256::from(100));
    owner.claim_public_swap(request).unwrap();
    let executor = owner
        .prepare_public_swap_destination(operation, id, Some(candidate), &authorization)
        .await
        .unwrap()
        .executor();
    let delegated = delegate_setup(&store, operation, profile);
    assert_eq!(delegated.executor(), executor);

    // Across answers each quote with the next of the outputs the test sets.
    let outputs = Arc::new(Mutex::new(Vec::<U256>::new()));
    let answers = outputs.clone();
    let (across_url, requests, across_task) = spawn_bridge_stub(move |_| {
        across_fee_quote(spoke_pool, answers.lock().unwrap().remove(0), 3 * 60 * 60)
    })
    .await;
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let across = crate::bridge::AcrossClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        across_url,
    )
    .unwrap();
    let valid_to = u32::try_from(unix_now() + 30 * 60).unwrap();
    let sign = || {
        owner.sign_public_swap_delivery(PublicSwapDeliverySigning {
            operation,
            swap_use: id,
            delegated,
            origin: &origin_chain,
            across: &across,
            input_amount: U256::from(1_010),
            valid_to,
            authorization: &authorization,
            notes: None,
        })
    };
    // The amount and the message of every quote request so far.
    let quoted = || -> Vec<(String, String)> {
        requests
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| {
                let url = url::Url::parse(&format!("http://across{path}")).unwrap();
                let sent = |name: &str| {
                    url.query_pairs()
                        .find(|(key, _)| key == name)
                        .map(|(_, value)| value.into_owned())
                        .unwrap()
                };
                (sent("amount"), sent("message"))
            })
            .collect()
    };

    // 900 for 1,010 needs a deposit of 1,124, which is 114 more: beyond the cushion.
    *outputs.lock().unwrap() = vec![U256::from(900)];
    let PublicSwapDelivery::ReviewRequired(change) = sign().await.unwrap() else {
        panic!("a shortfall beyond the cushion signs no delivery");
    };
    assert_eq!(
        change,
        SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(900),
        }
    );
    assert_eq!(quoted().len(), 1);

    // 999 for 1,010 needs ceil(1,010 * 1,000 / 999) = 1,012 and a margin of 1: 3 more, within
    // the cushion. Across quotes 1,013 with the same message, and its 1,002 covers the minimum.
    *outputs.lock().unwrap() = vec![U256::from(999), U256::from(1_002)];
    let PublicSwapDelivery::Signed { message, terms, .. } = sign().await.unwrap() else {
        panic!("a shortfall within the cushion signs the delivery");
    };
    assert_eq!(
        (terms.input_amount, terms.output_amount),
        (U256::from(1_013), U256::from(1_000))
    );
    assert_eq!(terms.message_hash, Some(keccak256(&message)));
    let quoted = quoted();
    let [
        _,
        (first_amount, first_message),
        (raised_amount, raised_message),
    ] = quoted.as_slice()
    else {
        panic!("the delivery was quoted twice more");
    };
    assert_eq!(
        (first_amount.as_str(), raised_amount.as_str()),
        ("1010", "1013")
    );
    assert_eq!(first_message, raised_message);
    assert_eq!(raised_message.parse::<Bytes>().unwrap(), message);
    // The Public account has still signed nothing.
    assert!(saved(&store, operation, id).swap.path().is_none());

    across_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// A restart after the destination setup is confirmed, before the Public account approves
// anything, resumes the same claim: the delegation is confirmed from the chain again and the
// swap's record is as it was.
#[tokio::test]
async fn a_restart_between_setup_and_approval_resumes_the_public_swap_unsigned() {
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    // The destination chain is the mock chain, whose history holds the setup.
    let mut config =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let profile = config.accepted_executor_profile().unwrap();
    let railgun = config.require_railgun().unwrap().deployment.contract;
    let mut mock = MockChain::new(1, railgun, profile.delegate());
    mock.head = 14;
    mock.nonces = vec![(11, 1)];
    let chain = Arc::new(Mutex::new(mock));
    let served = chain.clone();
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| served.lock().unwrap().respond(&request)),
        Arc::default(),
        Arc::default(),
    )
    .await;
    config.finality_depth = 1;
    config.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
    let start = |generation| {
        ExecutorOwner::new(
            generation,
            db.clone(),
            view.clone(),
            config.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap()
    };
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let (operation, id) = (
        ExecutorOperationId::random().unwrap(),
        SwapUseId::random().unwrap(),
    );
    let request = || claim(id, operation, DESTINATION_CHAIN, DESTINATION_TOKEN, USDC);

    // The claimed account is the mock chain's executor. Its setup won nonce 0 in block 11 and
    // is reconciled there.
    let owner = start(0);
    owner.claim_public_swap(request()).unwrap();
    store.bind_address(operation, EXECUTOR).unwrap();
    store
        .bind_public_swap_destination(operation, id, EXECUTOR)
        .unwrap();
    let before_setup = ExecutorNonceObservation::new(BlockNumHash::new(10, B256::ZERO), U256::ZERO);
    store.reconcile(operation, before_setup, &[]).unwrap();
    let setup_call = execute(vec![private_transaction(0x11, 0x12)], Vec::new(), 0);
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                profile.delegate(),
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(setup_call.clone(), before_setup, Vec::new()),
            ),
        )
        .unwrap();
    chain
        .lock()
        .unwrap()
        .add_transaction(11, EXECUTOR, setup_call, private_logs(0x11, 0x12));
    let reconciled = owner.reconcile_history(operation, 10..14).await.unwrap();
    assert_eq!(
        reconciled.record().payload_status(setup),
        Some(ExecutorPayloadStatus::Executed)
    );
    let before = saved(&store, operation, id);
    owner.shutdown().await;
    drop(owner);

    // The same request returns the same claim after the restart, and the destination account
    // is confirmed delegated at the confirmed block.
    let restarted = start(1);
    assert_eq!(
        restarted.claim_public_swap(request()).unwrap(),
        before.record
    );
    let delegated = restarted
        .delegated_public_swap_destination(operation, 13, id, None)
        .await
        .unwrap();
    assert_eq!(
        (
            delegated.operation(),
            delegated.executor(),
            delegated.setup_payload()
        ),
        (operation, EXECUTOR, setup)
    );

    // Nothing of the swap changed, and the Public account has no transaction.
    let after = saved(&store, operation, id);
    assert_eq!(after.swap, before.swap);
    assert!(after.swap.path().is_none());
    assert!(after.swap.transactions().is_empty());
    assert!(after.shields.is_empty());

    server.abort();
    restarted.shutdown().await;
    drop(restarted);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

/// The current time, Unix seconds.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The deposit id the origin chain's pool gives every deposit.
const DEPOSIT_ID: u64 = 77;
/// The block the origin chain starts at.
const ORIGIN_HEAD: u64 = 20;
/// The fee the Public account's transactions were reviewed at.
const REVIEWED_FEE: PublicActionGasFeeSelection = PublicActionGasFeeSelection::Custom {
    max_fee_per_gas: 100,
    max_priority_fee_per_gas: 1,
};

/// The token an order sells on the origin chain.
const ORDER_SELL_TOKEN: Address = Address::repeat_byte(0x5e);
/// The hook gas limit an order's approval binds.
const ORDER_HOOK_GAS_LIMIT: u64 = 517_000;
/// The id of the quote an order was reviewed with.
const ORDER_QUOTE_ID: i64 = 42;

/// The math contract's runtime code: what its creation code returns after the 30-byte
/// constructor.
fn swap_math_runtime_code() -> Bytes {
    Bytes::from_static(&SWAP_MATH_CREATION_CODE[30..])
}

/// Each order request an orderbook stub received: the swap's order as the record held it when
/// the request arrived, and the request's body.
type OrderRequests = Arc<Mutex<Vec<(Option<PublicSwapOrder>, Value)>>>;

/// The chain a Public account pays on: a `MockChain` that also answers what the account's
/// transaction step asks, and includes each broadcast transaction in the next block. An
/// approval sets the allowance it names, and a deposit emits the pool's event unless deposits
/// revert. A read of the settlement's fill of an order is the mock chain's.
struct OriginChain {
    chain: MockChain,
    source: Address,
    spoke_pool: Address,
    allowance: U256,
    deposits_revert: bool,
    /// What every balance read answers. `None` fails the read.
    balance: Option<U256>,
    /// Addresses without code. Every other address has some.
    codeless: Vec<Address>,
    /// The code at the math contract's address, which an order's post-hook calls.
    math_code: Bytes,
    /// The logs of the next broadcast transaction that isn't a deposit.
    receipt_logs: Vec<(Address, alloy::primitives::LogData)>,
    /// A block number and a timestamp: blocks from that number on are stamped from that time,
    /// one second apart. Earlier blocks keep the mock chain's own time, long past.
    late_from: Option<(u64, u64)>,
    /// Every request served, in order.
    requests: Vec<Value>,
    /// The token, owner and spender of every allowance read.
    allowance_reads: Vec<(Address, Address, Address)>,
    /// The token and account of every balance read.
    balance_reads: Vec<(Address, Address)>,
    /// Every transaction broadcast, in order.
    broadcasts: Vec<(B256, TxEnvelope)>,
    /// Called with a transaction's hash when its broadcast arrives, before it is included.
    on_broadcast: Box<dyn FnMut(B256) + Send>,
}

impl OriginChain {
    fn methods(&self) -> impl Iterator<Item = &str> {
        self.requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
    }

    fn respond(&mut self, request: &Value) -> Value {
        self.requests.push(request.clone());
        let params = &request["params"];
        let result = match request["method"].as_str().unwrap() {
            "eth_gasPrice" => json!("0x64"),
            "eth_maxPriorityFeePerGas" => json!("0x1"),
            "eth_feeHistory" => {
                return json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                    "code": -32601, "message": "fee history is unsupported"
                }});
            }
            "eth_getTransactionCount" => json!(format!("0x{:x}", self.broadcasts.len())),
            "eth_estimateGas" => json!("0xc350"),
            "eth_getBalance" => json!("0xde0b6b3a7640000"),
            "eth_call" => {
                let call = &params[0];
                let input: Bytes = serde_json::from_value(
                    call.get("input").unwrap_or_else(|| &call["data"]).clone(),
                )
                .unwrap();
                let token = serde_json::from_value(call["to"].clone()).unwrap();
                if let Ok(read) = PublicErc20::balanceOfCall::abi_decode(&input) {
                    self.balance_reads.push((token, read.account));
                    let Some(balance) = self.balance else {
                        return json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                            "code": -32000, "message": "the balance is unavailable"
                        }});
                    };
                    serde_json::to_value(B256::from(balance)).unwrap()
                } else if let Ok(read) = PublicErc20::allowanceCall::abi_decode(&input) {
                    self.allowance_reads.push((token, read.owner, read.spender));
                    serde_json::to_value(B256::from(self.allowance)).unwrap()
                } else {
                    return self.chain.respond(request);
                }
            }
            "eth_sendRawTransaction" => {
                let raw: Bytes = serde_json::from_value(params[0].clone()).unwrap();
                let hash = keccak256(&raw);
                (self.on_broadcast)(hash);
                let transaction = TxEnvelope::decode_2718(&mut &raw[..]).unwrap();
                let number = self.chain.head + 1;
                self.chain.head = number;
                let (status, logs) = if transaction.to() == Some(self.spoke_pool) {
                    let deposit =
                        SpokePool::depositV3Call::abi_decode(transaction.input()).unwrap();
                    let event = SpokePool::FundsDeposited {
                        inputToken: address_to_bytes32(deposit.inputToken),
                        outputToken: address_to_bytes32(deposit.outputToken),
                        inputAmount: deposit.inputAmount,
                        outputAmount: deposit.outputAmount,
                        destinationChainId: deposit.destinationChainId,
                        depositId: U256::from(DEPOSIT_ID),
                        quoteTimestamp: deposit.quoteTimestamp,
                        fillDeadline: deposit.fillDeadline,
                        exclusivityDeadline: 0,
                        depositor: address_to_bytes32(deposit.depositor),
                        recipient: address_to_bytes32(deposit.recipient),
                        exclusiveRelayer: address_to_bytes32(deposit.exclusiveRelayer),
                        message: deposit.message,
                    };
                    if self.deposits_revert {
                        (false, Vec::new())
                    } else {
                        (true, vec![(self.spoke_pool, event.encode_log_data())])
                    }
                } else {
                    // `approve(spender, amount)`: the amount is the second argument. Any other
                    // transaction succeeds and changes nothing here.
                    let approve = build_approve_calldata(Address::ZERO, U256::ZERO);
                    if transaction.input().get(..4) == Some(&approve[..4]) {
                        self.allowance = U256::from_be_slice(&transaction.input()[36..68]);
                    }
                    (true, std::mem::take(&mut self.receipt_logs))
                };
                self.chain
                    .include(number, transaction.clone(), self.source, status, logs);
                self.broadcasts.push((hash, transaction));
                json!(hash)
            }
            "eth_getCode"
                if serde_json::from_value::<Address>(params[0].clone()).unwrap()
                    == SWAP_MATH_ADDRESS =>
            {
                json!(self.math_code)
            }
            "eth_getCode"
                if self
                    .codeless
                    .contains(&serde_json::from_value(params[0].clone()).unwrap()) =>
            {
                json!("0x")
            }
            "eth_getBlockByNumber" => {
                // The fee quote reads the latest block by its tag.
                let mut numbered = request.clone();
                if params[0] == "latest" {
                    numbered["params"][0] = json!(format!("0x{:x}", self.chain.head));
                }
                let mut response = self.chain.respond(&numbered);
                let number = response["result"]["number"].as_str().map(|number| {
                    u64::from_str_radix(number.trim_start_matches("0x"), 16).unwrap()
                });
                if let (Some((from, timestamp)), Some(number)) = (self.late_from, number)
                    && number >= from
                {
                    response["result"]["timestamp"] =
                        json!(format!("0x{:x}", timestamp + (number - from)));
                }
                return response;
            }
            _ => return self.chain.respond(request),
        };
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    }
}

/// A claimed swap whose Public account deposits on the mock origin chain and whose destination
/// account, on chain 1, is set up and holds the delivery's signed shield.
struct PublicPayment {
    root: std::path::PathBuf,
    db: Arc<DbStore>,
    vault: DesktopVaultStore,
    view: Arc<DesktopViewSession>,
    origin: EffectiveChainConfig,
    destination: EffectiveChainConfig,
    chain: Arc<Mutex<OriginChain>>,
    /// The chain the swap delivers to, where its fill is read.
    delivery_chain: Arc<Mutex<MockChain>>,
    servers: [tokio::task::JoinHandle<()>; 2],
    store: ExecutorStore,
    operation: ExecutorOperationId,
    id: SwapUseId,
    signer: VaultedPublicSigner,
    source: Address,
    spoke_pool: Address,
    sell_amount: U256,
    delivery: AcrossPrivateDelivery,
    terms: AcrossOrderTerms,
    /// The message the delivery's deposit carries, whose hash the terms sign.
    message: Bytes,
    /// Until when an order of the swap is valid, Unix seconds.
    valid_to: u32,
    /// The nonce an order's hook batch is signed under.
    batch_nonce: B256,
    /// The block the orderbook stub reports the order's trade in. `None` reports no trade.
    trade_block: Arc<Mutex<Option<u64>>>,
    /// The destination account's recorded shield.
    shield: B256,
    /// The swap's record as each broadcast found it, with the broadcast transaction's hash.
    at_broadcast: Arc<Mutex<Vec<(B256, PublicSwapRecord)>>>,
    /// Notified when a broadcast arrives.
    broadcast: Arc<Notify>,
}

impl PublicPayment {
    /// A swap that sells `sell_token` on the origin chain, where the Public account's
    /// allowance to the pool is `allowance`. Its approval covers one approval and the deposit.
    async fn start(sell_token: Address, allowance: U256) -> (Self, ExecutorOwner) {
        Self::start_on(sell_token, allowance, false).await
    }

    /// A swap whose Public account sells another token through an order that buys at least
    /// 1,010 of the bridged token, with a signed delivery quoted for that amount and for
    /// [`Self::valid_to`]. Its approval allows 100 for gas, so its cushion is 20.
    async fn start_order() -> (Self, ExecutorOwner) {
        Self::start_on(ORDER_SELL_TOKEN, U256::MAX, true).await
    }

    async fn start_on(sell_token: Address, allowance: U256, order: bool) -> (Self, ExecutorOwner) {
        let (root, db, vault) = desktop_store_with_vault();
        let view = Arc::new(import_wallet_with_metadata(
            &vault,
            TEST_WALLET_ID,
            "Wallet",
        ));
        let configs = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap();
        let mut destination = configs.get(1).cloned().unwrap();
        let mut origin = configs.get(DESTINATION_CHAIN).cloned().unwrap();
        let profile = destination.accepted_executor_profile().unwrap();
        let railgun = destination.require_railgun().unwrap().deployment.contract;
        let spoke_pool = origin.bridge_origin_profile().unwrap().spoke_pool();
        let handler = destination.bridge_profile().unwrap().multicall_handler();
        let signer =
            VaultedPublicSigner::Software(SoftwareEvmSigner::from_private_key([7; 32]).unwrap());
        let source = signer.address();

        let mut mock = MockChain::new(DESTINATION_CHAIN, railgun, profile.delegate());
        mock.head = ORIGIN_HEAD;
        let chain = Arc::new(Mutex::new(OriginChain {
            chain: mock,
            source,
            spoke_pool,
            allowance,
            deposits_revert: false,
            balance: Some(U256::ZERO),
            codeless: Vec::new(),
            math_code: swap_math_runtime_code(),
            receipt_logs: Vec::new(),
            late_from: None,
            requests: Vec::new(),
            allowance_reads: Vec::new(),
            balance_reads: Vec::new(),
            broadcasts: Vec::new(),
            on_broadcast: Box::new(|_| {}),
        }));
        let served = chain.clone();
        let (origin_endpoint, origin_server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: Value| served.lock().unwrap().respond(&request)),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let delivery_chain = Arc::new(Mutex::new(MockChain::new(1, railgun, profile.delegate())));
        let delivering = delivery_chain.clone();
        let (destination_endpoint, destination_server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: Value| delivering.lock().unwrap().respond(&request)),
            Arc::default(),
            Arc::default(),
        )
        .await;
        origin.finality_depth = 1;
        origin.rpc_route = crate::RpcChainRoute::new(DESTINATION_CHAIN, vec![origin_endpoint]);
        destination.finality_depth = 1;
        destination.rpc_route = crate::RpcChainRoute::new(1, vec![destination_endpoint]);

        let owner = ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            destination.clone(),
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        let (operation, id) = (
            ExecutorOperationId::random().unwrap(),
            SwapUseId::random().unwrap(),
        );
        let mut request = claim(id, operation, DESTINATION_CHAIN, DESTINATION_TOKEN, USDC);
        request.source = source;
        request.approval.sell_token = sell_token;
        request.approval.max_gas_cost = public_swap_gas_plan(&origin, 1, true, 100, 1)
            .unwrap()
            .max_gas_cost;
        if order {
            request.intent.order = true;
            request.approval.bounds.buy_amount = U256::from(1_010);
            request.approval.bounds.gas_allowance = Some(U256::from(100));
            request.approval.bounds.post_hook_gas_limit = Some(ORDER_HOOK_GAS_LIMIT);
        }
        let sell_amount = request.approval.bounds.sell_amount;
        // A deposit bridges what it sells, an order what it buys.
        let input_amount = if order {
            request.approval.bounds.buy_amount
        } else {
            sell_amount
        };
        owner.claim_public_swap(request).unwrap();
        store.bind_address(operation, EXECUTOR).unwrap();
        store
            .bind_public_swap_destination(operation, id, EXECUTOR)
            .unwrap();
        let _delegated = delegate_setup(&store, operation, profile);

        // The delivery's shield is in the destination account's record, as signing leaves it.
        let shield_multicall = Bytes::from_static(b"signed destination shield");
        let shield = B256::repeat_byte(70);
        store
            .record_swap_destination_shield(
                operation,
                id,
                IssuedExecutorPayload::new(
                    U256::ONE,
                    profile.delegate(),
                    shield,
                    ExecutorPayloadPurpose::SwapDestinationShield,
                    ExecutorPayloadContext::new(
                        shield_multicall.clone(),
                        ExecutorNonceObservation::new(
                            BlockNumHash::new(12, B256::repeat_byte(12)),
                            U256::ONE,
                        ),
                        Vec::new(),
                    ),
                ),
            )
            .unwrap();
        let message = private_delivery_message(
            handler,
            USDC,
            EXECUTOR,
            shield_multicall.clone(),
            Some(EXECUTOR),
        );
        let delivery = AcrossPrivateDelivery {
            handler,
            destination_executor: EXECUTOR,
            shield_multicall,
            fallback: Some(EXECUTOR),
        };
        let now = u32::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        // An order's terms are checked against its validity, so they are quoted now.
        let (quote_timestamp, fill_deadline) = if order {
            (now - 5, now + 3 * 60 * 60)
        } else {
            (1_700_000_000, 1_700_021_600)
        };
        let terms = AcrossOrderTerms {
            spoke_pool,
            input_token: DESTINATION_TOKEN,
            output_token: USDC,
            input_amount,
            output_amount: U256::from(1_000),
            quote_timestamp,
            fill_deadline,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
            recipient: Some(handler),
            message_hash: Some(keccak256(&message)),
        };
        let batch_nonce = new_public_swap_batch_nonce().unwrap();

        let at_broadcast = Arc::new(Mutex::new(Vec::new()));
        let broadcast = Arc::new(Notify::new());
        let (seen, arrived) = (at_broadcast.clone(), broadcast.clone());
        let reader = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        chain.lock().unwrap().on_broadcast = Box::new(move |hash| {
            seen.lock()
                .unwrap()
                .push((hash, saved(&reader, operation, id).swap));
            arrived.notify_one();
        });
        (
            Self {
                root,
                db,
                vault,
                view,
                origin,
                destination,
                chain,
                delivery_chain,
                servers: [origin_server, destination_server],
                store,
                operation,
                id,
                signer,
                source,
                spoke_pool,
                sell_amount,
                delivery,
                terms,
                message,
                valid_to: now + 5 * 60,
                batch_nonce,
                trade_block: Arc::default(),
                shield,
                at_broadcast,
                broadcast,
            },
            owner,
        )
    }

    fn saved(&self) -> Saved {
        saved(&self.store, self.operation, self.id)
    }

    async fn approve(&self, owner: &ExecutorOwner) -> eyre::Result<()> {
        owner
            .submit_public_swap_approvals_with_signer(
                self.operation,
                self.id,
                &self.origin,
                &self.signer,
                REVIEWED_FEE,
                &mut |_: PublicActionProgressUpdate| {},
            )
            .await
    }

    async fn deposit(&self, owner: &ExecutorOwner) -> eyre::Result<PublicSwapTransactionOutcome> {
        owner
            .submit_public_swap_deposit_with_signer(
                self.operation,
                self.id,
                &self.origin,
                &self.signer,
                REVIEWED_FEE,
                &self.delivery,
                &self.terms,
                &mut |_: PublicActionProgressUpdate| {},
            )
            .await
    }

    /// Sign and submit the swap's order for `delivery` and `terms`.
    async fn order_for(
        &self,
        owner: &ExecutorOwner,
        orderbook: &CowOrderbookClient,
        delivery: &AcrossPrivateDelivery,
        terms: &AcrossOrderTerms,
    ) -> eyre::Result<PublicSwapOrderOutcome> {
        owner
            .submit_public_swap_order_with_signer(
                self.operation,
                self.id,
                &self.origin,
                &self.signer,
                orderbook,
                delivery,
                terms,
                self.valid_to,
                self.batch_nonce,
                Some(ORDER_QUOTE_ID),
                false,
            )
            .await
    }

    /// Sign and submit the swap's order for its signed delivery.
    async fn order(
        &self,
        owner: &ExecutorOwner,
        orderbook: &CowOrderbookClient,
    ) -> eyre::Result<PublicSwapOrderOutcome> {
        self.order_for(owner, orderbook, &self.delivery, &self.terms)
            .await
    }

    /// The Public account's cow-shed proxy on the origin chain.
    fn proxy(&self) -> Address {
        let profile = self.origin.public_swap_profile().unwrap();
        proxy_address(
            profile.cow_shed_factory(),
            profile.cow_shed_implementation(),
            self.source,
        )
    }

    /// An orderbook on the origin chain that records each order request with the swap's order
    /// as the record held it when the request arrived. With `lose_first_response` it accepts
    /// the first order without answering, and answers the next as a duplicate. A read of the
    /// order's trades answers [`Self::trade_block`].
    async fn orderbook(
        &self,
        lose_first_response: bool,
    ) -> (
        CowOrderbookClient,
        OrderRequests,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/polygon", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let requests = OrderRequests::default();
        let recorded = requests.clone();
        let reader = ExecutorStore::new(self.db.clone(), self.view.clone(), 1).unwrap();
        let (operation, id) = (self.operation, self.id);
        let settlement = self.settlement();
        let trade_block = self.trade_block.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut stream = BufReader::new(stream);
                let mut request_line = String::new();
                if stream.read_line(&mut request_line).await.unwrap() == 0 {
                    continue;
                }
                let Some(body) = read_json_body(&mut stream).await else {
                    continue;
                };
                if request_line.starts_with("GET") {
                    // The only read is of the order's trades, by its UID.
                    assert!(request_line.contains("/api/v1/trades?orderUid=0x"));
                    let reported = *trade_block.lock().unwrap();
                    let reply = reported
                        .map_or_else(|| json!([]), |block| json!([{"blockNumber": block}]))
                        .to_string();
                    stream
                        .get_mut()
                        .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
                        .await
                        .unwrap();
                    continue;
                }
                let owner = body["from"].as_str().unwrap().parse().unwrap();
                let uid = order_uid(
                    &submitted_order(&body),
                    DESTINATION_CHAIN,
                    settlement,
                    owner,
                );
                let persisted = saved(&reader, operation, id).swap.order().cloned();
                recorded.lock().unwrap().push((persisted, body));
                let served = recorded.lock().unwrap().len();
                if lose_first_response && served == 1 {
                    continue; // The server accepted the order, but the response was lost.
                }
                let (status, reply) = if lose_first_response {
                    (
                        "400 Bad Request",
                        json!({"errorType": "DuplicatedOrder", "description": "order already exists"})
                            .to_string(),
                    )
                } else {
                    ("201 Created", json!(uid.0).to_string())
                };
                stream
                    .get_mut()
                    .write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
                    .await
                    .unwrap();
            }
        });
        let client = CowOrderbookClient::new(
            OperationHttpClient::for_tests(
                reqwest::Client::new(),
                OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
            ),
            url,
            DESTINATION_CHAIN,
        )
        .unwrap();
        (client, requests, task)
    }

    /// One more block on the origin chain, which makes the one before it final.
    fn mine(&self) {
        self.chain.lock().unwrap().chain.head += 1;
    }

    /// The origin chain's pinned settlement contract.
    fn settlement(&self) -> Address {
        self.origin.public_swap_profile().unwrap().settlement()
    }

    /// The pool's event for a deposit of this swap's signed delivery that put in `input` for
    /// `output`.
    fn deposited(&self, input: U256, output: U256) -> SpokePool::FundsDeposited {
        SpokePool::FundsDeposited {
            inputToken: address_to_bytes32(self.terms.input_token),
            outputToken: address_to_bytes32(self.terms.output_token),
            inputAmount: input,
            outputAmount: output,
            destinationChainId: U256::from(1),
            depositId: U256::from(DEPOSIT_ID),
            quoteTimestamp: self.terms.quote_timestamp,
            fillDeadline: self.terms.fill_deadline,
            exclusivityDeadline: 0,
            depositor: address_to_bytes32(self.source),
            recipient: address_to_bytes32(self.terms.recipient.unwrap()),
            exclusiveRelayer: address_to_bytes32(self.terms.exclusive_relayer),
            message: self.message.clone(),
        }
    }

    /// A transaction to `to` in a new block of the origin chain, whose receipt carries `logs`.
    /// Returns the block's number.
    fn include(&self, to: Address, logs: Vec<(Address, alloy::primitives::LogData)>) -> u64 {
        let mut origin = self.chain.lock().unwrap();
        let number = origin.chain.head + 1;
        origin.chain.head = number;
        origin
            .chain
            .add_addressed_transaction(number, to, Bytes::new(), logs);
        number
    }

    /// The settlement's event for a fill of the order `uid` that buys `paid`.
    fn trade(&self, uid: OrderUid, paid: U256) -> (Address, alloy::primitives::LogData) {
        (
            self.settlement(),
            Trade {
                owner: self.source,
                sellToken: ORDER_SELL_TOKEN,
                buyToken: DESTINATION_TOKEN,
                sellAmount: self.sell_amount,
                buyAmount: paid,
                feeAmount: U256::ZERO,
                orderUid: uid.0.to_vec().into(),
            }
            .encode_log_data(),
        )
    }

    /// A settlement in a new block that fills the order `uid` and pays `paid` of the bought
    /// token to the proxy. Its receipt also holds `deposit`, when the order's hook ran. The
    /// orderbook stub reports the trade from then on. Returns the block's number.
    fn settle(
        &self,
        uid: OrderUid,
        paid: U256,
        deposit: Option<&SpokePool::FundsDeposited>,
    ) -> u64 {
        let settlement = self.settlement();
        let mut logs = vec![
            self.trade(uid, paid),
            (
                DESTINATION_TOKEN,
                Transfer {
                    from: settlement,
                    to: self.proxy(),
                    value: paid,
                }
                .encode_log_data(),
            ),
        ];
        if let Some(deposit) = deposit {
            logs.push((self.spoke_pool, deposit.encode_log_data()));
        }
        let number = self.include(settlement, logs);
        *self.trade_block.lock().unwrap() = Some(number);
        number
    }

    /// Submit the swap's order, and return its UID.
    async fn submitted(&self, owner: &ExecutorOwner, orderbook: &CowOrderbookClient) -> OrderUid {
        let PublicSwapOrderOutcome::Submitted { uid } = self.order(owner, orderbook).await.unwrap()
        else {
            panic!("the order is submitted");
        };
        uid
    }

    /// Read what became of the swap's order, and return the swap as the record then holds it.
    async fn observe(
        &self,
        owner: &ExecutorOwner,
        orderbook: &CowOrderbookClient,
    ) -> PublicSwapRecord {
        owner
            .observe_public_swap_order(self.operation, self.id, &self.origin, orderbook)
            .await
            .unwrap();
        self.saved().swap
    }

    /// How many log queries the origin chain has served.
    fn log_queries(&self) -> usize {
        self.chain
            .lock()
            .unwrap()
            .methods()
            .filter(|method| *method == "eth_getLogs")
            .count()
    }

    /// The swap's order state now.
    fn state(&self) -> Option<PublicSwapOrderState> {
        public_swap_order_state(&self.saved().swap)
    }

    /// Deposit from the Public account, and read the hand-off once its block is final.
    async fn handed_off(&self, owner: &ExecutorOwner) -> SwapBridgeHandoff {
        self.deposit(owner).await.unwrap();
        self.mine();
        owner
            .observe_public_swap_handoff(self.operation, self.id, &self.origin)
            .await
            .unwrap()
            .unwrap()
    }

    /// A relayer's fill of the swap's deposit in a new block of the destination chain, which
    /// the block after it makes final: the pinned pool's event for a deposit of `input` for
    /// `output`, the handler's transfer of `output` to the destination account and, with
    /// `shielded`, that account's shield. Returns the block and the fill's transaction.
    fn fill(&self, input: U256, output: U256, shielded: bool) -> (BlockNumHash, B256) {
        let profile = self.destination.bridge_profile().unwrap();
        let (pool, handler) = (profile.spoke_pool(), profile.multicall_handler());
        let railgun = self
            .destination
            .require_railgun()
            .unwrap()
            .deployment
            .contract;
        let recipient = address_to_bytes32(handler);
        let message_hash = self.terms.message_hash.unwrap();
        let fill = SpokePool::FilledRelay {
            inputToken: address_to_bytes32(self.terms.input_token),
            outputToken: address_to_bytes32(self.terms.output_token),
            inputAmount: input,
            outputAmount: output,
            repaymentChainId: U256::ONE,
            originChainId: U256::from(DESTINATION_CHAIN),
            depositId: U256::from(DEPOSIT_ID),
            fillDeadline: self.terms.fill_deadline,
            exclusivityDeadline: 0,
            exclusiveRelayer: B256::ZERO,
            relayer: address_to_bytes32(Address::repeat_byte(0x55)),
            depositor: address_to_bytes32(self.source),
            recipient,
            messageHash: message_hash,
            relayExecutionInfo: V3RelayExecutionEventInfo {
                updatedRecipient: recipient,
                updatedMessageHash: message_hash,
                updatedOutputAmount: output,
                fillType: 0,
            },
        };
        let transfer = |from: Address, to: Address, value: U256| {
            (USDC, Transfer { from, to, value }.encode_log_data())
        };
        let mut logs = vec![
            (pool, fill.encode_log_data()),
            transfer(handler, EXECUTOR, output),
        ];
        if shielded {
            // The shield moves the net amount to Railgun and the fee to its treasury.
            let fee = U256::from(25);
            logs.extend([
                transfer(EXECUTOR, railgun, output - fee),
                transfer(EXECUTOR, Address::repeat_byte(0xfe), fee),
            ]);
        }
        let mut chain = self.delivery_chain.lock().unwrap();
        let number = chain.head + 1;
        chain.add_addressed_transaction(number, pool, Bytes::new(), logs);
        chain.head = number + 1;
        (chain.block(number), chain.transactions.last().unwrap().0)
    }

    /// One routine tracking step of the swap on `owner`, the destination chain's.
    async fn track(
        &self,
        owner: &ExecutorOwner,
        across: &AcrossClient,
        orderbook: Option<&CowOrderbookClient>,
    ) -> eyre::Result<PublicSwapProgress> {
        owner
            .track_public_swap(
                self.operation,
                self.id,
                PublicSwapTracking {
                    origin: &self.origin,
                    across,
                    orderbook,
                    explicit: false,
                },
            )
            .await
    }

    /// The swaps `owner` lists for tracking on the destination account.
    fn to_track(&self, owner: &ExecutorOwner) -> Vec<SwapUseId> {
        owner
            .records()
            .unwrap()
            .iter()
            .find(|record| record.operation() == self.operation)
            .unwrap()
            .public_swaps_to_track()
            .map(|(id, _)| id)
            .collect()
    }

    /// No inclusion was looked up by a transaction's hash, then the fixture is torn down.
    async fn finish(self, owner: ExecutorOwner) {
        self.finish_after_lookups(owner, &[]).await;
    }

    /// [`Self::finish`] for a swap whose refund was verified: Across names the transactions
    /// `refunds`, which are its own and are looked up for their blocks.
    async fn finish_after_lookups(self, owner: ExecutorOwner, refunds: &[B256]) {
        assert!(
            self.chain
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|request| matches!(
                    request["method"].as_str().unwrap(),
                    "eth_getTransactionReceipt" | "eth_getTransactionByHash"
                ))
                .all(|request| {
                    refunds.contains(&serde_json::from_value(request["params"][0].clone()).unwrap())
                }),
            "a Public account's transaction is never looked up by its hash"
        );
        for server in &self.servers {
            server.abort();
        }
        // The broadcast hook holds a store of its own.
        self.chain.lock().unwrap().on_broadcast = Box::new(|_| {});
        owner.shutdown().await;
        drop(owner);
        let Self {
            root,
            db,
            vault,
            view,
            store,
            ..
        } = self;
        drop(store);
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

// An ERC-20 deposit from an account without an allowance sends the exact approval, then the
// deposit. The path and its terms are in the record before the deposit is broadcast, each
// transaction is in it before its own broadcast, and the hand-off is read from the deposit's
// block once that block is final.
#[tokio::test]
async fn a_public_swap_approves_then_deposits_with_its_path_recorded_first() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::ONE).await;

    // A short allowance that isn't zero takes a reset and an approval. The review covered one
    // approval, so nothing is sent.
    let error = payment.approve(&owner).await.unwrap_err();
    assert!(error.to_string().contains("review the swap again"));
    assert!(payment.chain.lock().unwrap().broadcasts.is_empty());
    assert!(payment.saved().swap.transactions().is_empty());

    payment.chain.lock().unwrap().allowance = U256::ZERO;
    payment.approve(&owner).await.unwrap();
    let PublicSwapTransactionOutcome::Included {
        block_number,
        transaction_hash,
    } = payment.deposit(&owner).await.unwrap()
    else {
        panic!("the deposit succeeds");
    };
    assert_eq!(block_number, ORIGIN_HEAD + 2);

    // The approval is the exact one to the pinned pool, and the deposit carries no value.
    let broadcasts = payment.chain.lock().unwrap().broadcasts.clone();
    let [(approval_hash, approval), (deposit_hash, deposit)] = broadcasts.as_slice() else {
        panic!("an approval and a deposit were broadcast");
    };
    assert_eq!(
        (approval.to(), approval.nonce(), &approval.input()[..]),
        (
            Some(DESTINATION_TOKEN),
            0,
            build_approve_calldata(payment.spoke_pool, payment.sell_amount).as_slice()
        )
    );
    assert_eq!(
        (deposit.to(), deposit.nonce(), deposit.value()),
        (Some(payment.spoke_pool), 1, U256::ZERO)
    );
    assert_eq!(*deposit_hash, transaction_hash);
    assert_eq!(
        payment.chain.lock().unwrap().allowance_reads,
        [(DESTINATION_TOKEN, payment.source, payment.spoke_pool); 2]
    );

    // The record holds the path, the terms and both transactions with their inclusions.
    let recorded = payment.saved().swap;
    assert_eq!(recorded.path(), Some(&PublicSwapPath::Deposit));
    assert_eq!(recorded.bridge(), Some(&payment.terms));
    let [approved, deposited] = recorded.transactions() else {
        panic!("both transactions are recorded");
    };
    for (transaction, kind, hash, head) in [
        (
            approved,
            PublicSwapTransactionKind::Approval,
            approval_hash,
            ORIGIN_HEAD,
        ),
        (
            deposited,
            PublicSwapTransactionKind::Deposit,
            deposit_hash,
            ORIGIN_HEAD + 1,
        ),
    ] {
        assert_eq!((transaction.kind, transaction.hash), (kind, *hash));
        assert_eq!(transaction.submitted_from_block, Some(head));
        let inclusion = transaction.inclusion.unwrap();
        assert!(inclusion.succeeded);
        assert_eq!(
            (
                inclusion.observation.block.number,
                inclusion.observation.transaction_hash
            ),
            (head + 1, Some(*hash))
        );
    }

    // Each broadcast found its own transaction recorded and not yet included, and the
    // deposit's found the path and the terms.
    {
        let at_broadcast = payment.at_broadcast.lock().unwrap();
        let [(_, at_approval), (_, at_deposit)] = at_broadcast.as_slice() else {
            panic!("two broadcasts arrived");
        };
        assert_eq!(at_approval.path(), None);
        assert!(matches!(
            at_approval.transactions(),
            [transaction] if transaction.hash == *approval_hash && transaction.inclusion.is_none()
        ));
        assert_eq!(at_deposit.path(), Some(&PublicSwapPath::Deposit));
        assert_eq!(at_deposit.bridge(), Some(&payment.terms));
        assert!(matches!(
            at_deposit.transactions(),
            [_, transaction] if transaction.hash == *deposit_hash && transaction.inclusion.is_none()
        ));
    }

    // The hand-off waits for the deposit's block to be final, and is then read from that
    // block's receipts without a log query.
    let observe =
        || owner.observe_public_swap_handoff(payment.operation, payment.id, &payment.origin);
    assert_eq!(observe().await.unwrap(), None);
    payment.mine();
    let handoff = observe().await.unwrap().unwrap();
    assert_eq!(handoff.deposit_id, Some(U256::from(DEPOSIT_ID)));
    assert_eq!(
        (
            handoff.observation.block.number,
            handoff.observation.transaction_hash
        ),
        (block_number, Some(transaction_hash))
    );
    let observations = payment.saved().swap.observations();
    assert_eq!(observations.bridge_handoff, Some(handoff));
    assert_eq!(
        observations.deposited,
        Some(PublicSwapDeposited {
            input_amount: payment.sell_amount,
            output_amount: U256::from(1_000),
        })
    );
    assert!(
        !payment
            .chain
            .lock()
            .unwrap()
            .methods()
            .any(|method| method == "eth_getLogs")
    );
    payment.finish(owner).await;
}

// The native asset is deposited as the transaction's value: one transaction, no approval and
// no allowance read.
#[tokio::test]
async fn a_native_public_swap_deposits_its_amount_as_value_without_an_approval() {
    let (payment, owner) = PublicPayment::start(Address::ZERO, U256::ZERO).await;
    payment.approve(&owner).await.unwrap();
    assert!(payment.chain.lock().unwrap().broadcasts.is_empty());
    assert!(matches!(
        payment.deposit(&owner).await.unwrap(),
        PublicSwapTransactionOutcome::Included { .. }
    ));

    let broadcasts = payment.chain.lock().unwrap().broadcasts.clone();
    let [(hash, deposit)] = broadcasts.as_slice() else {
        panic!("only the deposit was broadcast");
    };
    assert_eq!(
        (deposit.to(), deposit.value()),
        (Some(payment.spoke_pool), payment.sell_amount)
    );
    assert!(payment.chain.lock().unwrap().allowance_reads.is_empty());
    let recorded = payment.saved().swap;
    assert!(matches!(
        recorded.transactions(),
        [transaction] if transaction.kind == PublicSwapTransactionKind::Deposit
            && transaction.hash == *hash
    ));
    // A repeat call must not issue a fresh nonce and spend the native amount again.
    assert!(payment.deposit(&owner).await.is_err());
    assert_eq!(payment.chain.lock().unwrap().broadcasts.len(), 1);
    payment.finish(owner).await;
}

// A deposit that reverts bridged nothing: its failed inclusion is recorded, no hand-off ever
// is, and the destination account's shield stays recorded as it was.
#[tokio::test]
async fn a_reverted_public_swap_deposit_hands_nothing_off() {
    for succeeds_after_reorg in [false, true] {
        let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
        payment.chain.lock().unwrap().deposits_revert = true;
        let before = payment.saved();
        let shield_status = before.record.payload_status(payment.shield);
        assert_eq!(before.shields, [payment.shield]);

        let PublicSwapTransactionOutcome::Reverted {
            block_number,
            transaction_hash,
        } = payment.deposit(&owner).await.unwrap()
        else {
            panic!("the deposit reverts");
        };
        let after = payment.saved();
        let [deposit] = after.swap.transactions() else {
            panic!("only the deposit was sent");
        };
        let inclusion = deposit.inclusion.unwrap();
        assert_eq!(
            (
                deposit.hash,
                inclusion.succeeded,
                inclusion.observation.block.number
            ),
            (transaction_hash, false, block_number)
        );

        assert!(!inclusion.finalized);
        assert!(!after.swap.is_finished(false, unix_now()));
        assert_eq!(after.record.public_swaps_to_track().count(), 1);
        if succeeds_after_reorg {
            let event = payment.deposited(payment.terms.input_amount, payment.terms.output_amount);
            let mut origin = payment.chain.lock().unwrap();
            let transaction = origin.broadcasts[0].1.clone();
            origin.chain.reorg(block_number);
            origin.chain.head = block_number + 2;
            origin.chain.include(
                block_number + 2,
                transaction,
                payment.source,
                true,
                vec![(payment.spoke_pool, event.encode_log_data())],
            );
        }
        // Final observation may verify failure, or discover success after provisional failure.
        payment.mine();
        let handoff = owner
            .observe_public_swap_handoff(payment.operation, payment.id, &payment.origin)
            .await
            .unwrap();
        assert_eq!(handoff.is_some(), succeeds_after_reorg);
        let observed = payment.saved();
        assert_eq!(observed.swap.observations().bridge_handoff, handoff);
        let finalized = observed.swap.transactions()[0].inclusion.unwrap();
        assert!(finalized.finalized);
        assert_eq!(finalized.succeeded, succeeds_after_reorg);
        assert_eq!(
            observed.swap.is_finished(false, unix_now()),
            !succeeds_after_reorg
        );
        if succeeds_after_reorg {
            assert_eq!(finalized.observation.block.number, block_number + 2);
            assert_eq!(
                observed.swap.observations().deposited,
                Some(PublicSwapDeposited {
                    input_amount: payment.terms.input_amount,
                    output_amount: payment.terms.output_amount,
                })
            );
        }
        assert_eq!(observed.shields, [payment.shield]);
        assert_eq!(
            observed.record.payload_status(payment.shield),
            shield_status
        );
        assert!(
            !payment
                .chain
                .lock()
                .unwrap()
                .methods()
                .any(|method| method == "eth_getLogs")
        );
        payment.finish(owner).await;
    }
}

// The wallet stops between the deposit's broadcast and its receipt. The record already holds
// the transaction, and a new owner finds the deposit by matching its hash locally in bounded
// finalized block scans without sending the hash to RPC.
#[tokio::test]
async fn a_restart_after_the_deposit_broadcast_finds_its_handoff_in_finalized_blocks() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    tokio::select! {
        outcome = payment.deposit(&owner) => {
            panic!("the wallet stops before the receipt, but the deposit returned {outcome:?}");
        }
        () = payment.broadcast.notified() => {}
    }
    owner.shutdown().await;
    drop(owner);
    let interrupted = payment.saved().swap;
    let [deposit] = interrupted.transactions() else {
        panic!("the deposit was handed off");
    };
    assert_eq!(
        (
            deposit.kind,
            deposit.inclusion,
            deposit.submitted_from_block
        ),
        (PublicSwapTransactionKind::Deposit, None, Some(ORIGIN_HEAD))
    );
    let hash = deposit.hash;

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let observe =
        || restarted.observe_public_swap_handoff(payment.operation, payment.id, &payment.origin);
    // The deposit's block isn't final yet.
    assert_eq!(observe().await.unwrap(), None);
    payment.mine();
    let handoff = observe().await.unwrap().unwrap();
    assert_eq!(handoff.deposit_id, Some(U256::from(DEPOSIT_ID)));
    assert_eq!(
        (
            handoff.observation.block.number,
            handoff.observation.transaction_hash
        ),
        (ORIGIN_HEAD + 1, Some(hash))
    );
    let found = payment.saved().swap;
    assert_eq!(found.observations().bridge_handoff, Some(handoff));
    let inclusion = found.transactions()[0].inclusion.unwrap();
    assert!(inclusion.succeeded);
    assert_eq!(inclusion.observation, handoff.observation);

    // Discovery sends block identifiers only; no transaction-hash request reveals the deposit.
    {
        let chain = payment.chain.lock().unwrap();
        assert!(!chain.methods().any(|method| matches!(
            method,
            "eth_getLogs" | "eth_getTransactionReceipt" | "eth_getTransactionByHash"
        )));
        for request in &chain.requests {
            assert!(
                !request["params"]
                    .to_string()
                    .contains(&alloy::hex::encode(hash))
            );
        }
    }
    // A hand-off that is recorded is returned without another read.
    let served = payment.chain.lock().unwrap().requests.len();
    assert_eq!(observe().await.unwrap(), Some(handoff));
    assert_eq!(payment.chain.lock().unwrap().requests.len(), served);
    payment.finish(restarted).await;
}

// An interrupted failed deposit has no event to locate it. Final-block scan progress survives
// another restart, and a failed whole-block read cannot skip its unreconciled block.
#[tokio::test]
async fn an_interrupted_failed_deposit_resumes_bounded_final_block_scans() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    tokio::select! {
        outcome = payment.deposit(&owner) => panic!("deposit returned before interruption: {outcome:?}"),
        () = payment.broadcast.notified() => {}
    }
    owner.shutdown().await;
    drop(owner);
    let failed_block = ORIGIN_HEAD + 12;
    let (hash, transaction) = payment.chain.lock().unwrap().broadcasts[0].clone();
    {
        let mut origin = payment.chain.lock().unwrap();
        origin.chain.reorg(ORIGIN_HEAD + 1);
        origin
            .chain
            .include(failed_block, transaction, payment.source, false, Vec::new());
        origin.chain.head = failed_block + 1;
        origin.requests.clear();
    }
    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    assert!(payment.deposit(&restarted).await.is_err());
    assert_eq!(payment.chain.lock().unwrap().broadcasts.len(), 1);
    assert_eq!(
        restarted
            .observe_public_swap_handoff(payment.operation, payment.id, &payment.origin)
            .await
            .unwrap(),
        None
    );
    let paged = payment.saved();
    let deposit = &paged.swap.transactions()[0];
    assert_eq!(deposit.deposit_scan_from_block, Some(ORIGIN_HEAD + 8));
    assert_eq!(deposit.inclusion, None);
    assert_eq!(paged.record.public_swaps_to_track().count(), 1);
    restarted.shutdown().await;
    drop(restarted);
    let restarted = ExecutorOwner::new(
        2,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    payment.chain.lock().unwrap().chain.receipt_error = Some(-32601);
    assert!(
        restarted
            .observe_public_swap_handoff(payment.operation, payment.id, &payment.origin)
            .await
            .is_err()
    );
    assert_eq!(
        payment.saved().swap.transactions()[0].deposit_scan_from_block,
        Some(ORIGIN_HEAD + 8)
    );
    payment.chain.lock().unwrap().chain.receipt_error = None;
    assert_eq!(
        restarted
            .observe_public_swap_handoff(payment.operation, payment.id, &payment.origin)
            .await
            .unwrap(),
        None
    );
    let failed = payment.saved();
    let inclusion = failed.swap.transactions()[0].inclusion.unwrap();
    assert_eq!(
        (
            inclusion.observation.block.number,
            inclusion.succeeded,
            inclusion.finalized
        ),
        (failed_block, false, true)
    );
    assert!(failed.swap.is_finished(false, unix_now()));
    assert_eq!(failed.record.public_swaps_to_track().count(), 0);
    assert_eq!(failed.shields, [payment.shield]);
    {
        let origin = payment.chain.lock().unwrap();
        assert!(!origin.methods().any(|method| matches!(
            method,
            "eth_getTransactionReceipt" | "eth_getTransactionByHash" | "eth_getLogs"
        )));
        for request in &origin.requests {
            assert!(
                !request["params"]
                    .to_string()
                    .contains(&alloy::hex::encode(hash))
            );
        }
    }
    payment.finish(restarted).await;
}

// The Public account signs its proxy's hook batch, then the order that carries it. Both are in
// the record before the orderbook request, which names the account as owner and the proxy as
// receiver. The batch's signature recovers the account over the digest cow-shed verifies, and
// the order's over the settlement's.
#[tokio::test]
async fn a_public_order_is_recorded_with_its_signed_batch_before_it_is_submitted() {
    let (payment, owner) = PublicPayment::start_order().await;
    let profile = payment.origin.public_swap_profile().unwrap();
    let settlement = profile.settlement();
    let (source, proxy) = (payment.source, payment.proxy());
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;

    // The batch's terms can be shown before anything is signed, for the nonce the caller drew.
    let shown = owner
        .public_swap_order_batch_terms(
            payment.operation,
            payment.id,
            &payment.origin,
            &payment.delivery,
            &payment.terms,
            payment.valid_to,
            payment.batch_nonce,
        )
        .unwrap();
    assert!(payment.saved().swap.path().is_none());

    let PublicSwapOrderOutcome::Submitted { uid } =
        payment.order(&owner, &orderbook).await.unwrap()
    else {
        panic!("the order is submitted");
    };
    let recorded = payment.saved().swap;
    let order = recorded.order().unwrap().clone();
    assert_eq!(recorded.bridge(), Some(&payment.terms));
    assert_eq!(
        (
            order.uid(),
            order.buy_token(),
            order.proxy(),
            order.valid_to(),
            order.submission_status(),
        ),
        (
            uid,
            DESTINATION_TOKEN,
            proxy,
            payment.valid_to,
            SwapSubmissionStatus::Accepted,
        )
    );

    // The request found the order recorded and still pending.
    let served = requests.lock().unwrap().clone();
    let [(at_request, body)] = served.as_slice() else {
        panic!("one order request was sent");
    };
    let at_request = at_request.as_ref().unwrap();
    assert_eq!(
        (at_request.uid(), at_request.submission_status()),
        (uid, SwapSubmissionStatus::Pending)
    );
    assert_eq!(at_request.batch(), order.batch());

    // The Public account owns a fill-or-kill sell order that pays its proxy.
    let sent = submitted_order(body);
    assert_eq!(
        body["from"].as_str().unwrap().parse::<Address>().unwrap(),
        source
    );
    assert_eq!(body["signingScheme"], "eip712");
    assert_eq!(body["quoteId"], ORDER_QUOTE_ID);
    assert_eq!(
        (sent.sellToken, sent.buyToken, sent.receiver),
        (ORDER_SELL_TOKEN, DESTINATION_TOKEN, proxy)
    );
    assert_eq!(
        (
            sent.sellAmount,
            sent.buyAmount,
            sent.validTo,
            sent.feeAmount
        ),
        (
            payment.sell_amount,
            U256::from(1_010),
            payment.valid_to,
            U256::ZERO
        )
    );
    assert_eq!(
        (sent.kind.as_str(), sent.partiallyFillable),
        (ORDER_KIND_SELL, false)
    );
    let digest = order_digest(&sent, DESTINATION_CHAIN, settlement);
    assert_eq!(order_uid(&sent, DESTINATION_CHAIN, settlement, source), uid);
    let signature: Bytes = serde_json::from_value(body["signature"].clone()).unwrap();
    let signature: [u8; 65] = signature[..].try_into().unwrap();
    assert_eq!(recover_order_signer(&signature, &digest).unwrap(), source);
    assert_eq!(order.submission().signature(), &signature);

    // The app data holds one hook: the batch, run through the factory for the account.
    let document = body["appData"].as_str().unwrap();
    assert_eq!(keccak256(document), sent.appData);
    let app_data: AppData = serde_json::from_str(document).unwrap();
    assert!(app_data.metadata.hooks.pre.is_empty());
    let [hook] = app_data.metadata.hooks.post.as_slice() else {
        panic!("the order has one post-hook");
    };
    assert_eq!(
        (hook.target, hook.gas_limit),
        (profile.cow_shed_factory(), ORDER_HOOK_GAS_LIMIT)
    );
    assert_eq!(&hook.call_data, order.batch().calldata());
    let call = COWShedFactory::executeHooksCall::abi_decode(&hook.call_data).unwrap();
    assert_eq!(
        (call.user, call.nonce, call.deadline),
        (source, order.batch().nonce(), U256::from(payment.valid_to))
    );
    assert_eq!(order.batch().deadline(), payment.valid_to);
    let batch = ExecuteHooks {
        calls: call.calls.clone(),
        nonce: call.nonce,
        deadline: call.deadline,
    };
    assert!(matches!(call.signature[64], 27 | 28));
    assert_eq!(
        alloy::primitives::Signature::from_raw(&call.signature)
            .unwrap()
            .recover_address_from_prehash(&execute_hooks_digest(&batch, DESTINATION_CHAIN, proxy))
            .unwrap(),
        source
    );

    // The batch deposits the proxy's balance for the approved terms and the signed delivery.
    let signed = decode_deposit_hook_calls(&call.calls).unwrap();
    let deposit = &signed.deposit;
    assert_eq!(
        (signed.weiroll, deposit.math, deposit.spoke_pool),
        (profile.weiroll(), profile.math(), payment.spoke_pool)
    );
    assert_eq!(
        (
            deposit.proxy,
            deposit.depositor,
            deposit.destination_chain_id
        ),
        (proxy, source, 1)
    );
    assert_eq!(
        (deposit.buy_amount, deposit.destination_min),
        (U256::from(1_010), U256::from(1_000))
    );
    assert_eq!(
        (deposit.input_token, deposit.output_token),
        (DESTINATION_TOKEN, USDC)
    );
    assert_eq!(deposit.delivery, payment.delivery);
    // The batch the orderbook received is the one whose terms were shown for that nonce.
    assert_eq!(call.nonce, payment.batch_nonce);
    assert_eq!(shown, public_swap_batch_terms(&batch, deposit));

    // Only the proxy's balance of the bought token was read, and the account sent nothing.
    {
        let chain = payment.chain.lock().unwrap();
        assert_eq!(chain.balance_reads, [(DESTINATION_TOKEN, proxy)]);
        assert!(chain.allowance_reads.is_empty());
        assert!(chain.broadcasts.is_empty());
    }

    // The same request again finds the accepted order and neither signs nor sends anything.
    assert_eq!(
        payment.order(&owner, &orderbook).await.unwrap(),
        PublicSwapOrderOutcome::Submitted { uid }
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(payment.saved().swap.order(), Some(&order));

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The orderbook accepts the order, but its answer is lost and the wallet stops. A new owner
// resends the recorded order: the same body with the same signatures, and nothing is signed.
// The delivery was signed for a buy amount raised within the approval's cushion, which the
// order and its batch carry and the resent order is rebuilt with.
#[tokio::test]
async fn a_restart_before_the_orderbook_answers_resends_the_recorded_order() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(true).await;
    let raised = AcrossOrderTerms {
        input_amount: U256::from(1_013),
        ..payment.terms
    };

    assert!(
        payment
            .order_for(&owner, &orderbook, &payment.delivery, &raised)
            .await
            .is_err()
    );
    let interrupted = payment.saved().swap.order().unwrap().clone();
    assert_eq!(
        interrupted.submission_status(),
        SwapSubmissionStatus::Pending
    );
    owner.shutdown().await;
    drop(owner);

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let uid = restarted
        .resubmit_public_swap_order(payment.operation, payment.id, &payment.origin, &orderbook)
        .await
        .unwrap();
    assert_eq!(uid, interrupted.uid());

    let served = requests.lock().unwrap().clone();
    let [(_, first), (at_resend, resent)] = served.as_slice() else {
        panic!("the order was sent twice");
    };
    assert_eq!(first.to_string(), resent.to_string());
    assert_eq!(at_resend.as_ref(), Some(&interrupted));
    // The order buys the raised amount, and its batch guards and scales by it for the approved
    // destination minimum.
    assert_eq!(submitted_order(resent).buyAmount, raised.input_amount);
    let call =
        COWShedFactory::executeHooksCall::abi_decode(interrupted.batch().calldata()).unwrap();
    let deposit = decode_deposit_hook_calls(&call.calls).unwrap().deposit;
    assert_eq!(
        (deposit.buy_amount, deposit.destination_min),
        (raised.input_amount, U256::from(1_000))
    );
    assert_eq!(payment.saved().swap.bridge(), Some(&raised));
    // The duplicate answer is the acceptance, and the record's batch and signature are the
    // ones signed before the restart.
    let accepted = payment.saved().swap.order().unwrap().clone();
    assert_eq!(accepted.submission_status(), SwapSubmissionStatus::Accepted);
    assert_eq!(
        (accepted.uid(), accepted.batch(), accepted.submission()),
        (
            interrupted.uid(),
            interrupted.batch(),
            interrupted.submission()
        )
    );
    // An accepted order is not sent again.
    assert_eq!(
        restarted
            .resubmit_public_swap_order(payment.operation, payment.id, &payment.origin, &orderbook)
            .await
            .unwrap(),
        uid
    );
    assert_eq!(requests.lock().unwrap().len(), 2);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(restarted).await;
}

// A balance the proxy already holds would be deposited with the order's proceeds, so nothing
// is signed while it is there, or while it can't be read.
#[tokio::test]
async fn a_public_order_is_not_signed_while_its_proxy_holds_the_bought_token() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;
    let proxy = payment.proxy();

    for balance in [Some(U256::from(5)), None] {
        payment.chain.lock().unwrap().balance = balance;
        assert_eq!(
            payment.order(&owner, &orderbook).await.unwrap(),
            PublicSwapOrderOutcome::ProxyHoldsBoughtToken { proxy, balance }
        );
    }
    let recorded = payment.saved().swap;
    assert!(recorded.path().is_none());
    assert!(recorded.bridge().is_none());
    assert!(requests.lock().unwrap().is_empty());

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// A delivery to another account than the approved one, or terms for an amount below the
// approved buy amount, beyond its cushion or below the approved destination minimum, are
// refused before the proxy is read and before anything is signed.
#[tokio::test]
async fn a_public_order_for_other_terms_than_approved_is_refused_unsigned() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;

    let other_account = AcrossPrivateDelivery {
        destination_executor: Address::repeat_byte(0x99),
        ..payment.delivery.clone()
    };
    let [lower_amount, beyond_cushion] = [1_009, 1_031].map(|amount| AcrossOrderTerms {
        input_amount: U256::from(amount),
        ..payment.terms
    });
    let short = AcrossOrderTerms {
        output_amount: U256::from(999),
        ..payment.terms
    };
    for (delivery, terms) in [
        (&other_account, &payment.terms),
        (&payment.delivery, &lower_amount),
        (&payment.delivery, &beyond_cushion),
        (&payment.delivery, &short),
    ] {
        let error = payment
            .order_for(&owner, &orderbook, delivery, terms)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("review the swap again"));
    }
    assert!(payment.saved().swap.path().is_none());
    assert!(payment.chain.lock().unwrap().balance_reads.is_empty());
    assert!(requests.lock().unwrap().is_empty());

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// An order's post-hook calls the math contract. Where its address has no code, or other code,
// the hook would fail after the trade and strand the proceeds in the proxy, so an order is
// neither reviewed nor signed there. Once the pinned code is at the address the same order is
// placed.
#[tokio::test]
async fn an_order_is_refused_until_the_math_contract_is_deployed_on_its_chain() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(false).await;
    let (across, _, _, across_task) = across_stub().await;
    let registry = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let destination = crate::bridge::PublicBridgeDestination {
        destination: crate::bridge::BridgeDestination {
            destination_token: USDC,
            intermediate: DESTINATION_TOKEN,
            symbol: "USDC".into(),
            same_asset: true,
            near: None,
        },
        path: crate::bridge::PublicBridgePath::Order,
    };
    let refusal = crate::PublicSwapUnavailable::OrdersNotEnabled {
        chain_id: DESTINATION_CHAIN,
    };

    for code in [Bytes::new(), Bytes::from_static(&[0x60, 0x80])] {
        payment.chain.lock().unwrap().math_code = code;
        assert!(
            !owner
                .public_swap_orders_available(&payment.origin)
                .await
                .unwrap()
        );
        let error = owner
            .review_public_swap(crate::PublicSwapReviewRequest {
                origin: &payment.origin,
                source: payment.source,
                sell: crate::bridge::PublicSellAsset::Erc20(ORDER_SELL_TOKEN),
                sell_amount: payment.sell_amount,
                destination: &destination,
                slippage_bps: 50,
                gas_share_bps: 5_000,
                on_shield_failure: BridgeShieldFailure::KeepOnDestination,
                orderbook: Some(&orderbook),
                across: &across,
                anchor_cache: None,
                token_registry: &registry,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 1,
            })
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref(), Some(&refusal));
        assert_eq!(
            error.to_string(),
            "Swaps aren't available from this network yet. Only tokens that bridge directly can be sent."
        );
        let error = payment.order(&owner, &orderbook).await.unwrap_err();
        assert_eq!(error.downcast_ref(), Some(&refusal));
    }
    // Nothing was read of the proxy, signed, recorded or sent.
    assert!(payment.saved().swap.path().is_none());
    assert!(payment.chain.lock().unwrap().balance_reads.is_empty());
    assert!(requests.lock().unwrap().is_empty());

    payment.chain.lock().unwrap().math_code = swap_math_runtime_code();
    assert!(
        owner
            .public_swap_orders_available(&payment.origin)
            .await
            .unwrap()
    );
    assert!(matches!(
        payment.order(&owner, &orderbook).await.unwrap(),
        PublicSwapOrderOutcome::Submitted { .. }
    ));
    assert_eq!(requests.lock().unwrap().len(), 1);

    // A chain without a Public swap profile takes no orders, whatever code it has.
    let mut unprofiled = payment.origin.clone();
    unprofiled.enabled = false;
    assert!(
        !owner
            .public_swap_orders_available(&unprofiled)
            .await
            .unwrap()
    );

    for task in [orderbook_task, across_task] {
        task.abort();
        let _ = task.await;
    }
    payment.finish(owner).await;
}

// A direct deposit runs no hook, so it never reads the math contract and goes through on a
// chain that doesn't have it.
#[tokio::test]
async fn a_direct_deposit_needs_no_math_contract() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::ZERO).await;
    payment.chain.lock().unwrap().math_code = Bytes::new();

    payment.approve(&owner).await.unwrap();
    assert!(matches!(
        payment.deposit(&owner).await.unwrap(),
        PublicSwapTransactionOutcome::Included { .. }
    ));
    assert!(
        !payment
            .chain
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|request| {
                request["method"] == "eth_getCode"
                    && serde_json::from_value(request["params"][0].clone()).ok()
                        == Some(SWAP_MATH_ADDRESS)
            })
    );

    payment.finish(owner).await;
}

/// The gas limit the tests review an invalidation with.
const INVALIDATION_GAS_LIMIT: u64 = 100_000;

// A settlement that pays more than the order's buy amount and whose hook deposited it: once
// its block is final the trade, its amounts and the hand-off are recorded together, with the
// amounts the deposit's event carries.
#[tokio::test]
async fn a_settled_public_order_whose_hook_deposited_is_bridged() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Open));
    // Without a reported trade there is nothing to read.
    assert_eq!(
        payment.observe(&owner, &orderbook).await.observations(),
        PublicSwapObservations::default()
    );

    let (paid, output) = (U256::from(1_020), U256::from(1_009));
    let block = payment.settle(uid, paid, Some(&payment.deposited(paid, output)));
    // The settlement's block isn't final yet.
    assert_eq!(
        payment
            .observe(&owner, &orderbook)
            .await
            .observations()
            .traded,
        None
    );
    payment.mine();
    let observed = payment.observe(&owner, &orderbook).await.observations();

    let traded = observed.traded.unwrap();
    assert_eq!(traded.block.number, block);
    let amounts = observed.trade_amounts.unwrap();
    assert_eq!(
        (amounts.sell_amount, amounts.buy_amount),
        (payment.sell_amount, paid)
    );
    assert_eq!(
        observed.bridge_handoff,
        Some(SwapBridgeHandoff {
            observation: traded,
            deposit_id: Some(U256::from(DEPOSIT_ID)),
        })
    );
    // Both are above the signed minimums of 1,010 and 1,000.
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: paid,
            output_amount: output,
        })
    );
    assert_eq!(observed.held_by_proxy, None);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));
    assert_eq!(payment.log_queries(), 0);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// A deposit in the settlement's receipt that puts out less than the approved destination
// minimum is not the swap's hand-off: the proxy holds the proceeds.
#[tokio::test]
async fn a_deposit_below_the_approved_minimum_is_not_the_public_orders_handoff() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    let paid = U256::from(1_020);
    payment.settle(uid, paid, Some(&payment.deposited(paid, U256::from(999))));
    payment.mine();
    let observed = payment.observe(&owner, &orderbook).await.observations();
    assert_eq!(observed.bridge_handoff, None);
    assert_eq!(observed.deposited, None);
    assert_eq!(
        observed.held_by_proxy,
        Some(PublicSwapProxyHolding {
            observation: observed.traded.unwrap(),
            amount: paid,
        })
    );

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// A settlement whose hook didn't deposit leaves the payout with the proxy. The batch can still
// run until its deadline, so no deposit is looked for before the first block past it is final.
// Then one query for the Public account's deposits finds none, the deposit is ruled out, and
// nothing is asked again.
#[tokio::test]
async fn proceeds_a_public_orders_hook_left_are_held_until_the_batch_deadline_rules_a_deposit_out()
{
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    let paid = U256::from(1_020);
    let block = payment.settle(uid, paid, None);
    payment.mine();
    let observed = payment.observe(&owner, &orderbook).await.observations();
    let traded = observed.traded.unwrap();
    assert_eq!(traded.block.number, block);
    assert_eq!(
        observed.held_by_proxy,
        Some(PublicSwapProxyHolding {
            observation: traded,
            amount: paid,
        })
    );
    assert_eq!(
        (observed.bridge_handoff, observed.deposit_ruled_out),
        (None, None)
    );
    let live = PublicSwapOrderState::HeldByProxy {
        amount: paid,
        batch_live: true,
    };
    assert_eq!(payment.state(), Some(live));
    // More final blocks before the deadline change nothing and ask for no logs.
    payment.mine();
    payment.observe(&owner, &orderbook).await;
    assert_eq!(payment.state(), Some(live));
    assert_eq!(payment.log_queries(), 0);

    // The next block is the first past the deadline. Nothing is concluded until it is final.
    let deadline_block = {
        let mut origin = payment.chain.lock().unwrap();
        let number = origin.chain.head + 1;
        origin.chain.head = number;
        origin.late_from = Some((number, u64::from(payment.valid_to) + 1));
        number
    };
    payment.observe(&owner, &orderbook).await;
    assert_eq!(payment.state(), Some(live));
    assert_eq!(payment.log_queries(), 0);

    payment.mine();
    let ruled_out = payment
        .observe(&owner, &orderbook)
        .await
        .observations()
        .deposit_ruled_out
        .unwrap();
    assert_eq!(
        (ruled_out.block.number, ruled_out.transaction_hash),
        (deadline_block, None)
    );
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::HeldByProxy {
            amount: paid,
            batch_live: false,
        })
    );
    assert_eq!(payment.log_queries(), 1);

    // The query ran once: later calls ask the chain for no logs.
    payment.mine();
    payment.observe(&owner, &orderbook).await;
    assert_eq!(payment.log_queries(), 1);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// Someone runs the batch in a later transaction before its deadline. Once the first block past
// the deadline is final, one query for the Public account's deposits finds it and the swap is
// bridged. The proxy's balance is never read to decide that, whatever it still holds.
#[tokio::test]
async fn a_late_run_of_a_public_orders_batch_is_found_at_its_deadline() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    let paid = U256::from(1_020);
    payment.settle(uid, paid, None);
    payment.mine();
    let held = payment
        .observe(&owner, &orderbook)
        .await
        .observations()
        .held_by_proxy;
    assert!(held.is_some());

    // The late run deposits what the proxy held, and a stray unit stays behind.
    let factory = payment
        .origin
        .public_swap_profile()
        .unwrap()
        .cow_shed_factory();
    let output = U256::from(1_009);
    let deposit_block = payment.include(
        factory,
        vec![(
            payment.spoke_pool,
            payment.deposited(paid, output).encode_log_data(),
        )],
    );
    let balance_reads = {
        let mut origin = payment.chain.lock().unwrap();
        origin.balance = Some(U256::ONE);
        let number = origin.chain.head + 1;
        origin.chain.head = number + 1;
        origin.late_from = Some((number, u64::from(payment.valid_to) + 1));
        origin.balance_reads.len()
    };

    let observed = payment.observe(&owner, &orderbook).await.observations();
    let handoff = observed.bridge_handoff.unwrap();
    assert_eq!(handoff.deposit_id, Some(U256::from(DEPOSIT_ID)));
    assert_eq!(handoff.observation.block.number, deposit_block);
    assert_eq!(
        observed.deposited,
        Some(PublicSwapDeposited {
            input_amount: paid,
            output_amount: output,
        })
    );
    // The payout stays recorded as history, and no deposit was ruled out.
    assert_eq!(observed.held_by_proxy, held);
    assert_eq!(observed.deposit_ruled_out, None);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));
    assert_eq!(payment.log_queries(), 1);
    assert_eq!(
        payment.chain.lock().unwrap().balance_reads.len(),
        balance_reads,
        "the proxy's balance decides nothing"
    );

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The Public account withdraws proceeds its proxy holds: one transaction to the factory, which
// deploys the proxy, carrying a batch of its own that pays only the account. It is recorded
// before its broadcast, and the withdrawal counts once its block is final. Tracking continues
// until the final query at the hook batch's deadline rules out a deposit.
#[tokio::test]
async fn held_public_order_proceeds_are_withdrawn_to_the_public_account_through_the_factory() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;
    let profile = payment.origin.public_swap_profile().unwrap();
    let (proxy, factory) = (payment.proxy(), profile.cow_shed_factory());
    let order = payment.saved().swap.order().unwrap().clone();

    // Nothing is withdrawn from a proxy that holds no proceeds of the swap.
    assert!(
        owner
            .review_public_swap_withdrawal(payment.operation, payment.id, &payment.origin, 100, 1)
            .await
            .is_err()
    );
    let paid = U256::from(1_020);
    payment.settle(uid, paid, None);
    payment.mine();
    assert!(
        payment
            .observe(&owner, &orderbook)
            .await
            .proxy_holds_proceeds()
    );

    // Stopping the swap ends its approvals, and leaves the proceeds the user's to withdraw.
    payment.store.stop_swap_use(payment.operation).unwrap();
    assert!(
        payment
            .saved()
            .record
            .swap_use(payment.id)
            .unwrap()
            .is_stopped()
    );
    let refused = payment.approve(&owner).await.unwrap_err();
    assert!(refused.to_string().contains("stopped"));

    // The hook's factory call rolled back, so the proxy has no code yet.
    {
        let mut origin = payment.chain.lock().unwrap();
        origin.balance = Some(paid);
        origin.codeless.push(proxy);
        origin.receipt_logs = vec![(
            DESTINATION_TOKEN,
            Transfer {
                from: proxy,
                to: payment.source,
                value: paid,
            }
            .encode_log_data(),
        )];
    }
    let review = owner
        .review_public_swap_withdrawal(payment.operation, payment.id, &payment.origin, 100, 1)
        .await
        .unwrap();
    let gas_limit =
        PUBLIC_PROXY_DEPLOYING_WITHDRAWAL_GAS_UNITS + payment.origin.gas.gas_limit_buffer;
    assert_eq!(
        (review.proxy, review.token, review.amount),
        (proxy, DESTINATION_TOKEN, paid)
    );
    assert_eq!(
        (review.proxy_deployed, review.gas_limit, review.max_gas_cost),
        (false, gas_limit, U256::from(gas_limit) * U256::from(100))
    );

    let PublicSwapTransactionOutcome::Included {
        block_number,
        transaction_hash,
    } = owner
        .submit_public_swap_withdrawal_with_signer(
            payment.operation,
            payment.id,
            &payment.origin,
            &payment.signer,
            REVIEWED_FEE,
            &review,
            false,
            &mut |_: PublicActionProgressUpdate| {},
        )
        .await
        .unwrap()
    else {
        panic!("the withdrawal succeeds");
    };

    // The one transaction goes to the factory, and its batch is the account's own: the bought
    // token's transfer of the reviewed amount to the account, signed by it for its proxy.
    let broadcasts = payment.chain.lock().unwrap().broadcasts.clone();
    let [(hash, sent)] = broadcasts.as_slice() else {
        panic!("only the withdrawal was broadcast");
    };
    assert_eq!((*hash, sent.to()), (transaction_hash, Some(factory)));
    let call = COWShedFactory::executeHooksCall::abi_decode(sent.input()).unwrap();
    assert_eq!(call.user, payment.source);
    assert_eq!(
        decode_withdrawal_calls(&call.calls, payment.source).unwrap(),
        (DESTINATION_TOKEN, paid)
    );
    let batch = ExecuteHooks {
        calls: call.calls.clone(),
        nonce: call.nonce,
        deadline: call.deadline,
    };
    assert_eq!(
        alloy::primitives::Signature::from_raw(&call.signature)
            .unwrap()
            .recover_address_from_prehash(&execute_hooks_digest(&batch, DESTINATION_CHAIN, proxy))
            .unwrap(),
        payment.source
    );

    // It was recorded as a withdrawal from the account before it was broadcast, and the
    // order's own batch is as it was signed.
    {
        let at_broadcast = payment.at_broadcast.lock().unwrap();
        let [(_, at_withdrawal)] = at_broadcast.as_slice() else {
            panic!("one broadcast arrived");
        };
        assert!(matches!(
            at_withdrawal.transactions(),
            [transaction] if transaction.kind == PublicSwapTransactionKind::Withdrawal
                && transaction.hash == transaction_hash
                && transaction.transaction.from == Some(payment.source)
                && transaction.inclusion.is_none()
        ));
    }
    let sent_swap = payment.saved().swap;
    assert_ne!(call.nonce, order.batch().nonce());
    assert_eq!(sent_swap.order(), Some(&order));
    let inclusion = sent_swap.transactions()[0].inclusion.unwrap();
    assert!(inclusion.succeeded);
    assert!(!inclusion.finalized);
    assert_eq!(inclusion.observation.block.number, block_number);

    // Its block isn't final yet, so the proxy still counts as holding the proceeds.
    assert_eq!(sent_swap.observations().withdrawn, None);
    assert!(
        payment
            .observe(&owner, &orderbook)
            .await
            .proxy_holds_proceeds()
    );
    let competing = || {
        let id = SwapUseId::random().unwrap();
        let operation = ExecutorOperationId::random().unwrap();
        let mut approval = sent_swap.approval().clone();
        approval.sell_token = Address::repeat_byte(0x5f);
        approval.destination = SwapApprovedAccount {
            address: None,
            setup: true,
        };
        crate::vault::PublicSwapClaim {
            id,
            origin_chain: DESTINATION_CHAIN,
            source: payment.source,
            source_scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: TEST_WALLET_ID.to_owned(),
            },
            account: SwapAccountChoice::New(operation),
            delegate: payment
                .destination
                .accepted_executor_profile()
                .unwrap()
                .delegate(),
            destination_token: USDC,
            bridged_token: DESTINATION_TOKEN,
            order: true,
            approval,
            now: u64::from(order.batch().deadline()) + 1,
        }
    };
    assert!(matches!(
        payment.store.claim_public_swap(competing()),
        Err(ExecutorStoreError::PublicSwapBuysSameToken { swap, .. }) if swap == payment.id
    ));

    // The saved inclusion disappears, and the same signed transaction is included in a later
    // block. The old block becomes final without the transaction, so its receipt is only a hint.
    let re_included_at = {
        let mut origin = payment.chain.lock().unwrap();
        origin.chain.reorg(block_number);
        let number = block_number + 2;
        origin.chain.head = number;
        origin.chain.include(
            number,
            sent.clone(),
            payment.source,
            true,
            vec![(
                DESTINATION_TOKEN,
                Transfer {
                    from: proxy,
                    to: payment.source,
                    value: paid,
                }
                .encode_log_data(),
            )],
        );
        number
    };
    let reorganized = payment.observe(&owner, &orderbook).await;
    assert_eq!(reorganized.observations().withdrawn, None);
    assert!(reorganized.proxy_holds_proceeds());
    assert!(matches!(
        payment.store.claim_public_swap(competing()),
        Err(ExecutorStoreError::PublicSwapBuysSameToken { .. })
    ));
    payment.mine();
    let withdrawn = payment.observe(&owner, &orderbook).await;
    let confirmed = withdrawn.observations().withdrawn.unwrap();
    assert_eq!(
        (confirmed.block.number, confirmed.transaction_hash),
        (re_included_at, Some(transaction_hash))
    );
    assert_ne!(confirmed.block, inclusion.observation.block);
    assert_eq!(
        withdrawn.transactions()[0].inclusion,
        Some(PublicSwapInclusion {
            observation: confirmed,
            succeeded: true,
            finalized: true,
        })
    );
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::SwappedNotBridged)
    );
    assert!(!withdrawn.proxy_holds_proceeds());
    assert_eq!(withdrawn.observations().deposit_ruled_out, None);
    assert!(!withdrawn.is_finished(true, u64::from(payment.valid_to) + 1));
    assert_eq!(payment.to_track(&owner), [payment.id]);
    let withdrawal_queries = payment.log_queries();
    assert!(withdrawal_queries > 0);
    // Withdrawn proceeds are not withdrawn again.
    assert!(
        owner
            .review_public_swap_withdrawal(payment.operation, payment.id, &payment.origin, 100, 1)
            .await
            .is_err()
    );

    // The withdrawal doesn't rule out an earlier late run of the batch. Wait for the first
    // block past its deadline to become final before concluding that no deposit was made.
    {
        let mut origin = payment.chain.lock().unwrap();
        let number = origin.chain.head + 1;
        origin.chain.head = number;
        origin.late_from = Some((number, u64::from(payment.valid_to) + 1));
    }
    payment.observe(&owner, &orderbook).await;
    assert_eq!(payment.to_track(&owner), [payment.id]);
    assert_eq!(payment.log_queries(), withdrawal_queries);
    payment.mine();
    let finished = payment.observe(&owner, &orderbook).await;
    assert!(finished.observations().deposit_ruled_out.is_some());
    assert!(finished.is_finished(true, u64::from(payment.valid_to) + 1));
    assert!(payment.to_track(&owner).is_empty());
    assert_eq!(payment.log_queries(), withdrawal_queries + 1);
    payment.store.claim_public_swap(competing()).unwrap();

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

/// Invalidate the swap's order from its Public account.
async fn invalidate(
    payment: &PublicPayment,
    owner: &ExecutorOwner,
    orderbook: &CowOrderbookClient,
) -> eyre::Result<PublicSwapTransactionOutcome> {
    owner
        .submit_public_swap_invalidation_with_signer(
            payment.operation,
            payment.id,
            &payment.origin,
            &payment.signer,
            REVIEWED_FEE,
            orderbook,
            INVALIDATION_GAS_LIMIT,
            &mut |_: PublicActionProgressUpdate| {},
        )
        .await
}

// The invalidation succeeds on an order that already filled, so its inclusion decides nothing:
// the orderbook reports the trade, and the swap follows its settlement.
#[tokio::test]
async fn an_invalidation_included_after_a_fill_leaves_the_public_order_traded() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    // The order filled and its hook deposited, which the wallet hasn't read yet.
    let paid = U256::from(1_020);
    let block = payment.settle(uid, paid, Some(&payment.deposited(paid, U256::from(1_009))));
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Open));

    assert!(matches!(
        invalidate(&payment, &owner, &orderbook).await.unwrap(),
        PublicSwapTransactionOutcome::Included { .. }
    ));
    // The invalidation's block made the settlement's final, and the trade was read there.
    let observed = payment.saved().swap.observations();
    assert_eq!(observed.cancelled, None);
    assert_eq!(observed.traded.unwrap().block.number, block);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));

    // Later observations keep it so.
    payment.mine();
    assert_eq!(
        payment
            .observe(&owner, &orderbook)
            .await
            .observations()
            .cancelled,
        None
    );
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Bridged));

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// An invalidation only cancels once the settlement holds it in final canonical state. A
// provisional receipt that is reorganized out cannot release same-token selling or prevent
// another invalidation. The hook batch stays signed, so same-token buying remains blocked.
#[tokio::test]
async fn an_invalidation_without_a_trade_cancels_the_public_order_and_keeps_its_batch() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;
    let order = payment.saved().swap.order().unwrap().clone();

    // A stopped swap's open order is the one the user wants gone.
    payment.store.stop_swap_use(payment.operation).unwrap();
    assert!(
        payment
            .saved()
            .record
            .swap_use(payment.id)
            .unwrap()
            .is_stopped()
    );
    let PublicSwapTransactionOutcome::Included {
        block_number,
        transaction_hash,
    } = invalidate(&payment, &owner, &orderbook).await.unwrap()
    else {
        panic!("the invalidation succeeds");
    };

    let provisional = payment.saved().swap;
    let inclusion = provisional.transactions()[0].inclusion.unwrap();
    assert_eq!(provisional.observations().cancelled, None);
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Open));
    assert!(provisional.order_can_fill(unix_now()));

    let (competing_id, competing_operation) = (
        SwapUseId::random().unwrap(),
        ExecutorOperationId::random().unwrap(),
    );
    let competing = || {
        let mut request = claim(
            competing_id,
            competing_operation,
            DESTINATION_CHAIN,
            ORDER_SELL_TOKEN,
            USDC,
        );
        request.source = payment.source;
        request
    };
    assert!(matches!(
        owner.claim_public_swap(competing()).unwrap_err().downcast_ref(),
        Some(ExecutorStoreError::PublicSwapSellsSameToken { swap, .. }) if *swap == payment.id
    ));

    // Older records concluded cancellation directly from the sending step's receipt. Loading
    // that persisted shape discards its provisional cancellation before UI and admission use it.
    payment
        .store
        .record_public_swap_observations(
            payment.operation,
            payment.id,
            PublicSwapObservations {
                cancelled: Some(inclusion.observation),
                ..provisional.observations()
            },
        )
        .unwrap();
    let migrated = payment.saved().swap;
    assert_eq!(migrated.observations().cancelled, None);
    assert!(migrated.order_can_fill(unix_now()));
    assert_eq!(payment.state(), Some(PublicSwapOrderState::Open));
    assert!(!migrated.is_finished(true, u64::from(order.batch().deadline()) + 1));
    assert!(matches!(
        owner
            .claim_public_swap(competing())
            .unwrap_err()
            .downcast_ref(),
        Some(ExecutorStoreError::PublicSwapSellsSameToken { .. })
    ));

    // The provisional invalidation leaves the canonical chain. A final block now still reads
    // the order fill as zero, and its receipt cannot cancel the order on its own.
    {
        let mut origin = payment.chain.lock().unwrap();
        origin.chain.reorg(block_number);
        origin.chain.invalidated.clear();
    }
    payment.mine();
    assert_eq!(
        payment
            .observe(&owner, &orderbook)
            .await
            .observations()
            .cancelled,
        None
    );
    assert!(owner.claim_public_swap(competing()).is_err());

    let PublicSwapTransactionOutcome::Included {
        block_number: retry_block,
        ..
    } = invalidate(&payment, &owner, &orderbook).await.unwrap()
    else {
        panic!("the replacement invalidation succeeds");
    };
    payment
        .chain
        .lock()
        .unwrap()
        .chain
        .invalidated
        .push((uid, retry_block));
    assert_eq!(
        payment
            .observe(&owner, &orderbook)
            .await
            .observations()
            .cancelled,
        None
    );
    payment.mine();
    let cancelled = payment.observe(&owner, &orderbook).await;
    let at = cancelled.observations().cancelled.unwrap();
    assert_eq!((at.block.number, at.transaction_hash), (retry_block, None));
    assert_eq!(cancelled.observations().traded, None);
    // The admission rule reports the same time for another order that buys the token: the
    // batch's deadline plus one.
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::Cancelled {
            retry_at: u64::from(order.batch().deadline()) + 1,
        })
    );
    assert!(cancelled.batch_can_run(unix_now()));
    assert!(!cancelled.order_can_fill(unix_now()));

    // Nothing was sent to the proxy or the factory.
    let broadcasts = payment.chain.lock().unwrap().broadcasts.clone();
    let (hash, sent) = &broadcasts[0];
    assert_eq!(broadcasts.len(), 2);
    assert_eq!(
        (*hash, sent.to(), &sent.input()[..]),
        (
            transaction_hash,
            Some(payment.settlement()),
            &invalidate_order_calldata(&uid)[..]
        )
    );
    assert!(matches!(
        cancelled.transactions(),
        [first, second] if first.kind == PublicSwapTransactionKind::Invalidation
            && first.hash == transaction_hash
            && second.kind == PublicSwapTransactionKind::Invalidation
    ));
    // The recorded batch has the nonce, deadline and calldata it was signed with.
    assert_eq!(cancelled.order().unwrap().batch(), order.batch());
    // Loading a finalized state observation preserves cancellation, and same-token selling
    // is admitted again even though the old batch may still buy its bridged token.
    assert_eq!(payment.saved().swap.observations().cancelled, Some(at));
    owner.claim_public_swap(competing()).unwrap();

    // A cancelled order is not invalidated again.
    assert!(invalidate(&payment, &owner, &orderbook).await.is_err());
    assert_eq!(payment.chain.lock().unwrap().broadcasts.len(), 2);

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The order's state from hand-set observations: a hand-off wins over everything, then a
// withdrawal, then proceeds the proxy holds, and a cancellation wins over an expiry. An order
// past its `validTo` stays open until a final block shows it expired. Beside each state is
// whether tracking is finished with the swap once the batch's deadline has passed.
#[tokio::test]
async fn a_public_orders_state_follows_the_precedence_of_its_observations() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    // A swap that hasn't signed an order has no order state.
    assert_eq!(payment.state(), None);
    payment.submitted(&owner, &orderbook).await;

    let valid_to = u64::from(payment.valid_to);
    let at = SwapObservation {
        block: BlockNumHash::new(30, B256::repeat_byte(30)),
        transaction_hash: None,
    };
    let amount = U256::from(1_020);
    let expired = PublicSwapObservations {
        expired: Some(at),
        ..PublicSwapObservations::default()
    };
    let unsettled = PublicSwapObservations {
        cancelled: Some(at),
        ..expired
    };
    let held = PublicSwapObservations {
        traded: Some(at),
        held_by_proxy: Some(PublicSwapProxyHolding {
            observation: at,
            amount,
        }),
        ..unsettled
    };
    let ruled_out = PublicSwapObservations {
        deposit_ruled_out: Some(at),
        ..held
    };
    let withdrawn = PublicSwapObservations {
        withdrawn: Some(at),
        ..held
    };
    let withdrawn_without_deposit = PublicSwapObservations {
        deposit_ruled_out: Some(at),
        ..withdrawn
    };
    let bridged = PublicSwapObservations {
        bridge_handoff: Some(SwapBridgeHandoff {
            observation: at,
            deposit_id: Some(U256::from(DEPOSIT_ID)),
        }),
        ..withdrawn
    };
    for (observations, expected, finished) in [
        (
            PublicSwapObservations::default(),
            PublicSwapOrderState::Open,
            false,
        ),
        (expired, PublicSwapOrderState::Expired, true),
        (
            unsettled,
            PublicSwapOrderState::Cancelled {
                retry_at: valid_to + 1,
            },
            true,
        ),
        (
            held,
            PublicSwapOrderState::HeldByProxy {
                amount,
                batch_live: true,
            },
            false,
        ),
        (
            ruled_out,
            PublicSwapOrderState::HeldByProxy {
                amount,
                batch_live: false,
            },
            false,
        ),
        (withdrawn, PublicSwapOrderState::SwappedNotBridged, false),
        (
            withdrawn_without_deposit,
            PublicSwapOrderState::SwappedNotBridged,
            true,
        ),
        // The bridge's outcome isn't recorded yet.
        (bridged, PublicSwapOrderState::Bridged, false),
    ] {
        payment
            .store
            .record_public_swap_observations(payment.operation, payment.id, observations)
            .unwrap();
        let swap = payment.saved().swap;
        assert_eq!(
            public_swap_order_state(&swap),
            Some(expected),
            "{observations:?}"
        );
        assert_eq!(
            swap.is_finished(false, valid_to + 1),
            finished,
            "{observations:?}"
        );
        // An order that ended without a trade is tracked until its batch can no longer run.
        assert!(!swap.is_finished(false, valid_to) || observations.withdrawn.is_some());
    }
    // Admission already counts the open order as unable to fill once its `validTo` passes.
    payment
        .store
        .record_public_swap_observations(
            payment.operation,
            payment.id,
            PublicSwapObservations::default(),
        )
        .unwrap();
    assert!(!payment.saved().swap.order_can_fill(valid_to + 1));

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The settlement's receipt holds the order's trade and its hook's deposit, but no transfer to
// the proxy. The trade is the pinned settlement's, so it is recorded with what it bought as
// held by the proxy, and the deposit is left for the query at the batch's deadline.
#[tokio::test]
async fn a_public_orders_trade_without_its_payout_is_recorded_as_held_by_the_proxy() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    let paid = U256::from(1_020);
    let deposit = payment.deposited(paid, U256::from(1_009)).encode_log_data();
    let block = payment.include(
        payment.settlement(),
        vec![payment.trade(uid, paid), (payment.spoke_pool, deposit)],
    );
    *payment.trade_block.lock().unwrap() = Some(block);
    payment.mine();

    let observed = payment.observe(&owner, &orderbook).await.observations();
    let traded = observed.traded.unwrap();
    assert_eq!(traded.block.number, block);
    assert_eq!(
        observed.held_by_proxy,
        Some(PublicSwapProxyHolding {
            observation: traded,
            amount: paid,
        })
    );
    assert_eq!(observed.bridge_handoff, None);
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::HeldByProxy {
            amount: paid,
            batch_live: true,
        })
    );

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The wallet stops between a withdrawal's broadcast and its receipt, so its inclusion is never
// recorded. A new owner finds it by the bought token's transfers from the proxy to the Public
// account, verifies it in its block's receipts, and records the inclusion with the withdrawal.
#[tokio::test]
async fn a_withdrawal_whose_receipt_was_never_seen_is_found_by_its_transfer() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;
    let proxy = payment.proxy();

    let paid = U256::from(1_020);
    payment.settle(uid, paid, None);
    payment.mine();
    payment.observe(&owner, &orderbook).await;
    {
        let mut origin = payment.chain.lock().unwrap();
        origin.balance = Some(paid);
        origin.receipt_logs = vec![(
            DESTINATION_TOKEN,
            Transfer {
                from: proxy,
                to: payment.source,
                value: paid,
            }
            .encode_log_data(),
        )];
    }
    let review = owner
        .review_public_swap_withdrawal(payment.operation, payment.id, &payment.origin, 100, 1)
        .await
        .unwrap();
    let mut progress = |_: PublicActionProgressUpdate| {};
    tokio::select! {
        outcome = owner.submit_public_swap_withdrawal_with_signer(
            payment.operation,
            payment.id,
            &payment.origin,
            &payment.signer,
            REVIEWED_FEE,
            &review,
            false,
            &mut progress,
        ) => {
            panic!("the wallet stops before the receipt, but the withdrawal returned {outcome:?}");
        }
        () = payment.broadcast.notified() => {}
    }
    owner.shutdown().await;
    drop(owner);
    let included_at = payment.chain.lock().unwrap().chain.head;
    let interrupted = payment.saved().swap;
    let [withdrawal] = interrupted.transactions() else {
        panic!("the withdrawal was handed off");
    };
    assert_eq!(
        (withdrawal.kind, withdrawal.inclusion),
        (PublicSwapTransactionKind::Withdrawal, None)
    );
    let hash = withdrawal.hash;

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    // The withdrawal's block isn't final yet.
    assert!(
        payment
            .observe(&restarted, &orderbook)
            .await
            .proxy_holds_proceeds()
    );
    payment.mine();
    let found = payment.observe(&restarted, &orderbook).await;
    let withdrawn = found.observations().withdrawn.unwrap();
    assert_eq!(
        (withdrawn.block.number, withdrawn.transaction_hash),
        (included_at, Some(hash))
    );
    assert_eq!(
        found.transactions()[0].inclusion,
        Some(PublicSwapInclusion {
            observation: withdrawn,
            succeeded: true,
            finalized: true,
        })
    );
    assert_eq!(
        payment.state(),
        Some(PublicSwapOrderState::SwappedNotBridged)
    );

    // Every log query asked for the token's transfers from the proxy to the Public account,
    // and for nothing that names the transaction.
    {
        let chain = payment.chain.lock().unwrap();
        let queries: Vec<_> = chain
            .requests
            .iter()
            .filter(|request| request["method"] == "eth_getLogs")
            .map(|request| &request["params"][0])
            .collect();
        assert!(!queries.is_empty());
        for filter in queries {
            let tokens: Vec<Address> = match &filter["address"] {
                Value::Array(_) => serde_json::from_value(filter["address"].clone()).unwrap(),
                token => vec![serde_json::from_value(token.clone()).unwrap()],
            };
            assert_eq!(tokens, [DESTINATION_TOKEN]);
            let topics: Vec<Option<B256>> =
                serde_json::from_value(filter["topics"].clone()).unwrap();
            assert_eq!(
                topics[..3],
                [
                    Some(Transfer::SIGNATURE_HASH),
                    Some(proxy.into_word()),
                    Some(payment.source.into_word()),
                ]
            );
            assert!(topics[3..].iter().all(Option::is_none));
            assert!(!filter.to_string().contains(&alloy::hex::encode(hash)));
        }
    }

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(restarted).await;
}

// The wallet stops between an invalidation's broadcast and its receipt. A new owner reads the
// settlement's fill of the order at a final block: the invalidation set it to the maximum, and
// the orderbook reports no trade, so the swap is cancelled at that block.
#[tokio::test]
async fn an_invalidation_whose_receipt_was_never_seen_cancels_from_the_settlements_state() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let uid = payment.submitted(&owner, &orderbook).await;

    tokio::select! {
        outcome = invalidate(&payment, &owner, &orderbook) => {
            panic!("the wallet stops before the receipt, but the invalidation returned {outcome:?}");
        }
        () = payment.broadcast.notified() => {}
    }
    owner.shutdown().await;
    drop(owner);
    // The settlement holds the order invalidated from the block that included the transaction.
    let invalidated_at = {
        let mut origin = payment.chain.lock().unwrap();
        let number = origin.chain.head;
        origin.chain.invalidated.push((uid, number));
        number
    };
    let interrupted = payment.saved().swap;
    let [invalidation] = interrupted.transactions() else {
        panic!("the invalidation was handed off");
    };
    assert_eq!(
        (invalidation.kind, invalidation.inclusion),
        (PublicSwapTransactionKind::Invalidation, None)
    );

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    // The invalidation's block isn't final yet.
    assert_eq!(
        payment
            .observe(&restarted, &orderbook)
            .await
            .observations()
            .cancelled,
        None
    );
    payment.mine();
    let cancelled = payment
        .observe(&restarted, &orderbook)
        .await
        .observations()
        .cancelled
        .unwrap();
    assert_eq!(
        (cancelled.block.number, cancelled.transaction_hash),
        (invalidated_at, None)
    );
    assert!(matches!(
        payment.state(),
        Some(PublicSwapOrderState::Cancelled { .. })
    ));

    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(restarted).await;
}

/// Across's record of the swap's deposit: its `status`, the block of its fill and the refund
/// transaction it names.
fn across_deposit(status: &str, fill_block: Option<u64>, refund: Option<B256>) -> String {
    json!({"deposit": {
        "status": status,
        "fillBlockNumber": fill_block,
        "depositRefundTxHash": refund,
        "outputAmount": "1",
        "recipient": EXECUTOR,
        "destinationChainId": "1",
    }})
    .to_string()
}

/// The path and body of each request a bridge stub served.
type BridgeLookups = Arc<Mutex<Vec<(String, Value)>>>;

/// An Across API stub that answers every deposit lookup with what the returned reply holds,
/// at first a pending deposit. Returned with the lookups it served.
async fn across_stub() -> (
    AcrossClient,
    Arc<Mutex<String>>,
    BridgeLookups,
    tokio::task::JoinHandle<()>,
) {
    let reply = Arc::new(Mutex::new(across_deposit("pending", None, None)));
    let served = reply.clone();
    let (url, lookups, task) = spawn_bridge_stub(move |_| served.lock().unwrap().clone()).await;
    let client = AcrossClient::new(
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        ),
        url,
    )
    .unwrap();
    (client, reply, lookups, task)
}

// An order's hook deposits more than the signed minimums, and the fill repeats the deposit's
// amounts. The destination chain's owner matches the fill on those: a fill of the signed
// minimums is not this deposit's. The matching fill, the handler's transfer to the destination
// account and its shield in one final receipt deliver the swap, and the account's shield
// payload is recorded as run in the same write.
#[tokio::test]
async fn a_public_swap_is_delivered_by_the_fill_of_its_deposited_amounts_and_its_shield() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, _requests, orderbook_task) = payment.orderbook(false).await;
    let (across, reply, lookups, across_task) = across_stub().await;
    let uid = payment.submitted(&owner, &orderbook).await;
    let observe = || owner.observe_public_swap_bridge(payment.operation, payment.id, &across);

    // Without a hand-off there is nothing to ask Across about.
    assert_eq!(observe().await.unwrap(), None);
    assert!(lookups.lock().unwrap().is_empty());

    // The signed minimums are 1,010 and 1,000.
    let (input, output) = (U256::from(1_020), U256::from(1_009));
    payment.settle(uid, input, Some(&payment.deposited(input, output)));
    payment.mine();
    payment.observe(&owner, &orderbook).await;

    let (signed, _) = payment.fill(
        payment.terms.input_amount,
        payment.terms.output_amount,
        true,
    );
    *reply.lock().unwrap() = across_deposit("filled", Some(signed.number), None);
    assert_eq!(observe().await.unwrap(), None);
    assert_eq!(payment.saved().outcome, None);

    let (block, transaction_hash) = payment.fill(input, output, true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    let delivered = SwapBridgeOutcome::DeliveredVerified {
        block,
        transaction_hash,
        output_amount: output,
        shielded: true,
    };
    assert_eq!(observe().await.unwrap(), Some(delivered));
    let saved = payment.saved();
    assert_eq!(saved.swap.observations().bridge_outcome, Some(delivered));
    assert_eq!(
        saved.outcome,
        Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        })
    );
    // Across was asked about the deposit on the chain the Public account pays on.
    let asked = lookups.lock().unwrap().len();
    assert!(lookups.lock().unwrap().iter().all(|(path, _)| {
        *path == format!("/api/deposit?originChainId={DESTINATION_CHAIN}&depositId={DEPOSIT_ID}")
    }));
    // A recorded delivery isn't asked about again.
    assert_eq!(observe().await.unwrap(), Some(delivered));
    assert_eq!(lookups.lock().unwrap().len(), asked);

    across_task.abort();
    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(owner).await;
}

// The fill completed and its receipt holds no shield: the destination account holds the token,
// and its shield payload can still run.
#[tokio::test]
async fn a_public_swaps_fill_without_its_shield_is_held_on_the_destination() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    payment.handed_off(&owner).await;

    let amount = U256::from(1_000);
    let (block, transaction_hash) = payment.fill(payment.sell_amount, amount, false);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    let held = SwapBridgeOutcome::HeldOnDestination {
        block,
        transaction_hash,
        amount,
    };
    assert_eq!(
        owner
            .observe_public_swap_bridge(payment.operation, payment.id, &across)
            .await
            .unwrap(),
        Some(held)
    );
    let saved = payment.saved();
    assert_eq!(saved.swap.observations().bridge_outcome, Some(held));
    assert_eq!(
        saved.outcome,
        Some(SwapDestinationOutcome::Held {
            block,
            transaction_hash,
        })
    );

    across_task.abort();
    payment.finish(owner).await;
}

// Across reports the deposit filled, but no endpoint of the destination chain serves
// whole-block receipts. The delivery is private, so Across's word is not recorded: the swap
// stays tracked, and is delivered once the fill can be read.
#[tokio::test]
async fn a_public_swaps_fill_that_cant_be_read_is_not_recorded_on_the_word_of_across() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    payment.handed_off(&owner).await;

    let (block, transaction_hash) = payment.fill(payment.sell_amount, U256::from(1_000), true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    payment.delivery_chain.lock().unwrap().receipt_error = Some(-32601);
    assert!(
        owner
            .observe_public_swap_bridge(payment.operation, payment.id, &across)
            .await
            .is_err()
    );
    assert!(
        owner
            .check_public_swap_bridge(payment.operation, payment.id, &across)
            .await
            .is_err()
    );
    let unread = payment.saved();
    assert_eq!(unread.swap.observations().bridge_outcome, None);
    assert_eq!(unread.outcome, None);
    assert_eq!(payment.to_track(&owner), [payment.id]);

    payment.delivery_chain.lock().unwrap().receipt_error = None;
    assert_eq!(
        owner
            .observe_public_swap_bridge(payment.operation, payment.id, &across)
            .await
            .unwrap(),
        Some(SwapBridgeOutcome::DeliveredVerified {
            block,
            transaction_hash,
            output_amount: U256::from(1_000),
            shielded: true,
        })
    );

    across_task.abort();
    payment.finish(owner).await;
}

// Across reports the deposit expired: the swap is refunding and the destination account's
// shield unfilled. The refund is then verified on the chain the Public account pays on, in the
// final receipts of the block holding the transaction Across names: the deposited amount of the
// deposit's token from the pinned pool to the Public account. A verified refund is returned
// once, ends the swap's tracking, and releases nothing of the destination account.
#[tokio::test]
async fn a_refunding_public_swaps_refund_to_its_public_account_is_verified_on_the_origin_chain() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    payment.handed_off(&owner).await;
    let refund = || {
        owner.observe_public_swap_refund(payment.operation, payment.id, &payment.origin, &across)
    };

    // A swap that isn't refunding has no refund to look for.
    assert_eq!(refund().await.unwrap(), None);
    *reply.lock().unwrap() = across_deposit("expired", None, None);
    assert_eq!(
        owner
            .observe_public_swap_bridge(payment.operation, payment.id, &across)
            .await
            .unwrap(),
        Some(SwapBridgeOutcome::Refunding)
    );
    let refunding = payment.saved();
    assert_eq!(refunding.outcome, Some(SwapDestinationOutcome::Unfilled));
    assert_eq!(refunding.swap.observations().bridge_refund, None);
    // Across names no refund transaction yet.
    assert_eq!(refund().await.unwrap(), None);
    assert_eq!(payment.to_track(&owner), [payment.id]);

    // A transfer to another payee, and one of less than was deposited, are not the refund.
    let mut refunds = Vec::new();
    for (payee, value, verified) in [
        (Address::repeat_byte(0x99), payment.sell_amount, false),
        (payment.source, payment.sell_amount - U256::ONE, false),
        (payment.source, payment.sell_amount, true),
    ] {
        let number = payment.include(
            payment.spoke_pool,
            vec![(
                DESTINATION_TOKEN,
                Transfer {
                    from: payment.spoke_pool,
                    to: payee,
                    value,
                }
                .encode_log_data(),
            )],
        );
        payment.mine();
        let (block, refund_tx) = {
            let origin = payment.chain.lock().unwrap();
            (
                origin.chain.block(number),
                origin.chain.transactions.last().unwrap().0,
            )
        };
        refunds.push(refund_tx);
        *reply.lock().unwrap() = across_deposit("expired", None, Some(refund_tx));
        let expected = verified.then_some(SwapObservation {
            block,
            transaction_hash: Some(refund_tx),
        });
        assert_eq!(refund().await.unwrap(), expected, "refund to {payee}");
        assert_eq!(
            payment.saved().swap.observations().bridge_refund,
            expected,
            "refund to {payee}"
        );
    }
    // The caller refreshes the Public account's balances when a refund is returned, once.
    assert_eq!(refund().await.unwrap(), None);
    assert!(payment.to_track(&owner).is_empty());

    // The destination account's shield stays signed, and keeps the account from another swap.
    let refunded = payment.saved();
    assert_eq!(refunded.shields, [payment.shield]);
    assert_eq!(refunded.outcome, Some(SwapDestinationOutcome::Unfilled));
    assert_eq!(
        swap_account_refusal(
            &refunded.record,
            1,
            SwapAccountRole::Destination { token: USDC },
            SwapAccountUse::New,
            SwapAdmissionEvidence::Recorded,
        ),
        Some(SwapAccountRefusal::EarlierDeliveryUnresolved)
    );

    across_task.abort();
    payment.finish_after_lookups(owner, &refunds).await;
}

// Across can fill a deposit it reported expired. Routine polling stops at the refund, and an
// explicit status check asks again: the verified fill replaces the refund, and the destination
// account's shield payload is recorded as run.
#[tokio::test]
async fn an_explicit_check_replaces_a_public_swaps_refund_with_its_verified_fill() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    payment.handed_off(&owner).await;
    let observe = || owner.observe_public_swap_bridge(payment.operation, payment.id, &across);

    *reply.lock().unwrap() = across_deposit("expired", None, None);
    assert_eq!(observe().await.unwrap(), Some(SwapBridgeOutcome::Refunding));
    assert_eq!(
        payment.saved().outcome,
        Some(SwapDestinationOutcome::Unfilled)
    );

    let output = U256::from(1_000);
    let (block, transaction_hash) = payment.fill(payment.sell_amount, output, true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    assert_eq!(observe().await.unwrap(), Some(SwapBridgeOutcome::Refunding));
    let delivered = SwapBridgeOutcome::DeliveredVerified {
        block,
        transaction_hash,
        output_amount: output,
        shielded: true,
    };
    assert_eq!(
        owner
            .check_public_swap_bridge(payment.operation, payment.id, &across)
            .await
            .unwrap(),
        Some(delivered)
    );
    let saved = payment.saved();
    assert_eq!(saved.swap.observations().bridge_outcome, Some(delivered));
    assert_eq!(
        saved.outcome,
        Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        })
    );

    across_task.abort();
    payment.finish(owner).await;
}

/// A tracking step that found nothing.
const NO_PROGRESS: PublicSwapProgress = PublicSwapProgress {
    changed: false,
    refresh_public_balances: false,
    finished: false,
};

// The wallet stops after a direct deposit was included. Only the destination chain's owner is
// loaded again: no owner of the chain the Public account pays on, and no wallet session. It
// lists the swap, and its tracking steps read the hand-off, the fill and the shield, after
// which the swap is finished and no longer listed.
#[tokio::test]
async fn a_public_swap_resumes_from_its_destination_chains_owner_alone() {
    let (payment, owner) = PublicPayment::start(DESTINATION_TOKEN, U256::MAX).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    assert!(matches!(
        payment.deposit(&owner).await.unwrap(),
        PublicSwapTransactionOutcome::Included { .. }
    ));
    owner.shutdown().await;
    drop(owner);

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    assert_eq!(payment.to_track(&restarted), [payment.id]);

    // The deposit's block isn't final yet. Inspecting the preceding finalized block changes
    // only durable scan progress, so the full-record change flag is true without a hand-off.
    let before = payment.saved();
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        PublicSwapProgress {
            changed: true,
            ..NO_PROGRESS
        }
    );
    let scanned = payment.saved();
    assert_eq!(
        scanned.swap.transactions()[0].deposit_scan_from_block,
        Some(ORIGIN_HEAD + 1)
    );
    assert_eq!(scanned.swap.observations(), before.swap.observations());
    assert_eq!(scanned.shields, before.shields);
    // With no newly finalized block, another poll has no record change.
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        NO_PROGRESS
    );
    // Its hand-off is read once it is, while Across still reports the deposit pending.
    payment.mine();
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        PublicSwapProgress {
            changed: true,
            ..NO_PROGRESS
        }
    );
    assert!(payment.saved().swap.observations().bridge_handoff.is_some());
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        NO_PROGRESS
    );

    let (block, transaction_hash) = payment.fill(payment.sell_amount, U256::from(1_000), true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    let delivered = PublicSwapProgress {
        changed: true,
        refresh_public_balances: false,
        finished: true,
    };
    assert_eq!(
        payment.track(&restarted, &across, None).await.unwrap(),
        delivered
    );
    assert_eq!(
        payment.saved().outcome,
        Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        })
    );
    assert!(payment.to_track(&restarted).is_empty());

    across_task.abort();
    payment.finish(restarted).await;
}

// The wallet stops with a signed order whose submission is still pending. The destination
// chain's owner alone resends it, then reads its settlement, the hand-off in it and the fill.
#[tokio::test]
async fn a_public_order_resumes_from_its_destination_chains_owner_alone() {
    let (payment, owner) = PublicPayment::start_order().await;
    let (orderbook, requests, orderbook_task) = payment.orderbook(true).await;
    let (across, reply, _lookups, across_task) = across_stub().await;
    // The orderbook accepts the order, but its answer is lost.
    assert!(payment.order(&owner, &orderbook).await.is_err());
    owner.shutdown().await;
    drop(owner);

    let restarted = ExecutorOwner::new(
        1,
        payment.db.clone(),
        payment.view.clone(),
        payment.destination.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    assert_eq!(payment.to_track(&restarted), [payment.id]);
    // An order is tracked through its orderbook.
    assert!(payment.track(&restarted, &across, None).await.is_err());

    // The first step resends the recorded order, which the orderbook answers as a duplicate.
    assert_eq!(
        payment
            .track(&restarted, &across, Some(&orderbook))
            .await
            .unwrap(),
        PublicSwapProgress {
            changed: true,
            ..NO_PROGRESS
        }
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    let order = payment.saved().swap.order().unwrap().clone();
    assert_eq!(order.submission_status(), SwapSubmissionStatus::Accepted);

    let (input, output) = (U256::from(1_020), U256::from(1_009));
    payment.settle(order.uid(), input, Some(&payment.deposited(input, output)));
    payment.mine();
    let (block, transaction_hash) = payment.fill(input, output, true);
    *reply.lock().unwrap() = across_deposit("filled", Some(block.number), None);
    assert_eq!(
        payment
            .track(&restarted, &across, Some(&orderbook))
            .await
            .unwrap(),
        PublicSwapProgress {
            changed: true,
            refresh_public_balances: false,
            finished: true,
        }
    );
    assert_eq!(
        payment.saved().outcome,
        Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        })
    );
    assert!(payment.to_track(&restarted).is_empty());
    // An accepted order isn't sent again.
    assert_eq!(requests.lock().unwrap().len(), 2);

    across_task.abort();
    orderbook_task.abort();
    let _ = orderbook_task.await;
    payment.finish(restarted).await;
}
