use std::collections::BTreeMap;
use std::time::SystemTime;

use alloy::eips::BlockNumHash;
use alloy::primitives::U256;
use wallet_ops::{ExecutorAsset, ExecutorInspection};

#[derive(Clone, Copy)]
pub(super) struct BalanceValue {
    pub amount: U256,
    pub block: BlockNumHash,
    pub checked_at: SystemTime,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum CheckAttempt {
    #[default]
    NotChecked,
    Checking,
    Available,
    Unavailable,
    Stopped,
}

#[derive(Default)]
pub(super) struct BalanceObservation {
    pub value: Option<BalanceValue>,
    pub attempt: CheckAttempt,
    pub attempted_at: Option<SystemTime>,
}

#[derive(Default)]
pub(super) struct AccountObservations {
    pub assets: BTreeMap<ExecutorAsset, BalanceObservation>,
    pub inspection: Option<ExecutorInspection>,
}

impl AccountObservations {
    pub(super) fn begin(&mut self, assets: &[ExecutorAsset]) {
        for asset in assets {
            let balance = self.assets.entry(*asset).or_default();
            balance.attempt = CheckAttempt::Checking;
            balance.attempted_at = Some(SystemTime::now());
        }
    }

    pub(super) fn finish(&mut self, inspection: &ExecutorInspection, checked_at: SystemTime) {
        for (asset, amount) in inspection.balances() {
            self.merge(*asset, *amount, inspection.block(), checked_at);
        }
        self.inspection = Some(inspection.clone());
    }

    fn merge(
        &mut self,
        asset: ExecutorAsset,
        amount: Option<U256>,
        block: BlockNumHash,
        checked_at: SystemTime,
    ) {
        let balance = self.assets.entry(asset).or_default();
        balance.attempted_at = Some(checked_at);
        if let Some(amount) = amount {
            balance.value = Some(BalanceValue {
                amount,
                block,
                checked_at,
            });
            balance.attempt = CheckAttempt::Available;
        } else {
            balance.attempt = CheckAttempt::Unavailable;
        }
    }

    pub(super) fn fail(&mut self, assets: &[ExecutorAsset], attempted_at: SystemTime) {
        for asset in assets {
            let balance = self.assets.entry(*asset).or_default();
            balance.attempt = CheckAttempt::Unavailable;
            balance.attempted_at = Some(attempted_at);
        }
    }

    pub(super) fn stop(&mut self) {
        for balance in self.assets.values_mut() {
            if balance.attempt == CheckAttempt::Checking {
                balance.attempt = CheckAttempt::Stopped;
            }
        }
    }

    pub(super) fn holding(&self) -> bool {
        self.assets
            .values()
            .any(|balance| balance.value.is_some_and(|value| !value.amount.is_zero()))
    }

    /// Says nothing about unrequested assets. An unsuccessful latest attempt
    /// always keeps the row at full weight, even when an earlier check was zero.
    pub(super) fn checked_assets_zero(&self) -> bool {
        let mut checked = false;
        for balance in self.assets.values() {
            match balance.attempt {
                CheckAttempt::NotChecked => {}
                CheckAttempt::Available
                    if balance.value.is_some_and(|value| value.amount.is_zero()) =>
                {
                    checked = true;
                }
                _ => return false,
            }
        }
        checked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256};

    #[test]
    fn failed_and_narrow_checks_retain_dated_holdings_until_replaced() {
        let token = ExecutorAsset::Erc20(Address::repeat_byte(1));
        let time = SystemTime::UNIX_EPOCH;
        let block = BlockNumHash::new(12, B256::repeat_byte(12));
        let mut observations = AccountObservations::default();
        observations.merge(token, Some(U256::from(250)), block, time);
        observations.begin(&[ExecutorAsset::Native, token]);
        observations.merge(ExecutorAsset::Native, Some(U256::ZERO), block, time);
        observations.merge(token, None, block, time);
        assert!(observations.holding());
        assert!(!observations.checked_assets_zero());
        let previous = observations.assets[&token].value.unwrap();
        assert_eq!(
            (previous.amount, previous.block, previous.checked_at),
            (U256::from(250), block, time)
        );
        // A native-only retry cannot erase or resolve the unavailable token.
        observations.begin(&[ExecutorAsset::Native]);
        observations.fail(&[ExecutorAsset::Native], time);
        assert!(observations.holding());
        observations.begin(&[ExecutorAsset::Native, token]);
        observations.stop();
        assert!(observations.holding());
        let next = BlockNumHash::new(20, B256::repeat_byte(20));
        observations.merge(ExecutorAsset::Native, Some(U256::ZERO), next, time);
        observations.merge(token, Some(U256::ZERO), next, time);
        assert!(!observations.holding());
        assert!(observations.checked_assets_zero());
    }
}
