#[allow(clippy::wildcard_imports)]
use super::*;

/// What a `quote-error` mode's form is opened to sell, to buy and for which amount, and the
/// failure it holds in place of a quote: a pricing failure with the values of a quote on BNB
/// Chain, whose pair has no price anchor. `None` in every other mode.
pub(in crate::root::private_swap) fn quote_error() -> Option<(Address, Address, U256, eyre::Report)>
{
    if !matches!(mode()?, Mode::QuoteError) {
        return None;
    }
    let diagnostics = SwapQuoteDiagnostics {
        chain_id: 56,
        // USDC and USDT on BNB Chain, which both have 18 decimals.
        sell_token: address!("8ac76a51cc950d9822d68b83fe1ad97b32cd580d"),
        buy_token: address!("55d398326f99059ff775485246999027b3197955"),
        sell_amount: U256::from(100_000_000_000_000_000_000_u128),
        buy_amount: U256::from(99_912_504_117_302_255_616_u128),
        sell_token_price: "0.0013443372127831241559514641181749539100565016269683837890625"
            .to_owned(),
        gas_amount: "163218".to_owned(),
        gas_price: "50000000".to_owned(),
        anchor: SwapQuoteAnchor::NotConfigured,
    };
    Some((
        WETH,
        USDC,
        U256::from(SWAP_SPEND),
        eyre::Report::from(OrderLimitError::QuotePriceOutOfRange).wrap_err(diagnostics),
    ))
}

/// The record and the review a `retry-review` mode holds its form on: the swap's expired first
/// order, and a quote for its retry that delivers 100 USDC with no continuing approval. A
/// `speed-up-review`, `speed-up:open` or `speed-up:just-placed` mode holds the swap with that
/// retry open instead, a quote that delivers 95 USDC for a lower deposit while the first
/// order's approval of 90 can still decide the delivery, and the retry's ID, the order the
/// form speeds up. `None` in every other mode.
pub(in crate::root::private_swap) fn retry_review(
    profile: &SwapProfile,
    isolation: OperationNetworkIsolation,
) -> Option<(ExecutorRecord, SwapReview, Option<OrderUid>)> {
    let speed_up = match mode()? {
        Mode::RetryReview => None,
        Mode::SpeedUp(SpeedUp::Review | SpeedUp::Open | SpeedUp::JustPlaced) => {
            Some(SpeedUp::Review)
        }
        // The retry of the swap to Solana or Zcash, quoted for 101.18 USDC, whose expected JUP
        // or ZEC 1Click values at $100.73. No anchor prices either, so the review asks for the
        // unverified-price acknowledgement.
        Mode::NonEvm(NonEvm::Review, network) => {
            let record = staged_non_evm(None, network)?;
            let review = SwapReview::for_ui_fixture(
                profile,
                &record,
                WETH,
                USDC,
                U256::from(SWAP_SPEND),
                U256::from(101_180_000_u32),
                PairAnchorRate {
                    sell_rate: U256::from(1_000_000_000_000_000_000_u64),
                    buy_rate: U256::from(2_030_000_000_u32),
                },
                120_000_000,
                &SwapDelivery::NonEvmBridge(non_evm_delivery(network)?),
                U256::from(NON_EVM_MINIMUM),
                U256::from(NON_EVM_EXPECTED),
                Some(U256::from(100_730_000_u32)),
                Vec::new(),
                isolation,
            );
            return match review {
                Ok(review) => Some((record, review, None)),
                Err(error) => {
                    tracing::warn!(target: LOG_TARGET,%error, "the debug UI fixture couldn't synthesize its review");
                    None
                }
            };
        }
        _ => return None,
    };
    let record = staged_swap(None, speed_up)?;
    let open = speed_up
        .and_then(|_| record.swap()?.orders().last())
        .map(wallet_ops::vault::SwapOrderRecord::uid);
    // What the orderbook quotes, and what the bridge leg delivers of it.
    let (bought, delivered) = if open.is_some() {
        (96_140_000_u32, SPED_UP.1)
    } else {
        (101_180_000_u32, RETRY.1)
    };
    let first = record.swap()?.orders().first()?;
    let BridgeOrderTerms::Across(deposit) = first.bridge()? else {
        return None;
    };
    let earlier = SwapContinuingApproval {
        uid: first.uid(),
        attempt: first.attempt(),
        destination_minimum: deposit.output_amount,
        post_hook_nonce: first.post_hook()?.nonce(),
        quote_timestamp: deposit.quote_timestamp,
    };
    // Only a speed-up discloses an earlier approval.
    let continuing = if open.is_some() {
        vec![earlier]
    } else {
        Vec::new()
    };
    let review = SwapReview::for_ui_fixture(
        profile,
        &record,
        WETH,
        USDC,
        U256::from(SWAP_SPEND),
        U256::from(bought),
        // 2,030 USDC for one ETH, each in base units for one whole native token.
        PairAnchorRate {
            sell_rate: U256::from(1_000_000_000_000_000_000_u64),
            buy_rate: U256::from(2_030_000_000_u32),
        },
        120_000_000,
        &SwapDelivery::Bridge(SWAP_DELIVERY),
        U256::from(delivered),
        U256::from(delivered),
        None,
        continuing,
        isolation,
    );
    match review {
        Ok(review) => Some((record, review, open)),
        Err(error) => {
            tracing::warn!(target: LOG_TARGET,%error, "the debug UI fixture couldn't synthesize its review");
            None
        }
    }
}

/// The two accounts of a `setup:replaced` mode, synthesized once: the swap's new account #5,
/// reserved a minute ago with the use the swap started with four minutes ago, and the retired
/// account #4 it replaced, which holds no swap. `None` when they fail to decode.
pub(super) fn replaced_accounts() -> Option<&'static (ExecutorRecord, ExecutorRecord)> {
    static ACCOUNTS: OnceLock<Option<(ExecutorRecord, ExecutorRecord)>> = OnceLock::new();
    ACCOUNTS
        .get_or_init(|| {
            let delegate = Address::repeat_byte(0x5d);
            let retired = ExecutorOperationId::random().ok()?;
            // The use keeps the ID it took from the account it started with.
            let swap_use = SwapUseId::first(retired);
            let now = now_unix();
            let swap = staged_setup_account(
                ExecutorOperationId::random().ok()?,
                (5, SWAP_ACCOUNT, delegate),
                (false, false),
                &serde_json::json!({
                    "version": 2, "created_at": now.saturating_sub(60),
                    "purpose_summary": "Private swap", "active_swap_use": swap_use,
                    "swap_uses": [{
                        "id": swap_use, "started_at": now.saturating_sub(240), "fresh": true,
                        "role": { "Source": { "approval": staged_setup_approval(None)? } },
                    }],
                }),
            )?;
            let retired = staged_setup_account(
                retired,
                (4, Address::repeat_byte(0x3a), delegate),
                (false, false),
                &serde_json::json!({ "retired": true }),
            )?;
            Some((swap, retired))
        })
        .as_ref()
}

/// The record of one stealth account of a `setup:` mode's swap, decoded from the JSON a stored
/// record has, with `links` its own swap fields. A sent setup signed one payload, and a
/// confirmed one's nonce was read as consumed.
pub(super) fn staged_setup_account(
    operation: ExecutorOperationId,
    (index, address, delegate): (u32, Address, Address),
    (sent, confirmed): (bool, bool),
    links: &serde_json::Value,
) -> Option<ExecutorRecord> {
    let issued = if sent {
        vec![staged_payload(
            0,
            0xa0,
            ExecutorPayloadPurpose::Operation,
            Some(0xc0),
        )?]
    } else {
        Vec::new()
    };
    let mut record = serde_json::json!({
        "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": operation, "index": index, "address": address, "delegate": delegate,
        "retired": false, "created_at": now_unix().saturating_sub(240), "issued": issued,
        "nonce_watermark": confirmed
            .then(|| ExecutorNonceWatermark::new(U256::ONE, SWAP_ORIGIN_BLOCK + 12)),
    });
    for (field, value) in links.as_object()? {
        record[field] = value.clone();
    }
    serde_json::from_value(record).ok()
}

/// The approval saved with a `setup:` mode's swap, which sets up a new account of its own and,
/// for the private Bridge `delivery`, a new one on the destination network.
pub(super) fn staged_setup_approval(delivery: Option<BridgeDelivery>) -> Option<SwapApproval> {
    let bounds: SwapApprovedBounds = serde_json::from_value(serde_json::json!({
        "sell_amount": U256::from(SWAP_SELL), "unshield_amount": U256::from(SWAP_SPEND),
        "unshield_fee_bps": U256::from(25_u8), "buy_amount": U256::from(RETRY.0),
        "private_minimum": U256::from(RETRY.0), "shield_fee_bps": U256::from(25_u8),
        "slippage_bps": 50, "pre_hook_gas_limit": 1_045_000, "post_hook_gas_limit": 396_000,
        "anchors": [], "destination_minimum": delivery.map(|_| U256::from(RETRY.1)),
        "gas_share_bps": 2_500, "gas_estimate": U256::from(492_000_u32),
        "gas_allowance": U256::from(123_000_u32), "gas_price_wei": 120_000_000,
        "valid_for_secs": 1_200
    }))
    .ok()?;
    let new = |address| SwapApprovedAccount {
        address: Some(address),
        setup: true,
    };
    Some(SwapApproval {
        bounds,
        price_verified: Some(true),
        price_acknowledged: false,
        delivery: delivery.map_or(SwapDelivery::Reshield, SwapDelivery::Bridge),
        tokens: Some(SwapApprovalTokens {
            sell: WETH,
            buy: USDC,
        }),
        accounts: Some(SwapApprovedAccounts {
            source: new(SWAP_ACCOUNT),
            destination: delivery.map(|delivery| new(delivery.receiver)),
        }),
    })
}

/// The records of a `cancelled:` mode, each decoded from the JSON a stored record has: account
/// #5 on Ethereum, for the accepted `delegate`, and destination account #12 on Arbitrum One.
/// Fifteen minutes ago a private Bridge swap of 0.05 WETH for USDC reserved both fresh. Both
/// setups, at nonce 0, are confirmed, and the swap was cancelled before its order, which
/// stopped its use of each account and retired #12. With `Reused`, a later swap of 0.02 WETH
/// for USDC back to the private balance claimed #5 two minutes ago and has no order yet.
/// Between the two records is the cancelled swap's identity, under which the view finds #12.
pub(super) fn staged_cancelled(
    cancelled: Cancelled,
    delegate: Address,
) -> Option<(ExecutorRecord, SwapIdentity, ExecutorRecord)> {
    let now = now_unix();
    let started = now.saturating_sub(900);
    let operation = ExecutorOperationId::random().ok()?;
    let destination_operation = ExecutorOperationId::random().ok()?;
    let swap = SwapIdentity {
        operation,
        swap_use: SwapUseId::first(operation),
    };
    let mut uses = vec![serde_json::json!({
        "id": swap.swap_use, "started_at": started, "fresh": true, "stopped": true,
        "stopped_before_order": true,
        "role": { "Source": {
            "approval": staged_setup_approval(Some(PRIVATE_DELIVERY))?,
            "destination_operation": destination_operation,
        }},
    })];
    let mut active = swap.swap_use;
    if cancelled == Cancelled::Reused {
        // 0.02 WETH, less Railgun's unshield fee, for 40.18 USDC, on the account as it is.
        let mut approval = staged_setup_approval(None)?;
        approval.bounds.unshield_amount = Some(U256::from(20_000_000_000_000_000_u64));
        approval.bounds.sell_amount = U256::from(19_950_000_000_000_000_u64);
        approval.bounds.buy_amount = U256::from(40_184_000_u32);
        approval.bounds.private_minimum = U256::from(40_184_000_u32);
        approval.accounts = Some(SwapApprovedAccounts {
            source: SwapApprovedAccount {
                address: Some(SWAP_ACCOUNT),
                setup: false,
            },
            destination: None,
        });
        active = SwapUseId::random().ok()?;
        uses.push(serde_json::json!({
            "id": active, "started_at": now.saturating_sub(120), "fresh": false,
            "role": { "Source": { "approval": approval } },
        }));
    }
    let source = staged_setup_account(
        operation,
        (5, SWAP_ACCOUNT, delegate),
        (true, true),
        &serde_json::json!({
            "version": 2, "created_at": started, "purpose_summary": "Private swap",
            "swap_setup_stopped": cancelled == Cancelled::Confirmed,
            "active_swap_use": active, "swap_uses": uses,
        }),
    )?;
    let destination = staged_setup_account(
        destination_operation,
        (12, SETUP_DESTINATION_ACCOUNT, Address::repeat_byte(0x5d)),
        (true, true),
        &serde_json::json!({
            "version": 2, "created_at": started, "retired": true, "swap_setup_stopped": true,
            "active_swap_use": swap.swap_use,
            "swap_uses": [{
                "id": swap.swap_use, "started_at": started, "fresh": true, "stopped": true,
                "stopped_before_order": true,
                "role": { "Destination": {
                    "origin_chain": SWAP_ORIGIN, "origin_operation": operation,
                    "destination_token": PRIVATE_DELIVERY.destination_token,
                }},
            }],
        }),
    )?;
    Some((source, swap, destination))
}

/// This session's stages of a setup whose job is at `stage`, or ended there with `result`.
/// A running one's route is where a job's route starts.
pub(super) fn staged_setup_progress(
    stage: TransactionGenerationStage,
    result: Option<Result<&PublicBroadcasterResultKind, String>>,
) -> SetupProgress {
    let mut progress = running_setup_progress(stage, SubmissionRouteProgress::default());
    if let Some(result) = result {
        progress.finish(result);
    }
    progress
}

/// This session's stages of a setup whose job is at `stage`, with its route at `route`.
pub(super) fn running_setup_progress(
    stage: TransactionGenerationStage,
    route: SubmissionRouteProgress,
) -> SetupProgress {
    let (sender, receiver) =
        tokio::sync::watch::channel(TransactionGenerationStage::SelectingPrivateNotes);
    // The job reported the stage it is at, as a real one does from its first stage on.
    let _ = sender.send(stage);
    let (route_sender, route) = tokio::sync::watch::channel(route);
    hold_route(route_sender);
    // A long identifier shows how the Broadcaster row wraps in a narrow column.
    let broadcaster = crate::root::private_swap::setup_progress::stand_in_setup_broadcaster(
        SWAP_ORIGIN,
        Some("fixture-relay.example"),
    );
    SetupProgress::running(broadcaster, receiver, route, None)
}

/// Keep a mode's route sender for as long as the wallet runs, as a running job keeps its own,
/// so the route never reads as closed.
pub(super) fn hold_route(sender: SubmissionRouteSender) {
    static HELD: Mutex<Vec<SubmissionRouteSender>> = Mutex::new(Vec::new());
    HELD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(sender);
}
