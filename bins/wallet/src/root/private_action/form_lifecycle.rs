use super::*;
use crate::root::actions::{PRIVATE_ACTION_FORM_KEY_CONTEXT, RefreshBroadcasterEstimate};
use crate::root::{ConfirmationDialogProps, confirmation_dialog};
use std::rc::Rc;
use ui::dialog::WalletDialogExt as _;

mod amount_inputs;
mod broadcaster_options;
mod dialog_open;
mod input_pins;
mod self_broadcast_gas;
mod send;
mod unshield;

const DOUBLE_PAYMENT_RISK: ConfirmationDialogProps = ConfirmationDialogProps::danger(
    "Earlier proof may still go through",
    "An earlier proof for this payment may still be executed by the broadcaster. Continuing may use different notes, so if that proof also goes through, the recipient could be paid twice.",
    None,
    "Continue anyway",
);

/// A change to one of a private form's public broadcaster filters.
#[derive(Clone, Copy)]
enum BroadcasterFilter {
    AllowSuspicious(bool),
    FavoritesOnly(bool),
}

impl BroadcasterFilter {
    /// Applies the change to `(allow_suspicious, favorites_only)`.
    const fn apply(self, (allow_suspicious, favorites_only): (bool, bool)) -> (bool, bool) {
        match self {
            Self::AllowSuspicious(allow) => (allow, favorites_only),
            Self::FavoritesOnly(enabled) => (allow_suspicious, enabled),
        }
    }
}

/// Opens or closes one of a private form's fee breakdowns.
#[derive(Clone, Copy)]
enum FeeBreakdownOpen {
    TransactionFee(bool),
    SponsoredFunding(bool),
}

impl FeeBreakdownOpen {
    /// Applies the change to the `(transaction_fee, sponsored_funding)` open flags and
    /// reports whether the flag changed.
    const fn apply(self, (transaction_fee, sponsored_funding): (&mut bool, &mut bool)) -> bool {
        let (flag, open) = match self {
            Self::TransactionFee(open) => (transaction_fee, open),
            Self::SponsoredFunding(open) => (sponsored_funding, open),
        };
        std::mem::replace(flag, open) != open
    }
}

#[cfg(test)]
mod tests;
