//! What a failed swap quote says in the form: a message in the user's terms, and the technical
//! text behind it when that holds more than the message.

use alloy::primitives::{Address, U256};
use wallet_ops::cow::OrderLimitError;
use wallet_ops::{SwapQuoteAnchor, SwapQuoteDiagnostics};

use super::bridge_unreachable;

const ANCHORS_NOT_LOADED: &str = "This quote's price for the Sell token can't be used, and the price anchors haven't loaded yet. They refresh over the chain's RPC. Try again shortly.";
const NO_ANCHOR: &str = "This quote's price for the Sell token can't be used, and the pair has no price anchor to use instead. A new quote may differ, so try again.";
const AMOUNT_MISSING: &str =
    "This quote is missing an amount the wallet needs. Try again for a new quote.";
const CANT_CALCULATE: &str = "The wallet can't calculate with these amounts. See Details.";

/// A failed quote as the form shows it.
pub(super) struct QuoteFailure {
    pub(super) message: String,
    /// The technical text behind the message. `None` when it would repeat the message, or when
    /// it names a request endpoint.
    pub(super) details: Option<String>,
}

/// The message and details of a failed quote. `cost` formats the estimated cost, an amount of
/// the Buy token, for a sell amount too small to pay it.
pub(super) fn map_quote_failure(
    error: &eyre::Report,
    cost: impl FnOnce(Address, U256) -> String,
) -> QuoteFailure {
    let diagnostics = error.downcast_ref::<SwapQuoteDiagnostics>();
    let message = match error.downcast_ref::<OrderLimitError>() {
        Some(OrderLimitError::HookCostExceedsOutput {
            buy_token,
            gas_estimate,
            ..
        }) => format!(
            "Sell amount is too small. Estimated cost: {}",
            cost(*buy_token, *gas_estimate)
        ),
        Some(
            OrderLimitError::QuotePriceNotDecimal
            | OrderLimitError::QuotePriceZero
            | OrderLimitError::QuotePriceOutOfRange,
        ) => {
            if matches!(
                diagnostics.map(|diagnostics| &diagnostics.anchor),
                Some(SwapQuoteAnchor::Unavailable(_))
            ) {
                ANCHORS_NOT_LOADED.to_owned()
            } else {
                NO_ANCHOR.to_owned()
            }
        }
        Some(OrderLimitError::QuoteSellAmountZero | OrderLimitError::InvalidQuoteGas) => {
            AMOUNT_MISSING.to_owned()
        }
        Some(
            OrderLimitError::Overflow
            | OrderLimitError::InvalidPriceTolerance
            | OrderLimitError::InvalidGasShare
            | OrderLimitError::ShieldFee(_),
        ) => CANT_CALCULATE.to_owned(),
        _ => {
            // The provider's error names the request endpoint, which the form doesn't show.
            if let Some(message) = bridge_unreachable(error) {
                return QuoteFailure {
                    message,
                    details: None,
                };
            }
            match diagnostics {
                // The diagnostics lines belong in the details, not in the message.
                Some(diagnostics) => cause(error, diagnostics),
                None => format!("{error:#}"),
            }
        }
    };
    let details = match diagnostics {
        Some(diagnostics) => format!("cause: {}\n{diagnostics}", cause(error, diagnostics)),
        None => format!("{error:#}"),
    };
    QuoteFailure {
        details: (details != message).then_some(details),
        message,
    }
}

/// The report's chain without its diagnostics, joined as `{error:#}` joins it. That format
/// would put the cause at the end of the last diagnostics line.
fn cause(error: &eyre::Report, diagnostics: &SwapQuoteDiagnostics) -> String {
    let diagnostics = diagnostics.to_string();
    error
        .chain()
        .map(ToString::to_string)
        .filter(|text| *text != diagnostics)
        .collect::<Vec<_>>()
        .join(": ")
}

#[cfg(test)]
mod tests {
    use wallet_ops::bridge::{BridgeApi, BridgeApiError};

    use super::*;

    fn diagnostics(anchor: SwapQuoteAnchor) -> SwapQuoteDiagnostics {
        SwapQuoteDiagnostics {
            chain_id: 56,
            sell_token: Address::repeat_byte(1),
            buy_token: Address::repeat_byte(2),
            sell_amount: U256::from(100),
            buy_amount: U256::from(99),
            sell_token_price: "abc".to_owned(),
            gas_amount: "163218".to_owned(),
            gas_price: "50000000".to_owned(),
            anchor,
        }
    }

    fn priced(error: OrderLimitError, anchor: SwapQuoteAnchor) -> eyre::Report {
        eyre::Report::from(error).wrap_err(diagnostics(anchor))
    }

    fn map(error: &eyre::Report) -> QuoteFailure {
        map_quote_failure(error, |token, amount| format!("{amount} of {token}"))
    }

    #[test]
    fn each_pricing_cause_has_its_message_and_the_quote_values_as_details() {
        let unavailable = || SwapQuoteAnchor::Unavailable("no cached rate".to_owned());
        let too_small = OrderLimitError::HookCostExceedsOutput {
            buy_token: Address::repeat_byte(2),
            gas_estimate: U256::from(60),
            best_case: U256::from(49),
        };
        let cost = format!(
            "Sell amount is too small. Estimated cost: 60 of {}",
            Address::repeat_byte(2)
        );
        for (error, anchor, message) in [
            (too_small, SwapQuoteAnchor::NotConfigured, cost.as_str()),
            (
                OrderLimitError::QuotePriceNotDecimal,
                unavailable(),
                ANCHORS_NOT_LOADED,
            ),
            (
                OrderLimitError::QuotePriceZero,
                unavailable(),
                ANCHORS_NOT_LOADED,
            ),
            (
                OrderLimitError::QuotePriceNotDecimal,
                SwapQuoteAnchor::NotConfigured,
                NO_ANCHOR,
            ),
            (
                OrderLimitError::QuotePriceOutOfRange,
                SwapQuoteAnchor::NotConfigured,
                NO_ANCHOR,
            ),
            (
                OrderLimitError::QuoteSellAmountZero,
                SwapQuoteAnchor::NotConfigured,
                AMOUNT_MISSING,
            ),
            (
                OrderLimitError::InvalidQuoteGas,
                SwapQuoteAnchor::NotConfigured,
                AMOUNT_MISSING,
            ),
            (
                OrderLimitError::Overflow,
                SwapQuoteAnchor::Verified,
                CANT_CALCULATE,
            ),
            (
                OrderLimitError::InvalidPriceTolerance,
                SwapQuoteAnchor::Verified,
                CANT_CALCULATE,
            ),
            (
                OrderLimitError::InvalidGasShare,
                SwapQuoteAnchor::Verified,
                CANT_CALCULATE,
            ),
        ] {
            let values = diagnostics(anchor.clone());
            let failure = map(&priced(error, anchor));
            assert_eq!(failure.message, message, "{error:?}");
            assert_eq!(
                failure.details,
                Some(format!("cause: {error}\n{values}")),
                "{error:?}"
            );
        }
        // A price failure that reaches the form without the quote's values.
        let bare = map(&eyre::Report::from(OrderLimitError::QuotePriceZero));
        assert_eq!(bare.message, NO_ANCHOR);
        assert_eq!(
            bare.details,
            Some(OrderLimitError::QuotePriceZero.to_string())
        );
    }

    #[test]
    fn an_unreachable_provider_keeps_its_message_and_shows_no_endpoint() {
        for (error, message) in [
            (
                BridgeApiError::RateLimited {
                    api: BridgeApi::Across,
                },
                "Across is limiting requests right now. Retry in a minute.",
            ),
            (
                BridgeApiError::Timeout {
                    api: BridgeApi::Across,
                    operation: "quote",
                    endpoint: "https://example.invalid/quote".to_owned(),
                },
                "Couldn't reach Across through the swap's network route.",
            ),
            (
                BridgeApiError::Unavailable {
                    api: BridgeApi::NearIntents,
                    operation: "dry quote",
                    endpoint: "https://example.invalid/v0/quote".to_owned(),
                    status: 500,
                },
                "NEAR Intents is having a temporary problem. Retry in a few minutes.",
            ),
        ] {
            let failure = map(&eyre::Report::from(error).wrap_err("Could not quote the bridge"));
            assert_eq!(failure.message, message);
            assert_eq!(failure.details, None);
        }
    }

    #[test]
    fn one_click_outage_refusals_ask_for_a_retry_and_others_show_as_sent() {
        let rejection = |message: &str| {
            eyre::Report::from(BridgeApiError::Rejected {
                api: BridgeApi::NearIntents,
                operation: "dry quote",
                endpoint: "https://example.invalid/v0/quote".to_owned(),
                status: 400,
                code: "Bad Request".to_owned(),
                message: message.to_owned(),
            })
        };
        for (sent, shown, outage) in [
            (
                "Quoting for this pair is not available",
                "NEAR Intents can't quote this pair right now. This is usually temporary. Retry in a few minutes.",
                true,
            ),
            (
                "No liquidity available",
                "NEAR Intents has no liquidity for this pair right now. This is usually temporary. Retry in a few minutes.",
                true,
            ),
            (
                "Amount is too low for bridge, try at least 1000000",
                "NEAR Intents: Amount is too low for bridge, try at least 1000000",
                false,
            ),
        ] {
            let error = rejection(sent);
            assert_eq!(
                super::super::bridge_rejection(&error).as_deref(),
                Some(shown)
            );
            assert_eq!(super::super::near_intents_status(&error).is_some(), outage);
        }
        let unavailable = |api| {
            eyre::Report::from(BridgeApiError::Unavailable {
                api,
                operation: "quote",
                endpoint: "https://example.invalid/quote".to_owned(),
                status: 500,
            })
        };
        assert!(super::super::near_intents_status(&unavailable(BridgeApi::NearIntents)).is_some());
        assert!(super::super::near_intents_status(&unavailable(BridgeApi::Across)).is_none());
    }

    #[test]
    fn any_other_failure_shows_its_own_text_without_details() {
        // A sentence the wallet wrote is the whole failure.
        let sentence = map(&eyre::eyre!("Quoting stopped unexpectedly. Try again."));
        assert_eq!(sentence.message, "Quoting stopped unexpectedly. Try again.");
        assert_eq!(sentence.details, None);
        let chained = map(&eyre::eyre!("connection closed").wrap_err("The quote request failed"));
        assert_eq!(
            chained.message,
            "The quote request failed: connection closed"
        );
        assert_eq!(chained.details, None);
    }
}
