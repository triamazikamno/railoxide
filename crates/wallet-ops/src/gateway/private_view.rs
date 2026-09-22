//! Desktop-formatted private presentation for authenticated extension UI only.
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayPrivateView {
    pub selected_wallet: Option<String>,
    pub selected_wallet_choice: Option<String>,
    pub receive_address: Option<String>,
    pub wallets: Vec<GatewayPrivateWallet>,
    #[serde(serialize_with = "railgun_ui::chain_id::optional::serialize")]
    pub selected_chain: Option<u64>,
    /// Asset to unwrap by default on the selected chain, when the preference is enabled.
    pub default_unwrap_asset: Option<String>,
    pub selection_message: Option<String>,
    pub state: GatewayPrivateChainState,
    pub message: Option<String>,
    pub stage_label: Option<String>,
    pub percent: Option<u8>,
    pub forms_available: bool,
    pub generation_ready: bool,
    pub total: Option<String>,
    pub assets: Vec<GatewayPrivateAsset>,
    pub pending: Option<GatewayPrivatePending>,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayPrivateWallet {
    pub wallet_id: String,
    pub label: String,
    pub hardware: Option<String>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayPrivateChainState {
    #[default]
    Idle,
    Loading,
    Syncing,
    Ready,
    Error,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayPrivateAsset {
    pub asset: String,
    pub symbol: String,
    pub amount: String,
    pub usd: Option<String>,
    pub icon: Option<String>,
    pub pending_verification: Option<String>,
    pub pending_incoming: Option<String>,
    pub pending_outgoing: Option<String>,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayPrivatePending {
    pub title: String,
    pub detail: Option<String>,
    pub categories: Vec<GatewayPrivatePendingCategory>,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayPrivatePendingCategory {
    pub title: String,
    pub count: String,
    pub detail: String,
    pub assets: Vec<GatewayPrivatePendingAmount>,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayPrivatePendingAmount {
    pub label: String,
    pub amount: String,
    pub shield_wait: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayPrivateCommand {
    SelectWallet { wallet_id: String },
}
