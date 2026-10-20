use alloy::primitives::Address;
pub use broadcaster_core::contracts::executor::ExecutorCapability;

use super::{EffectiveChainConfig, RailgunDeployment};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorProfile {
    chain_id: u64,
    delegate: Address,
    capability: ExecutorCapability,
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
        self.capability
    }

    /// The built-in listed delegate `delegate` on `chain_id`, with the capability its entry
    /// records. Historical records still require one before delegated recovery.
    #[must_use]
    pub fn accepted(chain_id: u64, delegate: Address) -> Option<Self> {
        RailgunDeployment::for_chain(chain_id)?
            .relay_adapt_7702_delegates
            .iter()
            .find(|listed| listed.address == delegate)
            .map(|listed| Self {
                chain_id,
                delegate,
                capability: listed.capability,
            })
    }

    /// The listed delegate `delegate` on this profile's chain when it has this profile's
    /// capability. An executor account moves between such delegates keeping its nonce and payloads.
    #[must_use]
    pub fn interchangeable(self, delegate: Address) -> Option<Self> {
        Self::accepted(self.chain_id, delegate).filter(|other| other.capability == self.capability)
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
