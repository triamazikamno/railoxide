#[allow(clippy::wildcard_imports)]
use super::*;

/// A standalone direct Railgun shield of one request.
pub(in crate::public_wallet) const fn public_shield_gas_units(
    model: &RailgunGasModel,
    mode: GasEstimateMode,
) -> u64 {
    TRANSACTION_INTRINSIC_GAS.saturating_add(model.shield(mode, 1))
}

#[must_use]
pub fn public_native_action_gas_units(chain_id: u64, steps: &[PublicActionProgressStep]) -> u64 {
    public_native_action_gas_units_with_buffer(
        RailgunGasModel::for_chain(chain_id),
        steps,
        GAS_LIMIT_BUFFER,
    )
}

#[must_use]
pub(crate) fn public_native_action_gas_units_with_buffer(
    model: &RailgunGasModel,
    steps: &[PublicActionProgressStep],
    gas_limit_buffer: u64,
) -> u64 {
    steps.iter().fold(0_u64, |total, step| {
        let gas_units = public_native_step_gas_units(model, *step);
        if gas_units == 0 {
            total
        } else {
            total.saturating_add(gas_units + gas_limit_buffer)
        }
    })
}

#[must_use]
pub fn public_native_action_gas_reserve(
    chain_id: u64,
    max_fee_per_gas: u128,
    steps: &[PublicActionProgressStep],
) -> U256 {
    public_native_action_gas_reserve_with_buffer(
        RailgunGasModel::for_chain(chain_id),
        max_fee_per_gas,
        steps,
        GAS_LIMIT_BUFFER,
    )
}

#[must_use]
pub const fn public_native_action_gas_units_from_walletconnect_intent(
    kind: &WalletConnectDecodedCallKind,
) -> Option<u64> {
    match kind {
        WalletConnectDecodedCallKind::NativeTransfer => Some(PUBLIC_NATIVE_SEND_GAS_UNITS),
        WalletConnectDecodedCallKind::Erc20Approve { .. }
        | WalletConnectDecodedCallKind::Erc20Transfer { .. }
        | WalletConnectDecodedCallKind::Erc20TransferFrom { .. } => {
            Some(PUBLIC_ERC20_SEND_GAS_UNITS)
        }
        WalletConnectDecodedCallKind::WrappedDeposit => Some(PUBLIC_NATIVE_WRAP_GAS_UNITS),
        WalletConnectDecodedCallKind::WrappedWithdraw { .. } => {
            Some(PUBLIC_NATIVE_UNWRAP_GAS_UNITS)
        }
        WalletConnectDecodedCallKind::ContractCall { .. }
        | WalletConnectDecodedCallKind::ContractCreation => None,
    }
}

#[must_use]
pub fn public_walletconnect_operation_gas_limit(
    kind: &WalletConnectDecodedCallKind,
    gas_limit_buffer: u64,
) -> Option<u64> {
    public_native_action_gas_units_from_walletconnect_intent(kind)
        .map(|raw_gas| raw_gas.saturating_add(gas_limit_buffer))
}

#[must_use]
pub fn public_action_maximum_gas_cost_is_significant(
    expected_gas_cost: U256,
    maximum_gas_cost: U256,
) -> bool {
    if maximum_gas_cost <= expected_gas_cost {
        return false;
    }
    (maximum_gas_cost - expected_gas_cost)
        .checked_mul(U256::from(10_u8))
        .is_none_or(|scaled_difference| scaled_difference >= expected_gas_cost)
}

#[must_use]
pub fn public_shield_protocol_fee_amount(amount: U256) -> U256 {
    railgun_protocol_fee_amount(amount, RAILGUN_PROTOCOL_FEE_BPS)
}

pub(super) fn public_action_estimated_gas_units_with_buffer(
    model: &RailgunGasModel,
    kind: PublicActionKind,
    asset: PublicAssetId,
    profile: PublicShieldTransactionProfile,
    gas_limit_buffer: u64,
) -> Option<u64> {
    match kind {
        PublicActionKind::Send => {
            let gas_units = match asset {
                PublicAssetId::Native => PUBLIC_NATIVE_SEND_GAS_UNITS,
                PublicAssetId::Erc20(_) => PUBLIC_ERC20_SEND_GAS_UNITS,
            };
            Some(gas_units.saturating_add(gas_limit_buffer))
        }
        PublicActionKind::Wrap => {
            Some(PUBLIC_NATIVE_WRAP_GAS_UNITS.saturating_add(gas_limit_buffer))
        }
        PublicActionKind::Unwrap => {
            Some(PUBLIC_NATIVE_UNWRAP_GAS_UNITS.saturating_add(gas_limit_buffer))
        }
        PublicActionKind::Shield => {
            let (wrap, approve, shield) =
                public_shield_estimated_gas_units(model, asset, profile, gas_limit_buffer)?;
            Some(wrap.saturating_add(approve).saturating_add(shield))
        }
    }
}

/// The wrap, approval and Shield portions of the same model used for the displayed fee
/// budget; only the Railoxide native wrap fallback wraps. A native shield in Mimic Railway always
/// takes the stealth route, which has no model; its limits come from its preparation.
pub(in crate::public_wallet) fn public_shield_estimated_gas_units(
    model: &RailgunGasModel,
    asset: PublicAssetId,
    profile: PublicShieldTransactionProfile,
    gas_limit_buffer: u64,
) -> Option<(u64, u64, u64)> {
    let shield = public_shield_gas_units(model, GasEstimateMode::UpperBound);
    match (asset, profile) {
        (PublicAssetId::Native, PublicShieldTransactionProfile::Railway) => None,
        (PublicAssetId::Native, PublicShieldTransactionProfile::Railoxide) => Some((
            PUBLIC_NATIVE_WRAP_GAS_UNITS.saturating_add(gas_limit_buffer),
            PUBLIC_NATIVE_APPROVE_GAS_UNITS.saturating_add(gas_limit_buffer),
            shield.saturating_add(gas_limit_buffer),
        )),
        (PublicAssetId::Erc20(_), PublicShieldTransactionProfile::Railway) => Some((
            0,
            railway_gas_limit(PUBLIC_NATIVE_APPROVE_GAS_UNITS),
            railway_gas_limit(shield),
        )),
        (PublicAssetId::Erc20(_), PublicShieldTransactionProfile::Railoxide) => Some((
            0,
            PUBLIC_NATIVE_APPROVE_GAS_UNITS.saturating_add(gas_limit_buffer),
            shield.saturating_add(gas_limit_buffer),
        )),
    }
}

pub(in crate::public_wallet) const fn public_action_estimated_gas_usage_units(
    model: &RailgunGasModel,
    kind: PublicActionKind,
    asset: PublicAssetId,
) -> u64 {
    match kind {
        PublicActionKind::Send => match asset {
            PublicAssetId::Native => PUBLIC_NATIVE_SEND_GAS_UNITS,
            PublicAssetId::Erc20(_) => PUBLIC_ERC20_SEND_GAS_UNITS,
        },
        PublicActionKind::Wrap => PUBLIC_NATIVE_WRAP_GAS_UNITS,
        PublicActionKind::Unwrap => PUBLIC_NATIVE_UNWRAP_GAS_UNITS,
        PublicActionKind::Shield => {
            let approve_and_shield = PUBLIC_NATIVE_APPROVE_GAS_UNITS
                .saturating_add(public_shield_gas_units(model, GasEstimateMode::Expected));
            match asset {
                PublicAssetId::Native => {
                    PUBLIC_NATIVE_WRAP_GAS_UNITS.saturating_add(approve_and_shield)
                }
                PublicAssetId::Erc20(_) => approve_and_shield,
            }
        }
    }
}

#[must_use]
pub(super) fn public_native_action_gas_reserve_with_buffer(
    model: &RailgunGasModel,
    max_fee_per_gas: u128,
    steps: &[PublicActionProgressStep],
    gas_limit_buffer: u64,
) -> U256 {
    U256::from(public_native_action_gas_units_with_buffer(
        model,
        steps,
        gas_limit_buffer,
    )) * U256::from(max_fee_per_gas)
}

#[must_use]
pub(in crate::public_wallet) fn railway_gas_limit(estimated_gas: u64) -> u64 {
    let multiplied = u128::from(estimated_gas) * 120 / 100;
    multiplied.min(u128::from(u64::MAX)) as u64
}

/// A stealth-account native shield's buffered limit over its raw type-4 estimate: Mimic
/// Railway's 120%, or the chain's `gas_limit_buffer` in the Railoxide profile.
#[must_use]
pub(in crate::public_wallet) fn native_shield_gas_limit(
    profile: PublicShieldTransactionProfile,
    raw_gas_limit: u64,
    gas_limit_buffer: u64,
) -> u64 {
    match profile {
        PublicShieldTransactionProfile::Railway => railway_gas_limit(raw_gas_limit),
        PublicShieldTransactionProfile::Railoxide => raw_gas_limit.saturating_add(gas_limit_buffer),
    }
}

const fn public_native_step_gas_units(
    model: &RailgunGasModel,
    step: PublicActionProgressStep,
) -> u64 {
    match step {
        PublicActionProgressStep::Send => PUBLIC_NATIVE_SEND_GAS_UNITS,
        PublicActionProgressStep::Wrap => PUBLIC_NATIVE_WRAP_GAS_UNITS,
        PublicActionProgressStep::Unwrap => PUBLIC_NATIVE_UNWRAP_GAS_UNITS,
        PublicActionProgressStep::Approve => PUBLIC_NATIVE_APPROVE_GAS_UNITS,
        PublicActionProgressStep::Shield => {
            public_shield_gas_units(model, GasEstimateMode::UpperBound)
        }
        PublicActionProgressStep::ShieldKey
        | PublicActionProgressStep::Sponsor
        | PublicActionProgressStep::Unsponsor
        | PublicActionProgressStep::CallVote
        | PublicActionProgressStep::Vote
        | PublicActionProgressStep::GovernanceApprove
        | PublicActionProgressStep::Stake
        | PublicActionProgressStep::Delegate
        | PublicActionProgressStep::Undelegate
        | PublicActionProgressStep::Unlock
        | PublicActionProgressStep::PrincipalClaim
        | PublicActionProgressStep::RewardClaim(_) => 0,
    }
}
