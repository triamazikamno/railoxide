#[allow(clippy::wildcard_imports)]
use super::*;

pub(crate) const fn send_approximate_shape(
    selection: &railgun_wallet::tx::UnshieldSelectionInfo,
    max_receiver_amount: U256,
) -> ApproximateTransactionShape {
    ApproximateTransactionShape {
        transaction_count: selection.transaction_count,
        input_count: selection.input_count,
        private_output_count: selection.private_output_count,
        public_output_count: 0,
        max_receiver_amount,
        relay_call_count: 0,
        uses_relay_adapt: false,
        unwrap_count: 0,
        executor: false,
    }
}

pub(crate) const fn unshield_approximate_shape(
    selection: &railgun_wallet::tx::UnshieldSelectionInfo,
    max_receiver_amount: U256,
    unwrap: bool,
) -> ApproximateTransactionShape {
    ApproximateTransactionShape {
        transaction_count: selection.transaction_count,
        input_count: selection.input_count,
        private_output_count: selection.private_output_count,
        public_output_count: selection.public_output_count,
        max_receiver_amount,
        relay_call_count: if unwrap { 1 } else { 0 },
        uses_relay_adapt: unwrap,
        unwrap_count: if unwrap { 1 } else { 0 },
        executor: false,
    }
}

pub(crate) fn native_top_up_approximate_shape(
    utxos: &[Utxo],
    token: Address,
    fee_token: Address,
    receiver_amount: U256,
    fee_amount: U256,
    native_top_up: &DesktopNativeTopUpPlan,
) -> Result<ApproximateTransactionShape> {
    let wrapped_native = native_top_up.wrapped_native_token;
    if token == wrapped_native {
        let combined_wrapped_native_amount = native_top_up_required_wrapped_native_amount(
            token,
            wrapped_native,
            receiver_amount,
            native_top_up.native_amount,
        );
        let selection = native_top_up_selection_info_with_broadcaster_fee_seed(
            utxos,
            wrapped_native,
            fee_token,
            combined_wrapped_native_amount,
            fee_amount,
        )?;
        let max_combined_net = native_top_up_net_after_protocol_fee(selection.max_spendable);
        let max_receiver_amount = native_top_up_wrapped_native_amount_for_net(
            max_combined_net.saturating_sub(native_top_up.native_amount),
        );
        return Ok(ApproximateTransactionShape {
            transaction_count: selection.transaction_count,
            input_count: selection.input_count,
            private_output_count: selection.private_output_count,
            public_output_count: selection.public_output_count,
            max_receiver_amount,
            relay_call_count: 3,
            uses_relay_adapt: true,
            unwrap_count: 1,
            executor: false,
        });
    }

    let primary_selection = if fee_token == wrapped_native {
        unshield_selection_info(utxos, token, receiver_amount, false)?
    } else {
        native_top_up_selection_info_with_broadcaster_fee_seed(
            utxos,
            token,
            fee_token,
            receiver_amount,
            fee_amount,
        )?
    };
    let top_up_selection = if fee_token == wrapped_native {
        unshield_selection_info_with_broadcaster_fee_token(
            utxos,
            wrapped_native,
            wrapped_native,
            native_top_up.wrapped_native_amount,
            fee_amount,
            false,
        )?
    } else {
        unshield_selection_info(
            utxos,
            wrapped_native,
            native_top_up.wrapped_native_amount,
            false,
        )?
    };
    let transaction_count =
        primary_selection.transaction_count + top_up_selection.transaction_count;
    if transaction_count > MAX_BATCH_TRANSACTIONS {
        return Err(Report::new(BuildError::TooManyBatchTransactions {
            requested: transaction_count,
            max: MAX_BATCH_TRANSACTIONS,
        }));
    }

    Ok(ApproximateTransactionShape {
        transaction_count,
        input_count: primary_selection.input_count + top_up_selection.input_count,
        private_output_count: primary_selection.private_output_count
            + top_up_selection.private_output_count,
        public_output_count: primary_selection.public_output_count
            + top_up_selection.public_output_count,
        max_receiver_amount: primary_selection.max_spendable,
        relay_call_count: 2,
        uses_relay_adapt: true,
        unwrap_count: 1,
        executor: false,
    })
}

fn native_top_up_selection_info_with_broadcaster_fee_seed(
    utxos: &[Utxo],
    token: Address,
    fee_token: Address,
    amount: U256,
    fee_amount: U256,
) -> Result<railgun_wallet::tx::UnshieldSelectionInfo> {
    if fee_amount.is_zero() && token != fee_token {
        return unshield_selection_info_with_separate_broadcaster_fee_seed(
            utxos, token, fee_token, amount, false,
        )
        .map_err(Report::new);
    }

    unshield_selection_info_with_broadcaster_fee_token(
        utxos, token, fee_token, amount, fee_amount, false,
    )
    .map_err(Report::new)
}
