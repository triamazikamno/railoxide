#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::public_wallet) fn public_action_eip1559_transaction_request(
    tx_req: TransactionRequest,
    chain_id: u64,
    from: Address,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    nonce: u64,
) -> TransactionRequest {
    tx_req
        .with_chain_id(chain_id)
        .with_from(from)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
        .with_nonce(nonce)
}

pub(in crate::public_wallet) fn public_action_legacy_transaction_request(
    tx_req: TransactionRequest,
    chain_id: u64,
    from: Address,
    gas_price: u128,
    nonce: u64,
) -> TransactionRequest {
    tx_req
        .with_chain_id(chain_id)
        .with_from(from)
        .with_gas_price(gas_price)
        .with_nonce(nonce)
}

pub fn sanitize_walletconnect_transaction_request(
    tx_req: TransactionRequest,
    chain_id: u64,
    from: Address,
) -> TransactionRequest {
    TransactionRequest {
        chain_id: Some(chain_id),
        from: Some(from),
        to: tx_req.to,
        value: tx_req.value,
        input: tx_req.input,
        access_list: tx_req.access_list,
        ..Default::default()
    }
}

#[must_use]
pub fn walletconnect_transaction_payload_fingerprint(
    chain_id: u64,
    from: Address,
    tx_req: &TransactionRequest,
) -> FixedBytes<32> {
    let mut encoded = b"railoxide:walletconnect-transaction:v1".to_vec();
    encoded.extend_from_slice(&chain_id.to_be_bytes());
    encoded.extend_from_slice(from.as_slice());
    match tx_req.to {
        Some(TxKind::Call(to)) => {
            encoded.push(1);
            encoded.extend_from_slice(to.as_slice());
        }
        Some(TxKind::Create) | None => encoded.push(0),
    }
    encoded.extend_from_slice(&tx_req.value.unwrap_or_default().to_be_bytes::<32>());
    let input = tx_req.input.input().map_or(&[][..], |input| input.as_ref());
    encoded.extend_from_slice(&(input.len() as u64).to_be_bytes());
    encoded.extend_from_slice(input);
    if let Some(access_list) = tx_req.access_list.as_ref() {
        encoded.push(1);
        encoded.extend_from_slice(&serde_json::to_vec(access_list).unwrap_or_default());
    } else {
        encoded.push(0);
    }
    keccak256(encoded)
}

pub fn validate_walletconnect_reviewed_transaction(
    chain_id: u64,
    from: Address,
    tx_req: &TransactionRequest,
    payload_fingerprint: FixedBytes<32>,
    gas_limit: u64,
) -> Result<()> {
    if gas_limit == 0 {
        return Err(eyre!(
            "reviewed WalletConnect gas limit must be greater than zero"
        ));
    }
    if walletconnect_transaction_payload_fingerprint(chain_id, from, tx_req) != payload_fingerprint
    {
        return Err(eyre!(
            "WalletConnect transaction changed after simulation review"
        ));
    }
    Ok(())
}

pub(in crate::public_wallet) fn public_action_native_exposure(
    value: U256,
    gas_limit: u64,
    max_fee_per_gas: u128,
) -> U256 {
    value + (U256::from(gas_limit) * U256::from(max_fee_per_gas))
}

pub(in crate::public_wallet) const fn ensure_advanced_gas_estimate_authorized(
    estimated_gas: u64,
    authorized_gas_limit: u64,
) -> std::result::Result<(), PublicActionPreflightError> {
    if estimated_gas > authorized_gas_limit {
        return Err(PublicActionPreflightError::GasAboveReviewedLimit {
            required_gas_units: estimated_gas,
            reviewed_gas_units: authorized_gas_limit,
        });
    }
    Ok(())
}

/// A model-priced action signs within the limit its fee was reviewed at. Railoxide signs exactly
/// that limit over a raw estimate within it. Mimic Railway signs its 120% of the raw estimate
/// unclamped, which a clamp would no longer match, so it refuses once that exceeds the limit.
/// An estimate above the limit means unusual execution, such as a token or receiver that burns
/// gas. It is refused rather than reviewed again, because a fresh review computes the same limit.
pub(in crate::public_wallet) fn reviewed_signing_gas_limit(
    profile: PublicShieldTransactionProfile,
    raw_gas_limit: u64,
    reviewed_gas_limit: u64,
) -> std::result::Result<u64, PublicActionPreflightError> {
    let (required_gas_units, gas_limit) = match profile {
        PublicShieldTransactionProfile::Railway => {
            let gas_limit = crate::public_wallet::gas::railway_gas_limit(raw_gas_limit);
            (gas_limit, gas_limit)
        }
        PublicShieldTransactionProfile::Railoxide => (raw_gas_limit, reviewed_gas_limit),
    };
    if required_gas_units > reviewed_gas_limit {
        return Err(PublicActionPreflightError::GasAboveReviewedLimit {
            required_gas_units,
            reviewed_gas_units: reviewed_gas_limit,
        });
    }
    Ok(gas_limit)
}

/// A native shield signs its profile's buffer over its final raw estimate, at most its
/// reviewed limit. The final shield key can move the estimate slightly from the prepared one,
/// so only a raw estimate above the reviewed limit is refused.
pub(in crate::public_wallet) fn native_shield_signing_gas_limit(
    profile: PublicShieldTransactionProfile,
    gas_limit_buffer: u64,
    raw_gas_limit: u64,
    reviewed_gas_limit: u64,
) -> Result<u64> {
    if raw_gas_limit > reviewed_gas_limit {
        return Err(eyre!(
            "native shield gas estimate {raw_gas_limit} exceeds its reviewed signing limit {reviewed_gas_limit}; prepare and review the shield again"
        ));
    }
    let gas_limit = crate::public_wallet::gas::native_shield_gas_limit(
        profile,
        raw_gas_limit,
        gas_limit_buffer,
    );
    Ok(gas_limit.min(reviewed_gas_limit))
}

/// A prepared native shield is type 4, which a network without a base fee can't carry.
pub(in crate::public_wallet) fn ensure_native_shield_envelope(
    gas_limit_strategy: PublicActionGasLimitStrategy,
    legacy_envelope: bool,
) -> Result<()> {
    let prepared_native = matches!(
        gas_limit_strategy,
        PublicActionGasLimitStrategy::NativeShieldPrepared { .. }
    );
    if prepared_native && legacy_envelope {
        return Err(eyre!(
            "this network has no base fee, so the type-4 native shield is unavailable; prepare the shield again"
        ));
    }
    Ok(())
}
