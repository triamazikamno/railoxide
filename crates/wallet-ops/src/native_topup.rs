use super::*;

pub const NATIVE_TOP_UP_ETHEREUM_AMOUNT: U256 = uint!(3_000_000_000_000_000_U256);
pub const NATIVE_TOP_UP_ARBITRUM_AMOUNT: U256 = uint!(500_000_000_000_000_U256);
pub const NATIVE_TOP_UP_POLYGON_AMOUNT: U256 = uint!(1_000_000_000_000_000_000_U256);
pub const NATIVE_TOP_UP_BSC_AMOUNT: U256 = uint!(5_000_000_000_000_000_U256);
pub const NATIVE_TOP_UP_BASE_AMOUNT: U256 = uint!(500_000_000_000_000_U256);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeTopUpPolicy {
    pub top_up_amount: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopNativeTopUpRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopNativeTopUpPlan {
    pub recipient: Address,
    pub wrapped_native_token: Address,
    pub native_amount: U256,
    pub wrapped_native_amount: U256,
}

#[must_use]
pub const fn native_top_up_policy_for_chain(chain_id: u64) -> Option<NativeTopUpPolicy> {
    match chain_id {
        1 => Some(NativeTopUpPolicy {
            top_up_amount: NATIVE_TOP_UP_ETHEREUM_AMOUNT,
        }),
        42161 => Some(NativeTopUpPolicy {
            top_up_amount: NATIVE_TOP_UP_ARBITRUM_AMOUNT,
        }),
        137 => Some(NativeTopUpPolicy {
            top_up_amount: NATIVE_TOP_UP_POLYGON_AMOUNT,
        }),
        56 => Some(NativeTopUpPolicy {
            top_up_amount: NATIVE_TOP_UP_BSC_AMOUNT,
        }),
        8453 => Some(NativeTopUpPolicy {
            top_up_amount: NATIVE_TOP_UP_BASE_AMOUNT,
        }),
        _ => None,
    }
}

#[must_use]
pub fn native_top_up_wrapped_native_amount(native_amount: U256) -> U256 {
    native_top_up_wrapped_native_amount_for_net(native_amount)
}

#[must_use]
pub fn native_top_up_required_wrapped_native_amount(
    selected_token: Address,
    wrapped_native_token: Address,
    selected_receiver_amount: U256,
    native_amount: U256,
) -> U256 {
    let wrapped_native_amount = native_top_up_wrapped_native_amount(native_amount);
    if selected_token == wrapped_native_token {
        let primary_net = native_top_up_net_after_protocol_fee(selected_receiver_amount);
        native_top_up_wrapped_native_amount_for_net(primary_net + native_amount)
    } else {
        wrapped_native_amount
    }
}

#[must_use]
pub fn native_top_up_required_wrapped_native_amount_for_fee_mode(
    selected_token: Address,
    wrapped_native_token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    native_amount: U256,
) -> U256 {
    let selected_receiver_amount = match fee_mode {
        FeeHandlingMode::DeductFromAmount => entered_amount,
        FeeHandlingMode::AddToAmount => native_top_up_wrapped_native_amount_for_net(entered_amount),
    };
    native_top_up_required_wrapped_native_amount(
        selected_token,
        wrapped_native_token,
        selected_receiver_amount,
        native_amount,
    )
}

pub fn sponsored_unshield_additional_wrapped_native_spend(
    selected_token: Address,
    wrapped_native_token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    native_top_up_amount: Option<U256>,
) -> Result<U256> {
    if let Some(native_amount) = native_top_up_amount {
        return Ok(native_top_up_required_wrapped_native_amount_for_fee_mode(
            selected_token,
            wrapped_native_token,
            entered_amount,
            fee_mode,
            native_amount,
        ));
    }
    if selected_token == wrapped_native_token {
        return unshield_receiver_amount_for_fee_mode(entered_amount, fee_mode);
    }
    Ok(U256::ZERO)
}

#[must_use]
fn native_top_up_primary_recipient_amount(
    selected_token: Address,
    wrapped_native_token: Address,
    selected_receiver_amount: U256,
    native_amount: U256,
) -> U256 {
    if selected_token == wrapped_native_token {
        let combined_wrapped_native_amount = native_top_up_required_wrapped_native_amount(
            selected_token,
            wrapped_native_token,
            selected_receiver_amount,
            native_amount,
        );
        return native_top_up_net_after_protocol_fee(combined_wrapped_native_amount)
            .saturating_sub(native_amount);
    }

    native_top_up_net_after_protocol_fee(selected_receiver_amount)
}

#[must_use]
pub fn native_top_up_primary_recipient_amount_for_fee_mode(
    selected_token: Address,
    wrapped_native_token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    native_amount: U256,
) -> U256 {
    let selected_receiver_amount = match fee_mode {
        FeeHandlingMode::DeductFromAmount => entered_amount,
        FeeHandlingMode::AddToAmount => native_top_up_wrapped_native_amount_for_net(entered_amount),
    };
    native_top_up_primary_recipient_amount(
        selected_token,
        wrapped_native_token,
        selected_receiver_amount,
        native_amount,
    )
}

#[must_use]
pub(crate) fn native_top_up_wrapped_native_amount_for_net(net_amount: U256) -> U256 {
    if net_amount.is_zero() {
        return U256::ZERO;
    }

    let net_bps = FEE_BASIS_POINTS_DENOMINATOR - RAILGUN_PROTOCOL_FEE_BPS;
    ((net_amount - U256::from(1)) * FEE_BASIS_POINTS_DENOMINATOR / net_bps) + U256::from(1)
}

#[must_use]
pub(crate) fn native_top_up_net_after_protocol_fee(wrapped_native_amount: U256) -> U256 {
    wrapped_native_amount.saturating_sub(
        wrapped_native_amount * RAILGUN_PROTOCOL_FEE_BPS / FEE_BASIS_POINTS_DENOMINATOR,
    )
}
