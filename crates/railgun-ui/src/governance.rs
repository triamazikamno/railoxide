//! Static registry of the RAILGUN governance contracts deployed per chain.

use alloy::primitives::{Address, address};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GovernanceContracts {
    pub governance_token: Address,
    pub voting: Address,
    pub voting_legacy: Option<Address>,
    pub staking: Address,
    pub governor_rewards: Address,
    pub reward_tokens: &'static [GovernanceRewardToken],
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GovernanceRewardToken {
    pub symbol: &'static str,
    pub token: Address,
}

const ETHEREUM_REWARD_TOKENS: &[GovernanceRewardToken] = &[
    GovernanceRewardToken {
        symbol: "DAI",
        token: address!("0x6B175474E89094C44Da98b954EedeAC495271d0F"),
    },
    GovernanceRewardToken {
        symbol: "RAIL",
        token: address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D"),
    },
    GovernanceRewardToken {
        symbol: "WETH",
        token: address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
    },
];

const BSC_REWARD_TOKENS: &[GovernanceRewardToken] = &[
    GovernanceRewardToken {
        symbol: "DAI",
        token: address!("0x1af3f329e8be154074d8769d1ffa4ee058b1dbc3"),
    },
    GovernanceRewardToken {
        symbol: "RAILBSC",
        token: address!("0x3F847b01d4d498a293e3197B186356039eCd737F"),
    },
    GovernanceRewardToken {
        symbol: "WBNB",
        token: address!("0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c"),
    },
];

const POLYGON_REWARD_TOKENS: &[GovernanceRewardToken] = &[
    GovernanceRewardToken {
        symbol: "DAI",
        token: address!("0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063"),
    },
    GovernanceRewardToken {
        symbol: "RAILPOLY",
        token: address!("0x92A9C92C215092720C731c96D4Ff508c831a714f"),
    },
    GovernanceRewardToken {
        symbol: "WMATIC",
        token: address!("0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270"),
    },
];

#[rustfmt::skip]
const GOVERNANCE_CONTRACTS: &[(u64, GovernanceContracts)] = &[
    (1, GovernanceContracts {
        governance_token: address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D"),
        voting: address!("0xc480F68A3dcC3EdD82134FAB45C14A0FcF1dA3CC"),
        voting_legacy: Some(address!("0xfc4B580C9bda2EEf4E94D9Fb4bcB1F7a61660cf9")),
        staking: address!("0xee6a649aa3766bd117e12c161726b693a1b2ee20"),
        governor_rewards: address!("0xA02782CE1bF85f56f8cC7C0E66e61299Ac75c86f"),
        reward_tokens: ETHEREUM_REWARD_TOKENS,
    }),
    (56, GovernanceContracts {
        governance_token: address!("0x3F847b01d4d498a293e3197B186356039eCd737F"),
        voting: address!("0xc3f2C8F9d5F0705De706b1302B7a039e1e11aC88"),
        voting_legacy: Some(address!("0x569C15b356D3bA9c9f407945b12C867f7C3608C9")),
        staking: address!("0x753f0F9BA003DDA95eb9284533Cf5B0F19e441dc"),
        governor_rewards: address!("0xa7A9582C2563a1b923dbff6a8A2fa625ee2FB1f8"),
        reward_tokens: BSC_REWARD_TOKENS,
    }),
    (137, GovernanceContracts {
        governance_token: address!("0x92A9C92C215092720C731c96D4Ff508c831a714f"),
        voting: address!("0xc3f2C8F9d5F0705De706b1302B7a039e1e11aC88"),
        voting_legacy: None,
        staking: address!("0x9AC2bA4bf7FaCB0bbB33447e5fF8f8D63B71dDC1"),
        governor_rewards: address!("0x25f795A8eC8aF7904aa403fF2Cc7205ce683BF52"),
        reward_tokens: POLYGON_REWARD_TOKENS,
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
            assert_ne!(contracts.governance_token, Address::ZERO);
            assert_ne!(contracts.voting, Address::ZERO);
            assert_ne!(contracts.staking, Address::ZERO);
            assert_ne!(contracts.governor_rewards, Address::ZERO);
            let mut reward_tokens = HashSet::new();
            for reward_token in contracts.reward_tokens {
                assert_ne!(reward_token.token, Address::ZERO);
                assert!(reward_tokens.insert(reward_token.token));
            }
            if let Some(voting_legacy) = contracts.voting_legacy {
                assert_ne!(voting_legacy, Address::ZERO);
            }
            assert_eq!(governance_contracts(chain_id), Some(contracts));
        }

        assert_eq!(governance_contracts(u64::MAX), None);
    }

    #[test]
    fn registry_contains_the_deployed_metadata() {
        fn assert_entry(
            chain_id: u64,
            governance_token: Address,
            voting: Address,
            voting_legacy: Option<Address>,
            staking: Address,
            governor_rewards: Address,
            reward_tokens: [(&str, Address); 3],
        ) {
            let contracts = governance_contracts(chain_id).expect("supported deployment");
            assert_eq!(contracts.governance_token, governance_token);
            assert_eq!(contracts.voting, voting);
            assert_eq!(contracts.voting_legacy, voting_legacy);
            assert_eq!(contracts.staking, staking);
            assert_eq!(contracts.governor_rewards, governor_rewards);
            assert_eq!(
                contracts
                    .reward_tokens
                    .iter()
                    .map(|token| (token.symbol, token.token))
                    .collect::<Vec<_>>(),
                reward_tokens
            );
        }

        assert_entry(
            1,
            address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D"),
            address!("0xc480F68A3dcC3EdD82134FAB45C14A0FcF1dA3CC"),
            Some(address!("0xfc4B580C9bda2EEf4E94D9Fb4bcB1F7a61660cf9")),
            address!("0xee6a649aa3766bd117e12c161726b693a1b2ee20"),
            address!("0xA02782CE1bF85f56f8cC7C0E66e61299Ac75c86f"),
            [
                (
                    "DAI",
                    address!("0x6B175474E89094C44Da98b954EedeAC495271d0F"),
                ),
                (
                    "RAIL",
                    address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D"),
                ),
                (
                    "WETH",
                    address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
                ),
            ],
        );
        assert_entry(
            56,
            address!("0x3F847b01d4d498a293e3197B186356039eCd737F"),
            address!("0xc3f2C8F9d5F0705De706b1302B7a039e1e11aC88"),
            Some(address!("0x569C15b356D3bA9c9f407945b12C867f7C3608C9")),
            address!("0x753f0F9BA003DDA95eb9284533Cf5B0F19e441dc"),
            address!("0xa7A9582C2563a1b923dbff6a8A2fa625ee2FB1f8"),
            [
                (
                    "DAI",
                    address!("0x1af3f329e8be154074d8769d1ffa4ee058b1dbc3"),
                ),
                (
                    "RAILBSC",
                    address!("0x3F847b01d4d498a293e3197B186356039eCd737F"),
                ),
                (
                    "WBNB",
                    address!("0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c"),
                ),
            ],
        );
        assert_entry(
            137,
            address!("0x92A9C92C215092720C731c96D4Ff508c831a714f"),
            address!("0xc3f2C8F9d5F0705De706b1302B7a039e1e11aC88"),
            None,
            address!("0x9AC2bA4bf7FaCB0bbB33447e5fF8f8D63B71dDC1"),
            address!("0x25f795A8eC8aF7904aa403fF2Cc7205ce683BF52"),
            [
                (
                    "DAI",
                    address!("0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063"),
                ),
                (
                    "RAILPOLY",
                    address!("0x92A9C92C215092720C731c96D4Ff508c831a714f"),
                ),
                (
                    "WMATIC",
                    address!("0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270"),
                ),
            ],
        );
    }
}
