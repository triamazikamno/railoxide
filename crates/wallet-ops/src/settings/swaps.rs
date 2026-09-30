use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::{Address, address};

use super::{EffectiveChainConfig, EffectiveTokenInfo, EffectiveTokenRegistry};
use crate::FreshAnchorParams;
use crate::vault::SwapDelivery;

/// Built-in private swap parameters for one chain. Not user-editable and not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapProfile {
    chain_id: u64,
    settlement: Address,
    vault_relayer: Address,
    hooks_trampoline: Address,
    deadline_guard: Address,
    orderbook_api_base: &'static str,
    app_code: &'static str,
    app_data_byte_budget: usize,
    valid_to_window: Duration,
    chainlink_max_age: Duration,
    chainlink_max_age_overrides: &'static [(Address, Duration)],
    max_head_age: Duration,
    anchor_deviation_bps: u32,
}

/// Why a token or pair can't be swapped under a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapIneligibility {
    NativeAsset,
    /// Pair-only: the sell and buy tokens are the same.
    SameToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapTokenEligibility {
    Eligible,
    Ineligible(SwapIneligibility),
}

/// A token's side in a swap. The Buy side depends on delivery: only an External order pays
/// its receiver directly, so only it can buy the native asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapTokenRole {
    Sell,
    Buy(SwapDelivery),
}

/// Why an External swap can't pay its proceeds to a receiver: they would be lost or stranded
/// in a contract of the swap itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SwapReceiverRejection {
    #[error("the zero address can't receive the swap; the proceeds would be lost")]
    ZeroAddress,
    #[error("the swap's own stealth account can't be the receiver")]
    Executor,
    #[error("the Railgun contract can't be the receiver; use Private balance instead")]
    Railgun,
    #[error("CoW's settlement contract can't be the receiver; the proceeds would be stranded")]
    Settlement,
    #[error("CoW's vault relayer can't be the receiver; the proceeds would be stranded")]
    VaultRelayer,
    #[error("CoW's hooks trampoline can't be the receiver; the proceeds would be stranded")]
    HooksTrampoline,
}

const MAINNET_SWAP_PROFILE: SwapProfile = SwapProfile {
    chain_id: 1,
    settlement: address!("9008D19f58AAbD9eD0D60971565AA8510560ab41"),
    vault_relayer: address!("C92E8bdf79f0507f65a392b0ab4667716BFE0110"),
    hooks_trampoline: address!("60Bf78233f48eC42eE3F101b9a05eC7878728006"),
    // Uniswap SwapRouter02.
    deadline_guard: address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"),
    orderbook_api_base: "https://api.cow.fi/mainnet",
    app_code: "swap",
    // `POST /api/v1/orders` rejects bodies over 16 KiB, and the order carries
    // the full app data as an escaped string. Leave room for the order fields.
    app_data_byte_budget: 14_336,
    valid_to_window: Duration::from_mins(10),
    chainlink_max_age: Duration::from_hours(25),
    chainlink_max_age_overrides: &[],
    max_head_age: Duration::from_mins(1),
    anchor_deviation_bps: 300,
};

// CoW's core and HooksTrampoline deployments use the same addresses on these chains.
// SwapRouter02 supplies the deadline guard; BNB has a different deployment.
const BNB_SWAP_PROFILE: SwapProfile = SwapProfile {
    chain_id: 56,
    deadline_guard: address!("B971eF87ede563556b2ED4b1C0b0019111Dd85d2"),
    orderbook_api_base: "https://api.cow.fi/bnb",
    ..MAINNET_SWAP_PROFILE
};
const POLYGON_SWAP_PROFILE: SwapProfile = SwapProfile {
    chain_id: 137,
    orderbook_api_base: "https://api.cow.fi/polygon",
    ..MAINNET_SWAP_PROFILE
};
const ARBITRUM_SWAP_PROFILE: SwapProfile = SwapProfile {
    chain_id: 42161,
    orderbook_api_base: "https://api.cow.fi/arbitrum_one",
    ..MAINNET_SWAP_PROFILE
};

/// Built-in cross-chain delivery parameters for one chain: the bridge providers' API roots,
/// Across's `SpokePool` and 1Click's pinned quote-signing key. Not user-editable and not
/// persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeProfile {
    chain_id: u64,
    across_api_base: &'static str,
    spoke_pool: Address,
    one_click_api_base: &'static str,
    one_click_blockchain: &'static str,
    one_click_quote_key: &'static str,
}

// SpokePools are the Across proxies whose implementations contain `depositV3`, checked on-chain
// on 2026-09-29.
const MAINNET_BRIDGE_PROFILE: BridgeProfile = BridgeProfile {
    chain_id: 1,
    across_api_base: "https://app.across.to/api",
    spoke_pool: address!("5c7BCd6E7De5423a257D81B442095A1a6ced35C5"),
    one_click_api_base: "https://1click.chaindefuser.com",
    one_click_blockchain: "eth",
    // `ONE_CLICK_MANAGER_PUB_KEY` in 1Click's TypeScript SDK 0.1.26.
    one_click_quote_key: "ed25519:reYaWhvwu8Jzo3WUM3zhn6VrhuMEF4eADL17qtRVifc",
};
const BNB_BRIDGE_PROFILE: BridgeProfile = BridgeProfile {
    chain_id: 56,
    spoke_pool: address!("4e8E101924eDE233C13e2D8622DC8aED2872d505"),
    one_click_blockchain: "bsc",
    ..MAINNET_BRIDGE_PROFILE
};
const POLYGON_BRIDGE_PROFILE: BridgeProfile = BridgeProfile {
    chain_id: 137,
    spoke_pool: address!("9295ee1d8C5b022Be115A2AD3c30C72E34e7F096"),
    one_click_blockchain: "pol",
    ..MAINNET_BRIDGE_PROFILE
};
const ARBITRUM_BRIDGE_PROFILE: BridgeProfile = BridgeProfile {
    chain_id: 42161,
    spoke_pool: address!("e35e9842fceaCA96570B734083f4a58e8F7C5f2A"),
    one_click_blockchain: "arb",
    ..MAINNET_BRIDGE_PROFILE
};

impl BridgeProfile {
    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }
    #[must_use]
    pub const fn across_api_base(&self) -> &'static str {
        self.across_api_base
    }
    #[must_use]
    pub const fn spoke_pool(&self) -> Address {
        self.spoke_pool
    }
    #[must_use]
    pub const fn one_click_api_base(&self) -> &'static str {
        self.one_click_api_base
    }
    /// 1Click's `blockchain` identifier for this chain in its token list.
    #[must_use]
    pub const fn one_click_blockchain(&self) -> &'static str {
        self.one_click_blockchain
    }
    /// The `ed25519:`-prefixed base58 key that signs 1Click quotes.
    #[must_use]
    pub const fn one_click_quote_key(&self) -> &'static str {
        self.one_click_quote_key
    }

    /// Check a Bridge delivery receiver on this profile's chain, the destination chain,
    /// against that chain's Railgun proxy `railgun` and this profile's `SpokePool`. Any other
    /// address is accepted.
    pub fn check_receiver(
        &self,
        railgun: Address,
        receiver: Address,
    ) -> Result<(), BridgeReceiverRejection> {
        let rejection = if receiver == Address::ZERO {
            BridgeReceiverRejection::ZeroAddress
        } else if receiver == railgun {
            BridgeReceiverRejection::Railgun
        } else if receiver == self.spoke_pool {
            BridgeReceiverRejection::SpokePool
        } else {
            return Ok(());
        };
        Err(rejection)
    }
}

/// Why a Bridge swap can't deliver to a receiver on the destination chain: the funds would be
/// lost or stranded in a protocol contract there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BridgeReceiverRejection {
    #[error("the zero address can't receive the swap; the proceeds would be lost")]
    ZeroAddress,
    #[error(
        "the destination Railgun contract can't be the receiver; the proceeds would be stranded"
    )]
    Railgun,
    #[error(
        "the destination Across SpokePool can't be the receiver; the proceeds would be stranded"
    )]
    SpokePool,
}

impl SwapProfile {
    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }
    #[must_use]
    pub const fn settlement(&self) -> Address {
        self.settlement
    }
    #[must_use]
    pub const fn vault_relayer(&self) -> Address {
        self.vault_relayer
    }
    #[must_use]
    pub const fn hooks_trampoline(&self) -> Address {
        self.hooks_trampoline
    }
    #[must_use]
    pub const fn deadline_guard(&self) -> Address {
        self.deadline_guard
    }
    #[must_use]
    pub const fn orderbook_api_base(&self) -> &'static str {
        self.orderbook_api_base
    }
    #[must_use]
    pub const fn app_code(&self) -> &'static str {
        self.app_code
    }
    #[must_use]
    pub const fn app_data_byte_budget(&self) -> usize {
        self.app_data_byte_budget
    }
    #[must_use]
    pub const fn valid_to_window(&self) -> Duration {
        self.valid_to_window
    }
    /// Maximum `updatedAt` age for a Chainlink aggregator, honoring per-source overrides.
    #[must_use]
    pub fn chainlink_max_age(&self, aggregator: Address) -> Duration {
        self.chainlink_max_age_overrides
            .iter()
            .find(|(source, _)| *source == aggregator)
            .map_or(self.chainlink_max_age, |(_, age)| *age)
    }
    #[must_use]
    pub const fn max_head_age(&self) -> Duration {
        self.max_head_age
    }
    #[must_use]
    pub const fn anchor_deviation_bps(&self) -> u32 {
        self.anchor_deviation_bps
    }
    /// Freshness limits for this profile's anchor reads at approval and before signing.
    #[must_use]
    pub fn fresh_anchor_params(&self) -> FreshAnchorParams {
        FreshAnchorParams {
            chainlink_max_age: self.chainlink_max_age,
            chainlink_max_age_overrides: self
                .chainlink_max_age_overrides
                .iter()
                .map(|(source, age)| ((self.chain_id, *source), *age))
                .collect(),
            max_head_age: self.max_head_age,
        }
    }

    /// ERC-20 compatibility is the user's responsibility; there is no swap-specific allowlist.
    /// The native asset, `Address::ZERO`, is eligible only as the Buy asset of External delivery.
    #[must_use]
    pub fn token_eligibility(&self, token: Address, role: SwapTokenRole) -> SwapTokenEligibility {
        if token == Address::ZERO
            && !matches!(role, SwapTokenRole::Buy(SwapDelivery::External { .. }))
        {
            SwapTokenEligibility::Ineligible(SwapIneligibility::NativeAsset)
        } else {
            SwapTokenEligibility::Eligible
        }
    }

    #[must_use]
    pub fn pair_eligibility(
        &self,
        sell: Address,
        buy: Address,
        delivery: SwapDelivery,
    ) -> SwapTokenEligibility {
        for (token, role) in [
            (sell, SwapTokenRole::Sell),
            (buy, SwapTokenRole::Buy(delivery)),
        ] {
            let eligibility = self.token_eligibility(token, role);
            if eligibility != SwapTokenEligibility::Eligible {
                return eligibility;
            }
        }
        if sell == buy {
            return SwapTokenEligibility::Ineligible(SwapIneligibility::SameToken);
        }
        SwapTokenEligibility::Eligible
    }

    /// Check an External delivery receiver against the swap's own addresses: its `executor`,
    /// the chain's Railgun proxy `railgun`, and this profile's `CoW` contracts. Any other address,
    /// including another of the wallet's accounts, is accepted.
    pub fn check_receiver(
        &self,
        railgun: Address,
        executor: Address,
        receiver: Address,
    ) -> Result<(), SwapReceiverRejection> {
        let rejection = if receiver == Address::ZERO {
            SwapReceiverRejection::ZeroAddress
        } else if receiver == executor {
            SwapReceiverRejection::Executor
        } else if receiver == railgun {
            SwapReceiverRejection::Railgun
        } else if receiver == self.settlement {
            SwapReceiverRejection::Settlement
        } else if receiver == self.vault_relayer {
            SwapReceiverRejection::VaultRelayer
        } else if receiver == self.hooks_trampoline {
            SwapReceiverRejection::HooksTrampoline
        } else {
            return Ok(());
        };
        Err(rejection)
    }
}

impl EffectiveChainConfig {
    /// Swaps require a built-in profile and an accepted, enabled executor profile.
    #[must_use]
    pub fn swap_profile(&self) -> Option<SwapProfile> {
        if !self.built_in || self.accepted_executor_profile().is_none() {
            return None;
        }
        match self.chain_id {
            1 => Some(MAINNET_SWAP_PROFILE),
            56 => Some(BNB_SWAP_PROFILE),
            137 => Some(POLYGON_SWAP_PROFILE),
            42161 => Some(ARBITRUM_SWAP_PROFILE),
            _ => None,
        }
    }
}

impl EffectiveChainConfig {
    /// Bridge parameters for a built-in chain. Unlike [`Self::swap_profile`], this doesn't
    /// require an executor profile: a bridge's destination chain needs only RPC access.
    #[must_use]
    pub const fn bridge_profile(&self) -> Option<BridgeProfile> {
        if !self.built_in {
            return None;
        }
        match self.chain_id {
            1 => Some(MAINNET_BRIDGE_PROFILE),
            56 => Some(BNB_BRIDGE_PROFILE),
            137 => Some(POLYGON_BRIDGE_PROFILE),
            42161 => Some(ARBITRUM_BRIDGE_PROFILE),
            _ => None,
        }
    }
}

/// v1 destination-token source: configured tokens on the profile's chain filtered by eligibility.
/// The list is ERC-20 only for both delivery kinds; a native payout is an output choice on the
/// wrapped native token, not a list entry.
#[must_use]
pub fn swap_destination_tokens<'a>(
    registry: &'a EffectiveTokenRegistry,
    profile: &SwapProfile,
) -> Vec<&'a EffectiveTokenInfo> {
    registry
        .tokens
        .values()
        .filter(|token| token.chain_id == profile.chain_id)
        .filter(|token| {
            Address::from_str(&token.token_address).is_ok_and(|address| {
                profile.token_eligibility(address, SwapTokenRole::Buy(SwapDelivery::Reshield))
                    == SwapTokenEligibility::Eligible
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{
        CustomTokenSettings, TokenKey, WalletSettings, build_effective_chain_configs,
        build_effective_token_registry,
    };

    #[test]
    fn swap_profiles_require_accepted_built_in_executors() {
        let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        for (chain_id, api, guard) in [
            (
                1,
                "mainnet",
                address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"),
            ),
            (
                56,
                "bnb",
                address!("B971eF87ede563556b2ED4b1C0b0019111Dd85d2"),
            ),
            (
                137,
                "polygon",
                address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"),
            ),
            (
                42161,
                "arbitrum_one",
                address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"),
            ),
        ] {
            let chain = chains.get_mut(chain_id).unwrap();
            let profile = chain.swap_profile().expect("supported chain");
            assert_eq!(profile.chain_id(), chain_id);
            assert_eq!(
                profile.orderbook_api_base(),
                format!("https://api.cow.fi/{api}")
            );
            assert_eq!(profile.deadline_guard(), guard);
            assert_eq!(
                profile.settlement(),
                address!("9008D19f58AAbD9eD0D60971565AA8510560ab41")
            );
            assert_eq!(
                profile.vault_relayer(),
                address!("C92E8bdf79f0507f65a392b0ab4667716BFE0110")
            );
            assert_eq!(
                profile.hooks_trampoline(),
                address!("60Bf78233f48eC42eE3F101b9a05eC7878728006")
            );

            chain.built_in = false;
            assert!(chain.swap_profile().is_none());
            chain.built_in = true;
            let deployment = &mut chain.railgun.as_mut().unwrap().deployment;
            let delegate = deployment.relay_adapt_7702_contract;
            deployment.relay_adapt_7702_contract = Address::repeat_byte(7);
            assert!(chain.swap_profile().is_none());
            chain
                .railgun
                .as_mut()
                .unwrap()
                .deployment
                .relay_adapt_7702_contract = delegate;
            chain.enabled = false;
            assert!(chain.swap_profile().is_none());
        }
    }

    #[test]
    fn bridge_profiles_cover_every_swap_chain_only() {
        let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        let mut unsupported_built_in = 0;
        for chain in chains.values() {
            let bridge = chain.bridge_profile();
            if chain.swap_profile().is_some() {
                let bridge = bridge.expect("every swap chain can bridge");
                assert_eq!(bridge.chain_id(), chain.chain_id);
                assert_ne!(bridge.spoke_pool(), Address::ZERO);
            } else if chain.built_in && ![1, 56, 137, 42161].contains(&chain.chain_id) {
                assert!(bridge.is_none(), "chain {}", chain.chain_id);
                unsupported_built_in += 1;
            }
        }
        assert!(unsupported_built_in > 0);

        // A destination chain needs only RPC, so no executor or enabled check applies here.
        let chain = chains.get_mut(137).unwrap();
        chain.enabled = false;
        assert!(chain.swap_profile().is_none());
        assert!(chain.bridge_profile().is_some());
        chain.built_in = false;
        assert!(chain.bridge_profile().is_none());
    }

    #[test]
    fn swap_tokens_follow_the_configured_registry() {
        let profile = MAINNET_SWAP_PROFILE;
        let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let steth = address!("ae7ab96520DE3A18E5e111B5EaAb095312D7fE84");
        let custom = Address::repeat_byte(7);
        let other_chain = Address::repeat_byte(8);
        let mut settings = WalletSettings::default();
        for (chain_id, token) in [(1, custom), (56, other_chain)] {
            settings.tokens.custom_tokens.push(CustomTokenSettings {
                chain_id,
                token_address: token.to_string(),
                symbol: "CUSTOM".into(),
                ..Default::default()
            });
        }
        settings.tokens.built_in_tombstones.push(TokenKey {
            chain_id: 1,
            token_address: usdc.to_string(),
        });
        let registry = build_effective_token_registry(&settings).unwrap();
        let reshield = SwapDelivery::Reshield;
        let external = SwapDelivery::External {
            receiver: Address::repeat_byte(9),
        };
        let offered = swap_destination_tokens(&registry, &profile)
            .into_iter()
            .map(|token| token.token_address.parse::<Address>().unwrap())
            .collect::<Vec<_>>();
        assert!(offered.contains(&custom));
        assert!(offered.contains(&steth));
        assert!(!offered.contains(&other_chain));
        assert!(!offered.contains(&usdc));

        // The backend admits the same tokens in either direction, without a second list.
        for (sell, buy) in [(weth, custom), (custom, weth), (steth, weth)] {
            assert_eq!(
                profile.pair_eligibility(sell, buy, reshield),
                SwapTokenEligibility::Eligible
            );
        }
        let ineligible = SwapTokenEligibility::Ineligible;
        assert_eq!(
            profile.pair_eligibility(usdc, Address::ZERO, external),
            SwapTokenEligibility::Eligible
        );
        for (sell, buy, delivery) in [
            (usdc, Address::ZERO, reshield),
            (Address::ZERO, usdc, reshield),
            (Address::ZERO, usdc, external),
        ] {
            assert_eq!(
                profile.pair_eligibility(sell, buy, delivery),
                ineligible(SwapIneligibility::NativeAsset)
            );
        }
        assert_eq!(
            profile.pair_eligibility(weth, weth, reshield),
            ineligible(SwapIneligibility::SameToken)
        );
    }

    #[test]
    fn external_receivers_exclude_the_swaps_own_addresses() {
        let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        let chain = chains.get(1).unwrap();
        let profile = chain.swap_profile().unwrap();
        let railgun = chain.require_railgun().unwrap().deployment.contract;
        let executor = Address::repeat_byte(2);
        for (receiver, rejection) in [
            (Address::ZERO, SwapReceiverRejection::ZeroAddress),
            (executor, SwapReceiverRejection::Executor),
            (railgun, SwapReceiverRejection::Railgun),
            (profile.settlement(), SwapReceiverRejection::Settlement),
            (profile.vault_relayer(), SwapReceiverRejection::VaultRelayer),
            (
                profile.hooks_trampoline(),
                SwapReceiverRejection::HooksTrampoline,
            ),
        ] {
            assert_eq!(
                profile.check_receiver(railgun, executor, receiver),
                Err(rejection)
            );
        }
        // Another of the wallet's stealth accounts may receive.
        assert_eq!(
            profile.check_receiver(railgun, executor, Address::repeat_byte(3)),
            Ok(())
        );
    }

    #[test]
    fn bridge_receivers_exclude_the_destination_chains_protocol_contracts() {
        let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        let destination = chains.get(137).unwrap();
        let bridge = destination.bridge_profile().unwrap();
        let railgun = destination.require_railgun().unwrap().deployment.contract;
        for (receiver, rejection) in [
            (Address::ZERO, BridgeReceiverRejection::ZeroAddress),
            (railgun, BridgeReceiverRejection::Railgun),
            (bridge.spoke_pool(), BridgeReceiverRejection::SpokePool),
        ] {
            assert_eq!(bridge.check_receiver(railgun, receiver), Err(rejection));
        }
        assert_eq!(
            bridge.check_receiver(railgun, Address::repeat_byte(3)),
            Ok(())
        );
    }
}
