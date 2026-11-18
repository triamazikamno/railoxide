use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{Semaphore, mpsc, oneshot};

use alloy::primitives::{Address, B256, FixedBytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use chrono::{DateTime, Local, Utc};
use gpui::{
    App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, WeakEntity,
    Window, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, WindowExt,
    alert::Alert,
    button::{ButtonCustomVariant, ButtonVariants},
    collapsible::Collapsible,
    list::{List, ListDelegate, ListItem, ListState},
    progress::Progress,
    scroll::ScrollableElement,
    skeleton::Skeleton,
    spinner::Spinner,
    tab::{Tab, TabBar},
    text::TextView,
    tooltip::Tooltip,
};
use markdown::{ParseOptions, mdast::Node, to_mdast};
use railgun_ui::short_address;
use ui::clipboard::clipboard_with_toast;
use ui::controls::{
    app_button, app_button_base, app_input, app_muted_shimmer_text, app_muted_text,
    app_strong_text, app_text,
};
use ui::format::format_compact_duration;
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{
    ClassRoute, GovernanceContractRules, GovernanceDocument, GovernanceOverview,
    GovernanceProposal, GovernanceProposalStage, GovernanceProposalStatus, HttpContext,
    RpcTrafficClass, TokenAnchorRateCache, derive_governance_proposal_status,
    fetch_governance_chain_time, fetch_governance_overview, fetch_governance_page,
    resolve_governance_document,
    settings::{EffectiveChainConfig, load_wallet_settings},
    vault::{DesktopVaultStore, PublicAccountMetadata, PublicAddressBookEntry},
};

use super::governance::governance_route;
use super::governance_action::{
    ProposalActionKind, ProposalParticipationRow, proposal_participation_key,
    validate_proposal_action,
};
use super::markdown_sanitize::{max_backtick_run, sanitize_render_source};
use super::spend_authorization::spend_authorization_recipient_display;
use super::tokens::{
    format_native_token_amount_for_display, format_send_amount_input,
    format_token_amount_for_display, native_wrapped_output_labels, token_display_metadata,
};
use super::{WalletRoot, app_status_tag, format_report_chain};
use crate::assets::{RailgunActionIcon, WalletIconSource};

mod action_decoding;
mod action_form;
mod decoded_preview_render;
mod markdown_tables;
mod participation_card;
mod proposal_cards;
mod proposal_format;
mod proposal_loading;
mod proposal_render;
mod proposals_lifecycle;
mod proposals_state;
mod proposals_view;

#[cfg(test)]
use action_decoding::proposal_role_label;
use action_decoding::{
    action_calldata_hex, compact_calldata_display, decode_proposal_action, proposal_action_id,
    proposal_action_target_label, proposal_decoded_preview,
};
use action_form::render_proposal_action_form;
use decoded_preview_render::render_proposal_decoded_preview;
#[cfg(test)]
use markdown_tables::{
    ensure_mdast_node_limit, parse_proposal_blocks, prepared_markdown_source_len, table_fingerprint,
};
use markdown_tables::{inert_raw_fallback_source, prepare_proposal_presentation};
pub(super) use participation_card::*;
use proposal_cards::{
    proposal_card, proposal_document_card, render_details_card, render_proposal_actions_card,
    render_sponsorship_card, render_timeline_card, render_votes_card, sponsorship_per_mille,
};
#[cfg(test)]
use proposal_cards::{timeline_deadline_completed, timeline_sponsorship_completed};
pub(super) use proposal_format::*;
pub(super) use proposal_loading::*;
use proposal_render::{
    governance_permissionless_warning, proposal_address_badge, proposal_detail_title,
    proposal_row_meta, proposal_row_title, proposal_skeleton_row, proposal_stage_color,
    proposals_empty_state, render_proposal_blocks, render_proposal_detail_tabs,
};

/// Keeps the tracing target of the moved functions at this module's path.
const LOG_TARGET: &str = module_path!();

pub(super) const PROPOSALS_PAGE_SIZE: usize = 5;
const DOCUMENT_RESOLUTION_CONCURRENCY: usize = 4;
const CONTENT_WIDTH: gpui::Pixels = px(1080.0);
const MAX_MDAST_NODES: usize = 4_096;
const MAX_RENDER_COMPLEXITY: usize = 256;
const MAX_PREPARED_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_TABLE_COLUMN_LENGTH: usize = 5;
const MAX_TABLE_COLUMN_LENGTH: usize = 150;
const TABLE_COLUMN_CHROME_PX: usize = 17;
const TABLE_OUTER_BORDER_PX: usize = 2;
const MIN_TABLE_RENDER_WIDTH_PX: usize = 1;
const MAX_TABLE_RENDER_WIDTH_PX: usize = 4_096;

sol! {
    interface ProposalErc20 {
        function transfer(address recipient, uint256 amount) external;
        function transferFrom(address from, address to, uint256 amount) external;
        function approve(address spender, uint256 amount) external;
    }

    interface ProposalTreasury {
        function transferERC20(address token, address to, uint256 amount) external;
        function transferETH(address to, uint256 amount) external;
        function initializeTreasury(address owner) external;
        function grantRole(bytes32 role, address account) external;
        function revokeRole(bytes32 role, address account) external;
        function renounceRole(bytes32 role, address account) external;
    }

    interface ProposalOpStackSender {
        function readyTask(uint256 taskId) external;
        function setExecutorL2(address executor) external;
    }

    interface ProposalOwnable {
        function transferOwnership(address newOwner) external;
        function renounceOwnership() external;
    }

    interface ProposalWrappedNative {
        function deposit() external;
        function withdraw(uint256 amount) external;
    }

    interface ProposalProxyAdmin {
        function upgrade(address proxy, address implementation) external;
        function pause(address proxy) external;
        function transferProxyOwnership(address proxy, address newOwner) external;
    }

    interface ProposalRailgun {
        function changeFee(uint120 shieldFee, uint120 unshieldFee, uint256 nftFee) external;
    }

    interface ProposalGovernanceToken {
        function governanceMint(address account, uint256 amount) external;
    }

    interface ProposalGovernorRewards {
        function setIntervalBP(uint256 newIntervalBP) external;
        function addTokens(address[] tokens) external;
    }

    interface ProposalDelegator {
        function setPermission(
            address caller,
            address contractAddress,
            bytes4 selector,
            bool permission
        ) external;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DecodedProposalAction {
    Erc20Transfer {
        recipient: Address,
        amount: U256,
    },
    Erc20TransferFrom {
        from: Address,
        to: Address,
        amount: U256,
    },
    Erc20Approve {
        spender: Address,
        amount: U256,
    },
    TreasuryTransferErc20 {
        token: Address,
        to: Address,
        amount: U256,
    },
    TreasuryTransferEth {
        to: Address,
        amount: U256,
    },
    TreasuryInitialize {
        owner: Address,
    },
    TreasuryGrantRole {
        role: B256,
        account: Address,
    },
    TreasuryRevokeRole {
        role: B256,
        account: Address,
    },
    TreasuryRenounceRole {
        role: B256,
        account: Address,
    },
    OpStackReadyTask {
        task_id: U256,
    },
    OpStackSetExecutorL2 {
        executor: Address,
    },
    TransferOwnership {
        new_owner: Address,
    },
    WrappedDeposit,
    WrappedWithdraw {
        amount: U256,
    },
    ProxyUpgrade {
        proxy: Address,
        implementation: Address,
    },
    ProxyPause {
        proxy: Address,
    },
    ProxyTransferOwnership {
        proxy: Address,
        new_owner: Address,
    },
    ChangeFee {
        shield_fee: U256,
        unshield_fee: U256,
        nft_fee: U256,
    },
    GovernanceMint {
        account: Address,
        amount: U256,
    },
    SetIntervalBP {
        new_interval_bp: U256,
    },
    AddTokens {
        tokens: Vec<Address>,
    },
    DelegatorSetPermission {
        caller: Address,
        contract_address: Address,
        selector: FixedBytes<4>,
        permission: bool,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ProposalIdentity {
    contract_version: wallet_ops::GovernanceContractVersion,
    contract_address: alloy::primitives::Address,
    index: U256,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ProposalDetailTab {
    #[default]
    Description,
    Actions,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProposalActionIdentity {
    proposal: ProposalIdentity,
    ordinal: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProposalDocumentState {
    Pending,
    Resolved {
        document: GovernanceDocument,
        presentation: ProposalPresentation,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedProposal {
    pub proposal: GovernanceProposal,
    pub rules: GovernanceContractRules,
    document: ProposalDocumentState,
}

#[derive(Clone, Debug)]
struct ChainTimeAnchor {
    chain_time: U256,
    captured_at: Instant,
}

#[derive(Clone, Debug)]
struct ProposalsPage {
    rows: Arc<Vec<ResolvedProposal>>,
}

pub(super) struct ProposalsState {
    pub chain_id: u64,
    pub generation: u64,
    pub request_generation: u64,
    pub checked: bool,
    pub overview: Option<GovernanceOverview>,
    chain_time_anchor: Option<ChainTimeAnchor>,
    pub total_pages: usize,
    pub current_page: usize,
    pages: BTreeMap<usize, ProposalsPage>,
    loading_pages: BTreeSet<usize>,
    hydrating_pages: BTreeSet<usize>,
    prefetch_page: Option<usize>,
    active_page_token: u64,
    prefetch_token: u64,
    proposal_lists: BTreeMap<usize, Entity<ListState<ProposalListDelegate>>>,
    focus_list_on_render: bool,
    pub(super) table_scroll_handles: BTreeMap<String, ScrollHandle>,
    expanded_calldata: BTreeSet<ProposalActionIdentity>,
    detail_scroll_handle: ScrollHandle,
    pub loading: bool,
    pub refreshing: bool,
    pub error: Option<Arc<str>>,
    pub selected: Option<ProposalSelection>,
    time_tick_active: bool,
    task_tracker: ProposalTaskTracker,
    active_page_task_tracker: ProposalTaskTracker,
    prefetch_task_tracker: ProposalTaskTracker,
    document_semaphore: Arc<Semaphore>,
}

struct ProposalListDelegate {
    root: WeakEntity<WalletRoot>,
    selected: Option<gpui_component::IndexPath>,
}

#[derive(Clone, Debug)]
pub(super) struct ProposalSelection {
    page: usize,
    identity: ProposalIdentity,
    tab: ProposalDetailTab,
    participation_expanded: bool,
}

struct ProposalTaskTracker {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

pub(super) struct ProposalCleanup {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

pub(super) struct InitialLoad {
    overview: Option<GovernanceOverview>,
    chain_time_anchor: Option<ChainTimeAnchor>,
    total_pages: usize,
    page: usize,
    rows: Vec<ResolvedProposal>,
}

struct DocumentCompletion {
    identity: ProposalIdentity,
    document: GovernanceDocument,
    presentation: ProposalPresentation,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProposalCapacityKind {
    Sponsorship,
    Voting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProposalCapacitySummaryState {
    Empty,
    Loading,
    NotLoaded,
    Unavailable,
    Full,
    Partial,
    Exhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProposalClosedHistoryKind {
    Sponsorship,
    Voting,
}
enum ProposalRowTitle {
    Available(String),
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProposalBlock {
    Markdown(String),
    Table(ProposalTable),
    List(ProposalList),
    Blockquote(ProposalBlockquote),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedProposal {
    blocks: Vec<ProposalBlock>,
    table_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProposalPresentation {
    Prepared(Arc<PreparedProposal>),
    RawParseFallback(String),
    TooComplex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProposalPreparationFailure {
    RawParseFallback,
    TooComplex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalBlockquote {
    blocks: Vec<ProposalBlock>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalTable {
    source: String,
    render_source: String,
    ordinal: usize,
    render_width_px: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalList {
    ordered: bool,
    start: Option<u32>,
    items: Vec<ProposalListItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalListItem {
    checked: Option<bool>,
    blocks: Vec<ProposalBlock>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TableFingerprint {
    alignment: Vec<markdown::mdast::AlignKind>,
    row_cell_counts: Vec<usize>,
}

#[derive(Default)]
struct PreparationBudget {
    weighted_complexity: usize,
    prepared_source_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalDecodedHero {
    rounded: String,
    icon: Option<WalletIconSource>,
    context: Option<String>,
    amount_copy_value: Option<String>,
    context_copy_value: Option<String>,
    monospace: bool,
    danger: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProposalDecodedDetail {
    Party {
        role: String,
        address: Address,
        copy_value: String,
        badge: Option<String>,
    },
    Connector,
    Value {
        role: String,
        value: String,
        copy_value: Option<String>,
        monospace: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalDecodedPreview {
    verb: String,
    hero: Option<ProposalDecodedHero>,
    details: Vec<ProposalDecodedDetail>,
    unlimited_warning: Option<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimelineDeadline {
    SponsorshipClose,
    VotingOpen,
    YayVotingEnd,
    NayVotingEnd,
    ExecutionOpen,
    ExecutionClose,
}

#[cfg(test)]
mod tests;
