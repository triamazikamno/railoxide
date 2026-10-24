use alloy::eips::BlockId;
use alloy::primitives::aliases::U120;
use alloy::primitives::{Address, U256, uint};
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Result, eyre};

use crate::settings::{EffectiveChainConfig, resolve_effective_chain_rpc_route};
use crate::{HttpContext, RpcRoute, WalletRpcOrigin};

pub const RAILGUN_PROTOCOL_FEE_BPS: U256 = uint!(25_U256);
pub(crate) const FEE_BASIS_POINTS_DENOMINATOR: U256 = uint!(10_000_U256);

#[must_use]
pub(crate) fn railgun_protocol_fee_amount(amount: U256, fee_bps: U256) -> U256 {
    amount * fee_bps / FEE_BASIS_POINTS_DENOMINATOR
}

sol! {
    interface RailgunLogic {
        function shieldFee() external view returns (uint120);
        function unshieldFee() external view returns (uint120);
    }
}

/// Reads the Railgun contract's shield fee in basis points at `block`, or at `latest` when
/// `block` is `None`.
pub async fn read_shield_fee_bps(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    railgun_contract: Address,
    block: Option<BlockId>,
) -> Result<U256> {
    read_fee_bps(
        chain,
        http,
        railgun_contract,
        block,
        RailgunLogic::shieldFeeCall {},
        "shield",
    )
    .await
}

/// Reads the Railgun contract's unshield fee in basis points at `block`, or at `latest` when
/// `block` is `None`. The contract takes it from the unshielded value: the recipient gets
/// `amount - railgun_protocol_fee_amount(amount, fee)`.
pub async fn read_unshield_fee_bps(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    railgun_contract: Address,
    block: Option<BlockId>,
) -> Result<U256> {
    read_fee_bps(
        chain,
        http,
        railgun_contract,
        block,
        RailgunLogic::unshieldFeeCall {},
        "unshield",
    )
    .await
}

async fn read_fee_bps<C: SolCall<Return = U120> + 'static>(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
    railgun_contract: Address,
    block: Option<BlockId>,
    call: C,
    fee: &str,
) -> Result<U256> {
    let route = RpcRoute::from(
        resolve_effective_chain_rpc_route(chain.chain_id, chain)?
            .for_class(crate::RpcTrafficClass::General)?,
    );
    let value = http
        .rpc_broker()
        .submit_calls_decoded_at::<C>(
            route,
            vec![(railgun_contract, call.abi_encode().into())],
            block.unwrap_or_else(BlockId::latest),
            WalletRpcOrigin::Swaps.into(),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("{fee} fee read returned no result"))??;
    Ok(U256::from(value))
}

#[must_use]
pub fn format_protocol_fee_percentage(fee_bps: U256) -> String {
    let whole = fee_bps / U256::from(100);
    let fractional = fee_bps % U256::from(100);
    if fractional.is_zero() {
        return format!("{whole}%");
    }

    let mut fractional = format!("{fractional:0>2}");
    while fractional.ends_with('0') {
        fractional.pop();
    }
    format!("{whole}.{fractional}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_fee_percentage_formats_basis_points() {
        assert_eq!(format_protocol_fee_percentage(uint!(25_U256)), "0.25%");
        assert_eq!(format_protocol_fee_percentage(uint!(250_U256)), "2.5%");
        assert_eq!(format_protocol_fee_percentage(uint!(100_U256)), "1%");
    }

    #[tokio::test]
    async fn shield_fee_read_decodes_the_contract_value() {
        use alloy::primitives::Bytes;
        use serde_json::json;

        let railgun = Address::repeat_byte(0x22);
        let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
            std::sync::Arc::new(move |request| {
                assert_eq!(request["method"], "eth_call");
                assert_eq!(request["params"][0]["to"], railgun.to_string());
                assert_eq!(request["params"][1], json!("0xa"));
                let fee = RailgunLogic::shieldFeeCall::abi_encode_returns(
                    &alloy::primitives::aliases::U120::from(25),
                );
                json!({"jsonrpc": "2.0", "id": request["id"], "result": Bytes::from(fee)})
            }),
            std::sync::Arc::default(),
            std::sync::Arc::default(),
        )
        .await;
        let mut chains = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap();
        let chain = chains.get_mut(1).unwrap();
        chain.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
        let fee = read_shield_fee_bps(
            chain,
            &HttpContext::direct_for_tests(),
            railgun,
            Some(BlockId::number(10)),
        )
        .await
        .unwrap();
        assert_eq!(fee, RAILGUN_PROTOCOL_FEE_BPS);
        server.abort();
    }
}
