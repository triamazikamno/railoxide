use std::str::FromStr;
use std::time::Duration;

use alloy::primitives::{Address, address};
use broadcaster_core::contracts::swap_math::SWAP_MATH_ADDRESS;

use super::{
    EffectiveChainConfig, EffectiveTokenInfo, EffectiveTokenRegistry,
    resolve_effective_chain_rpc_route,
};
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
/// Across's `SpokePool` and `MulticallHandler`, and 1Click's pinned quote-signing key. Not
/// user-editable and not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeProfile {
    chain_id: u64,
    across_api_base: &'static str,
    spoke_pool: Address,
    multicall_handler: Address,
    one_click_api_base: &'static str,
    one_click_blockchain: &'static str,
    one_click_quote_key: &'static str,
}

const ACROSS_API_BASE: &str = "https://app.across.to/api";

/// Across's `SpokePool` pinned for a chain, the only place the wallet names one. Each is the
/// Across proxy whose implementation contains `depositV3`. The four swap chains were checked
/// on-chain on 2026-09-29. The others were added on 2026-10-06, each after a fork of the chain
/// accepted a `depositV3` that named another account as depositor, with the handler recipient
/// and a private-delivery message. Arc and Tempo have Across routes and are left out: their
/// tokens can't be simulated on a fork, so neither passed that check.
const fn pinned_spoke_pool(chain_id: u64) -> Option<Address> {
    match chain_id {
        1 => Some(address!("5c7BCd6E7De5423a257D81B442095A1a6ced35C5")),
        10 => Some(address!("6f26Bf09B1C792e3228e5467807a900A503c0281")),
        56 => Some(address!("4e8E101924eDE233C13e2D8622DC8aED2872d505")),
        // Base, Unichain and World Chain share one deterministic deployment address.
        130 | 480 | 8453 => Some(address!("09aea4b2242abC8bb4BB78D537A67a245A7bEC64")),
        137 => Some(address!("9295ee1d8C5b022Be115A2AD3c30C72E34e7F096")),
        143 => Some(address!("d2ecb3afe598b746F8123CaE365a598DA831A449")),
        999 => Some(address!("35E63eA3eb0fb7A3bc543C71FB66412e1F6B0E04")),
        4326 => Some(address!("3Db06DA8F0a24A525f314eeC954fC5c6a973d40E")),
        4663 => Some(address!("D29C85F15DF544bA632C9E25829fd29d767d7978")),
        9745 => Some(address!("50039fAEfebef707cFD94D6d462fE6D10B39207a")),
        42161 => Some(address!("e35e9842fceaCA96570B734083f4a58e8F7C5f2A")),
        43114 => Some(address!("FE9D541c92E4e90437C7152A00244886dE37a658")),
        57073 => Some(address!("eF684C38F94F48775959ECf2012D7E864ffb9dd4")),
        59144 => Some(address!("7E63A5f1a8F0B4d0934B2f2327DAED3F6bb2ee75")),
        _ => None,
    }
}

/// The pinned `SpokePool` a Bridge delivery's destination role names: the swap chains' only.
/// The pools pinned for other chains were checked as deposit origins, not as fill destinations.
const fn destination_spoke_pool(chain_id: u64) -> Option<Address> {
    match chain_id {
        1 | 56 | 137 | 42161 => pinned_spoke_pool(chain_id),
        _ => None,
    }
}

/// The bridge profile of a swap chain, with its `SpokePool` from [`pinned_spoke_pool`] and its
/// 1Click name from [`destination_one_click_blockchain`].
const fn built_in_bridge_profile(chain_id: u64) -> Option<BridgeProfile> {
    // Across's `MulticallHandler` deployments, checked on-chain on 2026-10-02; BNB Smart
    // Chain's is at its own address.
    let multicall_handler = match chain_id {
        1 | 137 | 42161 => address!("924a9f036260DdD5808007E1AA95f08eD08aA569"),
        56 => address!("AC537C12fE8f544D712d71ED4376a502EEa944d7"),
        _ => return None,
    };
    let Some(spoke_pool) = pinned_spoke_pool(chain_id) else {
        return None;
    };
    let Some(one_click_blockchain) = destination_one_click_blockchain(chain_id) else {
        return None;
    };
    Some(BridgeProfile {
        chain_id,
        across_api_base: ACROSS_API_BASE,
        spoke_pool,
        multicall_handler,
        one_click_api_base: "https://1click.chaindefuser.com",
        one_click_blockchain,
        // `ONE_CLICK_MANAGER_PUB_KEY` in 1Click's TypeScript SDK 0.1.26.
        one_click_quote_key: "ed25519:reYaWhvwu8Jzo3WUM3zhn6VrhuMEF4eADL17qtRVifc",
    })
}

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
    /// Across's `MulticallHandler`, the deposit recipient of a private delivery.
    #[must_use]
    pub const fn multicall_handler(&self) -> Address {
        self.multicall_handler
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
}

/// What a public Bridge delivery needs of its destination chain, any enabled chain with RPC
/// endpoints: 1Click's name for the chain and Across's pinned `SpokePool` there, where the
/// wallet ships them. Not user-editable and not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeDestinationProfile {
    chain_id: u64,
    one_click_blockchain: Option<&'static str>,
    spoke_pool: Option<Address>,
}

/// 1Click's `blockchain` identifier for a destination chain in its token list. The names
/// beyond the bridge profiles' own were checked against 1Click's token list on 2026-10-04.
const fn destination_one_click_blockchain(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some("eth"),
        10 => Some("op"),
        56 => Some("bsc"),
        100 => Some("gnosis"),
        137 => Some("pol"),
        143 => Some("monad"),
        4663 => Some("hood"),
        8453 => Some("base"),
        9745 => Some("plasma"),
        42161 => Some("arb"),
        43114 => Some("avax"),
        80094 => Some("bera"),
        _ => None,
    }
}

impl BridgeDestinationProfile {
    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }
    /// 1Click's `blockchain` identifier for this chain in its token list. `None` where NEAR
    /// Intents isn't offered.
    #[must_use]
    pub const fn one_click_blockchain(&self) -> Option<&'static str> {
        self.one_click_blockchain
    }
    /// Across's `SpokePool` on this chain, where the wallet pins one.
    #[must_use]
    pub const fn spoke_pool(&self) -> Option<Address> {
        self.spoke_pool
    }

    /// Check a Bridge delivery receiver on this profile's chain against that chain's Railgun
    /// proxy `railgun`, when it has one, and this profile's `SpokePool`, when one is pinned.
    /// Any other address is accepted.
    pub fn check_receiver(
        &self,
        railgun: Option<Address>,
        receiver: Address,
    ) -> Result<(), BridgeReceiverRejection> {
        let rejection = if receiver == Address::ZERO {
            BridgeReceiverRejection::ZeroAddress
        } else if Some(receiver) == railgun {
            BridgeReceiverRejection::Railgun
        } else if Some(receiver) == self.spoke_pool {
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

/// What a deposit a Public account sends itself needs of its own chain: Across's API root and
/// its pinned `SpokePool` there. Not user-editable and not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeOriginProfile {
    chain_id: u64,
    across_api_base: &'static str,
    spoke_pool: Address,
}

impl BridgeOriginProfile {
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
}

/// Built-in parameters for a swap paid from a Public account: the `CoW` settlement, vault
/// relayer and orderbook an order uses, and the cow-shed factory and implementation, weiroll
/// executor and math contract its post-hook uses. It carries nothing of the stealth flow's
/// pre-hook, so a chain needs no [`SwapProfile`] to have one, and unlike
/// [`EffectiveChainConfig::swap_profile`] it doesn't need an accepted executor profile. Not
/// user-editable and not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicSwapProfile {
    chain_id: u64,
    settlement: Address,
    vault_relayer: Address,
    orderbook_api_base: &'static str,
    app_code: &'static str,
    app_data_byte_budget: usize,
    valid_to_window: Duration,
    anchor_deviation_bps: u32,
    cow_shed_factory: Address,
    cow_shed_implementation: Address,
    weiroll: Address,
    math: Address,
}

// The cow-shed v2.1.0 factory and implementation and the weiroll executor are at the same
// addresses on every chain with a Public swap profile; their code hashes were checked on forks
// of chains 1, 56, 137 and 42161 and on Base, Avalanche, Linea and Ink on 2026-10-06.
const COW_SHED_FACTORY: Address = address!("0a654985c5856ab562237286f36d55c0ff637213");
const COW_SHED_IMPLEMENTATION: Address = address!("F0D586aB0017fDfE2ACf4AB008B3Ddb2CF50bB09");
const WEIROLL_EXECUTOR: Address = address!("9585c3062Df1C247d5E373Cfca9167F7dC2b5963");

/// The Public swap profile of the chain `cow` is for: the `CoW` parameters an order from a
/// Public account reads, taken from the stealth flow's profile so that neither is written twice.
const fn public_profile_of(cow: &SwapProfile) -> PublicSwapProfile {
    PublicSwapProfile {
        chain_id: cow.chain_id,
        settlement: cow.settlement,
        vault_relayer: cow.vault_relayer,
        orderbook_api_base: cow.orderbook_api_base,
        app_code: cow.app_code,
        app_data_byte_budget: cow.app_data_byte_budget,
        valid_to_window: cow.valid_to_window,
        anchor_deviation_bps: cow.anchor_deviation_bps,
        cow_shed_factory: COW_SHED_FACTORY,
        cow_shed_implementation: COW_SHED_IMPLEMENTATION,
        weiroll: WEIROLL_EXECUTOR,
        math: SWAP_MATH_ADDRESS,
    }
}

/// The Public swap profile of a chain: the four swap chains', from their [`SwapProfile`], and
/// those of Base, Avalanche, Linea and Ink, which have no stealth swap profile.
const fn built_in_public_swap_profile(chain_id: u64) -> Option<PublicSwapProfile> {
    if let Some(cow) = built_in_swap_profile(chain_id) {
        return Some(public_profile_of(&cow));
    }
    // Checked on-chain on 2026-10-06: each of these four has `GPv2Settlement` with this
    // chain's EIP-712 domain separator, its vault relayer, the hooks trampoline, the cow-shed
    // factory and implementation and the weiroll executor, all at the swap chains' addresses
    // and with their code hashes, and a `CoW` orderbook at the root named here. None of them
    // has a stealth swap profile.
    let orderbook_api_base = match chain_id {
        8453 => "https://api.cow.fi/base",
        43114 => "https://api.cow.fi/avalanche",
        57073 => "https://api.cow.fi/ink",
        59144 => "https://api.cow.fi/linea",
        _ => return None,
    };
    Some(PublicSwapProfile {
        chain_id,
        orderbook_api_base,
        ..public_profile_of(&MAINNET_SWAP_PROFILE)
    })
}

impl PublicSwapProfile {
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
    /// How far an order's quote may be from the pair's cached anchor rate, in basis points.
    #[must_use]
    pub const fn anchor_deviation_bps(&self) -> u32 {
        self.anchor_deviation_bps
    }
    #[must_use]
    pub const fn cow_shed_factory(&self) -> Address {
        self.cow_shed_factory
    }
    #[must_use]
    pub const fn cow_shed_implementation(&self) -> Address {
        self.cow_shed_implementation
    }
    #[must_use]
    pub const fn weiroll(&self) -> Address {
        self.weiroll
    }
    #[must_use]
    pub const fn math(&self) -> Address {
        self.math
    }
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

const fn built_in_swap_profile(chain_id: u64) -> Option<SwapProfile> {
    match chain_id {
        1 => Some(MAINNET_SWAP_PROFILE),
        56 => Some(BNB_SWAP_PROFILE),
        137 => Some(POLYGON_SWAP_PROFILE),
        42161 => Some(ARBITRUM_SWAP_PROFILE),
        _ => None,
    }
}

impl EffectiveChainConfig {
    /// Swaps require a built-in profile and an accepted, enabled executor profile.
    #[must_use]
    pub fn swap_profile(&self) -> Option<SwapProfile> {
        if !self.built_in || self.accepted_executor_profile().is_none() {
            return None;
        }
        built_in_swap_profile(self.chain_id)
    }

    /// Swaps paid from a Public account require a built-in profile on an enabled chain with a
    /// pinned `SpokePool`, since an order ends in a deposit. Unlike [`Self::swap_profile`],
    /// this doesn't require an executor profile.
    #[must_use]
    pub const fn public_swap_profile(&self) -> Option<PublicSwapProfile> {
        if !self.built_in || !self.enabled || self.bridge_origin_profile().is_none() {
            return None;
        }
        built_in_public_swap_profile(self.chain_id)
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
        built_in_bridge_profile(self.chain_id)
    }

    /// What a deposit a Public account sends itself needs of this chain: a built-in chain
    /// with a pinned `SpokePool`. No executor profile is required.
    #[must_use]
    pub const fn bridge_origin_profile(&self) -> Option<BridgeOriginProfile> {
        if !self.built_in {
            return None;
        }
        let Some(spoke_pool) = pinned_spoke_pool(self.chain_id) else {
            return None;
        };
        Some(BridgeOriginProfile {
            chain_id: self.chain_id,
            across_api_base: ACROSS_API_BASE,
            spoke_pool,
        })
    }

    /// What a public Bridge delivery needs of this chain as its destination: an enabled chain
    /// with RPC endpoints, built in or added by the user, with or without Railgun. The 1Click
    /// name and the pinned `SpokePool` follow the chain id alone.
    #[must_use]
    pub fn bridge_destination(&self) -> Option<BridgeDestinationProfile> {
        resolve_effective_chain_rpc_route(self.chain_id, self).ok()?;
        Some(BridgeDestinationProfile {
            chain_id: self.chain_id,
            one_click_blockchain: destination_one_click_blockchain(self.chain_id),
            spoke_pool: destination_spoke_pool(self.chain_id),
        })
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
        let mut origin_only = Vec::new();
        for chain in chains.values() {
            let bridge = chain.bridge_profile();
            if chain.swap_profile().is_some() {
                let bridge = bridge.expect("every swap chain can bridge");
                assert_eq!(bridge.chain_id(), chain.chain_id);
                assert_ne!(bridge.spoke_pool(), Address::ZERO);
                let handler = if chain.chain_id == 56 {
                    address!("AC537C12fE8f544D712d71ED4376a502EEa944d7")
                } else {
                    address!("924a9f036260DdD5808007E1AA95f08eD08aA569")
                };
                assert_eq!(bridge.multicall_handler(), handler);
                // The public destination role names the same pool and 1Click chain.
                let destination = chain.bridge_destination().expect("every swap chain");
                assert_eq!(destination.chain_id(), chain.chain_id);
                assert_eq!(destination.spoke_pool(), Some(bridge.spoke_pool()));
                assert_eq!(
                    destination.one_click_blockchain(),
                    Some(bridge.one_click_blockchain())
                );
                // So does the origin role of a deposit a Public account sends itself.
                let origin = chain.bridge_origin_profile().expect("every swap chain");
                assert_eq!(origin.chain_id(), chain.chain_id);
                assert_eq!(origin.spoke_pool(), bridge.spoke_pool());
                assert_eq!(origin.across_api_base(), bridge.across_api_base());
            } else if chain.built_in && ![1, 56, 137, 42161].contains(&chain.chain_id) {
                assert!(bridge.is_none(), "chain {}", chain.chain_id);
                // A chain with a pinned pool and no Public swap profile is a deposit origin
                // only.
                if let Some(origin) = chain.bridge_origin_profile() {
                    assert_ne!(origin.spoke_pool(), Address::ZERO);
                    assert_eq!(
                        chain.bridge_destination().and_then(|d| d.spoke_pool()),
                        None,
                        "chain {}",
                        chain.chain_id
                    );
                    if chain.public_swap_profile().is_none() {
                        origin_only.push(chain.chain_id);
                    }
                }
                unsupported_built_in += 1;
            }
        }
        assert!(unsupported_built_in > 0);
        origin_only.sort_unstable();
        assert_eq!(origin_only, [10, 130, 143, 480, 999, 4326, 4663, 9745]);
        // Across serves Arc and Tempo, which did not pass the fork check.
        for chain_id in [5042, 4217, 100] {
            assert!(
                chains
                    .get(chain_id)
                    .unwrap()
                    .bridge_origin_profile()
                    .is_none(),
                "chain {chain_id}"
            );
        }

        // A destination chain needs only RPC, so no executor or enabled check applies here.
        let chain = chains.get_mut(137).unwrap();
        chain.enabled = false;
        assert!(chain.swap_profile().is_none());
        assert!(chain.bridge_profile().is_some());
        assert!(chain.bridge_origin_profile().is_some());
        chain.built_in = false;
        assert!(chain.bridge_profile().is_none());
        assert!(chain.bridge_origin_profile().is_none());
    }

    #[test]
    fn public_swap_profiles_need_no_accepted_executor() {
        let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        for chain_id in [1, 56, 137, 42161] {
            let chain = chains.get_mut(chain_id).unwrap();
            let profile = chain.public_swap_profile().expect("supported chain");
            assert_eq!(profile.chain_id(), chain_id);
            let swap = chain.swap_profile().expect("swap chain");
            assert_eq!(profile.settlement(), swap.settlement());
            assert_eq!(profile.vault_relayer(), swap.vault_relayer());
            assert_eq!(profile.orderbook_api_base(), swap.orderbook_api_base());
            assert_eq!(profile.app_code(), swap.app_code());
            assert_eq!(profile.app_data_byte_budget(), swap.app_data_byte_budget());
            assert_eq!(profile.valid_to_window(), swap.valid_to_window());
            assert_eq!(profile.anchor_deviation_bps(), swap.anchor_deviation_bps());
            assert_eq!(
                profile.cow_shed_factory(),
                address!("0a654985c5856ab562237286f36d55c0ff637213")
            );
            assert_eq!(
                profile.cow_shed_implementation(),
                address!("F0D586aB0017fDfE2ACf4AB008B3Ddb2CF50bB09")
            );
            assert_eq!(
                profile.weiroll(),
                address!("9585c3062Df1C247d5E373Cfca9167F7dC2b5963")
            );
            assert_eq!(profile.math(), SWAP_MATH_ADDRESS);

            chain
                .railgun
                .as_mut()
                .unwrap()
                .deployment
                .relay_adapt_7702_contract = Address::repeat_byte(7);
            assert!(chain.swap_profile().is_none());
            assert_eq!(chain.public_swap_profile(), Some(profile));
            chain.enabled = false;
            assert!(chain.public_swap_profile().is_none());
            chain.enabled = true;
            chain.built_in = false;
            assert!(chain.public_swap_profile().is_none());
        }
    }

    /// Base, Avalanche, Linea and Ink take orders from a Public account without being swap or
    /// bridge chains of the stealth flow.
    #[test]
    fn public_swap_profiles_cover_chains_without_a_swap_profile() {
        let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        let mainnet = chains.get(1).unwrap().public_swap_profile().unwrap();
        for (chain_id, api) in [
            (8453, "base"),
            (43114, "avalanche"),
            (57073, "ink"),
            (59144, "linea"),
        ] {
            let chain = chains.get_mut(chain_id).unwrap();
            assert!(chain.swap_profile().is_none(), "chain {chain_id}");
            assert!(chain.bridge_profile().is_none(), "chain {chain_id}");
            assert!(chain.bridge_origin_profile().is_some(), "chain {chain_id}");
            let profile = chain.public_swap_profile().expect("order chain");
            assert_eq!(profile.chain_id(), chain_id);
            assert_eq!(
                profile.orderbook_api_base(),
                format!("https://api.cow.fi/{api}")
            );
            // Everything but the chain and its orderbook is as on the swap chains.
            assert_eq!(
                PublicSwapProfile {
                    chain_id: mainnet.chain_id,
                    orderbook_api_base: mainnet.orderbook_api_base,
                    ..profile
                },
                mainnet
            );

            chain.enabled = false;
            assert!(chain.public_swap_profile().is_none());
            chain.enabled = true;
            chain.built_in = false;
            assert!(chain.public_swap_profile().is_none());
        }
        // A pinned pool alone isn't enough: `CoW` runs no orderbook on Optimism, and Plasma has
        // no weiroll executor.
        for chain_id in [10, 9745] {
            let chain = chains.get(chain_id).unwrap();
            assert!(chain.bridge_origin_profile().is_some(), "chain {chain_id}");
            assert!(chain.public_swap_profile().is_none(), "chain {chain_id}");
        }
    }

    #[test]
    fn bridge_destinations_cover_every_enabled_chain_with_rpc() {
        const CUSTOM: u64 = 31337;
        let mut settings = WalletSettings::default();
        settings
            .chains
            .custom
            .insert(CUSTOM, crate::settings::tests::custom_evm_chain());
        let mut chains = build_effective_chain_configs(&settings).unwrap();
        // Built-in chains without Railgun, and a chain the user added: Base has a 1Click name,
        // and none of them a pinned pool.
        for (chain_id, one_click) in [(8453, Some("base")), (59144, None), (CUSTOM, None)] {
            let chain = chains.get(chain_id).unwrap();
            assert!(chain.railgun.is_none(), "chain {chain_id}");
            assert!(chain.bridge_profile().is_none(), "chain {chain_id}");
            let destination = chain.bridge_destination().expect("enabled chain with RPC");
            assert_eq!(destination.chain_id(), chain_id);
            assert_eq!(destination.one_click_blockchain(), one_click);
            assert_eq!(destination.spoke_pool(), None);
        }

        let chain = chains.get_mut(8453).unwrap();
        chain.enabled = false;
        assert!(chain.bridge_destination().is_none());
    }

    /// Across names a chain's wrapped native token as the destination of its native delivery,
    /// and only listed tokens are offered, so a chain whose native asset is ETH lists its WETH.
    #[test]
    fn eth_native_chains_list_their_wrapped_native_token() {
        let settings = WalletSettings::default();
        let chains = build_effective_chain_configs(&settings).unwrap();
        let registry = build_effective_token_registry(&settings).unwrap();
        let mut checked = 0;
        for chain in chains.values() {
            let Some(wrapped) = chain
                .wrapped_native_token
                .filter(|_| chain.built_in && chain.native_currency.symbol == "ETH")
            else {
                continue;
            };
            assert!(
                registry.get(chain.chain_id, &wrapped).is_some(),
                "chain {}",
                chain.chain_id
            );
            checked += 1;
        }
        assert!(checked > 2);
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
        let bridge = destination.bridge_destination().unwrap();
        let railgun = destination.require_railgun().unwrap().deployment.contract;
        let spoke_pool = bridge.spoke_pool().unwrap();
        for (receiver, rejection) in [
            (Address::ZERO, BridgeReceiverRejection::ZeroAddress),
            (railgun, BridgeReceiverRejection::Railgun),
            (spoke_pool, BridgeReceiverRejection::SpokePool),
        ] {
            assert_eq!(
                bridge.check_receiver(Some(railgun), receiver),
                Err(rejection)
            );
        }
        assert_eq!(
            bridge.check_receiver(Some(railgun), Address::repeat_byte(3)),
            Ok(())
        );

        // A chain without Railgun and without a pinned pool rejects only the zero address:
        // contracts that are protocol contracts on Polygon are ordinary addresses there.
        let base = chains.get(8453).unwrap().bridge_destination().unwrap();
        assert_eq!(
            base.check_receiver(None, Address::ZERO),
            Err(BridgeReceiverRejection::ZeroAddress)
        );
        for receiver in [railgun, spoke_pool, Address::repeat_byte(3)] {
            assert_eq!(base.check_receiver(None, receiver), Ok(()));
        }
    }
}
