#[allow(clippy::wildcard_imports)]
use super::*;

/// What 1Click lists on `network` for a `non-evm:review` mode's Buy picker, bought through USDC
/// on Ethereum, the token the swap delivers last. On Solana: SOL, USDC and the look-alike
/// sUSDC, each with its own mint, and JUP. The sUSDC mint is a stand-in. On Zcash: ZEC.
fn non_evm_tokens(network: NonEvmNetwork) -> Vec<NonEvmBridgeDestination> {
    let token =
        |contract: Option<&str>, symbol: &str, asset: &str, decimals| NonEvmBridgeDestination {
            contract: contract.map(str::to_owned),
            intermediate: USDC,
            symbol: symbol.to_owned(),
            same_asset: symbol == "USDC",
            near: NearAssets {
                origin_asset: "nep141:eth-0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.omft.near"
                    .to_owned(),
                destination_asset: asset.to_owned(),
                origin_decimals: 6,
                destination_decimals: decimals,
            },
        };
    if network == NonEvmNetwork::Zcash {
        return vec![token(None, "ZEC", "nep141:zec.omft.near", 8)];
    }
    vec![
        token(None, "SOL", "nep141:sol.omft.near", 9),
        token(
            Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            "USDC",
            "nep141:sol-5ce3bf3a31af18be40ba30f721101b4341690186.omft.near",
            6,
        ),
        token(
            Some("FiNPw48pLZgbwmFiaYdTyJDvQCsefYx3CsBy4iVZFpBz"),
            "sUSDC",
            "1cs_v1:sol:spl:FiNPw48pLZgbwmFiaYdTyJDvQCsefYx3CsBy4iVZFpBz",
            6,
        ),
        token(
            Some("JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN"),
            "JUP",
            "nep141:sol-bb27241c87aa401cc963c360c175dd7ca7035873.omft.near",
            6,
        ),
    ]
}

/// The delivery of a `non-evm:` mode's swap: JUP to the Solana receiver, or ZEC to the Zcash
/// receiver.
pub(super) fn non_evm_delivery(network: NonEvmNetwork) -> Option<NonEvmDelivery> {
    let token = non_evm_tokens(network).pop()?;
    let receiver = if network == NonEvmNetwork::Zcash {
        NON_EVM_ZCASH_RECEIVER
    } else {
        NON_EVM_RECEIVER
    };
    Some(NonEvmDelivery {
        network,
        receiver: receiver.to_owned(),
        asset_id: token.near.destination_asset,
        contract: token.contract,
        symbol: token.symbol,
        decimals: token.near.destination_decimals,
    })
}

/// The network and tokens a `non-evm:review` mode's form lists. `None` in every other mode.
pub(in crate::root::private_swap) fn non_evm_routes()
-> Option<(NonEvmNetwork, Vec<NonEvmBridgeDestination>)> {
    match mode()? {
        Mode::NonEvm(NonEvm::Review, network) => Some((network, non_evm_tokens(network))),
        _ => None,
    }
}

/// The record of a `non-evm:` mode's swap: one order of 0.05 WETH for 100.46 USDC handed to
/// 1Click for JUP on Solana or ZEC on Zcash. `None` stages it expired, as the swap a
/// `non-evm:review` mode retries. The other states traded it 40 blocks after it was signed, 50
/// minutes ago, and handed it off, with 1Click's report as `state` names it.
pub(super) fn staged_non_evm(
    state: Option<NonEvm>,
    network: NonEvmNetwork,
) -> Option<ExecutorRecord> {
    let signed = now_unix().saturating_sub(3_000);
    let at = |block: u64, transaction: Option<&str>| SwapObservation {
        block: BlockNumHash::new(SWAP_ORIGIN_BLOCK + block, keccak256(block.to_be_bytes())),
        transaction_hash: transaction.map(keccak256),
    };
    let observed = match state {
        None | Some(NonEvm::Review) => {
            let expired = SwapPreHookDeath {
                cause: SwapPreHookDeathCause::Expired,
                observation: at(110, None),
            };
            SwapOrderObservations {
                pre_hook_dead: Some(expired),
                expired: Some(expired.observation),
                ..Default::default()
            }
        }
        Some(state) => {
            let settlement = at(40, Some("settlement"));
            SwapOrderObservations {
                traded: Some(settlement),
                pre_hook_executed: Some(settlement),
                delivered: Some(settlement),
                trade_amounts: Some(SwapTradeAmounts {
                    sell_amount: U256::from(SWAP_SELL),
                    buy_amount: U256::from(RETRY.0),
                    fee_amount: U256::ZERO,
                    settlement_gas_used: Some(1_386_204),
                    settlement_effective_gas_price: Some(410_000_000),
                    executed_fee: Some(U256::from(11_400_000_000_000_u64)),
                    executed_fee_token: Some(WETH),
                }),
                bridge_handoff: Some(SwapBridgeHandoff {
                    observation: settlement,
                    deposit_id: None,
                }),
                bridge_outcome: match state {
                    NonEvm::Delivered => Some(SwapBridgeOutcome::DeliveredReported {
                        amount_out: Some(U256::from(NON_EVM_REPORTED)),
                        transaction_hash: None,
                        destination_transaction: Some(
                            if network == NonEvmNetwork::Zcash {
                                NON_EVM_ZCASH_TRANSACTION
                            } else {
                                NON_EVM_TRANSACTION
                            }
                            .to_owned(),
                        ),
                    }),
                    NonEvm::NeedsAttention => Some(SwapBridgeOutcome::NeedsAttention),
                    NonEvm::Review | NonEvm::InProgress => None,
                },
                ..Default::default()
            }
        }
    };
    let mut order = staged_order(
        0,
        signed,
        (RETRY.0, NON_EVM_MINIMUM),
        0xa1,
        &serde_json::to_value(observed).ok()?,
    )?;
    order["delivery"] =
        serde_json::to_value(SwapDelivery::NonEvmBridge(non_evm_delivery(network)?)).ok()?;
    order["bridge"] = serde_json::to_value(BridgeOrderTerms::NearIntents(NearIntentsOrderTerms {
        deposit_address: address!("3e4b8c1d27a95f60e8d4c2b71a09f5d36c8e2b14"),
        min_amount_out: U256::from(NON_EVM_MINIMUM),
        amount_out: U256::from(NON_EVM_EXPECTED),
        deadline: "2026-10-30T13:20:00.000Z".to_owned(),
        signed_quote: "{}".to_owned(),
    }))
    .ok()?;
    staged_swap_record(&[order], if state.is_some() { 3 } else { 1 }, signed)
}
