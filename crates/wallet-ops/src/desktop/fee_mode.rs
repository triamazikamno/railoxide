use super::*;
use eyre::eyre;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FeeHandlingMode {
    #[default]
    DeductFromAmount,
    AddToAmount,
}

pub(crate) fn railgun_protocol_gross_amount_for_recipient(
    recipient_amount: U256,
    fee_bps: U256,
) -> Result<U256> {
    if recipient_amount.is_zero() || fee_bps.is_zero() {
        return Ok(recipient_amount);
    }
    if fee_bps >= FEE_BASIS_POINTS_DENOMINATOR {
        return Err(eyre!("RAILGUN protocol fee must be below 100%"));
    }

    let net_bps = FEE_BASIS_POINTS_DENOMINATOR - fee_bps;
    Ok(
        ((recipient_amount - U256::from(1)) * FEE_BASIS_POINTS_DENOMINATOR / net_bps)
            + U256::from(1),
    )
}

pub fn unshield_receiver_amount_for_fee_mode(
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
) -> Result<U256> {
    match fee_mode {
        FeeHandlingMode::DeductFromAmount => Ok(entered_amount),
        FeeHandlingMode::AddToAmount => {
            railgun_protocol_gross_amount_for_recipient(entered_amount, RAILGUN_PROTOCOL_FEE_BPS)
        }
    }
}

pub fn unshield_protocol_fee_amount_for_fee_mode(
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
) -> Result<U256> {
    let gross_amount = unshield_receiver_amount_for_fee_mode(entered_amount, fee_mode)?;
    Ok(match fee_mode {
        FeeHandlingMode::DeductFromAmount => {
            railgun_protocol_fee_amount(gross_amount, RAILGUN_PROTOCOL_FEE_BPS)
        }
        FeeHandlingMode::AddToAmount => gross_amount.saturating_sub(entered_amount),
    })
}

pub(super) const fn recipient_amount_after_protocol_fee(
    amount: U256,
    protocol_fee_amount: U256,
) -> U256 {
    amount.saturating_sub(protocol_fee_amount)
}
