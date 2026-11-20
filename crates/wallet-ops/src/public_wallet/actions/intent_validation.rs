#[allow(clippy::wildcard_imports)]
use super::*;

/// A wrap or unwrap must call the chain's current wrapped native token, so a stale form can
/// never send to a contract the chain no longer names.
pub(super) fn validate_public_wrapped_native_target(
    intent: &PublicTransactionIntent,
    effective_chain: &EffectiveChainConfig,
) -> Result<()> {
    let (PublicTransactionIntent::WrapNative { wrapped, .. }
    | PublicTransactionIntent::UnwrapNative { wrapped, .. }) = intent
    else {
        return Ok(());
    };
    match effective_chain.wrapped_native_token {
        Some(configured) if configured == *wrapped => Ok(()),
        Some(_) => Err(eyre!(
            "wrapped native token changed; reopen the form to use the chain's current token"
        )),
        None => Err(eyre!("chain has no wrapped native token configured")),
    }
}

pub(in crate::public_wallet) fn validate_public_transaction_intent(
    intent: &PublicTransactionIntent,
) -> Result<()> {
    match intent {
        PublicTransactionIntent::Transfer { amount, .. }
        | PublicTransactionIntent::WrapNative { amount, .. }
        | PublicTransactionIntent::UnwrapNative { amount, .. }
            if amount.is_zero() =>
        {
            Err(eyre!("amount is required"))
        }
        PublicTransactionIntent::Raw { to: None, data, .. } if data.is_empty() => {
            Err(eyre!("contract creation requires non-empty init code"))
        }
        PublicTransactionIntent::Raw {
            to: Some(_),
            value,
            data,
        } if value.is_zero() && data.is_empty() => {
            Err(eyre!("contract call must include native value or data"))
        }
        _ => Ok(()),
    }
}
