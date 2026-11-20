#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) const fn public_action_winner_gas_fee(
    attempts: &[SubmittedPublicActionAttempt],
    winner_index: usize,
) -> PublicActionGasFeeSelection {
    let winner = &attempts[winner_index];
    PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: winner.info.max_fee_per_gas,
        max_priority_fee_per_gas: winner.info.max_priority_fee_per_gas,
    }
}

pub(in crate::public_wallet) fn ensure_public_action_command_gas_fee_authorized(
    authorized_gas_fee: Option<PublicActionGasFeeSelection>,
    requested_gas_fee: PublicActionGasFeeSelection,
) -> Result<()> {
    if authorized_gas_fee.is_some_and(|authorized| authorized != requested_gas_fee) {
        return Err(eyre!(
            "advanced transaction fee changed after authorization; refresh the estimate and authorize again"
        ));
    }
    Ok(())
}

pub(in crate::public_wallet) fn public_action_step_initial_gas_fee_selection(
    profile: PublicShieldTransactionProfile,
    fee_policy: PublicActionStepFeePolicy,
    authorized_fee: PublicActionGasFeeSelection,
) -> PublicActionGasFeeSelection {
    if profile == PublicShieldTransactionProfile::Railway
        && fee_policy == PublicActionStepFeePolicy::RefreshRailwayStandard
    {
        PublicActionGasFeeSelection::Auto
    } else {
        authorized_fee
    }
}

pub(in crate::public_wallet) const fn railway_auto_fee_within_authorized_ceiling(
    chain_id: u64,
    asset: PublicAssetId,
    authorized_fee: PublicActionGasFeeSelection,
    resolved_fee: &SelfBroadcastResolvedGasFee,
) -> bool {
    let PublicActionGasFeeSelection::Custom {
        max_fee_per_gas,
        max_priority_fee_per_gas,
    } = authorized_fee
    else {
        return false;
    };
    resolved_fee.max_fee_per_gas <= max_fee_per_gas
        && (PublicShieldTransactionProfile::Railway.uses_legacy_envelope(chain_id, asset)
            || resolved_fee.max_priority_fee_per_gas <= max_priority_fee_per_gas)
}
