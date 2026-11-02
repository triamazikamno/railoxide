//! Tron Send: the form a Tron asset tile's Send… opens, its review and its progress, on the
//! shared Public action components. `wallet_ops::tron::send` owns the draft; this module renders
//! it and passes the user's decisions.
//!
//! The send runs as one task. Stop aborts it, which before its handoff signs and broadcasts
//! nothing, as for EVM Public Send. After the handoff only solidified blocks decide the outcome.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use eyre::WrapErr as _;
use gpui::{
    AnyElement, App, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Pixels, Styled as _, Window, div, prelude::FluentBuilder as _, rgb,
};
use gpui_component::{
    Disableable as _, Sizable as _, WindowExt as _,
    alert::Alert,
    button::ButtonVariants as _,
    input::{InputEvent, InputState},
};
use tokio::sync::mpsc;
use ui::controls::{app_button, app_muted_text};
use ui::dialog::WalletDialogExt as _;
use ui::fees::estimated_network_fees;
use ui::recipient_picker::RecipientPickerEvent;
use ui::theme::{self, APP_MONO_FONT_FAMILY};
use wallet_ops::bridge::NonEvmNetwork;
use wallet_ops::tron::TRON_CHAIN_ID;
use wallet_ops::tron::address::encode_tron_address;
use wallet_ops::tron::confirmation::{
    TronBlockReads, TronReads, TronSendFailure, TronSendHandoff, TronSendOutcome,
    tron_send_refresh_accounts,
};
use wallet_ops::tron::send::{
    TronSendAmount, TronSendAsset, TronSendDraft, TronSendError, TronSendReads, TronSendReview,
    TronSendSubmission, parse_tron_receiver, prepare_tron_send, submit_tron_send,
    tron_receiver_suggestions,
};
use wallet_ops::tron::transaction::TronAsset;
use wallet_ops::vault::{
    DesktopVaultStore, DesktopViewSession, PublicAccountStatus, TronAccountSource,
};
use wallet_ops::{
    DesktopPrivateSpendAuthorization, OperationHttpClientSlot, PublicActionProgressStep,
    PublicActionSessionEventSender, PublicAssetId, TokenAnchorRateCache, parse_send_amount,
};

use super::public_account_display_label;
use super::tron::tron_list_source;
use crate::assets::WalletIconSource;
use crate::root::private_action::{
    RecipientOption, RecipientOptionSource, RecipientPickerState, RecipientRules,
    form_recipient_picker, recipient_suggestion_to_confirm, recipient_suggestions_for_input,
};
use crate::root::public_action::{
    PublicActionMode, format_gas_limit, public_action_accepts_update,
    public_action_closed_status_step, public_action_form, public_action_title_row,
    render_amount_input, render_public_action_active_status_notice,
};
use crate::root::public_balances::{public_asset_icon_path, public_balance_entry_for_chain};
use crate::root::spend_authorization::{
    SpendAuthorizationIntent, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
    is_spend_authorization_failure_error,
};
use crate::root::{
    PUBLIC_ACTION_DIALOG_WIDTH, WalletRoot, dialog_max_height, format_send_amount_input,
    format_value_with_usd_label, labeled_field, new_text_input, secondary_dialog_content_width,
};

/// How long the form waits after an edit before it prepares a review.
const PREPARE_DELAY: Duration = Duration::from_millis(400);
const TRON_RECEIVERS: RecipientRules = RecipientRules::NonEvm(NonEvmNetwork::Tron);
const NOT_A_TRON_ADDRESS: &str = "A Tron address is needed.";
const SENDER_MISSING: &str = "Send is unavailable: the account must first receive TRX.";
const FEES_ROSE: &str = "The fees rose since your review. Review the new fees and approve again.";

// A send's gate: Stop and the handoff each claim it from open, and whichever is first wins.
const GATE_OPEN: u8 = 0;
const GATE_STOPPED: u8 = 1;
const GATE_HANDED_OFF: u8 = 2;

/// A Send's reads and blocks over Tron's route. Tests and the UI fixture replace them.
pub(in crate::root) trait TronSendBackend: TronSendReads + TronBlockReads {}

impl<T: TronSendReads + TronBlockReads> TronSendBackend for T {}

/// Authorization and any hardware approval, ending in the account's signer.
type TronSigner = Pin<Box<dyn Future<Output = eyre::Result<PrivateKeySigner>> + Send>>;

/// Opens a Send's operation client and yields its reads over it.
type TronSendBackendOpen =
    Pin<Box<dyn Future<Output = eyre::Result<Arc<dyn TronSendBackend>>> + Send>>;

pub(in crate::root) struct TronSendState {
    pub(in crate::root) receiver_input: Entity<InputState>,
    pub(in crate::root) amount_input: Entity<InputState>,
    pub(in crate::root) form: Option<TronSendForm>,
    /// Replaces the reads over Tron's route.
    pub(in crate::root) backend: Option<Arc<dyn TronSendBackend>>,
    /// The Public action progress generation the last Tron Send ran under.
    pub(in crate::root) generation: Option<u64>,
    /// The last Tron Send's gate between Stop and its handoff.
    pub(in crate::root) gate: Arc<AtomicU8>,
    /// The last review preparation issued, across forms, so a late one never matches a new form.
    pub(in crate::root) prepare_seq: u64,
    /// What a completed send moved, which its progress shows.
    pub(in crate::root) completed: Option<Arc<str>>,
}

impl TronSendState {
    pub(in crate::root) fn new(window: &mut Window, cx: &mut Context<'_, WalletRoot>) -> Self {
        let receiver_input = new_text_input(window, cx, "Tron address (T…)");
        let amount_input = new_text_input(window, cx, "amount");
        cx.subscribe_in(
            &receiver_input,
            window,
            |root, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => root.tron_receiver_edited(window, cx),
                InputEvent::PressEnter { .. } => root.confirm_tron_receiver_suggestion(window, cx),
                _ => {}
            },
        )
        .detach();
        cx.subscribe_in(
            &amount_input,
            window,
            |root, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    if let Some(form) = root.tron_public.send.form.as_mut() {
                        form.max = false;
                    }
                    root.tron_send_inputs_changed(window, cx);
                }
            },
        )
        .detach();
        Self {
            receiver_input,
            amount_input,
            form: None,
            backend: None,
            generation: None,
            gate: Arc::default(),
            prepare_seq: 0,
            completed: None,
        }
    }
}

/// The open Send form.
pub(in crate::root) struct TronSendForm {
    pub(in crate::root) account_uuid: Arc<str>,
    pub(in crate::root) sender: Address,
    pub(in crate::root) source: TronAccountSource,
    pub(in crate::root) asset: TronSendAsset,
    pub(in crate::root) symbol: String,
    pub(in crate::root) decimals: u8,
    pub(in crate::root) icon: Option<WalletIconSource>,
    /// Whether the sender exists on chain; `None` until checked.
    pub(in crate::root) sender_exists: Option<bool>,
    pub(in crate::root) receiver_value: String,
    pub(in crate::root) receiver_picker: RecipientPickerState,
    /// Max on TRX: the review sets the amount, the balance less the maximum fee.
    pub(in crate::root) max: bool,
    pub(in crate::root) review: Option<TronSendReview>,
    pub(in crate::root) preparing: bool,
    pub(in crate::root) prepare_id: u64,
    pub(in crate::root) error: Option<Arc<str>>,
    /// This send's own route, from its first read to its outcome.
    pub(in crate::root) operation: OperationHttpClientSlot,
}

/// A reviewed Tron Send awaiting authorization.
#[derive(Clone)]
pub(in crate::root) struct TronSendIntent {
    pub(in crate::root) account_uuid: Arc<str>,
    pub(in crate::root) account_label: String,
    pub(in crate::root) source: TronAccountSource,
    pub(in crate::root) symbol: String,
    pub(in crate::root) decimals: u8,
    pub(in crate::root) icon: Option<WalletIconSource>,
    pub(in crate::root) review: TronSendReview,
    /// The form's operation client.
    pub(in crate::root) operation: OperationHttpClientSlot,
}

fn trx(sun: u64) -> String {
    railgun_ui::tron_native_currency().format_amount(U256::from(sun))
}

/// The energy and bandwidth `review`'s transfer needs, what the account has and the TRX burned
/// for any shortfall, then the note on a receiver that doesn't hold the token yet. Account
/// creation replaces the bandwidth charge, and its own note covers it.
fn resource_lines(review: &TronSendReview, symbol: &str) -> Vec<gpui::Div> {
    let burns = |sun: u64| {
        if sun == 0 {
            String::new()
        } else {
            format!(", burns {}", trx(sun))
        }
    };
    let mut lines = Vec::new();
    if matches!(review.transfer.asset, TronAsset::Trc20 { .. }) {
        lines.push(app_muted_text(format!(
            "Energy: {} needed, {} available{}",
            format_gas_limit(review.fees.energy_cap),
            format_gas_limit(review.resources.energy),
            burns(review.fees.expected_energy)
        )));
    }
    if !review.creates_account {
        lines.push(app_muted_text(format!(
            "Bandwidth: {} needed, {} free, {} staked{}",
            format_gas_limit(review.bandwidth_bytes),
            format_gas_limit(review.resources.free_bandwidth),
            format_gas_limit(review.resources.staked_bandwidth),
            burns(review.fees.expected_bandwidth)
        )));
    }
    if review.receiver_lacks_token {
        lines.push(
            app_muted_text(format!(
                "The receiver doesn't hold {symbol} yet, so this transfer needs more energy. \
                 Sending to an address that already holds {symbol} costs less."
            ))
            .debug_selector(|| "wallet-tron-send-receiver-note".into()),
        );
    }
    lines
        .into_iter()
        .map(gpui::Styled::whitespace_normal)
        .collect()
}

/// A fee in TRX with its USD value, as EVM fees show theirs.
fn trx_fee(sun: u64, rates: &TokenAnchorRateCache) -> String {
    let amount = U256::from(sun);
    format_value_with_usd_label(
        trx(sun),
        amount,
        Some(railgun_ui::tron_native_currency().decimals),
        rates.cached_native_usd_micro_value(TRON_CHAIN_ID, amount),
        false,
    )
}

const fn public_asset(asset: TronSendAsset) -> PublicAssetId {
    match asset {
        TronSendAsset::Trx => PublicAssetId::Native,
        TronSendAsset::Trc20(token) => PublicAssetId::Erc20(token),
    }
}

fn review_amount(review: &TronSendReview) -> U256 {
    match review.transfer.asset {
        TronAsset::Trx { amount_sun } => U256::from(amount_sun),
        TronAsset::Trc20 { amount, .. } => amount,
    }
}

/// `text` with its first letter capitalized, for an error that reads as a clause.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn tron_send_authorization_summary(
    intent: &TronSendIntent,
    rates: &TokenAnchorRateCache,
) -> SpendAuthorizationSummary {
    let review = &intent.review;
    let mut rows = vec![
        SpendAuthorizationSummaryRow::new(
            "Amount",
            format!(
                "{} {}",
                format_send_amount_input(review_amount(review), Some(intent.decimals)),
                intent.symbol
            ),
        )
        .with_icon(intent.icon.clone()),
        SpendAuthorizationSummaryRow::new("From", intent.account_label.clone()),
        SpendAuthorizationSummaryRow::new(
            "Receiver",
            encode_tron_address(review.transfer.receiver),
        )
        .with_shortened_copyable(),
    ];
    if review.creates_account {
        rows.push(SpendAuthorizationSummaryRow::new(
            "Receiver account",
            "Created by this send",
        ));
    }
    rows.extend([
        SpendAuthorizationSummaryRow::new("Expected fee", trx_fee(review.fees.expected, rates)),
        SpendAuthorizationSummaryRow::new("Maximum fee", trx_fee(review.fees.maximum, rates)),
    ]);
    let authorize = match intent.source {
        TronAccountSource::HardwareDerived { .. } => {
            "Approve on your hardware wallet to derive this account's key. The app then signs \
             the Tron transaction; the device doesn't."
        }
        TronAccountSource::Derived { .. } | TronAccountSource::Imported => {
            "Enter your vault password to authorize this Tron send."
        }
    };
    SpendAuthorizationSummary::new("Tron send", authorize, rows)
}

impl WalletRoot {
    /// Tron's reads for a Send over `operation`, unless a test or the UI fixture replaced them.
    /// `None` without a Tron route. The future opens the client, so await it before any read.
    fn tron_send_backend(
        &self,
        operation: &OperationHttpClientSlot,
    ) -> Option<TronSendBackendOpen> {
        if let Some(backend) = &self.tron_public.send.backend {
            return Some(Box::pin(std::future::ready(eyre::Ok(Arc::clone(backend)))));
        }
        let route = self.tron_public.route.clone()?;
        let (http, operation) = (self.http.clone(), operation.clone());
        Some(Box::pin(async move {
            let reads: Arc<dyn TronSendBackend> =
                Arc::new(TronReads::new(http, &route, operation.client().await?));
            eyre::Ok(reads)
        }))
    }

    /// The form account's balance of `asset` in the latest refresh, zero included.
    fn tron_send_balance(&self, form: &TronSendForm, asset: PublicAssetId) -> Option<U256> {
        let status = self
            .tron_public
            .accounts
            .iter()
            .find(|account| *account.tron_account_uuid == *form.account_uuid)?
            .status;
        public_balance_entry_for_chain(
            self.tron_public.snapshot.as_deref(),
            TRON_CHAIN_ID,
            &form.account_uuid,
            asset,
            status,
        )?
        .amount
        .amount()
    }

    /// The form account's TRX in sun in the latest refresh.
    fn tron_send_trx_balance(&self, form: &TronSendForm) -> Option<u64> {
        self.tron_send_balance(form, PublicAssetId::Native)
            .and_then(|balance| u64::try_from(balance).ok())
    }

    /// Whether the Public action progress is the last Tron Send's.
    pub(in crate::root) fn tron_send_owns_progress(&self) -> bool {
        self.tron_public.send.generation == Some(self.public_form.action_generation)
            && !self.public_form.action_progress.is_empty()
    }

    /// Whether a running Tron Send already won its handoff, so only its outcome may end it.
    /// Otherwise a running send is stopped from handing off, so the caller must end it.
    pub(in crate::root) fn claim_tron_send_stop_unless_handed_off(&self) -> bool {
        self.public_form.sending
            && self.tron_send_owns_progress()
            && self
                .tron_public
                .send
                .gate
                .compare_exchange(GATE_OPEN, GATE_STOPPED, Ordering::SeqCst, Ordering::SeqCst)
                .is_err_and(|state| state == GATE_HANDED_OFF)
    }

    pub(in crate::root) fn open_tron_send_dialog(
        &mut self,
        uuid: Arc<str>,
        asset: PublicAssetId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self
            .tron_public
            .accounts
            .iter()
            .find(|account| *account.tron_account_uuid == *uuid)
            .cloned()
        else {
            return;
        };
        let Some(entry) = self
            .public_account_visible_balances(&uuid, account.status)
            .into_iter()
            .find(|entry| entry.asset.id == asset)
        else {
            return;
        };
        window.close_all_dialogs(cx);
        let (send_asset, icon) = match asset {
            PublicAssetId::Native => (
                TronSendAsset::Trx,
                Some(WalletIconSource::embedded(railgun_ui::TRON_ICON_ASSET_PATH)),
            ),
            PublicAssetId::Erc20(token) => (
                TronSendAsset::Trc20(token),
                public_asset_icon_path(TRON_CHAIN_ID, asset, Some(&self.effective_token_registry)),
            ),
        };
        if !self.public_action_in_flight() {
            self.clear_public_action_progress_state();
        }
        for input in [
            &self.tron_public.send.receiver_input,
            &self.tron_public.send.amount_input,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        let title = format!("Send {}", entry.asset.symbol);
        self.tron_public.send.form = Some(TronSendForm {
            account_uuid: uuid,
            sender: account.address,
            source: account.source,
            asset: send_asset,
            symbol: entry.asset.symbol,
            decimals: entry.asset.decimals,
            icon: icon.clone(),
            sender_exists: None,
            receiver_value: String::new(),
            receiver_picker: RecipientPickerState::default(),
            max: false,
            review: None,
            preparing: false,
            prepare_id: 0,
            error: None,
            operation: OperationHttpClientSlot::new(self.http.clone()),
        });
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(PUBLIC_ACTION_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(public_action_title_row(title.clone(), icon.clone()))
                .on_ok(|_, _, _| false)
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| root.close_tron_send_form(window, cx));
                })
                .child(root.read(cx).render_tron_send_form(&root, content_width))
        });
        self.check_tron_sender_exists(cx);
        #[cfg(debug_assertions)]
        self.stage_tron_send_ui_fixture(window, cx);
        cx.defer_in(window, |root, window, cx| {
            root.tron_public
                .send
                .receiver_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        });
        cx.notify();
    }

    pub(in crate::root) fn close_tron_send_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.tron_public.send.form = None;
        for input in [
            &self.tron_public.send.receiver_input,
            &self.tron_public.send.amount_input,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        if !self.public_form.sending && self.tron_send_owns_progress() {
            self.clear_public_action_progress_state();
        }
    }

    /// Asks whether the sender exists on chain. A failed check leaves it to the review, which
    /// checks again.
    fn check_tron_sender_exists(&self, cx: &Context<'_, Self>) {
        let Some(form) = self.tron_public.send.form.as_ref() else {
            return;
        };
        let Some(reads) = self.tron_send_backend(&form.operation) else {
            return;
        };
        let (uuid, sender) = (Arc::clone(&form.account_uuid), form.sender);
        let join = self
            .runtime
            .spawn(async move { reads.await.ok()?.account_exists(sender).await.ok() });
        cx.spawn(async move |this, cx| {
            let Ok(Some(exists)) = join.await else {
                return;
            };
            let _ = this.update(cx, |root, cx| {
                if let Some(form) = root
                    .tron_public
                    .send
                    .form
                    .as_mut()
                    .filter(|form| form.account_uuid == uuid)
                {
                    form.sender_exists = Some(exists);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The address book's Tron entries and the wallet's other active Tron accounts.
    fn tron_receiver_options(&self) -> Vec<RecipientOption> {
        let Some(form) = self.tron_public.send.form.as_ref() else {
            return Vec::new();
        };
        let active: Vec<_> = self
            .tron_public
            .accounts
            .iter()
            .filter(|account| account.status == PublicAccountStatus::Active)
            .cloned()
            .collect();
        tron_receiver_suggestions(&self.non_evm_address_book, &active, form.sender)
            .into_iter()
            .map(|suggestion| {
                let address = encode_tron_address(suggestion.address);
                let own = active
                    .iter()
                    .any(|account| account.address == suggestion.address);
                RecipientOption {
                    label: Arc::from(
                        suggestion
                            .label
                            .unwrap_or_else(|| railgun_ui::short_non_evm(&address)),
                    ),
                    address: Arc::from(address),
                    source: if own {
                        RecipientOptionSource::PublicAccount
                    } else {
                        RecipientOptionSource::NonEvmAddressBook
                    },
                }
            })
            .collect()
    }

    fn tron_receiver_edited(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let options = self.tron_receiver_options();
        let value = self
            .tron_public
            .send
            .receiver_input
            .read(cx)
            .value()
            .to_string();
        let Some(form) = self.tron_public.send.form.as_mut() else {
            return;
        };
        if form.receiver_value == value {
            return;
        }
        form.receiver_value = value;
        form.receiver_picker.set(recipient_suggestions_for_input(
            TRON_RECEIVERS,
            &options,
            &form.receiver_value,
        ));
        self.tron_send_inputs_changed(window, cx);
    }

    pub(in crate::root) fn tron_receiver_picker_event(
        &mut self,
        event: &RecipientPickerEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let RecipientPickerEvent::Select(receiver) = event {
            self.set_tron_receiver(receiver, window, cx);
            return;
        }
        let options = self.tron_receiver_options();
        let Some(form) = self.tron_public.send.form.as_mut() else {
            return;
        };
        if form
            .receiver_picker
            .apply(TRON_RECEIVERS, &options, &form.receiver_value, event)
        {
            cx.notify();
        }
    }

    /// Enter in the receiver input picks the highlighted suggestion.
    pub(in crate::root) fn confirm_tron_receiver_suggestion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self
            .tron_public
            .send
            .form
            .as_ref()
            .filter(|form| form.receiver_picker.open())
        else {
            return;
        };
        let Some(receiver) = recipient_suggestion_to_confirm(
            TRON_RECEIVERS,
            &self.tron_receiver_options(),
            &form.receiver_value,
            form.receiver_picker.index(),
        ) else {
            return;
        };
        self.set_tron_receiver(&receiver, window, cx);
    }

    pub(in crate::root) fn set_tron_receiver(
        &mut self,
        receiver: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.tron_public.send.form.as_mut() else {
            return;
        };
        form.receiver_picker.set((false, None));
        if form.receiver_value == receiver {
            cx.notify();
            return;
        }
        receiver.clone_into(&mut form.receiver_value);
        self.tron_public
            .send
            .receiver_input
            .update(cx, |input, cx| {
                input.set_value(receiver.to_owned(), window, cx);
            });
        // Programmatic input changes don't emit InputEvent::Change.
        self.tron_send_inputs_changed(window, cx);
    }

    pub(in crate::root) fn set_tron_send_amount_to_max(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.tron_public.send.form.as_ref() else {
            return;
        };
        let balance = self.tron_send_balance(form, public_asset(form.asset));
        let Some(form) = self.tron_public.send.form.as_mut() else {
            return;
        };
        match form.asset {
            TronSendAsset::Trx => form.max = true,
            TronSendAsset::Trc20(_) => {
                let Some(balance) = balance else {
                    return;
                };
                let value = format_send_amount_input(balance, Some(form.decimals));
                self.tron_public
                    .send
                    .amount_input
                    .update(cx, |input, cx| input.set_value(value, window, cx));
            }
        }
        self.tron_send_inputs_changed(window, cx);
    }

    /// Drops the review and prepares another once the edits pause.
    fn tron_send_inputs_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let amount_entered = !self
            .tron_public
            .send
            .amount_input
            .read(cx)
            .value()
            .trim()
            .is_empty();
        let send = &mut self.tron_public.send;
        let Some(form) = send.form.as_mut() else {
            return;
        };
        form.review = None;
        form.error = None;
        send.prepare_seq = send.prepare_seq.wrapping_add(1);
        form.prepare_id = send.prepare_seq;
        form.preparing = !form.receiver_value.trim().is_empty() && (amount_entered || form.max);
        let id = form.prepare_id;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(PREPARE_DELAY).await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.prepare_tron_send_review(id, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// The form as a draft: `Ok(None)` while the receiver or the amount is missing or the
    /// receiver isn't an address, which the form already says.
    fn tron_send_draft(&self, cx: &App) -> Result<Option<TronSendDraft>, Arc<str>> {
        let Some(form) = self.tron_public.send.form.as_ref() else {
            return Ok(None);
        };
        let Ok(receiver) = parse_tron_receiver(&form.receiver_value) else {
            return Ok(None);
        };
        let amount = if form.max && form.asset == TronSendAsset::Trx {
            TronSendAmount::Max
        } else {
            let text = self
                .tron_public
                .send
                .amount_input
                .read(cx)
                .value()
                .to_string();
            if text.trim().is_empty() {
                return Ok(None);
            }
            match parse_send_amount(&text, Some(form.decimals)) {
                Ok(amount) if !amount.is_zero() => TronSendAmount::Exact(amount),
                Ok(_) => return Err(Arc::from("Amount must be greater than zero.")),
                Err(error) => return Err(Arc::from(sentence(&error.to_string()))),
            }
        };
        let Some(trx_balance) = self.tron_send_trx_balance(form) else {
            return Err(Arc::from("Refresh this account's balance before sending."));
        };
        Ok(Some(TronSendDraft {
            sender: form.sender,
            receiver,
            asset: form.asset,
            amount,
            trx_balance,
        }))
    }

    fn prepare_tron_send_review(&mut self, id: u64, window: &Window, cx: &mut Context<'_, Self>) {
        if self
            .tron_public
            .send
            .form
            .as_ref()
            .is_none_or(|form| form.prepare_id != id)
        {
            return;
        }
        let draft = self.tron_send_draft(cx);
        let reads = self
            .tron_public
            .send
            .form
            .as_ref()
            .and_then(|form| self.tron_send_backend(&form.operation));
        let Some(form) = self.tron_public.send.form.as_mut() else {
            return;
        };
        let (draft, reads) = match (draft, reads) {
            (Ok(Some(draft)), Some(reads)) => (draft, reads),
            (Ok(None), _) => {
                form.preparing = false;
                cx.notify();
                return;
            }
            (Ok(Some(_)), None) => {
                form.preparing = false;
                form.error = Some(Arc::from(
                    "Tron has no endpoint for this send. Check Tron's settings.",
                ));
                cx.notify();
                return;
            }
            (Err(error), _) => {
                form.preparing = false;
                form.error = Some(error);
                cx.notify();
                return;
            }
        };
        let join = self.runtime.spawn(async move {
            let reads = reads.await?;
            prepare_tron_send(&*reads, &draft).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.apply_tron_send_review(id, result, window, cx);
            });
        })
        .detach();
    }

    fn apply_tron_send_review(
        &mut self,
        id: u64,
        result: Result<Result<TronSendReview, TronSendError>, tokio::task::JoinError>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self
            .tron_public
            .send
            .form
            .as_mut()
            .filter(|form| form.prepare_id == id)
        else {
            return;
        };
        form.preparing = false;
        match result {
            Ok(Ok(review)) => {
                let max_amount = form
                    .max
                    .then(|| format_send_amount_input(review_amount(&review), Some(form.decimals)));
                form.review = Some(review);
                if let Some(amount) = max_amount {
                    self.tron_public
                        .send
                        .amount_input
                        .update(cx, |input, cx| input.set_value(amount, window, cx));
                }
            }
            Ok(Err(TronSendError::SenderMissing)) => form.sender_exists = Some(false),
            Ok(Err(error)) => form.error = Some(Arc::from(sentence(&error.to_string()))),
            Err(error) => {
                form.error = Some(Arc::from(format!("Preparing the review failed: {error}")));
            }
        }
        cx.notify();
    }

    pub(in crate::root) fn submit_tron_send_from_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_action_in_flight() {
            return;
        }
        let Some(form) = self.tron_public.send.form.as_ref() else {
            return;
        };
        let Some(review) = form.review.filter(|review| review.missing_trx.is_none()) else {
            return;
        };
        // The balance changed since this review was prepared.
        if self.tron_send_trx_balance(form) != Some(review.trx_balance) {
            self.tron_send_inputs_changed(window, cx);
            return;
        }
        let account_label = self
            .tron_public
            .listed
            .iter()
            .find(|account| *account.public_account_uuid == *form.account_uuid)
            .and_then(public_account_display_label)
            .unwrap_or_else(|| encode_tron_address(form.sender));
        let intent = TronSendIntent {
            account_uuid: Arc::clone(&form.account_uuid),
            account_label,
            source: form.source,
            symbol: form.symbol.clone(),
            decimals: form.decimals,
            icon: form.icon.clone(),
            review,
            operation: form.operation.clone(),
        };
        self.request_tron_send_authorization(intent, false, window, cx);
    }

    /// Opens the review for approval. A review the fees changed is always shown, even while the
    /// password is remembered.
    fn request_tron_send_authorization(
        &mut self,
        intent: TronSendIntent,
        changed: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.clear_public_action_progress_state();
        let summary =
            tron_send_authorization_summary(&intent, &self.public_broadcaster_anchor_cache);
        let summary = if changed {
            summary.requiring_explicit_review()
        } else {
            summary
        };
        let hardware = matches!(intent.source, TronAccountSource::HardwareDerived { .. });
        let intent = SpendAuthorizationIntent::TronSend(Box::new(intent));
        if hardware {
            Self::open_hardware_public_action_authorization_dialog(intent, summary, window, cx);
        } else {
            self.request_spend_authorization(intent, summary, window, cx);
        }
    }

    fn set_tron_send_error(&mut self, message: &str, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.tron_public.send.form.as_mut() {
            form.error = Some(Arc::from(message));
        }
        cx.notify();
    }

    /// Signs and sends the approved review, then follows it to its outcome.
    pub(in crate::root) fn submit_tron_send_authorized(
        &mut self,
        intent: TronSendIntent,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_action_in_flight() {
            return;
        }
        #[cfg(debug_assertions)]
        if crate::root::private_swap::ui_fixture_tron().is_some() {
            self.set_tron_send_error("The UI fixture sends nothing.", cx);
            return;
        }
        let Some(reads) = self.tron_send_backend(&intent.operation) else {
            self.set_tron_send_error(
                "Tron has no endpoint for this send. Check Tron's settings.",
                cx,
            );
            return;
        };
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let signer = match self.tron_send_signer(&intent, authorization, &event_tx, window, cx) {
            Ok(signer) => signer,
            Err(message) => {
                self.set_tron_send_error(message, cx);
                return;
            }
        };
        self.public_form.sending = true;
        let generation = self.start_public_action_progress_with_steps(
            PublicActionMode::Send,
            vec![PublicActionProgressStep::Send],
            intent.symbol.clone(),
            intent.icon.clone(),
            tron_list_source(intent.source),
            None,
            None,
            true,
        );
        self.tron_public.send.generation = Some(generation);
        self.tron_public.send.gate = Arc::new(AtomicU8::new(GATE_OPEN));
        let gate = Arc::clone(&self.tron_public.send.gate);
        self.tron_public.send.completed = None;
        let (chain_id, wallet) = (self.last_evm_chain, self.selected_wallet_id.clone());
        let progress_tx = Self::follow_public_action_progress(
            generation,
            chain_id,
            wallet.clone(),
            event_rx,
            None,
            window,
            cx,
        );
        let review = intent.review;
        let join = self.runtime.spawn(async move {
            let claim_handoff = move || {
                gate.compare_exchange(
                    GATE_OPEN,
                    GATE_HANDED_OFF,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            };
            // Before the signer, so a failed open prompts no device and sends nothing.
            let reads = reads.await?;
            submit_tron_send(
                &*reads,
                &review,
                signer,
                Some(&event_tx),
                claim_handoff,
                move |update| {
                    let _ = progress_tx.send(update);
                },
            )
            .await
        });
        self.public_form.action_task_abort_handle = Some(join.abort_handle());
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.selected_wallet_id == wallet {
                    root.finish_tron_send(generation, intent, result, window, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_tron_send(
        &mut self,
        generation: u64,
        intent: TronSendIntent,
        result: Result<Result<TronSendSubmission, TronSendError>, tokio::task::JoinError>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !public_action_accepts_update(
            self.public_form.action_generation,
            generation,
            self.public_form.action_stopped,
        ) {
            return;
        }
        self.public_form.sending = false;
        self.public_form.action_task_abort_handle = None;
        match result {
            Ok(Ok(TronSendSubmission::Decided { handoff, outcome })) => {
                self.show_tron_send_outcome(
                    &intent.symbol,
                    intent.decimals,
                    handoff.transfer.receiver,
                    outcome,
                );
                self.refresh_tron_after_send(&handoff, outcome, cx);
            }
            // Nothing was signed. The new review needs approval again, if it is available.
            Ok(Ok(TronSendSubmission::Rereview(review))) => {
                if self.public_form.action_progress_dialog_open {
                    window.close_dialog(cx);
                }
                self.clear_public_action_progress_state();
                if let Some(form) = self.tron_public.send.form.as_mut().filter(|form| {
                    form.account_uuid == intent.account_uuid && form.review == Some(intent.review)
                }) {
                    form.review = Some(review);
                    form.error = Some(Arc::from(FEES_ROSE));
                }
                if review.missing_trx.is_none() {
                    self.request_tron_send_authorization(
                        TronSendIntent { review, ..intent },
                        true,
                        window,
                        cx,
                    );
                }
            }
            Ok(Err(error)) => {
                let message = error.to_string();
                if is_spend_authorization_failure_error(&message) {
                    self.clear_spend_authorization(cx);
                }
                self.discard_active_trezor_session_if_stale(&message, cx);
                self.fail_public_action_progress(generation, message, cx);
                // Every error comes before the handoff.
                self.public_form.action_error_summary = Some(Arc::from(
                    "The send failed before reaching an endpoint, so nothing was broadcast.",
                ));
            }
            Err(error) => {
                self.fail_public_action_progress(
                    generation,
                    format!("Tron send task failed: {error}"),
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// The outcome's concise statement: what a completed send moved, or why it failed.
    fn show_tron_send_outcome(
        &mut self,
        symbol: &str,
        decimals: u8,
        receiver: Address,
        outcome: TronSendOutcome,
    ) {
        let summary = match outcome {
            TronSendOutcome::Complete { amount, .. } => {
                self.tron_public.send.completed = Some(Arc::from(format!(
                    "Sent {} {symbol} to {}.",
                    format_send_amount_input(amount, Some(decimals)),
                    railgun_ui::short_non_evm(&encode_tron_address(receiver)),
                )));
                return;
            }
            TronSendOutcome::Failed {
                reason: TronSendFailure::Failed,
                trx_burned_sun,
                ..
            } => format!("The transaction failed and burned {}.", trx(trx_burned_sun)),
            TronSendOutcome::Failed {
                reason: TronSendFailure::TokenDidNotMove,
                trx_burned_sun,
                ..
            } => format!(
                "The token didn't move. The transaction burned {}.",
                trx(trx_burned_sun)
            ),
            TronSendOutcome::Dropped => {
                "The transaction expired without being included. No TRX was spent.".to_owned()
            }
        };
        self.public_form.action_error_summary = Some(Arc::from(summary));
    }

    /// Refreshes the sender, and the receiver if it is the wallet's, with the sender's reads
    /// fenced at the including block.
    fn refresh_tron_after_send(
        &self,
        handoff: &TronSendHandoff,
        outcome: TronSendOutcome,
        cx: &mut Context<'_, Self>,
    ) {
        let visible: Vec<Address> = self
            .tron_public
            .accounts
            .iter()
            .map(|account| account.address)
            .collect();
        let refreshed = tron_send_refresh_accounts(&handoff.transfer, &visible);
        let block = match outcome {
            TronSendOutcome::Complete { block, .. } | TronSendOutcome::Failed { block, .. } => {
                Some(block)
            }
            TronSendOutcome::Dropped => None,
        };
        if let (Some(scope), Some(sender)) = (
            self.tron_balance_scope(),
            self.tron_public
                .listed
                .iter()
                .find(|account| account.address == handoff.transfer.sender),
        ) {
            let cache = &self.tron_public.balance_cache;
            cache.set_scope(scope.clone());
            cache.fence_account(&scope, sender, false, block);
        }
        let accounts: Vec<String> = self
            .tron_public
            .listed
            .iter()
            .filter(|account| refreshed.contains(&account.address))
            .map(|account| account.public_account_uuid.clone())
            .collect();
        self.schedule_tron_balance_refresh(accounts, cx);
    }

    fn tron_send_signer(
        &mut self,
        intent: &TronSendIntent,
        authorization: DesktopPrivateSpendAuthorization,
        events: &PublicActionSessionEventSender,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<TronSigner, &'static str> {
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            return Err("Wallet vault is locked");
        };
        let uuid = intent.account_uuid.to_string();
        if matches!(intent.source, TronAccountSource::HardwareDerived { .. }) {
            return self.tron_hardware_signer(store, view_session, uuid, events, window, cx);
        }
        let (password, seed) = match authorization {
            DesktopPrivateSpendAuthorization::VaultPassword(password) => (password, None),
            DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed { password, session } => {
                (password, Some(session))
            }
            _ => return Err("Authorize this Tron send with the vault password"),
        };
        Ok(Box::pin(async move {
            let mut grant = store
                .create_spend_grant(password.as_str())
                .wrap_err("authorize Tron send")?;
            let key = store
                .tron_account_signing_key(&mut grant, &view_session, &uuid, seed.as_deref())
                .wrap_err("load the Tron account's key")?;
            PrivateKeySigner::from_bytes(&B256::from(*key)).wrap_err("load the Tron account's key")
        }))
    }

    /// Fresh device approval of the wallet's synthetic seed, from which the account's key is
    /// derived for this signature alone.
    #[cfg(feature = "hardware")]
    fn tron_hardware_signer(
        &mut self,
        store: Arc<DesktopVaultStore>,
        view_session: Arc<DesktopViewSession>,
        uuid: String,
        events: &PublicActionSessionEventSender,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<TronSigner, &'static str> {
        use wallet_ops::PublicActionSessionEvent;

        let descriptor = self
            .selected_hardware_descriptor()
            .ok_or("Selected wallet is missing its hardware derivation descriptor")?;
        let hardware_session = view_session
            .hardware_profile_session()
            .cloned()
            .ok_or("Unlock the matching hardware profile before sending")?;
        let entropy = self.tron_hardware_entropy(hardware_session, &descriptor, window, cx);
        let events = events.clone();
        Ok(Box::pin(async move {
            let (entropy, hardware_session) = entropy.await?;
            let _ = events.send(PublicActionSessionEvent::HardwareProfileSessionRefreshed {
                session: hardware_session,
            });
            let key = store.hardware_derived_tron_account_signing_key(
                &view_session,
                &uuid,
                &descriptor,
                entropy.expose_secret(),
            );
            drop(entropy);
            PrivateKeySigner::from_bytes(&B256::from(*key?)).wrap_err("load the Tron account's key")
        }))
    }

    #[cfg(not(feature = "hardware"))]
    #[allow(
        clippy::unused_self,
        clippy::needless_pass_by_value,
        clippy::needless_pass_by_ref_mut,
        clippy::unnecessary_wraps
    )]
    fn tron_hardware_signer(
        &mut self,
        _store: Arc<DesktopVaultStore>,
        _view_session: Arc<DesktopViewSession>,
        _uuid: String,
        _events: &PublicActionSessionEventSender,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Result<TronSigner, &'static str> {
        Err("This build has no hardware wallet support, so it can't sign for this account")
    }

    /// A Tron Send's waiting step after its handoff. The endpoint may not have answered the
    /// broadcast, so the step doesn't claim it succeeded.
    pub(in crate::root) fn tron_send_pending_detail(&self) -> Option<&'static str> {
        self.tron_send_owns_progress()
            .then_some("Handed to an endpoint; waiting for confirmation.")
    }

    /// While a Tron Send's progress shows: the endpoint's answer while the send waits for
    /// solidified blocks, or what a completed send moved.
    pub(in crate::root) fn tron_send_progress_note(&self) -> Option<AnyElement> {
        if !self.tron_send_owns_progress() {
            return None;
        }
        if let Some(completed) = &self.tron_public.send.completed {
            return Some(
                Alert::success("wallet-tron-send-completed", completed.to_string())
                    .small()
                    .into_any_element(),
            );
        }
        let answer = self
            .public_form
            .action_progress
            .iter()
            .find(|step| {
                step.step == PublicActionProgressStep::Send
                    && step.status == crate::root::public_action::PublicActionStepStatus::Pending
            })?
            .message
            .as_ref()?;
        Some(
            Alert::warning(
                "wallet-tron-send-endpoint-answer",
                format!(
                    "{}. The send waits for solidified blocks to include it or pass its \
                     expiration.",
                    sentence(answer)
                ),
            )
            .small()
            .into_any_element(),
        )
    }

    fn render_tron_send_form(&self, root: &Entity<Self>, content_width: Pixels) -> gpui::Div {
        let Some(form) = self.tron_public.send.form.as_ref() else {
            return div().w(content_width);
        };
        let sending = self.public_form.sending;
        let unavailable = form.sender_exists == Some(false);
        let disabled = self.public_action_in_flight() || unavailable;
        let review = form.review.as_ref();
        let submit_disabled = disabled || review.is_none_or(|review| review.missing_trx.is_some());
        let keyboard_root = root.clone();
        let picker_root = root.clone();
        let save_root = root.clone();
        let max_root = root.clone();
        let submit_root = root.clone();
        let receiver_problem = (!form.receiver_value.trim().is_empty()
            && parse_tron_receiver(&form.receiver_value).is_err())
        .then(|| {
            app_muted_text(NOT_A_TRON_ADDRESS)
                .text_xs()
                .text_color(rgb(theme::DANGER))
        });
        let picker = form_recipient_picker(
            "wallet-tron-send-receiver",
            "wallet-tron-send-save-receiver".into(),
            TRON_RECEIVERS,
            &self.tron_public.send.receiver_input,
            &form.receiver_value,
            form.receiver_picker.open(),
            form.receiver_picker.index(),
            form.receiver_picker.scroll(),
            &self.tron_receiver_options(),
            disabled,
            move |event, window, cx| {
                picker_root.update(cx, |root, cx| {
                    root.tron_receiver_picker_event(event, window, cx);
                });
            },
            move |receiver, window, cx| {
                save_root.update(cx, |root, cx| {
                    root.open_save_recipient_dialog(TRON_RECEIVERS, receiver, window, cx);
                });
            },
        );
        let max_label = match form.asset {
            TronSendAsset::Trx => self.tron_send_trx_balance(form).map(|balance| {
                format!(
                    "{} {} before fees",
                    railgun_ui::format_token_amount(U256::from(balance), form.decimals),
                    form.symbol
                )
            }),
            TronSendAsset::Trc20(token) => self
                .tron_send_balance(form, PublicAssetId::Erc20(token))
                .map(|balance| {
                    format!(
                        "{} {}",
                        railgun_ui::format_token_amount(balance, form.decimals),
                        form.symbol
                    )
                }),
        };
        let amount = render_amount_input(
            "wallet-tron-send-max",
            &self.tron_public.send.amount_input,
            format!("Amount ({})", form.symbol),
            max_label,
            disabled,
            move |window, cx| {
                max_root.update(cx, |root, cx| {
                    root.set_tron_send_amount_to_max(window, cx);
                });
            },
        );
        let fees = match (review, form.preparing) {
            (Some(review), _) => Some(estimated_network_fees(
                trx_fee(review.fees.expected, &self.public_broadcaster_anchor_cache),
                Some(trx_fee(
                    review.fees.maximum,
                    &self.public_broadcaster_anchor_cache,
                )),
                APP_MONO_FONT_FAMILY,
            )),
            (None, true) => Some(estimated_network_fees(
                "Estimating...".to_owned(),
                None,
                APP_MONO_FONT_FAMILY,
            )),
            (None, false) => None,
        };
        let notice = (!self.public_form.action_progress_dialog_open
            && self.tron_send_owns_progress())
        .then(|| {
            public_action_closed_status_step(
                &self.public_form.action_progress,
                self.public_form.action_progress_lifecycle,
            )
        })
        .flatten()
        .map(|step| {
            render_public_action_active_status_notice(
                root.clone(),
                PublicActionMode::Send,
                Some("Tron send"),
                step,
                self.public_form.action_requires_device_approval,
                false,
                self.tron_send_pending_detail(),
            )
        });
        public_action_form(content_width, submit_disabled, move |window, cx| {
            keyboard_root.update(cx, |root, cx| root.submit_tron_send_from_form(window, cx));
        })
        .child(
            app_muted_text(format!(
                "From {}",
                railgun_ui::short_non_evm(&encode_tron_address(form.sender))
            ))
            .font_family(APP_MONO_FONT_FAMILY),
        )
        .when(unavailable, |content| {
            content.child(
                Alert::warning("wallet-tron-send-unavailable", SENDER_MISSING.to_owned()).small(),
            )
        })
        .child(labeled_field("To", picker).children(receiver_problem))
        .child(amount)
        .children(fees)
        .children(review.map_or_else(Vec::new, |review| resource_lines(review, &form.symbol)))
        .children(review.filter(|review| review.creates_account).map(|_| {
            app_muted_text(
                "This send creates the receiver's account. Its fee includes Tron's account \
                 creation fee.",
            )
            .whitespace_normal()
        }))
        .children(review.and_then(|review| review.missing_trx).map(|missing| {
            Alert::error(
                "wallet-tron-send-missing-trx",
                format!(
                    "Review is unavailable: the account needs {} more for the amount and the \
                     expected fee.",
                    trx(missing)
                ),
            )
            .small()
        }))
        .child(
            app_button(
                "wallet-tron-send-review",
                if sending { "Sending..." } else { "Review" },
            )
            .primary()
            .small()
            .loading(sending)
            .disabled(submit_disabled)
            .on_click(move |_event, window, cx| {
                submit_root.update(cx, |root, cx| root.submit_tron_send_from_form(window, cx));
            }),
        )
        .children(
            form.error
                .as_ref()
                .map(|error| Alert::error("wallet-tron-send-error", error.to_string()).small()),
        )
        .children(notice)
    }
}

/// The `tron:` modes of the debug UI fixture: synthesized Tron accounts with balances in place
/// of the wallet's own, reads answered in memory, and staged progress. Nothing is signed or sent.
#[cfg(debug_assertions)]
mod ui_fixture {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use alloy::primitives::{Address, B256, Bytes, U256, address};
    use gpui::{Context, Window};
    use wallet_ops::tron::TRON_CHAIN_ID;
    use wallet_ops::tron::address::encode_tron_address;
    use wallet_ops::tron::balances::TronAccountResources;
    use wallet_ops::tron::confirmation::{
        TronBlock, TronBlockReads, TronSendFailure, TronSendOutcome,
    };
    use wallet_ops::tron::fees::{AccountResources, ChainFeeParameters};
    use wallet_ops::tron::http_api::{TronBroadcast, TronTransactionInfo};
    use wallet_ops::tron::send::TronSendReads;
    use wallet_ops::tron::transaction::{ReferenceBlock, SignedTransaction};
    use wallet_ops::vault::{
        PublicAccountScope, PublicAccountStatus, TronAccountMetadata, TronAccountSource,
    };
    use wallet_ops::{
        PublicAccountBalance, PublicActionProgressStep, PublicAssetId, PublicBalanceAmount,
        PublicBalanceAsset, PublicBalanceEntry, PublicBalanceSnapshot, RpcBrokerError,
    };

    use super::trx;
    use crate::root::WalletRoot;
    use crate::root::private_swap::{UiFixtureTron, ui_fixture_tron};
    use crate::root::public_account::tron::{tron_list_account, tron_list_source};
    use crate::root::public_action::{
        PublicActionMode, PublicActionStepState, PublicActionStepStatus,
    };

    const USDT: Address = address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c");
    const SAVINGS: Address = address!("0x5a1e7a5e2c6b3f0d9e8a4b1c7d2f6e3a9b8c4d01");
    const TRADING: Address = address!("0x7d3a9c2e4b6f8a1d0c5e7b9f2a4c6e8d1b3f5a02");
    /// Holds only USDT, so it doesn't exist on chain.
    const DEPOSIT: Address = address!("0x2c4e6a8b0d1f3e5c7a9b2d4f6e8c0a1b3d5f7e03");
    /// A receiver that doesn't exist on chain, which a TRX send creates.
    const NEW_RECEIVER: Address = address!("0x9e1b3d5f7a2c4e6b8d0f1a3c5e7b9d2f4a6c8e04");
    const BLOCK: u64 = 80_412_655;
    const TXID: &str = "4c2f8e1a9b7d3c5e6f0a2b4d8c1e3f5a7b9d0c2e4f6a8b1d3c5e7f9a0b2c4d6e";

    /// Answers a Send's reads in memory and broadcasts nothing.
    struct FixtureChain;

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }

    #[async_trait::async_trait]
    impl TronSendReads for FixtureChain {
        async fn account_exists(&self, account: Address) -> Result<bool, RpcBrokerError> {
            Ok(account != DEPOSIT && account != NEW_RECEIVER)
        }

        async fn account_resources(
            &self,
            account: Address,
        ) -> Result<AccountResources, RpcBrokerError> {
            Ok(resources(account))
        }

        async fn chain_fee_parameters(&self) -> Result<ChainFeeParameters, RpcBrokerError> {
            Ok(ChainFeeParameters {
                bandwidth_price: 1_000,
                energy_price: 100,
                create_account_fee: 100_000,
                create_account_system_fee: 1_000_000,
                create_account_bandwidth_rate: 1,
            })
        }

        /// A transfer to `NEW_RECEIVER`, which holds no token, costs more than to a holder.
        async fn energy_estimate(
            &self,
            _sender: Address,
            _contract: Address,
            data: Bytes,
        ) -> Result<u64, RpcBrokerError> {
            // `transfer(address,uint256)`: the recipient ends the first argument word.
            let recipient = data.get(16..36).map(Address::from_slice);
            Ok(if recipient == Some(NEW_RECEIVER) {
                130_285
            } else {
                64_285
            })
        }

        async fn latest_block(&self) -> Result<ReferenceBlock, RpcBrokerError> {
            Ok(ReferenceBlock {
                number: BLOCK,
                hash: B256::repeat_byte(1),
                timestamp_ms: now_ms(),
            })
        }

        async fn broadcast(
            &self,
            _transaction: &SignedTransaction,
        ) -> Result<TronBroadcast, RpcBrokerError> {
            Err(RpcBrokerError::Timeout)
        }

        fn now_ms(&self) -> u64 {
            now_ms()
        }
    }

    #[async_trait::async_trait]
    impl TronBlockReads for FixtureChain {
        async fn solidified_head(&self) -> Result<TronBlock, RpcBrokerError> {
            Err(RpcBrokerError::Timeout)
        }

        async fn block(&self, _number: u64) -> Result<TronBlock, RpcBrokerError> {
            Err(RpcBrokerError::Timeout)
        }

        async fn transaction_infos(
            &self,
            _number: u64,
        ) -> Result<Vec<TronTransactionInfo>, RpcBrokerError> {
            Err(RpcBrokerError::Timeout)
        }
    }

    /// Savings has energy and staked bandwidth; the others only free bandwidth.
    fn resources(account: Address) -> AccountResources {
        let (staked_bandwidth, energy) = if account == SAVINGS {
            (1_500, 100_000)
        } else {
            (0, 0)
        };
        AccountResources {
            free_bandwidth: 600,
            staked_bandwidth,
            energy,
        }
    }

    fn entry(id: PublicAssetId, symbol: &str, amount: u64) -> PublicBalanceEntry {
        PublicBalanceEntry {
            asset: PublicBalanceAsset {
                id,
                symbol: symbol.to_owned(),
                decimals: 6,
            },
            amount: PublicBalanceAmount::Available(U256::from(amount)),
        }
    }

    impl WalletRoot {
        /// Stages a `tron:` mode's accounts, balances, prices and reads in place of the
        /// wallet's Tron accounts. Returns whether a mode is on.
        pub(in crate::root) fn stage_tron_ui_fixture(&mut self) -> bool {
            if ui_fixture_tron().is_none() || self.view_session.is_none() {
                return false;
            }
            let scope = self.selected_wallet_id.as_deref().map_or(
                PublicAccountScope::Global,
                |wallet_uuid| PublicAccountScope::PrivateWallet {
                    wallet_uuid: wallet_uuid.to_owned(),
                },
            );
            // Label, address, source, TRX in sun and USDT in its units.
            let staged = [
                (
                    "Savings",
                    SAVINGS,
                    TronAccountSource::Derived { index: 0 },
                    2_480_350_000,
                    1_250_000_000,
                ),
                (
                    "Trading",
                    TRADING,
                    TronAccountSource::Imported,
                    35_120_000,
                    312_500_000,
                ),
                (
                    "Deposit",
                    DEPOSIT,
                    TronAccountSource::Derived { index: 1 },
                    0,
                    25_000_000,
                ),
            ];
            let mut accounts = Vec::new();
            let mut balances = Vec::new();
            for (order, (label, address, source, trx_sun, usdt)) in (0_u32..).zip(staged) {
                let account = TronAccountMetadata {
                    tron_account_uuid: format!("ui-fixture-tron-{order}"),
                    address,
                    label: Some(label.to_owned()),
                    source,
                    scope: scope.clone(),
                    status: PublicAccountStatus::Active,
                    display_order: order,
                };
                balances.push(PublicAccountBalance {
                    account: tron_list_account(&account),
                    balances: vec![
                        entry(PublicAssetId::Native, "TRX", trx_sun),
                        entry(PublicAssetId::Erc20(USDT), "USDT", usdt),
                    ],
                    observed_at: Some(Instant::now()),
                    observed_block: Some(BLOCK),
                });
                accounts.push(account);
            }
            self.tron_public.listed = accounts.iter().map(tron_list_account).collect();
            self.tron_public.resources = accounts
                .iter()
                .map(|account| {
                    let state = if account.address == DEPOSIT {
                        TronAccountResources::NotOnChain
                    } else {
                        TronAccountResources::OnChain(resources(account.address))
                    };
                    (account.tron_account_uuid.clone(), Some(state))
                })
                .collect();
            self.tron_public.accounts = accounts;
            if !self.tron_public.listed.iter().any(|account| {
                Some(account.public_account_uuid.as_str()) == self.tron_public.selected.as_deref()
            }) {
                self.tron_public.selected = None;
            }
            self.tron_public.snapshot = Some(std::sync::Arc::new(PublicBalanceSnapshot {
                chain_id: TRON_CHAIN_ID,
                refreshed_at: SystemTime::now(),
                accounts: balances,
            }));
            // Stand-in prices: TRX at $0.25, so USDT's anchor is 0.25 USDT per TRX. Tron's
            // price refresh replaces them.
            self.public_broadcaster_anchor_cache.store_native_usd_rate(
                TRON_CHAIN_ID,
                U256::from(250_000),
                6,
            );
            self.public_broadcaster_anchor_cache.store_rate(
                TRON_CHAIN_ID,
                USDT,
                U256::from(250_000),
            );
            self.tron_public.send.backend = Some(std::sync::Arc::new(FixtureChain));
            true
        }

        /// Fills the Send form a `tron:` mode opened, and stages the progress of a send mode.
        pub(in crate::root) fn stage_tron_send_ui_fixture(
            &mut self,
            window: &mut Window,
            cx: &mut Context<'_, Self>,
        ) {
            let Some(mode) = ui_fixture_tron() else {
                return;
            };
            let Some(form) = self.tron_public.send.form.as_ref() else {
                return;
            };
            let receiver = match mode {
                UiFixtureTron::Accounts => return,
                UiFixtureTron::NewAccount => NEW_RECEIVER,
                _ if form.sender == TRADING => SAVINGS,
                _ => TRADING,
            };
            let (symbol, decimals, icon, source) = (
                form.symbol.clone(),
                form.decimals,
                form.icon.clone(),
                form.source,
            );
            let amount = U256::from(25_u64) * U256::from(10_u64).pow(U256::from(decimals));
            self.tron_public
                .send
                .amount_input
                .update(cx, |input, cx| input.set_value("25", window, cx));
            self.set_tron_receiver(&encode_tron_address(receiver), window, cx);
            let (status, message, outcome) = match mode {
                UiFixtureTron::Accounts | UiFixtureTron::NewAccount => return,
                UiFixtureTron::Waiting => (
                    PublicActionStepStatus::Pending,
                    Some(
                        "the endpoint didn't answer the broadcast: the request timed out"
                            .to_owned(),
                    ),
                    None,
                ),
                UiFixtureTron::Complete => (
                    PublicActionStepStatus::Done,
                    None,
                    Some(TronSendOutcome::Complete {
                        amount,
                        block: BLOCK,
                    }),
                ),
                UiFixtureTron::Failed | UiFixtureTron::TokenDidNotMove => {
                    let (reason, trx_burned_sun) = if mode == UiFixtureTron::Failed {
                        (TronSendFailure::Failed, 7_714_200)
                    } else {
                        (TronSendFailure::TokenDidNotMove, 345_000)
                    };
                    (
                        PublicActionStepStatus::Error,
                        Some(format!("{reason}; it burned {}", trx(trx_burned_sun))),
                        Some(TronSendOutcome::Failed {
                            reason,
                            trx_burned_sun,
                            block: BLOCK,
                        }),
                    )
                }
                UiFixtureTron::Dropped => (
                    PublicActionStepStatus::Error,
                    Some(
                        "the transaction expired without being included; no TRX was spent"
                            .to_owned(),
                    ),
                    Some(TronSendOutcome::Dropped),
                ),
            };
            let generation = self.start_public_action_progress_with_states(
                PublicActionMode::Send,
                vec![PublicActionStepState {
                    step: PublicActionProgressStep::Send,
                    status,
                    tx_hash: Some(std::sync::Arc::from(TXID)),
                    message: message.map(std::sync::Arc::from),
                    interval: None,
                }],
                symbol.clone(),
                icon,
                tron_list_source(source),
                None,
                None,
                true,
            );
            // Handed off: only the outcome ends it now.
            self.public_form.action_stop_available = false;
            self.tron_public.send.generation = Some(generation);
            self.tron_public.send.completed = None;
            if let Some(outcome) = outcome {
                self.show_tron_send_outcome(&symbol, decimals, receiver, outcome);
            }
            self.show_public_action_progress_dialog(window, cx);
        }
    }
}
