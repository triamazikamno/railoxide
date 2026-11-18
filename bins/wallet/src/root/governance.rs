use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256};
use futures_util::StreamExt;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable, FontWeight, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, WindowExt,
    alert::Alert,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    combobox::{Combobox, ComboboxEvent, ComboboxState},
    description_list::{DescriptionItem, DescriptionList},
    input::InputState,
    popover::Popover,
    scroll::{ScrollableElement, Scrollbar},
    searchable_list::{SearchableListItem, SearchableVec},
    spinner::Spinner,
    tab::{Tab, TabBar},
    table::{Column, DataTable, TableDelegate, TableState},
    tooltip::Tooltip,
};
use railgun_ui::{format_usd_micro_value, governance_contracts, short_address};
use rand::seq::SliceRandom as _;
use ui::clipboard::clipboard_with_toast;
use ui::controls::{
    app_button, app_button_base, app_button_label, app_input, app_muted_shimmer_text,
    app_muted_text, app_shimmer_text, app_strong_text, app_text,
};
use ui::dialog::WalletDialogExt as _;
use ui::format::format_compact_duration;
use ui::theme::{self, APP_MONO_FONT_FAMILY};
use wallet_ops::{
    AccountStakeResult, GovernorRewardsIntervalMetadata, RewardEvidence, StakePosition, StakeState,
    StakingGlobalMetrics, TokenAnchorRateCache, fetch_account_snapshots, fetch_account_stakes,
    fetch_interval_metadata, fetch_reward_batch_evidence, fetch_reward_evidence,
    fetch_reward_evidence_multi, fetch_staking_global_metrics, validate_governance_deployment,
    vault::{PublicAccountMetadata, PublicAccountStatus},
};
use wallet_ops::{
    GovernanceActionContext, GovernanceActionIntent, GovernanceContractKind,
    PublicActionGasFeeSelection, PublicAdvancedTransactionEstimateRequest,
    fetch_governance_token_balance_allowance, plan_delegate, plan_principal_claim, plan_stake,
    plan_undelegate, plan_unlock,
};

use super::gas_fee::{
    Eip1559GasFeeEditTarget, Eip1559GasFeeEditorState, Eip1559GasFeeMode, Eip1559GasFeeTarget,
    render_eip1559_gas_fee_editor,
};
use super::governance_action::{
    GovernanceContinuation, GovernanceDraftRecipe, GovernanceRefreshTarget, GovernanceSpendDraft,
    GovernanceStakingReviewProjection, ProposalActionKind, ProposalActionSelection,
    ProposalParticipationKey, ProposalParticipationState, build_typed_governance_spend_draft,
};
use super::participant::normalize_participant_ids;
use super::proposals::{format_compact_rail_amount, format_date_short};
use super::public_account::public_account_display_label;
use super::public_balances::isolated_read_jitter;
use super::tokens::{
    format_send_amount_input, format_token_amount_for_display, token_display_metadata,
};
use super::ui_helpers::token_label_row;
use super::{WalletRoot, app_refresh_button, app_status_tag};
use crate::assets::{PIGGY_BANK_ICON_PATH, RailgunSidebarIcon, USERS_ICON_PATH, WalletIconSource};

mod governance_state;
mod participation;
mod reward_claim_form;
mod reward_planning;
mod stake_table;
mod staking_action_form;
mod staking_actions;
mod staking_refresh;
mod workspace_render;

pub(super) use governance_state::*;
use reward_claim_form::render_reward_claim_selection_form;
pub(super) use reward_planning::*;
pub(super) use stake_table::*;
use staking_action_form::render_staking_action_form;

const GOVERNANCE_HEADER_HEIGHT: gpui::Pixels = px(52.0);
const GOVERNANCE_COMPACT_HEADER_BREAKPOINT: gpui::Pixels = px(1100.0);
const GOVERNANCE_PARTICIPANT_TRIGGER_WIDTH: gpui::Pixels = px(200.0);
const GOVERNANCE_COMPACT_PARTICIPANT_TRIGGER_WIDTH: gpui::Pixels = px(120.0);
const GOVERNANCE_PARTICIPANT_POPUP_WIDTH: gpui::Pixels = px(280.0);
const GOVERNANCE_CONTENT_WIDTH: gpui::Pixels = px(1080.0);
const GOVERNANCE_TIME_TICK: Duration = Duration::from_mins(1);
const STAKING_TABLE_COMPACT_BREAKPOINT: gpui::Pixels = px(720.0);
const STAKING_TABLE_COMPACT_ACTIONS_WIDTH: gpui::Pixels = px(224.0);
const STAKING_ACCOUNT_HEADER_STACK_BREAKPOINT: gpui::Pixels = px(860.0);
const STAKING_COLLAPSED_SIDEBAR_WIDTH: gpui::Pixels = px(48.0);
const STAKING_CARD_HORIZONTAL_CHROME: gpui::Pixels = px(34.0);
const STAKING_TABLE_STAKE_WIDTH: gpui::Pixels = px(76.0);
const STAKING_TABLE_AMOUNT_WIDTH: gpui::Pixels = px(120.0);
const STAKING_TABLE_COMPACT_DELEGATE_MIN_WIDTH: gpui::Pixels = px(88.0);
const STAKING_TABLE_STATE_WIDTH: gpui::Pixels = px(118.0);
const STAKING_TABLE_ACTIONS_WIDTH: gpui::Pixels = px(288.0);
const STAKING_TABLE_MIN_WIDTH: gpui::Pixels = px(76.0 + 120.0 + 88.0 + 118.0 + 224.0);
const STAKING_OVERVIEW_ROW_HEIGHT: gpui::Pixels = px(40.0);
const STAKING_TABLE_SCROLLBAR_WIDTH: gpui::Pixels = px(16.0);
const STAKING_REWARD_EVIDENCE_CONCURRENCY: usize = 4;
const STAKING_REWARD_EVIDENCE_TTL: Duration = Duration::from_hours(1);
/// Rapid clicks in the claim selection form cost one estimate, not one per click.
const REWARD_SELECTION_ESTIMATE_DEBOUNCE: Duration = Duration::from_millis(300);
const REWARD_SELECTION_DIALOG_WIDTH: gpui::Pixels = px(520.0);
const STAKING_ACTION_DIALOG_WIDTH: gpui::Pixels = px(440.0);
const REWARD_SELECTION_CHECKBOX_WIDTH: gpui::Pixels = px(20.0);
const REWARD_SELECTION_ASSET_WIDTH: gpui::Pixels = px(100.0);
const REWARD_SELECTION_FEE_WIDTH: gpui::Pixels = px(92.0);
const REWARD_SELECTION_NET_WIDTH: gpui::Pixels = px(80.0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RewardIntervalCountdown {
    interval: U256,
    boundary: U256,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum GovernanceTab {
    #[default]
    Proposals,
    Staking,
}

/// The shared route for one traffic class of `chain_id`, as used by dashboard General reads.
pub(super) fn governance_route(
    chain_id: u64,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    class: wallet_ops::RpcTrafficClass,
) -> Result<wallet_ops::ClassRoute, String> {
    wallet_ops::settings::resolve_effective_chain_rpc_route(chain_id, effective_chain)
        .map_err(|error| error.to_string())?
        .for_class(class)
        .map_err(|error| error.to_string())
}

/// Opens an isolated Sensitive route for one participant or acting account. A failure is
/// returned as is; there is no fallback to the shared route.
pub(super) async fn open_isolated_governance_route(
    chain_id: u64,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
) -> Result<wallet_ops::ClassRoute, String> {
    let route = governance_route(
        chain_id,
        effective_chain,
        wallet_ops::RpcTrafficClass::Sensitive,
    )?;
    let client = http
        .operation_http_client()
        .await
        .map_err(|error| error.to_string())?;
    Ok(route.isolated(client))
}

/// The acting account's Sensitive route over its action's operation client, and that client for
/// the action's estimates and submissions. A failure is returned as is, without a shared-route
/// fallback.
pub(super) async fn action_governance_route(
    chain_id: u64,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    operation: &wallet_ops::OperationHttpClientSlot,
) -> Result<(wallet_ops::ClassRoute, wallet_ops::OperationHttpClient), String> {
    let route = governance_route(
        chain_id,
        effective_chain,
        wallet_ops::RpcTrafficClass::Sensitive,
    )?;
    let client = operation
        .client()
        .await
        .map_err(|error| error.to_string())?;
    Ok((route.isolated(client.clone()), client))
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct GovernanceParticipantIdentity {
    pub uuid: String,
    pub address: Address,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BulkRewardReadiness {
    tokens: Vec<Address>,
    ready: bool,
}

#[derive(Clone, Debug)]
pub(super) enum RewardEvidenceMode {
    Fresh,
    CachedInitial {
        evidence: Box<wallet_ops::RewardBatchEvidence>,
        /// Steps already planned against this evidence by the selection form. Preflight validates
        /// them again and plans fresh ones when they are absent.
        steps: Option<Vec<wallet_ops::RewardClaimStep>>,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct GovernanceContextKey {
    pub wallet_id: Option<String>,
    pub chain_id: u64,
    pub participants: Vec<GovernanceParticipantIdentity>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ParticipantSummary {
    pub selected: usize,
    pub inactive: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ParticipantChoice {
    pub uuid: String,
    pub label: String,
    pub address: Address,
    pub global: bool,
    pub inactive: bool,
}

pub(super) type ParticipantPickerState = ComboboxState<SearchableVec<ParticipantChoice>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StakingAccountView {
    pub voting_power: Result<U256, Arc<str>>,
    pub balance: Result<U256, Arc<str>>,
    pub stakes: Result<Vec<StakePosition>, Arc<str>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RewardView {
    Zero,
    Positive {
        amount: U256,
        evidence: Box<RewardEvidence>,
    },
    Unavailable(Arc<str>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RewardUsdState {
    Loading,
    Unavailable,
    Value(U256),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RewardFeePresentation {
    Pending,
    Estimated(RewardFeeEstimate),
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RewardFeeEstimate {
    /// Fee text without the leading estimate marker, in USD when the native token is priced and
    /// in native units otherwise.
    label: String,
    /// Micro-USD value of the estimated fee when the native token has a cached rate.
    usd: Option<U256>,
    /// Whether the added fee exceeds the reward's value.
    below_fee: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum StakingRefreshStatus {
    #[default]
    Idle,
    Loading,
    Fresh,
    Stale,
    Error(Arc<str>),
}

#[derive(Clone, Debug)]
pub(super) struct StakingReadState {
    pub key: Option<GovernanceContextKey>,
    pub generation: u64,
    pub status: StakingRefreshStatus,
    pub global_ready: bool,
    pub current_account_ids: BTreeSet<String>,
    pub current_reward_keys: BTreeSet<(String, Address)>,
    pub metrics: Option<StakingGlobalMetrics>,
    pub accounts: BTreeMap<String, StakingAccountView>,
    pub rewards: BTreeMap<(String, Address), RewardView>,
    bulk_reward_readiness: BTreeMap<String, BulkRewardReadiness>,
    bulk_reward_evidence: BTreeMap<String, (wallet_ops::RewardBatchEvidence, Instant)>,
    positive_reward_captured_at: BTreeMap<(String, Address), Instant>,
    current_bulk_reward_keys: BTreeSet<String>,
    pub chain_time_anchor: Option<(U256, Instant)>,
    reward_interval_countdown: Option<RewardIntervalCountdown>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct StakingAccountProjection {
    unclaimed_principal: U256,
    active_self_delegated: U256,
    active_external_delegated: U256,
}

pub(super) struct GovernanceState {
    pub tab: GovernanceTab,
    pub staking: StakingReadState,
    pub proposal_participation: ProposalParticipationState,
    /// The latest participation reader and whether it has finished.
    proposal_participation_reader: Option<(gpui::Task<()>, Rc<Cell<bool>>)>,
    pub proposal_action_amount_input: Entity<InputState>,
    pub action_flow: GovernanceActionFlowState,
    pub staking_delegate_input: Entity<InputState>,
    pub reward_gas_fee: Eip1559GasFeeEditorState,
    pub compact_position_details: Option<(String, U256)>,
    pub participant_picker: Entity<ParticipantPickerState>,
    participant_picker_wallet: Option<String>,
    participant_picker_items: Vec<ParticipantChoice>,
    staking_tables: BTreeMap<String, Entity<TableState<StakeTableDelegate>>>,
    participant_time_tick_generation: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GovernanceActionSelection {
    Proposal(ProposalActionSelection),
    Staking(StakingActionSelection),
}

/// What the claim selection form knows about the cost of the exact selected call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RewardSelectionEstimateState {
    Pending,
    Estimated(Box<RewardSelectionEstimate>),
    Unavailable(Arc<str>),
}

/// The planned selected claim: the evidence it was planned from, the steps it splits into, and
/// the fee those steps were estimated at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RewardSelectionEstimate {
    pub evidence: Option<wallet_ops::RewardBatchEvidence>,
    pub steps: Vec<wallet_ops::RewardClaimStep>,
    pub expected_fee_per_gas: u128,
    pub added_gas: BTreeMap<Address, Option<u64>>,
    pub shared_gas: Option<u64>,
}

#[derive(Clone, Default)]
pub(super) struct GovernanceActionFlowState {
    pub selection: Option<GovernanceActionSelection>,
    pub recipe: Option<GovernanceDraftRecipe>,
    pub draft: Option<GovernanceSpendDraft>,
    pub pending: bool,
    pub error: Option<Arc<str>>,
    pub generation: u64,
    pub reward_selection_estimate: Option<RewardSelectionEstimateState>,
    pub reward_selection_generation: u64,
    pub seed_reward_selection: bool,
    /// The open action's operation client and the actor it was opened for. A submitted draft
    /// keeps its own clone through its last step.
    pub operation: Option<(Address, wallet_ops::OperationHttpClientSlot)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum StakingActionKind {
    Stake,
    Delegate {
        stake_id: U256,
    },
    Undelegate {
        stake_id: U256,
    },
    Unlock {
        stake_id: U256,
    },
    PrincipalClaim {
        stake_id: U256,
    },
    /// The live selection of positive reward tokens, ascending by numeric address. It may be
    /// empty while the selection form is open; review and the draft builder reject that.
    RewardClaimSelected {
        tokens: Vec<Address>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StakingActionSelection {
    pub actor_uuid: String,
    pub actor: Address,
    pub kind: StakingActionKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StakeTableAction {
    Delegate,
    Undelegate,
    Unlock,
    PrincipalClaim,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StakeTableRow {
    id: U256,
    amount: String,
    created: String,
    delegate: Address,
    delegate_label: String,
    state: String,
    state_detail: String,
    externally_delegated: bool,
    actions: Vec<StakeTableAction>,
    disabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StakeTableColumnKind {
    Stake,
    Amount,
    Delegate,
    State,
    Actions,
}

#[derive(Clone, Debug)]
struct StakingTableLayout {
    compact: bool,
    columns: Vec<(StakeTableColumnKind, Column)>,
}

struct StakeTableDelegate {
    root: WeakEntity<WalletRoot>,
    actor_uuid: String,
    actor: Address,
    rows: Arc<[StakeTableRow]>,
    columns: Vec<(StakeTableColumnKind, Column)>,
    compact: bool,
    pending_keyboard_focus: Option<U256>,
}

/// One positive reward row of the claim selection form, with everything the row, the totals, and
/// the default selection read.
struct RewardSelectionRow {
    token: Address,
    symbol: String,
    icon_path: Option<WalletIconSource>,
    amount: String,
    usd: RewardUsdState,
    fee: RewardFeePresentation,
    gas: Option<u64>,
    selected: bool,
}

#[cfg(test)]
mod tests;
