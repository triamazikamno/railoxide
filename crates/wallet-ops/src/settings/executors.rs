use alloy::primitives::{Address, address};
use serde::{Deserialize, Serialize};

use super::EffectiveChainConfig;

/// The user-accepted current four-argument execute and signed multicall ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutorCapability {
    NonceBearingV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorProfile {
    chain_id: u64,
    delegate: Address,
}

impl ExecutorProfile {
    #[must_use]
    pub const fn chain_id(self) -> u64 {
        self.chain_id
    }
    #[must_use]
    pub const fn delegate(self) -> Address {
        self.delegate
    }
    #[must_use]
    pub const fn capability(self) -> ExecutorCapability {
        ExecutorCapability::NonceBearingV1
    }

    /// Historical records still require a supported profile before delegated recovery.
    #[must_use]
    pub fn accepted(chain_id: u64, delegate: Address) -> Option<Self> {
        let accepted = match chain_id {
            1 => address!("05ae73c5925d843864ae6f261f3175de2ebcd963"),
            56 | 137 | 42161 => address!("48cf4b897f64d81212c1423d78a05e828d0ce19d"),
            _ => return None,
        };
        (delegate == accepted).then_some(Self { chain_id, delegate })
    }
}

impl EffectiveChainConfig {
    /// Configuration admission only. Native ownership separately admits signer and delivery.
    #[must_use]
    pub fn accepted_executor_profile(&self) -> Option<ExecutorProfile> {
        if !self.enabled {
            return None;
        }
        ExecutorProfile::accepted(
            self.chain_id,
            self.railgun.as_ref()?.deployment.relay_adapt_7702_contract,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{WalletSettings, build_effective_chain_configs};

    #[test]
    fn effective_overrides_cannot_admit_an_unknown_executor_profile() {
        let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        for chain_id in [1, 56, 137, 42161] {
            let chain = chains.get_mut(chain_id).unwrap();
            let accepted = chain.accepted_executor_profile().unwrap();
            assert_eq!(accepted.chain_id(), chain_id);
            chain
                .railgun
                .as_mut()
                .unwrap()
                .deployment
                .relay_adapt_7702_contract = Address::repeat_byte(7);
            assert!(chain.accepted_executor_profile().is_none());
            chain
                .railgun
                .as_mut()
                .unwrap()
                .deployment
                .relay_adapt_7702_contract = accepted.delegate();
            chain.enabled = false;
            assert!(chain.accepted_executor_profile().is_none());
        }
        assert!(
            ExecutorProfile::accepted(10, address!("48cf4b897f64d81212c1423d78a05e828d0ce19d"))
                .is_none()
        );
    }
}
