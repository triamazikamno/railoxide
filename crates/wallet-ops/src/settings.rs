use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use alloy::hex;
use alloy::primitives::{Address, FixedBytes, U256};
use local_db::DbStore;
use railgun_ui::TokenAnchorSource;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    BroadcasterFeePolicy, GAS_LIMIT_BUFFER, GAS_PRICE_BUFFER_DENOMINATOR,
    GAS_PRICE_BUFFER_NUMERATOR, PUBLIC_BROADCASTER_REPUBLISH_INTERVAL, PoiArtifactManifestSource,
    PoiArtifactSourceConfig, PoiProxyFallback, PoiReadSource, SensitiveUrl, WalletNetworkMode,
    public_balance_refresh_interval_secs,
};
use broadcaster_core::deployment::RailgunDeployment;
use sync_service::RailgunSyncOptions;
use waku::{RAILGUN_TREE, parse_multiaddr, parse_peer_id};

mod chain_editor;
mod core;
mod effective;
mod executors;
mod indexed_artifacts;
mod legacy;
mod mutation;
mod network_chains;
mod poi_broadcaster;
mod presets;
mod storage;
mod tokens_gas_waku;
mod validation;
mod walletconnect;

use validation::{
    parse_fixed_hex_32, validate_address, validate_enr_tree, validate_optional_address,
    validate_optional_non_empty, validate_optional_range, validate_range, validate_required_u64,
    validate_url_scheme, validate_waku_direct_peer,
};

pub use chain_editor::*;
pub use core::*;
pub use effective::*;
pub use executors::*;
pub use indexed_artifacts::*;
pub use mutation::*;
pub use network_chains::*;
pub use poi_broadcaster::*;
pub use storage::*;
pub use tokens_gas_waku::*;
pub use walletconnect::*;

#[cfg(test)]
mod tests;
