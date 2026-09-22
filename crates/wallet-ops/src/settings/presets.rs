use super::NativeCurrency;
use alloy::primitives::{Address, address};
use std::time::Duration;

pub(super) struct EvmPreset {
    pub name: &'static str,
    pub native_currency: NativeCurrency,
    pub rpc_endpoints: Vec<String>,
    pub explorer_urls: &'static [&'static str],
    pub multicall: Option<Address>,
    pub wrapped_native: Option<Address>,
    pub native_usd_oracle: Option<Address>,
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
                explorer_urls: &["https://etherscan.io"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419")),
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
                explorer_urls: &["https://bscscan.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x0567F2323251f0Aab15c8dFb1967E4e8A7D42aeE")),
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
                explorer_urls: &["https://polygonscan.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xAB594600376Ec9fD91F8e885dADF0CE036862dE0")),
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
                explorer_urls: &["https://arbiscan.io"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(250),
                finality_depth: 64,
            }),
            5042 => Some(Self {
                name: "Arc",
                native_currency: NativeCurrency {
                    name: "USDC".into(),
                    symbol: "USDC".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://rpc.beamrpc.com",
                    "https://rpc.blockdaemon.mainnet.arc.io",
                    "https://rpc.drpc.mainnet.arc.io",
                    "https://rpc.mainnet.arc.io",
                    "https://rpc.quicknode.mainnet.arc.io",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://explorer.arc.io/"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: None,
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(500),
                finality_depth: 8,
            }),
            9745 => Some(Self {
                name: "Plasma",
                native_currency: NativeCurrency {
                    name: "Plasma".into(),
                    symbol: "XPL".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://rpc.swiftnodes.io/rpc/plasma",
                    "https://rpc.plasma.to",
                    "https://9745.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://plasmascan.to"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xF932477C37715aE6657Ab884414Bd9876FE3f750")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(1),
                finality_depth: 8,
            }),
            5000 => Some(Self {
                name: "Mantle",
                native_currency: NativeCurrency {
                    name: "Mantle".into(),
                    symbol: "MNT".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://rpc-mantle.blockmachine.io",
                    "https://mantle.drpc.org",
                    "https://mantle.api.pocket.network",
                    "https://mantle-rpc.publicnode.com",
                    "https://rpc.mantle.xyz",
                    "https://5000.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://mantlescan.xyz", "https://explorer.mantle.xyz"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xD97F20bEbeD74e8144134C4b148fE93417dd0F96")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(2),
                finality_depth: 64,
            }),
            42793 => Some(Self {
                name: "Etherlink",
                native_currency: NativeCurrency {
                    name: "tez".into(),
                    symbol: "XTZ".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://rpc.ankr.com/etherlink_mainnet",
                    "https://node.mainnet.etherlink.com",
                    "https://42793.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://explorer.etherlink.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x929dB17A4673f150251fDc7AC4E7B5dd7b2Fd654")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(700),
                finality_depth: 8,
            }),
            988 => Some(Self {
                name: "Stable",
                native_currency: NativeCurrency {
                    name: "USDT0".into(),
                    symbol: "USDT0".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://stable-mainnet.rpc.sentio.xyz",
                    "https://stable.drpc.org",
                    "https://rpc.stable.xyz",
                    "https://988.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://stablescan.xyz"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: None,
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(700),
                finality_depth: 8,
            }),
            143 => Some(Self {
                name: "Monad",
                native_currency: NativeCurrency {
                    name: "Monad".into(),
                    symbol: "MON".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://infra.originstake.com/monad/evm",
                    "https://monad-rpc.huginn.tech",
                    "https://monad-mainnet.rpc.sentio.xyz",
                    "https://monad-mainnet-rpc.spidernode.net",
                    "https://rpc-mainnet.monadinfra.com",
                    "https://rpc1.monad.xyz",
                    "https://rpc.monad.xyz",
                    "https://rpc3.monad.xyz",
                    "https://rpc4.monad.xyz",
                    "https://143.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://monadvision.com", "https://monadscan.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xBcD78f76005B7515837af6b50c7C52BCf73822fb")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(300),
                finality_depth: 8,
            }),
            4663 => Some(Self {
                name: "Robinhood Chain",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://robinhood.api.pocket.network",
                    "https://robinhood-rpc.publicnode.com",
                    "https://rpc.ordofi.network",
                    "https://rpc-robinhood.blockmachine.io",
                    "https://rpc.mainnet.chain.robinhood.com",
                    "https://robinhood.rpc.blxrbdn.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &[
                    "https://robinscan.io",
                    "https://robinhoodchain.blockscout.com",
                ],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x78F3556b67E17Df817D51Ef5a990cDaF09E8d3A9")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(100),
                finality_depth: 64,
            }),
            146 => Some(Self {
                name: "Sonic",
                native_currency: NativeCurrency {
                    name: "Sonic".into(),
                    symbol: "S".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://sonic.drpc.org",
                    "https://sonic.api.pocket.network",
                    "https://sonic-json-rpc.stakely.io",
                    "https://sonic-mainnet.rpc.sentio.xyz",
                    "https://sonic-rpc.publicnode.com",
                    "https://rpc.soniclabs.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://sonicscan.org", "https://explorer.soniclabs.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xc76dFb89fF298145b417d221B2c747d84952e01d")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(950),
                finality_depth: 8,
            }),
            130 => Some(Self {
                name: "Unichain",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://unichain.drpc.org",
                    "https://unichain-rpc.publicnode.com",
                    "https://rpc.swiftnodes.io/rpc/unichain",
                    "https://unichain-mainnet.rpc.sentio.xyz",
                    "https://mainnet.unichain.org",
                    "https://130.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://uniscan.xyz", "https://unichain.blockscout.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xe8D9FbC10e00ecc9f0694617075fDAF657a76FB2")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(1),
                finality_depth: 64,
            }),
            57073 => Some(Self {
                name: "Ink",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://ink.drpc.org",
                    "https://ink.api.pocket.network",
                    "https://rpc-qnd.inkonchain.com",
                    "https://rpc-gel.inkonchain.com",
                    "https://57073.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://explorer.inkonchain.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x963d5d3aD2Dfd3fe759d376fF62A0963176DBdF5")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(1),
                finality_depth: 64,
            }),
            8453 => Some(Self {
                name: "Base",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://base.drpc.org",
                    "https://base-rpc.publicnode.com",
                    "https://base.api.pocket.network",
                    "https://base.rpc.sentio.xyz",
                    "https://base.public.blockpi.network/v1/rpc/public",
                    "https://base-public.nodies.app",
                    "https://base-mainnet.public.blastapi.io",
                    "https://developer-access-mainnet.base.org",
                    "https://rpc.baseazul.dev",
                    "https://mainnet.base.org",
                    "https://xrpc.cl/base",
                    "https://8453.rpc.thirdweb.com",
                    "https://gateway.tenderly.co/public/base",
                    "https://base.gateway.tenderly.co",
                    "https://base.rpc.blxrbdn.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://basescan.org", "https://base.blockscout.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(2),
                finality_depth: 64,
            }),
            252 => Some(Self {
                name: "Fraxtal",
                native_currency: NativeCurrency {
                    name: "Frax".into(),
                    symbol: "FRAX".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://fraxtal.drpc.org",
                    "https://fraxtal.api.pocket.network",
                    "https://frax-mainnet.rpc.sentio.xyz",
                    "https://fraxtal-rpc.publicnode.com",
                    "https://rpc.frax.com",
                    "https://252.rpc.thirdweb.com",
                    "https://fraxtal.gateway.tenderly.co",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://fraxscan.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xbf228a9131AB3BB8ca8C7a4Ad574932253D99Cd1")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(2),
                finality_depth: 64,
            }),
            10 => Some(Self {
                name: "Optimism",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://rpc-optimism.blockmachine.io",
                    "https://optimism.drpc.org",
                    "https://optimism-rpc.publicnode.com",
                    "https://public-op-mainnet.fastnode.io",
                    "https://op.api.pocket.network",
                    "https://public.1rpc.io/op",
                    "https://rpc.swiftnodes.io/rpc/optimism",
                    "https://optimism.public.blockpi.network/v1/rpc/public",
                    "https://optimism.rpc.sentio.xyz",
                    "https://optimism-public.nodies.app",
                    "https://mainnet.optimism.io",
                    "https://xrpc.cl/optimism",
                    "https://gateway.tenderly.co/public/optimism",
                    "https://optimism.gateway.tenderly.co",
                    "https://10.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &[
                    "https://optimistic.etherscan.io",
                    "https://optimism.blockscout.com",
                ],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x13e3Ee699D1909E989722E753853AE30b17e08c5")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(2),
                finality_depth: 64,
            }),
            43114 => Some(Self {
                name: "Avalanche",
                native_currency: NativeCurrency {
                    name: "Avalanche".into(),
                    symbol: "AVAX".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://avalanche-c-chain-rpc.publicnode.com",
                    "https://avalanche.drpc.org",
                    "https://avax.api.pocket.network",
                    "https://rpc-avalanche.blockmachine.io",
                    "https://public.1rpc.io/avax/c",
                    "https://avalanche.rpc.sentio.xyz",
                    "https://avalanche.api.onfinality.io/public/ext/bc/C/rpc",
                    "https://rpc.swiftnodes.io/rpc/avalanche",
                    "https://api.avax.network/ext/bc/C/rpc",
                    "https://xrpc.cl/avalanche",
                    "https://avalanche-mainnet.gateway.tenderly.co",
                    "https://spectrum-01.simplystaking.xyz/avalanche-mn-rpc/ext/bc/C/rpc",
                    "https://43114.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://snowscan.xyz", "https://snowtrace.io"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x0A77230d17318075983913bC2145DB16C7366156")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(1100),
                finality_depth: 8,
            }),
            100 => Some(Self {
                name: "Gnosis",
                native_currency: NativeCurrency {
                    name: "xDAI".into(),
                    symbol: "XDAI".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://gnosis-rpc.publicnode.com",
                    "https://gnosis.drpc.org",
                    "https://gnosis.api.pocket.network",
                    "https://rpc.gnosischain.com",
                    "https://gnosis.oat.farm",
                    "https://rpc.ap-southeast-1.gateway.fm/v4/gnosis/non-archival/mainnet",
                    "https://rpc.gnosis.gateway.fm",
                    "https://100.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://gnosisscan.io", "https://gnosis.blockscout.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xE5269eF0CE04E509E8134624c7BF043b21e10897")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(5100),
                finality_depth: 32,
            }),
            81457 => Some(Self {
                name: "Blast",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://blast-rpc.publicnode.com",
                    "https://blast.drpc.org",
                    "https://blast.api.pocket.network",
                    "https://blast-mainnet.rpc.sentio.xyz",
                    "https://rpc.blast.io",
                    "https://blast.gateway.tenderly.co",
                    "https://81457.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://blastscan.io", "https://blastexplorer.io"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x458AD5B487F4442245E4C5eA7249009E607A5583")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(2),
                finality_depth: 64,
            }),
            59144 => Some(Self {
                name: "Linea",
                native_currency: NativeCurrency {
                    name: "Linea Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://linea-rpc.publicnode.com",
                    "https://linea.drpc.org",
                    "https://linea.api.pocket.network",
                    "https://public.1rpc.io/linea",
                    "https://linea.rpc.sentio.xyz",
                    "https://rpc.linea.build",
                    "https://59144.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://lineascan.build", "https://explorer.linea.build"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x3c6Cd9Cc7c7a4c2Cf5a82734CD249D7D593354dA")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(6800),
                finality_depth: 64,
            }),
            747_474 => Some(Self {
                name: "Katana",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://katana.drpc.org",
                    "https://katana.rpc.sentio.xyz",
                    "https://katana.gateway.tenderly.co",
                    "https://rpc.katana.network",
                    "https://rpc.katanarpc.com",
                    "https://747474.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://katanascan.com", "https://explorer.katanarpc.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x7BdBDB772f4a073BadD676A567C6ED82049a8eEE")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(1),
                finality_depth: 64,
            }),
            30 => Some(Self {
                name: "Rootstock",
                native_currency: NativeCurrency {
                    name: "Smart Bitcoin".into(),
                    symbol: "RBTC".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://rootstock.drpc.org",
                    "https://public-node.rsk.co",
                    "https://mycrypto.rsk.co",
                    "https://30.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &[
                    "https://explorer.rsk.co",
                    "https://rootstock.blockscout.com",
                ],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: None,
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(31150),
                finality_depth: 12,
            }),
            25 => Some(Self {
                name: "Cronos",
                native_currency: NativeCurrency {
                    name: "Cronos".into(),
                    symbol: "CRO".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://cronos-evm-rpc.publicnode.com",
                    "https://cronos.drpc.org",
                    "https://public.1rpc.io/cro",
                    "https://cronos.rpc.sentio.xyz",
                    "https://evm.cronos.org",
                    "https://rpc.vvs.finance",
                    "https://25.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://explorer.cronos.org"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x4636AC8216805Fe96dE9E7aFc62dA99096a930F6")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(400),
                finality_depth: 8,
            }),
            4326 => Some(Self {
                name: "MegaETH",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://megaeth.drpc.org",
                    "https://megaeth.rpc.sentio.xyz",
                    "https://rpc-megaeth-mainnet.globalstake.io",
                    "https://mainnet.megaeth.com/rpc",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &[
                    "https://mega.etherscan.io",
                    "https://megaeth.blockscout.com",
                ],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xcA4e254D95637DE95E2a2F79244b03380d697feD")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(1),
                finality_depth: 64,
            }),
            80094 => Some(Self {
                name: "Berachain",
                native_currency: NativeCurrency {
                    name: "BERA Token".into(),
                    symbol: "BERA".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://berachain-rpc.publicnode.com",
                    "https://berachain.drpc.org",
                    "https://rpc.berachain-apis.com",
                    "https://bera.api.pocket.network",
                    "https://berachain.rpc.sentio.xyz",
                    "https://rpc.berachain.com",
                    "https://80094.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://berascan.com"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x29d2fEC890B037B2d34f061F9a50f76F85ddBcAE")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(2),
                finality_depth: 8,
            }),
            4217 => Some(Self {
                name: "Tempo",
                native_currency: NativeCurrency {
                    name: "US Dollar".into(),
                    symbol: "USD".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://rpc.mainnet.tempo.xyz",
                    "https://tempo-mainnet.drpc.org",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://explore.tempo.xyz"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: None,
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(550),
                finality_depth: 8,
            }),
            480 => Some(Self {
                name: "World Chain",
                native_currency: NativeCurrency {
                    name: "Ether".into(),
                    symbol: "ETH".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://worldchain.drpc.org",
                    "https://worldchain-mainnet.g.alchemy.com/public",
                    "https://worldchain-mainnet.gateway.tenderly.co",
                    "https://sparkling-autumn-dinghy.worldchain-mainnet.quiknode.pro",
                    "https://480.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &[
                    "https://worldscan.org",
                    "https://worldchain-mainnet.explorer.alchemy.com",
                ],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x9d5d754a6397c01795Aaa8BBA565FEB99a4cEA6d")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(2),
                finality_depth: 64,
            }),
            1672 => Some(Self {
                name: "Pharos",
                native_currency: NativeCurrency {
                    name: "PharosCoin".into(),
                    symbol: "PROS".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://infra.originstake.com/pharos/evm",
                    "https://rpc.pharos.xyz",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://pharosscan.xyz"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0x9356c87a48f913d11c87a0d4b8cd16cd04624bf3")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_millis(1150),
                finality_depth: 8,
            }),
            999 => Some(Self {
                name: "HyperEVM",
                native_currency: NativeCurrency {
                    name: "HYPE".into(),
                    symbol: "HYPE".into(),
                    decimals: 18,
                },
                rpc_endpoints: [
                    "https://hyperevm.rpc.sentio.xyz",
                    "https://hyperliquid.drpc.org",
                    "https://hyperliquid-json-rpc.stakely.io",
                    "https://rpc.hyperliquid.xyz/evm",
                    "https://hyperliquid.rpc.blxrbdn.com",
                    "https://rpc.hypurrscan.io",
                    "https://999.rpc.thirdweb.com",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                explorer_urls: &["https://hyperevmscan.io"],
                multicall: Some(address!("0xcA11bde05977b3631167028862bE2a173976CA11")),
                native_usd_oracle: Some(address!("0xa8a94Da411425634e3Ed6C331a32ab4fd774aa43")),
                wrapped_native: crate::amounts::wrapped_native_token_for_chain(chain_id),
                block_time: Duration::from_secs(1),
                finality_depth: 8,
            }),
            _ => None,
        }
    }
}
