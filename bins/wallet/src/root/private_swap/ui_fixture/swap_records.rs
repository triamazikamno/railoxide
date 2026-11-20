#[allow(clippy::wildcard_imports)]
use super::*;

/// The destination record of `swap` at `stage`, decoded from the JSON a stored record has.
pub(super) fn staged_record(
    stage: Detail,
    identity: SwapIdentity,
    swap: &Staged,
) -> Option<ExecutorRecord> {
    const ORIGIN_BLOCK: u64 = 31_204_518;
    const DESTINATION_BLOCK: u64 = 402_118_977;
    let now = now_unix();
    // An open order's validity is still ahead. An expired order's is behind.
    let valid_to = u32::try_from(if stage == Detail::Expired {
        now.saturating_sub(600)
    } else {
        now.saturating_add(1_200)
    })
    .ok()?;
    let hash = B256::repeat_byte(0x7a);
    let at = |number| SwapObservation {
        block: BlockNumHash::new(number, B256::repeat_byte(0xb1)),
        transaction_hash: Some(hash),
    };
    let fill = BlockNumHash::new(DESTINATION_BLOCK + 640, B256::repeat_byte(0xb2));
    let handed_off = |outcome| PublicSwapObservations {
        traded: Some(at(ORIGIN_BLOCK + 21)),
        bridge_handoff: Some(SwapBridgeHandoff {
            observation: at(ORIGIN_BLOCK + 21),
            deposit_id: Some(U256::from(1_279_830_u32)),
        }),
        deposited: Some(PublicSwapDeposited {
            input_amount: swap.bought,
            output_amount: swap.received,
        }),
        bridge_outcome: outcome,
        ..Default::default()
    };
    let observed = match stage {
        Detail::SetupNotSent
        | Detail::SetupPending
        | Detail::AccountReady
        | Detail::Approving
        | Detail::OrderOpen
        | Detail::PermitOrderOpen
        | Detail::PermitUsedUp => PublicSwapObservations::default(),
        Detail::Expired => PublicSwapObservations {
            expired: Some(SwapObservation {
                transaction_hash: None,
                ..at(ORIGIN_BLOCK + 60)
            }),
            ..Default::default()
        },
        Detail::Bridging => handed_off(None),
        Detail::Delivered => handed_off(Some(SwapBridgeOutcome::DeliveredVerified {
            block: fill,
            transaction_hash: hash,
            output_amount: swap.received,
            shielded: true,
        })),
        Detail::HeldOnDestination | Detail::HeldAfterPartialRecovery => {
            handed_off(Some(SwapBridgeOutcome::HeldOnDestination {
                block: fill,
                transaction_hash: hash,
                amount: swap.received,
            }))
        }
        Detail::Refunded => PublicSwapObservations {
            bridge_refund: Some(at(ORIGIN_BLOCK + 900)),
            ..handed_off(Some(SwapBridgeOutcome::Refunding))
        },
    };
    let account = Address::repeat_byte(0x3a);
    let delegate = Address::repeat_byte(0x5d);
    // An approval signed as a permit binds its pre-hook's gas limit and sends no transaction.
    let permit = matches!(stage, Detail::PermitOrderOpen | Detail::PermitUsedUp);
    let pre_hook_gas_limit = if permit { 110_000_u64 } else { 0 };
    let bounds: SwapApprovedBounds = serde_json::from_value(serde_json::json!({
        "sell_amount": swap.sold, "buy_amount": swap.bought,
        "private_minimum": swap.received * U256::from(99_u8) / U256::from(100_u8),
        "shield_fee_bps": "0x0", "slippage_bps": 50,
        "pre_hook_gas_limit": pre_hook_gas_limit, "anchors": []
    }))
    .ok()?;
    let saved = PublicSwapRecord::new(
        PublicSwapApproval {
            bounds,
            price_verified: Some(true),
            price_acknowledged: false,
            sell_token: swap.sell,
            on_shield_failure: if matches!(
                stage,
                Detail::HeldOnDestination | Detail::HeldAfterPartialRecovery
            ) {
                BridgeShieldFailure::KeepOnDestination
            } else {
                BridgeShieldFailure::RefundOnOrigin
            },
            destination: SwapApprovedAccount {
                address: Some(account),
                setup: true,
            },
            max_gas_cost: U256::ZERO,
        },
        PublicSwapIntent {
            bridged_token: swap.bridged,
            order: swap.order,
        },
    );
    let mut saved = serde_json::to_value(saved).ok()?;
    let traded = observed.traded.is_some();
    saved["observations"] = serde_json::to_value(observed).ok()?;
    if traded {
        saved["observations"]["trade_amounts"] = serde_json::json!({
            "sell_amount": swap.sold, "buy_amount": swap.bought, "fee_amount": "0x0"
        });
    }
    // The setup is on its way until the Public account approves, and its order follows.
    let set_up = !matches!(stage, Detail::SetupNotSent | Detail::SetupPending);
    let approves = set_up && stage != Detail::AccountReady;
    if approves && !permit {
        let inclusion = (stage != Detail::Approving).then(|| {
            serde_json::json!({
                "observation": at(ORIGIN_BLOCK), "finalized": true, "succeeded": true
            })
        });
        saved["transactions"] = serde_json::json!([{
            "kind": "Approval", "transaction": {}, "hash": hash, "inclusion": inclusion
        }]);
    }
    if approves && stage != Detail::Approving {
        let mut uid = [0_u8; 56];
        uid[52..].copy_from_slice(&valid_to.to_be_bytes());
        saved["path"] = serde_json::json!({ "Order": {
            "uid": alloy::hex::encode_prefixed(uid),
            "buy_token": swap.bridged, "proxy": Address::repeat_byte(0x4c),
            "batch": { "calldata": "0x", "nonce": B256::ZERO, "deadline": valid_to },
            "submission": {
                "signature": alloy::hex::encode_prefixed([0_u8; 65]), "quote_id": null
            },
            "submission_status": "Accepted"
        }});
        if permit {
            saved["path"]["Order"]["permit"] = serde_json::json!({
                "nonce": U256::ZERO, "deadline": valid_to, "value": swap.sold,
                "signature": alloy::hex::encode_prefixed([0_u8; 65])
            });
        }
    }
    let setup_block = BlockNumHash::new(DESTINATION_BLOCK, B256::repeat_byte(0xb3));
    let setup = serde_json::to_value(IssuedExecutorPayload::new(
        U256::ZERO,
        delegate,
        B256::repeat_byte(0x5e),
        ExecutorPayloadPurpose::Operation,
        ExecutorPayloadContext::new(
            Bytes::new(),
            ExecutorNonceObservation::new(setup_block, U256::ZERO),
            Vec::new(),
        ),
    ))
    .ok()?;
    // A setup that wasn't sent issued no payload.
    let issued = if stage == Detail::SetupNotSent {
        Vec::new()
    } else {
        vec![setup]
    };
    // A confirmed setup is one whose nonce an account read showed consumed.
    let consumed = set_up.then(|| ExecutorNonceWatermark::new(U256::ONE, DESTINATION_BLOCK));
    serde_json::from_value(serde_json::json!({
        "version": 2, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": identity.operation, "index": 3,
        "address": account, "delegate": delegate, "retired": false, "issued": issued,
        "nonce_watermark": consumed,
        "swap_uses": [{ "id": identity.swap_use, "started_at": now.saturating_sub(240),
            "fresh": true,
            "role": { "PublicSourceDestination": { "origin_chain": swap.origin,
                "source": swap.source, "destination_token": swap.delivered, "swap": saved
            }}
        }]
    }))
    .ok()
}

impl Reported {
    /// How many blocks the head is past the reported trade block.
    pub(super) const fn confirmations(self) -> u64 {
        match self {
            Self::Fill | Self::Handoff => 3,
            Self::Delivered | Self::Shielded | Self::Held => 2,
        }
    }

    /// Whether Across reports the swap's delivery.
    pub(super) const fn delivered(self) -> bool {
        matches!(self, Self::Delivered | Self::Shielded | Self::Held)
    }

    /// Whether the swap delivers to the private balance on Arbitrum One.
    pub(super) const fn private(self) -> bool {
        matches!(self, Self::Shielded | Self::Held)
    }
}

/// The record of a `reported:` mode's swap: one open order, signed five minutes ago, with
/// nothing observed. `Fill` delivers to the private balance on Ethereum, `Shielded` and `Held`
/// by Across to the private balance on Arbitrum One with a 0.25% shield fee there, and the
/// other states by Across to the public receiver on Arbitrum One.
pub(super) fn staged_reported(reported: Reported) -> Option<ExecutorRecord> {
    let signed = now_unix().saturating_sub(300);
    let open = serde_json::to_value(SwapOrderObservations::default()).ok()?;
    let mut order = staged_order(0, signed, RETRY, 0xa1, &open)?;
    if reported == Reported::Fill {
        order["delivery"] = serde_json::to_value(SwapDelivery::Reshield).ok()?;
        order["bridge"] = serde_json::Value::Null;
        order["bounds"]["destination_minimum"] = serde_json::Value::Null;
    }
    if reported.private() {
        order["delivery"] = serde_json::to_value(SwapDelivery::Bridge(PRIVATE_DELIVERY)).ok()?;
        order["bounds"]["destination_shield_fee_bps"] = serde_json::json!(U256::from(25_u8));
    }
    staged_swap_record(&[order], 1, signed)
}

/// Order `attempt` of the synthesized private swap, as its record stores it: signed at `signed`
/// for 20 minutes, with a deposit that Across has four hours to fill. `hash` tells its ID and
/// its hooks, at nonces 1 and 2, apart from another order's.
pub(super) fn staged_order(
    attempt: u32,
    signed: u64,
    (deposit, minimum): (u64, u64),
    hash: u8,
    observations: &serde_json::Value,
) -> Option<serde_json::Value> {
    let quoted = u32::try_from(signed).ok()?;
    let uid = OrderUid::new(keccak256([hash]), SWAP_ACCOUNT, quoted.checked_add(1_200)?);
    Some(serde_json::json!({
        "attempt": attempt, "uid": uid.0, "delivery": SwapDelivery::Bridge(SWAP_DELIVERY),
        "bounds": {
            "sell_amount": U256::from(SWAP_SELL), "unshield_amount": U256::from(SWAP_SPEND),
            "unshield_fee_bps": U256::from(25_u8), "buy_amount": U256::from(deposit),
            "private_minimum": U256::from(deposit), "shield_fee_bps": U256::ZERO,
            "slippage_bps": 50, "pre_hook_gas_limit": 1_045_000, "post_hook_gas_limit": 396_000,
            "anchors": [], "destination_minimum": U256::from(minimum), "gas_share_bps": 2_500,
            "gas_estimate": U256::from(492_000_u32), "gas_allowance": U256::from(123_000_u32),
            "gas_price_wei": 120_000_000, "valid_for_secs": 1_200
        },
        "pre_hook": { "nonce": U256::ONE, "payload": B256::repeat_byte(hash) },
        "post_hook": { "nonce": U256::from(2_u8), "payload": B256::repeat_byte(hash + 1) },
        "invalidates": null, "observations": observations, "submission_status": "Accepted",
        "bridge": BridgeOrderTerms::Across(AcrossOrderTerms {
            spoke_pool: address!("5c7bcd6e7de5423a257d81b442095a1a6ced35c5"),
            input_token: USDC,
            output_token: SWAP_DELIVERY.destination_token,
            input_amount: U256::from(deposit),
            output_amount: U256::from(minimum),
            quote_timestamp: quoted,
            fill_deadline: quoted.checked_add(14_400)?,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
            recipient: None,
            message_hash: None,
        }),
    }))
}

/// The record of the synthesized private swap, decoded from the JSON a stored record has.
/// Without `split`, its first order expired and nothing ran. With it, the retry traded and its
/// settlement made the first order's deposit, whose outcome the stage names. `Unverified` is
/// one order instead, whose trade showed no deposit. With `speed_up` and no `split`, the
/// expired first order is followed by its open retry, or for `TwoOrders` the swap holds the
/// retry and its speed-up, both open. `Rejected` is `TwoOrders` with the speed-up's submission
/// rejected. The other speed-up stages hold those two orders once one of them traded, as
/// [`staged_overlap`] stages them.
pub(super) fn staged_swap(
    split: Option<Split>,
    speed_up: Option<SpeedUp>,
) -> Option<ExecutorRecord> {
    let now = now_unix();
    // A refund follows the deposit's fill deadline.
    let refunds = matches!(split, Some(Split::Refunding | Split::RefundVerified));
    let signed = now.saturating_sub(if refunds { 18_000 } else { 3_000 });
    let at = |block: u64, transaction: Option<&str>| SwapObservation {
        block: BlockNumHash::new(SWAP_ORIGIN_BLOCK + block, keccak256(block.to_be_bytes())),
        transaction_hash: transaction.map(keccak256),
    };
    let value = |observed: SwapOrderObservations| serde_json::to_value(observed).ok();
    // The first order's 20 minutes are 100 blocks, and the retry is signed 25 blocks later.
    let expired = SwapPreHookDeath {
        cause: SwapPreHookDeathCause::Expired,
        observation: at(110, None),
    };
    let single = split == Some(Split::Unverified);
    let settlement = at(if single { 40 } else { 160 }, Some("settlement"));
    let traded = SwapOrderObservations {
        traded: Some(settlement),
        trade_amounts: Some(SwapTradeAmounts {
            sell_amount: U256::from(SWAP_SELL),
            buy_amount: U256::from(SWAP_BOUGHT),
            fee_amount: U256::ZERO,
            settlement_gas_used: Some(1_386_204),
            settlement_effective_gas_price: Some(410_000_000),
            executed_fee: Some(U256::from(11_400_000_000_000_u64)),
            executed_fee_token: Some(WETH),
        }),
        ..Default::default()
    };
    // The orders, and the account's nonce once their hooks ran.
    let (orders, nonce) = match split {
        None => {
            let ended = value(SwapOrderObservations {
                pre_hook_dead: Some(expired),
                expired: Some(expired.observation),
                ..Default::default()
            })?;
            let open = value(SwapOrderObservations::default())?;
            // An open order was signed five minutes ago, and the one before it five earlier.
            // A just-placed one is signed now.
            let orders = match speed_up {
                None => vec![staged_order(0, signed, FIRST_ORDER, 0xa1, &ended)?],
                Some(SpeedUp::Review | SpeedUp::Open) => vec![
                    staged_order(0, signed, FIRST_ORDER, 0xa1, &ended)?,
                    staged_order(1, now.saturating_sub(300), RETRY, 0xb1, &open)?,
                ],
                Some(SpeedUp::JustPlaced) => vec![
                    staged_order(0, signed, FIRST_ORDER, 0xa1, &ended)?,
                    staged_order(1, now, RETRY, 0xb1, &open)?,
                ],
                Some(SpeedUp::TwoOrders | SpeedUp::Rejected) => {
                    let mut placed =
                        staged_order(1, now.saturating_sub(300), SPED_UP, 0xb1, &open)?;
                    if speed_up == Some(SpeedUp::Rejected) {
                        placed["submission_status"] = serde_json::json!("Rejected");
                    }
                    vec![
                        staged_order(0, now.saturating_sub(600), RETRY, 0xa1, &open)?,
                        placed,
                    ]
                }
                Some(won) => return staged_overlap(won, settlement, &traded, now),
            };
            (orders, 1_u8)
        }
        // The pre-hook ran and the post-hook, which deposits, didn't.
        Some(Split::Unverified) => {
            let traded = value(SwapOrderObservations {
                pre_hook_executed: Some(settlement),
                ..traded
            })?;
            (vec![staged_order(0, signed, RETRY, 0xa1, &traded)?], 2)
        }
        Some(split) => {
            let outcome = match split {
                Split::Delivered => Some(SwapBridgeOutcome::DeliveredVerified {
                    block: BlockNumHash::new(402_118_977, keccak256("fill block")),
                    transaction_hash: keccak256("fill"),
                    output_amount: U256::from(FIRST_ORDER.1),
                    shielded: false,
                }),
                Split::Refunding | Split::RefundVerified => Some(SwapBridgeOutcome::Refunding),
                Split::Bridging | Split::Unverified => None,
            };
            let mut deposit = value(SwapOrderObservations {
                pre_hook_dead: Some(expired),
                delivered: Some(settlement),
                bridge_handoff: Some(SwapBridgeHandoff {
                    observation: settlement,
                    deposit_id: Some(U256::from(3_481_207_u32)),
                }),
                bridge_outcome: outcome,
                bridge_refund: (split == Split::RefundVerified).then(|| at(1_420, Some("refund"))),
                ..Default::default()
            })?;
            // The settlement reshielded what the trade bought beyond the deposit, less the
            // shield fee.
            deposit["settlement_credit"] = serde_json::json!({
                "observation": settlement,
                "private_amount": U256::from(10_865_249_u32), "fee": U256::from(27_231_u32)
            });
            (
                vec![
                    staged_order(0, signed, FIRST_ORDER, 0xa1, &deposit)?,
                    staged_order(1, signed + 1_500, RETRY, 0xb1, &value(traded)?)?,
                ],
                3,
            )
        }
    };
    staged_swap_record(&orders, nonce, signed)
}

/// The synthesized swap's two competing orders once one of them traded, which `won` names:
/// the original, signed ten minutes ago, and its speed-up, signed five minutes ago at the same
/// pre-hook nonce. The traded order holds `traded` at `settlement` with its own delivery, and
/// the other holds nothing, so it reads as not filled.
fn staged_overlap(
    won: SpeedUp,
    settlement: SwapObservation,
    traded: &SwapOrderObservations,
    now: u64,
) -> Option<ExecutorRecord> {
    let private = won == SpeedUp::OriginalPrivate;
    let (terms, delivers) = if won == SpeedUp::ReplacementDelivered {
        (SPED_UP, true)
    } else {
        (RETRY, won == SpeedUp::OriginalDelivered)
    };
    let mut filled = serde_json::to_value(SwapOrderObservations {
        pre_hook_executed: Some(settlement),
        delivered: Some(settlement),
        bridge_handoff: (!private).then_some(SwapBridgeHandoff {
            observation: settlement,
            deposit_id: Some(U256::from(3_481_207_u32)),
        }),
        bridge_outcome: delivers.then(|| SwapBridgeOutcome::DeliveredVerified {
            block: BlockNumHash::new(402_118_977, keccak256("fill block")),
            transaction_hash: keccak256("fill"),
            output_amount: U256::from(terms.1),
            shielded: false,
        }),
        ..traded.clone()
    })
    .ok()?;
    // The settlement shielded everything a private swap bought, and for a Bridge swap what
    // the trade bought beyond the deposit, less the shield fee.
    if private {
        filled["shielded"] = serde_json::json!({
            "observation": settlement,
            "private_amount": U256::from(SWAP_BOUGHT), "fee": U256::ZERO
        });
    } else {
        let surplus = SWAP_BOUGHT.checked_sub(terms.0)?;
        let fee = surplus / 400;
        filled["settlement_credit"] = serde_json::json!({
            "observation": settlement,
            "private_amount": U256::from(surplus - fee), "fee": U256::from(fee)
        });
    }
    let unfilled = serde_json::to_value(SwapOrderObservations::default()).ok()?;
    let (original, speed_up) = if won == SpeedUp::ReplacementDelivered {
        (&unfilled, &filled)
    } else {
        (&filled, &unfilled)
    };
    let mut orders = vec![
        staged_order(0, now.saturating_sub(600), RETRY, 0xa1, original)?,
        staged_order(1, now.saturating_sub(300), SPED_UP, 0xb1, speed_up)?,
    ];
    if private {
        for order in &mut orders {
            order["delivery"] = serde_json::to_value(SwapDelivery::Reshield).ok()?;
            order["bridge"] = serde_json::Value::Null;
            order["bounds"]["destination_minimum"] = serde_json::Value::Null;
        }
    }
    staged_swap_record(&orders, 3, now.saturating_sub(600))
}

/// The synthesized swap's record around `orders`, whose hooks left the account at `nonce`,
/// reserved five minutes before `signed`.
pub(super) fn staged_swap_record(
    orders: &[serde_json::Value],
    nonce: u8,
    signed: u64,
) -> Option<ExecutorRecord> {
    serde_json::from_value(staged_swap_json(orders, nonce, signed)?).ok()
}

/// [`staged_swap_record`] as the JSON a stored record has.
fn staged_swap_json(
    orders: &[serde_json::Value],
    nonce: u8,
    signed: u64,
) -> Option<serde_json::Value> {
    use ExecutorPayloadPurpose::{Operation, SwapPostHook, SwapPreHook};
    let mut issued = vec![staged_payload(0, 0xa0, Operation, Some(0xc0))];
    for hash in [0xa1, 0xb1].into_iter().take(orders.len()) {
        issued.push(staged_payload(1, hash, SwapPreHook, None));
        issued.push(staged_payload(2, hash + 1, SwapPostHook, None));
    }
    Some(serde_json::json!({
        "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": ExecutorOperationId::random().ok()?, "index": 4,
        "address": SWAP_ACCOUNT, "delegate": Address::repeat_byte(0x5d), "retired": false,
        "created_at": signed.saturating_sub(300),
        "issued": issued.into_iter().collect::<Option<Vec<_>>>()?,
        "nonce_watermark": ExecutorNonceWatermark::new(
            U256::from(nonce),
            SWAP_ORIGIN_BLOCK + 1_500,
        ),
        "swap": {
            "terms": SwapTerms::new(
                WETH,
                USDC,
                SwapRecipient::new(U256::ONE, [0; 32]),
                B256::repeat_byte(0xa0),
            ),
            "proof": SwapProof::new(keccak256("proof"), Vec::new()),
            "orders": orders,
        },
    }))
}

/// The record of a `setup:` mode's swap of 0.05 WETH for USDC back to the private balance,
/// whose one order was placed on its own setup while that setup was included: the setup at
/// nonce 0, which the record's confirmed nonce read leaves unresolved, and the order's hooks at
/// nonces 1 and 2. The order was signed five minutes ago, or with `expired` 25 minutes ago, past
/// its 20-minute validity, and ended at its expiry with its pre-hook never run.
pub(super) fn staged_setup_under_order(expired: bool) -> Option<ExecutorRecord> {
    let signed = now_unix().saturating_sub(if expired { 1_500 } else { 300 });
    let observations = if expired {
        let observation = SwapObservation {
            block: BlockNumHash::new(SWAP_ORIGIN_BLOCK + 1_600, keccak256("expired")),
            transaction_hash: None,
        };
        SwapOrderObservations {
            pre_hook_dead: Some(SwapPreHookDeath {
                cause: SwapPreHookDeathCause::Expired,
                observation,
            }),
            expired: Some(observation),
            ..Default::default()
        }
    } else {
        SwapOrderObservations::default()
    };
    let mut order = staged_order(
        0,
        signed,
        RETRY,
        0xa1,
        &serde_json::to_value(observations).ok()?,
    )?;
    order["delivery"] = serde_json::to_value(SwapDelivery::Reshield).ok()?;
    order["bridge"] = serde_json::Value::Null;
    order["bounds"]["destination_minimum"] = serde_json::Value::Null;
    let mut record = staged_swap_json(&[order], 0, signed)?;
    // The account was last read at nonce 0, below the setup's inclusion.
    record["nonce_observation"] = serde_json::to_value(ExecutorNonceObservation::new(
        BlockNumHash::new(SWAP_ORIGIN_BLOCK + 1_500, keccak256("observed")),
        U256::ZERO,
    ))
    .ok()?;
    // The approval saved with the swap, which keeps the fee limit of a setup sent again.
    record["swap_approval"] = serde_json::to_value(staged_setup_approval(None)?).ok()?;
    serde_json::from_value(record).ok()
}
