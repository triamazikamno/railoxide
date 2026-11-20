#[allow(clippy::wildcard_imports)]
use super::*;

pub(crate) fn public_broadcaster_amount_split(
    entered_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
) -> Result<PublicBroadcasterAmountSplit> {
    let (receiver_amount, total_private_spend) = match fee_mode {
        FeeHandlingMode::DeductFromAmount => {
            if entered_amount <= fee_amount {
                return Err(eyre!(
                    "entered amount must be greater than the broadcaster fee"
                ));
            }
            (entered_amount - fee_amount, entered_amount)
        }
        FeeHandlingMode::AddToAmount => (
            entered_amount,
            entered_amount
                .checked_add(fee_amount)
                .ok_or_else(|| eyre!("amount plus transaction fee exceeds the supported range"))?,
        ),
    };
    Ok(PublicBroadcasterAmountSplit {
        entered_amount,
        receiver_amount,
        total_private_spend,
        fee_amount,
        fee_mode,
    })
}

pub(crate) fn public_broadcaster_amount_split_for_tokens(
    entered_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
) -> Result<PublicBroadcasterAmountSplit> {
    public_broadcaster_amount_split_for_tokens_and_protocol(
        entered_amount,
        fee_amount,
        fee_mode,
        same_token_fee,
        U256::ZERO,
    )
}

pub(crate) fn public_broadcaster_amount_split_for_tokens_and_protocol(
    entered_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
    protocol_fee_bps: U256,
) -> Result<PublicBroadcasterAmountSplit> {
    if same_token_fee && protocol_fee_bps.is_zero() {
        return public_broadcaster_amount_split(entered_amount, fee_amount, fee_mode);
    }

    let receiver_amount = match fee_mode {
        FeeHandlingMode::DeductFromAmount => {
            if same_token_fee {
                if entered_amount <= fee_amount {
                    return Err(eyre!(
                        "entered amount must be greater than the broadcaster fee"
                    ));
                }
                entered_amount - fee_amount
            } else {
                entered_amount
            }
        }
        FeeHandlingMode::AddToAmount => {
            railgun_protocol_gross_amount_for_recipient(entered_amount, protocol_fee_bps)?
        }
    };
    let total_private_spend = if same_token_fee {
        receiver_amount
            .checked_add(fee_amount)
            .ok_or_else(|| eyre!("amount plus transaction fee exceeds the supported range"))?
    } else {
        receiver_amount
    };

    Ok(PublicBroadcasterAmountSplit {
        entered_amount,
        receiver_amount,
        total_private_spend,
        fee_amount,
        fee_mode,
    })
}

pub(crate) fn public_broadcaster_max_entered_amount(
    max_receiver_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
) -> U256 {
    match fee_mode {
        FeeHandlingMode::DeductFromAmount => max_receiver_amount + fee_amount,
        FeeHandlingMode::AddToAmount => max_receiver_amount,
    }
}

#[cfg(test)]
pub(crate) fn public_broadcaster_max_entered_amount_for_tokens(
    max_receiver_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
) -> U256 {
    public_broadcaster_max_entered_amount_for_tokens_and_protocol(
        max_receiver_amount,
        fee_amount,
        fee_mode,
        same_token_fee,
        U256::ZERO,
    )
}

pub(crate) fn public_broadcaster_max_entered_amount_for_tokens_and_protocol(
    max_receiver_amount: U256,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
    protocol_fee_bps: U256,
) -> U256 {
    match fee_mode {
        FeeHandlingMode::DeductFromAmount => {
            if same_token_fee {
                public_broadcaster_max_entered_amount(max_receiver_amount, fee_amount, fee_mode)
            } else {
                max_receiver_amount
            }
        }
        FeeHandlingMode::AddToAmount => recipient_amount_after_protocol_fee(
            max_receiver_amount,
            railgun_protocol_fee_amount(max_receiver_amount, protocol_fee_bps),
        ),
    }
}

pub(crate) fn public_broadcaster_reported_amounts(
    action_token: Address,
    fee_token: Address,
    split: PublicBroadcasterAmountSplit,
    protocol_fee_bps: U256,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
) -> PublicBroadcasterReportedAmounts {
    if let Some(native_top_up) = native_top_up
        && action_token == native_top_up.wrapped_native_token
    {
        let combined_wrapped_native_amount = native_top_up_required_wrapped_native_amount(
            action_token,
            native_top_up.wrapped_native_token,
            split.receiver_amount,
            native_top_up.native_amount,
        );
        let protocol_fee_amount =
            railgun_protocol_fee_amount(combined_wrapped_native_amount, protocol_fee_bps);
        let recipient_amount = recipient_amount_after_protocol_fee(
            combined_wrapped_native_amount,
            protocol_fee_amount,
        )
        .saturating_sub(native_top_up.native_amount);
        let fee_spend = if action_token == fee_token {
            split.fee_amount
        } else {
            U256::ZERO
        };
        return PublicBroadcasterReportedAmounts {
            recipient_amount,
            total_private_spend: combined_wrapped_native_amount + fee_spend,
            protocol_fee_amount,
        };
    }

    let protocol_fee_amount = railgun_protocol_fee_amount(split.receiver_amount, protocol_fee_bps);
    PublicBroadcasterReportedAmounts {
        recipient_amount: recipient_amount_after_protocol_fee(
            split.receiver_amount,
            protocol_fee_amount,
        ),
        total_private_spend: split.total_private_spend,
        protocol_fee_amount,
    }
}
