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
    button::ButtonVariants,
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
    app_button, app_button_base, app_input, app_muted_text, app_strong_text, app_text,
};
use ui::format::format_compact_duration;
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{
    GovernanceContractRules, GovernanceDocument, GovernanceOverview, GovernanceProposal,
    GovernanceProposalStage, GovernanceProposalStatus, HttpContext, TokenAnchorRateCache,
    derive_governance_proposal_status, fetch_governance_chain_time, fetch_governance_overview,
    fetch_governance_page, resolve_governance_document,
    settings::{EffectiveChainConfig, load_wallet_settings},
    vault::{DesktopVaultStore, PublicAccountMetadata, PublicAddressBookEntry},
};

use super::governance_action::{
    ProposalActionKind, ProposalParticipationRow, proposal_participation_key,
    validate_proposal_action,
};
use super::spend_authorization::spend_authorization_recipient_display;
use super::tokens::{
    format_native_token_amount_for_display, format_send_amount_input,
    format_token_amount_for_display, native_wrapped_output_labels, token_display_metadata,
};
use super::{WalletRoot, app_status_tag, format_report_chain};
use crate::assets::{RailgunActionIcon, WalletIconSource};

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

impl DecodedProposalAction {
    const fn method_name(&self) -> &'static str {
        match self {
            Self::Erc20Transfer { .. } => "transfer",
            Self::Erc20TransferFrom { .. } => "transferFrom",
            Self::Erc20Approve { .. } => "approve",
            Self::TreasuryTransferErc20 { .. } => "transferERC20",
            Self::TreasuryTransferEth { .. } => "transferETH",
            Self::TreasuryInitialize { .. } => "initializeTreasury",
            Self::TreasuryGrantRole { .. } => "grantRole",
            Self::TreasuryRevokeRole { .. } => "revokeRole",
            Self::TreasuryRenounceRole { .. } => "renounceRole",
            Self::OpStackReadyTask { .. } => "readyTask",
            Self::OpStackSetExecutorL2 { .. } => "setExecutorL2",
            Self::TransferOwnership { .. } => "transferOwnership",
            Self::WrappedDeposit => "deposit",
            Self::WrappedWithdraw { .. } => "withdraw",
            Self::ProxyUpgrade { .. } => "upgrade",
            Self::ProxyPause { .. } => "pause",
            Self::ProxyTransferOwnership { .. } => "transferProxyOwnership",
            Self::ChangeFee { .. } => "changeFee",
            Self::GovernanceMint { .. } => "governanceMint",
            Self::SetIntervalBP { .. } => "setIntervalBP",
            Self::AddTokens { .. } => "addTokens",
            Self::DelegatorSetPermission { .. } => "setPermission",
        }
    }

    const fn contract_family_label(&self) -> Option<&'static str> {
        match self {
            Self::OpStackReadyTask { .. } | Self::OpStackSetExecutorL2 { .. } => {
                Some("OpStack sender")
            }
            Self::ProxyUpgrade { .. }
            | Self::ProxyPause { .. }
            | Self::ProxyTransferOwnership { .. } => Some("Proxy admin"),
            _ => None,
        }
    }
}

fn decode_exact_call<C: SolCall>(calldata: &[u8]) -> Option<C> {
    let call = C::abi_decode_validate(calldata).ok()?;
    (call.abi_encode().as_slice() == calldata).then_some(call)
}

fn decode_proposal_action(
    chain_id: u64,
    target: Address,
    calldata: &[u8],
    wrapped_native_token: Option<Address>,
    railgun_contract: Option<Address>,
) -> Option<DecodedProposalAction> {
    if railgun_ui::governance_treasury(chain_id).is_some_and(|treasury| treasury == target) {
        if let Some(call) = decode_exact_call::<ProposalTreasury::transferERC20Call>(calldata) {
            return Some(DecodedProposalAction::TreasuryTransferErc20 {
                token: call.token,
                to: call.to,
                amount: call.amount,
            });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::transferETHCall>(calldata) {
            return Some(DecodedProposalAction::TreasuryTransferEth {
                to: call.to,
                amount: call.amount,
            });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::initializeTreasuryCall>(calldata)
        {
            return Some(DecodedProposalAction::TreasuryInitialize { owner: call.owner });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::grantRoleCall>(calldata) {
            return Some(DecodedProposalAction::TreasuryGrantRole {
                role: call.role,
                account: call.account,
            });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::revokeRoleCall>(calldata) {
            return Some(DecodedProposalAction::TreasuryRevokeRole {
                role: call.role,
                account: call.account,
            });
        }
        if let Some(call) = decode_exact_call::<ProposalTreasury::renounceRoleCall>(calldata) {
            return Some(DecodedProposalAction::TreasuryRenounceRole {
                role: call.role,
                account: call.account,
            });
        }
    }

    if let Some(call) = decode_exact_call::<ProposalOpStackSender::readyTaskCall>(calldata) {
        return Some(DecodedProposalAction::OpStackReadyTask {
            task_id: call.taskId,
        });
    }
    if let Some(call) = decode_exact_call::<ProposalOpStackSender::setExecutorL2Call>(calldata) {
        return Some(DecodedProposalAction::OpStackSetExecutorL2 {
            executor: call.executor,
        });
    }

    if let Some(call) = decode_exact_call::<ProposalOwnable::transferOwnershipCall>(calldata) {
        return Some(DecodedProposalAction::TransferOwnership {
            new_owner: call.newOwner,
        });
    }

    if let Some(call) = decode_exact_call::<ProposalProxyAdmin::upgradeCall>(calldata) {
        return Some(DecodedProposalAction::ProxyUpgrade {
            proxy: call.proxy,
            implementation: call.implementation,
        });
    }
    if let Some(call) = decode_exact_call::<ProposalProxyAdmin::pauseCall>(calldata) {
        return Some(DecodedProposalAction::ProxyPause { proxy: call.proxy });
    }
    if let Some(call) =
        decode_exact_call::<ProposalProxyAdmin::transferProxyOwnershipCall>(calldata)
    {
        return Some(DecodedProposalAction::ProxyTransferOwnership {
            proxy: call.proxy,
            new_owner: call.newOwner,
        });
    }

    if railgun_contract.is_some_and(|railgun| railgun == target)
        && let Some(call) = decode_exact_call::<ProposalRailgun::changeFeeCall>(calldata)
    {
        return Some(DecodedProposalAction::ChangeFee {
            shield_fee: U256::from(call.shieldFee),
            unshield_fee: U256::from(call.unshieldFee),
            nft_fee: call.nftFee,
        });
    }

    if railgun_ui::governance_contracts(chain_id)
        .is_some_and(|contracts| contracts.governance_token == target)
        && let Some(call) =
            decode_exact_call::<ProposalGovernanceToken::governanceMintCall>(calldata)
    {
        return Some(DecodedProposalAction::GovernanceMint {
            account: call.account,
            amount: call.amount,
        });
    }

    if railgun_ui::governance_contracts(chain_id)
        .is_some_and(|contracts| contracts.governor_rewards == target)
        && let Some(call) =
            decode_exact_call::<ProposalGovernorRewards::setIntervalBPCall>(calldata)
    {
        return Some(DecodedProposalAction::SetIntervalBP {
            new_interval_bp: call.newIntervalBP,
        });
    }
    if railgun_ui::governance_contracts(chain_id)
        .is_some_and(|contracts| contracts.governor_rewards == target)
        && let Some(call) = decode_exact_call::<ProposalGovernorRewards::addTokensCall>(calldata)
    {
        return Some(DecodedProposalAction::AddTokens {
            tokens: call.tokens,
        });
    }

    if railgun_ui::governance_contracts(chain_id)
        .is_some_and(|contracts| contracts.delegator == target)
        && let Some(call) = decode_exact_call::<ProposalDelegator::setPermissionCall>(calldata)
    {
        return Some(DecodedProposalAction::DelegatorSetPermission {
            caller: call.caller,
            contract_address: call.contractAddress,
            selector: call.selector,
            permission: call.permission,
        });
    }

    if wrapped_native_token.is_some_and(|wrapped| wrapped == target) {
        if decode_exact_call::<ProposalWrappedNative::depositCall>(calldata).is_some() {
            return Some(DecodedProposalAction::WrappedDeposit);
        }
        if let Some(call) = decode_exact_call::<ProposalWrappedNative::withdrawCall>(calldata) {
            return Some(DecodedProposalAction::WrappedWithdraw {
                amount: call.amount,
            });
        }
    }

    if let Some(call) = decode_exact_call::<ProposalErc20::transferCall>(calldata) {
        return Some(DecodedProposalAction::Erc20Transfer {
            recipient: call.recipient,
            amount: call.amount,
        });
    }
    if let Some(call) = decode_exact_call::<ProposalErc20::transferFromCall>(calldata) {
        return Some(DecodedProposalAction::Erc20TransferFrom {
            from: call.from,
            to: call.to,
            amount: call.amount,
        });
    }
    decode_exact_call::<ProposalErc20::approveCall>(calldata).map(|call| {
        DecodedProposalAction::Erc20Approve {
            spender: call.spender,
            amount: call.amount,
        }
    })
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

impl ProposalDocumentState {
    const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedProposal {
    pub proposal: GovernanceProposal,
    pub rules: GovernanceContractRules,
    document: ProposalDocumentState,
}

impl ResolvedProposal {
    const fn identity(&self) -> ProposalIdentity {
        ProposalIdentity {
            contract_version: self.proposal.contract_version,
            contract_address: self.proposal.contract_address,
            index: self.proposal.index,
        }
    }

    const fn document(&self) -> Option<&GovernanceDocument> {
        match &self.document {
            ProposalDocumentState::Pending => None,
            ProposalDocumentState::Resolved { document, .. } => Some(document),
        }
    }

    const fn presentation(&self) -> Option<&ProposalPresentation> {
        match &self.document {
            ProposalDocumentState::Pending => None,
            ProposalDocumentState::Resolved { presentation, .. } => Some(presentation),
        }
    }

    const fn has_pending_document(&self) -> bool {
        self.document.is_pending()
    }

    fn status(&self, chain_time: U256) -> GovernanceProposalStatus {
        derive_governance_proposal_status(&self.proposal, &self.rules, chain_time)
            .expect("validated governance rules must derive a status")
    }
}

#[derive(Clone, Debug)]
struct ChainTimeAnchor {
    chain_time: U256,
    captured_at: Instant,
}

impl ChainTimeAnchor {
    fn current(&self) -> U256 {
        self.current_at(self.captured_at.elapsed().as_secs())
    }

    fn current_at(&self, elapsed_secs: u64) -> U256 {
        advance_chain_time(self.chain_time, elapsed_secs)
    }
}

fn advance_chain_time(chain_time: U256, elapsed_secs: u64) -> U256 {
    chain_time
        .checked_add(U256::from(elapsed_secs))
        .unwrap_or(U256::MAX)
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

impl ProposalListDelegate {
    const fn new(root: WeakEntity<WalletRoot>) -> Self {
        Self {
            root,
            selected: None,
        }
    }
}

impl ListDelegate for ProposalListDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, cx: &gpui::App) -> usize {
        self.root.upgrade().map_or(0, |root| {
            root.read_with(cx, |root, _| {
                root.proposals
                    .rows(root.proposals.current_page)
                    .map_or(0, <[_]>::len)
            })
        })
    }

    fn render_item(
        &mut self,
        ix: gpui_component::IndexPath,
        _window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        self.root.upgrade().and_then(|root| {
            root.read_with(cx, |root, _| {
                let page = root.proposals.current_page;
                let chain_time = root.proposals.chain_time();
                root.proposals
                    .rows(page)
                    .and_then(|rows| rows.get(ix.row))
                    .map(|proposal| {
                        WalletRoot::render_proposal_row(
                            proposal,
                            chain_time,
                            self.selected == Some(ix),
                        )
                    })
            })
        })
    }

    fn set_selected_index(
        &mut self,
        ix: Option<gpui_component::IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected = ix;
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) {
        let Some(ix) = self.selected else {
            return;
        };
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let Some((page, identity)) = root.read_with(cx, |root, _| {
            let page = root.proposals.current_page;
            let identity = root
                .proposals
                .rows(page)
                .and_then(|rows| rows.get(ix.row))
                .map(ResolvedProposal::identity);
            identity.map(|identity| (page, identity))
        }) else {
            return;
        };
        root.update(cx, |root, cx| {
            root.select_proposal(page, &identity, window, cx);
        });
    }
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

impl ProposalTaskTracker {
    fn track(&mut self, task: tokio::task::JoinHandle<()>) {
        self.tasks.retain(|task| !task.is_finished());
        self.tasks.push(task);
    }

    fn abort_all(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }

    fn abort_and_clear(&mut self) {
        self.abort_all();
        self.tasks.clear();
    }

    fn take(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        std::mem::take(&mut self.tasks)
    }
}

impl Drop for ProposalTaskTracker {
    fn drop(&mut self) {
        self.abort_all();
    }
}

pub(super) struct ProposalCleanup {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ProposalCleanup {
    #[cfg(test)]
    pub(super) const fn empty() -> Self {
        Self { tasks: Vec::new() }
    }

    pub(super) async fn shutdown(self) -> Result<(), String> {
        for task in &self.tasks {
            task.abort();
        }
        let mut failures = 0;
        for task in self.tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                failures += 1;
                tracing::warn!(%error, "proposal worker failed during cleanup");
            }
        }
        if failures == 0 {
            Ok(())
        } else {
            Err(format!(
                "{failures} proposal worker failures during cleanup"
            ))
        }
    }
}

impl ProposalsState {
    pub(super) fn new(chain_id: u64) -> Self {
        Self {
            chain_id,
            generation: 0,
            request_generation: 0,
            checked: false,
            overview: None,
            chain_time_anchor: None,
            total_pages: 0,
            current_page: 0,
            pages: BTreeMap::new(),
            loading_pages: BTreeSet::new(),
            hydrating_pages: BTreeSet::new(),
            prefetch_page: None,
            active_page_token: 0,
            prefetch_token: 0,
            proposal_lists: BTreeMap::new(),
            focus_list_on_render: false,
            table_scroll_handles: BTreeMap::new(),
            expanded_calldata: BTreeSet::new(),
            detail_scroll_handle: ScrollHandle::new(),
            loading: false,
            refreshing: false,
            error: None,
            selected: None,
            time_tick_active: false,
            task_tracker: ProposalTaskTracker { tasks: Vec::new() },
            active_page_task_tracker: ProposalTaskTracker { tasks: Vec::new() },
            prefetch_task_tracker: ProposalTaskTracker { tasks: Vec::new() },
            document_semaphore: Arc::new(Semaphore::new(DOCUMENT_RESOLUTION_CONCURRENCY)),
        }
    }
    fn invalidate(&mut self, chain_id: u64) {
        self.task_tracker.abort_and_clear();
        self.cancel_active_page(self.current_page);
        self.cancel_prefetch();
        self.chain_id = chain_id;
        self.generation = self.generation.wrapping_add(1);
        self.request_generation = self.request_generation.wrapping_add(1);
        self.checked = false;
        self.overview = None;
        self.chain_time_anchor = None;
        self.total_pages = 0;
        self.current_page = 0;
        self.pages.clear();
        self.loading_pages.clear();
        self.hydrating_pages.clear();
        self.proposal_lists.clear();
        self.focus_list_on_render = false;
        self.table_scroll_handles.clear();
        self.expanded_calldata.clear();
        self.detail_scroll_handle = ScrollHandle::new();
        self.loading = false;
        self.refreshing = false;
        self.error = None;
        self.selected = None;
        self.time_tick_active = false;
    }
    pub(super) fn take_cleanup(&mut self) -> ProposalCleanup {
        self.generation = self.generation.wrapping_add(1);
        self.request_generation = self.request_generation.wrapping_add(1);
        self.active_page_token = self.active_page_token.wrapping_add(1);
        self.prefetch_token = self.prefetch_token.wrapping_add(1);
        self.loading_pages.clear();
        self.hydrating_pages.clear();
        self.prefetch_page = None;
        self.table_scroll_handles.clear();
        self.expanded_calldata.clear();
        self.detail_scroll_handle = ScrollHandle::new();
        self.loading = false;
        self.refreshing = false;
        self.time_tick_active = false;
        let mut tasks = self.task_tracker.take();
        tasks.extend(self.active_page_task_tracker.take());
        tasks.extend(self.prefetch_task_tracker.take());
        ProposalCleanup { tasks }
    }
    fn cancel_prefetch(&mut self) {
        self.prefetch_task_tracker.abort_and_clear();
        self.prefetch_token = self.prefetch_token.wrapping_add(1);
        if let Some(page) = self.prefetch_page.take() {
            self.loading_pages.remove(&page);
            self.hydrating_pages.remove(&page);
        }
    }
    fn cancel_active_page(&mut self, page: usize) {
        self.active_page_task_tracker.abort_and_clear();
        self.active_page_token = self.active_page_token.wrapping_add(1);
        self.loading_pages.remove(&page);
        self.hydrating_pages.remove(&page);
    }
    fn owns_page_work(&self, page: usize, prefetch: bool, token: u64) -> bool {
        if prefetch {
            self.prefetch_page == Some(page) && self.prefetch_token == token
        } else {
            self.current_page == page && self.active_page_token == token
        }
    }
    fn rows(&self, page: usize) -> Option<&[ResolvedProposal]> {
        self.pages.get(&page).map(|p| p.rows.as_slice())
    }

    pub(super) fn selected_proposal(&self) -> Option<&GovernanceProposal> {
        let selection = self.selected.as_ref()?;
        self.rows(selection.page)
            .and_then(|rows| rows.iter().find(|row| row.identity() == selection.identity))
            .map(|row| &row.proposal)
    }

    fn chain_time(&self) -> U256 {
        self.chain_time_anchor
            .as_ref()
            .map_or(U256::ZERO, ChainTimeAnchor::current)
    }

    fn pending_document_rows(&self, page: usize) -> Option<Vec<ResolvedProposal>> {
        self.pages.get(&page).map(|page| {
            page.rows
                .iter()
                .filter(|row| row.has_pending_document())
                .cloned()
                .collect()
        })
    }

    fn page_is_terminal(&self, page: usize) -> bool {
        self.pages
            .get(&page)
            .is_some_and(|page| page.rows.iter().all(|row| !row.has_pending_document()))
    }

    fn apply_document(&mut self, page: usize, completion: DocumentCompletion) -> bool {
        let Some(page_rows) = self.pages.get_mut(&page) else {
            return false;
        };
        let Some(row) = Arc::make_mut(&mut page_rows.rows)
            .iter_mut()
            .find(|row| row.identity() == completion.identity)
        else {
            return false;
        };
        let table_count = match &completion.presentation {
            ProposalPresentation::Prepared(prepared) => prepared.table_count,
            ProposalPresentation::RawParseFallback(_) | ProposalPresentation::TooComplex => 0,
        };
        row.document = ProposalDocumentState::Resolved {
            document: completion.document,
            presentation: completion.presentation,
        };
        self.ensure_table_scroll_handles(&completion.identity, table_count);
        self.selected.as_ref().is_some_and(|selected| {
            selected.page == page && selected.identity == completion.identity
        }) || self.current_page == page
    }

    fn ensure_table_scroll_handles(&mut self, identity: &ProposalIdentity, table_count: usize) {
        for ordinal in 0..table_count {
            self.table_scroll_handles
                .entry(proposal_table_scroll_key(identity, ordinal))
                .or_default();
        }
    }

    fn prepare_manual_refresh(&mut self) {
        let page = self.current_page;
        let Some(page_rows) = self.pages.get_mut(&page) else {
            return;
        };
        for row in Arc::make_mut(&mut page_rows.rows) {
            if let ProposalDocumentState::Resolved { document, .. } = &row.document
                && !document.available
            {
                row.document = ProposalDocumentState::Pending;
            }
        }
    }

    fn replace_refreshed_page(
        &mut self,
        source_page: usize,
        destination_page: usize,
        mut rows: Vec<ResolvedProposal>,
    ) {
        let source_rows = self.pages.get(&source_page).map(|page| page.rows.clone());
        if let Some(source_rows) = source_rows {
            for row in &mut rows {
                let Some(source_row) = source_rows
                    .iter()
                    .find(|source_row| source_row.identity() == row.identity())
                else {
                    continue;
                };
                if let ProposalDocumentState::Resolved {
                    document,
                    presentation,
                } = &source_row.document
                    && document.available
                {
                    row.document = ProposalDocumentState::Resolved {
                        document: document.clone(),
                        presentation: presentation.clone(),
                    };
                }
            }
        }

        let selected = self.selected.take().and_then(|mut selected| {
            if selected.page == source_page
                && rows.iter().any(|row| row.identity() == selected.identity)
            {
                selected.page = destination_page;
                Some(selected)
            } else {
                None
            }
        });
        let source_list = self.proposal_lists.remove(&source_page);
        self.pages.clear();
        self.pages.insert(
            destination_page,
            ProposalsPage {
                rows: Arc::new(rows),
            },
        );
        self.current_page = destination_page;
        self.loading_pages.clear();
        self.hydrating_pages.clear();
        self.proposal_lists.clear();
        if let Some(source_list) = source_list {
            self.proposal_lists.insert(destination_page, source_list);
        }
        let prepared_tables = self.pages[&destination_page]
            .rows
            .iter()
            .filter_map(|row| {
                row.presentation().and_then(|presentation| {
                    let ProposalPresentation::Prepared(prepared) = presentation else {
                        return None;
                    };
                    Some((row.identity(), prepared.table_count))
                })
            })
            .collect::<Vec<_>>();
        for (identity, table_count) in prepared_tables {
            self.ensure_table_scroll_handles(&identity, table_count);
        }
        self.selected = selected;
        let valid_actions = self
            .pages
            .get(&destination_page)
            .map(|page| {
                page.rows
                    .iter()
                    .flat_map(|row| {
                        (0..row.proposal.actions.len()).map(move |ordinal| ProposalActionIdentity {
                            proposal: row.identity(),
                            ordinal,
                        })
                    })
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        self.expanded_calldata
            .retain(|key| valid_actions.contains(key));
    }

    fn prefetch_candidate(&self, current: usize) -> Option<usize> {
        if current != self.current_page || !self.page_is_terminal(current) {
            return None;
        }
        next_proposal_page(current, self.total_pages)
    }
}

struct InitialLoad {
    overview: Option<GovernanceOverview>,
    chain_time_anchor: Option<ChainTimeAnchor>,
    total_pages: usize,
    page: usize,
    rows: Vec<ResolvedProposal>,
}

impl WalletRoot {
    pub(super) fn render_proposal_action_dialog_content(
        &self,
        root: Entity<Self>,
        content_width: gpui::Pixels,
        cx: &App,
    ) -> gpui::Div {
        let proposal = self.proposals.selected.as_ref().and_then(|selection| {
            self.proposals
                .rows(selection.page)
                .and_then(|rows| rows.iter().find(|row| row.identity() == selection.identity))
        });
        let Some(proposal) = proposal else {
            let close_root = root;
            return div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .child(Alert::error(
                    "governance-action-stale-selection",
                    "The selected proposal is no longer available. Refresh before reviewing this action.",
                ))
                .child(
                    div().flex().justify_end().child(
                        app_button_base("governance-action-stale-cancel")
                            .ghost()
                            .small()
                            .child("Cancel")
                            .on_click(move |_event, window, cx| {
                                close_root.update(cx, Self::close_proposal_action);
                                window.close_dialog(cx);
                            }),
                    ),
                );
        };
        let token = railgun_ui::governance_contracts(self.selected_chain)
            .map_or(Address::ZERO, |contracts| contracts.governance_token);
        let decimals = self
            .effective_token_registry
            .get(self.selected_chain, &token)
            .map(|info| info.decimals);
        render_proposal_action_form(
            &root,
            self,
            proposal,
            decimals,
            content_width,
            self.proposals.chain_time(),
            cx,
        )
            .unwrap_or_else(|| {
                let close_root = root;
                div()
                    .w(content_width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(Alert::error(
                        "governance-action-stale-context",
                        "This proposal action is no longer valid for the current account or chain context. Refresh before trying again.",
                    ))
                    .child(
                        div().flex().justify_end().child(
                            app_button_base("governance-action-stale-context-cancel")
                                .ghost()
                                .small()
                                .child("Cancel")
                                .on_click(move |_event, window, cx| {
                                    close_root.update(cx, Self::close_proposal_action);
                                    window.close_dialog(cx);
                                }),
                        ),
                    )
            })
    }

    pub(super) fn open_proposals(&mut self, cx: &mut Context<'_, Self>) {
        self.active_activity = super::sidebar::Activity::Proposals;
        self.clean_governance_participants();
        self.proposals.focus_list_on_render = true;
        if self.proposals.chain_time_anchor.is_some() {
            self.start_proposals_time_tick(cx);
        }
        if self.governance.tab == super::governance::GovernanceTab::Proposals
            && !self.proposals.checked
            && !self.proposals.loading
        {
            self.start_proposals_refresh(false, cx);
        } else if self.governance.tab == super::governance::GovernanceTab::Staking
            && matches!(
                self.governance.staking.status,
                super::governance::StakingRefreshStatus::Idle
            )
        {
            self.start_staking_refresh(cx);
        } else if self.governance.tab == super::governance::GovernanceTab::Proposals {
            let selected = self.proposals.selected.as_ref().and_then(|selection| {
                self.proposals.rows(selection.page).and_then(|rows| {
                    rows.iter()
                        .find(|row| row.identity() == selection.identity)
                        .map(|row| row.proposal.clone())
                })
            });
            if let Some(proposal) = selected.as_ref() {
                self.start_proposal_participation(proposal, cx);
            }
        }
        cx.notify();
    }

    fn start_proposals_time_tick(&mut self, cx: &mut Context<'_, Self>) {
        if self.proposals.time_tick_active {
            return;
        }
        self.proposals.time_tick_active = true;
        let generation = self.proposals.generation;
        let (tick_tx, tick_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_mins(1)).await;
            let _ = tick_tx.send(());
        });
        self.proposals.task_tracker.track(join);
        cx.spawn(async move |this, cx| {
            let Ok(()) = tick_rx.await else {
                return;
            };
            let _ = this.update(cx, |root, cx| {
                root.proposals.time_tick_active = false;
                if root.proposals.generation == generation
                    && root.active_activity == super::sidebar::Activity::Proposals
                {
                    cx.notify();
                    root.start_proposals_time_tick(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn invalidate_proposals_chain(&mut self, chain_id: u64) {
        self.proposals.invalidate(chain_id);
        if self.active_activity == super::sidebar::Activity::Proposals {
            self.proposals.focus_list_on_render = true;
        }
    }
    pub(super) fn start_proposals_refresh(&mut self, manual: bool, cx: &mut Context<'_, Self>) {
        if manual {
            self.proposals.prepare_manual_refresh();
        }
        self.proposals.task_tracker.abort_and_clear();
        self.proposals.time_tick_active = false;
        self.proposals
            .cancel_active_page(self.proposals.current_page);
        self.proposals.cancel_prefetch();
        self.proposals.hydrating_pages.clear();
        self.proposals.loading = false;
        let chain_id = self.selected_chain;
        let requested_page = if manual {
            self.proposals.current_page
        } else {
            0
        };
        self.proposals.chain_id = chain_id;
        self.proposals.loading = true;
        self.proposals.refreshing = manual;
        self.proposals.error = None;
        self.proposals.loading_pages.clear();
        self.proposals.request_generation = self.proposals.request_generation.wrapping_add(1);
        let request_generation = self.proposals.request_generation;
        let chain_generation = self.proposals.generation;
        let http = self.http.clone();
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let (result_tx, result_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            let result =
                load_initial(chain_id, requested_page, effective_chain.as_ref(), &http).await;
            let _ = result_tx.send(result);
        });
        self.proposals.task_tracker.track(join);
        cx.spawn(async move |this, cx| {
            let Ok(result) = result_rx.await else {
                return;
            };
            let _ = this.update(cx, |root, cx| {
                if root.selected_chain != chain_id
                    || root.proposals.generation != chain_generation
                    || root.proposals.request_generation != request_generation
                {
                    return;
                }
                root.proposals.loading = false;
                root.proposals.refreshing = false;
                match result {
                    Ok(load) => {
                        root.proposals.checked = true;
                        root.proposals.error = None;
                        root.proposals.overview = load.overview;
                        root.proposals.chain_time_anchor = load.chain_time_anchor;
                        root.proposals.total_pages = load.total_pages;
                        root.proposals
                            .replace_refreshed_page(requested_page, load.page, load.rows);
                        let selected_proposal =
                            root.proposals.selected.as_ref().and_then(|selection| {
                                root.proposals.rows(selection.page).and_then(|rows| {
                                    rows.iter()
                                        .find(|row| row.identity() == selection.identity)
                                        .map(|row| row.proposal.clone())
                                })
                            });
                        if let Some(proposal) = selected_proposal.as_ref() {
                            root.start_proposal_participation(proposal, cx);
                        }
                        if root.proposals.overview.is_some() {
                            root.start_proposals_time_tick(cx);
                            root.resume_active_proposals_page(load.page, cx);
                        }
                    }
                    Err(error) => {
                        root.proposals.error = Some(Arc::from(format_report_chain(&error)));
                        if root.proposals.chain_time_anchor.is_some()
                            && root.active_activity == super::sidebar::Activity::Proposals
                        {
                            root.start_proposals_time_tick(cx);
                        }
                        let page = root.proposals.current_page;
                        root.resume_active_proposals_page(page, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(super) fn select_proposals_page(&mut self, page: usize, cx: &mut Context<'_, Self>) {
        if page >= self.proposals.total_pages {
            return;
        }
        if page != self.proposals.current_page {
            self.proposals
                .cancel_active_page(self.proposals.current_page);
            self.proposals.cancel_prefetch();
        }
        self.proposals.current_page = page;
        self.proposals.selected = None;
        self.close_proposal_action(cx);
        self.proposals.expanded_calldata.clear();
        self.proposals.focus_list_on_render = true;
        if self.proposals.rows(page).is_none() {
            self.request_proposals_page(page, false, cx);
        } else {
            self.resume_active_proposals_page(page, cx);
        }
        cx.notify();
    }
    pub(super) fn select_proposal(
        &mut self,
        page: usize,
        identity: &ProposalIdentity,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let table_count =
            self.proposals
                .rows(page)
                .and_then(|rows| rows.iter().find(|row| row.identity().eq(identity)))
                .and_then(ResolvedProposal::presentation)
                .and_then(|presentation| match presentation {
                    ProposalPresentation::Prepared(prepared) => Some(prepared.table_count),
                    ProposalPresentation::RawParseFallback(_)
                    | ProposalPresentation::TooComplex => None,
                });
        if let Some(table_count) = table_count {
            self.proposals
                .ensure_table_scroll_handles(identity, table_count);
        }
        self.proposals.expanded_calldata.clear();
        self.proposals.selected = Some(ProposalSelection {
            page,
            identity: identity.clone(),
            tab: ProposalDetailTab::default(),
            participation_expanded: false,
        });
        self.proposals.detail_scroll_handle = ScrollHandle::new();
        let selected_proposal = self.proposals.rows(page).and_then(|rows| {
            rows.iter()
                .find(|row| row.identity().eq(identity))
                .map(|row| row.proposal.clone())
        });
        if let Some(proposal) = selected_proposal.as_ref() {
            self.start_proposal_participation(proposal, cx);
        }
        self.proposal_detail_focus.focus(window);
        cx.notify();
    }
    pub(super) fn clear_selected_proposal(&mut self, cx: &mut Context<'_, Self>) {
        self.proposals.selected = None;
        self.close_proposal_action(cx);
        self.proposals.expanded_calldata.clear();
        self.proposals.focus_list_on_render = true;
        cx.notify();
    }
    fn select_proposal_detail_tab(&mut self, tab: ProposalDetailTab, cx: &mut Context<'_, Self>) {
        if let Some(selection) = self.proposals.selected.as_mut() {
            selection.tab = tab;
            cx.notify();
        }
    }
    fn toggle_proposal_participation(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(selection) = self.proposals.selected.as_mut() {
            selection.participation_expanded = !selection.participation_expanded;
            cx.notify();
        }
    }
    fn toggle_proposal_calldata(
        &mut self,
        identity: ProposalIdentity,
        ordinal: usize,
        cx: &mut Context<'_, Self>,
    ) {
        let key = ProposalActionIdentity {
            proposal: identity,
            ordinal,
        };
        if !self.proposals.expanded_calldata.remove(&key) {
            self.proposals.expanded_calldata.insert(key);
        }
        cx.notify();
    }
    pub(super) fn retry_proposals(&mut self, cx: &mut Context<'_, Self>) {
        self.proposals.error = None;
        let page = self.proposals.current_page;
        if self.proposals.overview.is_some()
            && page < self.proposals.total_pages
            && self.proposals.rows(page).is_none()
        {
            self.request_proposals_page(page, false, cx);
        } else {
            self.start_proposals_refresh(true, cx);
        }
        cx.notify();
    }
    fn schedule_proposals_prefetch(&mut self, current: usize, cx: &Context<'_, Self>) {
        let Some(page) = self.proposals.prefetch_candidate(current) else {
            return;
        };
        if self.proposals.prefetch_page == Some(page) {
            return;
        }
        self.proposals.cancel_prefetch();
        self.proposals.prefetch_page = Some(page);
        if self.proposals.rows(page).is_some() {
            self.start_proposals_hydration(page, true, cx);
        } else {
            self.request_proposals_page(page, true, cx);
        }
    }
    fn request_proposals_page(&mut self, page: usize, prefetch: bool, cx: &Context<'_, Self>) {
        let Some(overview) = self.proposals.overview.clone() else {
            return;
        };
        if self.proposals.pages.contains_key(&page) || !self.proposals.loading_pages.insert(page) {
            return;
        }
        let chain_id = self.selected_chain;
        let chain_generation = self.proposals.generation;
        let request_generation = self.proposals.request_generation;
        let work_token = if prefetch {
            self.proposals.prefetch_token
        } else {
            self.proposals.active_page_token
        };
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let http = self.http.clone();
        tracing::debug!(chain_id, page, prefetch, "loading governance proposal page");
        let (result_tx, result_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            let result = load_page(&overview, page, effective_chain.as_ref(), &http).await;
            let _ = result_tx.send(result);
        });
        if prefetch {
            self.proposals.prefetch_task_tracker.track(join);
        } else {
            self.proposals.active_page_task_tracker.track(join);
        }
        cx.spawn(async move |this, cx| {
            let Ok(result) = result_rx.await else {
                return;
            };
            let _ = this.update(cx, |root, cx| {
                if root.selected_chain != chain_id
                    || root.proposals.generation != chain_generation
                    || root.proposals.request_generation != request_generation
                {
                    return;
                }
                if !root.proposals.owns_page_work(page, prefetch, work_token) {
                    return;
                }
                root.proposals.loading_pages.remove(&page);
                match result {
                    Ok(rows) => {
                        root.proposals.pages.insert(
                            page,
                            ProposalsPage {
                                rows: Arc::new(rows),
                            },
                        );
                        if prefetch {
                            root.start_proposals_hydration(page, true, cx);
                        } else {
                            root.resume_active_proposals_page(page, cx);
                        }
                        if !prefetch && page == root.proposals.current_page {
                            root.proposals.error = None;
                            let selected_proposal =
                                root.proposals.selected.as_ref().and_then(|selection| {
                                    root.proposals.rows(selection.page).and_then(|rows| {
                                        rows.iter()
                                            .find(|row| row.identity() == selection.identity)
                                            .map(|row| row.proposal.clone())
                                    })
                                });
                            if let Some(proposal) = selected_proposal.as_ref() {
                                root.start_proposal_participation(proposal, cx);
                            }
                        }
                    }
                    Err(error) => {
                        if !prefetch {
                            root.proposals.error = Some(Arc::from(format_report_chain(&error)));
                        } else if root.proposals.owns_page_work(page, true, work_token) {
                            root.proposals.prefetch_page = None;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn refresh_selected_proposal_page(&mut self, cx: &mut Context<'_, Self>) {
        let page = self
            .proposals
            .selected
            .as_ref()
            .map_or(self.proposals.current_page, |selection| selection.page);
        self.proposals.pages.remove(&page);
        self.proposals.proposal_lists.remove(&page);
        self.proposals.loading_pages.remove(&page);
        self.request_proposals_page(page, false, cx);
        cx.notify();
    }
    fn resume_active_proposals_page(&mut self, page: usize, cx: &Context<'_, Self>) {
        self.start_proposals_hydration(page, false, cx);
        self.schedule_proposals_prefetch(page, cx);
    }
    fn start_proposals_hydration(&mut self, page: usize, prefetch: bool, cx: &Context<'_, Self>) {
        let Some(rows) = self.proposals.pending_document_rows(page) else {
            return;
        };
        if rows.is_empty() {
            return;
        }
        if !self.proposals.hydrating_pages.insert(page) {
            return;
        }
        let chain_id = self.selected_chain;
        let chain_generation = self.proposals.generation;
        let request_generation = self.proposals.request_generation;
        let work_token = if prefetch {
            self.proposals.prefetch_token
        } else {
            self.proposals.active_page_token
        };
        let http = self.http.clone();
        let vault_store = self.vault_store.clone();
        let semaphore = Arc::clone(&self.proposals.document_semaphore);
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let join = self.runtime.spawn(async move {
            hydrate_page(rows, vault_store.as_ref(), &http, semaphore, completion_tx).await;
        });
        if prefetch {
            self.proposals.prefetch_task_tracker.track(join);
        } else {
            self.proposals.active_page_task_tracker.track(join);
        }
        cx.spawn(async move |this, cx| {
            while let Some(first) = completion_rx.recv().await {
                let mut completions = vec![first];
                while let Ok(completion) = completion_rx.try_recv() {
                    completions.push(completion);
                }
                let _ = this.update(cx, |root, cx| {
                    if root.selected_chain != chain_id
                        || root.proposals.generation != chain_generation
                        || root.proposals.request_generation != request_generation
                    {
                        return;
                    }
                    if !root.proposals.owns_page_work(page, prefetch, work_token) {
                        return;
                    }
                    let mut visible = false;
                    for completion in completions {
                        visible |= root.proposals.apply_document(page, completion);
                    }
                    if visible {
                        cx.notify();
                    }
                });
            }
            let _ = this.update(cx, |root, cx| {
                if root.selected_chain != chain_id
                    || root.proposals.generation != chain_generation
                    || root.proposals.request_generation != request_generation
                {
                    return;
                }
                if !root.proposals.owns_page_work(page, prefetch, work_token) {
                    return;
                }
                root.proposals.hydrating_pages.remove(&page);
                if !prefetch && page == root.proposals.current_page {
                    root.schedule_proposals_prefetch(page, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn ensure_proposal_list(
        &mut self,
        page: usize,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<ListState<ProposalListDelegate>> {
        if let Some(list) = self.proposals.proposal_lists.get(&page) {
            return list.clone();
        }
        let list =
            cx.new(|cx| ListState::new(ProposalListDelegate::new(root.downgrade()), window, cx));
        let observed_root = root.clone();
        list.update(cx, |_list, cx| {
            cx.observe(&observed_root, |_list, _root, cx| cx.notify())
                .detach();
        });
        self.proposals.proposal_lists.insert(page, list.clone());
        list
    }

    pub(super) fn render_proposals_view(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let active_detail = self.proposals.selected.as_ref().and_then(|selection| {
            self.proposals
                .rows(selection.page)
                .and_then(|rows| rows.iter().find(|row| row.identity() == selection.identity))
        });
        let has_active_detail = active_detail.is_some();
        let active_content = if let Some(proposal) = active_detail {
            self.render_proposal_detail(root, proposal, window, cx)
                .into_any_element()
        } else {
            self.render_proposal_list(root, window, cx)
                .into_any_element()
        };
        let keyboard_root = root.clone();
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .on_key_down(move |event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "escape" && has_active_detail {
                    keyboard_root.update(cx, |root, cx| {
                        root.clear_selected_proposal(cx);
                    });
                    cx.stop_propagation();
                    return;
                }
                if has_active_detail || event.keystroke.modifiers.modified() {
                    return;
                }
                let is_left = match event.keystroke.key.as_str() {
                    "left" => true,
                    "right" => false,
                    _ => return,
                };
                keyboard_root.update(cx, |root, cx| {
                    let page = if is_left {
                        root.proposals.current_page.checked_sub(1)
                    } else {
                        next_proposal_page(root.proposals.current_page, root.proposals.total_pages)
                    };
                    if let Some(page) = page {
                        root.select_proposals_page(page, cx);
                    }
                });
                cx.stop_propagation();
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .child(active_content),
            )
            .into_any_element()
    }
    fn render_proposal_list(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::AnyElement {
        let page = self.proposals.current_page;
        let row_count = self.proposals.rows(page).map_or(0, <[_]>::len);
        let list = if row_count > 0 {
            let list = self.ensure_proposal_list(page, root, window, cx);
            let focus_list_on_render = self.proposals.focus_list_on_render;
            list.update(cx, |list, cx| {
                if let Some(index) = list.selected_index()
                    && index.row >= row_count
                {
                    list.set_selected_index(
                        Some(gpui_component::IndexPath::default().row(row_count - 1)),
                        window,
                        cx,
                    );
                }
                if focus_list_on_render {
                    if list.selected_index().is_none() {
                        list.set_selected_index(
                            Some(gpui_component::IndexPath::default()),
                            window,
                            cx,
                        );
                    }
                    list.focus(window, cx);
                }
            });
            if focus_list_on_render {
                self.proposals.focus_list_on_render = false;
            }
            Some(list)
        } else {
            None
        };
        let state = &self.proposals;
        let mut content = div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED));
        let mut body = div()
            .w(CONTENT_WIDTH)
            .max_w_full()
            .mx_auto()
            .flex_1()
            .min_h(px(0.0))
            .p(px(16.0))
            .flex()
            .flex_col();
        let loading_without_rows =
            (state.loading || state.loading_pages.contains(&page)) && state.rows(page).is_none();
        let mut list_content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .gap_3()
            .child(governance_permissionless_warning());
        let total = state
            .overview
            .as_ref()
            .and_then(|overview| total_proposals(overview).ok());
        let show_toolbar = total.is_some() || state.total_pages > 1;
        if show_toolbar {
            let mut toolbar = div().w_full().flex().items_center().justify_between();
            if let Some(total) = total {
                toolbar =
                    toolbar.child(app_muted_text(format!("{total} proposals")).text_size(px(12.0)));
            }
            if state.total_pages > 1 {
                let previous_root = root.clone();
                let next_root = root.clone();
                toolbar = toolbar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            app_button_base("wallet-proposals-previous")
                                .ghost()
                                .xsmall()
                                .compact()
                                .disabled(state.current_page == 0)
                                .icon(IconName::ChevronLeft)
                                .on_click(move |_event, _window, cx| {
                                    previous_root.update(cx, |root, cx| {
                                        let page = root.proposals.current_page.saturating_sub(1);
                                        root.select_proposals_page(page, cx);
                                    });
                                }),
                        )
                        .child(
                            app_muted_text(format!(
                                "{} / {}",
                                state.current_page + 1,
                                state.total_pages
                            ))
                            .text_size(px(12.0)),
                        )
                        .child(
                            app_button_base("wallet-proposals-next")
                                .ghost()
                                .xsmall()
                                .compact()
                                .disabled(state.current_page + 1 >= state.total_pages)
                                .icon(IconName::ChevronRight)
                                .on_click(move |_event, _window, cx| {
                                    next_root.update(cx, |root, cx| {
                                        let page = root.proposals.current_page + 1;
                                        root.select_proposals_page(page, cx);
                                    });
                                }),
                        ),
                );
            }
            list_content = list_content.child(toolbar);
        } else if loading_without_rows {
            list_content = list_content.child(div().h(px(16.0)).flex_none());
        }
        if loading_without_rows {
            let mut skeleton_rows = div().flex().flex_col().gap_3();
            for _ in 0..PROPOSALS_PAGE_SIZE {
                skeleton_rows = skeleton_rows.child(proposal_skeleton_row());
            }
            list_content = list_content.child(skeleton_rows);
        } else if state.overview.is_none() && state.checked && state.error.is_none() {
            list_content = list_content.child(proposals_empty_state(
                "Governance is not deployed on this chain.",
            ));
        } else if state.rows(page).is_some_and(<[_]>::is_empty) && state.error.is_none() {
            list_content =
                list_content.child(proposals_empty_state("No governance proposals found."));
        } else if let Some(list) = list {
            list_content = list_content.child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(List::new(&list).size_full()),
            );
        } else if state.error.is_some() {
            list_content =
                list_content.child(proposals_empty_state("Proposal data could not be loaded."));
        }
        if let Some(error) = state.error.as_ref() {
            let retry_root = root.clone();
            list_content = list_content.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Alert::error("wallet-proposals-error", error.to_string()).small())
                    .child(
                        app_button("wallet-proposals-retry", "Retry")
                            .small()
                            .on_click(move |_event, _window, cx| {
                                retry_root.update(cx, Self::retry_proposals);
                            }),
                    ),
            );
        }
        body = body.child(list_content);
        let body = if row_count > 0 {
            body.into_any_element()
        } else {
            body.overflow_y_scrollbar().into_any_element()
        };
        content = content.child(body);
        content.into_any_element()
    }
    fn render_proposal_row(
        proposal: &ResolvedProposal,
        chain_time: U256,
        selected: bool,
    ) -> ListItem {
        let status = proposal.status(chain_time);
        let title = proposal.document().map(|document| {
            if document.available {
                ProposalRowTitle::Available(if document.title.is_empty() {
                    format!("Proposal #{}", proposal.proposal.index)
                } else {
                    document.title.clone()
                })
            } else {
                ProposalRowTitle::Unavailable
            }
        });
        let identity = proposal.identity();
        let group_name = proposal_row_group(&identity);
        let row_id = SharedString::from(format!(
            "wallet-proposal-row-{}-{}-{}",
            proposal_version_label(proposal.proposal.contract_version),
            proposal.proposal.contract_address,
            proposal.proposal.index,
        ));
        let row = div()
            .group(group_name)
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .px(px(14.0))
            .py(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_muted_text(format!("#{}", proposal.proposal.index))
                            .text_color(rgb(theme::TEXT_SUBTLE))
                            .text_size(px(15.0))
                            .line_height(px(18.0)),
                    )
                    .child(proposal_row_title(title))
                    .child(div().ml_auto().child(app_status_tag(
                        proposal_stage_label(status.stage),
                        proposal_stage_color(status.stage),
                    ))),
            )
            .child(proposal_row_meta(proposal, chain_time));
        let row = if selected {
            row.bg(rgb(theme::SURFACE))
        } else {
            row
        };
        ListItem::new(row_id)
            .p_0()
            .mb_3()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(theme::SURFACE))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(row)
    }
    fn render_proposal_detail(
        &self,
        root: &Entity<Self>,
        proposal: &ResolvedProposal,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::AnyElement {
        let back_root = root.clone();
        let chain_time = self.proposals.chain_time();
        let status = proposal.status(chain_time);
        let detail_tab = self
            .proposals
            .selected
            .as_ref()
            .map_or_else(ProposalDetailTab::default, |selection| selection.tab);
        let proposer = proposal.proposal.proposer.to_checksum(None);
        let description_id = SharedString::from(format!(
            "wallet-proposal-description-{}-{}",
            proposal_version_label(proposal.proposal.contract_version),
            proposal.proposal.index,
        ));
        let detail_scroll = self.proposals.detail_scroll_handle.clone();
        let mut detail_scroller = div()
            .id("wallet-proposals-detail-scroll")
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .track_scroll(&detail_scroll)
            .overflow_y_scroll()
            .vertical_scrollbar(&detail_scroll)
            .relative();
        detail_scroller.style().restrict_scroll_to_axis = Some(true);
        detail_scroller
            .bg(rgb(theme::SURFACE_ELEVATED))
            .track_focus(&self.proposal_detail_focus)
            .child(
                div()
                    .w(CONTENT_WIDTH)
                    .max_w_full()
                    .mx_auto()
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(governance_permissionless_warning())
                    .child(
                        div().flex().items_center().flex_wrap().gap_2().child(
                            div()
                                .flex()
                                .items_center()
                                .flex_none()
                                .gap_2()
                                .child(
                                    app_button_base("wallet-proposals-back")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .icon(IconName::ArrowLeft)
                                        .tooltip("Back to governance")
                                        .on_click(move |_event, _window, cx| {
                                            back_root.update(cx, |root, cx| {
                                                root.clear_selected_proposal(cx);
                                            });
                                        }),
                                )
                                .child(
                                    app_muted_text(format!(
                                        "Proposal #{}",
                                        proposal.proposal.index
                                    ))
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(12.0)),
                                )
                                .child(app_status_tag(
                                    proposal_stage_label(status.stage),
                                    proposal_stage_color(status.stage),
                                ))
                                .children(
                                    (proposal.proposal.contract_version
                                        == wallet_ops::GovernanceContractVersion::V1)
                                        .then(|| app_status_tag("V1", theme::TEXT_MUTED)),
                                ),
                        ),
                    )
                    .child(proposal_detail_title(proposal))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(app_muted_text("Proposed by").text_size(px(12.0)))
                                    .child(
                                        app_text(spend_authorization_recipient_display(&proposer))
                                            .font_family(APP_MONO_FONT_FAMILY)
                                            .text_size(px(12.0)),
                                    )
                                    .child(
                                        div()
                                            .id(proposal_copy_id(
                                                proposal,
                                                "proposer-meta",
                                                "action",
                                            ))
                                            .tooltip(|window, cx| {
                                                Tooltip::new("Copy Proposer").build(window, cx)
                                            })
                                            .child(clipboard_with_toast(
                                                proposal_copy_id(
                                                    proposal,
                                                    "proposer-meta",
                                                    "clipboard",
                                                ),
                                                proposer.clone(),
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .flex_none()
                                    .child(app_muted_text("·").text_size(px(12.0)))
                                    .child(
                                        app_muted_text(format!(
                                            "Published {}",
                                            format_datetime_short(&proposal.proposal.publish_time)
                                        ))
                                        .text_size(px(12.0)),
                                    ),
                            ),
                    )
                    .child(render_proposal_participation_card(
                        root,
                        self,
                        proposal,
                        status.stage,
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .items_start()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(render_proposal_detail_tabs(
                                        root,
                                        detail_tab,
                                        proposal.proposal.actions.len(),
                                        &proposal.identity(),
                                    ))
                                    .child(match detail_tab {
                                        ProposalDetailTab::Description => proposal_document_card(
                                            proposal,
                                            description_id,
                                            &self.proposals.table_scroll_handles,
                                            window,
                                            cx,
                                        )
                                        .into_any_element(),
                                        ProposalDetailTab::Actions => render_proposal_actions_card(
                                            root,
                                            proposal,
                                            self.proposals.chain_id,
                                            &self.proposals.expanded_calldata,
                                            &self.public_broadcaster_anchor_cache,
                                            &self.effective_token_registry,
                                            self.effective_chain_configs
                                                .get(&self.proposals.chain_id),
                                            &self.public_accounts,
                                            self.view_session
                                                .as_ref()
                                                .map(|_| self.public_address_book.as_slice()),
                                        )
                                        .into_any_element(),
                                    }),
                            )
                            .child(
                                div()
                                    .w(px(300.0))
                                    .flex_none()
                                    .pt(px(44.0))
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .children(match status.stage {
                                        GovernanceProposalStage::AwaitingSponsorship
                                        | GovernanceProposalStage::ReadyToCallVote
                                        | GovernanceProposalStage::SponsorshipExpired
                                        | GovernanceProposalStage::VoteCallExpired => {
                                            vec![render_sponsorship_card(proposal, chain_time)]
                                        }
                                        _ => vec![render_votes_card(proposal, chain_time)],
                                    })
                                    .child(render_timeline_card(proposal, chain_time))
                                    .child(render_details_card(proposal)),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn proposal_copy_control(
    proposal: &ResolvedProposal,
    label: &'static str,
    value: String,
    field: &'static str,
) -> gpui::Div {
    let display_value = spend_authorization_recipient_display(&value);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text(label).text_size(px(11.0)))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .mt(px(1.0))
                .child(
                    app_text(display_value)
                        .flex_1()
                        .min_w(px(0.0))
                        .text_color(rgb(theme::TEXT))
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_size(px(12.0)),
                )
                .child(
                    div()
                        .id(proposal_copy_id(proposal, field, "action"))
                        .flex_none()
                        .tooltip(move |window, cx| {
                            Tooltip::new(format!("Copy {label}")).build(window, cx)
                        })
                        .child(clipboard_with_toast(
                            proposal_copy_id(proposal, field, "clipboard"),
                            value,
                        )),
                ),
        )
}

async fn load_initial(
    chain_id: u64,
    requested_page: usize,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> eyre::Result<InitialLoad> {
    let overview = fetch_governance_overview(chain_id, effective_chain, http).await?;
    let Some(overview) = overview else {
        return Ok(InitialLoad {
            overview: None,
            chain_time_anchor: None,
            total_pages: 0,
            page: 0,
            rows: Vec::new(),
        });
    };
    let chain_time = fetch_governance_chain_time(overview.chain_id, effective_chain, http).await?;
    let chain_time_anchor = ChainTimeAnchor {
        chain_time,
        captured_at: Instant::now(),
    };
    let total = total_proposals(&overview)?;
    let total_pages = total.div_ceil(PROPOSALS_PAGE_SIZE);
    let page = if total_pages == 0 {
        0
    } else {
        requested_page.min(total_pages - 1)
    };
    let rows = if total == 0 {
        Vec::new()
    } else {
        load_page(&overview, page, effective_chain, http).await?
    };
    Ok(InitialLoad {
        overview: Some(overview),
        chain_time_anchor: Some(chain_time_anchor),
        total_pages,
        page,
        rows,
    })
}
async fn load_page(
    overview: &GovernanceOverview,
    page: usize,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> eyre::Result<Vec<ResolvedProposal>> {
    let size = std::num::NonZeroUsize::new(PROPOSALS_PAGE_SIZE).expect("non-zero page size");
    let proposals = fetch_governance_page(overview, page, size, effective_chain, http).await?;
    let mut rows = Vec::with_capacity(proposals.len());
    for proposal in proposals {
        let rules = rules_for(overview, proposal.contract_version)?;
        rows.push(ResolvedProposal {
            proposal,
            rules,
            document: ProposalDocumentState::Pending,
        });
    }
    Ok(rows)
}

struct DocumentCompletion {
    identity: ProposalIdentity,
    document: GovernanceDocument,
    presentation: ProposalPresentation,
}

async fn hydrate_page(
    rows: Vec<ResolvedProposal>,
    store: Option<&Arc<DesktopVaultStore>>,
    http: &HttpContext,
    semaphore: Arc<Semaphore>,
    completion_tx: mpsc::UnboundedSender<DocumentCompletion>,
) {
    let Some(db) = store.map(|store| store.db()) else {
        for row in rows {
            let _ = completion_tx.send(DocumentCompletion {
                identity: row.identity(),
                document: unavailable_document(),
                presentation: ProposalPresentation::RawParseFallback(inert_raw_fallback_source(
                    &unavailable_document().description,
                )),
            });
        }
        return;
    };
    let gateways = load_wallet_settings(db.as_ref())
        .ok()
        .map(|settings| settings.poi.artifact.gateway_urls)
        .unwrap_or_default();
    let futures = FuturesUnordered::new();
    for row in rows {
        let identity = row.identity();
        let cid = row.proposal.proposal_document.clone();
        let db = Arc::clone(&db);
        let http = http.clone();
        let gateways = gateways.clone();
        let semaphore = Arc::clone(&semaphore);
        futures.push(async move {
            let document = match semaphore.acquire_owned().await {
                Ok(permit) => {
                    let document = resolve_governance_document(&db, &http, &cid, &gateways).await;
                    drop(permit);
                    document
                }
                Err(_) => unavailable_document(),
            };
            let presentation = if document.available {
                let source = document.description.clone();
                let fallback_source = source.clone();
                match tokio::task::spawn_blocking(move || prepare_proposal_presentation(&source))
                    .await
                {
                    Ok(presentation) => presentation,
                    Err(_) => ProposalPresentation::RawParseFallback(inert_raw_fallback_source(
                        &fallback_source,
                    )),
                }
            } else {
                ProposalPresentation::RawParseFallback(inert_raw_fallback_source(
                    &document.description,
                ))
            };
            DocumentCompletion {
                identity,
                document,
                presentation,
            }
        });
    }
    send_document_completions(futures, completion_tx).await;
}

async fn send_document_completions<F>(
    mut futures: FuturesUnordered<F>,
    completion_tx: mpsc::UnboundedSender<DocumentCompletion>,
) where
    F: Future<Output = DocumentCompletion>,
{
    while let Some(completion) = futures.next().await {
        let _ = completion_tx.send(completion);
    }
}
fn unavailable_document() -> GovernanceDocument {
    GovernanceDocument {
        title: "Document unavailable".to_string(),
        description: String::new(),
        available: false,
    }
}
fn total_proposals(overview: &GovernanceOverview) -> eyre::Result<usize> {
    let v2 = usize::try_from(overview.v2.proposal_count)
        .map_err(|_| eyre::eyre!("governance V2 proposal count is too large"))?;
    let v1 = overview
        .v1
        .as_ref()
        .map_or(Ok(0), |s| usize::try_from(s.proposal_count))
        .map_err(|_| eyre::eyre!("governance V1 proposal count is too large"))?;
    v2.checked_add(v1)
        .ok_or_else(|| eyre::eyre!("governance proposal count overflows platform limits"))
}
fn rules_for(
    overview: &GovernanceOverview,
    version: wallet_ops::GovernanceContractVersion,
) -> eyre::Result<GovernanceContractRules> {
    Ok(match version {
        wallet_ops::GovernanceContractVersion::V2 => overview.v2.rules.clone(),
        wallet_ops::GovernanceContractVersion::V1 => overview
            .v1
            .as_ref()
            .ok_or_else(|| eyre::eyre!("governance V1 rules are unavailable"))?
            .rules
            .clone(),
    })
}
pub(super) const fn proposal_stage_label(stage: GovernanceProposalStage) -> &'static str {
    match stage {
        GovernanceProposalStage::AwaitingSponsorship => "Awaiting sponsorship",
        GovernanceProposalStage::ReadyToCallVote => "Ready to call vote",
        GovernanceProposalStage::SponsorshipExpired => "Sponsorship expired",
        GovernanceProposalStage::VoteCallExpired => "Vote call expired",
        GovernanceProposalStage::VotingDelay => "Voting delay",
        GovernanceProposalStage::VotingOpen => "Voting open",
        GovernanceProposalStage::NayOnlyVoting => "Nay-only voting",
        GovernanceProposalStage::Failed => "Failed",
        GovernanceProposalStage::PassedAwaitingExecution => "Passed · awaiting execution",
        GovernanceProposalStage::PassedExecutable => "Passed · executable",
        GovernanceProposalStage::ExecutionExpired => "Execution expired",
        GovernanceProposalStage::Executed => "Executed",
    }
}
const fn proposal_stage_color(stage: GovernanceProposalStage) -> u32 {
    match stage {
        GovernanceProposalStage::AwaitingSponsorship
        | GovernanceProposalStage::ReadyToCallVote
        | GovernanceProposalStage::VotingDelay => theme::WARNING,
        GovernanceProposalStage::VotingOpen | GovernanceProposalStage::NayOnlyVoting => theme::BLUE,
        GovernanceProposalStage::PassedAwaitingExecution
        | GovernanceProposalStage::PassedExecutable
        | GovernanceProposalStage::Executed => theme::SUCCESS,
        GovernanceProposalStage::Failed => theme::DANGER,
        GovernanceProposalStage::SponsorshipExpired
        | GovernanceProposalStage::VoteCallExpired
        | GovernanceProposalStage::ExecutionExpired => theme::TEXT_MUTED,
    }
}
const fn proposal_version_label(version: wallet_ops::GovernanceContractVersion) -> &'static str {
    match version {
        wallet_ops::GovernanceContractVersion::V2 => "V2",
        wallet_ops::GovernanceContractVersion::V1 => "V1",
    }
}
fn next_proposal_page(current: usize, total_pages: usize) -> Option<usize> {
    current.checked_add(1).filter(|page| *page < total_pages)
}
fn governance_permissionless_warning() -> Alert {
    Alert::warning(
        "wallet-governance-permissionless-warning",
        "Governance is permissionless. Proposals may contain misleading or malicious content. Exercise caution, verify claims independently, and do your own research before taking action.",
    )
    .small()
}
fn proposals_empty_state(message: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .p(px(48.0))
        .child(
            Icon::new(IconName::Inbox)
                .size_6()
                .text_color(rgb(theme::TEXT_MUTED)),
        )
        .child(app_muted_text(message))
}
fn proposal_copy_id(proposal: &ResolvedProposal, field: &str, control: &str) -> SharedString {
    SharedString::from(format!(
        "wallet-proposal-{}-{}-{field}-{control}",
        proposal_version_label(proposal.proposal.contract_version),
        proposal.proposal.index,
    ))
}
fn proposal_table_scroll_key(identity: &ProposalIdentity, ordinal: usize) -> String {
    format!(
        "wallet-proposal-table-scroll-v1-{}-{}-{}-{ordinal}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    )
}
fn proposal_row_group(identity: &ProposalIdentity) -> SharedString {
    SharedString::from(format!(
        "wallet-proposal-row-group-{}-{}-{}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    ))
}
fn format_deadline(deadline: &U256) -> String {
    format_datetime_short(deadline)
}
pub(super) fn format_date_short(timestamp: &U256) -> String {
    format_timestamp_with_pattern(timestamp, "%d %b %Y")
}
fn format_datetime_short(timestamp: &U256) -> String {
    format_timestamp_with_pattern(timestamp, "%d %b %Y, %H:%M")
}
fn format_timestamp_with_pattern(timestamp: &U256, pattern: &str) -> String {
    let Ok(seconds) = i64::try_from(*timestamp) else {
        return format!("Unix timestamp {timestamp}");
    };
    let Some(utc) = DateTime::<Utc>::from_timestamp(seconds, 0) else {
        return format!("Unix timestamp {timestamp}");
    };
    utc.with_timezone(&Local).format(pattern).to_string()
}

pub(super) fn format_compact_rail_amount(amount: U256) -> String {
    let scale = U256::from(10).pow(U256::from(18));
    let whole = amount / scale;
    if whole < U256::from(1_000) {
        return railgun_ui::format_token_amount(amount, 18);
    }
    let whole = u128::try_from(whole).unwrap_or(u128::MAX);
    if whole < 1_000_000 {
        let integer = whole / 1_000;
        let decimal = (whole % 1_000) / 100;
        return compact_suffix(integer, decimal, 'K');
    }
    if whole < 1_000_000_000 {
        let integer = whole / 1_000_000;
        let decimal = (whole % 1_000_000) / 10_000;
        return compact_suffix_two_decimals(integer, decimal, 'M');
    }
    let integer = whole / 1_000_000_000;
    let decimal = (whole % 1_000_000_000) / 10_000_000;
    compact_suffix_two_decimals(integer, decimal, 'B')
}
pub(super) fn format_compact_rail_amount_with_unit(amount: U256) -> String {
    format!("{} RAIL", format_compact_rail_amount(amount))
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProposalCapacityKind {
    Sponsorship,
    Voting,
}

const fn proposal_capacity_kind(stage: GovernanceProposalStage) -> ProposalCapacityKind {
    match stage {
        GovernanceProposalStage::AwaitingSponsorship
        | GovernanceProposalStage::ReadyToCallVote
        | GovernanceProposalStage::SponsorshipExpired
        | GovernanceProposalStage::VoteCallExpired => ProposalCapacityKind::Sponsorship,
        GovernanceProposalStage::VotingDelay
        | GovernanceProposalStage::VotingOpen
        | GovernanceProposalStage::NayOnlyVoting
        | GovernanceProposalStage::Failed
        | GovernanceProposalStage::PassedAwaitingExecution
        | GovernanceProposalStage::PassedExecutable
        | GovernanceProposalStage::ExecutionExpired
        | GovernanceProposalStage::Executed => ProposalCapacityKind::Voting,
    }
}

fn compact_suffix(integer: u128, decimal: u128, suffix: char) -> String {
    if decimal == 0 {
        format!("{integer}{suffix}")
    } else {
        format!("{integer}.{decimal}{suffix}")
    }
}
fn compact_suffix_two_decimals(integer: u128, decimal: u128, suffix: char) -> String {
    if decimal == 0 {
        format!("{integer}{suffix}")
    } else if decimal.is_multiple_of(10) {
        format!("{integer}.{}{}", decimal / 10, suffix)
    } else {
        format!("{integer}.{decimal:02}{suffix}")
    }
}
fn per_mille(numerator: U256, denominator: U256) -> u16 {
    if denominator.is_zero() {
        return 0;
    }
    let numerator = u128::try_from(numerator).unwrap_or(u128::MAX);
    let denominator = u128::try_from(denominator).unwrap_or(u128::MAX);
    u16::try_from((numerator.saturating_mul(1_000) / denominator).min(1_000)).unwrap_or(1_000)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProposalCapacitySummaryState {
    Empty,
    Loading,
    Unavailable,
    Full,
    Partial,
    Exhausted,
}

fn proposal_capacity_summary_state(
    available: Option<U256>,
    maximum: Option<U256>,
    loading_count: usize,
    unavailable_count: usize,
    participant_count: usize,
) -> ProposalCapacitySummaryState {
    if participant_count == 0 {
        return ProposalCapacitySummaryState::Empty;
    }
    if unavailable_count > 0 {
        return ProposalCapacitySummaryState::Unavailable;
    }
    if loading_count > 0 || available.is_none() || maximum.is_none() {
        return ProposalCapacitySummaryState::Loading;
    }
    let (Some(available), Some(maximum)) = (available, maximum) else {
        return ProposalCapacitySummaryState::Loading;
    };
    match (available, maximum) {
        (available, maximum) if available == maximum => ProposalCapacitySummaryState::Full,
        (available, maximum) if available.is_zero() && !maximum.is_zero() => {
            ProposalCapacitySummaryState::Exhausted
        }
        _ => ProposalCapacitySummaryState::Partial,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProposalClosedHistoryKind {
    Sponsorship,
    Voting,
}

const fn proposal_closed_history_kind(
    stage: GovernanceProposalStage,
) -> Option<ProposalClosedHistoryKind> {
    match stage {
        GovernanceProposalStage::SponsorshipExpired | GovernanceProposalStage::VoteCallExpired => {
            Some(ProposalClosedHistoryKind::Sponsorship)
        }
        GovernanceProposalStage::Failed
        | GovernanceProposalStage::PassedAwaitingExecution
        | GovernanceProposalStage::PassedExecutable
        | GovernanceProposalStage::ExecutionExpired
        | GovernanceProposalStage::Executed => Some(ProposalClosedHistoryKind::Voting),
        _ => None,
    }
}

fn vote_split(yay: U256, nay: U256) -> Option<u16> {
    let total = yay.checked_add(nay)?;
    if total.is_zero() {
        return None;
    }
    Some(per_mille(yay, total))
}
fn format_ratio_percent(per_mille: u16) -> String {
    format!("{}.{:01}%", per_mille / 10, per_mille % 10)
}
fn ratio_fraction(per_mille: u16) -> f32 {
    f32::from(per_mille) / 1_000.0
}
fn countdown(deadline: &U256, now: U256) -> Option<String> {
    let remaining = deadline.checked_sub(now)?;
    let remaining = u64::try_from(remaining).ok()?;
    Some(format_compact_duration(Duration::from_secs(remaining)))
}
fn list_voting_deadline(status: &GovernanceProposalStatus) -> Option<(U256, &'static str)> {
    match status.stage {
        GovernanceProposalStage::VotingOpen => {
            Some((*status.deadlines.yay_end.as_ref()?, "Yay voting ends in"))
        }
        GovernanceProposalStage::NayOnlyVoting => {
            Some((*status.deadlines.nay_end.as_ref()?, "Nay voting ends in"))
        }
        _ => None,
    }
}
enum ProposalRowTitle {
    Available(String),
    Unavailable,
}
fn proposal_row_title(title: Option<ProposalRowTitle>) -> gpui::Div {
    match title {
        Some(ProposalRowTitle::Available(title)) if !title.is_empty() => app_strong_text(title)
            .flex_1()
            .min_w(px(0.0))
            .text_size(px(15.0))
            .line_height(px(18.0))
            .font_weight(FontWeight::SEMIBOLD)
            .truncate(),
        Some(ProposalRowTitle::Available(_) | ProposalRowTitle::Unavailable) => {
            app_muted_text("Document unavailable")
                .flex_1()
                .min_w(px(0.0))
                .italic()
        }
        None => div()
            .flex_1()
            .min_w(px(0.0))
            .child(Skeleton::new().h(px(14.0)).w(px(180.0))),
    }
}
fn proposal_row_meta(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let mut left = div()
        .flex()
        .items_center()
        .gap_1()
        .flex_1()
        .min_w(px(0.0))
        .child(
            app_muted_text(format!(
                "Published {}",
                format_date_short(&proposal.proposal.publish_time)
            ))
            .text_size(px(12.0)),
        );
    if let Some(remaining) = match status.stage {
        GovernanceProposalStage::AwaitingSponsorship => {
            countdown(&status.deadlines.sponsorship, chain_time)
                .map(|remaining| format!(" · Sponsorship ends in {remaining}"))
        }
        GovernanceProposalStage::VotingOpen | GovernanceProposalStage::NayOnlyVoting => {
            list_voting_deadline(&status).and_then(|(deadline, label)| {
                countdown(&deadline, chain_time).map(|remaining| format!(" · {label} {remaining}"))
            })
        }
        GovernanceProposalStage::VotingDelay => countdown(
            &status
                .deadlines
                .voting_start
                .expect("called proposal deadline"),
            chain_time,
        )
        .map(|remaining| format!(" · Voting opens in {remaining}")),
        GovernanceProposalStage::PassedAwaitingExecution => countdown(
            &status
                .deadlines
                .execution_start
                .expect("called proposal deadline"),
            chain_time,
        )
        .map(|remaining| format!(" · Execution opens in {remaining}")),
        GovernanceProposalStage::PassedExecutable => countdown(
            &status
                .deadlines
                .execution_end
                .expect("called proposal deadline"),
            chain_time,
        )
        .map(|remaining| format!(" · Execution closes in {remaining}")),
        _ => None,
    } {
        left = left.child(app_muted_text(remaining).text_size(px(12.0)));
    }
    if proposal.proposal.contract_version == wallet_ops::GovernanceContractVersion::V1 {
        left = left.child(app_status_tag("V1", theme::TEXT_MUTED));
    }
    div()
        .flex()
        .items_center()
        .gap_2()
        .mt(px(4.0))
        .child(left)
        .child(render_list_meter(proposal, chain_time))
}
fn render_list_meter(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let group_name = proposal_row_group(&proposal.identity());
    let (for_amount, against_amount, caption, for_per_mille, color) = match status.stage {
        GovernanceProposalStage::AwaitingSponsorship
        | GovernanceProposalStage::ReadyToCallVote
        | GovernanceProposalStage::SponsorshipExpired
        | GovernanceProposalStage::VoteCallExpired => {
            let per_mille = sponsorship_per_mille(proposal);
            let percent = (u32::from(per_mille) + 5) / 10;
            (
                None,
                None,
                format!(
                    "{} / {} RAIL sponsored · {percent}%",
                    format_compact_rail_amount(proposal.proposal.sponsorship),
                    format_compact_rail_amount(proposal.rules.sponsor_threshold)
                ),
                per_mille,
                if matches!(
                    status.stage,
                    GovernanceProposalStage::SponsorshipExpired
                        | GovernanceProposalStage::VoteCallExpired
                ) {
                    theme::TEXT_MUTED
                } else {
                    theme::PRIMARY
                },
            )
        }
        _ => {
            let split = vote_split(proposal.proposal.yay_votes, proposal.proposal.nay_votes);
            let caption = split.map_or_else(
                || "No votes".to_string(),
                |_| {
                    format!(
                        "{} for · {} against",
                        format_compact_rail_amount(proposal.proposal.yay_votes),
                        format_compact_rail_amount(proposal.proposal.nay_votes)
                    )
                },
            );
            (
                Some(proposal.proposal.yay_votes),
                Some(proposal.proposal.nay_votes),
                caption,
                split.unwrap_or(0),
                theme::PRIMARY,
            )
        }
    };
    let track = div()
        .w(px(230.0))
        .h(px(6.0))
        .flex()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_HOVER))
        .child(
            if let (Some(yay), Some(nay)) = (for_amount, against_amount)
                && let Some(split) = vote_split(yay, nay)
            {
                let total_width = 230.0;
                let gap = if split > 0 && split < 1_000 { 2.0 } else { 0.0 };
                let split = ratio_fraction(split);
                div()
                    .flex()
                    .w(px(total_width))
                    .h(px(6.0))
                    .child(
                        div()
                            .w(px((total_width - gap) * split))
                            .h(px(6.0))
                            .bg(rgb(theme::PRIMARY)),
                    )
                    .child(
                        div()
                            .w(px(gap))
                            .h(px(6.0))
                            .bg(rgb(theme::SURFACE))
                            .group_hover(group_name, |style| {
                                style.bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                            }),
                    )
                    .child(div().flex_1().h(px(6.0)).bg(rgb(theme::DANGER)))
            } else {
                div()
                    .w(px((230.0 * ratio_fraction(for_per_mille)).clamp(0.0, 230.0)))
                    .h(px(6.0))
                    .bg(rgb(color))
            },
        );
    div()
        .w(px(230.0))
        .flex()
        .flex_col()
        .items_end()
        .gap_1()
        .child(track)
        .child(app_muted_text(caption).text_size(px(11.0)))
}
fn proposal_skeleton_row() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .h(px(72.0))
        .gap_3()
        .px(px(14.0))
        .py(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(Skeleton::new().h(px(14.0)).w(px(240.0)))
        .child(Skeleton::new().secondary().h(px(10.0)).w(px(160.0)))
}
fn proposal_detail_title(proposal: &ResolvedProposal) -> gpui::Div {
    match proposal.document() {
        None => div().child(Skeleton::new().h(px(22.0)).w(px(360.0))),
        Some(document) if document.available && !document.title.is_empty() => {
            app_strong_text(document.title.clone())
                .text_size(px(22.0))
                .font_weight(FontWeight::SEMIBOLD)
                .whitespace_normal()
        }
        Some(_) => app_strong_text(format!("Proposal #{}", proposal.proposal.index))
            .text_size(px(22.0))
            .font_weight(FontWeight::SEMIBOLD),
    }
}

fn render_proposal_participation_card(
    root: &Entity<WalletRoot>,
    wallet: &WalletRoot,
    proposal: &ResolvedProposal,
    stage: GovernanceProposalStage,
) -> gpui::Div {
    let history_kind = proposal_closed_history_kind(stage);
    let actionable = matches!(
        stage,
        GovernanceProposalStage::AwaitingSponsorship
            | GovernanceProposalStage::ReadyToCallVote
            | GovernanceProposalStage::VotingOpen
            | GovernanceProposalStage::NayOnlyVoting
    );
    let key_matches = wallet
        .governance
        .proposal_participation
        .key
        .as_ref()
        .is_some_and(|key| {
            key.index == proposal.proposal.index
                && key.contract == proposal.proposal.contract_address
                && key.version == proposal.proposal.contract_version
        });
    let capacity_kind = proposal_capacity_kind(stage);
    let power_phrase = match capacity_kind {
        ProposalCapacityKind::Sponsorship => "sponsorship",
        ProposalCapacityKind::Voting => "voting",
    };
    let expanded = wallet.proposals.selected.as_ref().is_some_and(|selection| {
        selection.identity == proposal.identity() && selection.participation_expanded
    });
    let participants = wallet.governance_participants();
    let total_accounts = participants.len();
    let mut aggregate_available = Some(U256::ZERO);
    let mut aggregate_maximum = Some(U256::ZERO);
    let mut aggregate_valid = key_matches || participants.is_empty();
    let mut total_used_accounts = 0usize;
    let mut inactive_count = 0usize;
    let mut loading_count = 0usize;
    let mut unavailable_count = 0usize;
    let mut closed_sponsored = Some(U256::ZERO);
    let mut closed_voted = Some(U256::ZERO);
    for account in &participants {
        let row = wallet
            .governance
            .proposal_participation
            .rows
            .get(&account.address);
        if account.status == wallet_ops::vault::PublicAccountStatus::Inactive {
            inactive_count += 1;
        }
        if !key_matches {
            continue;
        }
        match row {
            Some(ProposalParticipationRow::Loading) | None => loading_count += 1,
            Some(ProposalParticipationRow::Unavailable(_)) => unavailable_count += 1,
            Some(ProposalParticipationRow::Ready(participation)) => {
                let participation_matches = participation.proposal_version
                    == proposal.proposal.contract_version
                    && participation.proposal_id == proposal.proposal.index
                    && participation.voting_contract == proposal.proposal.contract_address;
                if !participation_matches {
                    unavailable_count += 1;
                    continue;
                }
                if let Some(kind) = history_kind {
                    let sponsored = closed_sponsored
                        .and_then(|total| total.checked_add(participation.sponsored));
                    let voted =
                        closed_voted.and_then(|total| total.checked_add(participation.voted));
                    if sponsored.is_none() || voted.is_none() {
                        if aggregate_valid {
                            unavailable_count += 1;
                        }
                        aggregate_valid = false;
                    }
                    closed_sponsored = sponsored;
                    closed_voted = voted;
                    if matches!(kind, ProposalClosedHistoryKind::Voting) {
                        if !participation.voted.is_zero() {
                            total_used_accounts += 1;
                        }
                    } else if !participation.sponsored.is_zero() {
                        total_used_accounts += 1;
                    }
                    continue;
                }
                let sponsor_capacity = participation.sponsorship_capacity();
                let voting_capacity = participation.voting_capacity();
                let capacity = match capacity_kind {
                    ProposalCapacityKind::Sponsorship => &sponsor_capacity,
                    ProposalCapacityKind::Voting => &voting_capacity,
                };
                let Ok(capacity) = capacity else {
                    unavailable_count += 1;
                    aggregate_valid = false;
                    continue;
                };
                let Some((available, maximum)) = capacity.remaining.zip(capacity.snapshot_power)
                else {
                    unavailable_count += 1;
                    aggregate_valid = false;
                    continue;
                };
                aggregate_available =
                    aggregate_available.and_then(|total| total.checked_add(available));
                aggregate_maximum = aggregate_maximum.and_then(|total| total.checked_add(maximum));
                if aggregate_available.is_none() || aggregate_maximum.is_none() {
                    if aggregate_valid {
                        unavailable_count += 1;
                    }
                    aggregate_valid = false;
                }
                if !capacity.allocated.is_zero() {
                    total_used_accounts += 1;
                }
            }
        }
    }
    if !key_matches && total_accounts > 0 {
        loading_count = total_accounts;
    }
    let mut details = div().w_full().flex().flex_col().mt(px(8.0));
    if participants.is_empty() {
        details = details.child(
            app_muted_text("Enroll a Public account to view voting power and allocations.")
                .mb(px(8.0)),
        );
    }
    for account in &participants {
        let row = wallet
            .governance
            .proposal_participation
            .rows
            .get(&account.address);
        let label =
            super::public_account::public_account_display_label(account).unwrap_or_else(|| {
                super::spend_authorization::spend_authorization_recipient_display(&format!(
                    "{:#x}",
                    account.address
                ))
            });
        let mut row_card = div()
            .flex()
            .flex_wrap()
            .items_start()
            .gap_3()
            .py(px(9.0))
            .px(px(2.0))
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE));
        let mut account_column = div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(250.0))
            .flex_none()
            .min_w(px(0.0))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(app_strong_text(label))
                    .child(
                        app_muted_text(
                            super::spend_authorization::spend_authorization_recipient_display(
                                &format!("{:#x}", account.address),
                            ),
                        )
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_size(px(11.0))
                        .truncate(),
                    ),
            );
        if account.status == wallet_ops::vault::PublicAccountStatus::Inactive {
            account_column = account_column.child(
                app_muted_text("Inactive account")
                    .text_size(px(10.0))
                    .text_color(rgb(theme::WARNING)),
            );
        }
        row_card = row_card.child(account_column);
        let mut capacity_column = div().flex().flex_col().gap_1().flex_1().min_w(px(220.0));
        let mut actions = div().flex().flex_wrap().gap_2().flex_none();
        match row {
            Some(ProposalParticipationRow::Ready(participation)) => {
                let participation_matches = participation.proposal_version
                    == proposal.proposal.contract_version
                    && participation.proposal_id == proposal.proposal.index
                    && participation.voting_contract == proposal.proposal.contract_address;
                if participation_matches {
                    if let Some(history_kind) = history_kind {
                        let allocation = match history_kind {
                            ProposalClosedHistoryKind::Sponsorship => {
                                if participation.sponsored.is_zero() {
                                    "No sponsorship allocated".to_owned()
                                } else {
                                    format!(
                                        "{} sponsored",
                                        format_compact_rail_amount_with_unit(
                                            participation.sponsored
                                        )
                                    )
                                }
                            }
                            ProposalClosedHistoryKind::Voting => {
                                if participation.voted.is_zero()
                                    && participation.sponsored.is_zero()
                                {
                                    "No allocation".to_owned()
                                } else if participation.voted.is_zero() {
                                    format!(
                                        "{} sponsored",
                                        format_compact_rail_amount_with_unit(
                                            participation.sponsored
                                        )
                                    )
                                } else if participation.sponsored.is_zero() {
                                    format!(
                                        "{} voted",
                                        format_compact_rail_amount_with_unit(participation.voted)
                                    )
                                } else {
                                    format!(
                                        "{} voted · {} sponsored",
                                        format_compact_rail_amount_with_unit(participation.voted),
                                        format_compact_rail_amount_with_unit(
                                            participation.sponsored
                                        )
                                    )
                                }
                            }
                        };
                        capacity_column =
                            capacity_column.child(app_text(allocation).text_size(px(12.0)));
                    } else {
                        let capacity_result = match capacity_kind {
                            ProposalCapacityKind::Sponsorship => {
                                participation.sponsorship_capacity()
                            }
                            ProposalCapacityKind::Voting => participation.voting_capacity(),
                        };
                        match capacity_result {
                            Ok(capacity) => {
                                let remaining = capacity.remaining.unwrap_or_default();
                                let snapshot = capacity.snapshot_power.unwrap_or_default();
                                let kind = match capacity_kind {
                                    ProposalCapacityKind::Sponsorship => "sponsor",
                                    ProposalCapacityKind::Voting => "vote",
                                };
                                if capacity.allocated.is_zero() {
                                    capacity_column = capacity_column.child(
                                        app_text(format!(
                                            "{} available to {kind}",
                                            format_compact_rail_amount_with_unit(snapshot)
                                        ))
                                        .text_size(px(12.0)),
                                    );
                                } else {
                                    capacity_column = capacity_column.child(
                                        app_text(format!(
                                            "{} of {} RAIL",
                                            format_compact_rail_amount(remaining),
                                            format_compact_rail_amount(snapshot)
                                        ))
                                        .text_size(px(12.0)),
                                    );
                                    capacity_column = capacity_column.child(
                                        app_muted_text(format!(
                                            "available · {} {}",
                                            format_compact_rail_amount_with_unit(
                                                capacity.allocated
                                            ),
                                            if capacity_kind == ProposalCapacityKind::Sponsorship {
                                                "sponsored"
                                            } else {
                                                "voted"
                                            }
                                        ))
                                        .text_size(px(11.0)),
                                    );
                                    capacity_column = capacity_column.child(
                                        Progress::new().w_full().max_w(px(340.0)).value(
                                            f32::from(per_mille(remaining, snapshot)) / 10.0,
                                        ),
                                    );
                                    if remaining.is_zero() {
                                        capacity_column = capacity_column.child(
                                            app_muted_text(format!(
                                                "All {power_phrase} power used for this proposal"
                                            ))
                                            .text_size(px(11.0))
                                            .text_color(rgb(theme::WARNING)),
                                        );
                                    }
                                }
                                if capacity_kind == ProposalCapacityKind::Voting
                                    && !participation.sponsored.is_zero()
                                {
                                    capacity_column = capacity_column.child(
                                        app_muted_text(format!(
                                            "Sponsored {} earlier in this proposal",
                                            format_compact_rail_amount_with_unit(
                                                participation.sponsored
                                            )
                                        ))
                                        .text_size(px(11.0)),
                                    );
                                }
                                let active_snapshot = match capacity_kind {
                                    ProposalCapacityKind::Sponsorship => {
                                        participation.sponsorship_snapshot.voting_power
                                    }
                                    ProposalCapacityKind::Voting => {
                                        participation.voting_snapshot.voting_power
                                    }
                                };
                                if participation.current_voting_power != active_snapshot {
                                    capacity_column = capacity_column.child(
                                        app_muted_text(format!(
                                            "Live received power {} (snapshot {})",
                                            format_compact_rail_amount_with_unit(
                                                participation.current_voting_power
                                            ),
                                            format_compact_rail_amount_with_unit(active_snapshot)
                                        ))
                                        .text_size(px(11.0)),
                                    );
                                }
                            }
                            Err(error) => {
                                capacity_column = capacity_column.child(
                                    Alert::error(
                                        SharedString::from(format!(
                                            "governance-capacity-error-{}",
                                            account.address
                                        )),
                                        format!("Capacity unavailable: {error}"),
                                    )
                                    .small(),
                                );
                            }
                        }
                    }
                } else {
                    capacity_column = capacity_column.child(app_muted_text("Participation data does not match this proposal; actions are unavailable.").text_size(px(11.0)));
                }
                if account.status == wallet_ops::vault::PublicAccountStatus::Inactive {
                    capacity_column = capacity_column.child(
                        app_muted_text(
                            "Inactive account, shown for information; actions disabled.",
                        )
                        .text_size(px(11.0))
                        .text_color(rgb(theme::WARNING)),
                    );
                }
                let sponsor_remaining = participation
                    .sponsorship_capacity()
                    .ok()
                    .and_then(|capacity| capacity.remaining);
                let voting_remaining = participation
                    .voting_capacity()
                    .ok()
                    .and_then(|capacity| capacity.remaining);
                if key_matches
                    && participation_matches
                    && actionable
                    && account.status == wallet_ops::vault::PublicAccountStatus::Active
                {
                    let actor = account.address;
                    let open = |kind: ProposalActionKind, root: &Entity<WalletRoot>| {
                        let root = root.clone();
                        let mut button = app_button_base(SharedString::from(format!(
                            "governance-proposal-action-{kind:?}-{actor}"
                        )))
                        .child(match kind {
                            ProposalActionKind::Sponsor => "Sponsor",
                            ProposalActionKind::Unsponsor => "Unsponsor",
                            ProposalActionKind::CallVote => "Call vote",
                            ProposalActionKind::Yay => "Yay",
                            ProposalActionKind::Nay => "Nay",
                        });
                        button = match kind {
                            ProposalActionKind::Yay => {
                                button.small().primary().icon(IconName::ThumbsUp)
                            }
                            ProposalActionKind::Nay => {
                                button.small().warning().icon(IconName::ThumbsDown)
                            }
                            ProposalActionKind::Sponsor
                            | ProposalActionKind::Unsponsor
                            | ProposalActionKind::CallVote => button.outline().small(),
                        };
                        button.on_click(move |_event, window, cx| {
                            root.update(cx, |root, cx| {
                                root.open_proposal_action(actor, kind, window, cx);
                            });
                        })
                    };
                    match stage {
                        GovernanceProposalStage::AwaitingSponsorship => {
                            if sponsor_remaining.is_some_and(|remaining| !remaining.is_zero()) {
                                actions = actions.child(open(ProposalActionKind::Sponsor, root));
                            }
                            if !participation.sponsored.is_zero() {
                                actions = actions.child(open(ProposalActionKind::Unsponsor, root));
                            }
                        }
                        GovernanceProposalStage::ReadyToCallVote => {
                            if sponsor_remaining.is_some_and(|remaining| !remaining.is_zero()) {
                                actions = actions.child(open(ProposalActionKind::Sponsor, root));
                            }
                            if !participation.sponsored.is_zero() {
                                actions = actions.child(open(ProposalActionKind::Unsponsor, root));
                            }
                            actions = actions.child(open(ProposalActionKind::CallVote, root));
                        }
                        GovernanceProposalStage::VotingOpen => {
                            if voting_remaining.is_some_and(|remaining| !remaining.is_zero()) {
                                actions = actions
                                    .child(open(ProposalActionKind::Yay, root))
                                    .child(open(ProposalActionKind::Nay, root));
                            }
                        }
                        GovernanceProposalStage::NayOnlyVoting
                            if voting_remaining.is_some_and(|remaining| !remaining.is_zero()) =>
                        {
                            actions = actions.child(open(ProposalActionKind::Nay, root));
                        }
                        _ => {}
                    }
                }
            }
            Some(ProposalParticipationRow::Unavailable(error)) => {
                capacity_column = capacity_column
                    .child(
                        Alert::error(
                            SharedString::from(format!(
                                "governance-participation-error-{}",
                                account.address
                            )),
                            error.to_string(),
                        )
                        .small(),
                    )
                    .child(
                        app_muted_text("Actions unavailable until this account refreshes.")
                            .text_size(px(11.0)),
                    );
            }
            Some(ProposalParticipationRow::Loading) | None => {
                capacity_column = capacity_column
                    .child(app_muted_text("Loading account participation…").text_size(px(11.0)));
            }
        }
        row_card = row_card.child(capacity_column).child(actions);
        details = details.child(row_card);
    }
    if !key_matches {
        details = details.child(
            app_muted_text("Refreshing participation for this proposal…")
                .text_size(px(11.0))
                .mt(px(8.0)),
        );
    }
    let summary_state = if !key_matches && total_accounts > 0 {
        ProposalCapacitySummaryState::Loading
    } else if !aggregate_valid {
        ProposalCapacitySummaryState::Unavailable
    } else {
        proposal_capacity_summary_state(
            aggregate_available,
            aggregate_maximum,
            loading_count,
            unavailable_count,
            total_accounts,
        )
    };
    let (kind_phrase, action_word) = match capacity_kind {
        ProposalCapacityKind::Sponsorship => ("sponsor", "sponsored"),
        ProposalCapacityKind::Voting => ("vote", "voted"),
    };
    let account_word = if total_accounts == 1 {
        "account"
    } else {
        "accounts"
    };
    let mut summary = div().w_full().flex().flex_col().gap_2();
    match (summary_state, history_kind) {
        (ProposalCapacitySummaryState::Empty, _) => {
            summary = summary.child(
                app_muted_text(
                    "No participating accounts enrolled · enroll an account to view governance power",
                )
                .text_size(px(12.0)),
            );
        }
        (ProposalCapacitySummaryState::Loading, _) => {
            summary = summary.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .py(px(6.0))
                    .child(Spinner::new().small())
                    .child(app_muted_text("Loading account participation…").text_size(px(12.0))),
            );
        }
        (ProposalCapacitySummaryState::Unavailable, _) => {
            let unavailable_word = if unavailable_count == 1 {
                "account unavailable"
            } else {
                "accounts unavailable"
            };
            let inactive_suffix = if inactive_count == 0 {
                String::new()
            } else {
                format!(" · {inactive_count} inactive")
            };
            summary = summary.child(div().w_full().flex().items_baseline().flex_wrap().gap_2().child(app_text("—").text_size(px(16.0)).text_color(rgb(theme::DANGER))).child(app_muted_text("available power unknown").text_size(px(12.0))).child(div().ml_auto().child(app_text(format!("{unavailable_count} {unavailable_word} · expand for details{inactive_suffix}")).text_size(px(12.0)).text_color(rgb(theme::DANGER)))));
        }
        (
            ProposalCapacitySummaryState::Full
            | ProposalCapacitySummaryState::Partial
            | ProposalCapacitySummaryState::Exhausted,
            Some(history),
        ) => {
            let sponsored = closed_sponsored.unwrap_or_default();
            let voted = closed_voted.unwrap_or_default();
            let (history_text, history_amount) = match history {
                ProposalClosedHistoryKind::Sponsorship => {
                    ("Sponsorship closed · your accounts allocated", sponsored)
                }
                ProposalClosedHistoryKind::Voting => {
                    ("Voting closed · your accounts allocated", voted)
                }
            };
            let right = if total_accounts == 0 {
                "0 accounts · enroll an account to view history".to_owned()
            } else if inactive_count > 0 {
                format!(
                    "{total_accounts} {account_word} · {total_used_accounts} {action_word} · {inactive_count} inactive"
                )
            } else {
                format!("{total_accounts} {account_word} · {total_used_accounts} {action_word}")
            };
            summary = summary.child(
                div()
                    .w_full()
                    .flex()
                    .items_baseline()
                    .flex_wrap()
                    .gap_2()
                    .child(app_muted_text(history_text).text_size(px(12.0)).mr(px(8.0)))
                    .child(
                        app_text(format_compact_rail_amount_with_unit(history_amount))
                            .text_size(px(16.0))
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .child(app_muted_text(right).text_size(px(12.0))),
                    ),
            );
        }
        (state, None) => {
            let available = aggregate_available.unwrap_or_default();
            let maximum = aggregate_maximum.unwrap_or_default();
            let amount = if state == ProposalCapacitySummaryState::Full {
                format_compact_rail_amount_with_unit(available)
            } else {
                format!(
                    "{} of {} RAIL",
                    format_compact_rail_amount(available),
                    format_compact_rail_amount(maximum)
                )
            };
            let right = if total_accounts == 0 {
                "0 accounts · enroll an account to participate".to_owned()
            } else if state == ProposalCapacitySummaryState::Exhausted {
                if inactive_count > 0 {
                    format!(
                        "{total_accounts} {account_word} · all {power_phrase} power is used · {inactive_count} inactive"
                    )
                } else {
                    format!("{total_accounts} {account_word} · all {power_phrase} power is used")
                }
            } else if total_used_accounts == 0 {
                if inactive_count > 0 {
                    format!(
                        "{total_accounts} {account_word} · none {action_word} yet · {inactive_count} inactive"
                    )
                } else {
                    format!("{total_accounts} {account_word} · none {action_word} yet")
                }
            } else {
                if inactive_count > 0 {
                    format!(
                        "{total_accounts} {account_word} · {total_used_accounts} {action_word} · {inactive_count} inactive"
                    )
                } else {
                    format!("{total_accounts} {account_word} · {total_used_accounts} {action_word}")
                }
            };
            let amount_text = app_text(amount)
                .text_size(px(16.0))
                .font_weight(FontWeight::SEMIBOLD);
            let amount_text = if state == ProposalCapacitySummaryState::Exhausted {
                amount_text.text_color(rgb(theme::WARNING))
            } else {
                amount_text
            };
            summary = summary.child(
                div()
                    .w_full()
                    .flex()
                    .items_baseline()
                    .flex_wrap()
                    .gap_2()
                    .child(amount_text)
                    .child(
                        app_muted_text(format!("available to {kind_phrase}"))
                            .text_size(px(12.0))
                            .ml(px(8.0)),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .child(app_muted_text(right).text_size(px(12.0))),
                    ),
            );
            if matches!(
                state,
                ProposalCapacitySummaryState::Partial | ProposalCapacitySummaryState::Exhausted
            ) {
                summary = summary.child(
                    Progress::new()
                        .w_full()
                        .value(f32::from(per_mille(available, maximum)) / 10.0),
                );
            }
        }
    }
    let toggle_root = root.clone();
    let toggle_id = SharedString::from(format!(
        "wallet-proposal-participation-toggle-{}-{}-{}",
        proposal_version_label(proposal.proposal.contract_version),
        proposal.proposal.contract_address,
        proposal.proposal.index,
    ));
    let card = proposal_card("Participation").child(
        Collapsible::new()
            .open(expanded)
            .w_full()
            .child(summary)
            .content(details),
    );
    div().relative().w_full().pb(px(10.0)).child(card).child(
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(0.0))
            .flex()
            .justify_center()
            .child(
                app_button_base(toggle_id)
                    .outline()
                    .bg(rgb(theme::SURFACE))
                    .xsmall()
                    .rounded_full()
                    .icon(if expanded {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .tooltip(if expanded {
                        "Hide participation details"
                    } else {
                        "Show participation details"
                    })
                    .on_click(move |_event, _window, cx| {
                        toggle_root.update(cx, |root, cx| {
                            root.toggle_proposal_participation(cx);
                        });
                    }),
            ),
    )
}

pub(super) const fn proposal_action_title(kind: ProposalActionKind) -> &'static str {
    match kind {
        ProposalActionKind::Sponsor => "Sponsor proposal",
        ProposalActionKind::Unsponsor => "Withdraw sponsorship",
        ProposalActionKind::CallVote => "Call vote",
        ProposalActionKind::Yay => "Vote Yay",
        ProposalActionKind::Nay => "Vote Nay",
    }
}

fn render_proposal_action_form(
    root: &Entity<WalletRoot>,
    wallet: &WalletRoot,
    proposal: &ResolvedProposal,
    decimals: Option<u8>,
    content_width: gpui::Pixels,
    chain_time: U256,
    cx: &App,
) -> Option<gpui::Div> {
    let displayed_key =
        proposal_participation_key(&proposal.proposal, wallet.governance_context_key());
    if !wallet.proposal_participation_key_matches(&proposal.proposal, &displayed_key) {
        return None;
    }
    let Some(super::governance::GovernanceActionSelection::Proposal(selection)) =
        wallet.governance.action_flow.selection.as_ref()
    else {
        return None;
    };
    let Some(ProposalParticipationRow::Ready(participation)) = wallet
        .governance
        .proposal_participation
        .rows
        .get(&selection.actor)
    else {
        return Some(
            app_muted_text(
                "Selected account participation is unavailable; refresh before authorizing.",
            )
            .text_size(px(11.0)),
        );
    };
    let participation = participation.as_ref();
    if participation.proposal_version != proposal.proposal.contract_version
        || participation.proposal_id != proposal.proposal.index
        || participation.voting_contract != proposal.proposal.contract_address
    {
        return None;
    }
    let amount_input = &wallet.governance.proposal_action_amount_input;
    let raw_amount = amount_input.read(cx).value();
    let raw_amount_present = !raw_amount.trim().is_empty();
    let parsed_amount = wallet_ops::parse_send_amount(raw_amount.as_ref(), decimals);
    let amount = parsed_amount
        .as_ref()
        .ok()
        .filter(|amount| !amount.is_zero())
        .copied();
    let (maximum, capacity_label) = match selection.kind {
        ProposalActionKind::Sponsor => (
            participation
                .sponsorship_capacity()
                .ok()
                .and_then(|capacity| capacity.remaining),
            "available to sponsor",
        ),
        ProposalActionKind::Unsponsor => (Some(participation.sponsored), "sponsored"),
        ProposalActionKind::Yay | ProposalActionKind::Nay => (
            participation
                .voting_capacity()
                .ok()
                .and_then(|capacity| capacity.remaining),
            "available voting power",
        ),
        ProposalActionKind::CallVote => (None, ""),
    };
    let sponsor_lockout_until = matches!(selection.kind, ProposalActionKind::Sponsor)
        .then(|| {
            (participation.last_sponsored.proposal_id != proposal.proposal.index)
                .then_some(participation.last_sponsored.last_sponsor_time)
                .and_then(|last| {
                    last.checked_add(U256::from(
                        wallet_ops::GOVERNANCE_SPONSOR_LOCKOUT_TIME_SECONDS,
                    ))
                })
        })
        .flatten()
        .filter(|until| chain_time <= *until);
    let capacity_text = maximum.map(|maximum| {
        format!(
            "{} RAIL {}",
            format_compact_rail_amount(maximum),
            capacity_label,
        )
    });
    let validation = if sponsor_lockout_until.is_some() {
        Some("This account is temporarily locked from sponsoring another proposal.".to_owned())
    } else if raw_amount_present {
        validate_proposal_action(
            &proposal.proposal,
            &proposal.rules,
            chain_time,
            participation,
            selection.kind,
            amount,
        )
        .err()
        .map(|error| humanize_proposal_action_error(error, selection.kind, maximum))
    } else {
        None
    };
    let parse_error = raw_amount_present
        .then_some(parsed_amount.as_ref().err())
        .flatten()
        .map(|error| format!("Enter a valid RAIL amount ({error})"));
    let validation = parse_error.or(validation);
    let ready = if matches!(selection.kind, ProposalActionKind::CallVote) {
        true
    } else {
        amount.is_some() && validation.is_none() && sponsor_lockout_until.is_none()
    };
    let proposal_for_review = proposal.proposal.clone();
    let selection = *selection;
    let action_pending = wallet.governance.action_flow.pending;
    let close_root = root.clone();
    let max_root = root.clone();
    let review_root = root.clone();
    let max_input = amount_input.clone();
    let max_value = maximum.map(|maximum| format_send_amount_input(maximum, decimals));
    let action_label = "Prepare authorization";
    let display_label = public_account_display_label_for_proposal_actor(wallet, selection.actor);
    let mut content = div()
        .w(content_width)
        .flex()
        .flex_col()
        .gap_3()
        .child(
            app_muted_text(format!(
                "Proposal #{} · {}",
                proposal.proposal.index,
                proposal
                    .document()
                    .filter(|document| document.available && !document.title.is_empty())
                    .map_or("Governance proposal", |document| document.title.as_str()),
            ))
            .text_size(px(12.0))
            .truncate(),
        )
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .p_2()
                .rounded_md()
                .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .min_w(px(0.0))
                        .child(
                            app_strong_text(display_label)
                                .text_size(px(12.0))
                                .line_height(px(16.0)),
                        )
                        .child(
                            app_muted_text(short_address(&selection.actor))
                                .font_family(APP_MONO_FONT_FAMILY)
                                .text_size(px(11.0))
                                .line_height(px(16.0))
                                .truncate(),
                        ),
                )
                .when_some(capacity_text, |this, text| {
                    this.child(
                        app_muted_text(text)
                            .text_size(px(11.0))
                            .line_height(px(16.0))
                            .ml_auto()
                            .truncate(),
                    )
                }),
        );
    if !matches!(selection.kind, ProposalActionKind::CallVote) {
        content =
            content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(app_muted_text("Amount").text_size(px(12.0)))
                            .when_some(maximum, |this, maximum| {
                                this.child(
                                    app_button_base("governance-action-max")
                                        .link()
                                        .xsmall()
                                        .compact()
                                        .disabled(sponsor_lockout_until.is_some())
                                        .child(format!(
                                            "Max: {} RAIL",
                                            format_send_amount_input(maximum, decimals)
                                        ))
                                        .on_click(move |_event, window, cx| {
                                            if let Some(value) = &max_value {
                                                max_input.update(cx, |input, cx| {
                                                    input.set_value(value.clone(), window, cx);
                                                    cx.notify();
                                                });
                                            }
                                            max_root.update(cx, |_, cx| cx.notify());
                                        }),
                                )
                            }),
                    )
                    .child(
                        app_input(amount_input)
                            .small()
                            .disabled(sponsor_lockout_until.is_some())
                            .suffix(app_muted_text("RAIL").text_size(px(11.0))),
                    )
                    .children(validation.clone().map(|error| {
                        app_muted_text(error)
                            .text_color(rgb(theme::DANGER))
                            .text_size(px(11.0))
                    }))
                    .children(
                        (validation.is_none()
                            && amount.is_some()
                            && matches!(selection.kind, ProposalActionKind::Unsponsor))
                        .then(|| {
                            app_muted_text("Returns to this account's available sponsorship power")
                                .text_size(px(11.0))
                        }),
                    )
                    .children(
                        (validation.is_none()
                            && amount.is_some()
                            && !matches!(selection.kind, ProposalActionKind::Unsponsor))
                        .then(|| {
                            let remaining = maximum.and_then(|maximum| {
                                amount.and_then(|amount| maximum.checked_sub(amount))
                            });
                            remaining.map(|remaining| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        app_muted_text(format!(
                                            "{} RAIL will remain available",
                                            format_compact_rail_amount(remaining)
                                        ))
                                        .text_size(px(11.0)),
                                    )
                                    .child(Progress::new().w_full().value(
                                        maximum.filter(|maximum| !maximum.is_zero()).map_or(
                                            0.0,
                                            |maximum| {
                                                f32::from(per_mille(remaining, maximum)) / 10.0
                                            },
                                        ),
                                    ))
                            })
                        })
                        .flatten(),
                    ),
            );
    }
    if matches!(selection.kind, ProposalActionKind::Sponsor) {
        if let Some(unlock) = sponsor_lockout_until {
            content = content.child(
                Alert::warning(
                    "governance-action-sponsor-lockout",
                    format!(
                        "Sponsoring another proposal is locked until {}.",
                        format_datetime_short(&unlock)
                    ),
                )
                .small(),
            );
        }
        content = content.child(app_muted_text(
            "Sponsorship can be withdrawn before the vote is called. Sponsoring starts a 7-day lockout on other proposals with this account.",
        ).text_size(px(11.0)));
    }
    if matches!(selection.kind, ProposalActionKind::Unsponsor) {
        let after = proposal
            .proposal
            .sponsorship
            .checked_sub(amount.unwrap_or_default());
        if after.is_some_and(|after| after < proposal.rules.sponsor_threshold) {
            content = content.child(
                Alert::warning(
                    "governance-action-unsponsor-warning",
                    "This withdrawal lowers proposal sponsorship below the threshold required to call a vote.",
                )
                .small(),
            );
        }
    }
    let action_error = wallet
        .governance
        .action_flow
        .error
        .as_ref()
        .map(|error| Alert::error("governance-action-error", error.to_string()).small());
    content = content.children(action_error).child(
        div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                app_button_base("governance-action-cancel")
                    .ghost()
                    .small()
                    .child("Cancel")
                    .on_click(move |_event, window, cx| {
                        close_root.update(cx, WalletRoot::close_proposal_action);
                        window.close_dialog(cx);
                    }),
            )
            .child(
                app_button_base("governance-action-review")
                    .primary()
                    .small()
                    .loading(action_pending)
                    .disabled(!ready || action_pending)
                    .child(if action_pending {
                        "Preparing authorization…"
                    } else {
                        action_label
                    })
                    .on_click(move |_event, window, cx| {
                        review_root.update(cx, |root, cx| {
                            root.review_proposal_action(
                                &proposal_for_review,
                                selection,
                                amount,
                                window,
                                cx,
                            );
                        });
                    }),
            ),
    );
    Some(content)
}

fn public_account_display_label_for_proposal_actor(wallet: &WalletRoot, actor: Address) -> String {
    wallet
        .governance_participants()
        .into_iter()
        .find(|account| account.address == actor)
        .and_then(|account| super::public_account::public_account_display_label(&account))
        .unwrap_or_else(|| "Public account".to_owned())
}

fn humanize_proposal_action_error(
    error: String,
    kind: ProposalActionKind,
    maximum: Option<U256>,
) -> String {
    let amount_limit =
        maximum.map(|maximum| format!(" ({})", format_compact_rail_amount_with_unit(maximum)));
    let limit = amount_limit.as_deref().unwrap_or("");
    match kind {
        ProposalActionKind::Sponsor if error.contains("sponsorship capacity") => {
            format!("Exceeds this account's available sponsorship power{limit}")
        }
        ProposalActionKind::Unsponsor if error.contains("sponsored") => {
            format!("Exceeds this account's sponsored amount{limit}")
        }
        ProposalActionKind::Yay | ProposalActionKind::Nay
            if error.contains("voting capacity") || error.contains("snapshot") =>
        {
            format!("Exceeds this account's available voting power{limit}")
        }
        _ => error,
    }
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

fn proposal_source_slice(
    source: &str,
    position: Option<&markdown::unist::Position>,
) -> Option<String> {
    let position = position?;
    source
        .get(position.start.offset..position.end.offset)
        .map(str::to_owned)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProposalSourceEdit {
    start: usize,
    end: usize,
    replacement: String,
}

fn max_backtick_run(value: &str) -> usize {
    let mut longest_run = 0;
    let mut run: usize = 0;
    for character in value.chars() {
        if character == '`' {
            run = run.saturating_add(1);
            longest_run = longest_run.max(run);
        } else {
            run = 0;
        }
    }
    longest_run
}

fn inert_code_span(value: &str) -> String {
    let longest_run = max_backtick_run(value);
    let fence = "`".repeat(longest_run.saturating_add(1).max(1));
    format!("{fence}{value}{fence}")
}

fn inert_raw_fallback_source(source: &str) -> String {
    let fence = "`".repeat(max_backtick_run(source).saturating_add(1).max(3));
    format!("{fence}\n{source}\n{fence}")
}

fn inert_text_source(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_string()
            } else {
                format!("&#{};", u32::from(character))
            }
        })
        .collect()
}

fn url_shaped_label(value: &str) -> bool {
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("mailto:")
        || value.starts_with("xmpp:")
        || value.starts_with("www.")
        || (value.contains('@') && !value.chars().any(char::is_whitespace))
}

fn visible_node_text(node: &Node) -> String {
    match node {
        Node::Image(image) => image.alt.clone(),
        Node::ImageReference(image) => image.alt.clone(),
        Node::Break(_) => "\n".to_owned(),
        _ => node.children().map_or_else(
            || node.to_string(),
            |children| children.iter().map(visible_node_text).collect(),
        ),
    }
}

fn bare_email_label(value: &str) -> bool {
    value.contains('@') && !value.contains(':') && !value.chars().any(char::is_whitespace)
}

fn label_represents_destination(label: &str, destination: &str) -> bool {
    (label == destination && url_shaped_label(label))
        || (bare_email_label(label)
            && destination
                .strip_prefix("mailto:")
                .is_some_and(|address| address == label))
}

fn inert_equivalent_label_source(
    source: &str,
    node: &Node,
    definitions: &BTreeMap<String, String>,
) -> Option<String> {
    match node {
        Node::Text(text) => Some(inert_text_source(&text.value)),
        Node::Link(_)
        | Node::LinkReference(_)
        | Node::Image(_)
        | Node::ImageReference(_)
        | Node::Html(_) => sanitized_node_source(source, node, definitions),
        _ => {
            let position = node.position()?;
            let original = source.get(position.start.offset..position.end.offset)?;
            let Some(children) = node.children() else {
                return Some(original.to_owned());
            };
            let mut rendered = String::with_capacity(original.len());
            let mut cursor = position.start.offset;
            for child in children {
                let child_position = child.position()?;
                if child_position.start.offset < cursor
                    || child_position.end.offset > position.end.offset
                {
                    return None;
                }
                rendered.push_str(source.get(cursor..child_position.start.offset)?);
                rendered.push_str(&inert_equivalent_label_source(source, child, definitions)?);
                cursor = child_position.end.offset;
            }
            rendered.push_str(source.get(cursor..position.end.offset)?);
            Some(rendered)
        }
    }
}

fn child_discloses_destination(
    node: &Node,
    destination: &str,
    definitions: &BTreeMap<String, String>,
) -> bool {
    match node {
        Node::Image(image) => image.url == destination,
        Node::ImageReference(image) => definitions
            .get(&image.identifier)
            .is_some_and(|image_destination| image_destination == destination),
        _ => node.children().is_some_and(|children| {
            children
                .iter()
                .any(|child| child_discloses_destination(child, destination, definitions))
        }),
    }
}

fn sanitized_image_source(alt: &str, destination: &str) -> String {
    if label_represents_destination(alt, destination) {
        inert_text_source(alt)
    } else {
        format!(
            "{} ({})",
            inert_text_source(alt),
            inert_text_source(destination)
        )
    }
}

fn sanitized_link_source(
    source: &str,
    children: &[Node],
    destination: &str,
    definitions: &BTreeMap<String, String>,
) -> Option<String> {
    let visible_label = children.iter().map(visible_node_text).collect::<String>();
    let represents_destination = label_represents_destination(&visible_label, destination);
    let mut rendered = String::new();
    for child in children {
        let child_source = if represents_destination {
            inert_equivalent_label_source(source, child, definitions)?
        } else {
            sanitized_node_source(source, child, definitions)?
        };
        rendered.push_str(&child_source);
    }
    if !represents_destination
        && !children
            .iter()
            .any(|child| child_discloses_destination(child, destination, definitions))
    {
        rendered.push_str(" (");
        rendered.push_str(&inert_text_source(destination));
        rendered.push(')');
    }
    Some(rendered)
}

fn sanitized_node_source(
    source: &str,
    node: &Node,
    definitions: &BTreeMap<String, String>,
) -> Option<String> {
    match node {
        Node::Link(link) => sanitized_link_source(source, &link.children, &link.url, definitions),
        Node::LinkReference(reference) => {
            let destination = definitions.get(&reference.identifier)?;
            sanitized_link_source(source, &reference.children, destination, definitions)
        }
        Node::Image(image) => Some(sanitized_image_source(&image.alt, &image.url)),
        Node::ImageReference(image) => {
            let destination = definitions.get(&image.identifier)?;
            Some(sanitized_image_source(&image.alt, destination))
        }
        Node::Html(html) => Some(inert_code_span(&html.value)),
        _ => {
            let position = node.position()?;
            let original = source.get(position.start.offset..position.end.offset)?;
            let Some(children) = node.children() else {
                return Some(original.to_owned());
            };
            let mut rendered = String::with_capacity(original.len());
            let mut cursor = position.start.offset;
            for child in children {
                let child_position = child.position()?;
                if child_position.start.offset < cursor
                    || child_position.end.offset > position.end.offset
                {
                    return None;
                }
                rendered.push_str(source.get(cursor..child_position.start.offset)?);
                rendered.push_str(&sanitized_node_source(source, child, definitions)?);
                cursor = child_position.end.offset;
            }
            rendered.push_str(source.get(cursor..position.end.offset)?);
            Some(rendered)
        }
    }
}

fn collect_sanitizer_edits(
    source: &str,
    nodes: &[Node],
    definitions: &BTreeMap<String, String>,
    edits: &mut Vec<ProposalSourceEdit>,
) -> Option<()> {
    for node in nodes {
        let target = matches!(
            node,
            Node::Link(_)
                | Node::LinkReference(_)
                | Node::Image(_)
                | Node::ImageReference(_)
                | Node::Html(_)
        );
        if target {
            let position = node.position()?;
            let replacement = sanitized_node_source(source, node, definitions)?;
            source.get(position.start.offset..position.end.offset)?;
            edits.push(ProposalSourceEdit {
                start: position.start.offset,
                end: position.end.offset,
                replacement,
            });
        } else if let Some(children) = node.children() {
            collect_sanitizer_edits(source, children, definitions, edits)?;
        }
    }
    Some(())
}

fn apply_source_edits(source: &str, mut edits: Vec<ProposalSourceEdit>) -> Option<String> {
    edits.sort_by_key(|edit| (edit.start, edit.end));
    let mut rendered = String::with_capacity(source.len());
    let mut cursor = 0;
    for edit in edits {
        if edit.start < cursor
            || edit.start > edit.end
            || source.get(edit.start..edit.end).is_none()
        {
            return None;
        }
        rendered.push_str(source.get(cursor..edit.start)?);
        rendered.push_str(&edit.replacement);
        cursor = edit.end;
    }
    rendered.push_str(source.get(cursor..)?);
    Some(rendered)
}

fn contains_activatable_markdown(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| {
        matches!(
            node,
            Node::Link(_)
                | Node::LinkReference(_)
                | Node::Image(_)
                | Node::ImageReference(_)
                | Node::Html(_)
        ) || node
            .children()
            .is_some_and(|children| contains_activatable_markdown(children))
    })
}

fn sanitize_render_source(source: &str) -> Result<String, ProposalPreparationFailure> {
    let Node::Root(root) = to_mdast(source, &ParseOptions::gfm())
        .map_err(|_| ProposalPreparationFailure::RawParseFallback)?
    else {
        return Err(ProposalPreparationFailure::RawParseFallback);
    };
    let definitions = collect_effective_definition_urls(&root.children);
    let mut edits = Vec::new();
    collect_sanitizer_edits(source, &root.children, &definitions, &mut edits)
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    let mut rendered =
        apply_source_edits(source, edits).ok_or(ProposalPreparationFailure::RawParseFallback)?;
    let Node::Root(mut rendered_root) = to_mdast(&rendered, &ParseOptions::gfm())
        .map_err(|_| ProposalPreparationFailure::RawParseFallback)?
    else {
        return Err(ProposalPreparationFailure::RawParseFallback);
    };
    if contains_activatable_markdown(&rendered_root.children) {
        let definitions = collect_effective_definition_urls(&rendered_root.children);
        let mut edits = Vec::new();
        collect_sanitizer_edits(&rendered, &rendered_root.children, &definitions, &mut edits)
            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
        rendered = apply_source_edits(&rendered, edits)
            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
        let Node::Root(reparsed_root) = to_mdast(&rendered, &ParseOptions::gfm())
            .map_err(|_| ProposalPreparationFailure::RawParseFallback)?
        else {
            return Err(ProposalPreparationFailure::RawParseFallback);
        };
        rendered_root = reparsed_root;
    }
    if contains_activatable_markdown(&rendered_root.children) {
        return Err(ProposalPreparationFailure::RawParseFallback);
    }
    Ok(rendered)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TableFingerprint {
    alignment: Vec<markdown::mdast::AlignKind>,
    row_cell_counts: Vec<usize>,
}

fn table_fingerprint(table: &markdown::mdast::Table) -> Option<TableFingerprint> {
    let mut row_cell_counts = Vec::with_capacity(table.children.len());
    for row in &table.children {
        let Node::TableRow(row) = row else {
            return None;
        };
        if row
            .children
            .iter()
            .any(|cell| !matches!(cell, Node::TableCell(_)))
        {
            return None;
        }
        row_cell_counts.push(row.children.len());
    }
    Some(TableFingerprint {
        alignment: table.align.clone(),
        row_cell_counts,
    })
}

fn proposal_table_render_width_px(table: &markdown::mdast::Table) -> Option<u16> {
    let mut column_lengths = Vec::new();
    for row in &table.children {
        let Node::TableRow(row) = row else {
            return None;
        };
        for (column, cell) in row.children.iter().enumerate() {
            if !matches!(cell, Node::TableCell(_)) {
                return None;
            }
            if column_lengths.len() <= column {
                column_lengths.resize(column + 1, DEFAULT_TABLE_COLUMN_LENGTH);
            }
            let length = cell.to_string().len().min(MAX_TABLE_COLUMN_LENGTH);
            column_lengths[column] = column_lengths[column]
                .max(length)
                .min(MAX_TABLE_COLUMN_LENGTH);
        }
    }

    let total_length = column_lengths
        .iter()
        .try_fold(0usize, |total, length| total.checked_add(*length))?;
    let width_per_byte = usize::from(APP_TEXT_SIZE.ceil());
    let content_width = total_length.checked_mul(width_per_byte)?;
    let column_chrome = column_lengths.len().checked_mul(TABLE_COLUMN_CHROME_PX)?;
    content_width
        .checked_add(column_chrome)?
        .checked_add(TABLE_OUTER_BORDER_PX)
        .map(|width| width.clamp(MIN_TABLE_RENDER_WIDTH_PX, MAX_TABLE_RENDER_WIDTH_PX))
        .and_then(|width| u16::try_from(width).ok())
}

fn parsed_table_fingerprint(source: &str) -> Option<TableFingerprint> {
    let Node::Root(root) = to_mdast(source, &ParseOptions::gfm()).ok()? else {
        return None;
    };
    let [Node::Table(table)] = root.children.as_slice() else {
        return None;
    };
    table_fingerprint(table)
}

fn source_line_start_offset(source: &str, line: usize) -> Option<usize> {
    if line == 0 {
        return None;
    }
    if line == 1 {
        return Some(0);
    }
    let bytes = source.as_bytes();
    let mut current_line = 1;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' || (bytes[index] == b'\r' && bytes.get(index + 1) != Some(&b'\n'))
        {
            current_line += 1;
            if current_line == line {
                return Some(index + 1);
            }
        }
        index += 1;
    }
    None
}

fn validate_table_start_position(source: &str, position: &markdown::unist::Position) -> bool {
    if position.start.column == 0
        || position.start.offset > position.end.offset
        || source
            .get(position.start.offset..position.end.offset)
            .is_none()
    {
        return false;
    }
    let Some(line_start) = source_line_start_offset(source, position.start.line) else {
        return false;
    };
    let Some(prefix) = source.get(line_start..position.start.offset) else {
        return false;
    };
    let mut column = 1usize;
    for character in prefix.chars() {
        if character == '\t' {
            let remainder = column % 4;
            column = column.saturating_add(if remainder == 0 { 1 } else { 5 - remainder });
        } else {
            column = column.saturating_add(1);
        }
    }
    column == position.start.column
}

fn prefix_bytes_to_table_column(line: &str, target_column: usize) -> Option<usize> {
    let mut visual_column = 0usize;
    let mut byte_offset = 0usize;
    while visual_column < target_column {
        let byte = *line.as_bytes().get(byte_offset)?;
        match byte {
            b' ' => {
                visual_column = visual_column.checked_add(1)?;
                byte_offset += 1;
            }
            b'\t' => {
                let tab_width = 4 - (visual_column % 4);
                visual_column = visual_column.checked_add(tab_width)?;
                if visual_column > target_column {
                    return None;
                }
                byte_offset += 1;
            }
            b'>' => {
                visual_column = visual_column.checked_add(1)?;
                if visual_column > target_column {
                    return None;
                }
                byte_offset += 1;
            }
            _ => return None,
        }
    }
    (visual_column == target_column).then_some(byte_offset)
}

fn split_table_line(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else if let Some(body) = line.strip_suffix('\r') {
        (body, "\r")
    } else {
        (line, "")
    }
}

fn normalize_table_source(
    source: &str,
    position: &markdown::unist::Position,
    raw: &str,
) -> Option<String> {
    if !validate_table_start_position(source, position) {
        return None;
    }
    let target_column = position.start.column.checked_sub(1)?;
    let mut normalized = String::with_capacity(raw.len());
    let mut first_line = true;
    let mut cursor = 0;
    while cursor < raw.len() {
        let remaining = &raw[cursor..];
        let segment_len = remaining
            .find(['\r', '\n'])
            .map_or(remaining.len(), |index| {
                if remaining.as_bytes().get(index) == Some(&b'\r')
                    && remaining.as_bytes().get(index + 1) == Some(&b'\n')
                {
                    index + 2
                } else {
                    index + 1
                }
            });
        let segment = &remaining[..segment_len];
        let (body, ending) = split_table_line(segment);
        if first_line {
            normalized.push_str(segment);
            first_line = false;
        } else {
            let prefix_len = prefix_bytes_to_table_column(body, target_column)?;
            normalized.push_str(body.get(prefix_len..)?);
            normalized.push_str(ending);
        }
        cursor += segment_len;
    }
    (!first_line).then_some(normalized)
}

fn prepare_table_source(
    source: &str,
    table: &markdown::mdast::Table,
) -> Result<(String, TableFingerprint), ProposalPreparationFailure> {
    let fingerprint =
        table_fingerprint(table).ok_or(ProposalPreparationFailure::RawParseFallback)?;
    let position = table
        .position
        .as_ref()
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    if !validate_table_start_position(source, position) {
        return Err(ProposalPreparationFailure::RawParseFallback);
    }
    let raw = proposal_source_slice(source, Some(position))
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    if parsed_table_fingerprint(&raw).is_some_and(|parsed| parsed == fingerprint) {
        return Ok((raw, fingerprint));
    }
    let normalized = normalize_table_source(source, position, &raw)
        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
    if parsed_table_fingerprint(&normalized).is_some_and(|parsed| parsed == fingerprint) {
        Ok((normalized, fingerprint))
    } else {
        Err(ProposalPreparationFailure::RawParseFallback)
    }
}

fn validate_prepared_table_source(
    source: &str,
    fingerprint: &TableFingerprint,
    expected_definition_ids: &[&str],
) -> bool {
    let Ok(Node::Root(root)) = to_mdast(source, &ParseOptions::gfm()) else {
        return false;
    };
    let Some(Node::Table(table)) = root.children.first() else {
        return false;
    };
    if table_fingerprint(table).as_ref() != Some(fingerprint)
        || root.children.len() != expected_definition_ids.len() + 1
    {
        return false;
    }
    root.children[1..]
        .iter()
        .map(|node| match node {
            Node::Definition(definition) => Some(definition.identifier.as_str()),
            _ => None,
        })
        .eq(expected_definition_ids.iter().copied().map(Some))
}

#[cfg(test)]
fn parse_proposal_blocks(source: &str) -> Option<Vec<ProposalBlock>> {
    match prepare_proposal_presentation(source) {
        ProposalPresentation::Prepared(prepared) => Some(prepared.blocks.clone()),
        ProposalPresentation::RawParseFallback(_) | ProposalPresentation::TooComplex => None,
    }
}

fn prepare_proposal_presentation(source: &str) -> ProposalPresentation {
    let prepared = (|| {
        let Node::Root(root) = to_mdast(source, &ParseOptions::gfm())
            .map_err(|_| ProposalPreparationFailure::RawParseFallback)?
        else {
            return Err(ProposalPreparationFailure::RawParseFallback);
        };
        ensure_mdast_node_limit(&root.children)?;
        let definitions = collect_effective_definitions(source, &root.children)
            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
        let mut budget = PreparationBudget::default();
        let mut table_ordinal = 0;
        let blocks = parse_proposal_block_nodes(
            source,
            &root.children,
            &definitions,
            &mut budget,
            &mut table_ordinal,
        )?;
        Ok(PreparedProposal {
            blocks,
            table_count: table_ordinal,
        })
    })();
    match prepared {
        Ok(prepared) => ProposalPresentation::Prepared(Arc::new(prepared)),
        Err(ProposalPreparationFailure::RawParseFallback) => {
            ProposalPresentation::RawParseFallback(inert_raw_fallback_source(source))
        }
        Err(ProposalPreparationFailure::TooComplex) => ProposalPresentation::TooComplex,
    }
}

fn ensure_mdast_node_limit(root_children: &[Node]) -> Result<(), ProposalPreparationFailure> {
    let mut nodes = root_children.iter().collect::<Vec<_>>();
    let mut count = 1usize;
    if count > MAX_MDAST_NODES {
        return Err(ProposalPreparationFailure::TooComplex);
    }
    while let Some(node) = nodes.pop() {
        count = count
            .checked_add(1)
            .ok_or(ProposalPreparationFailure::TooComplex)?;
        if count > MAX_MDAST_NODES {
            return Err(ProposalPreparationFailure::TooComplex);
        }
        if let Some(children) = node.children() {
            nodes.extend(children);
        }
    }
    Ok(())
}

#[derive(Default)]
struct PreparationBudget {
    weighted_complexity: usize,
    prepared_source_bytes: usize,
}

impl PreparationBudget {
    fn account_weight(&mut self, weight: usize) -> Result<(), ProposalPreparationFailure> {
        let next = self
            .weighted_complexity
            .checked_add(weight)
            .ok_or(ProposalPreparationFailure::TooComplex)?;
        if next > MAX_RENDER_COMPLEXITY {
            return Err(ProposalPreparationFailure::TooComplex);
        }
        self.weighted_complexity = next;
        Ok(())
    }

    fn account_markdown(&mut self, source_bytes: usize) -> Result<(), ProposalPreparationFailure> {
        let next_source_bytes = self
            .prepared_source_bytes
            .checked_add(source_bytes)
            .ok_or(ProposalPreparationFailure::TooComplex)?;
        if next_source_bytes > MAX_PREPARED_SOURCE_BYTES {
            return Err(ProposalPreparationFailure::TooComplex);
        }
        self.account_weight(3)?;
        self.prepared_source_bytes = next_source_bytes;
        Ok(())
    }

    fn account_list_item(&mut self) -> Result<(), ProposalPreparationFailure> {
        self.account_weight(1)
    }

    fn account_blockquote(&mut self) -> Result<(), ProposalPreparationFailure> {
        self.account_weight(1)
    }
}

fn referenced_definitions<'a>(
    references: &BTreeSet<String>,
    definitions: &'a [(String, String)],
) -> Vec<&'a str> {
    definitions
        .iter()
        .filter(|(identifier, _)| references.contains(identifier))
        .map(|(_, source)| source.as_str())
        .collect()
}

fn prepared_markdown_source_len(
    source_len: usize,
    referenced: &[&str],
) -> Result<usize, ProposalPreparationFailure> {
    if referenced.is_empty() {
        return Ok(source_len);
    }
    let appendix_len = referenced.iter().try_fold(0usize, |length, source| {
        length
            .checked_add(source.len())
            .ok_or(ProposalPreparationFailure::TooComplex)
    })?;
    let separators = referenced
        .len()
        .checked_sub(1)
        .ok_or(ProposalPreparationFailure::TooComplex)?;
    source_len
        .checked_add(2)
        .and_then(|length| length.checked_add(appendix_len))
        .and_then(|length| length.checked_add(separators))
        .ok_or(ProposalPreparationFailure::TooComplex)
}

fn collect_effective_definitions(source: &str, nodes: &[Node]) -> Option<Vec<(String, String)>> {
    let mut definitions = Vec::new();
    let mut seen = BTreeSet::new();
    collect_definition_nodes(source, nodes, &mut seen, &mut definitions)?;
    Some(definitions)
}

fn collect_effective_definition_urls(nodes: &[Node]) -> BTreeMap<String, String> {
    let mut definitions = BTreeMap::new();
    let mut pending = nodes.iter().rev().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if let Node::Definition(definition) = node {
            definitions
                .entry(definition.identifier.clone())
                .or_insert_with(|| definition.url.clone());
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().rev());
        }
    }
    definitions
}

fn collect_definition_nodes(
    source: &str,
    nodes: &[Node],
    seen: &mut BTreeSet<String>,
    definitions: &mut Vec<(String, String)>,
) -> Option<()> {
    let mut pending = nodes.iter().rev().collect::<Vec<_>>();
    while let Some(node) = pending.pop() {
        if let Node::Definition(definition) = node {
            let source_slice = proposal_source_slice(source, definition.position.as_ref())?;
            if seen.insert(definition.identifier.clone()) {
                definitions.push((definition.identifier.clone(), source_slice));
            }
        }
        if let Some(children) = node.children() {
            pending.extend(children.iter().rev());
        }
    }
    Some(())
}

fn parse_proposal_block_nodes(
    source: &str,
    nodes: &[Node],
    definitions: &[(String, String)],
    budget: &mut PreparationBudget,
    table_ordinal: &mut usize,
) -> Result<Vec<ProposalBlock>, ProposalPreparationFailure> {
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < nodes.len() {
        let block = match &nodes[index] {
            Node::Definition(definition) => {
                proposal_source_slice(source, definition.position.as_ref())
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                index += 1;
                continue;
            }
            Node::List(list) => {
                // Validate the list range even though its children provide the rendered text.
                proposal_source_slice(source, list.position.as_ref())
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let items = list
                    .children
                    .iter()
                    .map(|child| {
                        let Node::ListItem(item) = child else {
                            return Err(ProposalPreparationFailure::RawParseFallback);
                        };
                        proposal_source_slice(source, item.position.as_ref())
                            .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                        budget.account_list_item()?;
                        Ok(ProposalListItem {
                            checked: item.checked,
                            blocks: parse_proposal_block_nodes(
                                source,
                                &item.children,
                                definitions,
                                budget,
                                table_ordinal,
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                ProposalBlock::List(ProposalList {
                    ordered: list.ordered,
                    start: list.start,
                    items,
                })
            }
            Node::Blockquote(blockquote) => {
                proposal_source_slice(source, blockquote.position.as_ref())
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                budget.account_blockquote()?;
                ProposalBlock::Blockquote(ProposalBlockquote {
                    blocks: parse_proposal_block_nodes(
                        source,
                        &blockquote.children,
                        definitions,
                        budget,
                        table_ordinal,
                    )?,
                })
            }
            Node::Table(table) => {
                let (source_slice, fingerprint) = prepare_table_source(source, table)?;
                let render_width_px = proposal_table_render_width_px(table)
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let mut references = BTreeSet::new();
                collect_reference_ids(&Node::Table(table.clone()), &mut references);
                let referenced = referenced_definitions(&references, definitions);
                let expected_definition_ids = definitions
                    .iter()
                    .filter(|(identifier, _)| references.contains(identifier))
                    .map(|(identifier, _)| identifier.as_str())
                    .collect::<Vec<_>>();
                let _ = prepared_markdown_source_len(source_slice.len(), &referenced)?;
                let source = if referenced.is_empty() {
                    source_slice
                } else {
                    format!("{source_slice}\n\n{}", referenced.join("\n"))
                };
                if !validate_prepared_table_source(&source, &fingerprint, &expected_definition_ids)
                {
                    return Err(ProposalPreparationFailure::RawParseFallback);
                }
                let render_source = sanitize_render_source(&source)?;
                if !validate_prepared_table_source(
                    &render_source,
                    &fingerprint,
                    &expected_definition_ids,
                ) {
                    return Err(ProposalPreparationFailure::RawParseFallback);
                }
                budget.account_markdown(render_source.len())?;
                let ordinal = *table_ordinal;
                *table_ordinal = (*table_ordinal)
                    .checked_add(1)
                    .ok_or(ProposalPreparationFailure::TooComplex)?;
                ProposalBlock::Table(ProposalTable {
                    source,
                    render_source,
                    ordinal,
                    render_width_px,
                })
            }
            _ => {
                let start = nodes[index]
                    .position()
                    .and_then(|position| {
                        source
                            .get(position.start.offset..position.start.offset)
                            .map(|_| position.start.offset)
                    })
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let mut end = start;
                let mut references = BTreeSet::new();
                let mut next = index;
                while next < nodes.len()
                    && !matches!(
                        nodes[next],
                        Node::Definition(_) | Node::Table(_) | Node::List(_) | Node::Blockquote(_)
                    )
                {
                    let position = nodes[next]
                        .position()
                        .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                    end = position.end.offset;
                    collect_reference_ids(&nodes[next], &mut references);
                    next += 1;
                }
                let source_slice = source
                    .get(start..end)
                    .map(str::to_owned)
                    .ok_or(ProposalPreparationFailure::RawParseFallback)?;
                let referenced = referenced_definitions(&references, definitions);
                let _ = prepared_markdown_source_len(source_slice.len(), &referenced)?;
                let source_slice = if referenced.is_empty() {
                    source_slice
                } else {
                    format!("{source_slice}\n\n{}", referenced.join("\n"))
                };
                let source_slice = sanitize_render_source(&source_slice)?;
                budget.account_markdown(source_slice.len())?;
                index = next;
                ProposalBlock::Markdown(source_slice)
            }
        };
        if matches!(
            &block,
            ProposalBlock::Table(_) | ProposalBlock::List(_) | ProposalBlock::Blockquote(_)
        ) {
            index += 1;
        }
        blocks.push(block);
    }
    Ok(blocks)
}

fn collect_reference_ids(node: &Node, references: &mut BTreeSet<String>) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node {
            Node::LinkReference(reference) => {
                references.insert(reference.identifier.clone());
            }
            Node::ImageReference(reference) => {
                references.insert(reference.identifier.clone());
            }
            _ => {}
        }
        if let Some(children) = node.children() {
            pending.extend(children);
        }
    }
}

fn render_proposal_blocks(
    blocks: &[ProposalBlock],
    description_id: &SharedString,
    path: &str,
    identity: &ProposalIdentity,
    table_scroll_handles: &BTreeMap<String, ScrollHandle>,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) -> gpui::AnyElement {
    let mut container = div().w_full().min_w(px(0.0)).flex().flex_col().gap_2();
    for (index, block) in blocks.iter().enumerate() {
        let block_path = format!("{path}-{index}");
        let element = match block {
            ProposalBlock::Markdown(source) => TextView::markdown(
                SharedString::from(format!("{description_id}-text-{block_path}")),
                source.clone(),
                window,
                cx,
            )
            .selectable(true)
            .into_any_element(),
            ProposalBlock::Table(table) => {
                let scroll = table_scroll_handles
                    .get(&proposal_table_scroll_key(identity, table.ordinal))
                    .cloned()
                    .unwrap_or_else(ScrollHandle::new);
                let table_id = format!(
                    "{description_id}-table-{}-{}",
                    identity.contract_address, table.ordinal
                );
                let text = TextView::markdown(
                    SharedString::from(format!("{table_id}-text")),
                    table.render_source.clone(),
                    window,
                    cx,
                )
                .w(px(f32::from(table.render_width_px)))
                .min_w_full()
                .flex_none()
                .selectable(true);
                let mut table_scroller = div()
                    .id(SharedString::from(format!("{table_id}-scroll")))
                    .w_full()
                    .min_w(px(0.0))
                    .track_scroll(&scroll)
                    .overflow_x_scroll()
                    .horizontal_scrollbar(&scroll)
                    .relative();
                table_scroller.style().restrict_scroll_to_axis = Some(true);
                div()
                    .id(SharedString::from(format!("{table_id}-wrapper")))
                    .w_full()
                    .min_w(px(0.0))
                    .child(table_scroller.child(text))
                    .into_any_element()
            }
            ProposalBlock::List(list) => render_proposal_list(
                list,
                description_id,
                &block_path,
                identity,
                table_scroll_handles,
                window,
                cx,
            ),
            ProposalBlock::Blockquote(blockquote) => div()
                .id(SharedString::from(format!(
                    "{description_id}-blockquote-{block_path}"
                )))
                .w_full()
                .min_w(px(0.0))
                .pl(px(12.0))
                .border_l_2()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .child(render_proposal_blocks(
                    &blockquote.blocks,
                    description_id,
                    &block_path,
                    identity,
                    table_scroll_handles,
                    window,
                    cx,
                ))
                .into_any_element(),
        };
        container = container.child(element);
    }
    container.into_any_element()
}

fn render_proposal_list(
    list: &ProposalList,
    description_id: &SharedString,
    path: &str,
    identity: &ProposalIdentity,
    table_scroll_handles: &BTreeMap<String, ScrollHandle>,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) -> gpui::AnyElement {
    let mut container = div().w_full().min_w(px(0.0)).flex().flex_col().gap_2();
    let start = list.start.unwrap_or(1);
    for (index, item) in list.items.iter().enumerate() {
        let marker = if let Some(checked) = item.checked {
            if checked { "[x]" } else { "[ ]" }.to_owned()
        } else if list.ordered {
            format!("{}.", start.saturating_add(index as u32))
        } else {
            "•".to_owned()
        };
        let item_path = format!("{path}-{index}");
        let body = render_proposal_blocks(
            &item.blocks,
            description_id,
            &item_path,
            identity,
            table_scroll_handles,
            window,
            cx,
        );
        container = container.child(
            div()
                .id(SharedString::from(format!(
                    "{description_id}-list-row-{item_path}"
                )))
                .w_full()
                .min_w(px(0.0))
                .flex()
                .items_start()
                .child(
                    div()
                        .w(px(32.0))
                        .flex_none()
                        .child(app_muted_text(marker).text_size(px(14.0))),
                )
                .child(div().flex_1().min_w(px(0.0)).child(body)),
        );
    }
    container.into_any_element()
}

fn render_proposal_detail_tabs(
    root: &Entity<WalletRoot>,
    selected: ProposalDetailTab,
    action_count: usize,
    identity: &ProposalIdentity,
) -> impl IntoElement {
    let selected_index = match selected {
        ProposalDetailTab::Description => 0,
        ProposalDetailTab::Actions => 1,
    };
    let tab_root = root.clone();
    TabBar::new(SharedString::from(format!(
        "wallet-proposal-detail-tabs-{}-{}-{}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    )))
    .underline()
    .w_full()
    .flex_none()
    .selected_index(selected_index)
    .on_click(move |index, _window, cx| {
        let tab = match *index {
            0 => ProposalDetailTab::Description,
            1 => ProposalDetailTab::Actions,
            _ => return,
        };
        tab_root.update(cx, |root, cx| root.select_proposal_detail_tab(tab, cx));
    })
    .children([
        Tab::new().min_w(px(112.0)).label("Description"),
        Tab::new()
            .min_w(px(112.0))
            .label(format!("Actions ({action_count})")),
    ])
}

fn action_calldata_hex(calldata: &alloy::primitives::Bytes) -> String {
    format!("0x{}", alloy::hex::encode(calldata))
}

fn compact_calldata_display(value: &str) -> String {
    if value.chars().count() <= 42 {
        return value.to_owned();
    }
    let prefix = value.chars().take(20).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(16)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn proposal_action_id(
    identity: &ProposalIdentity,
    ordinal: usize,
    field: &str,
    control: &str,
) -> SharedString {
    SharedString::from(format!(
        "wallet-proposal-action-{}-{}-{}-{ordinal}-{field}-{control}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    ))
}

fn proposal_known_address_label(
    chain_id: u64,
    address: Address,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
) -> Option<String> {
    if let Some(metadata) = token_display_metadata(Some(token_registry), chain_id, &address) {
        return Some(metadata.symbol);
    }
    if let Some(contracts) = railgun_ui::governance_contracts(chain_id) {
        if address == contracts.governance_token {
            return Some("Governance token".to_owned());
        }
        if address == contracts.delegator {
            return Some("Delegator".to_owned());
        }
        if address == contracts.voting {
            return Some("Voting".to_owned());
        }
        if contracts.voting_legacy == Some(address) {
            return Some("Legacy voting".to_owned());
        }
        if address == contracts.staking {
            return Some("Staking".to_owned());
        }
        if address == contracts.governor_rewards {
            return Some("Governor rewards".to_owned());
        }
        if let Some(reward) = contracts
            .reward_tokens
            .iter()
            .find(|reward| reward.token == address)
        {
            return Some(reward.symbol.to_owned());
        }
    }
    if railgun_ui::governance_treasury(chain_id) == Some(address) {
        return Some("Treasury".to_owned());
    }
    let effective_chain = effective_chain?;
    let configured = [
        ("RAILGUN", effective_chain.railgun_contract.as_str()),
        ("Relay Adapt", effective_chain.relay_adapt_contract.as_str()),
        (
            "Relay Adapt 7702",
            effective_chain.relay_adapt_7702_contract.as_str(),
        ),
        ("Multicall", effective_chain.multicall_contract.as_str()),
    ];
    configured
        .into_iter()
        .find_map(|(label, raw)| {
            (raw.parse::<Address>().ok() == Some(address)).then_some(label.to_owned())
        })
        .or_else(|| {
            effective_chain
                .wrapped_native_token
                .as_deref()
                .and_then(|raw| raw.parse::<Address>().ok())
                .filter(|wrapped| *wrapped == address)
                .map(|_| "Wrapped native token".to_owned())
        })
        .or_else(|| {
            effective_chain
                .coinbase_payer
                .filter(|payer| *payer == address)
                .map(|_| "Coinbase payer".to_owned())
        })
}

fn proposal_action_target_label(
    chain_id: u64,
    target: Address,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    decoded: Option<&DecodedProposalAction>,
) -> Option<String> {
    proposal_known_address_label(chain_id, target, token_registry, effective_chain).or_else(|| {
        decoded
            .and_then(DecodedProposalAction::contract_family_label)
            .map(str::to_owned)
    })
}

fn proposal_role_label(role: B256) -> String {
    if role == B256::ZERO {
        "DEFAULT_ADMIN_ROLE".to_owned()
    } else if role == alloy::primitives::keccak256(b"TRANSFER_ROLE") {
        "TRANSFER_ROLE".to_owned()
    } else {
        role.to_string()
    }
}

fn proposal_basis_points_label(basis_points: U256) -> String {
    let whole_percent = basis_points / U256::from(100);
    let fractional_percent = (basis_points % U256::from(100)).to::<u8>();
    format!("{whole_percent}.{fractional_percent:02}% ({basis_points} bp)")
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

fn proposal_party_badge_label(
    chain_id: u64,
    address: Address,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    public_accounts: &[PublicAccountMetadata],
    public_address_book: Option<&[PublicAddressBookEntry]>,
) -> Option<String> {
    proposal_known_address_label(chain_id, address, token_registry, effective_chain)
        .or_else(|| {
            public_accounts
                .iter()
                .find(|account| account.address == address)
                .map(|account| {
                    account
                        .label
                        .as_deref()
                        .filter(|label| !label.trim().is_empty())
                        .map_or_else(
                            || "Your account".to_owned(),
                            |label| format!("Your account · {label}"),
                        )
                })
        })
        .or_else(|| {
            public_address_book
                .and_then(|entries| entries.iter().find(|entry| entry.address == address))
                .map(|entry| entry.label.clone())
        })
}

fn proposal_party_detail_with_context(
    role: &str,
    chain_id: u64,
    address: Address,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    public_accounts: &[PublicAccountMetadata],
    public_address_book: Option<&[PublicAddressBookEntry]>,
) -> ProposalDecodedDetail {
    ProposalDecodedDetail::Party {
        role: role.to_owned(),
        address,
        copy_value: address.to_checksum(None),
        badge: proposal_party_badge_label(
            chain_id,
            address,
            token_registry,
            effective_chain,
            public_accounts,
            public_address_book,
        ),
    }
}

fn proposal_address_badge(label: impl Into<SharedString>) -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
        .px(px(5.0))
        .py(px(2.0))
        .text_size(px(10.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .whitespace_normal()
        .child(label.into())
}

fn proposal_delegator_selector_label(selector: FixedBytes<4>) -> String {
    let raw = format!("0x{}", alloy::hex::encode(selector.as_slice()));
    if selector == FixedBytes::from([0; 4]) {
        format!("Any function · {raw}")
    } else if selector == FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]) {
        format!("setVerificationKey · {raw}")
    } else {
        raw
    }
}

fn proposal_decoded_preview(
    decoded: &DecodedProposalAction,
    target: Address,
    action_value: U256,
    chain_id: u64,
    anchor_rates: &TokenAnchorRateCache,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    public_accounts: &[PublicAccountMetadata],
    public_address_book: Option<&[PublicAddressBookEntry]>,
) -> ProposalDecodedPreview {
    let party = |role: &str, address: Address| {
        proposal_party_detail_with_context(
            role,
            chain_id,
            address,
            token_registry,
            effective_chain,
            public_accounts,
            public_address_book,
        )
    };
    let combine_context = |usd_context: Option<String>, semantic_context: Option<String>| match (
        usd_context,
        semantic_context,
    ) {
        (Some(usd), Some(context)) => Some(format!("{usd} · {context}")),
        (Some(usd), None) | (None, Some(usd)) => Some(usd),
        (None, None) => None,
    };
    let token_hero = |token: Address, amount: U256, context: Option<String>| {
        if let Some(metadata) = token_display_metadata(Some(token_registry), chain_id, &token) {
            let usd_context = anchor_rates
                .cached_token_usd_micro_value(chain_id, token, amount)
                .and_then(|usd| {
                    railgun_ui::non_redundant_usd_micro_value(amount, metadata.decimals, usd)
                })
                .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)));
            ProposalDecodedHero {
                rounded: format_token_amount_for_display(
                    chain_id,
                    token,
                    amount,
                    Some(token_registry),
                ),
                icon: metadata.icon_path,
                context: combine_context(usd_context, context),
                amount_copy_value: None,
                context_copy_value: None,
                monospace: false,
                danger: false,
            }
        } else {
            ProposalDecodedHero {
                rounded: format!("{amount} raw token units"),
                icon: None,
                context: Some(format!(
                    "Token not in registry · {}",
                    railgun_ui::short_address(&token)
                )),
                amount_copy_value: Some(amount.to_string()),
                context_copy_value: Some(token.to_checksum(None)),
                monospace: true,
                danger: false,
            }
        }
    };
    let native_hero = |amount: U256, context: Option<String>| {
        let usd_context = anchor_rates
            .cached_native_usd_micro_value(chain_id, amount)
            .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)));
        ProposalDecodedHero {
            rounded: format_native_token_amount_for_display(chain_id, amount),
            icon: railgun_ui::chain_icon_asset_path(chain_id).map(WalletIconSource::embedded),
            context: combine_context(usd_context, context),
            amount_copy_value: None,
            context_copy_value: None,
            monospace: false,
            danger: false,
        }
    };
    match decoded {
        DecodedProposalAction::Erc20Transfer { recipient, amount } => ProposalDecodedPreview {
            verb: "Send".to_owned(),
            hero: Some(token_hero(target, *amount, None)),
            details: vec![party("TO", *recipient)],
            unlimited_warning: None,
        },
        DecodedProposalAction::Erc20TransferFrom { from, to, amount } => ProposalDecodedPreview {
            verb: "Send".to_owned(),
            hero: Some(token_hero(target, *amount, None)),
            details: vec![
                party("FROM", *from),
                ProposalDecodedDetail::Connector,
                party("TO", *to),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::Erc20Approve { spender, amount } => {
            let unlimited = *amount == U256::MAX;
            let mut hero = token_hero(target, *amount, Some("allowance".to_owned()));
            if unlimited {
                hero.rounded = token_display_metadata(Some(token_registry), chain_id, &target)
                    .map_or_else(
                        || "Unlimited raw token units".to_owned(),
                        |metadata| format!("Unlimited {}", metadata.symbol),
                    );
                hero.danger = true;
            }
            ProposalDecodedPreview {
                verb: "Allow spending".to_owned(),
                hero: Some(hero),
                details: vec![party("SPENDER", *spender)],
                unlimited_warning: unlimited.then(|| {
                    format!(
                        "Unlimited allowance: {} can keep spending this token until the allowance is revoked.",
                        spender.to_checksum(None)
                    )
                }),
            }
        }
        DecodedProposalAction::TreasuryTransferErc20 { token, to, amount } => {
            ProposalDecodedPreview {
                verb: "Send from treasury".to_owned(),
                hero: Some(token_hero(*token, *amount, None)),
                details: vec![
                    party("FROM", target),
                    ProposalDecodedDetail::Connector,
                    party("TO", *to),
                ],
                unlimited_warning: None,
            }
        }
        DecodedProposalAction::TreasuryTransferEth { to, amount } => ProposalDecodedPreview {
            verb: "Send from treasury".to_owned(),
            hero: Some(native_hero(*amount, None)),
            details: vec![
                party("FROM", target),
                ProposalDecodedDetail::Connector,
                party("TO", *to),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::TreasuryInitialize { owner } => ProposalDecodedPreview {
            verb: "Initialize treasury".to_owned(),
            hero: None,
            details: vec![party("OWNER", *owner)],
            unlimited_warning: None,
        },
        DecodedProposalAction::TreasuryGrantRole { role, account } => ProposalDecodedPreview {
            verb: "Grant role".to_owned(),
            hero: None,
            details: vec![
                ProposalDecodedDetail::Value {
                    role: "ROLE".to_owned(),
                    value: proposal_role_label(*role),
                    copy_value: None,
                    monospace: true,
                },
                party("ACCOUNT", *account),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::TreasuryRevokeRole { role, account } => ProposalDecodedPreview {
            verb: "Revoke role".to_owned(),
            hero: None,
            details: vec![
                ProposalDecodedDetail::Value {
                    role: "ROLE".to_owned(),
                    value: proposal_role_label(*role),
                    copy_value: None,
                    monospace: true,
                },
                party("ACCOUNT", *account),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::TreasuryRenounceRole { role, account } => ProposalDecodedPreview {
            verb: "Renounce role".to_owned(),
            hero: None,
            details: vec![
                ProposalDecodedDetail::Value {
                    role: "ROLE".to_owned(),
                    value: proposal_role_label(*role),
                    copy_value: None,
                    monospace: true,
                },
                party("ACCOUNT", *account),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::OpStackReadyTask { task_id } => ProposalDecodedPreview {
            verb: "Ready task".to_owned(),
            hero: None,
            details: vec![ProposalDecodedDetail::Value {
                role: "TASK ID".to_owned(),
                value: task_id.to_string(),
                copy_value: None,
                monospace: true,
            }],
            unlimited_warning: None,
        },
        DecodedProposalAction::OpStackSetExecutorL2 { executor } => ProposalDecodedPreview {
            verb: "Set executor".to_owned(),
            hero: None,
            details: vec![party("EXECUTOR", *executor)],
            unlimited_warning: None,
        },
        DecodedProposalAction::TransferOwnership { new_owner } => ProposalDecodedPreview {
            verb: "Transfer ownership".to_owned(),
            hero: None,
            details: vec![party("NEW OWNER", *new_owner)],
            unlimited_warning: None,
        },
        DecodedProposalAction::WrappedDeposit => ProposalDecodedPreview {
            verb: "Wrap".to_owned(),
            hero: Some(native_hero(
                action_value,
                native_wrapped_output_labels(chain_id)
                    .map(|(_, wrapped)| format!("wrapped into {wrapped}")),
            )),
            details: Vec::new(),
            unlimited_warning: None,
        },
        DecodedProposalAction::WrappedWithdraw { amount } => ProposalDecodedPreview {
            verb: "Unwrap".to_owned(),
            hero: Some(token_hero(
                target,
                *amount,
                native_wrapped_output_labels(chain_id)
                    .map(|(native, _)| format!("unwrapped into {native}")),
            )),
            details: Vec::new(),
            unlimited_warning: None,
        },
        DecodedProposalAction::ProxyUpgrade {
            proxy,
            implementation,
        } => ProposalDecodedPreview {
            verb: "Upgrade proxy".to_owned(),
            hero: None,
            details: vec![
                party("PROXY", *proxy),
                party("IMPLEMENTATION", *implementation),
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::ProxyPause { proxy } => ProposalDecodedPreview {
            verb: "Pause proxy".to_owned(),
            hero: None,
            details: vec![party("PROXY", *proxy)],
            unlimited_warning: None,
        },
        DecodedProposalAction::ProxyTransferOwnership { proxy, new_owner } => {
            ProposalDecodedPreview {
                verb: "Transfer proxy ownership".to_owned(),
                hero: None,
                details: vec![party("PROXY", *proxy), party("NEW OWNER", *new_owner)],
                unlimited_warning: None,
            }
        }
        DecodedProposalAction::ChangeFee {
            shield_fee,
            unshield_fee,
            nft_fee,
        } => ProposalDecodedPreview {
            verb: "Change protocol fees".to_owned(),
            hero: None,
            details: vec![
                ProposalDecodedDetail::Value {
                    role: "SHIELD FEE".to_owned(),
                    value: proposal_basis_points_label(*shield_fee),
                    copy_value: Some(shield_fee.to_string()),
                    monospace: false,
                },
                ProposalDecodedDetail::Value {
                    role: "UNSHIELD FEE".to_owned(),
                    value: proposal_basis_points_label(*unshield_fee),
                    copy_value: Some(unshield_fee.to_string()),
                    monospace: false,
                },
                ProposalDecodedDetail::Value {
                    role: "NFT FEE".to_owned(),
                    value: format!(
                        "{} ({} wei)",
                        format_native_token_amount_for_display(chain_id, *nft_fee),
                        nft_fee
                    ),
                    copy_value: Some(nft_fee.to_string()),
                    monospace: false,
                },
            ],
            unlimited_warning: None,
        },
        DecodedProposalAction::GovernanceMint { account, amount } => ProposalDecodedPreview {
            verb: "Mint governance tokens".to_owned(),
            hero: Some(token_hero(target, *amount, None)),
            details: vec![party("TO", *account)],
            unlimited_warning: None,
        },
        DecodedProposalAction::SetIntervalBP { new_interval_bp } => ProposalDecodedPreview {
            verb: "Set reward interval rate".to_owned(),
            hero: None,
            details: vec![ProposalDecodedDetail::Value {
                role: "RATE".to_owned(),
                value: proposal_basis_points_label(*new_interval_bp),
                copy_value: Some(new_interval_bp.to_string()),
                monospace: false,
            }],
            unlimited_warning: None,
        },
        DecodedProposalAction::AddTokens { tokens } => ProposalDecodedPreview {
            verb: "Add reward tokens".to_owned(),
            hero: None,
            details: tokens.iter().map(|token| party("TOKEN", *token)).collect(),
            unlimited_warning: None,
        },
        DecodedProposalAction::DelegatorSetPermission {
            caller,
            contract_address,
            selector,
            permission,
        } => ProposalDecodedPreview {
            verb: if *permission {
                "Grant call permission".to_owned()
            } else {
                "Revoke call permission".to_owned()
            },
            hero: None,
            details: vec![
                party("CALLER", *caller),
                if *contract_address == Address::ZERO {
                    ProposalDecodedDetail::Value {
                        role: "CONTRACT".to_owned(),
                        value: format!("Any contract · {}", short_address(contract_address)),
                        copy_value: Some(contract_address.to_checksum(None)),
                        monospace: false,
                    }
                } else {
                    party("CONTRACT", *contract_address)
                },
                ProposalDecodedDetail::Value {
                    role: "FUNCTION".to_owned(),
                    value: proposal_delegator_selector_label(*selector),
                    copy_value: Some(format!("0x{}", alloy::hex::encode(selector.as_slice()))),
                    monospace: true,
                },
            ],
            unlimited_warning: None,
        },
    }
}

fn render_proposal_decoded_preview(
    preview: &ProposalDecodedPreview,
    identity: &ProposalIdentity,
    ordinal: usize,
) -> gpui::Div {
    let mut inset = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_2()
        .p(px(10.0))
        .rounded_sm()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap_2()
                .child(
                    app_strong_text(preview.verb.clone())
                        .text_size(px(12.0))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(div().flex_1().min_w(px(0.0)))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .child(
                            Icon::new(RailgunActionIcon::Sparkles)
                                .with_size(px(13.0))
                                .text_color(rgb(theme::PRIMARY)),
                        )
                        .child(app_muted_text("DECODED").text_size(px(10.0))),
                ),
        );

    if let Some(hero) = &preview.hero {
        let mut amount = app_strong_text(hero.rounded.clone()).text_size(px(17.0));
        if hero.monospace {
            amount = amount.font_family(APP_MONO_FONT_FAMILY);
        }
        if hero.danger {
            amount = amount.text_color(rgb(theme::DANGER));
        }
        let mut amount_group = div()
            .min_w(px(0.0))
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(amount.whitespace_normal());
        if let Some(copy_value) = &hero.amount_copy_value {
            amount_group = amount_group.child(
                div()
                    .id(proposal_action_id(
                        identity,
                        ordinal,
                        "decoded-hero-raw-amount",
                        "action",
                    ))
                    .flex_none()
                    .tooltip(|window, cx| Tooltip::new("Copy raw amount").build(window, cx))
                    .child(clipboard_with_toast(
                        proposal_action_id(
                            identity,
                            ordinal,
                            "decoded-hero-raw-amount",
                            "clipboard",
                        ),
                        copy_value.clone(),
                    )),
            );
        }
        let mut body = div()
            .min_w(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .gap_1()
            .child(amount_group);
        if let Some(context) = &hero.context {
            let mut context_group = div()
                .min_w(px(0.0))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(app_muted_text(context.clone()).text_size(px(11.0)));
            if let Some(copy_value) = &hero.context_copy_value {
                context_group = context_group.child(
                    div()
                        .id(proposal_action_id(
                            identity,
                            ordinal,
                            "decoded-hero-token-address",
                            "action",
                        ))
                        .flex_none()
                        .tooltip(|window, cx| Tooltip::new("Copy token address").build(window, cx))
                        .child(clipboard_with_toast(
                            proposal_action_id(
                                identity,
                                ordinal,
                                "decoded-hero-token-address",
                                "clipboard",
                            ),
                            copy_value.clone(),
                        )),
                );
            }
            body = body.child(context_group);
        }
        let mut hero_row = div().w_full().min_w(px(0.0)).flex().items_center().gap_3();
        if let Some(icon) = hero.icon.clone() {
            hero_row = hero_row.child(img(icon).size(px(30.0)).rounded_full().flex_none());
        }
        inset = inset.child(hero_row.child(body));
    }

    inset = inset.children(
        preview
            .details
            .iter()
            .enumerate()
            .map(|(detail_index, detail)| match detail {
                ProposalDecodedDetail::Connector => {
                    div().w_full().min_w(px(0.0)).flex().pl(px(66.0)).child(
                        Icon::new(IconName::ArrowDown)
                            .with_size(px(13.0))
                            .text_color(rgb(theme::TEXT_SUBTLE)),
                    )
                }
                ProposalDecodedDetail::Party {
                    role,
                    address,
                    copy_value,
                    badge,
                } => {
                    let role_address_line = div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            app_muted_text(role.clone())
                                .w(px(58.0))
                                .flex_none()
                                .font_family(APP_MONO_FONT_FAMILY)
                                .text_size(px(12.0))
                                .line_height(px(12.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::TEXT_SUBTLE)),
                        )
                        .child(
                            app_text(railgun_ui::short_address(address))
                                .font_family(APP_MONO_FONT_FAMILY)
                                .font_weight(FontWeight::NORMAL)
                                .text_size(px(12.0))
                                .line_height(px(12.0)),
                        );
                    let copy_control = div()
                        .id(proposal_action_id(
                            identity,
                            ordinal,
                            &format!("decoded-party-{detail_index}"),
                            "action",
                        ))
                        .flex_none()
                        .tooltip({
                            let tooltip = format!("Copy {role}");
                            move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx)
                        })
                        .child(clipboard_with_toast(
                            proposal_action_id(
                                identity,
                                ordinal,
                                &format!("decoded-party-{detail_index}"),
                                "clipboard",
                            ),
                            copy_value.clone(),
                        ));
                    let party_line = div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .child(role_address_line)
                        .child(copy_control);
                    let mut row = div()
                        .w_full()
                        .min_w(px(0.0))
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(party_line);
                    if let Some(badge) = badge {
                        row = row.child(proposal_address_badge(badge.clone()));
                    }
                    row
                }
                ProposalDecodedDetail::Value {
                    role,
                    value,
                    copy_value,
                    monospace,
                } => {
                    let mut rendered_value = app_text(value.clone())
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .whitespace_normal();
                    if *monospace {
                        rendered_value = rendered_value.font_family(APP_MONO_FONT_FAMILY);
                    }
                    let mut row = div()
                        .w_full()
                        .min_w(px(0.0))
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            app_muted_text(role.clone())
                                .w(px(58.0))
                                .flex_none()
                                .font_family(APP_MONO_FONT_FAMILY)
                                .text_size(px(12.0))
                                .line_height(px(12.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::TEXT_SUBTLE)),
                        )
                        .child(rendered_value);
                    if let Some(copy_value) = copy_value {
                        row = row.child(clipboard_with_toast(
                            proposal_action_id(
                                identity,
                                ordinal,
                                &format!("decoded-value-{detail_index}"),
                                "clipboard",
                            ),
                            copy_value.clone(),
                        ));
                    }
                    row
                }
            }),
    );
    if let Some(warning) = &preview.unlimited_warning {
        inset = inset.child(
            Alert::error(
                proposal_action_id(identity, ordinal, "decoded-unlimited", "alert"),
                warning.clone(),
            )
            .small(),
        );
    }
    inset
}

fn render_proposal_actions_card(
    root: &Entity<WalletRoot>,
    proposal: &ResolvedProposal,
    chain_id: u64,
    expanded_calldata: &BTreeSet<ProposalActionIdentity>,
    anchor_rates: &TokenAnchorRateCache,
    token_registry: &wallet_ops::settings::EffectiveTokenRegistry,
    effective_chain: Option<&EffectiveChainConfig>,
    public_accounts: &[PublicAccountMetadata],
    public_address_book: Option<&[PublicAddressBookEntry]>,
) -> impl IntoElement {
    let identity = proposal.identity();
    let mut card = div()
        .id(SharedString::from(format!(
            "wallet-proposal-actions-{}-{}-{}",
            proposal_version_label(identity.contract_version),
            identity.contract_address,
            identity.index,
        )))
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_3()
        .p(px(20.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER));
    if proposal.proposal.actions.is_empty() {
        return card.child(app_muted_text("No contract actions"));
    }
    for (ordinal, action) in proposal.proposal.actions.iter().enumerate() {
        let action_identity = ProposalActionIdentity {
            proposal: identity.clone(),
            ordinal,
        };
        let calldata = action_calldata_hex(&action.calldata);
        let expanded = expanded_calldata.contains(&action_identity);
        let compact_calldata = compact_calldata_display(&calldata);
        let address = action.call_contract.to_checksum(None);
        let wrapped_native_token = effective_chain
            .and_then(|chain| chain.wrapped_native_token.as_deref())
            .and_then(|address| address.parse::<Address>().ok());
        let railgun_contract =
            effective_chain.and_then(|chain| chain.railgun_contract.parse::<Address>().ok());
        let decoded = decode_proposal_action(
            chain_id,
            action.call_contract,
            &action.calldata,
            wrapped_native_token,
            railgun_contract,
        );
        let target_label = proposal_action_target_label(
            chain_id,
            action.call_contract,
            token_registry,
            effective_chain,
            decoded.as_ref(),
        );
        let value = action.value;
        let address_copy_id = proposal_action_id(&identity, ordinal, "address", "clipboard");
        let calldata_copy_id = proposal_action_id(&identity, ordinal, "calldata", "clipboard");
        let calldata_toggle_id = proposal_action_id(&identity, ordinal, "calldata", "toggle");
        let calldata_copy_wrapper_id =
            proposal_action_id(&identity, ordinal, "calldata", "copy-wrapper");
        let calldata_toggle_root = root.clone();
        let calldata_toggle_identity = identity.clone();
        let mut action_card = div()
            .id(proposal_action_id(&identity, ordinal, "wrapper", "card"))
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap_2()
            .p(px(12.0))
            .rounded_md()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(
                app_strong_text(match decoded.as_ref() {
                    Some(decoded) => format!("Action {} · {}", ordinal + 1, decoded.method_name()),
                    None => format!("Action {}", ordinal + 1),
                })
                .text_size(px(13.0)),
            )
            .child(app_muted_text("Target").text_size(px(11.0)))
            .child({
                let mut target_row = div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .max_w_full()
                            .min_w(px(0.0))
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                app_text(address.clone())
                                    .max_w_full()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(12.0)),
                            )
                            .child(
                                div()
                                    .id(proposal_action_id(&identity, ordinal, "address", "action"))
                                    .flex_none()
                                    .tooltip(|window, cx| {
                                        Tooltip::new("Copy target").build(window, cx)
                                    })
                                    .child(clipboard_with_toast(address_copy_id, address.clone())),
                            ),
                    );
                if let Some(label) = target_label {
                    target_row = target_row.child(proposal_address_badge(label));
                }
                target_row
            })
            .children(decoded.as_ref().map(|decoded| {
                let preview = proposal_decoded_preview(
                    decoded,
                    action.call_contract,
                    value,
                    chain_id,
                    anchor_rates,
                    token_registry,
                    effective_chain,
                    public_accounts,
                    public_address_book,
                );
                render_proposal_decoded_preview(&preview, &identity, ordinal)
            }))
            .child(app_muted_text("Raw calldata").text_size(px(11.0)))
            .child(
                Collapsible::new()
                    .open(expanded)
                    .w_full()
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                app_button_base(calldata_toggle_id)
                                    .text()
                                    .small()
                                    .compact()
                                    .icon(if expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(
                                        app_text(compact_calldata)
                                            .font_family(APP_MONO_FONT_FAMILY)
                                            .text_size(px(12.0))
                                            .whitespace_nowrap(),
                                    )
                                    .on_click(move |_event, _window, cx| {
                                        cx.stop_propagation();
                                        calldata_toggle_root.update(cx, |root, cx| {
                                            root.toggle_proposal_calldata(
                                                calldata_toggle_identity.clone(),
                                                ordinal,
                                                cx,
                                            );
                                        });
                                    }),
                            ),
                    )
                    .content(
                        div()
                            .w_full()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .pl(px(14.0))
                            .pt(px(8.0))
                            .pb(px(7.0))
                            .child(
                                div()
                                    .id(proposal_action_id(
                                        &identity, ordinal, "calldata", "expanded",
                                    ))
                                    .w_full()
                                    .min_w(px(0.0))
                                    .relative()
                                    .child(
                                        div()
                                            .absolute()
                                            .top(px(-2.0))
                                            .right(px(0.0))
                                            .id(calldata_copy_wrapper_id)
                                            .tooltip(|window, cx| {
                                                Tooltip::new("Copy calldata").build(window, cx)
                                            })
                                            .child(clipboard_with_toast(
                                                calldata_copy_id,
                                                calldata.clone(),
                                            )),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .min_w(px(0.0))
                                            .pr(px(28.0))
                                            .font_family(APP_MONO_FONT_FAMILY)
                                            .text_size(px(12.0))
                                            .line_height(px(16.0))
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .whitespace_normal()
                                            .child(SharedString::from(calldata)),
                                    ),
                            ),
                    ),
            );
        if !value.is_zero() {
            action_card = action_card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(app_muted_text("Value").text_size(px(11.0)))
                    .child(
                        app_text(format_native_token_amount_for_display(chain_id, value))
                            .text_size(px(12.0))
                            .font_family(APP_MONO_FONT_FAMILY),
                    ),
            );
        }
        card = card.child(action_card);
    }
    card
}

fn proposal_document_card(
    proposal: &ResolvedProposal,
    description_id: SharedString,
    table_scroll_handles: &BTreeMap<String, ScrollHandle>,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) -> gpui::Div {
    let mut card = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_2()
        .p(px(20.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER));
    let mut header = div()
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .child(app_strong_text("Proposal").text_size(px(13.0)))
        .child(div().flex_1());
    if let Some(document) = proposal.document().filter(|document| document.available) {
        header = header.child(
            div()
                .id(proposal_copy_id(proposal, "proposal", "action"))
                .tooltip(|window, cx| Tooltip::new("Copy proposal text").build(window, cx))
                .child(clipboard_with_toast(
                    proposal_copy_id(proposal, "proposal", "clipboard"),
                    document.description.clone(),
                )),
        );
    }
    let content = match proposal.document() {
        None => div()
            .flex()
            .flex_col()
            .gap_2()
            .children([520.0_f32, 460.0, 500.0, 340.0].into_iter().map(|width| {
                Skeleton::new()
                    .secondary()
                    .h(px(14.0))
                    .w(px(width))
                    .max_w_full()
            }))
            .into_any_element(),
        Some(document) if !document.available => div()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .p(px(24.0))
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size_6()
                    .text_color(rgb(theme::TEXT_MUTED)),
            )
            .child(app_muted_text(
                "Document could not be retrieved from IPFS gateways.",
            ))
            .child(app_muted_text("Refresh retries the download.").text_size(px(12.0)))
            .into_any_element(),
        Some(document) => match proposal.presentation() {
            Some(ProposalPresentation::Prepared(prepared)) => render_proposal_blocks(
                &prepared.blocks,
                &description_id,
                "root",
                &proposal.identity(),
                table_scroll_handles,
                window,
                cx,
            ),
            Some(ProposalPresentation::RawParseFallback(source)) => {
                TextView::markdown(description_id, source.clone(), window, cx)
                    .selectable(true)
                    .into_any_element()
            }
            None => TextView::markdown(
                description_id,
                inert_raw_fallback_source(&document.description),
                window,
                cx,
            )
            .selectable(true)
            .into_any_element(),
            Some(ProposalPresentation::TooComplex) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .p(px(24.0))
                .child(app_muted_text(
                    "This proposal is too complex to render safely.",
                ))
                .child(
                    app_muted_text("Copy the proposal text to view it externally.")
                        .text_size(px(12.0)),
                )
                .into_any_element(),
        },
    };
    card = card.child(header).child(content);
    card
}
fn proposal_card(title: &'static str) -> gpui::Div {
    div()
        .w_full()
        .p(px(14.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .flex()
        .flex_col()
        .gap_2()
        .child(app_strong_text(title).text_size(px(13.0)))
}
fn sponsorship_per_mille(proposal: &ResolvedProposal) -> u16 {
    let scale = U256::from(10).pow(U256::from(18));
    let threshold = u128::try_from(proposal.rules.sponsor_threshold / scale).unwrap_or(0);
    let sponsored = u128::try_from(proposal.proposal.sponsorship / scale).unwrap_or(0);
    per_mille(U256::from(sponsored), U256::from(threshold))
}
fn render_sponsorship_card(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let per_mille = sponsorship_per_mille(proposal);
    let fraction = ratio_fraction(per_mille);
    let color = if status.stage == GovernanceProposalStage::SponsorshipExpired {
        theme::TEXT_MUTED
    } else {
        theme::PRIMARY
    };
    let percent = (u32::from(per_mille) + 5) / 10;
    let mut footer_row = div()
        .flex()
        .justify_between()
        .child(app_muted_text(format!("{percent}% of threshold")).text_size(px(11.0)));
    if status.stage == GovernanceProposalStage::SponsorshipExpired {
        footer_row = footer_row.child(
            app_muted_text(format!(
                "Expired {}",
                format_deadline(&status.deadlines.sponsorship,)
            ))
            .text_size(px(11.0)),
        );
    } else if let Some(remaining) = countdown(&status.deadlines.sponsorship, chain_time) {
        footer_row =
            footer_row.child(app_muted_text(format!("Ends in {remaining}")).text_size(px(11.0)));
    }
    proposal_card("Sponsorship")
        .child(
            app_strong_text(format!(
                "{} / {} RAIL",
                format_compact_rail_amount(proposal.proposal.sponsorship),
                format_compact_rail_amount(proposal.rules.sponsor_threshold)
            ))
            .text_size(px(15.0)),
        )
        .child(
            div()
                .w_full()
                .h(px(6.0))
                .rounded_full()
                .overflow_hidden()
                .bg(rgb(theme::SURFACE_HOVER))
                .child(
                    div()
                        .w(px((300.0 - 28.0) * fraction))
                        .h(px(6.0))
                        .rounded_full()
                        .bg(rgb(color)),
                ),
        )
        .child(footer_row)
}
fn render_votes_card(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let split = vote_split(proposal.proposal.yay_votes, proposal.proposal.nay_votes);
    let for_fraction = split.map_or(0.0, ratio_fraction);
    let gap = if split.is_some_and(|value| value > 0 && value < 1_000) {
        2.0
    } else {
        0.0
    };
    let width = 272.0;
    let meter = div()
        .w_full()
        .h(px(6.0))
        .flex()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(theme::SURFACE_HOVER))
        .children(split.map(|_| {
            div()
                .w(px((width - gap) * for_fraction))
                .h(px(6.0))
                .bg(rgb(theme::PRIMARY))
        }))
        .children(
            split
                .filter(|_| gap > 0.0)
                .map(|_| div().w(px(gap)).h(px(6.0)).bg(rgb(theme::SURFACE))),
        )
        .children(split.map(|_| div().flex_1().h(px(6.0)).bg(rgb(theme::DANGER))));
    let legend = |label: &'static str, amount: U256, color: u32, fraction: Option<u16>| {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().size(px(8.0)).rounded_full().bg(rgb(color)))
            .child(app_muted_text(label).flex_1())
            .child(
                app_text(format_compact_rail_amount_with_unit(amount))
                    .text_color(rgb(theme::TEXT))
                    .text_size(px(12.0)),
            )
            .children(
                fraction
                    .map(|value| app_muted_text(format_ratio_percent(value)).text_size(px(11.0))),
            )
    };
    proposal_card("Votes")
        .child(meter)
        .children([
            legend("For", proposal.proposal.yay_votes, theme::PRIMARY, split),
            legend(
                "Against",
                proposal.proposal.nay_votes,
                theme::DANGER,
                split.map(|value| 1_000 - value),
            ),
        ])
        .children(
            split
                .is_none()
                .then(|| app_muted_text("No votes").text_size(px(11.0))),
        )
        .child(
            app_muted_text(format!(
                "Sponsored {} / {} RAIL",
                format_compact_rail_amount(proposal.proposal.sponsorship),
                format_compact_rail_amount(proposal.rules.sponsor_threshold)
            ))
            .text_size(px(11.0)),
        )
        .child(
            app_muted_text(format!(
                "Quorum: {} / {} · {}",
                format_compact_rail_amount(status.quorum_progress),
                format_compact_rail_amount(status.quorum),
                if status.quorum_met { "met" } else { "not met" },
            ))
            .text_size(px(11.0)),
        )
        .child(
            app_muted_text(match status.majority {
                wallet_ops::GovernanceMajorityResult::Yay => "Majority: yay",
                wallet_ops::GovernanceMajorityResult::Nay => "Majority: nay",
                wallet_ops::GovernanceMajorityResult::Tie => "Majority: tie",
            })
            .text_size(px(11.0)),
        )
        .children((status.stage == GovernanceProposalStage::Failed).then(|| {
            app_muted_text(if status.quorum_met {
                "Failed: yay majority not met"
            } else {
                "Failed: quorum not met"
            })
            .text_size(px(11.0))
        }))
}
fn timeline_step(
    icon: Option<IconName>,
    label: &'static str,
    value: String,
    color: u32,
    has_connector: bool,
) -> gpui::Div {
    let marker = icon.map_or_else(
        || {
            div()
                .size(px(10.0))
                .rounded_full()
                .border_1()
                .border_color(rgb(theme::BORDER))
        },
        |icon| {
            div()
                .size(px(14.0))
                .child(Icon::new(icon).text_color(rgb(color)))
        },
    );
    let mut body = div()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_strong_text(label).text_size(px(12.0)))
        .child(app_muted_text(value).text_size(px(11.0)));
    if has_connector {
        body = body.mb(px(12.0));
    }
    div()
        .flex()
        .gap_2()
        .child(
            div()
                .w(px(14.0))
                .flex()
                .flex_col()
                .items_center()
                .child(marker)
                .children(has_connector.then(|| {
                    div()
                        .w(px(1.0))
                        .flex_1()
                        .min_h(px(12.0))
                        .bg(rgb(theme::BORDER_SUBTLE))
                })),
        )
        .child(body)
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

fn timeline_deadline_completed(
    milestone: TimelineDeadline,
    chain_time: U256,
    deadline: U256,
) -> bool {
    match milestone {
        TimelineDeadline::VotingOpen | TimelineDeadline::ExecutionOpen => chain_time > deadline,
        TimelineDeadline::SponsorshipClose
        | TimelineDeadline::YayVotingEnd
        | TimelineDeadline::NayVotingEnd
        | TimelineDeadline::ExecutionClose => chain_time >= deadline,
    }
}

fn timeline_sponsorship_completed(called: bool, chain_time: U256, deadline: U256) -> bool {
    called || timeline_deadline_completed(TimelineDeadline::SponsorshipClose, chain_time, deadline)
}

fn render_timeline_card(proposal: &ResolvedProposal, chain_time: U256) -> gpui::Div {
    let status = proposal.status(chain_time);
    let called = !proposal.proposal.vote_call_time.is_zero();
    let sponsorship = if status.stage == GovernanceProposalStage::SponsorshipExpired {
        (
            Some(IconName::CircleX),
            format!("Expired {}", format_deadline(&status.deadlines.sponsorship)),
            theme::TEXT_MUTED,
        )
    } else if timeline_sponsorship_completed(called, chain_time, status.deadlines.sponsorship) {
        (
            Some(IconName::CircleCheck),
            format_deadline(&status.deadlines.sponsorship),
            theme::SUCCESS,
        )
    } else {
        (
            None,
            format_deadline(&status.deadlines.sponsorship),
            theme::TEXT_MUTED,
        )
    };
    let mut steps = div().flex().flex_col();
    steps = steps.child(timeline_step(
        Some(IconName::CircleCheck),
        "Published",
        format_datetime_short(&proposal.proposal.publish_time),
        theme::SUCCESS,
        true,
    ));
    steps = steps.child(timeline_step(
        sponsorship.0,
        "Sponsorship closes",
        sponsorship.1,
        sponsorship.2,
        true,
    ));
    steps = steps.child(timeline_step(
        called.then_some(IconName::CircleCheck),
        "Voting called",
        if called {
            format_datetime_short(&proposal.proposal.vote_call_time)
        } else {
            "Not called".to_string()
        },
        if called {
            theme::SUCCESS
        } else {
            theme::TEXT_MUTED
        },
        true,
    ));
    if called {
        for (milestone, label, deadline) in [
            (
                TimelineDeadline::VotingOpen,
                "Voting opens",
                status.deadlines.voting_start,
            ),
            (
                TimelineDeadline::YayVotingEnd,
                "Yay voting ends",
                status.deadlines.yay_end,
            ),
            (
                TimelineDeadline::NayVotingEnd,
                "Nay voting ends",
                status.deadlines.nay_end,
            ),
            (
                TimelineDeadline::ExecutionOpen,
                "Execution opens",
                status.deadlines.execution_start,
            ),
            (
                TimelineDeadline::ExecutionClose,
                "Execution closes",
                status.deadlines.execution_end,
            ),
        ] {
            if let Some(deadline) = deadline {
                let completed = timeline_deadline_completed(milestone, chain_time, deadline);
                steps = steps.child(timeline_step(
                    completed.then_some(IconName::CircleCheck),
                    label,
                    format_deadline(&deadline),
                    if completed {
                        theme::SUCCESS
                    } else {
                        theme::TEXT_MUTED
                    },
                    true,
                ));
            }
        }
    }
    if proposal.proposal.executed {
        steps = steps.child(timeline_step(
            Some(IconName::CircleCheck),
            "Executed",
            "Confirmed on chain".to_string(),
            theme::SUCCESS,
            false,
        ));
    }
    proposal_card("Timeline").child(steps)
}
fn render_details_card(proposal: &ResolvedProposal) -> gpui::Div {
    let proposer = proposal.proposal.proposer.to_checksum(None);
    let contract = proposal.proposal.contract_address.to_checksum(None);
    proposal_card("Details")
        .child(proposal_copy_control(
            proposal, "Proposer", proposer, "proposer",
        ))
        .child(proposal_copy_control(
            proposal,
            "IPFS CID",
            proposal.proposal.proposal_document.clone(),
            "document",
        ))
        .child(proposal_copy_control(
            proposal, "Contract", contract, "contract",
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_muted_text("Version").text_size(px(11.0)))
                .child(
                    app_text(match proposal.proposal.contract_version {
                        wallet_ops::GovernanceContractVersion::V2 => "V2 Voting",
                        wallet_ops::GovernanceContractVersion::V1 => "V1 VotingLegacy",
                    })
                    .text_size(px(12.0)),
                ),
        )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Instant;

    use alloy::primitives::{Address, B256, FixedBytes, U256, address};
    use alloy::sol_types::SolCall;
    use markdown::{ParseOptions, mdast::Node, to_mdast};

    use super::{
        CONTENT_WIDTH, DecodedProposalAction, DocumentCompletion, MAX_MDAST_NODES,
        MAX_PREPARED_SOURCE_BYTES, MAX_TABLE_RENDER_WIDTH_PX, PreparationBudget,
        ProposalActionIdentity, ProposalBlock, ProposalCapacityKind, ProposalCapacitySummaryState,
        ProposalClosedHistoryKind, ProposalDecodedDetail, ProposalDelegator, ProposalDetailTab,
        ProposalDocumentState, ProposalErc20, ProposalGovernanceToken, ProposalGovernorRewards,
        ProposalOpStackSender, ProposalOwnable, ProposalPreparationFailure, ProposalPresentation,
        ProposalProxyAdmin, ProposalRailgun, ProposalSelection, ProposalTreasury,
        ProposalWrappedNative, ProposalsPage, ProposalsState, ResolvedProposal,
        TABLE_COLUMN_CHROME_PX, TABLE_OUTER_BORDER_PX, TimelineDeadline, action_calldata_hex,
        compact_calldata_display, decode_proposal_action, ensure_mdast_node_limit,
        format_compact_rail_amount, format_compact_rail_amount_with_unit,
        inert_raw_fallback_source, list_voting_deadline, next_proposal_page, parse_proposal_blocks,
        prepare_proposal_presentation, prepared_markdown_source_len, proposal_action_id,
        proposal_action_target_label, proposal_capacity_kind, proposal_capacity_summary_state,
        proposal_closed_history_kind, proposal_decoded_preview, proposal_role_label,
        proposal_table_scroll_key, send_document_completions, table_fingerprint,
        timeline_deadline_completed, timeline_sponsorship_completed, vote_split,
    };
    use ui::theme::APP_TEXT_SIZE;
    use wallet_ops::{
        GovernanceContractRules, GovernanceContractSummary, GovernanceContractVersion,
        GovernanceDocument, GovernanceOverview, GovernanceProposal, GovernanceProposalAction,
        GovernanceProposalDeadlines, GovernanceProposalStage, GovernanceProposalStatus,
        TokenAnchorRateCache,
    };

    fn test_proposal(index: u64, document: ProposalDocumentState) -> ResolvedProposal {
        ResolvedProposal {
            proposal: GovernanceProposal {
                contract_version: wallet_ops::GovernanceContractVersion::V2,
                index: U256::from(index),
                contract_address: Address::ZERO,
                proposer: Address::ZERO,
                proposal_document: String::new(),
                publish_time: U256::ZERO,
                vote_call_time: U256::ZERO,
                sponsorship: U256::ZERO,
                executed: false,
                yay_votes: U256::ZERO,
                nay_votes: U256::ZERO,
                sponsor_snapshot_interval: U256::ZERO,
                voting_snapshot_interval: U256::ZERO,
                actions: Vec::new(),
            },
            rules: GovernanceContractRules {
                sponsor_threshold: U256::ZERO,
                quorum: U256::ZERO,
                sponsor_window: U256::from(1),
                voting_start_offset: U256::from(1),
                voting_yay_end_offset: U256::from(2),
                voting_nay_end_offset: U256::from(3),
                execution_start_offset: U256::from(4),
                execution_end_offset: U256::from(5),
            },
            document,
        }
    }

    #[test]
    fn participation_capacity_follows_proposal_stage() {
        let cases = [
            (
                GovernanceProposalStage::AwaitingSponsorship,
                ProposalCapacityKind::Sponsorship,
            ),
            (
                GovernanceProposalStage::ReadyToCallVote,
                ProposalCapacityKind::Sponsorship,
            ),
            (
                GovernanceProposalStage::SponsorshipExpired,
                ProposalCapacityKind::Sponsorship,
            ),
            (
                GovernanceProposalStage::VoteCallExpired,
                ProposalCapacityKind::Sponsorship,
            ),
            (
                GovernanceProposalStage::VotingDelay,
                ProposalCapacityKind::Voting,
            ),
            (
                GovernanceProposalStage::VotingOpen,
                ProposalCapacityKind::Voting,
            ),
            (
                GovernanceProposalStage::NayOnlyVoting,
                ProposalCapacityKind::Voting,
            ),
            (
                GovernanceProposalStage::Failed,
                ProposalCapacityKind::Voting,
            ),
            (
                GovernanceProposalStage::PassedAwaitingExecution,
                ProposalCapacityKind::Voting,
            ),
            (
                GovernanceProposalStage::PassedExecutable,
                ProposalCapacityKind::Voting,
            ),
            (
                GovernanceProposalStage::ExecutionExpired,
                ProposalCapacityKind::Voting,
            ),
            (
                GovernanceProposalStage::Executed,
                ProposalCapacityKind::Voting,
            ),
        ];
        for (stage, expected) in cases {
            assert_eq!(proposal_capacity_kind(stage), expected);
        }
    }

    #[test]
    fn participation_summary_state_classifies_capacity_and_failures() {
        let cases = [
            (
                Some(U256::from(3)),
                Some(U256::from(3)),
                0,
                0,
                1,
                ProposalCapacitySummaryState::Full,
            ),
            (
                Some(U256::ZERO),
                Some(U256::ZERO),
                0,
                0,
                1,
                ProposalCapacitySummaryState::Full,
            ),
            (
                Some(U256::ZERO),
                Some(U256::from(3)),
                0,
                0,
                1,
                ProposalCapacitySummaryState::Exhausted,
            ),
            (
                Some(U256::from(1)),
                Some(U256::from(3)),
                0,
                0,
                1,
                ProposalCapacitySummaryState::Partial,
            ),
            (None, None, 1, 0, 1, ProposalCapacitySummaryState::Loading),
            (
                Some(U256::from(1)),
                None,
                0,
                1,
                1,
                ProposalCapacitySummaryState::Unavailable,
            ),
            (
                None,
                None,
                1,
                1,
                1,
                ProposalCapacitySummaryState::Unavailable,
            ),
            (
                Some(U256::ZERO),
                Some(U256::ZERO),
                0,
                0,
                0,
                ProposalCapacitySummaryState::Empty,
            ),
        ];
        for (available, maximum, loading, unavailable, participants, expected) in cases {
            assert_eq!(
                proposal_capacity_summary_state(
                    available,
                    maximum,
                    loading,
                    unavailable,
                    participants,
                ),
                expected
            );
        }
    }

    #[test]
    fn participation_history_maps_only_terminal_stages() {
        for stage in [
            GovernanceProposalStage::SponsorshipExpired,
            GovernanceProposalStage::VoteCallExpired,
        ] {
            assert_eq!(
                proposal_closed_history_kind(stage),
                Some(ProposalClosedHistoryKind::Sponsorship)
            );
        }
        for stage in [
            GovernanceProposalStage::Failed,
            GovernanceProposalStage::PassedAwaitingExecution,
            GovernanceProposalStage::PassedExecutable,
            GovernanceProposalStage::ExecutionExpired,
            GovernanceProposalStage::Executed,
        ] {
            assert_eq!(
                proposal_closed_history_kind(stage),
                Some(ProposalClosedHistoryKind::Voting)
            );
        }
        assert_eq!(
            proposal_closed_history_kind(GovernanceProposalStage::VotingDelay),
            None
        );
    }

    fn resolved_document() -> ProposalDocumentState {
        resolved_document_with_title("resolved")
    }

    fn resolved_document_with_title(title: &str) -> ProposalDocumentState {
        ProposalDocumentState::Resolved {
            document: GovernanceDocument {
                title: title.to_string(),
                description: String::new(),
                available: true,
            },
            presentation: ProposalPresentation::RawParseFallback(inert_raw_fallback_source("")),
        }
    }

    #[test]
    fn proposal_block_parser_preserves_all_description_list_items() {
        let source = "Making way for Railgun v3, this proposal:\n\n- Aims to make it easier for others (through a logic contract upgrade) to build & run public broadcaster code, ideally resulting in a more reliable and easier to use broadcaster network.\n- Updates the cyptographic circuits of RAILGUN to improve gas efficiency, remove redundant code for efficiency purposes, and add additional security features.\n- Adds new randomness to the circuits via an additional ceremony.\n- No other changes.\n\nThis will be a full upgrade of the RAILGUN logic contract and circuits if the YES vote is successful.\n\nPlease feel free to read through the code of this RAILGUN v2.5 proposal and vote YES if you support these upgrades.\n";
        let blocks = parse_proposal_blocks(source).expect("description should parse");
        assert_eq!(blocks.len(), 3);
        let ProposalBlock::List(list) = &blocks[1] else {
            panic!("description list should remain a list block");
        };
        assert_eq!(list.items.len(), 4);
        let contents = list
            .items
            .iter()
            .map(|item| match item.blocks.first() {
                Some(ProposalBlock::Markdown(content)) => content.as_str(),
                _ => panic!("list item should retain paragraph content"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            contents,
            vec![
                "Aims to make it easier for others (through a logic contract upgrade) to build & run public broadcaster code, ideally resulting in a more reliable and easier to use broadcaster network.",
                "Updates the cyptographic circuits of RAILGUN to improve gas efficiency, remove redundant code for efficiency purposes, and add additional security features.",
                "Adds new randomness to the circuits via an additional ceremony.",
                "No other changes.",
            ]
        );
    }

    #[test]
    fn proposal_block_parser_keeps_only_effective_referenced_definitions() {
        let source = "[first][guide]\n\n[guide]: https://example.com/first\n[guide]: https://example.com/second\n[unused]: https://example.com/unused\n";
        let blocks = parse_proposal_blocks(source).expect("description should parse");
        assert_eq!(blocks.len(), 1);
        let ProposalBlock::Markdown(content) = &blocks[0] else {
            panic!("reference paragraph should remain a markdown leaf");
        };
        assert!(content.contains("[guide]: https://example.com/first"));
        assert!(!content.contains("https://example.com/second"));
        assert!(!content.contains("https://example.com/unused"));
    }

    #[test]
    fn proposal_block_parser_coalesces_exact_ranges_and_respects_boundaries() {
        let source = "first\n\n[one][guide]\n\n[two][guide]\n\n[guide]: https://example.com/guide\n\n- item\n\n> quote\n\nlast";
        let blocks = parse_proposal_blocks(source).expect("description should parse");
        assert_eq!(blocks.len(), 4);
        let ProposalBlock::Markdown(content) = &blocks[0] else {
            panic!("ordinary nodes should be coalesced into one markdown leaf");
        };
        assert!(content.starts_with("first\n\n"));
        let Node::Root(rendered) =
            to_mdast(content, &ParseOptions::gfm()).expect("coalesced markdown should parse")
        else {
            panic!("coalesced markdown should have a root");
        };
        let visible = super::visible_node_text(&Node::Root(rendered));
        assert!(visible.contains("one (https://example.com/guide)"));
        assert!(visible.contains("two (https://example.com/guide)"));
        assert_eq!(
            content
                .matches("[guide]: https://example.com/guide")
                .count(),
            1
        );
        assert!(matches!(&blocks[1], ProposalBlock::List(_)));
        assert!(matches!(&blocks[2], ProposalBlock::Blockquote(_)));
        assert_eq!(blocks[3], ProposalBlock::Markdown("last".to_string()));
    }

    #[test]
    fn proposal_block_parser_keeps_tables_as_exact_scoped_boundaries() {
        let source = "before\n\n| Name | Link |\n| --- | --- |\n| one | [guide][guide] |\n| two | ![diagram](https://image.example) |\n\n[guide]: https://example.com/guide\n\nafter";
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(source) else {
            panic!("table description should prepare");
        };
        assert_eq!(prepared.table_count, 1);
        assert_eq!(prepared.blocks.len(), 3);
        assert_eq!(
            prepared.blocks[0],
            ProposalBlock::Markdown("before".to_string())
        );
        let ProposalBlock::Table(table) = &prepared.blocks[1] else {
            panic!("table should remain a structural block");
        };
        assert_eq!(table.ordinal, 0);
        assert!(table.source.starts_with("| Name | Link |\n| --- | --- |"));
        assert!(
            table
                .render_source
                .starts_with("| Name | Link |\n| --- | --- |")
        );
        let Node::Root(rendered) =
            to_mdast(&table.render_source, &ParseOptions::gfm()).expect("render table parses")
        else {
            panic!("render table should have a root");
        };
        assert!(!super::contains_activatable_markdown(&rendered.children));
        assert!(
            super::visible_node_text(&Node::Root(rendered))
                .contains("diagram (https://image.example)")
        );
        assert_eq!(
            table
                .source
                .matches("[guide]: https://example.com/guide")
                .count(),
            1
        );
        assert_eq!(
            prepared.blocks[2],
            ProposalBlock::Markdown("after".to_string())
        );
    }

    #[test]
    fn proposal_tables_receive_recursive_source_order_ordinals() {
        let source = "| root |\n| --- |\n| one |\n\n> | quote |\n> | --- |\n> | two |\n\n- | list |\n  | --- |\n  | three |\n";
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(source) else {
            panic!("nested table description should prepare");
        };
        assert_eq!(prepared.table_count, 3);
        let ProposalBlock::Table(root) = &prepared.blocks[0] else {
            panic!("root table should be first");
        };
        let ProposalBlock::Blockquote(quote) = &prepared.blocks[1] else {
            panic!("quote should remain structural");
        };
        let ProposalBlock::Table(quoted) = &quote.blocks[0] else {
            panic!("quoted table should be second");
        };
        let ProposalBlock::List(list) = &prepared.blocks[2] else {
            panic!("list should remain structural");
        };
        let ProposalBlock::Table(listed) = &list.items[0].blocks[0] else {
            panic!("list table should be third");
        };
        assert_eq!((root.ordinal, quoted.ordinal, listed.ordinal), (0, 1, 2));
        assert_eq!(root.source, "| root |\n| --- |\n| one |");
        for table in [root, quoted, listed] {
            let Node::Root(parsed) = to_mdast(&table.source, &ParseOptions::gfm())
                .expect("prepared table should reparse")
            else {
                panic!("standalone table source should produce a root");
            };
            assert_eq!(parsed.children.len(), 1);
            assert!(matches!(parsed.children.first(), Some(Node::Table(_))));
            assert!(matches!(
                parsed.children.first(),
                Some(Node::Table(table)) if table_fingerprint(table).is_some()
            ));
        }
    }

    #[test]
    fn nested_table_normalization_preserves_inline_content_and_effective_definition() {
        let source = "- > | Name | Details |\n  > | --- | --- |\n  > | *one* | `two` [guide][guide] |\n\n[guide]: https://example.com/guide\n";
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(source) else {
            panic!("nested table description should prepare");
        };
        let ProposalBlock::List(list) = &prepared.blocks[0] else {
            panic!("list should remain structural");
        };
        let ProposalBlock::Blockquote(quote) = &list.items[0].blocks[0] else {
            panic!("blockquote should remain structural");
        };
        let ProposalBlock::Table(table) = &quote.blocks[0] else {
            panic!("nested table should remain structural");
        };
        assert_eq!(table.source.matches("[guide]:").count(), 1);
        let Node::Root(rendered) =
            to_mdast(&table.render_source, &ParseOptions::gfm()).expect("render table parses")
        else {
            panic!("render table should have a root");
        };
        assert!(!super::contains_activatable_markdown(&rendered.children));
        let Node::Root(parsed) =
            to_mdast(&table.source, &ParseOptions::gfm()).expect("normalized table should reparse")
        else {
            panic!("normalized table should produce a root");
        };
        assert!(matches!(parsed.children.first(), Some(Node::Table(_))));
        assert!(matches!(parsed.children.get(1), Some(Node::Definition(_))));
        assert_eq!(parsed.children.len(), 2);
    }

    #[test]
    fn proposal_table_intrinsic_width_tracks_content_and_is_bounded() {
        let long_cell = "W".repeat(100);
        let source = format!("| Value |\n| --- |\n| {long_cell} |\n");
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(&source)
        else {
            panic!("long-wide table should prepare");
        };
        let ProposalBlock::Table(table) = &prepared.blocks[0] else {
            panic!("long-wide table should remain structural");
        };
        let expected_content_width = 100 * usize::from(APP_TEXT_SIZE.ceil());
        let expected_width =
            expected_content_width + TABLE_COLUMN_CHROME_PX + TABLE_OUTER_BORDER_PX;
        assert!(usize::from(table.render_width_px) >= expected_width);
        assert!(usize::from(table.render_width_px) > usize::from(CONTENT_WIDTH));

        let compact_source = "| Name | Link |\n| --- | --- |\n| one | two |\n";
        let ProposalPresentation::Prepared(prepared) =
            prepare_proposal_presentation(compact_source)
        else {
            panic!("compact table should prepare");
        };
        let ProposalBlock::Table(table) = &prepared.blocks[0] else {
            panic!("compact table should remain structural");
        };
        assert!(usize::from(table.render_width_px) < usize::from(CONTENT_WIDTH));

        let multibyte_source = "| Value |\n| --- |\n| 世界🌍 |\n";
        let ProposalPresentation::Prepared(prepared) =
            prepare_proposal_presentation(multibyte_source)
        else {
            panic!("multibyte table should prepare");
        };
        let ProposalBlock::Table(table) = &prepared.blocks[0] else {
            panic!("multibyte table should remain structural");
        };
        assert!(usize::from(table.render_width_px) <= MAX_TABLE_RENDER_WIDTH_PX);

        let column_count = 40;
        let headers = (0..column_count)
            .map(|column| format!("h{column}"))
            .collect::<Vec<_>>()
            .join(" | ");
        let separators = (0..column_count)
            .map(|_| "---")
            .collect::<Vec<_>>()
            .join(" | ");
        let pathological_cell = "x".repeat(160);
        let cells = (0..column_count)
            .map(|_| pathological_cell.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        let source = format!("| {headers} |\n| {separators} |\n| {cells} |\n");
        let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(&source)
        else {
            panic!("pathological table should prepare");
        };
        let ProposalBlock::Table(table) = &prepared.blocks[0] else {
            panic!("pathological table should remain structural");
        };
        assert_eq!(
            usize::from(table.render_width_px),
            MAX_TABLE_RENDER_WIDTH_PX
        );
    }

    #[test]
    fn proposal_preparation_enforces_render_complexity_at_limit() {
        let mut budget = PreparationBudget::default();
        for _ in 0..85 {
            budget
                .account_markdown(1)
                .expect("85 markdown chunks should use 255 units");
        }
        budget
            .account_list_item()
            .expect("the 256th unit should be admitted");
        assert_eq!(budget.weighted_complexity, 256);
        assert_eq!(
            budget.account_blockquote(),
            Err(ProposalPreparationFailure::TooComplex)
        );
    }

    #[test]
    fn proposal_preparation_accounts_table_complexity_through_production() {
        let source = (0..86)
            .map(|_| "| value |\n| --- |\n\n")
            .collect::<String>();
        assert!(matches!(
            prepare_proposal_presentation(&source),
            ProposalPresentation::TooComplex
        ));
    }

    #[test]
    fn proposal_preparation_rejects_mdast_node_limit_before_building_leaves() {
        let source = "# \n".repeat(MAX_MDAST_NODES);
        let Node::Root(root) = to_mdast(&source, &ParseOptions::gfm()).expect("markdown parses")
        else {
            panic!("markdown parser should produce a root");
        };
        assert_eq!(
            ensure_mdast_node_limit(&root.children),
            Err(ProposalPreparationFailure::TooComplex)
        );

        let source = "# \n".repeat(MAX_MDAST_NODES - 1);
        let Node::Root(root) = to_mdast(&source, &ParseOptions::gfm()).expect("markdown parses")
        else {
            panic!("markdown parser should produce a root");
        };
        assert!(ensure_mdast_node_limit(&root.children).is_ok());
        assert!(matches!(
            prepare_proposal_presentation(&"# \n".repeat(MAX_MDAST_NODES)),
            ProposalPresentation::TooComplex
        ));
    }

    #[test]
    fn proposal_preparation_counts_definition_appendix_bytes_cumulatively() {
        let referenced = ["[guide]: https://example.com/guide"];
        let source_len = MAX_PREPARED_SOURCE_BYTES - 40;
        let prepared_len = prepared_markdown_source_len(source_len, &referenced)
            .expect("source length arithmetic should be checked");
        let mut budget = PreparationBudget::default();
        budget
            .account_markdown(prepared_len)
            .expect("the first prepared leaf should fit");
        assert_eq!(budget.prepared_source_bytes, prepared_len);
        assert_eq!(
            budget.account_markdown(20),
            Err(ProposalPreparationFailure::TooComplex)
        );
    }

    #[test]
    fn proposal_block_parser_prepares_lists_nested_in_blockquotes() {
        let source = "> quoted\n>\n> - outer\n>   - inner\n";
        let blocks = parse_proposal_blocks(source).expect("description should parse");
        let ProposalBlock::Blockquote(quote) = &blocks[0] else {
            panic!("quote should remain a structural block");
        };
        let ProposalBlock::List(list) = &quote.blocks[1] else {
            panic!("quoted list should remain a custom list block");
        };
        let ProposalBlock::List(nested) = &list.items[0].blocks[1] else {
            panic!("nested quoted list should remain a custom list block");
        };
        assert_eq!(nested.items.len(), 1);
    }

    #[test]
    fn proposal_markdown_links_are_rendered_as_inert_visible_content() {
        let source = "[human **label**](https://example.com) [https://label.example](https://destination.example) [https://same.example](https://same.example) [support@example.com](mailto:support@example.com) *[docs at https://example.com](https://destination.example)* *[contact support@example.com](mailto:support@example.com)* https://bare.example <https://autolink.example>\n\n[full][guide] [shortcut] [collapsed][] [![image](https://image.example)](https://wrapper.example)\n\n[guide]: https://reference.example\n[shortcut]: https://shortcut.example\n[collapsed]: https://collapsed.example";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        assert!(rendered.contains("**label**"));
        assert!(rendered.contains("human"));
        assert!(rendered.contains("&#58;"));
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("human label (https://example.com)"));
        assert!(visible.contains("https://label.example (https://destination.example)"));
        assert_eq!(visible.matches("https://same.example").count(), 1);
        assert_eq!(visible.matches("mailto:support@example.com").count(), 1);
        assert!(visible.contains("docs at https://example.com (https://destination.example)"));
        assert!(visible.contains("contact support@example.com"));
        assert!(visible.contains("full (https://reference.example)"));
        assert!(visible.contains("shortcut (https://shortcut.example)"));
        assert!(visible.contains("collapsed (https://collapsed.example)"));
        assert!(visible.contains("image (https://image.example) (https://wrapper.example)"));
        assert_eq!(visible.matches("https://autolink.example").count(), 1);
        assert_eq!(visible.matches("https://bare.example").count(), 1);
    }

    #[test]
    fn proposal_reference_links_use_first_definition_and_deduplicate_equivalent_urls() {
        let source = "[same][same] [same-short] [same-collapsed][] [email][email-ref]\n\n[same]: https://same.example\n[same-short]: https://short.example\n[same-collapsed]: https://collapsed.example\n[email-ref]: mailto:support@example.com\n[same]: https://later.example";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("same (https://same.example)"));
        assert!(!visible.contains("later.example"));
        assert!(visible.contains("same-short (https://short.example)"));
        assert!(visible.contains("same-collapsed (https://collapsed.example)"));
        assert!(visible.contains("email (mailto:support@example.com)"));
    }

    fn contains_markdown_node(nodes: &[Node], predicate: &impl Fn(&Node) -> bool) -> bool {
        nodes.iter().any(|node| {
            predicate(node)
                || node
                    .children()
                    .is_some_and(|children| contains_markdown_node(children, predicate))
        })
    }

    #[test]
    fn proposal_images_are_inert_and_preserve_distinct_destinations() {
        let source = "![direct](https://image.example) ![https://same-image.example](https://same-image.example) [![nested](https://shared.example)](https://shared.example) [![different](https://inner.example)](https://outer.example) ![reference][image-ref]\n\n[image-ref]: https://reference-image.example";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("direct (https://image.example)"));
        assert_eq!(visible.matches("https://same-image.example").count(), 1);
        assert_eq!(visible.matches("https://shared.example").count(), 1);
        assert!(visible.contains("different (https://inner.example) (https://outer.example)"));
        assert!(visible.contains("reference (https://reference-image.example)"));
    }

    #[test]
    fn proposal_destination_equivalence_is_exact_and_mailto_specific() {
        let source = "[mailto:https://label.example](https://label.example) [https://label.example](mailto:https://label.example) [foo](foo) [support@example.com](mailto:support@example.com)";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("mailto:https://label.example (https://label.example)"));
        assert!(visible.contains("https://label.example (mailto:https://label.example)"));
        assert!(visible.contains("foo (foo)"));
        assert_eq!(visible.matches("support@example.com").count(), 1);
        assert!(!visible.contains("mailto:support@example.com"));
    }

    #[test]
    fn proposal_equivalent_url_labels_preserve_safe_formatting() {
        let source = "[**https://strong.example**](https://strong.example) [*https://emphasis.example*](https://emphasis.example) [~~https://delete.example~~](https://delete.example) [`https://code.example`](https://code.example) [https://**mixed**.example](https://mixed.example)";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::Strong(_)
        )));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::Emphasis(_)
        )));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::Delete(_)
        )));
        assert!(contains_markdown_node(&root.children, &|node| matches!(
            node,
            Node::InlineCode(_)
        )));
        let visible = super::visible_node_text(&Node::Root(root));
        for destination in [
            "https://strong.example",
            "https://emphasis.example",
            "https://delete.example",
            "https://code.example",
            "https://mixed.example",
        ] {
            assert_eq!(visible.matches(destination).count(), 1);
        }
    }

    #[test]
    fn proposal_destinations_preserve_decoded_boundary_whitespace() {
        let source = "[inline](<https://example.com/a&#32;>) [reference][space]\n\n[space]: <&#32;https://example.com/b&#32;>";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        let visible = super::visible_node_text(&Node::Root(root));
        assert!(visible.contains("inline (https://example.com/a )"));
        assert!(visible.contains("reference ( https://example.com/b )"));
    }

    #[test]
    fn proposal_destination_entities_are_counted_in_prepared_source_budget() {
        let destination = "/".repeat(MAX_PREPARED_SOURCE_BYTES / 4);
        let source = format!("[label](<{destination}>)");
        assert!(matches!(
            prepare_proposal_presentation(&source),
            ProposalPresentation::TooComplex
        ));
    }

    #[test]
    fn proposal_markdown_sanitizes_nested_structures_and_html() {
        let source = "> quote [link](https://example.com)\n>\n> - [nested][ref]\n\n[ref]: https://reference.example\n\n<a href=\"https://example.com\">anchor</a>";
        let rendered = super::sanitize_render_source(source).expect("source should sanitize");
        assert!(rendered.contains("quote link"));
        assert!(rendered.contains("nested"));
        assert!(rendered.contains("anchor"));
        assert!(rendered.contains("`<a href=\"https://example.com\">"));
        assert!(rendered.contains("</a>`"));
        let Node::Root(root) =
            to_mdast(&rendered, &ParseOptions::gfm()).expect("sanitized markdown")
        else {
            panic!("sanitized markdown should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
    }

    #[test]
    fn malformed_proposal_fallback_is_selectable_inert_code() {
        let source = "[not closed\n<a href=\"https://example.com\">";
        let fallback = super::inert_raw_fallback_source(source);
        let Node::Root(root) = to_mdast(&fallback, &ParseOptions::gfm()).expect("fallback parses")
        else {
            panic!("fallback should have a root");
        };
        assert!(!super::contains_activatable_markdown(&root.children));
        assert!(fallback.contains(source));
    }

    #[test]
    fn table_scroll_handles_are_stable_per_proposal_and_ordinal() {
        let mut state = ProposalsState::new(1);
        let v2 = test_proposal(7, ProposalDocumentState::Pending).identity();
        let mut v1 = v2.clone();
        v1.contract_version = GovernanceContractVersion::V1;
        state.ensure_table_scroll_handles(&v2, 2);
        state.ensure_table_scroll_handles(&v1, 1);
        let first_key = proposal_table_scroll_key(&v2, 0);
        let second_key = proposal_table_scroll_key(&v2, 1);
        let v1_key = proposal_table_scroll_key(&v1, 0);
        assert_ne!(first_key, second_key);
        assert_ne!(first_key, v1_key);
        let first_offset = gpui::point(gpui::px(17.0), gpui::px(-3.0));
        state.table_scroll_handles[&first_key].set_offset(first_offset);
        let first_handle = state.table_scroll_handles[&first_key].clone();
        state.ensure_table_scroll_handles(&v2, 2);
        assert_eq!(state.table_scroll_handles.len(), 3);
        assert_eq!(
            state.table_scroll_handles[&first_key].offset(),
            first_offset
        );
        assert_eq!(
            state.table_scroll_handles[&first_key].offset(),
            first_handle.offset()
        );
    }

    #[test]
    fn calldata_display_preserves_exact_copy_source_and_compact_preview() {
        let calldata = alloy::primitives::Bytes::from(vec![0xab; 40]);
        let exact = action_calldata_hex(&calldata);
        assert_eq!(exact, format!("0x{}", "ab".repeat(40)));
        assert_eq!(
            compact_calldata_display(&exact),
            "0xababababababababab…abababababababab"
        );
    }

    #[test]
    fn proposal_decodes_generic_erc20_calls_to_semantic_fields() {
        let target = address!("0x1111111111111111111111111111111111111111");
        let recipient = address!("0x2222222222222222222222222222222222222222");
        let from = address!("0x3333333333333333333333333333333333333333");
        let amount = U256::from(42);
        assert_eq!(
            decode_proposal_action(
                1,
                target,
                &ProposalErc20::transferCall { recipient, amount }.abi_encode(),
                None,
                None,
            ),
            Some(DecodedProposalAction::Erc20Transfer { recipient, amount })
        );
        assert_eq!(
            decode_proposal_action(
                1,
                target,
                &ProposalErc20::transferFromCall {
                    from,
                    to: recipient,
                    amount,
                }
                .abi_encode(),
                None,
                None,
            ),
            Some(DecodedProposalAction::Erc20TransferFrom {
                from,
                to: recipient,
                amount,
            })
        );
        let spender = address!("0x4444444444444444444444444444444444444444");
        assert_eq!(
            decode_proposal_action(
                1,
                target,
                &ProposalErc20::approveCall { spender, amount }.abi_encode(),
                None,
                None,
            ),
            Some(DecodedProposalAction::Erc20Approve { spender, amount })
        );
    }

    #[test]
    fn proposal_decodes_common_governance_calls_with_target_guards() {
        let arbitrary_target = address!("0x1111111111111111111111111111111111111111");
        let proxy = address!("0x2222222222222222222222222222222222222222");
        let implementation = address!("0x3333333333333333333333333333333333333333");
        let account = address!("0x4444444444444444444444444444444444444444");
        let upgrade = ProposalProxyAdmin::upgradeCall {
            proxy,
            implementation,
        }
        .abi_encode();
        let upgrade_decoded = DecodedProposalAction::ProxyUpgrade {
            proxy,
            implementation,
        };
        assert_eq!(
            decode_proposal_action(1, arbitrary_target, &upgrade, None, None),
            Some(upgrade_decoded.clone())
        );
        let empty_registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        assert_eq!(
            proposal_action_target_label(
                1,
                arbitrary_target,
                &empty_registry,
                None,
                Some(&upgrade_decoded),
            ),
            Some("Proxy admin".to_owned())
        );

        let pause = ProposalProxyAdmin::pauseCall { proxy }.abi_encode();
        assert_eq!(&pause[..4], &[0x76, 0xa6, 0x7a, 0x51]);
        assert_eq!(
            decode_proposal_action(1, arbitrary_target, &pause, None, None),
            Some(DecodedProposalAction::ProxyPause { proxy })
        );
        let proxy_owner = ProposalProxyAdmin::transferProxyOwnershipCall {
            proxy,
            newOwner: account,
        }
        .abi_encode();
        assert_eq!(&proxy_owner[..4], &[0x00, 0x36, 0x1d, 0x55]);
        assert_eq!(
            decode_proposal_action(1, arbitrary_target, &proxy_owner, None, None),
            Some(DecodedProposalAction::ProxyTransferOwnership {
                proxy,
                new_owner: account,
            })
        );

        let railgun = address!("0x5555555555555555555555555555555555555555");
        let change_fee = ProposalRailgun::changeFeeCall {
            shieldFee: alloy::primitives::Uint::<120, 2>::from_limbs([10, 0]),
            unshieldFee: alloy::primitives::Uint::<120, 2>::from_limbs([10, 0]),
            nftFee: U256::ZERO,
        }
        .abi_encode();
        assert_eq!(&change_fee[..4], &[0xcc, 0x1f, 0x73, 0xfd]);
        assert_eq!(
            decode_proposal_action(1, railgun, &change_fee, None, Some(railgun)),
            Some(DecodedProposalAction::ChangeFee {
                shield_fee: U256::from(10),
                unshield_fee: U256::from(10),
                nft_fee: U256::ZERO,
            })
        );
        assert_eq!(
            decode_proposal_action(1, arbitrary_target, &change_fee, None, Some(railgun)),
            None
        );
        let mut trailing_change_fee = change_fee;
        trailing_change_fee.push(0);
        assert_eq!(
            decode_proposal_action(1, railgun, &trailing_change_fee, None, Some(railgun)),
            None
        );

        let contracts = railgun_ui::governance_contracts(1).expect("Ethereum governance");
        let mint = ProposalGovernanceToken::governanceMintCall {
            account,
            amount: U256::from(42),
        }
        .abi_encode();
        assert_eq!(
            decode_proposal_action(1, contracts.governance_token, &mint, None, None),
            Some(DecodedProposalAction::GovernanceMint {
                account,
                amount: U256::from(42),
            })
        );
        assert_eq!(
            decode_proposal_action(1, arbitrary_target, &mint, None, None),
            None
        );

        let interval = ProposalGovernorRewards::setIntervalBPCall {
            newIntervalBP: U256::from(420),
        }
        .abi_encode();
        assert_eq!(
            decode_proposal_action(1, contracts.governor_rewards, &interval, None, None),
            Some(DecodedProposalAction::SetIntervalBP {
                new_interval_bp: U256::from(420),
            })
        );
        assert_eq!(
            decode_proposal_action(1, arbitrary_target, &interval, None, None),
            None
        );

        let usdt = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
        let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let add_tokens = ProposalGovernorRewards::addTokensCall {
            tokens: vec![usdt, usdc],
        }
        .abi_encode();
        assert_eq!(&add_tokens[..4], [0x4a, 0xe0, 0x5c, 0x7d]);
        assert_eq!(
            decode_proposal_action(1, contracts.governor_rewards, &add_tokens, None, None),
            Some(DecodedProposalAction::AddTokens {
                tokens: vec![usdt, usdc],
            })
        );
        assert_eq!(
            decode_proposal_action(1, arbitrary_target, &add_tokens, None, None),
            None
        );
        let mut malformed_offset = add_tokens.clone();
        malformed_offset[4..36].fill(0);
        assert_eq!(
            decode_proposal_action(1, contracts.governor_rewards, &malformed_offset, None, None),
            None
        );
        let mut trailing_tokens = add_tokens;
        trailing_tokens.push(0);
        assert_eq!(
            decode_proposal_action(1, contracts.governor_rewards, &trailing_tokens, None, None),
            None
        );

        let caller = address!("0x64DA0892E8E24fECa6Eb5E3D8cbf2D9b6Fbe7598");
        let contract_address = address!("0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
        let selector = FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]);
        let permission = ProposalDelegator::setPermissionCall {
            caller,
            contractAddress: contract_address,
            selector,
            permission: true,
        }
        .abi_encode();
        assert_eq!(&permission[..4], [0xe6, 0x46, 0x24, 0xfa]);
        let permission_decoded = DecodedProposalAction::DelegatorSetPermission {
            caller,
            contract_address,
            selector,
            permission: true,
        };
        assert_eq!(
            decode_proposal_action(1, contracts.delegator, &permission, None, None),
            Some(permission_decoded)
        );
        assert_eq!(
            decode_proposal_action(1, arbitrary_target, &permission, None, None),
            None
        );

        let mut malformed_bool = permission.clone();
        malformed_bool[131] = 2;
        assert_eq!(
            decode_proposal_action(1, contracts.delegator, &malformed_bool, None, None),
            None
        );
        let mut malformed_address = permission.clone();
        malformed_address[4] = 1;
        assert_eq!(
            decode_proposal_action(1, contracts.delegator, &malformed_address, None, None),
            None
        );
        let mut malformed_padding = permission.clone();
        malformed_padding[72] = 1;
        assert_eq!(
            decode_proposal_action(1, contracts.delegator, &malformed_padding, None, None),
            None
        );
        let mut trailing_permission = permission.clone();
        trailing_permission.push(0);
        assert_eq!(
            decode_proposal_action(1, contracts.delegator, &trailing_permission, None, None),
            None
        );
        assert_eq!(
            decode_proposal_action(
                1,
                contracts.delegator,
                &permission[..permission.len() - 1],
                None,
                None
            ),
            None
        );

        let mut trailing = upgrade;
        trailing.push(0);
        assert_eq!(
            decode_proposal_action(1, arbitrary_target, &trailing, None, None),
            None
        );
    }

    #[test]
    fn common_governance_decoded_previews_show_parties_tokens_and_rates() {
        let proxy = address!("0x2222222222222222222222222222222222222222");
        let implementation = address!("0x3333333333333333333333333333333333333333");
        let account = address!("0x4444444444444444444444444444444444444444");
        let token = railgun_ui::governance_contracts(1)
            .expect("Ethereum governance")
            .governance_token;
        let usdt = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
        let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::from([
                (
                    (1, token.to_string().to_ascii_lowercase()),
                    wallet_ops::settings::EffectiveTokenInfo {
                        chain_id: 1,
                        token_address: token.to_string(),
                        symbol: "RAIL".to_owned(),
                        decimals: 18,
                        icon_path: None,
                        price_anchor: None,
                        built_in: false,
                    },
                ),
                (
                    (1, usdt.to_string().to_ascii_lowercase()),
                    wallet_ops::settings::EffectiveTokenInfo {
                        chain_id: 1,
                        token_address: usdt.to_string(),
                        symbol: "USDT".to_owned(),
                        decimals: 6,
                        icon_path: None,
                        price_anchor: None,
                        built_in: false,
                    },
                ),
                (
                    (1, usdc.to_string().to_ascii_lowercase()),
                    wallet_ops::settings::EffectiveTokenInfo {
                        chain_id: 1,
                        token_address: usdc.to_string(),
                        symbol: "USDC".to_owned(),
                        decimals: 6,
                        icon_path: None,
                        price_anchor: None,
                        built_in: false,
                    },
                ),
            ]),
        };
        let anchor_rates = TokenAnchorRateCache::new();
        let upgrade = proposal_decoded_preview(
            &DecodedProposalAction::ProxyUpgrade {
                proxy,
                implementation,
            },
            address!("0x1111111111111111111111111111111111111111"),
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(upgrade.verb, "Upgrade proxy");
        assert!(matches!(
            upgrade.details.as_slice(),
            [
                ProposalDecodedDetail::Party { role: proxy_role, address: proxy_address, .. },
                ProposalDecodedDetail::Party {
                    role: implementation_role,
                    address: implementation_address,
                    ..
                },
            ] if proxy_role == "PROXY"
                && *proxy_address == proxy
                && implementation_role == "IMPLEMENTATION"
                && *implementation_address == implementation
        ));

        let ownership = proposal_decoded_preview(
            &DecodedProposalAction::TransferOwnership { new_owner: account },
            address!("0x1111111111111111111111111111111111111111"),
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(ownership.verb, "Transfer ownership");
        assert!(ownership.hero.is_none());
        assert!(matches!(
            &ownership.details[0],
            ProposalDecodedDetail::Party { role, address, .. }
                if role == "NEW OWNER" && *address == account
        ));

        let pause = proposal_decoded_preview(
            &DecodedProposalAction::ProxyPause { proxy },
            address!("0x1111111111111111111111111111111111111111"),
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(pause.verb, "Pause proxy");
        assert!(matches!(
            &pause.details[0],
            ProposalDecodedDetail::Party { role, address, .. }
                if role == "PROXY" && *address == proxy
        ));

        let proxy_ownership = proposal_decoded_preview(
            &DecodedProposalAction::ProxyTransferOwnership {
                proxy,
                new_owner: account,
            },
            address!("0x1111111111111111111111111111111111111111"),
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(proxy_ownership.verb, "Transfer proxy ownership");
        assert!(matches!(
            proxy_ownership.details.as_slice(),
            [
                ProposalDecodedDetail::Party { role: proxy_role, address: proxy_address, .. },
                ProposalDecodedDetail::Party { role: owner_role, address: owner_address, .. },
            ] if proxy_role == "PROXY"
                && *proxy_address == proxy
                && owner_role == "NEW OWNER"
                && *owner_address == account
        ));

        let fees = proposal_decoded_preview(
            &DecodedProposalAction::ChangeFee {
                shield_fee: U256::from(10),
                unshield_fee: U256::from(10),
                nft_fee: U256::ZERO,
            },
            address!("0x5555555555555555555555555555555555555555"),
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(fees.verb, "Change protocol fees");
        assert!(fees.hero.is_none());
        assert!(matches!(
            fees.details.as_slice(),
            [
                ProposalDecodedDetail::Value { role: shield_role, value: shield_value, copy_value: Some(shield_copy), .. },
                ProposalDecodedDetail::Value { role: unshield_role, value: unshield_value, copy_value: Some(unshield_copy), .. },
                ProposalDecodedDetail::Value { role: nft_role, value: nft_value, copy_value: Some(nft_copy), .. },
            ] if shield_role == "SHIELD FEE"
                && shield_value == "0.10% (10 bp)"
                && shield_copy == "10"
                && unshield_role == "UNSHIELD FEE"
                && unshield_value == "0.10% (10 bp)"
                && unshield_copy == "10"
                && nft_role == "NFT FEE"
                && nft_value == "0 ETH (0 wei)"
                && nft_copy == "0"
        ));

        let mint = proposal_decoded_preview(
            &DecodedProposalAction::GovernanceMint {
                account,
                amount: U256::from(10_000_000_000_000_000_000_000_u128),
            },
            token,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(mint.verb, "Mint governance tokens");
        assert_eq!(
            mint.hero.as_ref().map(|hero| hero.rounded.as_str()),
            Some("10000 RAIL")
        );
        assert!(matches!(
            &mint.details[0],
            ProposalDecodedDetail::Party { role, address, .. }
                if role == "TO" && *address == account
        ));

        let interval = proposal_decoded_preview(
            &DecodedProposalAction::SetIntervalBP {
                new_interval_bp: U256::from(420),
            },
            Address::ZERO,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(interval.verb, "Set reward interval rate");
        assert!(matches!(
            &interval.details[0],
            ProposalDecodedDetail::Value {
                role,
                value,
                copy_value: Some(copy_value),
                monospace,
            } if role == "RATE"
                && value == "4.20% (420 bp)"
                && copy_value == "420"
                && !monospace
        ));

        let add_tokens = proposal_decoded_preview(
            &DecodedProposalAction::AddTokens {
                tokens: vec![usdt, usdc],
            },
            railgun_ui::governance_contracts(1)
                .expect("Ethereum governance")
                .governor_rewards,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(add_tokens.verb, "Add reward tokens");
        assert!(add_tokens.hero.is_none());
        assert!(matches!(
            add_tokens.details.as_slice(),
            [
                ProposalDecodedDetail::Party {
                    role: first_role,
                    address: first_address,
                    badge: Some(first_badge),
                    ..
                },
                ProposalDecodedDetail::Party {
                    role: second_role,
                    address: second_address,
                    badge: Some(second_badge),
                    ..
                },
            ] if first_role == "TOKEN"
                && *first_address == usdt
                && first_badge == "USDT"
                && second_role == "TOKEN"
                && *second_address == usdc
                && second_badge == "USDC"
        ));

        let caller = address!("0x64DA0892E8E24fECa6Eb5E3D8cbf2D9b6Fbe7598");
        let contract_address = address!("0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
        let known_selector = FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]);
        let grant = proposal_decoded_preview(
            &DecodedProposalAction::DelegatorSetPermission {
                caller,
                contract_address,
                selector: known_selector,
                permission: true,
            },
            railgun_ui::governance_contracts(1)
                .expect("Ethereum governance")
                .delegator,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(grant.verb, "Grant call permission");
        assert!(grant.hero.is_none());
        assert!(matches!(
            grant.details.as_slice(),
            [
                ProposalDecodedDetail::Party { role: caller_role, address: caller_value, .. },
                ProposalDecodedDetail::Party {
                    role: contract_role,
                    address: contract_value,
                    ..
                },
                ProposalDecodedDetail::Value {
                    role: function_role,
                    value,
                    copy_value: Some(copy_value),
                    monospace,
                },
            ] if caller_role == "CALLER"
                && *caller_value == caller
                && contract_role == "CONTRACT"
                && *contract_value == contract_address
                && function_role == "FUNCTION"
                && value == "setVerificationKey · 0x2ec0f359"
                && copy_value == "0x2ec0f359"
                && *monospace
        ));

        let unknown_selector = FixedBytes::from([0xaa, 0xbb, 0xcc, 0xdd]);
        let revoke = proposal_decoded_preview(
            &DecodedProposalAction::DelegatorSetPermission {
                caller,
                contract_address,
                selector: unknown_selector,
                permission: false,
            },
            Address::ZERO,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(revoke.verb, "Revoke call permission");
        assert!(matches!(
            &revoke.details[2],
            ProposalDecodedDetail::Value {
                role,
                value,
                copy_value: Some(copy_value),
                ..
            } if role == "FUNCTION" && value == "0xaabbccdd" && copy_value == "0xaabbccdd"
        ));
    }

    #[test]
    fn delegator_wildcard_preview_details_preserve_specific_counterpart() {
        let caller = address!("0x64DA0892E8E24fECa6Eb5E3D8cbf2D9b6Fbe7598");
        let contract = address!("0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
        let known_selector = FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]);
        let zero_selector = FixedBytes::from([0; 4]);
        let zero_copy = Address::ZERO.to_checksum(None);
        let contract_copy = contract.to_checksum(None);
        let registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        let anchor_rates = TokenAnchorRateCache::new();
        let cases = [
            (
                "zero contract only",
                Address::ZERO,
                known_selector,
                true,
                "Any contract · 0x0000…0000",
                zero_copy.as_str(),
                "setVerificationKey · 0x2ec0f359",
                "0x2ec0f359",
            ),
            (
                "zero selector only",
                contract,
                zero_selector,
                false,
                "",
                contract_copy.as_str(),
                "Any function · 0x00000000",
                "0x00000000",
            ),
            (
                "both zero",
                Address::ZERO,
                zero_selector,
                true,
                "Any contract · 0x0000…0000",
                zero_copy.as_str(),
                "Any function · 0x00000000",
                "0x00000000",
            ),
        ];

        for (
            label,
            contract_address,
            selector,
            wildcard_contract,
            expected_contract_value,
            expected_contract_copy,
            expected_function_value,
            expected_function_copy,
        ) in cases
        {
            let preview = proposal_decoded_preview(
                &DecodedProposalAction::DelegatorSetPermission {
                    caller,
                    contract_address,
                    selector,
                    permission: true,
                },
                Address::ZERO,
                U256::ZERO,
                1,
                &anchor_rates,
                &registry,
                None,
                &[],
                None,
            );
            assert!(
                matches!(
                    &preview.details[0],
                    ProposalDecodedDetail::Party {
                        role,
                        address,
                        copy_value,
                        ..
                    } if role == "CALLER"
                        && *address == caller
                        && copy_value == caller.to_checksum(None).as_str()
                ),
                "{label}: caller detail"
            );

            match (&preview.details[1], wildcard_contract) {
                (
                    ProposalDecodedDetail::Value {
                        role,
                        value,
                        copy_value: Some(copy_value),
                        monospace,
                    },
                    true,
                ) if role == "CONTRACT"
                    && value == expected_contract_value
                    && copy_value == expected_contract_copy
                    && !monospace => {}
                (
                    ProposalDecodedDetail::Party {
                        role,
                        address,
                        copy_value,
                        ..
                    },
                    false,
                ) if role == "CONTRACT"
                    && *address == contract
                    && copy_value == expected_contract_copy => {}
                _ => panic!("{label}: unexpected contract detail"),
            }

            assert!(
                matches!(
                    &preview.details[2],
                    ProposalDecodedDetail::Value {
                        role,
                        value,
                        copy_value: Some(copy_value),
                        monospace,
                    } if role == "FUNCTION"
                        && value == expected_function_value
                        && copy_value == expected_function_copy
                        && *monospace
                ),
                "{label}: function detail"
            );
        }
    }

    #[test]
    fn proposal_decoded_preview_uses_typed_token_and_party_presentation() {
        let target = address!("0x1111111111111111111111111111111111111111");
        let recipient = address!("0x2222222222222222222222222222222222222222");
        let registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::from([(
                (1, target.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: target.to_string(),
                    symbol: "TEST".to_owned(),
                    decimals: 6,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            )]),
        };
        let anchor_rates = TokenAnchorRateCache::new();
        let transfer = proposal_decoded_preview(
            &DecodedProposalAction::Erc20Transfer {
                recipient,
                amount: U256::from(15_109_211_424_u64),
            },
            target,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            Some(&[]),
        );
        let hero = transfer.hero.as_ref().expect("known token hero");
        assert_eq!(hero.rounded, "15109 TEST");
        assert!(hero.icon.is_none(), "custom icon-less symbols have no slot");
        assert!(hero.amount_copy_value.is_none());
        assert!(hero.context_copy_value.is_none());
        assert_eq!(transfer.details.len(), 1);
        assert!(matches!(
            &transfer.details[0],
            ProposalDecodedDetail::Party { role, address, badge, .. }
                if role == "TO" && *address == recipient && badge.is_none()
        ));

        let unknown_token = address!("0x9999999999999999999999999999999999999999");
        let unknown = proposal_decoded_preview(
            &DecodedProposalAction::Erc20Transfer {
                recipient,
                amount: U256::from(42),
            },
            unknown_token,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        let unknown_hero = unknown.hero.as_ref().expect("unknown token hero");
        assert!(unknown_hero.icon.is_none());
        assert_eq!(unknown_hero.rounded, "42 raw token units");
        assert_eq!(unknown_hero.amount_copy_value.as_deref(), Some("42"));
        let unknown_token_short = railgun_ui::short_address(&unknown_token);
        assert!(
            unknown_hero
                .context
                .as_deref()
                .is_some_and(|context| context.contains(unknown_token_short.as_str()))
        );
        let unknown_token_checksum = unknown_token.to_checksum(None);
        assert_eq!(
            unknown_hero.context_copy_value.as_deref(),
            Some(unknown_token_checksum.as_str())
        );
    }

    #[test]
    fn proposal_decoded_preview_adds_only_nonredundant_cached_usd_context() {
        let target = address!("0x1111111111111111111111111111111111111111");
        let recipient = address!("0x2222222222222222222222222222222222222222");
        let registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::from([(
                (1, target.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: target.to_string(),
                    symbol: "WBTC".to_owned(),
                    decimals: 8,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            )]),
        };
        let anchor_rates = TokenAnchorRateCache::new();
        anchor_rates.store_rate(1, target, U256::from(100_000_000_u64));
        anchor_rates.store_native_usd_rate(1, U256::from(13_194_600_000_u64));
        let priced = proposal_decoded_preview(
            &DecodedProposalAction::Erc20Transfer {
                recipient,
                amount: U256::from(100_000_000_u64),
            },
            target,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(
            priced
                .hero
                .as_ref()
                .and_then(|hero| hero.context.as_deref()),
            Some("≈ $13,194.60")
        );

        let stable_target = address!("0x3333333333333333333333333333333333333333");
        let stable_registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::from([(
                (1, stable_target.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: stable_target.to_string(),
                    symbol: "USDC".to_owned(),
                    decimals: 6,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            )]),
        };
        let stable_rates = TokenAnchorRateCache::new();
        stable_rates.store_rate(1, stable_target, U256::from(1_000_000_000_u64));
        stable_rates.store_native_usd_rate(1, U256::from(1_000_000_000_u64));
        let stable = proposal_decoded_preview(
            &DecodedProposalAction::Erc20Transfer {
                recipient,
                amount: U256::from(10_000_000_u64),
            },
            stable_target,
            U256::ZERO,
            1,
            &stable_rates,
            &stable_registry,
            None,
            &[],
            None,
        );
        assert!(
            stable
                .hero
                .as_ref()
                .is_some_and(|hero| hero.context.is_none())
        );

        let missing_rates = TokenAnchorRateCache::new();
        let missing = proposal_decoded_preview(
            &DecodedProposalAction::Erc20Transfer {
                recipient,
                amount: U256::from(100_000_000_u64),
            },
            target,
            U256::ZERO,
            1,
            &missing_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert!(
            missing
                .hero
                .as_ref()
                .is_some_and(|hero| hero.context.is_none())
        );
    }

    #[test]
    fn proposal_decoded_preview_preserves_direction_treasury_badges_and_approval_warning() {
        let target = address!("0x1111111111111111111111111111111111111111");
        let recipient = address!("0x2222222222222222222222222222222222222222");
        let from = address!("0x3333333333333333333333333333333333333333");
        let spender = address!("0x52908400098527886e0f7030069857d2e4169ee7");
        let registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::from([(
                (1, target.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: target.to_string(),
                    symbol: "TEST".to_owned(),
                    decimals: 2,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            )]),
        };
        let anchor_rates = TokenAnchorRateCache::new();
        let transfer_from = proposal_decoded_preview(
            &DecodedProposalAction::Erc20TransferFrom {
                from,
                to: recipient,
                amount: U256::from(1),
            },
            target,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert!(matches!(
            transfer_from.details.as_slice(),
            [
                ProposalDecodedDetail::Party { role: from_role, address: from_address, .. },
                ProposalDecodedDetail::Connector,
                ProposalDecodedDetail::Party { role: to_role, address: to_address, .. },
            ] if from_role == "FROM" && *from_address == from && to_role == "TO" && *to_address == recipient
        ));

        let treasury = railgun_ui::governance_treasury(1).expect("ethereum treasury metadata");
        let treasury_transfer = proposal_decoded_preview(
            &DecodedProposalAction::TreasuryTransferErc20 {
                token: target,
                to: recipient,
                amount: U256::from(1),
            },
            treasury,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert!(matches!(
            &treasury_transfer.details[0],
            ProposalDecodedDetail::Party { role, address, badge: Some(badge), .. }
                if role == "FROM" && *address == treasury && badge == "Treasury"
        ));

        let bounded = proposal_decoded_preview(
            &DecodedProposalAction::Erc20Approve {
                spender,
                amount: U256::from(1),
            },
            target,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert!(bounded.unlimited_warning.is_none());
        assert!(!bounded.hero.as_ref().expect("approval hero").danger);

        let unlimited = proposal_decoded_preview(
            &DecodedProposalAction::Erc20Approve {
                spender,
                amount: U256::MAX,
            },
            target,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        let unlimited_hero = unlimited.hero.as_ref().expect("approval hero");
        assert_eq!(unlimited_hero.rounded, "Unlimited TEST");
        assert!(unlimited_hero.danger);
        let expected_warning = format!(
            "Unlimited allowance: {} can keep spending this token until the allowance is revoked.",
            spender.to_checksum(None)
        );
        assert_eq!(
            unlimited.unlimited_warning.as_deref(),
            Some(expected_warning.as_str())
        );
        assert!(matches!(
            &unlimited.details[0],
            ProposalDecodedDetail::Party { role, address, .. }
                if role == "SPENDER" && *address == spender
        ));
    }

    #[test]
    fn proposal_decoded_preview_covers_wrapping_and_role_shells() {
        let target = address!("0x1111111111111111111111111111111111111111");
        let account = address!("0x2222222222222222222222222222222222222222");
        let registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        let anchor_rates = TokenAnchorRateCache::new();
        let wrapped = proposal_decoded_preview(
            &DecodedProposalAction::WrappedDeposit,
            target,
            U256::from(1_000_000_000_000_000_000_u128),
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        let wrapped_hero = wrapped.hero.as_ref().expect("wrap hero");
        assert_eq!(wrapped.verb, "Wrap");
        assert_eq!(wrapped_hero.rounded, "1 ETH");
        assert_eq!(wrapped_hero.context.as_deref(), Some("wrapped into WETH"));

        let role = B256::repeat_byte(7);
        let role_preview = proposal_decoded_preview(
            &DecodedProposalAction::TreasuryGrantRole { role, account },
            target,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert_eq!(role_preview.details.len(), 2);
        assert!(matches!(
            &role_preview.details[0],
            ProposalDecodedDetail::Value { role: label, value, .. }
                if label == "ROLE" && value == &proposal_role_label(role)
        ));
        assert!(matches!(
            &role_preview.details[1],
            ProposalDecodedDetail::Party { role, address, .. }
                if role == "ACCOUNT" && *address == account
        ));
    }

    #[test]
    fn proposal_decoded_preview_retains_exact_address_copy_fidelity() {
        let first = address!("0x123400000000000000000000000000000000abcd");
        let second = address!("0x123411111111111111111111111111111111abcd");
        assert_eq!(
            railgun_ui::short_address(&first),
            railgun_ui::short_address(&second)
        );
        let registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        let anchor_rates = TokenAnchorRateCache::new();
        let preview = proposal_decoded_preview(
            &DecodedProposalAction::TreasuryInitialize { owner: first },
            Address::ZERO,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert!(matches!(
            &preview.details[0],
            ProposalDecodedDetail::Party { address, copy_value, .. }
                if *address == first && copy_value == &first.to_checksum(None)
        ));
        let other = proposal_decoded_preview(
            &DecodedProposalAction::TreasuryInitialize { owner: second },
            Address::ZERO,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert!(matches!(
            &other.details[0],
            ProposalDecodedDetail::Party { address, copy_value, .. }
                if *address == second && copy_value == &second.to_checksum(None)
        ));
    }

    #[test]
    fn proposal_decodes_all_treasury_writes_only_at_known_target() {
        let treasury = railgun_ui::governance_treasury(1).expect("ethereum treasury metadata");
        let other = address!("0x1111111111111111111111111111111111111111");
        let token = address!("0x2222222222222222222222222222222222222222");
        let account = address!("0x3333333333333333333333333333333333333333");
        let role = B256::repeat_byte(7);
        let calls = [
            (
                ProposalTreasury::transferERC20Call {
                    token,
                    to: account,
                    amount: U256::from(1),
                }
                .abi_encode(),
                DecodedProposalAction::TreasuryTransferErc20 {
                    token,
                    to: account,
                    amount: U256::from(1),
                },
            ),
            (
                ProposalTreasury::transferETHCall {
                    to: account,
                    amount: U256::from(2),
                }
                .abi_encode(),
                DecodedProposalAction::TreasuryTransferEth {
                    to: account,
                    amount: U256::from(2),
                },
            ),
            (
                ProposalTreasury::initializeTreasuryCall { owner: account }.abi_encode(),
                DecodedProposalAction::TreasuryInitialize { owner: account },
            ),
            (
                ProposalTreasury::grantRoleCall { role, account }.abi_encode(),
                DecodedProposalAction::TreasuryGrantRole { role, account },
            ),
            (
                ProposalTreasury::revokeRoleCall { role, account }.abi_encode(),
                DecodedProposalAction::TreasuryRevokeRole { role, account },
            ),
            (
                ProposalTreasury::renounceRoleCall { role, account }.abi_encode(),
                DecodedProposalAction::TreasuryRenounceRole { role, account },
            ),
        ];
        let arbitrum_treasury =
            railgun_ui::governance_treasury(42161).expect("arbitrum treasury metadata");
        assert_eq!(
            decode_proposal_action(42161, arbitrum_treasury, &calls[0].0, None, None),
            Some(calls[0].1.clone())
        );
        for (calldata, expected) in calls {
            assert_eq!(
                decode_proposal_action(1, treasury, &calldata, None, None),
                Some(expected)
            );
            assert_eq!(
                decode_proposal_action(1, other, &calldata, None, None),
                None
            );
        }
    }

    #[test]
    fn proposal_role_labels_cover_known_treasury_roles() {
        assert_eq!(proposal_role_label(B256::ZERO), "DEFAULT_ADMIN_ROLE");
        assert_eq!(
            proposal_role_label(alloy::primitives::keccak256(b"TRANSFER_ROLE")),
            "TRANSFER_ROLE"
        );
        let unknown = B256::repeat_byte(7);
        assert_eq!(proposal_role_label(unknown), unknown.to_string());
    }

    #[test]
    fn proposal_decodes_sender_writes_at_any_target_and_labels_decoded_actions() {
        let first_target = address!("0x1111111111111111111111111111111111111111");
        let second_target = address!("0x2222222222222222222222222222222222222222");
        let executor = address!("0x3333333333333333333333333333333333333333");
        let empty_registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        let ready_task = ProposalOpStackSender::readyTaskCall {
            taskId: U256::from(1),
        }
        .abi_encode();
        let ready_task_decoded = DecodedProposalAction::OpStackReadyTask {
            task_id: U256::from(1),
        };
        assert_eq!(
            decode_proposal_action(1, first_target, &ready_task, None, None),
            Some(ready_task_decoded.clone())
        );
        assert_eq!(
            proposal_action_target_label(
                1,
                first_target,
                &empty_registry,
                None,
                Some(&ready_task_decoded),
            ),
            Some("OpStack sender".to_owned())
        );

        let treasury = railgun_ui::governance_treasury(1).expect("mainnet treasury metadata");
        assert_eq!(
            proposal_action_target_label(
                1,
                treasury,
                &empty_registry,
                None,
                Some(&ready_task_decoded),
            ),
            Some("Treasury".to_owned())
        );

        let delegator = railgun_ui::governance_contracts(1)
            .expect("mainnet governance metadata")
            .delegator;
        assert_eq!(
            proposal_action_target_label(
                1,
                delegator,
                &empty_registry,
                None,
                Some(&ready_task_decoded),
            ),
            Some("Delegator".to_owned())
        );

        let set_executor = ProposalOpStackSender::setExecutorL2Call { executor }.abi_encode();
        let set_executor_decoded = DecodedProposalAction::OpStackSetExecutorL2 { executor };
        assert_eq!(
            decode_proposal_action(42161, second_target, &set_executor, None, None),
            Some(set_executor_decoded.clone())
        );
        assert_eq!(
            proposal_action_target_label(
                42161,
                second_target,
                &empty_registry,
                None,
                Some(&set_executor_decoded),
            ),
            Some("OpStack sender".to_owned())
        );

        let transfer_ownership =
            ProposalOwnable::transferOwnershipCall { newOwner: executor }.abi_encode();
        assert_eq!(&transfer_ownership[..4], &[0xf2, 0xfd, 0xe3, 0x8b]);
        assert_eq!(
            decode_proposal_action(1, first_target, &transfer_ownership, None, None),
            Some(DecodedProposalAction::TransferOwnership {
                new_owner: executor
            })
        );
        assert_eq!(
            proposal_action_target_label(
                1,
                first_target,
                &empty_registry,
                None,
                Some(&DecodedProposalAction::TransferOwnership {
                    new_owner: executor
                }),
            ),
            None
        );
        let renounce_ownership = ProposalOwnable::renounceOwnershipCall {}.abi_encode();
        assert_eq!(
            decode_proposal_action(42161, second_target, &renounce_ownership, None, None),
            None
        );
    }

    #[test]
    fn proposal_decodes_wrapped_native_calls_only_at_configured_target() {
        let wrapped = address!("0x1111111111111111111111111111111111111111");
        let other = address!("0x2222222222222222222222222222222222222222");
        let deposit = ProposalWrappedNative::depositCall {}.abi_encode();
        assert_eq!(
            decode_proposal_action(1, wrapped, &deposit, Some(wrapped), None),
            Some(DecodedProposalAction::WrappedDeposit)
        );
        assert_eq!(
            decode_proposal_action(1, other, &deposit, Some(wrapped), None),
            None
        );
        let amount = U256::from(9);
        let withdraw = ProposalWrappedNative::withdrawCall { amount }.abi_encode();
        assert_eq!(
            decode_proposal_action(1, wrapped, &withdraw, Some(wrapped), None),
            Some(DecodedProposalAction::WrappedWithdraw { amount })
        );
    }

    #[test]
    fn proposal_keeps_unknown_malformed_and_trailing_calls_undecoded() {
        let target = address!("0x1111111111111111111111111111111111111111");
        let mut trailing = ProposalErc20::transferCall {
            recipient: target,
            amount: U256::from(1),
        }
        .abi_encode();
        trailing.push(0);
        assert_eq!(
            decode_proposal_action(1, target, &trailing, None, None),
            None
        );
        assert_eq!(
            decode_proposal_action(1, target, &[0xa9, 0x05], None, None),
            None
        );
        assert_eq!(
            decode_proposal_action(1, target, &[0xff; 36], None, None),
            None
        );
    }

    #[test]
    fn action_control_ids_are_unique_per_proposal_ordinal_and_control() {
        let identity = test_proposal(7, resolved_document()).identity();
        let first = proposal_action_id(&identity, 0, "calldata", "clipboard");
        let second = proposal_action_id(&identity, 1, "calldata", "clipboard");
        let display = proposal_action_id(&identity, 0, "calldata", "display");
        assert_ne!(first, second);
        assert_ne!(first, display);
    }

    #[test]
    fn refresh_prunes_action_expansion_only_for_removed_ordinals() {
        let mut state = ProposalsState::new(1);
        let mut source = test_proposal(7, resolved_document());
        source.proposal.actions = vec![
            GovernanceProposalAction {
                call_contract: Address::ZERO,
                calldata: alloy::primitives::Bytes::from(vec![1]),
                value: U256::ZERO,
            },
            GovernanceProposalAction {
                call_contract: Address::ZERO,
                calldata: alloy::primitives::Bytes::from(vec![2]),
                value: U256::ZERO,
            },
        ];
        let identity = source.identity();
        state.pages.insert(
            0,
            ProposalsPage {
                rows: Arc::new(vec![source]),
            },
        );
        state.expanded_calldata.extend([
            ProposalActionIdentity {
                proposal: identity.clone(),
                ordinal: 0,
            },
            ProposalActionIdentity {
                proposal: identity.clone(),
                ordinal: 1,
            },
        ]);
        let mut refreshed = test_proposal(7, ProposalDocumentState::Pending);
        refreshed.proposal.actions = vec![GovernanceProposalAction {
            call_contract: Address::ZERO,
            calldata: alloy::primitives::Bytes::from(vec![1]),
            value: U256::ZERO,
        }];
        state.replace_refreshed_page(0, 0, vec![refreshed]);
        assert!(state.expanded_calldata.contains(&ProposalActionIdentity {
            proposal: identity.clone(),
            ordinal: 0,
        }));
        assert!(!state.expanded_calldata.contains(&ProposalActionIdentity {
            proposal: identity,
            ordinal: 1,
        }));
    }

    #[test]
    fn prefetch_selects_only_the_immediately_next_page() {
        assert_eq!(next_proposal_page(3, 10), Some(4));
        assert_eq!(next_proposal_page(8, 10), Some(9));
        assert_eq!(next_proposal_page(9, 10), None);

        let mut state = ProposalsState::new(1);
        state.current_page = 3;
        state.total_pages = 5;
        state.pages.insert(
            3,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, resolved_document())]),
            },
        );
        assert_eq!(state.prefetch_candidate(3), Some(4));

        state.pages.insert(
            3,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, ProposalDocumentState::Pending)]),
            },
        );
        assert_eq!(state.prefetch_candidate(3), None);

        state.current_page = 4;
        state.pages.insert(
            4,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, resolved_document())]),
            },
        );
        assert_eq!(state.prefetch_candidate(4), None);
    }

    #[test]
    fn rail_amounts_preserve_18_decimal_scaling() {
        let amount = U256::from(1_500_000_000_000_000_000u128);
        assert_eq!(format_compact_rail_amount_with_unit(amount), "1.5 RAIL");

        let rounded = U256::from(12_345_600_000_000_000_000u128);
        assert_eq!(format_compact_rail_amount_with_unit(rounded), "12.35 RAIL");
        assert_eq!(
            format_compact_rail_amount(U256::from(27_000u128) * U256::from(10).pow(U256::from(18))),
            "27K"
        );
        assert_eq!(
            format_compact_rail_amount(
                U256::from(242_879u128) * U256::from(10).pow(U256::from(18))
            ),
            "242.8K"
        );
        assert_eq!(
            format_compact_rail_amount_with_unit(
                U256::from(2_043_001u128) * U256::from(10).pow(U256::from(18))
            ),
            "2.04M RAIL"
        );
    }

    #[test]
    fn vote_split_uses_raw_base_unit_votes() {
        let point_nine_rail = U256::from(900_000_000_000_000_000u128);
        let point_eight_rail = U256::from(800_000_000_000_000_000u128);
        let one_rail = U256::from(1_000_000_000_000_000_000u128);

        assert_eq!(vote_split(U256::ZERO, U256::ZERO), None);
        assert_eq!(vote_split(point_nine_rail, one_rail), Some(473));
        assert_eq!(vote_split(point_nine_rail, point_eight_rail), Some(529));
    }

    #[test]
    fn chain_time_anchor_current_at_uses_sampled_time() {
        let anchor = super::ChainTimeAnchor {
            chain_time: U256::from(100),
            captured_at: Instant::now(),
        };
        assert_eq!(anchor.current_at(0), U256::from(100));
        assert_eq!(anchor.current_at(60), U256::from(160));
    }

    #[test]
    fn list_deadline_tracks_each_voting_phase() {
        let deadlines = GovernanceProposalDeadlines {
            sponsorship: U256::ZERO,
            voting_start: Some(U256::from(10)),
            yay_end: Some(U256::from(20)),
            nay_end: Some(U256::from(30)),
            execution_start: Some(U256::from(40)),
            execution_end: Some(U256::from(50)),
        };
        let mut status = GovernanceProposalStatus {
            stage: GovernanceProposalStage::VotingOpen,
            deadlines,
            quorum_basis: wallet_ops::GovernanceQuorumBasis::AffirmativeOnly,
            quorum: U256::ZERO,
            quorum_progress: U256::ZERO,
            quorum_met: false,
            majority: wallet_ops::GovernanceMajorityResult::Tie,
        };
        assert_eq!(
            list_voting_deadline(&status),
            Some((U256::from(20), "Yay voting ends in"))
        );
        status.stage = GovernanceProposalStage::NayOnlyVoting;
        assert_eq!(
            list_voting_deadline(&status),
            Some((U256::from(30), "Nay voting ends in"))
        );
    }

    #[test]
    fn timeline_deadline_boundaries_match_protocol_semantics() {
        assert!(timeline_sponsorship_completed(
            true,
            U256::from(9),
            U256::from(10)
        ));
        assert!(!timeline_sponsorship_completed(
            false,
            U256::from(9),
            U256::from(10)
        ));
        assert!(timeline_sponsorship_completed(
            false,
            U256::from(10),
            U256::from(10)
        ));

        for milestone in [
            TimelineDeadline::SponsorshipClose,
            TimelineDeadline::YayVotingEnd,
            TimelineDeadline::NayVotingEnd,
            TimelineDeadline::ExecutionClose,
        ] {
            assert!(timeline_deadline_completed(
                milestone,
                U256::from(10),
                U256::from(10)
            ));
            assert!(!timeline_deadline_completed(
                milestone,
                U256::from(9),
                U256::from(10)
            ));
        }
        for milestone in [
            TimelineDeadline::VotingOpen,
            TimelineDeadline::ExecutionOpen,
        ] {
            assert!(!timeline_deadline_completed(
                milestone,
                U256::from(10),
                U256::from(10)
            ));
            assert!(timeline_deadline_completed(
                milestone,
                U256::from(11),
                U256::from(10)
            ));
        }
    }

    #[test]
    fn canceled_page_work_token_rejects_reused_page_ownership() {
        let mut state = ProposalsState::new(1);
        state.prefetch_page = Some(1);
        let old_prefetch_token = state.prefetch_token;
        assert!(state.owns_page_work(1, true, old_prefetch_token));
        state.cancel_prefetch();
        state.prefetch_page = Some(1);
        assert!(!state.owns_page_work(1, true, old_prefetch_token));
        assert!(state.owns_page_work(1, true, state.prefetch_token));

        state.current_page = 1;
        let old_active_token = state.active_page_token;
        state.cancel_active_page(1);
        assert!(!state.owns_page_work(1, false, old_active_token));
        assert!(state.owns_page_work(1, false, state.active_page_token));
    }

    #[test]
    fn take_cleanup_invalidates_work_and_preserves_cached_ui_state() {
        let mut state = ProposalsState::new(42);
        state.generation = 7;
        state.request_generation = 11;
        state.active_page_token = 13;
        state.prefetch_token = 17;
        state.checked = true;
        state.overview = Some(GovernanceOverview {
            chain_id: 42,
            v2: GovernanceContractSummary {
                version: GovernanceContractVersion::V2,
                address: Address::ZERO,
                proposal_count: U256::from(10),
                rules: GovernanceContractRules {
                    sponsor_threshold: U256::from(2),
                    quorum: U256::from(1),
                    sponsor_window: U256::from(1),
                    voting_start_offset: U256::from(1),
                    voting_yay_end_offset: U256::from(2),
                    voting_nay_end_offset: U256::from(3),
                    execution_start_offset: U256::from(4),
                    execution_end_offset: U256::from(5),
                },
            },
            v1: None,
        });
        state.total_pages = 2;
        state.current_page = 1;
        state.pages.insert(
            1,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(3, resolved_document())]),
            },
        );
        state.loading_pages.extend([1, 2]);
        state.hydrating_pages.insert(1);
        state.prefetch_page = Some(2);
        state.loading = true;
        state.refreshing = true;
        state.error = Some(Arc::from("cached error"));
        let identity = test_proposal(3, resolved_document()).identity();
        state.selected = Some(ProposalSelection {
            page: 1,
            identity,
            tab: ProposalDetailTab::Actions,
            participation_expanded: false,
        });

        let old_generation = state.generation;
        let old_request_generation = state.request_generation;
        let old_active_token = state.active_page_token;
        let old_prefetch_token = state.prefetch_token;
        assert!(state.owns_page_work(1, false, old_active_token));
        assert!(state.owns_page_work(2, true, old_prefetch_token));
        let cached_rows = state.pages[&1].rows.clone();
        let overview = state.overview.clone();
        let selected = state.selected.clone();
        let document_semaphore = Arc::clone(&state.document_semaphore);
        let cleanup = state.take_cleanup();

        assert_eq!(state.generation, old_generation.wrapping_add(1));
        assert_eq!(
            state.request_generation,
            old_request_generation.wrapping_add(1)
        );
        assert_eq!(state.active_page_token, old_active_token.wrapping_add(1));
        assert_eq!(state.prefetch_token, old_prefetch_token.wrapping_add(1));
        assert!(!state.owns_page_work(1, false, old_active_token));
        assert!(!state.owns_page_work(2, true, old_prefetch_token));
        assert!(state.loading_pages.is_empty());
        assert!(state.hydrating_pages.is_empty());
        assert_eq!(state.prefetch_page, None);
        assert!(!state.loading);
        assert!(!state.refreshing);
        assert_eq!(state.chain_id, 42);
        assert!(state.checked);
        assert_eq!(state.overview, overview);
        assert_eq!(state.total_pages, 2);
        assert_eq!(state.current_page, 1);
        assert!(Arc::ptr_eq(&state.pages[&1].rows, &cached_rows));
        assert!(Arc::ptr_eq(&state.document_semaphore, &document_semaphore));
        assert_eq!(state.error.as_deref(), Some("cached error"));
        assert_eq!(
            state.selected.as_ref().map(|selected| selected.page),
            selected.as_ref().map(|selected| selected.page)
        );
        assert_eq!(
            state.selected.as_ref().map(|selected| &selected.identity),
            selected.as_ref().map(|selected| &selected.identity)
        );
        assert!(cleanup.tasks.is_empty());
    }

    #[test]
    fn pending_hydration_selection_keeps_only_pending_rows() {
        let mut state = ProposalsState::new(1);
        state.pages.insert(
            2,
            ProposalsPage {
                rows: Arc::new(vec![
                    test_proposal(1, ProposalDocumentState::Pending),
                    test_proposal(2, resolved_document()),
                ]),
            },
        );

        let rows = state
            .pending_document_rows(2)
            .expect("cached page should be selectable");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].has_pending_document());
        assert!(state.pending_document_rows(3).is_none());
    }

    #[test]
    fn pending_rows_distinguish_terminal_and_uncached_pages() {
        let mut state = ProposalsState::new(1);
        state.pages.insert(
            2,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, ProposalDocumentState::Pending)]),
            },
        );
        state.pages.insert(
            4,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(2, resolved_document())]),
            },
        );

        assert!(!state.page_is_terminal(2));
        assert!(state.page_is_terminal(4));
        assert!(
            state
                .pending_document_rows(4)
                .is_some_and(|rows| rows.is_empty())
        );
        assert!(state.pending_document_rows(6).is_none());
    }

    #[test]
    fn manual_refresh_keeps_active_page_and_retries_only_unavailable_documents() {
        let mut state = ProposalsState::new(1);
        state.current_page = 3;
        state.pages.insert(
            1,
            ProposalsPage {
                rows: Arc::new(vec![test_proposal(1, resolved_document())]),
            },
        );
        state.pages.insert(
            2,
            ProposalsPage {
                rows: Arc::new(vec![
                    test_proposal(10, resolved_document_with_title("cached destination")),
                    test_proposal(12, resolved_document()),
                ]),
            },
        );
        state.pages.insert(
            3,
            ProposalsPage {
                rows: Arc::new(vec![
                    test_proposal(10, resolved_document()),
                    test_proposal(
                        11,
                        ProposalDocumentState::Resolved {
                            document: GovernanceDocument {
                                title: "Document unavailable".to_string(),
                                description: String::new(),
                                available: false,
                            },
                            presentation: ProposalPresentation::RawParseFallback(
                                inert_raw_fallback_source(""),
                            ),
                        },
                    ),
                ]),
            },
        );
        state.loading_pages.extend([1, 2, 3, 4]);
        state.hydrating_pages.extend([2, 3]);
        let selected_identity = test_proposal(10, resolved_document()).identity();
        state.selected = Some(ProposalSelection {
            page: 3,
            identity: selected_identity.clone(),
            tab: ProposalDetailTab::Actions,
            participation_expanded: false,
        });
        let expanded_identity = ProposalActionIdentity {
            proposal: selected_identity.clone(),
            ordinal: 0,
        };
        state.expanded_calldata.insert(expanded_identity.clone());
        let detail_offset = gpui::point(gpui::px(9.0), gpui::px(-21.0));
        state.detail_scroll_handle.set_offset(detail_offset);

        state.prepare_manual_refresh();
        assert!(state.pages[&3].rows[0].document().is_some());
        assert!(state.pages[&3].rows[1].has_pending_document());

        state.replace_refreshed_page(3, 2, {
            let mut selected_refreshed = test_proposal(10, ProposalDocumentState::Pending);
            selected_refreshed
                .proposal
                .actions
                .push(GovernanceProposalAction {
                    call_contract: Address::ZERO,
                    calldata: alloy::primitives::Bytes::from(vec![1]),
                    value: U256::ZERO,
                });
            vec![
                selected_refreshed,
                test_proposal(11, ProposalDocumentState::Pending),
                test_proposal(12, ProposalDocumentState::Pending),
            ]
        });

        assert_eq!(state.current_page, 2);
        assert_eq!(state.pages.len(), 1);
        assert!(state.pages.contains_key(&2));
        assert!(!state.pages.contains_key(&1));
        assert!(!state.pages.contains_key(&3));
        assert!(state.loading_pages.is_empty());
        assert!(state.hydrating_pages.is_empty());
        assert_eq!(
            state.selected.as_ref().map(|selected| &selected.identity),
            Some(&selected_identity)
        );
        assert_eq!(
            state.selected.as_ref().map(|selected| selected.page),
            Some(2)
        );
        assert_eq!(
            state.selected.as_ref().map(|selected| selected.tab),
            Some(ProposalDetailTab::Actions)
        );
        assert_eq!(state.detail_scroll_handle.offset(), detail_offset);
        assert!(state.expanded_calldata.contains(&expanded_identity));
        assert_eq!(
            state.pages[&2].rows[0]
                .document()
                .map(|document| document.title.as_str()),
            Some("resolved")
        );
        assert!(state.pages[&2].rows[1].has_pending_document());
        assert!(state.pages[&2].rows[2].has_pending_document());
    }

    #[tokio::test]
    async fn proposal_cleanup_aborts_and_awaits_held_workers() {
        struct DropSentinel(Arc<AtomicBool>);
        impl Drop for DropSentinel {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let active_dropped = Arc::new(AtomicBool::new(false));
        let prefetch_dropped = Arc::new(AtomicBool::new(false));
        let lifecycle_dropped = Arc::new(AtomicBool::new(false));
        let active_sentinel = DropSentinel(Arc::clone(&active_dropped));
        let prefetch_sentinel = DropSentinel(Arc::clone(&prefetch_dropped));
        let lifecycle_sentinel = DropSentinel(Arc::clone(&lifecycle_dropped));
        let active_worker = tokio::spawn(async move {
            let _sentinel = active_sentinel;
            std::future::pending::<()>().await;
        });
        let prefetch_worker = tokio::spawn(async move {
            let _sentinel = prefetch_sentinel;
            std::future::pending::<()>().await;
        });
        let lifecycle_worker = tokio::spawn(async move {
            let _sentinel = lifecycle_sentinel;
            std::future::pending::<()>().await;
        });
        let mut state = ProposalsState::new(1);
        state.task_tracker.track(lifecycle_worker);
        state.active_page_task_tracker.track(active_worker);
        state.prefetch_task_tracker.track(prefetch_worker);
        let cleanup = state.take_cleanup();
        cleanup
            .shutdown()
            .await
            .expect("cleanup should cancel normally");
        assert!(active_dropped.load(Ordering::Acquire));
        assert!(prefetch_dropped.load(Ordering::Acquire));
        assert!(lifecycle_dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn proposal_cleanup_drops_in_flight_document_futures() {
        struct DropSentinel(Arc<AtomicBool>);
        impl Drop for DropSentinel {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let futures = futures_util::stream::FuturesUnordered::new();
        let future_dropped = Arc::clone(&dropped);
        let future_started = Arc::clone(&started);
        let future_notify = Arc::clone(&notify);
        futures.push(async move {
            future_started.store(true, Ordering::Release);
            future_notify.notify_one();
            let _sentinel = DropSentinel(future_dropped);
            std::future::pending::<DocumentCompletion>().await
        });
        let (completion_tx, _completion_rx) = tokio::sync::mpsc::unbounded_channel();
        let worker = tokio::spawn(send_document_completions(futures, completion_tx));
        notify.notified().await;
        assert!(started.load(Ordering::Acquire));
        assert!(!dropped.load(Ordering::Acquire));

        let mut state = ProposalsState::new(1);
        state.active_page_task_tracker.track(worker);
        let cleanup = state.take_cleanup();
        cleanup
            .shutdown()
            .await
            .expect("cleanup should cancel normally");
        assert!(dropped.load(Ordering::Acquire));
    }
}
