//! Static registry of the RAILGUN governance contracts deployed per chain.

use alloy::primitives::{Address, address};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GovernanceContracts {
    pub voting: Address,
    pub voting_legacy: Option<Address>,
    pub staking: Address,
}

#[rustfmt::skip]
const GOVERNANCE_CONTRACTS: &[(u64, GovernanceContracts)] = &[
    (1, GovernanceContracts {
        voting: address!("0xc480F68A3dcC3EdD82134FAB45C14A0FcF1dA3CC"),
        voting_legacy: Some(address!("0xfc4B580C9bda2EEf4E94D9Fb4bcB1F7a61660cf9")),
        staking: address!("0xee6a649aa3766bd117e12c161726b693a1b2ee20"),
    }),
    (56, GovernanceContracts {
        voting: address!("0xc3f2C8F9d5F0705De706b1302B7a039e1e11aC88"),
        voting_legacy: Some(address!("0x569C15b356D3bA9c9f407945b12C867f7C3608C9")),
        staking: address!("0x753f0F9BA003DDA95eb9284533Cf5B0F19e441dc"),
    }),
    (137, GovernanceContracts {
        voting: address!("0xc3f2C8F9d5F0705De706b1302B7a039e1e11aC88"),
        voting_legacy: None,
        staking: address!("0x9AC2bA4bf7FaCB0bbB33447e5fF8f8D63B71dDC1"),
    }),
];

#[must_use]
pub fn governance_contracts(chain_id: u64) -> Option<GovernanceContracts> {
    GOVERNANCE_CONTRACTS
        .iter()
        .find(|(deployed_chain_id, _)| *deployed_chain_id == chain_id)
        .map(|(_, contracts)| *contracts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registry_entries_are_unique_nonzero_and_lookupable() {
        let mut chain_ids = HashSet::new();

        for &(chain_id, contracts) in GOVERNANCE_CONTRACTS {
            assert!(chain_ids.insert(chain_id), "duplicate chain ID: {chain_id}");
            assert_ne!(contracts.voting, Address::ZERO);
            assert_ne!(contracts.staking, Address::ZERO);
            if let Some(voting_legacy) = contracts.voting_legacy {
                assert_ne!(voting_legacy, Address::ZERO);
            }
            assert_eq!(governance_contracts(chain_id), Some(contracts));
        }

        assert_eq!(governance_contracts(u64::MAX), None);
    }
}
