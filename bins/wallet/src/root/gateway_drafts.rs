//! Extension drafts are ephemeral, peer-scoped inputs to the existing Public action owner.
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy::primitives::{Address, U256};
use gpui::{Context, Window};
use gpui_component::WindowExt as _;
use wallet_ops::{
    HttpContext, PublicActionGasFeeMode, PublicActionGasFeeQuote, PublicActionGasFeeQuoteBundle,
    PublicActionGasFeeSelection, PublicActionKind, PublicAssetId, PublicBalanceAmount,
    PublicShieldTransactionProfile, PublicTransactionIntent, RAILGUN_PROTOCOL_FEE_BPS,
    TokenAnchorRateCache, estimate_public_action_gas_cost_with_profile_and_ceiling,
    gateway::{
        GatewayDraftCommand, GatewayDraftEstimate, GatewayDraftEstimatePayload,
        GatewayDraftExecution, GatewayDraftFee, GatewayDraftGasQuote, GatewayDraftInput,
        GatewayDraftKind, GatewayDraftPayload, GatewayDraftRecipient, GatewayDraftStatus,
        GatewayDraftView,
    },
    parse_send_amount, public_shield_protocol_fee_amount,
    quote_public_action_gas_fee_bundle_with_profile, resolve_public_ens_recipient,
    settings::{EffectiveChainConfig, EffectiveTokenRegistry},
    vault::{DesktopVaultStore, DesktopViewSession, PublicAccountMetadata, PublicAccountSource},
};

use super::gas_fee::{format_gwei, parse_gwei_to_wei};
use super::public_action::{
    PublicActionFeeDisplay, PublicSendDraft, PublicShieldDraft,
    authorized_public_action_gas_fee_selection, format_gas_limit,
    public_action_max_amount_after_reserve, public_action_protocol_fee_label,
    public_send_authorization_summary, public_shield_authorization_summary,
};
use super::public_balances::public_asset_icon_path;
use super::spend_authorization::SpendAuthorizationIntent;
use super::{WalletRoot, format_send_amount_input, public_balance_amount_label};

mod private;
mod self_broadcast;
use private::valid_private_input_size;

const ESTIMATE_LIFETIME: Duration = Duration::from_secs(30);
const MAX_DRAFT_PEERS: usize = 8;

#[derive(Default)]
pub(super) struct GatewayDraftBook {
    records: HashMap<String, DraftRecord>,
}

struct DraftRecord {
    view: GatewayDraftView,
    wallet: Arc<DesktopViewSession>,
    wallet_generation: u64,
    prepared: Option<PreparedDraft>,
    context_binding: Option<DraftContextBinding>,
    estimated_at: Option<Instant>,
    estimation: Option<tokio::task::AbortHandle>,
    execution: Option<GatewayDraftExecution>,
    picker: Option<private::PrivateDraftPicker>,
}

#[derive(Clone)]
enum PreparedDraft {
    Send(Box<PublicSendDraft>),
    Shield(Box<PublicShieldDraft>),
    Private(Box<private::PreparedPrivateDraft>),
    PrivateSelfBroadcast(Box<self_broadcast::PreparedSelfBroadcastDraft>),
}

impl GatewayDraftBook {
    fn create(
        &mut self,
        peer_id: &str,
        request_id: String,
        input: GatewayDraftPayload,
        wallet: Arc<DesktopViewSession>,
        wallet_generation: u64,
    ) -> bool {
        if request_id.is_empty() || request_id.len() > 64 || !valid_input_size(&input) {
            return false;
        }
        if let Some(existing) = self.records.get(peer_id) {
            // A repeated Create cannot replace work or restart an estimate after reconnect.
            if existing.view.request_id == request_id || existing.execution.is_some() {
                return false;
            }
        } else if self.records.len() >= MAX_DRAFT_PEERS {
            return false;
        }
        if let Some(old) = self.records.remove(peer_id)
            && let Some(job) = old.estimation
        {
            job.abort();
        }
        let draft_id = alloy::hex::encode(rand::random::<[u8; 16]>());
        self.records.insert(
            peer_id.to_owned(),
            DraftRecord {
                view: GatewayDraftView {
                    peer_id: peer_id.to_owned(),
                    draft_id,
                    request_id,
                    revision: 0,
                    input,
                    status: GatewayDraftStatus::Editing,
                    estimate: None,
                    private_progress: None,
                    private_options: None,
                    gas_quote: None,
                    recipients: Vec::new(),
                    step_label: String::new(),
                    message: String::new(),
                    warning: false,
                    can_cancel: true,
                    can_retry: false,
                },
                wallet,
                wallet_generation,
                prepared: None,
                context_binding: None,
                estimated_at: None,
                estimation: None,
                execution: None,
                picker: None,
            },
        );
        true
    }

    fn update(
        &mut self,
        peer_id: &str,
        draft_id: &str,
        revision: u64,
        input: GatewayDraftPayload,
        wallet: &Arc<DesktopViewSession>,
        wallet_generation: u64,
    ) -> bool {
        if !valid_input_size(&input) {
            return false;
        }
        let Some(record) = self.records.get_mut(peer_id).filter(|record| {
            record.view.draft_id == draft_id
                && record.execution.is_none()
                && revision > record.view.revision
                && Arc::ptr_eq(wallet, &record.wallet)
                && wallet_generation == record.wallet_generation
                && std::mem::discriminant(&record.view.input) == std::mem::discriminant(&input)
        }) else {
            return false;
        };
        if let Some(job) = record.estimation.take() {
            job.abort();
        }
        if quote_identity(&record.view.input) != quote_identity(&input) {
            record.view.gas_quote = None;
        }
        record.picker = None;
        record.view.input = input;
        record.view.revision = revision;
        record.view.estimate = None;
        record.prepared = None;
        record.estimated_at = None;
        true
    }

    fn dismiss(&mut self, peer_id: &str, draft_id: &str) -> bool {
        if !self.records.get(peer_id).is_some_and(|record| {
            record.view.draft_id == draft_id
                && record.execution.as_ref().is_none_or(|execution| {
                    matches!(
                        execution.snapshot().status,
                        GatewayDraftStatus::Done | GatewayDraftStatus::Failed
                    )
                })
        }) {
            return false;
        }
        self.records.remove(peer_id).is_some_and(|record| {
            if let Some(job) = record.estimation {
                job.abort();
            }
            true
        })
    }

    #[cfg(feature = "hardware")]
    pub(super) fn refresh_hardware_session(
        &mut self,
        previous: &Arc<DesktopViewSession>,
        refreshed: &Arc<DesktopViewSession>,
        generation: u64,
    ) {
        for record in self.records.values_mut() {
            if record.wallet_generation == generation
                && Arc::ptr_eq(&record.wallet, previous)
                && record.execution.as_ref().is_some_and(|execution| {
                    execution.generation().is_some()
                        || (execution.snapshot().private.is_some()
                            && execution.has_review_approval())
                })
            {
                // Only the known hardware-profile refresh may carry running progress to its
                // replacement view. Unsubmitted drafts keep their original authority binding.
                record.wallet = refreshed.clone();
            }
        }
    }

    pub(super) fn stopped(&self, generation: u64) {
        for execution in self
            .records
            .values()
            .filter_map(|record| record.execution.as_ref())
        {
            if execution.snapshot().private.is_none() && execution.generation() == Some(generation)
            {
                execution.stopped();
            }
        }
    }
    pub(super) fn reconcile_wallet(
        &mut self,
        wallet: Option<&Arc<DesktopViewSession>>,
        generation: u64,
    ) {
        self.records.retain(|_, record| {
            let keep = wallet.is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
                && generation == record.wallet_generation;
            if !keep {
                retire_record(record);
            }
            keep
        });
    }

    pub(super) fn retain_peers(&mut self, peers: &[wallet_ops::gateway::GatewayPeerSummary]) {
        self.records.retain(|id, record| {
            let keep = peers
                .iter()
                .any(|peer| alloy::hex::encode(peer.id.to_bytes()) == *id);
            if !keep {
                retire_record(record);
            }
            keep
        });
    }
    pub(super) fn retire(&mut self) {
        for (_, record) in self.records.drain() {
            if let Some(job) = record.estimation {
                job.abort();
            }
            if let Some(execution) = record.execution {
                execution.cancel_review();
            }
        }
    }

    pub(super) fn views(&self, root: &WalletRoot) -> Vec<GatewayDraftView> {
        let mut views: Vec<_> = self
            .records
            .values()
            .filter(|record| {
                root.view_session
                    .as_ref()
                    .is_some_and(|view| Arc::ptr_eq(view, &record.wallet))
                    && root.active_wallet_generation == record.wallet_generation
            })
            .map(|record| {
                let mut view = record.view.clone();
                if let GatewayDraftPayload::Private(input) = &record.view.input {
                    let prepared = match &record.prepared {
                        Some(PreparedDraft::Private(prepared)) => Some(prepared.as_ref()),
                        _ => None,
                    };
                    view.private_options = (record.execution.is_none()).then(|| {
                        root.gateway_private_draft_options(input, prepared, record.picker.as_ref())
                    });
                }
                if record.execution.is_none()
                    && let Some(PreparedDraft::Private(prepared)) = &record.prepared
                    && (!prepared.is_current(root, &record.view.input)
                        || record
                            .estimated_at
                            .is_none_or(|at| at.elapsed() > ESTIMATE_LIFETIME))
                {
                    view.status = GatewayDraftStatus::Editing;
                    view.message =
                        "Private funds or fee estimate changed. Refresh the estimate.".into();
                }
                if let Some(execution) = &record.execution {
                    let progress = execution.snapshot();
                    view.private_progress.clone_from(&progress.private);
                    view.status = progress.status;
                    view.step_label = progress.step_label;
                    view.message = progress.message;
                    view.warning = progress.warning;
                    view.can_retry = progress.can_retry;
                    view.can_cancel = matches!(
                        view.status,
                        GatewayDraftStatus::Attention | GatewayDraftStatus::InProgress
                    ) && execution.generation().is_none_or(|generation| {
                        if let Some(private) = &view.private_progress {
                            return private.stop || private.stop_waiting;
                        }
                        generation == root.public_form.action_generation
                            && root.public_form.action_stop_available
                    });
                }
                if record.execution.is_none()
                    && let Some(PreparedDraft::PrivateSelfBroadcast(prepared)) = &record.prepared
                    && (!prepared.is_current(root, &record.view.input)
                        || (!prepared.retains_background_estimate()
                            && record
                                .estimated_at
                                .is_none_or(|at| at.elapsed() > ESTIMATE_LIFETIME)))
                {
                    view.status = GatewayDraftStatus::Editing;
                    view.message =
                        "Private funds or self-broadcast estimate changed. Refresh the estimate."
                            .into();
                }
                view
            })
            .collect();
        views.sort_by(|a, b| a.draft_id.cmp(&b.draft_id));
        views
    }
}

struct EstimationContext {
    input: GatewayDraftInput,
    account: PublicAccountMetadata,
    asset: PublicAssetId,
    symbol: String,
    decimals: u8,
    balance: U256,
    native_balance: U256,
    recipient: Option<Address>,
    effective_chain: Option<EffectiveChainConfig>,
    ethereum: Option<EffectiveChainConfig>,
    registry: EffectiveTokenRegistry,
    anchors: Arc<TokenAnchorRateCache>,
    http: HttpContext,
    wallet: Arc<DesktopViewSession>,
    store: Arc<DesktopVaultStore>,
    icon: Option<crate::assets::WalletIconSource>,
}

#[derive(PartialEq, Eq)]
struct DraftContextBinding {
    account: PublicAccountMetadata,
    balance: U256,
    native_balance: U256,
    recipient: Option<Address>,
}
impl EstimationContext {
    fn binding(&self) -> DraftContextBinding {
        DraftContextBinding {
            account: self.account.clone(),
            balance: self.balance,
            native_balance: self.native_balance,
            recipient: self.recipient,
        }
    }
}

impl WalletRoot {
    pub(super) fn apply_gateway_draft_command(
        &mut self,
        peer_id: &str,
        command: GatewayDraftCommand,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(wallet) = self.view_session.clone() else {
            return;
        };
        match command {
            GatewayDraftCommand::Create { request_id, input } => {
                if !self.gateway.drafts.borrow_mut().create(
                    peer_id,
                    request_id,
                    input,
                    wallet,
                    self.active_wallet_generation,
                ) {
                    return;
                }
                self.estimate_gateway_draft(peer_id, cx);
            }
            GatewayDraftCommand::Update {
                draft_id,
                revision,
                input,
            } => {
                if !self.gateway.drafts.borrow_mut().update(
                    peer_id,
                    &draft_id,
                    revision,
                    input,
                    &wallet,
                    self.active_wallet_generation,
                ) {
                    return;
                }
                self.estimate_gateway_draft(peer_id, cx);
            }
            GatewayDraftCommand::Submit { draft_id, revision } => {
                self.submit_gateway_draft(peer_id, &draft_id, revision, window, cx);
            }
            GatewayDraftCommand::Cancel { draft_id } => {
                let mut book = self.gateway.drafts.borrow_mut();
                let Some(record) = book
                    .records
                    .get_mut(peer_id)
                    .filter(|record| record.view.draft_id == draft_id)
                else {
                    return;
                };
                if let Some(execution) = record.execution.clone() {
                    drop(book);
                    if execution.snapshot().private.is_some() && execution.generation().is_some() {
                        return; // Private operations use execution-scoped controls.
                    }
                    if let Some(generation) = execution.generation() {
                        if generation == self.public_form.action_generation
                            && self.public_form.action_stop_available
                        {
                            self.stop_public_action_progress(cx);
                            if self.public_form.action_stopped {
                                execution.stopped();
                            }
                        }
                    } else {
                        let reviewing =
                            execution.snapshot().status == GatewayDraftStatus::Attention;
                        execution.cancel_review();
                        let owns_visible_review = execution.snapshot().private.is_none()
                            || self.gateway_private_form(&execution).is_some();
                        if reviewing
                            && execution.snapshot().status == GatewayDraftStatus::Failed
                            && owns_visible_review
                        {
                            // Closing this review also releases the desktop dialog for an explicit retry.
                            window.close_dialog(cx);
                        }
                        self.release_gateway_private_form(&execution, cx);
                    }
                } else {
                    if let Some(job) = record.estimation.take() {
                        job.abort();
                    }
                    book.records.remove(peer_id);
                }
            }
            GatewayDraftCommand::PrivatePicker {
                draft_id,
                revision,
                view_id,
                open,
                query,
            } => {
                if view_id.is_empty() || view_id.len() > 128 || query.len() > 1024 {
                    return;
                }
                let mut book = self.gateway.drafts.borrow_mut();
                let Some(record) = book.records.get_mut(peer_id).filter(|record| {
                    record.view.draft_id == draft_id
                        && record.view.revision == revision
                        && record.execution.is_none()
                        && matches!(&record.view.input, GatewayDraftPayload::Private(input) if input.delivery.broadcaster().is_some())
                }) else {
                    return;
                };
                if open {
                    let query = query.trim().to_ascii_lowercase();
                    if let Some(picker) = record
                        .picker
                        .as_mut()
                        .filter(|picker| picker.view_id == view_id)
                    {
                        picker.query = query;
                    } else {
                        record.picker = Some(private::PrivateDraftPicker::new(view_id, query));
                    }
                } else if record
                    .picker
                    .as_ref()
                    .is_some_and(|picker| picker.view_id == view_id)
                {
                    record.picker = None;
                }
            }
            GatewayDraftCommand::PrivateControl {
                draft_id,
                execution_id,
                control,
            } => {
                let execution = self
                    .gateway
                    .drafts
                    .borrow()
                    .records
                    .get(peer_id)
                    .filter(|record| {
                        record.view.draft_id == draft_id
                            && record.wallet_generation == self.active_wallet_generation
                            && self
                                .view_session
                                .as_ref()
                                .is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
                    })
                    .and_then(|record| record.execution.clone());
                if let Some(execution) = execution {
                    self.gateway_private_control(&execution, &execution_id, control, cx);
                }
            }
            GatewayDraftCommand::Dismiss { draft_id } => {
                self.gateway.drafts.borrow_mut().dismiss(peer_id, &draft_id);
            }
        }
        self.watch_gateway_drafts(cx);
        self.publish_gateway_desktop_state();
        cx.notify();
    }

    fn gateway_draft_context(
        &self,
        input: &GatewayDraftInput,
    ) -> Result<EstimationContext, String> {
        let wallet = self
            .view_session
            .clone()
            .ok_or("Unlock the desktop wallet")?;
        if self.selected_chain != input.chain_id
            || self.public_form.selected_account_uuid.as_deref() != Some(input.account.as_str())
        {
            return Err("Account or network changed. Open a new draft.".into());
        }
        let account = self
            .public_accounts
            .iter()
            .find(|account| {
                account.public_account_uuid == input.account
                    && account.is_active_for_wallet(wallet.wallet_id())
            })
            .cloned()
            .ok_or("This public account is unavailable")?;
        let asset = if input.asset == "native" {
            PublicAssetId::Native
        } else {
            PublicAssetId::Erc20(
                input
                    .asset
                    .parse()
                    .map_err(|_| "Choose an available asset")?,
            )
        };
        let balances = self
            .public_balance_snapshot
            .as_ref()
            .filter(|snapshot| snapshot.chain_id == input.chain_id)
            .and_then(|snapshot| {
                snapshot
                    .accounts
                    .iter()
                    .find(|entry| entry.account == account)
            })
            .ok_or("Refresh balances before preparing a transaction")?;
        let entry = balances
            .balances
            .iter()
            .find(|entry| entry.asset.id == asset)
            .ok_or("Choose an available asset")?;
        let balance = entry
            .amount
            .amount()
            .ok_or("Asset balance is unavailable")?;
        let native_balance = balances
            .balances
            .iter()
            .find(|entry| entry.asset.id == PublicAssetId::Native)
            .and_then(|entry| entry.amount.amount())
            .ok_or("Native gas balance is unavailable")?;
        let store = self
            .vault_store
            .clone()
            .ok_or("Wallet storage is unavailable")?;
        let recipient = if input.kind == GatewayDraftKind::Shield {
            None
        } else if let Some(id) = &input.address_book_entry {
            Some(
                store
                    .list_public_address_book_entries_for_session(&wallet)
                    .map_err(|_| "Address book is unavailable")?
                    .into_iter()
                    .find(|entry| entry.entry_uuid == *id)
                    .ok_or("Address book entry is unavailable. Choose another recipient.")?
                    .address,
            )
        } else {
            input.recipient.trim().parse::<Address>().ok()
        };
        Ok(EstimationContext {
            input: input.clone(),
            account,
            asset,
            symbol: entry.asset.symbol.clone(),
            decimals: entry.asset.decimals,
            balance,
            native_balance,
            recipient,
            effective_chain: self.effective_chain_configs.get(&input.chain_id).cloned(),
            ethereum: self.effective_chain_configs.get(&1).cloned(),
            registry: self.effective_token_registry.clone(),
            anchors: self.public_broadcaster_anchor_cache.clone(),
            http: self.http.clone(),
            wallet,
            store,
            icon: public_asset_icon_path(
                input.chain_id,
                asset,
                Some(&self.effective_token_registry),
            ),
        })
    }

    fn estimate_gateway_draft(&mut self, peer_id: &str, cx: &mut Context<'_, Self>) {
        let input = self.gateway.drafts.borrow().records[peer_id]
            .view
            .input
            .clone();
        let Some(input) = input.public() else {
            if matches!(&input, GatewayDraftPayload::Private(input) if input.delivery.broadcaster().is_some())
            {
                self.ensure_waku_for_delivery(super::DeliveryMode::PublicBroadcaster, cx);
            }
            self.estimate_gateway_private_draft(peer_id, cx);
            return;
        };
        let context = self.gateway_draft_context(input);
        let mut book = self.gateway.drafts.borrow_mut();
        let record = book.records.get_mut(peer_id).expect("admitted draft");
        record.view.recipients = self
            .view_session
            .as_ref()
            .zip(self.vault_store.as_ref())
            .and_then(|(wallet, store)| {
                store
                    .list_public_address_book_entries_for_session(wallet)
                    .ok()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|entry| GatewayDraftRecipient {
                id: entry.entry_uuid,
                label: entry.label,
                address: entry.address.to_checksum(None),
            })
            .collect();
        record.view.status = GatewayDraftStatus::Estimating;
        record.view.message.clear();
        let context = match context {
            Ok(context) => context,
            Err(error) => {
                record.view.gas_quote = None;
                record.view.status = GatewayDraftStatus::Editing;
                record.view.message = error;
                return;
            }
        };
        record.context_binding = Some(context.binding());
        record.prepared = None;
        record.estimated_at = None;
        record.view.estimate = None;
        let draft_id = record.view.draft_id.clone();
        let revision = record.view.revision;
        let wallet = record.wallet.clone();
        let wallet_generation = record.wallet_generation;
        let join = self.runtime.spawn(estimate_draft(context));
        record.estimation = Some(join.abort_handle());
        drop(book);
        let peer_id = peer_id.to_owned();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let _ = this.update(cx, |root, cx| {
                if !root
                    .view_session
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &wallet))
                    || root.active_wallet_generation != wallet_generation
                {
                    return;
                }
                let mut book = root.gateway.drafts.borrow_mut();
                let Some(record) = book.records.get_mut(&peer_id).filter(|record| {
                    record.view.draft_id == draft_id
                        && record.view.revision == revision
                        && record.execution.is_none()
                }) else {
                    return;
                };
                record.estimation = None;
                if root.selected_chain != record.view.input.chain_id()
                    || root.public_form.selected_account_uuid.as_deref()
                        != record
                            .view
                            .input
                            .public()
                            .map(|input| input.account.as_str())
                {
                    record.view.status = GatewayDraftStatus::Editing;
                    record.view.message = "Account or network changed. Open a new draft.".into();
                } else {
                    let result = if let Ok(estimation) = result {
                        record.view.gas_quote = estimation.gas_quote;
                        estimation.result
                    } else {
                        record.view.gas_quote = None;
                        Err("Estimate unavailable. Refresh the gas price to try again.".into())
                    };
                    match result {
                        Ok((prepared, estimate)) => {
                            record.estimated_at = prepared.as_ref().map(|_| Instant::now());
                            record.view.status = if prepared.is_some() {
                                GatewayDraftStatus::Ready
                            } else {
                                record.view.message = "Enter a recipient".into();
                                GatewayDraftStatus::Editing
                            };
                            record.prepared = prepared;
                            record.view.estimate =
                                Some(GatewayDraftEstimatePayload::Public(estimate));
                        }
                        Err(message) => {
                            record.view.status = GatewayDraftStatus::Editing;
                            record.view.message = message;
                        }
                    }
                }
                drop(book);
                root.publish_gateway_desktop_state();
                cx.notify();
            });
        })
        .detach();
    }

    fn submit_gateway_draft(
        &mut self,
        peer_id: &str,
        draft_id: &str,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mut book = self.gateway.drafts.borrow_mut();
        let Some(record) = book.records.get_mut(peer_id).filter(|record| {
            record.view.draft_id == draft_id
                && record.view.revision == revision
                && record.execution.is_none()
                && record.view.status == GatewayDraftStatus::Ready
                && self
                    .view_session
                    .as_ref()
                    .is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
                && self.active_wallet_generation == record.wallet_generation
        }) else {
            return;
        };
        let Some(input) = record.view.input.public() else {
            drop(book);
            self.submit_gateway_private_draft(peer_id, draft_id, revision, window, cx);
            return;
        };
        if self.public_form.sending || self.public_form.shielding || window.has_active_dialog(cx) {
            record.view.message =
                "Finish or close the current action in the desktop app first.".into();
            return;
        }
        if record
            .estimated_at
            .is_none_or(|at| at.elapsed() > ESTIMATE_LIFETIME)
        {
            drop(book);
            self.estimate_gateway_draft(peer_id, cx);
            return;
        }
        match self.gateway_draft_context(input) {
            Ok(context) if record.context_binding.as_ref() != Some(&context.binding()) => {
                drop(book);
                self.estimate_gateway_draft(peer_id, cx);
                return;
            }
            Ok(_) => {}
            Err(error) => {
                record.view.gas_quote = None;
                record.view.status = GatewayDraftStatus::Editing;
                record.view.message = error;
                record.prepared = None;
                return;
            }
        }
        let Some(mut prepared) = record.prepared.take() else {
            return;
        };
        let execution = GatewayDraftExecution::default();
        record.execution = Some(execution.clone());
        let (intent, summary, hardware) = match &mut prepared {
            PreparedDraft::Send(draft) => {
                draft.gateway_execution = Some(execution);
                (
                    SpendAuthorizationIntent::PublicSend(draft.clone()),
                    public_send_authorization_summary(draft),
                    draft.public_account_source == PublicAccountSource::HardwareDerived,
                )
            }
            PreparedDraft::Private(_) | PreparedDraft::PrivateSelfBroadcast(_) => return,
            PreparedDraft::Shield(draft) => {
                draft.gateway_execution = Some(execution);
                (
                    SpendAuthorizationIntent::PublicShield(draft.clone()),
                    public_shield_authorization_summary(draft),
                    draft.public_account_source == PublicAccountSource::HardwareDerived,
                )
            }
        };
        drop(book);
        let summary = summary.requiring_explicit_review();
        self.clear_public_action_progress_state();
        window.activate_window();
        if hardware {
            Self::open_hardware_public_action_authorization_dialog(intent, summary, window, cx);
        } else {
            self.request_spend_authorization(intent, summary, window, cx);
        }
        self.watch_gateway_drafts(cx);
    }

    fn watch_gateway_drafts(&mut self, cx: &Context<'_, Self>) {
        if self.gateway.draft_watch.is_some() {
            return;
        }
        self.gateway.draft_watch = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let keep = this
                    .update(cx, |root, cx| {
                        root.refresh_gateway_private_drafts(cx);
                        root.publish_gateway_desktop_state();
                        let active = root.gateway.drafts.borrow().records.values().any(|record| {
                            (record.execution.is_none()
                                && matches!(record.view.input, GatewayDraftPayload::Private(_)))
                                || record.execution.as_ref().is_some_and(|execution| {
                                    matches!(
                                        execution.snapshot().status,
                                        GatewayDraftStatus::Attention
                                            | GatewayDraftStatus::InProgress
                                    )
                                })
                        });
                        if !active {
                            root.gateway.draft_watch = None;
                        }
                        cx.notify();
                        active
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }
}

fn retire_record(record: &mut DraftRecord) {
    if let Some(job) = record.estimation.take() {
        job.abort();
    }
    if let Some(execution) = &record.execution {
        execution.cancel_review();
    }
}

#[derive(PartialEq, Eq)]
enum DraftGasQuoteIdentity<'a> {
    Public(&'a str, u64, GatewayDraftKind, bool),
    Private(&'a str, u64),
}

fn quote_identity(input: &GatewayDraftPayload) -> Option<DraftGasQuoteIdentity<'_>> {
    match input {
        GatewayDraftPayload::Public(input) => Some(DraftGasQuoteIdentity::Public(
            input.account.as_str(),
            input.chain_id,
            input.kind,
            input.mimic_railway,
        )),
        GatewayDraftPayload::Private(input) => {
            input
                .delivery
                .broadcaster()
                .is_none()
                .then_some(DraftGasQuoteIdentity::Private(
                    &input.wallet,
                    input.chain_id,
                ))
        }
    }
}

fn valid_input_size(input: &GatewayDraftPayload) -> bool {
    let input = match input {
        GatewayDraftPayload::Public(input) => input,
        GatewayDraftPayload::Private(input) => return valid_private_input_size(input),
    };
    input.account.len() <= 128
        && input.asset.len() <= 128
        && input.amount.len() <= 100
        && input.recipient.len() <= 1024
        && input
            .address_book_entry
            .as_ref()
            .is_none_or(|id| id.len() <= 128)
        && match &input.fee {
            GatewayDraftFee::Custom {
                max_fee_gwei,
                priority_fee_gwei,
            } => max_fee_gwei.len() <= 100 && priority_fee_gwei.len() <= 100,
            _ => true,
        }
}

fn draft_gas_fee(
    fee: &GatewayDraftFee,
    quote: PublicActionGasFeeQuote,
    profile: PublicShieldTransactionProfile,
    chain_id: u64,
) -> Result<PublicActionGasFeeSelection, String> {
    let custom = |max_fee_per_gas, max_priority_fee_per_gas| {
        if max_fee_per_gas == 0 || max_priority_fee_per_gas > max_fee_per_gas {
            return Err(
                "Enter a positive maximum fee at least as large as the priority fee".into(),
            );
        }
        Ok(PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        })
    };
    match fee {
        GatewayDraftFee::Normal => authorized_public_action_gas_fee_selection(
            PublicActionGasFeeSelection::Auto,
            Some(quote),
            profile,
            chain_id,
        ),
        GatewayDraftFee::Custom {
            max_fee_gwei,
            priority_fee_gwei,
        } => custom(
            parse_gwei_to_wei(max_fee_gwei)?,
            parse_gwei_to_wei(priority_fee_gwei)?,
        ),
        GatewayDraftFee::Slow | GatewayDraftFee::Fast => {
            let numerator = if matches!(fee, GatewayDraftFee::Slow) {
                3
            } else {
                5
            };
            let scale = |value: u128| {
                value
                    .checked_mul(numerator)
                    .map(|value| value / 4)
                    .ok_or_else(|| "Gas fee is too large".to_owned())
            };
            if profile.uses_legacy_envelope(chain_id) {
                return custom(scale(quote.rpc_gas_price)?.max(1), 0);
            }
            let tip = scale(quote.suggested_max_priority_fee_per_gas)?;
            let cap = quote
                .suggested_max_fee_per_gas
                .saturating_sub(quote.suggested_max_priority_fee_per_gas)
                .checked_add(tip)
                .ok_or("Gas fee is too large")?;
            custom(cap, tip)
        }
    }
}

struct DraftEstimation {
    gas_quote: Option<GatewayDraftGasQuote>,
    result: Result<(Option<PreparedDraft>, GatewayDraftEstimate), String>,
}

async fn estimate_draft(mut context: EstimationContext) -> DraftEstimation {
    // Resolve first so a slow ENS lookup cannot age the subsequently captured quote.
    // A resolution failure still permits the gas hint to be returned for editing.
    let recipient_error = if context.input.kind == GatewayDraftKind::Send
        && context.recipient.is_none()
        && !context.input.recipient.trim().is_empty()
    {
        if let Ok(recipient) = resolve_public_ens_recipient(
            context.input.recipient.trim(),
            context.ethereum.as_ref(),
            &context.http,
        )
        .await
        {
            context.recipient = Some(recipient);
            None
        } else {
            Some("Recipient could not be resolved. Check the address or ENS name.".to_owned())
        }
    } else {
        None
    };
    let profile = if context.input.kind == GatewayDraftKind::Shield && context.input.mimic_railway {
        PublicShieldTransactionProfile::Railway
    } else {
        PublicShieldTransactionProfile::Railoxide
    };
    let Ok(bundle) = quote_public_action_gas_fee_bundle_with_profile(
        context.input.chain_id,
        context.effective_chain.as_ref(),
        profile,
        &context.http,
    )
    .await
    else {
        return DraftEstimation {
            gas_quote: None,
            result: Err(
                "Fee estimate unavailable. Check the desktop connection and try again.".into(),
            ),
        };
    };
    // A gas price hint does not depend on a valid amount, recipient, or custom fee.
    // Keep it separate from the prepared transaction and its submission admission.
    DraftEstimation {
        gas_quote: Some(GatewayDraftGasQuote::new(
            format_gwei(bundle.standard.suggested_max_fee_per_gas),
            format_gwei(bundle.standard.suggested_max_priority_fee_per_gas),
        )),
        result: recipient_error.map_or_else(|| prepare_draft(context, bundle, profile), Err),
    }
}

fn prepare_draft(
    context: EstimationContext,
    bundle: PublicActionGasFeeQuoteBundle,
    profile: PublicShieldTransactionProfile,
) -> Result<(Option<PreparedDraft>, GatewayDraftEstimate), String> {
    let EstimationContext {
        input,
        account,
        asset,
        symbol,
        decimals,
        balance,
        native_balance,
        recipient,
        effective_chain,
        ethereum: _,
        registry,
        anchors,
        http: _,
        wallet,
        store,
        icon,
    } = context;
    let gas_fee = draft_gas_fee(&input.fee, bundle.standard, profile, input.chain_id)?;
    let gas_fee_mode = if matches!(input.fee, GatewayDraftFee::Normal) {
        PublicActionGasFeeMode::Auto
    } else {
        PublicActionGasFeeMode::Custom
    };
    let ceiling = if input.kind == GatewayDraftKind::Shield
        && profile == PublicShieldTransactionProfile::Railway
        && matches!(asset, PublicAssetId::Erc20(_))
        && gas_fee_mode == PublicActionGasFeeMode::Auto
    {
        bundle.authorization_ceiling
    } else {
        gas_fee
    };
    let costs = estimate_public_action_gas_cost_with_profile_and_ceiling(
        input.chain_id,
        effective_chain.as_ref(),
        if input.kind == GatewayDraftKind::Shield {
            PublicActionKind::Shield
        } else {
            PublicActionKind::Send
        },
        asset,
        profile,
        gas_fee,
        Some(bundle.standard),
        Some(ceiling),
    )
    .map_err(|_| "This action cannot be estimated on the selected network.".to_owned())?;
    let amount = draft_amount(
        &input,
        asset,
        decimals,
        balance,
        native_balance,
        costs.maximum_cost,
    )?;
    let fee_display = PublicActionFeeDisplay::from_estimate(
        input.chain_id,
        Some(costs),
        (input.kind == GatewayDraftKind::Shield
            && asset == PublicAssetId::Native
            && profile == PublicShieldTransactionProfile::Railway)
            .then_some(6_000_000),
        (input.kind == GatewayDraftKind::Shield)
            .then(|| (asset, public_shield_protocol_fee_amount(amount))),
        &registry,
        &anchors,
    );
    let account_label = account
        .label
        .clone()
        .unwrap_or_else(|| railgun_ui::short_address(&account.address));
    let estimate = GatewayDraftEstimate {
        amount: format_send_amount_input(amount, Some(decimals)),
        amount_label: format!(
            "{} {symbol}",
            public_balance_amount_label(&PublicBalanceAmount::Available(amount), decimals)
        ),
        amount_value: match asset {
            PublicAssetId::Native => anchors.cached_native_usd_micro_value(input.chain_id, amount),
            PublicAssetId::Erc20(token) => {
                anchors.cached_token_usd_micro_value(input.chain_id, token, amount)
            }
        }
        .map(railgun_ui::format_usd_micro_value),
        max_amount_label: (asset == PublicAssetId::Native)
            .then(|| public_action_max_amount_after_reserve(balance, costs.maximum_cost))
            .flatten()
            .map(|amount| {
                format!(
                    "{} {symbol} after est. gas",
                    public_balance_amount_label(&PublicBalanceAmount::Available(amount), decimals)
                )
            }),
        recipient: recipient.map(|address| address.to_checksum(None)),
        gas_limit: fee_display.gas_limit.map(format_gas_limit),
        expected_gas_cost: fee_display.expected_gas_cost.clone().unwrap_or_default(),
        maximum_gas_cost: fee_display.maximum_gas_cost.clone().unwrap_or_default(),
        show_maximum_gas_cost: fee_display.show_maximum_gas_cost,
        protocol_fee: fee_display.protocol_fee.clone(),
        protocol_fee_label: fee_display
            .protocol_fee
            .as_ref()
            .map(|_| public_action_protocol_fee_label(RAILGUN_PROTOCOL_FEE_BPS)),
    };
    // Fee/Max presentation is useful before a destination is chosen. It never admits
    // submission: only a fully prepared draft can become Ready.
    if input.kind == GatewayDraftKind::Send && recipient.is_none() {
        return Ok((None, estimate));
    }
    let prepared = match input.kind {
        GatewayDraftKind::Send => {
            let recipient = recipient.ok_or("Enter a recipient")?;
            PreparedDraft::Send(Box::new(PublicSendDraft {
                chain_id: input.chain_id,
                asset,
                asset_label: symbol,
                asset_icon_path: icon,
                asset_decimals: Some(decimals),
                public_account_uuid: account.public_account_uuid.into(),
                public_account_label: account_label,
                public_account_source: account.source,
                view_session: wallet,
                vault_store: store,
                amount,
                recipient,
                intent: PublicTransactionIntent::Transfer {
                    asset,
                    amount,
                    recipient,
                },
                advanced_estimate: None,
                gas_fee,
                fee_display,
                gateway_execution: None,
            }))
        }
        GatewayDraftKind::Shield => PreparedDraft::Shield(Box::new(PublicShieldDraft {
            chain_id: input.chain_id,
            asset,
            asset_label: symbol,
            asset_icon_path: icon,
            asset_decimals: Some(decimals),
            public_account_uuid: account.public_account_uuid.into(),
            public_account_label: account_label,
            public_account_source: account.source,
            view_session: wallet,
            vault_store: store,
            amount,
            profile,
            gas_fee,
            gas_fee_mode,
            authorized_fee_ceiling: ceiling,
            fee_display,
            gateway_execution: None,
        })),
    };
    Ok((Some(prepared), estimate))
}

fn draft_amount(
    input: &GatewayDraftInput,
    asset: PublicAssetId,
    decimals: u8,
    balance: U256,
    native_balance: U256,
    maximum_cost: U256,
) -> Result<U256, String> {
    let amount = if input.max {
        if asset == PublicAssetId::Native {
            balance
                .checked_sub(maximum_cost)
                .ok_or("Not enough native balance for gas")?
        } else {
            balance
        }
    } else {
        parse_send_amount(&input.amount, Some(decimals)).map_err(|error| error.to_string())?
    };
    if amount.is_zero() {
        return Err("Amount must be greater than zero".into());
    }
    if amount > balance {
        return Err("Amount exceeds the available balance".into());
    }
    let needed_native = if asset == PublicAssetId::Native {
        amount
            .checked_add(maximum_cost)
            .ok_or("Amount is too large")?
    } else {
        maximum_cost
    };
    if needed_native > native_balance {
        return Err("Not enough native balance for the maximum gas cost".into());
    }
    Ok(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_wallet() -> (
        std::path::PathBuf,
        DesktopVaultStore,
        Arc<DesktopViewSession>,
    ) {
        use wallet_ops::vault::{KdfParams, WalletSource};
        let path =
            std::env::temp_dir().join(format!("gateway-drafts-{:032x}", rand::random::<u128>()));
        let store = DesktopVaultStore::open(path.clone()).unwrap();
        let password = "synthetic draft test password";
        store
            .create_vault_with_params(password, KdfParams::new(1024, 1, 1))
            .unwrap();
        let metadata = store
            .new_wallet_metadata(password, "wallet", 0, WalletSource::Imported, "Wallet")
            .unwrap();
        store.import_wallet_mnemonic_with_metadata(password, "wallet", 0, "english",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about", &metadata).unwrap();
        let wallet = Arc::new(store.load_view_session(password, "wallet").unwrap());
        (path, store, wallet)
    }

    #[test]
    fn selected_private_contact_is_reloaded_and_never_silently_retargeted() {
        use wallet_ops::gateway::{GatewayPrivateDraftInput, GatewayPrivateDraftKind};
        let (path, store, wallet) = synthetic_wallet();
        let entry = store
            .add_public_address_book_entry_for_session(
                &wallet,
                "Synthetic contact",
                "0x1111111111111111111111111111111111111111",
            )
            .unwrap();
        let mut input = GatewayPrivateDraftInput::default();
        input.kind = GatewayPrivateDraftKind::Unshield;
        input.address_book_entry = Some(entry.entry_uuid.clone());
        input.recipient = entry.address.to_checksum(None);
        assert_eq!(
            private::resolve_private_draft_contact(&store, &wallet, &input).unwrap(),
            input.recipient
        );
        let changed = store
            .update_public_address_book_entry_for_session(
                &wallet,
                &entry.entry_uuid,
                "Renamed contact",
                "0x2222222222222222222222222222222222222222",
            )
            .unwrap();
        assert!(private::resolve_private_draft_contact(&store, &wallet, &input).is_err());
        input.recipient = changed.address.to_checksum(None);
        assert_eq!(
            private::resolve_private_draft_contact(&store, &wallet, &input).unwrap(),
            input.recipient
        );
        input.address_book_entry = Some("removed-contact".into());
        assert!(private::resolve_private_draft_contact(&store, &wallet, &input).is_err());
        drop(wallet);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn private_drafts_share_bounded_peer_slots_and_reject_duplicate_or_stale_edits() {
        use wallet_ops::gateway::{GatewayPrivateDraftInput, GatewayPrivateDraftKind};
        let (path, store, wallet) = synthetic_wallet();
        let private_input = || {
            let mut input = GatewayPrivateDraftInput::default();
            input.wallet = "wallet".into();
            input.chain_id = 1;
            input.kind = GatewayPrivateDraftKind::PrivateSend;
            GatewayDraftPayload::Private(input)
        };
        let mut book = GatewayDraftBook::default();
        assert!(book.create("peer", "create".into(), private_input(), wallet.clone(), 1));
        let id = book.records["peer"].view.draft_id.clone();
        assert!(!book.create("peer", "create".into(), private_input(), wallet.clone(), 1));
        assert_eq!(book.records["peer"].view.draft_id, id);
        assert!(!book.update("other", &id, 1, private_input(), &wallet, 1));
        assert!(!book.update("peer", &id, 0, private_input(), &wallet, 1));
        assert!(!book.update("peer", &id, 1, private_input(), &wallet, 2));
        assert!(book.update("peer", &id, 2, private_input(), &wallet, 1));
        assert!(!book.update("peer", &id, 1, private_input(), &wallet, 1));
        let execution = GatewayDraftExecution::private("private-execution".into());
        assert!(execution.approve_review());
        assert!(execution.start());
        execution.bind_generation(1);
        book.records.get_mut("peer").unwrap().execution = Some(execution.clone());
        book.stopped(1); // The Public form may have the same generation counter.
        assert_eq!(execution.snapshot().status, GatewayDraftStatus::InProgress);
        assert!(!book.update("peer", &id, 3, private_input(), &wallet, 1));
        assert!(!book.create("peer", "another".into(), private_input(), wallet.clone(), 1));
        for ix in 1..MAX_DRAFT_PEERS {
            assert!(book.create(
                &format!("peer-{ix}"),
                "create".into(),
                private_input(),
                wallet.clone(),
                1
            ));
        }
        assert!(!book.create(
            "overflow",
            "create".into(),
            private_input(),
            wallet.clone(),
            1
        ));
        book.reconcile_wallet(Some(&wallet), 2);
        assert!(book.records.is_empty());
        drop(book);
        drop(wallet);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[tokio::test]
    async fn dismiss_retires_editable_drafts_without_removing_live_executions() {
        use wallet_ops::gateway::GatewayPrivateDraftInput;
        let (path, store, wallet) = synthetic_wallet();
        let input = GatewayDraftPayload::Private(GatewayPrivateDraftInput::default());
        let mut book = GatewayDraftBook::default();
        assert!(book.create("peer", "first".into(), input.clone(), wallet.clone(), 1));
        let id = book.records["peer"].view.draft_id.clone();
        let estimate = tokio::spawn(std::future::pending::<()>());
        book.records.get_mut("peer").unwrap().estimation = Some(estimate.abort_handle());
        assert!(!book.dismiss("other-peer", &id));
        assert!(!book.dismiss("peer", "another-draft"));
        assert!(!estimate.is_finished());
        assert!(book.dismiss("peer", &id));
        assert!(estimate.await.unwrap_err().is_cancelled());
        assert!(!book.dismiss("peer", &id));
        assert!(book.create("peer", "next".into(), input, wallet.clone(), 1));
        let next_id = book.records["peer"].view.draft_id.clone();
        assert!(!book.dismiss("peer", &id));
        let execution = GatewayDraftExecution::private("execution".into());
        book.records.get_mut("peer").unwrap().execution = Some(execution.clone());
        // The editable view can lag behind the native approval/execution owner.
        assert!(!book.dismiss("peer", &next_id));
        assert!(execution.approve_review());
        assert!(execution.start());
        assert!(!book.dismiss("peer", &next_id));
        execution.stopped();
        assert!(book.dismiss("peer", &next_id));
        drop(book);
        drop(wallet);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn private_gas_quotes_survive_form_edits_but_not_chain_or_delivery_changes() {
        use wallet_ops::gateway::{
            GatewayPrivateDelivery, GatewayPrivateDraftInput, GatewayPrivateFunding,
            GatewayPrivateGasFee, GatewayPrivateSelfBroadcastInput,
        };
        let (path, store, wallet) = synthetic_wallet();
        let mut input = GatewayPrivateDraftInput::default();
        input.wallet = "wallet".into();
        input.chain_id = 1;
        input.delivery = GatewayPrivateDelivery::SelfBroadcast {
            delivery: GatewayPrivateSelfBroadcastInput::SelfBroadcast {
                signer: None,
                funding: GatewayPrivateFunding::PublicBalance {},
                fee: GatewayPrivateGasFee::Auto {},
            },
        };
        let mut book = GatewayDraftBook::default();
        assert!(book.create(
            "peer",
            "create".into(),
            GatewayDraftPayload::Private(input.clone()),
            wallet.clone(),
            1,
        ));
        let id = book.records["peer"].view.draft_id.clone();
        book.records.get_mut("peer").unwrap().view.gas_quote =
            Some(GatewayDraftGasQuote::new("2".into(), "0.1".into()));
        input.amount = "1".into();
        assert!(book.update(
            "peer",
            &id,
            1,
            GatewayDraftPayload::Private(input.clone()),
            &wallet,
            1
        ));
        assert!(book.records["peer"].view.gas_quote.is_some());
        assert!(book.records["peer"].prepared.is_none());

        input.chain_id = 137;
        assert!(book.update(
            "peer",
            &id,
            2,
            GatewayDraftPayload::Private(input.clone()),
            &wallet,
            1
        ));
        assert!(book.records["peer"].view.gas_quote.is_none());

        book.records.get_mut("peer").unwrap().view.gas_quote =
            Some(GatewayDraftGasQuote::new("3".into(), "1".into()));
        input.delivery = GatewayPrivateDraftInput::default().delivery;
        assert!(book.update(
            "peer",
            &id,
            3,
            GatewayDraftPayload::Private(input),
            &wallet,
            1
        ));
        assert!(book.records["peer"].view.gas_quote.is_none());
        drop(book);
        drop(wallet);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn max_reserves_the_selected_fee_ceiling_and_token_max_still_requires_native_gas() {
        let input = GatewayDraftInput {
            account: "public".into(),
            chain_id: 1,
            kind: GatewayDraftKind::Send,
            asset: "native".into(),
            amount: String::new(),
            recipient: String::new(),
            address_book_entry: None,
            fee: GatewayDraftFee::Normal,
            mimic_railway: true,
            max: true,
        };
        let quote = PublicActionGasFeeQuote {
            rpc_gas_price: 100,
            current_base_fee_per_gas: Some(80),
            suggested_max_fee_per_gas: 100,
            suggested_max_priority_fee_per_gas: 4,
        };
        let balance = U256::from(1_000_000_000_000_u64);
        let mut previous = U256::MAX;
        for fee in [
            GatewayDraftFee::Slow,
            GatewayDraftFee::Normal,
            GatewayDraftFee::Fast,
        ] {
            let fee =
                draft_gas_fee(&fee, quote, PublicShieldTransactionProfile::Railoxide, 1).unwrap();
            let costs = estimate_public_action_gas_cost_with_profile_and_ceiling(
                1,
                None,
                PublicActionKind::Send,
                PublicAssetId::Native,
                PublicShieldTransactionProfile::Railoxide,
                fee,
                Some(quote),
                Some(fee),
            )
            .unwrap();
            let amount = draft_amount(
                &input,
                PublicAssetId::Native,
                18,
                balance,
                balance,
                costs.maximum_cost,
            )
            .unwrap();
            assert_eq!(amount + costs.maximum_cost, balance);
            assert!(amount < previous);
            previous = amount;
        }
        let token = PublicAssetId::Erc20(Address::repeat_byte(1));
        assert_eq!(
            draft_amount(&input, token, 6, balance, U256::from(7), U256::from(7)).unwrap(),
            balance
        );
        assert!(draft_amount(&input, token, 6, balance, U256::from(6), U256::from(7)).is_err());
        assert!(
            draft_amount(&input, PublicAssetId::Native, 18, balance, balance, balance).is_err()
        );
        assert!(
            draft_gas_fee(
                &GatewayDraftFee::Custom {
                    max_fee_gwei: "1".into(),
                    priority_fee_gwei: "2".into()
                },
                quote,
                PublicShieldTransactionProfile::Railoxide,
                1
            )
            .is_err()
        );
    }
}
