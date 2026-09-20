use super::NativeCurrency;
use alloy::primitives::{Address, address};
use std::time::Duration;

pub(super) struct EvmPreset {
    pub name: &'static str,
    pub native_currency: NativeCurrency,
    pub rpc_endpoints: Vec<String>,
    pub multicall: Address,
    pub wrapped_native: Option<Address>,
    pub native_usd_oracle: Address,
    pub block_time: Duration,
    pub finality_depth: u64,
}

impl EvmPreset {
    pub(super) fn for_chain(chain_id: u64) -> Option<Self> {
        match chain_id {
            1 => Some(Self {
                name: "Ethereum",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://ethereum-public.nodies.app",
                    "https://ethereum-rpc.publicnode.com",
                    "https://rpc.eth.gateway.fm",
                    "https://public-eth.nownodes.io",
                    "https://eth.api.pocket.network",
                    "https://mainnet.rpc.sentio.xyz",
                    "https://eth.drpc.org",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                multicall: address!("0xcA11bde05977b3631167028862bE2a173976CA11"),
                native_usd_oracle: address!("0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419"),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(12),
                finality_depth: 12,
            }),
            56 => Some(Self {
                name: "BSC",
                native_currency: NativeCurrency {
                    name: "BNB".into(),
                    symbol: "BNB".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://bsc.publicnode.com",
                    "https://binance-smart-chain-public.nodies.app",
                    "https://bsc-mainnet.nodereal.io/v1/64a9df0874fb4a93b9d0a3849de012d3",
                    "https://bsc.rpc.blxrbdn.com",
                    "https://bsc.drpc.org",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                multicall: address!("0xcA11bde05977b3631167028862bE2a173976CA11"),
                native_usd_oracle: address!("0x0567F2323251f0Aab15c8dFb1967E4e8A7D42aeE"),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(450),
                finality_depth: 15,
            }),
            137 => Some(Self {
                name: "Polygon",
                native_currency: NativeCurrency {
                    name: "Matic".into(),
                    symbol: "MATIC".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://rpc-mainnet.matic.quiknode.pro",
                    "https://polygon-public.nodies.app",
                    "https://polygon-bor-rpc.publicnode.com",
                    "https://poly.api.pocket.network",
                    "https://polygon.drpc.org",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                multicall: address!("0xcA11bde05977b3631167028862bE2a173976CA11"),
                native_usd_oracle: address!("0xAB594600376Ec9fD91F8e885dADF0CE036862dE0"),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(1),
                finality_depth: 256,
            }),
            42161 => Some(Self {
                name: "Arbitrum",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://arbitrum-one-public.nodies.app",
                    "https://arb1.arbitrum.io/rpc",
                    "https://arbitrum-one.public.blastapi.io",
                    "https://arbitrum-one-rpc.publicnode.com",
                    "https://api.zan.top/arb-one",
                    "https://arbitrum.rpc.subquery.network/public",
                    "https://arb1.lava.build",
                    "https://arbitrum.gateway.tenderly.co",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                multicall: address!("0xcA11bde05977b3631167028862bE2a173976CA11"),
                native_usd_oracle: address!("0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612"),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(250),
                finality_depth: 64,
            }),
            _ => None,
        }
    }
}
