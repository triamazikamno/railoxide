//! Tron Send fees, in sun. Arithmetic saturates, which can only raise a fee
//! or deny a resource, so an overflow never understates what a send can burn.

use super::transaction::UnsignedTransaction;

/// The fixed allowance Tron adds to a transaction's size for its result when charging bandwidth.
pub const RESULT_ALLOWANCE_BYTES: u64 = 64;

/// Chain parameters, read at signing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainFeeParameters {
    /// `getTransactionFee`: sun per bandwidth byte.
    pub bandwidth_price: u64,
    /// `getEnergyFee`: sun per energy unit.
    pub energy_price: u64,
    /// `getCreateAccountFee`: the 0.1 TRX that staked bandwidth can waive.
    pub create_account_fee: u64,
    /// `getCreateNewAccountFeeInSystemContract`: the 1 TRX always charged.
    pub create_account_system_fee: u64,
    /// `getCreateNewAccountBandwidthRate`: staked bandwidth per byte that waives the 0.1 TRX.
    pub create_account_bandwidth_rate: u64,
}

/// The sender's available resources.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountResources {
    pub free_bandwidth: u64,
    /// Staked or delegated bandwidth.
    pub staked_bandwidth: u64,
    pub energy: u64,
}

/// What the transaction needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeInputs {
    /// See [`bandwidth_bytes`].
    pub bandwidth_bytes: u64,
    /// `eth_estimateGas`; zero for a TRX send.
    pub energy_estimate: u64,
    /// A TRX send to an address that doesn't exist on chain.
    pub creates_account: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TronFees {
    /// The energy estimate plus 20%, rounded up.
    pub energy_cap: u64,
    /// A TRC-20 transfer's `fee_limit`: the energy cap at the energy price.
    pub fee_limit: u64,
    /// What the transaction can burn whatever resources the account holds.
    pub maximum: u64,
    /// What it burns with the account's current resources.
    pub expected: u64,
}

/// The bandwidth a transaction is charged for: its signed size with `ret` cleared, plus
/// [`RESULT_ALLOWANCE_BYTES`].
#[must_use]
pub fn bandwidth_bytes(transaction: &UnsignedTransaction) -> u64 {
    (transaction.signed_len() as u64).saturating_add(RESULT_ALLOWANCE_BYTES)
}

#[must_use]
pub const fn energy_cap(estimate: u64) -> u64 {
    estimate.saturating_add(estimate.div_ceil(5))
}

/// The `fee_limit` set on a TRC-20 transaction, in sun.
#[must_use]
pub const fn fee_limit_sun(energy_estimate: u64, energy_price: u64) -> u64 {
    energy_cap(energy_estimate).saturating_mul(energy_price)
}

#[must_use]
pub const fn tron_fees(
    parameters: &ChainFeeParameters,
    resources: &AccountResources,
    inputs: &FeeInputs,
) -> TronFees {
    let energy_cap = energy_cap(inputs.energy_estimate);
    let fee_limit = fee_limit_sun(inputs.energy_estimate, parameters.energy_price);
    let expected_energy = energy_cap
        .saturating_sub(resources.energy)
        .saturating_mul(parameters.energy_price);
    let (maximum_bandwidth, expected_bandwidth) = if inputs.creates_account {
        // Account creation replaces the per-byte charge, and only staked bandwidth can waive
        // the creation fee.
        let staked_covers = resources.staked_bandwidth
            >= inputs
                .bandwidth_bytes
                .saturating_mul(parameters.create_account_bandwidth_rate);
        let maximum = parameters
            .create_account_system_fee
            .saturating_add(parameters.create_account_fee);
        let expected = if staked_covers {
            parameters.create_account_system_fee
        } else {
            maximum
        };
        (maximum, expected)
    } else {
        let maximum = inputs
            .bandwidth_bytes
            .saturating_mul(parameters.bandwidth_price);
        // Tron takes bandwidth from one source, so each must cover the whole transaction.
        let covered = resources.staked_bandwidth >= inputs.bandwidth_bytes
            || resources.free_bandwidth >= inputs.bandwidth_bytes;
        (maximum, if covered { 0 } else { maximum })
    };
    TronFees {
        energy_cap,
        fee_limit,
        maximum: maximum_bandwidth.saturating_add(fee_limit),
        expected: expected_bandwidth.saturating_add(expected_energy),
    }
}

/// The TRX Review is missing: the amount sent (zero for a token send) plus the expected fee,
/// less the balance. `None` when the balance covers both.
#[must_use]
pub const fn missing_trx(balance: u64, trx_amount: u64, expected_fee: u64) -> Option<u64> {
    match trx_amount.saturating_add(expected_fee).checked_sub(balance) {
        Some(0) | None => None,
        missing => missing,
    }
}

/// Max on TRX: the balance less the maximum fee.
#[must_use]
pub const fn max_trx_amount(balance: u64, maximum_fee: u64) -> u64 {
    balance.saturating_sub(maximum_fee)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRX: u64 = 1_000_000;
    const PARAMETERS: ChainFeeParameters = ChainFeeParameters {
        bandwidth_price: 1_000,
        energy_price: 100,
        create_account_fee: TRX / 10,
        create_account_system_fee: TRX,
        create_account_bandwidth_rate: 1,
    };
    const USDT_SEND: FeeInputs = FeeInputs {
        bandwidth_bytes: 345,
        energy_estimate: 65_000,
        creates_account: false,
    };
    // 345 bytes at 1000 sun, plus 78 000 energy (65 000 + 20%) at 100 sun.
    const USDT_MAXIMUM: u64 = 345_000 + 7_800_000;
    const NEW_ACCOUNT_TRX_SEND: FeeInputs = FeeInputs {
        bandwidth_bytes: 267,
        energy_estimate: 0,
        creates_account: true,
    };

    #[test]
    fn usdt_send_without_resources_expects_the_maximum() {
        let fees = tron_fees(&PARAMETERS, &AccountResources::default(), &USDT_SEND);
        assert_eq!(fees.energy_cap, 78_000);
        assert_eq!(fees.maximum, USDT_MAXIMUM);
        assert_eq!(fees.expected, fees.maximum);
        assert_eq!(energy_cap(65_001), 78_002);
    }

    #[test]
    fn fully_resourced_usdt_send_needs_no_trx_and_keeps_the_full_fee_limit() {
        let resources = AccountResources {
            free_bandwidth: 0,
            staked_bandwidth: 345,
            energy: 78_000,
        };
        let fees = tron_fees(&PARAMETERS, &resources, &USDT_SEND);
        assert_eq!(fees.expected, 0);
        assert_eq!(missing_trx(0, 0, fees.expected), None);
        assert_eq!(fees.fee_limit, 7_800_000);
        assert_eq!(fees.maximum, USDT_MAXIMUM);
    }

    #[test]
    fn bandwidth_sources_do_not_combine() {
        let resources = AccountResources {
            free_bandwidth: 300,
            staked_bandwidth: 300,
            energy: 78_000,
        };
        let fees = tron_fees(&PARAMETERS, &resources, &USDT_SEND);
        assert_eq!(fees.expected, 345_000);
    }

    #[test]
    fn trx_to_a_new_address_with_exactly_the_amount_plus_fee() {
        let amount = 5 * TRX;
        let cases = [
            (
                "staked",
                AccountResources {
                    staked_bandwidth: 267,
                    ..Default::default()
                },
                TRX,
            ),
            ("none", AccountResources::default(), TRX + TRX / 10),
            (
                "free only",
                AccountResources {
                    free_bandwidth: 5_000,
                    ..Default::default()
                },
                TRX + TRX / 10,
            ),
        ];
        for (name, resources, expected) in cases {
            let fees = tron_fees(&PARAMETERS, &resources, &NEW_ACCOUNT_TRX_SEND);
            assert_eq!(fees.maximum, TRX + TRX / 10, "{name}");
            assert_eq!(fees.expected, expected, "{name}");
            assert_eq!(
                missing_trx(amount + expected, amount, fees.expected),
                None,
                "{name}"
            );
            assert_eq!(
                missing_trx(amount + expected - 1, amount, fees.expected),
                Some(1),
                "{name}"
            );
        }
    }

    #[test]
    fn insufficient_trx_reports_the_missing_amount() {
        let fees = tron_fees(&PARAMETERS, &AccountResources::default(), &USDT_SEND);
        assert_eq!(missing_trx(TRX, 0, fees.expected), Some(USDT_MAXIMUM - TRX));
    }

    #[test]
    fn max_on_trx_leaves_the_maximum_fee() {
        let existing = FeeInputs {
            creates_account: false,
            ..NEW_ACCOUNT_TRX_SEND
        };
        let fees = tron_fees(&PARAMETERS, &AccountResources::default(), &existing);
        assert_eq!(max_trx_amount(10 * TRX, fees.maximum), 10 * TRX - 267_000);
        let fees = tron_fees(
            &PARAMETERS,
            &AccountResources::default(),
            &NEW_ACCOUNT_TRX_SEND,
        );
        assert_eq!(max_trx_amount(TRX, fees.maximum), 0);
    }
}
