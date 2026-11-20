#[allow(clippy::wildcard_imports)]
use super::*;

#[must_use]
pub fn broadcaster_fee_amount(
    token_fee_per_unit_gas: U256,
    gas_limit: u64,
    gas_price: u128,
) -> U256 {
    const FEE_SCALE: U256 = uint!(1_000_000_000_000_000_000_U256);
    // The rate comes from an untrusted fee row; wrapping would turn a huge rate into a tiny fee.
    token_fee_per_unit_gas
        .saturating_mul(U256::from(gas_limit))
        .saturating_mul(U256::from(gas_price))
        / FEE_SCALE
}

#[must_use]
pub const fn public_broadcaster_service_gas_price(min_gas_price: u128) -> u128 {
    min_gas_price * 125 / 100
}

#[must_use]
pub const fn public_broadcaster_bound_min_gas_price(chain_id: u64, min_gas_price: u128) -> u128 {
    match chain_id {
        // Arbitrum eth_call/eth_estimateGas exposes the current child-chain gas price as
        // tx.gasprice, even when a higher legacy gasPrice is supplied. The RAILGUN contract
        // documents minGasPrice as type-0-only, so bind zero on Arbitrum and keep pricing separate.
        42_161 | 42_170 | 421_614 => 0,
        _ => min_gas_price,
    }
}

#[must_use]
pub fn public_broadcaster_native_gas_cost(gas_limit: u64, min_gas_price: u128) -> U256 {
    U256::from(gas_limit) * U256::from(public_broadcaster_service_gas_price(min_gas_price))
}

#[must_use]
pub fn public_broadcaster_fee_breakdown(
    total_fee: U256,
    gas_limit: u64,
    min_gas_price: u128,
    fee_token_anchor_rate: Option<U256>,
) -> PublicBroadcasterFeeBreakdown {
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    let fee_token_gas_cost = fee_token_anchor_rate
        .filter(|anchor_rate| !anchor_rate.is_zero())
        .map(|anchor_rate| broadcaster_fee_amount(anchor_rate, gas_limit, service_gas_price));
    PublicBroadcasterFeeBreakdown {
        native_gas_cost: U256::from(gas_limit) * U256::from(service_gas_price),
        fee_token_gas_cost,
        broadcaster_fee: fee_token_gas_cost
            .map(|gas_cost| PublicBroadcasterFeeMargin::from_total_and_gas(total_fee, gas_cost)),
    }
}

pub(crate) fn broadcaster_fee_covers(available_fee: U256, required_fee: U256) -> bool {
    available_fee >= required_fee
}

/// Allow a 25% increase over the estimated fee in spend review. This is an approval
/// ceiling; payment still uses the required fee with the ordinary fee buffer.
#[must_use]
pub fn default_public_broadcaster_fee_limit(estimated_fee: U256) -> U256 {
    estimated_fee.saturating_add(estimated_fee / U256::from(4))
}

#[must_use]
pub fn buffered_public_broadcaster_fee(required_fee: U256) -> U256 {
    let buffer = required_fee / PUBLIC_BROADCASTER_FEE_BUFFER_DIVISOR;
    required_fee
        + if buffer.is_zero() {
            uint!(1_U256)
        } else {
            buffer
        }
}

pub(crate) fn bounded_public_broadcaster_fee(
    required_fee: U256,
    maximum: Option<U256>,
) -> Result<U256> {
    if maximum.is_some_and(|maximum| required_fee > maximum) {
        return Err(eyre!(
            "broadcaster fee exceeds the reviewed maximum; refresh and approve the quote"
        ));
    }
    let buffered = buffered_public_broadcaster_fee(required_fee);
    Ok(maximum.map_or(buffered, |maximum| buffered.min(maximum)))
}

pub(crate) fn validate_custom_public_broadcaster_fee(
    amount: U256,
    required: U256,
    maximum: Option<U256>,
) -> Result<U256> {
    if amount.is_zero() || amount < required {
        return Err(eyre!(
            "Custom transaction fee is too low. Increase it or use automatic estimation."
        ));
    }
    if maximum.is_some_and(|maximum| amount > maximum) {
        return Err(eyre!(
            "Custom transaction fee exceeds the reviewed maximum. Review the updated fee before submitting."
        ));
    }
    Ok(amount)
}
