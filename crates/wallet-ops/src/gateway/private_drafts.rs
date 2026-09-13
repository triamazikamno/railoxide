//! Private draft inputs and display records. Native owners retain notes and signing authority.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayPrivateDraftKind {
    #[default]
    PrivateSend,
    Unshield,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayPrivateFeeMode {
    #[default]
    Deduct,
    AddOnTop,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayBroadcasterChoice {
    #[default]
    Random,
    Specific {
        id: String,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateDraftInput {
    pub wallet: String,
    pub chain_id: u64,
    pub kind: GatewayPrivateDraftKind,
    pub asset: String,
    pub amount: String,
    pub max: bool,
    pub recipient: String,
    pub address_book_entry: Option<String>,
    pub fee_mode: GatewayPrivateFeeMode,
    pub unwrap: bool,
    pub native_top_up: bool,
    #[serde(flatten)]
    pub delivery: GatewayPrivateDelivery,
}

/// The legacy broadcaster fields keep their flat wire shape. Each delivery rejects
/// fields belonging to another mode, including endpoint overrides.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub enum GatewayPrivateDelivery {
    Broadcaster(GatewayPrivateBroadcasterInput),
    SelfBroadcast {
        delivery: GatewayPrivateSelfBroadcastInput,
    },
}

impl Default for GatewayPrivateDelivery {
    fn default() -> Self {
        Self::Broadcaster(GatewayPrivateBroadcasterInput::default())
    }
}

impl GatewayPrivateDelivery {
    #[must_use]
    pub const fn broadcaster(&self) -> Option<&GatewayPrivateBroadcasterInput> {
        match self {
            Self::Broadcaster(input) => Some(input),
            Self::SelfBroadcast { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct GatewayPrivateBroadcasterInput {
    pub fee_token: String,
    pub broadcaster: GatewayBroadcasterChoice,
    pub allow_out_of_range: bool,
    pub favorites_only: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayPrivateSelfBroadcastInput {
    SelfBroadcast {
        signer: Option<String>,
        funding: GatewayPrivateFunding,
        fee: GatewayPrivateGasFee,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayPrivateFunding {
    PublicBalance {},
    Sponsorship { incentive: GatewayPrivateIncentive },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayPrivateIncentive {
    Economy {},
    Standard {},
    Priority {},
    Custom { percent: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayPrivateGasFee {
    Auto {},
    Custom {
        max_fee_gwei: String,
        priority_fee_gwei: String,
    },
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateDisplayRow {
    pub label: String,
    pub value: String,
    pub suffix: Option<String>,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateDraftEstimate {
    pub amount: String,
    pub amount_label: String,
    pub max_amount: Option<String>,
    pub max_amount_label: Option<String>,
    pub recipient: Option<String>,
    pub broadcaster: String,
    pub shape: String,
    pub outcome: Vec<GatewayPrivateDisplayRow>,
    pub transaction_fee: String,
    pub fee_breakdown: Vec<GatewayPrivateDisplayRow>,
    pub network_gas: String,
    pub context: Vec<GatewayPrivateDisplayRow>,
    pub warnings: Vec<String>,
    /// Formatted presentation from the shared self-broadcast fee model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_broadcast_fees: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_fee_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayPrivateDraftResult {
    Submitted,
    Confirmed,
    Reverted,
    InclusionUnknown,
    Failed,
    TimedOut,
    Stopped,
    Cancelled,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateDraftProgress {
    pub execution_id: String,
    pub summary: String,
    pub result: Option<GatewayPrivateDraftResult>,
    pub context: Vec<GatewayPrivateDisplayRow>,
    pub transaction_hash: Option<String>,
    pub stop: bool,
    pub stop_retries: bool,
    pub stop_waiting: bool,
    pub ban: bool,
    pub favorite: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayPrivateDraftControl {
    Stop,
    StopWaiting,
    Ban,
    Favorite,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateAssetChoice {
    pub id: String,
    pub label: String,
    pub available: String,
    pub max_amount: String,
    pub max_amount_label: String,
    pub broadcaster_count: usize,
    pub icon: Option<String>,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateTopUpOption {
    pub label: String,
    pub funding_detail: String,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateDraftOptions {
    pub assets: Vec<GatewayPrivateAssetChoice>,
    pub metrics: Vec<GatewayPrivateAmountMetric>,
    pub warnings: Vec<String>,
    pub fee_tokens: Vec<GatewayPrivateAssetChoice>,
    pub show_fee_mode: bool,
    pub unwrap_labels: Option<[String; 2]>,
    pub native_top_up: Option<GatewayPrivateTopUpOption>,
    pub candidate_count: usize,
    pub specific_label: String,
    pub picker: Option<GatewayPrivateDraftPicker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_broadcast: Option<GatewayPrivateSelfBroadcastOptions>,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateSelfBroadcastOptions {
    pub signers: Vec<GatewayPrivateSignerChoice>,
    pub default_signer: Option<String>,
    pub show_sponsorship: bool,
    pub sponsorship_unavailable: Option<String>,
    pub signer_error: Option<String>,
    pub gas_error: Option<String>,
    pub incentive_error: Option<String>,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateSignerChoice {
    pub id: String,
    pub label: String,
    pub address_label: String,
    pub balance_label: String,
    pub random_candidate: bool,
    pub unavailable: Option<String>,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateDraftPicker {
    pub view_id: String,
    pub query: String,
    pub total_count: usize,
    /// Render-only records serialized by the shared UI model. Never accepted as commands.
    pub rows: Vec<serde_json::Value>,
}

#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct GatewayPrivateAmountMetric {
    pub label: String,
    pub value: String,
    pub amount: String,
}
