use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::network::TransactionBuilder as _;
use alloy::primitives::U256;
use alloy::rpc::types::TransactionRequest;
use chrono::{Local, TimeZone as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Pixels, SharedString, Styled, Window, div, img, px, rgb,
};
use gpui_component::{
    Disableable, IconName, IndexPath, Sizable, WindowExt,
    alert::Alert,
    badge::Badge,
    button::ButtonVariants,
    input::InputState,
    scroll::ScrollableElement,
    select::{SearchableVec, Select, SelectItem, SelectState},
};
use railgun_ui::{chain_icon_asset_path, chain_name, short_address};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_button_base, app_input, app_muted_text, app_strong_text};
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{
    HardwareTrezorPinMatrixProvider, HttpContext, PublicActionSessionEvent,
    PublicActionSessionEventSender, PublicBalanceSnapshot, TokenAnchorRateCache,
    WALLETCONNECT_DEFAULT_PROJECT_ID, WalletConnectError, WalletConnectEvmTransaction,
    WalletConnectHardwareTypedDataCapabilityRequest, WalletConnectJsonRpcRequest,
    WalletConnectJsonRpcResponse, WalletConnectLifecycleRequestOutcome,
    WalletConnectNamespaceAccountSupport, WalletConnectNamespaceNegotiation,
    WalletConnectPairingUri, WalletConnectParsedRequest, WalletConnectPendingRequest,
    WalletConnectPersonalSignRequest, WalletConnectProposalRejectionReason,
    WalletConnectRelayClient, WalletConnectRelayClientAuth, WalletConnectRelayConfig,
    WalletConnectRelayRpc, WalletConnectRelaySocket, WalletConnectRelayStep,
    WalletConnectRelaySubscriptionPayload, WalletConnectRequestErrorKind,
    WalletConnectSendTransactionRequest, WalletConnectSessionProposal,
    WalletConnectSupportedMethod, WalletConnectTypedDataSignRequest,
    approve_walletconnect_session_with_account_support, build_walletconnect_disconnect_plan,
    build_walletconnect_jsonrpc_error, build_walletconnect_session_event,
    decode_walletconnect_message, decode_walletconnect_session_proposal,
    encode_walletconnect_message, handle_walletconnect_lifecycle_request,
    hardware::HardwareTypedDataSigningMode,
    is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required,
    negotiate_walletconnect_namespaces_with_account_support, parse_walletconnect_session_request,
    reject_walletconnect_session_proposal,
    settings::EffectiveChainConfig,
    start_walletconnect_pairing, submit_walletconnect_send_transaction,
    validate_walletconnect_session_request_with_account_support,
    vault::{
        DesktopVaultStore, DesktopViewSession, HardwareProfileSession, PublicAccountMetadata,
        PublicAccountSource, PublicAccountStatus, WalletConnectPeerMetadata,
        WalletConnectRelayIdentity, WalletConnectSessionAccountResolution,
        WalletConnectSessionLifecycleState, WalletConnectSessionRecord,
    },
    walletconnect_hardware_typed_data_hash_fallback_confirmation_session,
    walletconnect_probe_hardware_typed_data_signing_mode, walletconnect_sign_personal_message,
    walletconnect_sign_typed_data,
};
use zeroize::Zeroizing;

use crate::assets::WALLETCONNECT_ICON_PATH;
use crate::root::gas_fee::{Eip1559GasFeeEditTarget, Eip1559GasFeeEditorState};

use super::public_action::{
    PublicActionStepStatus, public_action_step_color, render_public_action_step_marker,
};
use super::public_balances::public_account_usd_total_label_for_chain;
use super::spend_authorization::{
    SpendAuthorizationIntent, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
};
use super::utxo::short_hash;
use super::{
    WalletRoot, app_step_row, app_stepper_container, dialog_content_max_height,
    format_report_chain, new_text_input, rgb_with_alpha, scrollable_dialog_content,
    secondary_dialog_content_width,
};

mod account_select;
mod fee;
mod helpers;
mod intent;
mod relay;
mod render;
mod requests;
mod root;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(in crate::root) use account_select::{
    normalized_walletconnect_account_uuid, walletconnect_account_matches_search,
    walletconnect_account_select_items,
};
pub(in crate::root) use render::walletconnect_logo_with_presence;

use fee::WalletConnectFeeState;
pub(super) use fee::WalletConnectReviewedFeeProjection;
use render::walletconnect_approval_progress_steps;

const WALLETCONNECT_RELAY_TTL_SECS: u64 = 300;
const WALLETCONNECT_SESSION_DELETE_TTL_SECS: u64 = 86_400;
const WALLETCONNECT_SESSION_PING_TTL_SECS: u64 = 30;
const WC_SESSION_REQUEST_RESPONSE_TAG: u32 = 1109;
const WC_SESSION_EVENT_REQUEST_TAG: u32 = 1110;
const WC_SESSION_DELETE_RESPONSE_TAG: u32 = 1113;
const WC_SESSION_PING_RESPONSE_TAG: u32 = 1115;
const WC_SESSION_PROPOSE_REJECT_TAG: u32 = 1120;
const WALLETCONNECT_SUBSCRIPTION_PUSH_WAIT_SECS: u64 = 15;
const WALLETCONNECT_FETCH_MAX_PAGES: usize = 16;
const WALLETCONNECT_HANDLED_REQUEST_KEY_LIMIT: usize = 1024;
const WALLETCONNECT_REFRESHED_STATUS: &str = "WalletConnect sessions refreshed.";
const WALLETCONNECT_RELAY_ID_ENTROPY_FACTOR: u64 = 1_000_000;
const WALLETCONNECT_BLUE: u32 = 0x3396ff;
const WALLETCONNECT_BLUE_HOVER: u32 = 0x4aa3ff;
static WALLETCONNECT_RELAY_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) struct WalletConnectAttentionTransition {
    pub(in crate::root) sync_badge_count: bool,
    pub(in crate::root) request_attention: bool,
    pub(in crate::root) clear_attention: bool,
}

pub(in crate::root) const fn walletconnect_attention_count(
    has_pending_proposal: bool,
    pending_request_count: usize,
) -> usize {
    pending_request_count + if has_pending_proposal { 1 } else { 0 }
}

pub(in crate::root) const fn walletconnect_attention_transition(
    previous_count: usize,
    next_count: usize,
    window_active: bool,
) -> WalletConnectAttentionTransition {
    WalletConnectAttentionTransition {
        sync_badge_count: previous_count != next_count,
        request_attention: next_count > previous_count && !window_active,
        clear_attention: previous_count != 0 && next_count == 0,
    }
}

pub(super) struct WalletConnectUiState {
    pub(super) uri_input: Entity<InputState>,
    pub(super) account_select: Entity<SelectState<SearchableVec<WalletConnectAccountSelectItem>>>,
    pending_pairings: BTreeMap<String, WalletConnectPairingUri>,
    pending_proposal: Option<WalletConnectProposalUi>,
    pending_requests: BTreeMap<String, WalletConnectRequestUi>,
    completed_request_dialogs: BTreeMap<String, WalletConnectCompletedRequestUi>,
    sessions: Vec<WalletConnectSessionRecord>,
    approval_handoff_sessions: BTreeMap<String, WalletConnectSessionRecord>,
    subscriptions: BTreeMap<String, String>,
    selected_account_uuid: Option<Arc<str>>,
    connection_dialog_open: bool,
    request_dialog_open: bool,
    request_dialog_key: Option<Arc<str>>,
    request_dialog_focus: FocusHandle,
    request_dialog_refresh_active: bool,
    request_dialog_refresh_generation: u64,
    walletconnect_gas_fee: Eip1559GasFeeEditorState,
    walletconnect_fee_state: Option<WalletConnectFeeState>,
    walletconnect_fee_request_generation: u64,
    request_disclosure_states: BTreeMap<String, WalletConnectRequestDisclosureState>,
    dismissed_request_dialog_keys: BTreeSet<String>,
    handled_request_keys: BTreeSet<String>,
    handled_request_key_order: VecDeque<String>,
    request_dialog_deferred_logged: bool,
    pairing_in_progress: bool,
    approving_proposal: bool,
    request_expiry_timer_active: bool,
    session_expiry_timer_active: bool,
    relay_reconnecting: bool,
    relay_workers: BTreeMap<String, WalletConnectRelayWorkerHandle>,
    request_expiry_generation: u64,
    session_expiry_generation: u64,
    approval_progress_generation: u64,
    session_expiry_deadline: Option<u64>,
    request_actions: BTreeSet<String>,
    request_approval_progress: BTreeMap<String, WalletConnectApprovalProgress>,
    disconnecting_sessions: BTreeSet<String>,
    status: Option<Arc<str>>,
    pub(super) error: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
enum WalletConnectApprovalProgressStep {
    PrepareRequest,
    ApproveOnDevice,
    BroadcastTransaction,
    RespondToDapp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WalletConnectApprovalStepState {
    step: WalletConnectApprovalProgressStep,
    status: PublicActionStepStatus,
    tx_hash: Option<Arc<str>>,
    message: Option<Arc<str>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WalletConnectApprovalProgress {
    generation: u64,
    steps: Vec<WalletConnectApprovalStepState>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct WalletConnectRequestDisclosureState {
    network_fee_open: bool,
    transaction_details_open: bool,
    raw_request_open: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletConnectRequestDisclosure {
    NetworkFee,
    TransactionDetails,
    RawRequest,
}

#[derive(Clone)]
struct WalletConnectCompletedRequestUi {
    request: WalletConnectRequestUi,
    status: WalletConnectCompletedRequestStatus,
    message: Arc<str>,
    error: Option<Arc<str>>,
    submitted_tx_hash: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletConnectCompletedRequestStatus {
    Approved,
    TransactionSubmitted,
    AuthorizationFailed,
    RequestFailed,
    Expired,
    RelayResponseFailed,
    TransactionSubmittedRelayResponseFailed,
}

impl WalletConnectUiState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<'_, WalletRoot>) -> Self {
        let uri_input = new_text_input(window, cx, "paste wc: URI");
        let account_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true)
        });
        let request_dialog_focus = cx.focus_handle();
        let walletconnect_gas_fee = Eip1559GasFeeEditorState::new(window, cx);
        let max_fee_input = walletconnect_gas_fee.max_fee_input.clone();
        let max_priority_fee_input = walletconnect_gas_fee.max_priority_fee_input.clone();
        cx.subscribe(
            &max_fee_input,
            |root, input, event: &gpui_component::input::InputEvent, cx| {
                if matches!(event, gpui_component::input::InputEvent::Change) {
                    if root
                        .walletconnect
                        .walletconnect_gas_fee
                        .consume_programmatic_input_change(
                            Eip1559GasFeeEditTarget::MaxFee,
                            input.read(cx).value().as_ref(),
                        )
                    {
                        return;
                    }
                    root.walletconnect_fee_editor_changed(cx);
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe(
            &max_priority_fee_input,
            |root, input, event: &gpui_component::input::InputEvent, cx| {
                if matches!(event, gpui_component::input::InputEvent::Change) {
                    if root
                        .walletconnect
                        .walletconnect_gas_fee
                        .consume_programmatic_input_change(
                            Eip1559GasFeeEditTarget::MaxTip,
                            input.read(cx).value().as_ref(),
                        )
                    {
                        return;
                    }
                    root.walletconnect_fee_editor_changed(cx);
                    cx.notify();
                }
            },
        )
        .detach();
        Self {
            uri_input,
            account_select,
            pending_pairings: BTreeMap::new(),
            pending_proposal: None,
            pending_requests: BTreeMap::new(),
            completed_request_dialogs: BTreeMap::new(),
            sessions: Vec::new(),
            approval_handoff_sessions: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            selected_account_uuid: None,
            connection_dialog_open: false,
            request_dialog_open: false,
            request_dialog_key: None,
            request_dialog_focus,
            request_dialog_refresh_active: false,
            request_dialog_refresh_generation: 0,
            walletconnect_gas_fee,
            walletconnect_fee_state: None,
            walletconnect_fee_request_generation: 0,
            request_disclosure_states: BTreeMap::new(),
            dismissed_request_dialog_keys: BTreeSet::new(),
            handled_request_keys: BTreeSet::new(),
            handled_request_key_order: VecDeque::new(),
            request_dialog_deferred_logged: false,
            pairing_in_progress: false,
            approving_proposal: false,
            request_expiry_timer_active: false,
            session_expiry_timer_active: false,
            relay_reconnecting: false,
            relay_workers: BTreeMap::new(),
            request_expiry_generation: 0,
            session_expiry_generation: 0,
            approval_progress_generation: 0,
            session_expiry_deadline: None,
            request_actions: BTreeSet::new(),
            request_approval_progress: BTreeMap::new(),
            disconnecting_sessions: BTreeSet::new(),
            status: None,
            error: None,
        }
    }

    pub(super) fn clear_runtime(&mut self) {
        self.pending_pairings.clear();
        self.pending_proposal = None;
        self.pending_requests.clear();
        self.completed_request_dialogs.clear();
        self.sessions.clear();
        self.approval_handoff_sessions.clear();
        self.subscriptions.clear();
        self.selected_account_uuid = None;
        self.connection_dialog_open = false;
        self.request_dialog_open = false;
        self.request_dialog_key = None;
        self.request_dialog_refresh_active = false;
        self.request_dialog_refresh_generation =
            self.request_dialog_refresh_generation.wrapping_add(1);
        self.walletconnect_gas_fee.refresh_id =
            self.walletconnect_gas_fee.refresh_id.wrapping_add(1);
        self.walletconnect_gas_fee.quote = None;
        self.walletconnect_gas_fee.refreshing = false;
        self.walletconnect_gas_fee.error = None;
        self.walletconnect_gas_fee.quote_error = None;
        self.walletconnect_fee_state = None;
        self.walletconnect_fee_request_generation =
            self.walletconnect_fee_request_generation.wrapping_add(1);
        self.request_disclosure_states.clear();
        self.dismissed_request_dialog_keys.clear();
        self.handled_request_keys.clear();
        self.handled_request_key_order.clear();
        self.request_dialog_deferred_logged = false;
        self.pairing_in_progress = false;
        self.approving_proposal = false;
        self.request_expiry_timer_active = false;
        self.session_expiry_timer_active = false;
        self.relay_reconnecting = false;
        for worker in self.relay_workers.values() {
            worker.stop();
        }
        self.relay_workers.clear();
        self.request_expiry_generation = self.request_expiry_generation.wrapping_add(1);
        self.session_expiry_generation = self.session_expiry_generation.wrapping_add(1);
        self.approval_progress_generation = self.approval_progress_generation.wrapping_add(1);
        self.session_expiry_deadline = None;
        self.request_actions.clear();
        self.request_approval_progress.clear();
        self.disconnecting_sessions.clear();
        self.status = None;
        self.error = None;
    }

    fn remove_pending_request(&mut self, request_key: &str) -> Option<WalletConnectRequestUi> {
        self.dismissed_request_dialog_keys.remove(request_key);
        self.request_approval_progress.remove(request_key);
        self.request_disclosure_states.remove(request_key);
        let request = self.pending_requests.remove(request_key);
        if self
            .walletconnect_fee_state
            .as_ref()
            .is_some_and(|state| state.request_key.as_ref() == request_key)
        {
            self.walletconnect_fee_state = None;
        }
        if request.is_some() {
            self.remember_handled_request(request_key);
        }
        request
    }

    fn attention_count(&self) -> usize {
        walletconnect_attention_count(self.pending_proposal.is_some(), self.pending_requests.len())
    }

    fn remember_handled_request(&mut self, request_key: &str) {
        requests::remember_walletconnect_handled_request_key(
            &mut self.handled_request_keys,
            &mut self.handled_request_key_order,
            request_key.to_owned(),
            WALLETCONNECT_HANDLED_REQUEST_KEY_LIMIT,
        );
    }

    fn retain_pending_requests(
        &mut self,
        mut keep: impl FnMut(&String, &mut WalletConnectRequestUi) -> bool,
    ) {
        self.pending_requests
            .retain(|key, request| keep(key, request));
        self.prune_dismissed_request_dialog_keys();
        self.prune_request_approval_progress();
        self.prune_request_disclosure_states();
        if self.walletconnect_fee_state.as_ref().is_some_and(|state| {
            !self
                .pending_requests
                .contains_key(state.request_key.as_ref())
        }) {
            self.walletconnect_fee_state = None;
        }
    }

    fn dismiss_request_dialog(&mut self, request_key: &str) {
        if self.pending_requests.contains_key(request_key) {
            self.dismissed_request_dialog_keys
                .insert(request_key.to_owned());
        }
    }

    fn prune_dismissed_request_dialog_keys(&mut self) {
        self.dismissed_request_dialog_keys
            .retain(|key| self.pending_requests.contains_key(key));
    }

    fn prune_request_approval_progress(&mut self) {
        self.request_approval_progress
            .retain(|key, _| self.pending_requests.contains_key(key));
    }

    fn request_disclosure_state(&self, request_key: &str) -> WalletConnectRequestDisclosureState {
        self.request_disclosure_states
            .get(request_key)
            .copied()
            .unwrap_or_default()
    }

    fn toggle_request_disclosure(
        &mut self,
        request_key: &str,
        disclosure: WalletConnectRequestDisclosure,
    ) {
        if !self.pending_requests.contains_key(request_key) {
            return;
        }
        let state = self
            .request_disclosure_states
            .entry(request_key.to_owned())
            .or_default();
        toggle_request_disclosure_state(state, disclosure);
    }

    fn prune_request_disclosure_states(&mut self) {
        prune_request_disclosure_states(
            &mut self.request_disclosure_states,
            &self.pending_requests,
        );
    }

    fn start_request_approval_progress(
        &mut self,
        request_key: &str,
        request: &WalletConnectRequestUi,
    ) -> u64 {
        self.approval_progress_generation = self.approval_progress_generation.wrapping_add(1);
        let generation = self.approval_progress_generation;
        self.request_approval_progress.insert(
            request_key.to_owned(),
            WalletConnectApprovalProgress::new(generation, request),
        );
        generation
    }

    fn apply_request_approval_progress_update(
        &mut self,
        request_key: &str,
        generation: u64,
        step: WalletConnectApprovalProgressStep,
        status: PublicActionStepStatus,
        tx_hash: Option<String>,
        message: Option<String>,
    ) {
        let Some(progress) = self.request_approval_progress.get_mut(request_key) else {
            return;
        };
        if progress.generation != generation {
            return;
        }
        progress.apply_update(step, status, tx_hash, message);
    }

    fn fail_request_approval_progress(
        &mut self,
        request_key: &str,
        generation: u64,
        message: String,
    ) {
        let Some(progress) = self.request_approval_progress.get_mut(request_key) else {
            return;
        };
        if progress.generation != generation {
            return;
        }
        progress.fail(message);
    }
}

const fn toggle_request_disclosure_state(
    state: &mut WalletConnectRequestDisclosureState,
    disclosure: WalletConnectRequestDisclosure,
) {
    match disclosure {
        WalletConnectRequestDisclosure::NetworkFee => {
            state.network_fee_open = !state.network_fee_open;
        }
        WalletConnectRequestDisclosure::TransactionDetails => {
            state.transaction_details_open = !state.transaction_details_open;
        }
        WalletConnectRequestDisclosure::RawRequest => {
            state.raw_request_open = !state.raw_request_open;
        }
    }
}

fn prune_request_disclosure_states(
    states: &mut BTreeMap<String, WalletConnectRequestDisclosureState>,
    pending_requests: &BTreeMap<String, WalletConnectRequestUi>,
) {
    states.retain(|key, _| pending_requests.contains_key(key));
}

impl WalletConnectApprovalProgress {
    fn new(generation: u64, request: &WalletConnectRequestUi) -> Self {
        let mut steps = walletconnect_approval_progress_steps(request)
            .into_iter()
            .map(|step| WalletConnectApprovalStepState {
                step,
                status: PublicActionStepStatus::NotStarted,
                tx_hash: None,
                message: None,
            })
            .collect::<Vec<_>>();
        if let Some(first) = steps.first_mut() {
            first.status = PublicActionStepStatus::Pending;
        }
        Self { generation, steps }
    }

    fn apply_update(
        &mut self,
        step: WalletConnectApprovalProgressStep,
        status: PublicActionStepStatus,
        tx_hash: Option<String>,
        message: Option<String>,
    ) {
        let Some(step) = self.steps.iter_mut().find(|item| item.step == step) else {
            return;
        };
        step.status = status;
        if let Some(tx_hash) = tx_hash {
            step.tx_hash = Some(Arc::from(tx_hash));
        }
        if let Some(message) = message {
            step.message = Some(Arc::from(message));
        } else if status != PublicActionStepStatus::Error {
            step.message = None;
        }
    }

    fn fail(&mut self, message: String) {
        let step_index = self
            .steps
            .iter()
            .position(|step| step.status == PublicActionStepStatus::Pending)
            .or_else(|| {
                self.steps
                    .iter()
                    .position(|step| step.status == PublicActionStepStatus::NotStarted)
            })
            .or_else(|| self.steps.len().checked_sub(1));
        if let Some(step_index) = step_index {
            let step = &mut self.steps[step_index];
            step.status = PublicActionStepStatus::Error;
            step.message = Some(Arc::from(message));
        }
    }
}

impl WalletConnectCompletedRequestUi {
    fn from_outcome(
        request: WalletConnectRequestUi,
        outcome: &WalletConnectRequestApprovalOutcome,
    ) -> Self {
        let status = walletconnect_completed_request_status(outcome);
        Self {
            request,
            status,
            message: Arc::from(walletconnect_completed_request_message(status)),
            error: outcome
                .relay_error
                .as_ref()
                .or(outcome.request_error.as_ref())
                .map(|error| Arc::from(error.as_str())),
            submitted_tx_hash: outcome
                .submitted_tx_hash
                .as_ref()
                .map(|tx_hash| Arc::from(tx_hash.as_str())),
        }
    }
}

const fn walletconnect_completed_request_status(
    outcome: &WalletConnectRequestApprovalOutcome,
) -> WalletConnectCompletedRequestStatus {
    if outcome.relay_error.is_some() {
        if outcome.submitted_tx_hash.is_some() {
            WalletConnectCompletedRequestStatus::TransactionSubmittedRelayResponseFailed
        } else {
            WalletConnectCompletedRequestStatus::RelayResponseFailed
        }
    } else if outcome.authorization_failed {
        WalletConnectCompletedRequestStatus::AuthorizationFailed
    } else if outcome.request_error.is_some() {
        WalletConnectCompletedRequestStatus::RequestFailed
    } else if outcome.expired {
        WalletConnectCompletedRequestStatus::Expired
    } else if outcome.submitted_tx_hash.is_some() {
        WalletConnectCompletedRequestStatus::TransactionSubmitted
    } else {
        WalletConnectCompletedRequestStatus::Approved
    }
}

const fn walletconnect_completed_request_message(
    status: WalletConnectCompletedRequestStatus,
) -> &'static str {
    match status {
        WalletConnectCompletedRequestStatus::Approved => {
            "Request approved and WalletConnect response published."
        }
        WalletConnectCompletedRequestStatus::TransactionSubmitted => {
            "Transaction submitted and WalletConnect response published."
        }
        WalletConnectCompletedRequestStatus::AuthorizationFailed => {
            "Request was not authorized; error response published to the dapp."
        }
        WalletConnectCompletedRequestStatus::RequestFailed => {
            "Request was not approved; error response published to the dapp."
        }
        WalletConnectCompletedRequestStatus::Expired => {
            "Request expired before approval completed."
        }
        WalletConnectCompletedRequestStatus::RelayResponseFailed => {
            "Request was handled locally, but the WalletConnect response failed to publish."
        }
        WalletConnectCompletedRequestStatus::TransactionSubmittedRelayResponseFailed => {
            "Transaction submitted, but the WalletConnect response failed to publish."
        }
    }
}

#[derive(Clone)]
pub(super) struct WalletConnectAccountSelectItem {
    public_account_uuid: Arc<str>,
    label: Arc<str>,
    address: alloy::primitives::Address,
    usd_total_label: Option<Arc<str>>,
}

#[derive(Clone)]
struct WalletConnectProposalUi {
    pairing: WalletConnectPairingUri,
    proposal: WalletConnectSessionProposal,
}

#[derive(Clone)]
struct WalletConnectRequestUi {
    key: String,
    review_token: u64,
    session: WalletConnectSessionRecord,
    parsed: WalletConnectParsedRequest,
    item: WalletConnectPendingRequest,
    account_source: PublicAccountSource,
}

#[derive(Debug, PartialEq, Eq)]
struct WalletConnectRequestDialogNav {
    index: usize,
    total: usize,
    previous_key: Option<String>,
    next_key: Option<String>,
}

#[derive(Debug, Clone)]
struct WalletConnectRelayMessage {
    topic: String,
    message: String,
}

#[derive(Default)]
struct WalletConnectRelayOutput {
    messages: Vec<WalletConnectRelayMessage>,
    subscriptions: BTreeMap<String, String>,
}

#[derive(Clone)]
struct WalletConnectRelayWorkerHandle {
    worker_id: u64,
    project_id: String,
    command_tx: mpsc::UnboundedSender<WalletConnectRelayWorkerCommand>,
}

impl WalletConnectRelayWorkerHandle {
    fn execute(
        &self,
        steps: Vec<WalletConnectRelayStep>,
        wait_for_push: bool,
        emit_pushes: bool,
    ) -> oneshot::Receiver<Result<WalletConnectRelayOutput, String>> {
        let (response_tx, response_rx) = oneshot::channel();
        let _ = self
            .command_tx
            .send(WalletConnectRelayWorkerCommand::Execute {
                steps,
                wait_for_push,
                emit_pushes,
                response_tx,
            });
        response_rx
    }

    fn set_topics(&self, topics: Vec<String>) {
        let _ = self
            .command_tx
            .send(WalletConnectRelayWorkerCommand::SetTopics { topics });
    }

    fn stop(&self) {
        let _ = self.command_tx.send(WalletConnectRelayWorkerCommand::Stop);
    }

    fn stop_after_unsubscribe(&self) {
        let _ = self
            .command_tx
            .send(WalletConnectRelayWorkerCommand::StopAfterUnsubscribe);
    }
}

enum WalletConnectRelayWorkerCommand {
    SetTopics {
        topics: Vec<String>,
    },
    Execute {
        steps: Vec<WalletConnectRelayStep>,
        wait_for_push: bool,
        emit_pushes: bool,
        response_tx: oneshot::Sender<Result<WalletConnectRelayOutput, String>>,
    },
    StopAfterUnsubscribe,
    Stop,
}

enum WalletConnectRelayWorkerCommandOutcome {
    Continue,
    Reconnect,
    Stop,
}

enum WalletConnectRelayWorkerEvent {
    Output(WalletConnectRelayOutput),
    Reconnecting(String),
    Reconnected,
}

struct WalletConnectClientContext {
    worker: WalletConnectRelayWorkerHandle,
}

struct WalletConnectRelayProcessingPlan {
    store: Arc<DesktopVaultStore>,
    view_session: Arc<DesktopViewSession>,
    worker: WalletConnectRelayWorkerHandle,
    pairings: Vec<WalletConnectPairingUri>,
    sessions: Vec<WalletConnectSessionRecord>,
    enabled_chain_ids: BTreeSet<u64>,
}

struct WalletConnectRelayProcessingResult {
    proposals: Vec<WalletConnectProposalUi>,
    removed_pairings: Vec<String>,
    pending_requests: Vec<WalletConnectRequestUi>,
    removed_sessions: Vec<String>,
    subscriptions: BTreeMap<String, String>,
    error: Option<String>,
}

struct WalletConnectApprovalRelayResult {
    output: WalletConnectRelayOutput,
    post_persist_error: Option<String>,
}

struct WalletConnectSessionRequestFailure {
    kind: WalletConnectRequestErrorKind,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WalletConnectApprovedChainDisplay {
    label: String,
    icon_path: Option<&'static str>,
}

struct WalletConnectRequestApprovalOutcome {
    authorization_failed: bool,
    response_published: bool,
    submitted_tx_hash: Option<String>,
    relay_error: Option<String>,
    request_error: Option<String>,
    expired: bool,
    hash_fallback_confirmation_required: bool,
    #[cfg_attr(not(feature = "hardware"), allow(dead_code))]
    refreshed_hardware_session: Option<HardwareProfileSession>,
}

impl WalletConnectRequestApprovalOutcome {
    const fn expired(
        response_published: bool,
        relay_error: Option<String>,
        submitted_tx_hash: Option<String>,
    ) -> Self {
        Self {
            authorization_failed: false,
            response_published,
            submitted_tx_hash,
            relay_error,
            request_error: None,
            expired: true,
            hash_fallback_confirmation_required: false,
            refreshed_hardware_session: None,
        }
    }

    const fn hash_fallback_confirmation_required(
        refreshed_hardware_session: Option<HardwareProfileSession>,
    ) -> Self {
        Self {
            authorization_failed: false,
            response_published: false,
            submitted_tx_hash: None,
            relay_error: None,
            request_error: None,
            expired: false,
            hash_fallback_confirmation_required: true,
            refreshed_hardware_session,
        }
    }
}

fn walletconnect_request_uses_hardware_typed_data_hash_fallback(
    request: &WalletConnectRequestUi,
    mode: HardwareTypedDataSigningMode,
) -> bool {
    request.account_source == PublicAccountSource::HardwareDerived
        && matches!(
            request.item.method,
            WalletConnectSupportedMethod::EthSignTypedData
                | WalletConnectSupportedMethod::EthSignTypedDataV4
        )
        && mode.requires_hash_fallback_warning()
}

const fn walletconnect_request_approve_label(
    in_flight: bool,
    hardware_request: bool,
    hash_fallback: bool,
    unlimited_allowance: bool,
) -> &'static str {
    if in_flight {
        if hardware_request {
            "Waiting for device..."
        } else {
            "Approving..."
        }
    } else if hash_fallback {
        "Continue with hash fallback"
    } else if hardware_request {
        "Approve on device"
    } else if unlimited_allowance {
        "Approve unlimited"
    } else {
        "Approve"
    }
}
