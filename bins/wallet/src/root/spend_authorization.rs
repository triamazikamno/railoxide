use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::DeliveryMode;
use alloy::primitives::U256;
use gpui::{
    Anchor, AnyElement, App, AppContext, ClickEvent, Context, Entity, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window, div, img, prelude::FluentBuilder as _, px,
    relative, rems, rgb,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, IndexPath, Selectable, Sizable, WindowExt,
    alert::Alert,
    button::ButtonVariants,
    collapsible::Collapsible,
    description_list::{DescriptionItem, DescriptionList},
    input::{InputEvent, InputState},
    popover::Popover,
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    spinner::Spinner,
    tooltip::Tooltip,
};
use ui::clipboard::clipboard_with_toast;
#[cfg(feature = "hardware")]
use ui::controls::app_masked_input;
use ui::controls::{
    app_amount_text, app_button, app_button_base, app_muted_text, app_strong_text, app_text,
};
use ui::hint::hint_card;
use ui::theme::{self, APP_MONO_FONT_FAMILY};
use wallet_ops::hardware::HardwareDerivationDescriptor;
#[cfg(feature = "hardware")]
use wallet_ops::hardware::{
    HardwareDerivationError, HardwareDeviceKind,
    ledger::LedgerHardwareDerivationClient,
    synthetic_entropy_from_hardware_output,
    trezor::{TrezorHardwareDerivationClient, TrezorPinMatrixProvider},
};
#[cfg(feature = "hardware")]
use wallet_ops::vault::{DesktopVaultStore, DesktopViewSession, HardwareProfileSession};
use wallet_ops::vault::{
    PublicAccountSource, SoftwareSeedSessionBinding, VaultError, WalletSoftwareContextKind,
};
use wallet_ops::{
    BlockedShieldRescueUtxoId, DesktopPrivateSpendAuthorization, SponsoredAuthorizationLimit,
};
use zeroize::Zeroizing;

use crate::assets::WalletIconSource;
use crate::root::ui_helpers::{dialog_footer, network_mention, network_name, network_token_icon};

use super::device_auth::{
    DEVICE_AUTH_REASON_SPEND, DeviceAuthMethod, DeviceAuthPassword, DeviceAuthPrompt,
    device_auth_buttons, masked_input_with_device_auth,
};
use super::governance_action::GovernanceSpendDraft;
use super::private_action::UnshieldAssetKey;
use super::public_action::{PublicSendDraft, PublicShieldDraft};
use super::vault::hardware_device_label;
use super::walletconnect::WalletConnectReviewedFeeProjection;
use super::{WalletRoot, dialog_max_height, new_masked_input, secondary_dialog_content_width};

const SPEND_AUTHORIZATION_DIALOG_WIDTH: gpui::Pixels = px(560.0);
const SPEND_AUTHORIZATION_SESSION_WARNING: &str = "Spending remains authorized for the selected lifetime without re-entering the password. Only use this on a trusted device.";
const SUMMARY_RECIPIENT_PREFIX_CHARS: usize = 8;
const SUMMARY_RECIPIENT_SUFFIX_CHARS: usize = 8;
const SUMMARY_RECIPIENT_SHORTEN_THRESHOLD_CHARS: usize = 28;
/// Wide enough for the longest lifetime label, "Until vault locks/app closes".
const SPEND_AUTHORIZATION_LIFETIME_SELECT_WIDTH: gpui::Pixels = px(248.0);
/// The least height of a compact row, which fits its info button.
const SUMMARY_COMPACT_ROW_MIN_HEIGHT: gpui::Pixels = px(26.0);
const SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE: &str = "wallet-spend-auth-row-group-toggle";
const SPEND_AUTHORIZATION_DETAILS_TOGGLE: &str = "wallet-spend-auth-details-toggle";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SpendAuthorizationLifetime {
    Once,
    FiveMinutes,
    FifteenMinutes,
    UntilVaultLock,
}

type SpendAuthorizationLifetimeSelect = SelectState<SearchableVec<SpendAuthorizationLifetime>>;

impl SelectItem for SpendAuthorizationLifetime {
    type Value = Self;

    fn title(&self) -> SharedString {
        SharedString::from(self.label())
    }

    fn value(&self) -> &Self::Value {
        self
    }
}

impl SpendAuthorizationLifetime {
    const ALL: [Self; 4] = [
        Self::Once,
        Self::FiveMinutes,
        Self::FifteenMinutes,
        Self::UntilVaultLock,
    ];

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Once => "Just this spend",
            Self::FiveMinutes => "5 minutes",
            Self::FifteenMinutes => "15 minutes",
            Self::UntilVaultLock => "Until vault locks/app closes",
        }
    }

    const fn duration(self) -> Option<Duration> {
        match self {
            Self::Once | Self::UntilVaultLock => None,
            Self::FiveMinutes => Some(Duration::from_mins(5)),
            Self::FifteenMinutes => Some(Duration::from_mins(15)),
        }
    }

    pub(super) const fn requires_reusable_authorization_warning(self) -> bool {
        !matches!(self, Self::Once)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SpendAuthorizationScope {
    base_profile_uuid: Arc<str>,
    wallet_uuid: Arc<str>,
    protected_seed_binding: Option<SoftwareSeedSessionBinding>,
}

impl SpendAuthorizationScope {
    fn new(
        base_profile_uuid: impl Into<Arc<str>>,
        wallet_uuid: impl Into<Arc<str>>,
        protected_seed_binding: Option<SoftwareSeedSessionBinding>,
    ) -> Self {
        Self {
            base_profile_uuid: base_profile_uuid.into(),
            wallet_uuid: wallet_uuid.into(),
            protected_seed_binding,
        }
    }
}

pub(super) struct SpendAuthorizationCache {
    password: Zeroizing<String>,
    scope: SpendAuthorizationScope,
    expires_at: Option<Instant>,
}

fn clear_protected_software_seed_session_state(
    protected_software_seed_session: &mut Option<
        Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>,
    >,
    spend_authorization_cache: &mut Option<SpendAuthorizationCache>,
) -> bool {
    let cleared = protected_software_seed_session.take().is_some();
    spend_authorization_cache.take().is_some() || cleared
}

impl SpendAuthorizationCache {
    fn new(
        password: Zeroizing<String>,
        lifetime: SpendAuthorizationLifetime,
        scope: SpendAuthorizationScope,
        now: Instant,
    ) -> Option<Self> {
        match lifetime {
            SpendAuthorizationLifetime::Once => None,
            SpendAuthorizationLifetime::UntilVaultLock => Some(Self {
                password,
                scope,
                expires_at: None,
            }),
            SpendAuthorizationLifetime::FiveMinutes
            | SpendAuthorizationLifetime::FifteenMinutes => {
                lifetime.duration().map(|duration| Self {
                    password,
                    scope,
                    expires_at: Some(now + duration),
                })
            }
        }
    }

    fn is_valid_at(&self, scope: &SpendAuthorizationScope, now: Instant) -> bool {
        self.scope == *scope && self.expires_at.is_none_or(|expires_at| now < expires_at)
    }
}

#[derive(Clone)]
pub(super) enum SpendAuthorizationIntent {
    ExecutorGasPassword {
        intent: Box<Self>,
        summary: Box<SpendAuthorizationSummary>,
        payer: String,
    },
    StealthAccounts(
        Entity<super::stealth_accounts::StealthAccountsView>,
        Arc<super::stealth_accounts::StealthAuthorization>,
    ),
    PrivateSwap(
        Entity<super::private_swap::PrivateSwapsView>,
        Arc<super::private_swap::SwapAuthorization>,
    ),
    PublicSwap(
        Entity<super::private_swap::PrivateSwapsView>,
        Arc<super::private_swap::PublicSwapAuthorization>,
    ),
    PublicSwapSource {
        view: Entity<super::private_swap::PrivateSwapsView>,
        command: Arc<super::private_swap::PublicSwapAuthorization>,
        private_authorization: Rc<RefCell<Option<DesktopPrivateSpendAuthorization>>>,
    },
    PrepareExecutorUnshield(
        UnshieldAssetKey,
        Arc<super::private_action::ExecutorUnshieldApproval>,
        Option<wallet_ops::gateway::GatewayDraftExecution>,
    ),
    ExecutorUnshield(
        UnshieldAssetKey,
        Arc<super::private_action::ExecutorUnshieldReview>,
        Option<wallet_ops::gateway::GatewayDraftExecution>,
    ),
    PrivateSend(
        UnshieldAssetKey,
        Option<SponsoredAuthorizationLimit>,
        Option<wallet_ops::gateway::GatewayDraftExecution>,
        Option<(alloy::primitives::Address, alloy::primitives::U256)>,
    ),
    PrivateSendSelfBroadcastGasPassword(
        UnshieldAssetKey,
        Option<SponsoredAuthorizationLimit>,
        Option<wallet_ops::gateway::GatewayDraftExecution>,
    ),
    PrivateUnshield(
        UnshieldAssetKey,
        Option<SponsoredAuthorizationLimit>,
        Option<wallet_ops::gateway::GatewayDraftExecution>,
        Option<(alloy::primitives::Address, alloy::primitives::U256)>,
    ),
    PrivateUnshieldSelfBroadcastGasPassword(
        UnshieldAssetKey,
        Option<SponsoredAuthorizationLimit>,
        Option<wallet_ops::gateway::GatewayDraftExecution>,
    ),
    BlockedShieldRefund(BlockedShieldRescueUtxoId),
    BlockedShieldRefundGasPassword(BlockedShieldRescueUtxoId),
    PublicSend(Box<PublicSendDraft>),
    PublicShield(Box<PublicShieldDraft>),
    Governance(Box<GovernanceSpendDraft>),
    WalletConnectRequest {
        request_key: String,
        review_token: u64,
        reviewed_fee: Option<WalletConnectReviewedFeeProjection>,
    },
}

impl SpendAuthorizationIntent {
    pub(super) fn hardware_executor_action(
        &self,
        root: &WalletRoot,
    ) -> Option<wallet_ops::HardwareExecutorAction> {
        use wallet_ops::HardwareExecutorAction;
        let (source, account) = match self {
            Self::PrepareExecutorUnshield(_, approval, _) => {
                return Some(HardwareExecutorAction::Execute(approval.operation));
            }
            Self::ExecutorUnshield(_, review, _) => {
                return Some(HardwareExecutorAction::Execute(review.prepared.operation()));
            }
            Self::StealthAccounts(_, command) => return Some(command.hardware_executor_action()),
            Self::PrivateSwap(_, command) => return Some(command.hardware_executor_action()),
            Self::PublicSwap(_, command) => return Some(command.hardware_executor_action()),
            Self::PrivateSend(key, ..) | Self::PrivateUnshield(key, ..) => {
                let (delivery, uuid) = if let Self::PrivateSend(..) = self {
                    let form = root.send_forms.get(key)?;
                    (
                        form.delivery_mode,
                        form.self_broadcast_gas_payer_uuid.as_deref(),
                    )
                } else {
                    let form = root.unshield_forms.get(key)?;
                    (
                        form.delivery_mode,
                        form.self_broadcast_gas_payer_uuid.as_deref(),
                    )
                };
                if delivery != DeliveryMode::SelfBroadcast {
                    return None;
                }
                let account = root.selected_self_broadcast_gas_payer_account(uuid)?;
                let PublicAccountSource::ExecutorDerived(source) = account.source else {
                    return None;
                };
                return Some(HardwareExecutorAction::GasPayment {
                    account: account.public_account_uuid.clone(),
                    operation: source.operation(),
                });
            }
            Self::BlockedShieldRefund(utxo_id) => {
                return root.blocked_shield_refund_executor_gas_payment(*utxo_id);
            }
            Self::PublicSend(draft) => (
                draft.public_account_source,
                draft.public_account_uuid.to_string(),
            ),
            Self::PublicShield(draft) => (
                draft.public_account_source,
                draft.public_account_uuid.to_string(),
            ),
            Self::Governance(draft) => (draft.actor_source, draft.actor_uuid.to_string()),
            Self::WalletConnectRequest {
                request_key,
                review_token,
                ..
            } => {
                return root
                    .walletconnect_hardware_executor_action(request_key, *review_token)
                    .map(|(_, action)| action);
            }
            _ => return None,
        };
        let wallet_ops::vault::PublicAccountSource::ExecutorDerived(source) = source else {
            return None;
        };
        Some(HardwareExecutorAction::Public {
            account,
            operation: source.operation(),
        })
    }

    fn gateway_execution(&self) -> Option<&wallet_ops::gateway::GatewayDraftExecution> {
        match self {
            Self::ExecutorGasPassword { intent, .. } => intent.gateway_execution(),
            Self::PrivateSend(_, _, execution, _)
            | Self::PrepareExecutorUnshield(_, _, execution)
            | Self::ExecutorUnshield(_, _, execution)
            | Self::PrivateUnshield(_, _, execution, _)
            | Self::PrivateSendSelfBroadcastGasPassword(_, _, execution)
            | Self::PrivateUnshieldSelfBroadcastGasPassword(_, _, execution) => execution.as_ref(),
            Self::PublicSend(draft) => draft.gateway_execution.as_ref(),
            Self::PublicShield(draft) => draft.gateway_execution.as_ref(),
            _ => None,
        }
    }

    fn private_attention(&self, step: &str, message: &str) {
        if let Some(execution) = self.gateway_execution()
            && let Some(progress) = execution.snapshot().private
        {
            execution.update_private(
                wallet_ops::gateway::GatewayDraftStatus::Attention,
                step.into(),
                message.into(),
                false,
                progress,
            );
        }
    }

    fn private_review_current(&self, root: &WalletRoot) -> bool {
        if let Self::ExecutorGasPassword { intent, .. } = self {
            return intent.private_review_current(root);
        }
        let custom_fee_matches = match self {
            Self::PrivateSend(key, _, _, expected) => {
                root.send_forms.get(key).is_some_and(|form| {
                    form.custom_fee_amount
                        .map(|amount| (form.selected_fee_token, amount))
                        == *expected
                })
            }
            Self::PrivateUnshield(key, _, _, expected) => {
                root.unshield_forms.get(key).is_some_and(|form| {
                    form.custom_fee_amount
                        .map(|amount| (form.selected_fee_token, amount))
                        == *expected
                })
            }
            _ => true,
        };
        if !custom_fee_matches {
            return false;
        }
        if let Self::StealthAccounts(_, command) = self {
            return root.stealth_session_is_current(command.session());
        }
        if let Self::PrivateSwap(_, command) = self {
            return command.sessions_are_current(root);
        }
        if let Self::PublicSwap(_, command) | Self::PublicSwapSource { command, .. } = self {
            return command.sessions_are_current(root);
        }
        if let Self::ExecutorUnshield(key, review, _) = self
            && !root
                .unshield_forms
                .get(key)
                .and_then(|form| form.executor_review.as_ref())
                .is_some_and(|current| Arc::ptr_eq(current, review))
        {
            return false;
        }
        let current = match self {
            Self::PrivateSend(key, _, _, _)
            | Self::PrivateSendSelfBroadcastGasPassword(key, _, _) => root
                .send_forms
                .get(key)
                .map(|form| form.gateway_execution.as_ref()),
            Self::PrivateUnshield(key, _, _, _)
            | Self::PrepareExecutorUnshield(key, _, _)
            | Self::ExecutorUnshield(key, _, _)
            | Self::PrivateUnshieldSelfBroadcastGasPassword(key, _, _) => root
                .unshield_forms
                .get(key)
                .map(|form| form.gateway_execution.as_ref()),
            _ => return true,
        };
        match (self.gateway_execution(), current) {
            (None, Some(None)) => true,
            (Some(expected), Some(Some(current))) => expected.same_execution(current),
            _ => false,
        }
    }

    pub(super) fn approve_gateway_review(&self, root: &WalletRoot) -> bool {
        self.private_review_current(root)
            && self
                .gateway_execution()
                .is_none_or(wallet_ops::gateway::GatewayDraftExecution::approve_review)
    }

    const fn uses_private_wallet(&self) -> bool {
        matches!(
            self,
            Self::PrivateSend(..)
                | Self::StealthAccounts(..)
                | Self::PrivateSwap(..)
                | Self::PublicSwap(..)
                | Self::PrivateUnshield(..)
                | Self::PrepareExecutorUnshield(..)
                | Self::ExecutorUnshield(..)
                | Self::BlockedShieldRefund(_)
        )
    }
}

#[derive(Clone)]
#[cfg_attr(not(feature = "hardware"), allow(dead_code))]
pub(super) enum HardwareSpendAuthorizationCompletion {
    Continue(SpendAuthorizationIntent),
    ExecutorWithGasPayer {
        intent: SpendAuthorizationIntent,
        payer: String,
        password: Zeroizing<String>,
        seed_session: Option<Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>>,
    },
    PrivateSendSelfBroadcast {
        key: UnshieldAssetKey,
        vault_password: Zeroizing<String>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
    },
    PrivateUnshieldSelfBroadcast {
        key: UnshieldAssetKey,
        vault_password: Zeroizing<String>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
    },
    BlockedShieldRefund {
        utxo_id: BlockedShieldRescueUtxoId,
        vault_password: Zeroizing<String>,
    },
}

impl HardwareSpendAuthorizationCompletion {
    fn private_intent(&self) -> Option<SpendAuthorizationIntent> {
        match self {
            Self::Continue(intent) | Self::ExecutorWithGasPayer { intent, .. } => {
                Some(intent.clone())
            }
            Self::PrivateSendSelfBroadcast {
                key,
                authorization_limit,
                execution,
                ..
            } => Some(SpendAuthorizationIntent::PrivateSend(
                *key,
                *authorization_limit,
                execution.clone(),
                None,
            )),
            Self::PrivateUnshieldSelfBroadcast {
                key,
                authorization_limit,
                execution,
                ..
            } => Some(SpendAuthorizationIntent::PrivateUnshield(
                *key,
                *authorization_limit,
                execution.clone(),
                None,
            )),
            Self::BlockedShieldRefund { .. } => None,
        }
    }
}

#[cfg(feature = "hardware")]
enum HardwareSpendAuthorizationError {
    Hardware(HardwareDerivationError),
    Vault(VaultError),
    Executor(String),
}

/// The authorization, a private Bridge swap's second one for its destination network from the
/// same device session, and the refreshed hardware session.
#[cfg(feature = "hardware")]
type HardwareSpendAuthorizationTaskOutput = Result<
    (
        DesktopPrivateSpendAuthorization,
        Option<DesktopPrivateSpendAuthorization>,
        HardwareProfileSession,
    ),
    HardwareSpendAuthorizationError,
>;

#[cfg(feature = "hardware")]
impl From<HardwareDerivationError> for HardwareSpendAuthorizationError {
    fn from(error: HardwareDerivationError) -> Self {
        Self::Hardware(error)
    }
}

#[cfg(feature = "hardware")]
impl From<VaultError> for HardwareSpendAuthorizationError {
    fn from(error: VaultError) -> Self {
        Self::Vault(error)
    }
}

#[derive(Clone)]
pub(super) struct SpendAuthorizationSummary {
    title: Arc<str>,
    detail: Arc<str>,
    confirm_label: Arc<str>,
    context: Option<Arc<str>>,
    cards: Option<[SpendAuthorizationCard; 2]>,
    rows: Vec<SpendAuthorizationSummaryRow>,
    compact_rows: bool,
    warnings: Vec<Arc<str>>,
    payload: Option<SpendAuthorizationPayload>,
    requires_explicit_review: bool,
    steps: Option<SpendAuthorizationSteps>,
    title_network: Option<u64>,
    row_group: Option<SpendAuthorizationDetails>,
    details: Option<SpendAuthorizationDetails>,
    disclosure: Option<SpendAuthorizationDisclosure>,
}

impl SpendAuthorizationSummary {
    pub(super) fn new(
        title: impl Into<Arc<str>>,
        detail: impl Into<Arc<str>>,
        rows: Vec<SpendAuthorizationSummaryRow>,
    ) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
            confirm_label: "Authorize and continue".into(),
            context: None,
            cards: None,
            rows,
            compact_rows: false,
            warnings: Vec::new(),
            payload: None,
            requires_explicit_review: false,
            steps: None,
            title_network: None,
            row_group: None,
            details: None,
            disclosure: None,
        }
    }

    pub(super) fn with_confirm_label(mut self, label: impl Into<Arc<str>>) -> Self {
        self.confirm_label = label.into();
        self
    }

    pub(super) fn with_context(mut self, context: impl Into<Arc<str>>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Two cards above the rows: what the spend gives up, then what it receives.
    pub(super) fn with_cards(
        mut self,
        sell: SpendAuthorizationCard,
        receive: SpendAuthorizationCard,
    ) -> Self {
        self.cards = Some([sell, receive]);
        self
    }

    /// Show each row on one line, its value right-aligned and its hint behind an info button.
    pub(super) const fn with_compact_rows(mut self) -> Self {
        self.compact_rows = true;
        self
    }

    /// An alert under the rows that names what the spend makes public. `summary` is its
    /// message under the hint's title, and the hint's card holds the full text.
    pub(super) fn with_disclosure(
        mut self,
        summary: impl Into<Arc<str>>,
        hint: SpendAuthorizationHint,
    ) -> Self {
        self.disclosure = Some(SpendAuthorizationDisclosure {
            summary: summary.into(),
            hint,
        });
        self
    }

    pub(super) fn with_warnings(mut self, warnings: Vec<Arc<str>>) -> Self {
        self.warnings = warnings;
        self
    }

    /// A stepper under the title: the steps named by `labels`, of which `current`, counted
    /// from one, is the one this dialog authorizes. `hint` explains them.
    pub(super) fn with_steps<L: Into<SpendAuthorizationLabel>>(
        mut self,
        current: usize,
        labels: impl IntoIterator<Item = L>,
        hint: SpendAuthorizationHint,
    ) -> Self {
        self.steps = Some(SpendAuthorizationSteps::new(current, labels, hint));
        self
    }

    /// A small chip beside the title, naming the network `chain_id`.
    pub(super) const fn with_title_network(mut self, chain_id: u64) -> Self {
        self.title_network = Some(chain_id);
        self
    }

    /// A collapsed disclosure under the rows, laid out as one more compact row.
    pub(super) fn with_details<L, V>(
        mut self,
        title: impl Into<Arc<str>>,
        collapsed_summary: impl Into<Arc<str>>,
        rows: Vec<(L, V)>,
        note: Option<&str>,
    ) -> Self
    where
        L: Into<Arc<str>>,
        V: Into<Arc<str>>,
    {
        self.details = Some(SpendAuthorizationDetails {
            title: title.into(),
            collapsed_summary: collapsed_summary.into(),
            content: SpendAuthorizationDetailsContent::Lines {
                rows: rows
                    .into_iter()
                    .map(|(label, value)| (label.into(), value.into()))
                    .collect(),
                note: note.map(Arc::from),
            },
        });
        self
    }

    /// A collapsed disclosure above the rows, laid out like [`Self::with_details`]. Open, it
    /// shows `rows` as compact rows in place of `collapsed_summary`. Fewer than two rows aren't
    /// grouped: they lead the summary's rows.
    pub(super) fn with_row_group(
        mut self,
        title: impl Into<Arc<str>>,
        collapsed_summary: impl Into<Arc<str>>,
        rows: Vec<SpendAuthorizationSummaryRow>,
    ) -> Self {
        if rows.len() < 2 {
            self.rows.splice(0..0, rows);
            return self;
        }
        self.row_group = Some(SpendAuthorizationDetails {
            title: title.into(),
            collapsed_summary: collapsed_summary.into(),
            content: SpendAuthorizationDetailsContent::Rows(rows),
        });
        self
    }

    /// The row group's rows, which the summary's rows follow.
    fn grouped_rows(&self) -> &[SpendAuthorizationSummaryRow] {
        match self.row_group.as_ref().map(|group| &group.content) {
            Some(SpendAuthorizationDetailsContent::Rows(rows)) => rows,
            _ => &[],
        }
    }

    pub(super) fn with_payload(
        mut self,
        label: impl Into<Arc<str>>,
        value: impl Into<Arc<str>>,
    ) -> Self {
        self.payload = Some(SpendAuthorizationPayload {
            label: label.into(),
            value: value.into(),
        });
        self
    }

    pub(super) fn with_custom_transaction_fee(mut self, amount: String) -> Self {
        self.rows.push(SpendAuthorizationSummaryRow::new(
            "Custom transaction fee",
            amount,
        ));
        self.requires_explicit_review = true;
        self
    }

    pub(super) const fn requiring_explicit_review(mut self) -> Self {
        self.requires_explicit_review = true;
        self
    }

    /// The row group's rows, then the rows outside it.
    #[cfg(test)]
    fn all_rows(&self) -> impl Iterator<Item = &SpendAuthorizationSummaryRow> {
        self.grouped_rows().iter().chain(&self.rows)
    }

    /// Each row's label and value, the row group's rows first. A copyable account's value
    /// follows its prefix, in full.
    #[cfg(test)]
    pub(in crate::root) fn rows_for_test(&self) -> Vec<(String, String)> {
        self.all_rows()
            .map(SpendAuthorizationSummaryRow::values_for_test)
            .collect()
    }

    /// The row group's title, its collapsed summary and its rows' labels.
    #[cfg(test)]
    pub(in crate::root) fn row_group_for_test(&self) -> Option<(String, String, Vec<String>)> {
        self.row_group.as_ref().map(|group| {
            (
                group.title.to_string(),
                group.collapsed_summary.to_string(),
                self.grouped_rows()
                    .iter()
                    .map(|row| row.label.to_string())
                    .collect(),
            )
        })
    }

    /// Each row's label and what the review shows for it, the row group's rows first: a
    /// copyable address is shortened after its prefix.
    #[cfg(test)]
    pub(in crate::root) fn shown_rows_for_test(&self) -> Vec<(String, String)> {
        self.all_rows()
            .map(|row| {
                (
                    row.label.to_string(),
                    if row.shortened_copyable {
                        row.shortened_value()
                    } else {
                        row.value.to_string()
                    },
                )
            })
            .collect()
    }

    #[cfg(test)]
    pub(in crate::root) fn warnings_for_test(&self) -> Vec<String> {
        self.warnings.iter().map(ToString::to_string).collect()
    }

    #[cfg(test)]
    pub(in crate::root) fn steps_for_test(&self) -> Option<(usize, Vec<String>)> {
        self.steps.as_ref().map(|steps| {
            (
                steps.current,
                steps
                    .labels
                    .iter()
                    .map(SpendAuthorizationLabel::text)
                    .collect(),
            )
        })
    }

    #[cfg(test)]
    pub(in crate::root) fn details_for_test(&self) -> Vec<(String, String)> {
        match self.details.as_ref().map(|details| &details.content) {
            Some(SpendAuthorizationDetailsContent::Lines { rows, .. }) => rows
                .iter()
                .map(|(label, value)| (label.to_string(), value.to_string()))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The details disclosure's note.
    #[cfg(test)]
    pub(in crate::root) fn details_note_for_test(&self) -> Option<String> {
        match self.details.as_ref().map(|details| &details.content) {
            Some(SpendAuthorizationDetailsContent::Lines { note, .. }) => {
                note.as_deref().map(str::to_owned)
            }
            _ => None,
        }
    }

    /// The text of the hint behind the info button of the row labelled `label`.
    #[cfg(test)]
    pub(in crate::root) fn row_hint_for_test(&self, label: &str) -> Option<String> {
        self.all_rows()
            .find(|row| row.label.as_ref() == label)
            .and_then(|row| row.hint.as_ref())
            .map(SpendAuthorizationHint::text_for_test)
    }

    /// The disclosure alert's summary, its card's text, and whether it warns.
    #[cfg(test)]
    pub(in crate::root) fn disclosure_for_test(&self) -> Option<(String, String, bool)> {
        self.disclosure.as_ref().map(|disclosure| {
            (
                disclosure.summary.to_string(),
                disclosure.hint.text_for_test(),
                disclosure.hint.warning.is_some(),
            )
        })
    }

    /// The Send card's label and amount.
    #[cfg(test)]
    pub(in crate::root) fn send_card_for_test(&self) -> Option<(String, String)> {
        self.cards
            .as_ref()
            .map(|[send, _]| (send.label.text(), send.amount.to_string()))
    }

    /// The networks whose badges the Send and Receive cards' icons carry.
    #[cfg(test)]
    pub(in crate::root) fn card_networks_for_test(&self) -> Option<[Option<u64>; 2]> {
        self.cards
            .as_ref()
            .map(|cards| cards.each_ref().map(|card| card.network))
    }

    /// The Receive card's label, amount and text lines.
    #[cfg(test)]
    pub(in crate::root) fn receive_card_for_test(&self) -> Option<(String, String, Vec<String>)> {
        self.cards.as_ref().map(|[_, receive]| {
            (
                receive.label.text(),
                receive.amount.to_string(),
                receive
                    .lines
                    .iter()
                    .filter_map(|line| match line {
                        SpendAuthorizationCardLine::Text {
                            before,
                            strong,
                            after,
                        } => Some(
                            [before, strong, after]
                                .into_iter()
                                .filter(|part| !part.is_empty())
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                        SpendAuthorizationCardLine::Receiver { .. }
                        | SpendAuthorizationCardLine::Account { .. } => None,
                    })
                    .collect(),
            )
        })
    }

    /// The name and the address of the Send card's account line.
    #[cfg(test)]
    pub(in crate::root) fn send_account_for_test(&self) -> Option<(String, String)> {
        self.cards.as_ref().and_then(|[send, _]| {
            send.lines.iter().find_map(|line| match line {
                SpendAuthorizationCardLine::Account { name, address } => {
                    Some((name.to_string(), address.to_string()))
                }
                _ => None,
            })
        })
    }

    #[cfg(test)]
    pub(in crate::root) fn title_for_test(&self) -> (String, Option<String>) {
        (self.title.to_string(), self.title_network.map(network_name))
    }

    #[cfg(test)]
    pub(in crate::root) const fn compact_rows_for_test(&self) -> bool {
        self.compact_rows
    }

    /// The Sell and Receive cards' amount changes, each with whether it is adverse.
    #[cfg(test)]
    pub(in crate::root) fn card_deltas_for_test(&self) -> [Option<(String, bool)>; 2] {
        self.cards.as_ref().map_or([None, None], |cards| {
            cards.each_ref().map(|card| {
                card.delta
                    .as_ref()
                    .map(|delta| (delta.text.clone(), delta.adverse))
            })
        })
    }

    /// The amount change of the row labelled `label`, and whether it is adverse.
    #[cfg(test)]
    pub(in crate::root) fn row_delta_for_test(&self, label: &str) -> Option<(String, bool)> {
        self.all_rows()
            .find(|row| row.label.as_ref() == label)
            .and_then(|row| row.delta.as_ref())
            .map(|delta| (delta.text.clone(), delta.adverse))
    }

    /// The address the Receive card's receiver line copies and shows on hover, and the label
    /// the hover shows before it.
    #[cfg(test)]
    pub(in crate::root) fn receiver_for_test(&self) -> Option<(String, Option<String>)> {
        self.cards.as_ref().and_then(|[_, receive]| {
            receive.lines.iter().find_map(|line| match line {
                SpendAuthorizationCardLine::Receiver { address, label } => {
                    Some((address.to_string(), label.as_deref().map(str::to_owned)))
                }
                SpendAuthorizationCardLine::Text { .. }
                | SpendAuthorizationCardLine::Account { .. } => None,
            })
        })
    }
}

/// An explanation behind an info button: a card of paragraphs under a title, shown on hover
/// and, on click or from the keyboard, in a popover.
#[derive(Clone)]
pub(super) struct SpendAuthorizationHint {
    title: Arc<str>,
    paragraphs: Vec<Arc<str>>,
    warning: Option<Arc<str>>,
    fact: Option<(Arc<str>, Arc<str>)>,
}

impl SpendAuthorizationHint {
    pub(super) fn new<P: Into<Arc<str>>>(
        title: impl Into<Arc<str>>,
        paragraphs: impl IntoIterator<Item = P>,
    ) -> Self {
        Self {
            title: title.into(),
            paragraphs: paragraphs.into_iter().map(Into::into).collect(),
            warning: None,
            fact: None,
        }
    }

    /// A last paragraph in the warning colour, which the title takes too.
    pub(super) fn with_warning(mut self, warning: impl Into<Arc<str>>) -> Self {
        self.warning = Some(warning.into());
        self
    }

    /// A label and its value on a line under the paragraphs.
    pub(super) fn with_fact(
        mut self,
        label: impl Into<Arc<str>>,
        value: impl Into<Arc<str>>,
    ) -> Self {
        self.fact = Some((label.into(), value.into()));
        self
    }

    /// The card's text below its title, one paragraph per line.
    #[cfg(test)]
    fn text_for_test(&self) -> String {
        self.paragraphs
            .iter()
            .chain(&self.warning)
            .map(ToString::to_string)
            .chain(
                self.fact
                    .iter()
                    .map(|(label, value)| format!("{label} {value}")),
            )
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A stepper's steps: see [`SpendAuthorizationSummary::with_steps`].
#[derive(Clone)]
pub(super) struct SpendAuthorizationSteps {
    current: usize,
    labels: Vec<SpendAuthorizationLabel>,
    hint: SpendAuthorizationHint,
}

impl SpendAuthorizationSteps {
    pub(super) fn new<L: Into<SpendAuthorizationLabel>>(
        current: usize,
        labels: impl IntoIterator<Item = L>,
        hint: SpendAuthorizationHint,
    ) -> Self {
        Self {
            current,
            labels: labels.into_iter().map(Into::into).collect(),
            hint,
        }
    }
}

#[derive(Clone)]
struct SpendAuthorizationDisclosure {
    summary: Arc<str>,
    hint: SpendAuthorizationHint,
}

#[derive(Clone)]
struct SpendAuthorizationDetails {
    title: Arc<str>,
    collapsed_summary: Arc<str>,
    content: SpendAuthorizationDetailsContent,
}

/// What an open details disclosure shows under its line.
#[derive(Clone)]
enum SpendAuthorizationDetailsContent {
    /// Plain label and value lines, then a muted note.
    Lines {
        rows: Vec<(Arc<str>, Arc<str>)>,
        note: Option<Arc<str>>,
    },
    /// Compact rows, each with its info button.
    Rows(Vec<SpendAuthorizationSummaryRow>),
}

/// A card's or a step's label, or a row's label or value. One that names a network shows the
/// network's mark and name between `prefix` and `suffix`; any other is `prefix` alone.
#[derive(Clone)]
pub(super) struct SpendAuthorizationLabel {
    prefix: Arc<str>,
    network: Option<u64>,
    suffix: Arc<str>,
}

impl SpendAuthorizationLabel {
    /// `prefix`, a space, the network `chain_id`, then `suffix` as it is written, such as
    /// ", exactly".
    pub(super) fn on_network(
        prefix: impl Into<Arc<str>>,
        chain_id: u64,
        suffix: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            network: Some(chain_id),
            suffix: suffix.into(),
        }
    }

    /// The label as plain text, with the network's name in place.
    fn text(&self) -> String {
        match self.network {
            Some(chain_id) => {
                let name = network_name(chain_id);
                format!("{} {name}{}", self.prefix, self.suffix)
            }
            None => self.prefix.to_string(),
        }
    }
}

impl From<&str> for SpendAuthorizationLabel {
    fn from(text: &str) -> Self {
        Self {
            prefix: text.into(),
            network: None,
            suffix: "".into(),
        }
    }
}

impl From<String> for SpendAuthorizationLabel {
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

/// An amount card of a review: a small label, the token's icon and amount, the amount's
/// change beside it, the amount's USD value at the right edge, then small lines.
#[derive(Clone)]
pub(super) struct SpendAuthorizationCard {
    label: SpendAuthorizationLabel,
    amount: Arc<str>,
    delta: Option<SpendAuthorizationAmountDelta>,
    icon: Option<WalletIconSource>,
    network: Option<u64>,
    usd: Option<Arc<str>>,
    lines: Vec<SpendAuthorizationCardLine>,
}

#[derive(Clone)]
enum SpendAuthorizationCardLine {
    /// `strong` stands out between `before` and `after`. Empty parts are left out.
    Text {
        before: Arc<str>,
        strong: Arc<str>,
        after: Arc<str>,
    },
    /// "to" and the shortened `address`, with a copy button for the full address. Hovering
    /// shows the full address, after `label` when it has one.
    Receiver {
        address: Arc<str>,
        label: Option<Arc<str>>,
    },
    /// "from", the account's `name` and its shortened `address`, with a copy button for the
    /// full address, which hovering shows.
    Account { name: Arc<str>, address: Arc<str> },
}

impl SpendAuthorizationCard {
    pub(super) fn new(
        label: impl Into<SpendAuthorizationLabel>,
        amount: impl Into<Arc<str>>,
        icon: Option<WalletIconSource>,
    ) -> Self {
        Self {
            label: label.into(),
            amount: amount.into(),
            delta: None,
            icon,
            network: None,
            usd: None,
            lines: Vec::new(),
        }
    }

    /// The network `chain_id` as a badge on the card's icon.
    pub(super) const fn with_network(mut self, chain_id: u64) -> Self {
        self.network = Some(chain_id);
        self
    }

    /// The signed difference of the card's amount from `previous`, beside the amount.
    pub(super) fn with_amount_change(
        mut self,
        previous: Option<U256>,
        current: U256,
        higher_is_worse: bool,
        format_amount: impl FnOnce(U256) -> String,
    ) -> Self {
        self.delta = SpendAuthorizationAmountDelta::between(
            previous,
            current,
            higher_is_worse,
            format_amount,
        );
        self
    }

    pub(super) fn with_usd(mut self, usd: Option<String>) -> Self {
        self.usd = usd.map(Arc::from);
        self
    }

    pub(super) fn with_line(self, text: impl Into<Arc<str>>) -> Self {
        self.with_emphasis(text, "", "")
    }

    /// A line whose `strong` part stands out between `before` and `after`.
    pub(super) fn with_emphasis(
        mut self,
        before: impl Into<Arc<str>>,
        strong: impl Into<Arc<str>>,
        after: impl Into<Arc<str>>,
    ) -> Self {
        self.lines.push(SpendAuthorizationCardLine::Text {
            before: before.into(),
            strong: strong.into(),
            after: after.into(),
        });
        self
    }

    /// A line naming the receiver by its shortened `address`, which it copies and shows on
    /// hover in full. `label` is the wallet's name for the address, when it has one.
    pub(super) fn with_receiver(
        mut self,
        address: impl Into<Arc<str>>,
        label: Option<String>,
    ) -> Self {
        self.lines.push(SpendAuthorizationCardLine::Receiver {
            address: address.into(),
            label: label.map(Arc::from),
        });
        self
    }

    /// A line naming the paying account: "from", its `name` and its shortened `address`,
    /// which it copies and shows on hover in full.
    pub(super) fn with_account(
        mut self,
        name: impl Into<Arc<str>>,
        address: impl Into<Arc<str>>,
    ) -> Self {
        self.lines.push(SpendAuthorizationCardLine::Account {
            name: name.into(),
            address: address.into(),
        });
        self
    }
}

pub(in crate::root) const fn spend_authorization_can_use_cached_password(
    summary: &SpendAuthorizationSummary,
) -> bool {
    !summary.requires_explicit_review
}

#[derive(Clone)]
struct SpendAuthorizationPayload {
    label: Arc<str>,
    value: Arc<str>,
}

struct SpendAuthorizationPayloadDisclosure {
    payload: SpendAuthorizationPayload,
    open: bool,
}

impl SpendAuthorizationPayloadDisclosure {
    const fn new(payload: SpendAuthorizationPayload) -> Self {
        Self {
            payload,
            open: false,
        }
    }

    fn toggle(&mut self, cx: &mut Context<'_, Self>) {
        self.open = !self.open;
        cx.notify();
    }
}

impl gpui::Render for SpendAuthorizationPayloadDisclosure {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let disclosure = cx.entity();
        render_spend_authorization_payload(&self.payload, self.open, move |_, _, cx| {
            disclosure.update(cx, Self::toggle);
        })
    }
}

/// A details disclosure for a dialog without its own content entity.
struct SpendAuthorizationDetailsDisclosure {
    id: &'static str,
    details: SpendAuthorizationDetails,
    open: bool,
}

impl SpendAuthorizationDetailsDisclosure {
    const fn new(id: &'static str, details: SpendAuthorizationDetails) -> Self {
        Self {
            id,
            details,
            open: false,
        }
    }

    fn toggle(&mut self, cx: &mut Context<'_, Self>) {
        self.open = !self.open;
        cx.notify();
    }
}

impl gpui::Render for SpendAuthorizationDetailsDisclosure {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let disclosure = cx.entity();
        render_spend_authorization_details(
            self.id,
            &self.details,
            self.open,
            move |_, _, cx| {
                disclosure.update(cx, Self::toggle);
            },
            cx,
        )
    }
}

#[derive(Clone)]
pub(super) struct SpendAuthorizationSummaryRow {
    label: Arc<str>,
    value: Arc<str>,
    /// The label and the value of a row that names a network there, which a compact row shows
    /// with the network's mark. `label` and `value` hold their text.
    label_network: Option<SpendAuthorizationLabel>,
    value_network: Option<SpendAuthorizationLabel>,
    icon_path: Option<WalletIconSource>,
    shortened_copyable: bool,
    /// What a shortened, copyable value follows and what its copy control calls it, such as an
    /// account's number and "stealth account address".
    copyable_account: Option<(Arc<str>, Arc<str>)>,
    delta: Option<SpendAuthorizationAmountDelta>,
    hint: Option<SpendAuthorizationHint>,
}

#[derive(Clone)]
struct SpendAuthorizationAmountDelta {
    text: String,
    adverse: bool,
}

impl SpendAuthorizationAmountDelta {
    /// The signed difference of `current` from `previous`, formatted by `format_amount`. `None`
    /// without a previous amount or when the two are equal.
    fn between(
        previous: Option<U256>,
        current: U256,
        higher_is_worse: bool,
        format_amount: impl FnOnce(U256) -> String,
    ) -> Option<Self> {
        let previous = previous.filter(|previous| *previous != current)?;
        let increased = current > previous;
        let (sign, magnitude) = if increased {
            ("+", current - previous)
        } else {
            ("−", previous - current)
        };
        Some(Self {
            text: format!("{sign}{}", format_amount(magnitude)),
            adverse: increased == higher_is_worse,
        })
    }
}

impl SpendAuthorizationSummaryRow {
    pub(super) fn new(label: impl Into<Arc<str>>, value: impl Into<Arc<str>>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            label_network: None,
            value_network: None,
            icon_path: None,
            shortened_copyable: false,
            copyable_account: None,
            delta: None,
            hint: None,
        }
    }

    /// A row whose label or value may name a network, which then carries the network's mark.
    /// Shown for compact rows.
    pub(super) fn naming_network(
        label: impl Into<SpendAuthorizationLabel>,
        value: impl Into<SpendAuthorizationLabel>,
    ) -> Self {
        let (label, value): (SpendAuthorizationLabel, SpendAuthorizationLabel) =
            (label.into(), value.into());
        let mut row = Self::new(label.text(), value.text());
        row.label_network = label.network.is_some().then_some(label);
        row.value_network = value.network.is_some().then_some(value);
        row
    }

    /// An explanation behind an info button at the row's end. Shown for compact rows.
    pub(super) fn with_hint(mut self, hint: SpendAuthorizationHint) -> Self {
        self.hint = Some(hint);
        self
    }

    pub(super) fn with_icon(mut self, icon_path: Option<WalletIconSource>) -> Self {
        self.icon_path = icon_path;
        self
    }

    pub(super) const fn with_shortened_copyable(mut self) -> Self {
        self.shortened_copyable = true;
        self
    }

    /// The value is an account's address: shortened after `prefix`, such as "#35 · ", with a
    /// control that copies it in full and is named `name`.
    pub(super) fn with_copyable_account(
        mut self,
        prefix: impl Into<Arc<str>>,
        name: impl Into<Arc<str>>,
    ) -> Self {
        self.shortened_copyable = true;
        self.copyable_account = Some((prefix.into(), name.into()));
        self
    }

    /// What a shortened, copyable row shows: its value, shortened, after its prefix.
    fn shortened_value(&self) -> String {
        let short = spend_authorization_recipient_display(self.value.as_ref());
        match &self.copyable_account {
            Some((prefix, _)) => format!("{prefix}{short}"),
            None => short,
        }
    }

    /// The tooltip of a shortened, copyable row's copy control.
    fn copy_tooltip(&self) -> String {
        match &self.copyable_account {
            Some((_, name)) => format!("Copy {name}"),
            None => format!("Copy {}", self.label.to_ascii_lowercase()),
        }
    }

    pub(super) fn with_amount_change(
        mut self,
        previous: Option<U256>,
        current: U256,
        higher_is_worse: bool,
        format_amount: impl FnOnce(U256) -> String,
    ) -> Self {
        self.delta = SpendAuthorizationAmountDelta::between(
            previous,
            current,
            higher_is_worse,
            format_amount,
        );
        self
    }

    #[cfg(test)]
    pub(in crate::root) fn values_for_test(&self) -> (String, String) {
        let prefix = self
            .copyable_account
            .as_ref()
            .map_or("", |(prefix, _)| prefix.as_ref());
        (self.label.to_string(), format!("{prefix}{}", self.value))
    }
}

struct SpendAuthorizationDialogContent {
    root: Entity<WalletRoot>,
    intent: SpendAuthorizationIntent,
    summary: SpendAuthorizationSummary,
    password_input: Entity<InputState>,
    lifetime: SpendAuthorizationLifetime,
    lifetime_select: Entity<SpendAuthorizationLifetimeSelect>,
    payload_open: bool,
    row_group_open: bool,
    details_open: bool,
    error: Option<Arc<str>>,
    pending: bool,
    cancelled: bool,
    review_authorization: Option<(SpendAuthorizationScope, DesktopPrivateSpendAuthorization)>,
    review_focus: gpui::FocusHandle,
    device_auth: Option<DeviceAuthPrompt>,
    device_auth_pending: bool,
    lease: Weak<Cell<bool>>,
}

#[derive(Clone, PartialEq, Eq)]
struct HardwareGasPaymentReview {
    form: gpui::EntityId,
    recipient: String,
    amount: U256,
    payer: Option<String>,
    funding: super::private_action::SelfBroadcastFundingMode,
    gas_fee: wallet_ops::SelfBroadcastGasFeeSelection,
    incentive: wallet_ops::SponsoredIncentive,
    fee_mode: wallet_ops::FeeHandlingMode,
    unwrap: bool,
    top_up: Option<wallet_ops::DesktopNativeTopUpPlan>,
}

#[cfg_attr(not(feature = "hardware"), allow(dead_code))]
struct HardwareSpendAuthorizationDialogContent {
    root: Entity<WalletRoot>,
    completion: HardwareSpendAuthorizationCompletion,
    gas_review: Option<HardwareGasPaymentReview>,
    summary: SpendAuthorizationSummary,
    device_label: &'static str,
    pending: bool,
    cancelled: bool,
    completed: bool,
    payload_open: bool,
    row_group_open: bool,
    details_open: bool,
    error: Option<Arc<str>>,
}

impl HardwareSpendAuthorizationDialogContent {
    #[allow(clippy::missing_const_for_fn)]
    fn new(
        root: Entity<WalletRoot>,
        completion: HardwareSpendAuthorizationCompletion,
        gas_review: Option<HardwareGasPaymentReview>,
        summary: SpendAuthorizationSummary,
        device_label: &'static str,
    ) -> Self {
        Self {
            root,
            completion,
            gas_review,
            summary,
            device_label,
            pending: false,
            cancelled: false,
            completed: false,
            payload_open: false,
            row_group_open: false,
            details_open: false,
            error: None,
        }
    }

    fn cancel(&mut self, cx: &mut Context<'_, Self>) {
        if self.completed {
            return;
        }
        self.cancelled = true;
        if let Some(intent) = self.completion.private_intent() {
            self.root.update(cx, |root, cx| {
                if let Some(execution) = intent.gateway_execution() {
                    execution.cancel_private_authorization();
                    root.release_gateway_private_form(execution, cx);
                }
                root.cancel_spend_authorization(&intent, cx);
            });
        }
        cx.notify();
    }

    fn toggle_payload(&mut self, cx: &mut Context<'_, Self>) {
        self.payload_open = !self.payload_open;
        cx.notify();
    }

    fn toggle_row_group(&mut self, cx: &mut Context<'_, Self>) {
        self.row_group_open = !self.row_group_open;
        cx.notify();
    }

    fn toggle_details(&mut self, cx: &mut Context<'_, Self>) {
        self.details_open = !self.details_open;
        cx.notify();
    }

    /// The dialog's footer, which stays in view while the content scrolls.
    fn render_footer(&self, dialog: &Entity<Self>) -> gpui::Div {
        let dialog = dialog.clone();
        let pending = self.pending;
        let submit_label = if pending || self.error.is_none() {
            format!("Approve on {}", self.device_label)
        } else {
            "Try again".to_owned()
        };
        div()
            .flex()
            .flex_wrap()
            .justify_end()
            .gap_2()
            .child(
                app_button("wallet-hardware-spend-auth-cancel", "Cancel")
                    .flex_none()
                    .disabled(pending)
                    .on_click(move |_event, window, cx| {
                        window.close_dialog(cx);
                    }),
            )
            .child(
                app_button("wallet-hardware-spend-auth-submit", submit_label)
                    .primary()
                    .flex_none()
                    .disabled(pending)
                    .on_click(move |_event, window, cx| {
                        dialog.update(cx, |dialog, cx| dialog.start(window, cx));
                    }),
            )
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn start(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.pending {
            return;
        }
        self.pending = true;
        self.cancelled = false;
        self.error = None;
        cx.notify();

        #[cfg(not(feature = "hardware"))]
        {
            let _ = window;
            self.pending = false;
            self.error = Some(Arc::from(
                "Hardware wallet support is not enabled in this build. Rebuild the wallet with the hardware feature to authorize hardware-derived spends.",
            ));
            cx.notify();
        }

        #[cfg(feature = "hardware")]
        {
            let root = self.root.clone();
            let completion = self.completion.clone();
            let gas_review = self.gas_review.clone();
            if root.update(cx, |root, cx| {
                root.hardware_gas_payment_review(&completion, cx)
            }) != gas_review
            {
                self.pending = false;
                self.error =
                    Some("The action changed. Close this dialog and review it again.".into());
                cx.notify();
                return;
            }
            let approved_view = root.read(cx).view_session.clone();
            let approved_generation = root.read(cx).active_wallet_generation;
            let task = root.update(cx, |root, cx| {
                root.start_hardware_spend_authorization_task(&completion, window, cx)
            });
            match task {
                Ok(join) => {
                    cx.spawn_in(window, async move |this, cx| {
                        let result = join.await;
                        let _ = this.update_in(cx, |dialog, window, cx| {
                            if dialog.cancelled {
                                return;
                            }
                            dialog.pending = false;
                            match result {
                                Ok(Ok((authorization, destination, hardware_session))) => {
                                    let root = dialog.root.clone();
                                    if root.read(cx).active_wallet_generation != approved_generation
                                        || !approved_view.as_ref().zip(root.read(cx).view_session.as_ref())
                                            .is_some_and(|(approved, current)| approved.is_same_wallet_session(current))
                                    {
                                        dialog.error = Some("The wallet session changed. Close this dialog and authorize the action again.".into());
                                        cx.notify();
                                        return;
                                    }
                                    if root.update(cx, |root, cx| root.hardware_gas_payment_review(&completion, cx)) != gas_review {
                                        dialog.error = Some("The action changed while awaiting the device. Review it again.".into());
                                        cx.notify();
                                        return;
                                    }
                                    if completion.private_intent().is_some_and(|intent| !intent.approve_gateway_review(root.read(cx))) {
                                        return;
                                    }
                                    dialog.completed = true;
                                    window.close_dialog(cx);
                                    root.update(cx, |root, cx| {
                                        root.refresh_active_hardware_profile_session(
                                            hardware_session,
                                            cx,
                                        );
                                        match completion {
                                            HardwareSpendAuthorizationCompletion::Continue(intent) | HardwareSpendAuthorizationCompletion::ExecutorWithGasPayer { intent, .. } => {
                                                root.continue_authorized_spend_with_destination(
                                                    intent,
                                                    authorization,
                                                    destination,
                                                    window,
                                                    cx,
                                                );
                                            }
                                             HardwareSpendAuthorizationCompletion::PrivateSendSelfBroadcast {
                                                 key,
                                                 vault_password,
                                                 authorization_limit,
                                                 execution,
                                             } => {
                                                root.generate_send_calldata_authorized_with_gas_password(
                                                    key,
                                                     authorization,
                                                     Some(vault_password),
                                                     authorization_limit,
                                                     window,
                                                    cx,
                                                );
                                                if let Some(execution) = execution {
                                                    root.reject_gateway_private_authorization(&execution, cx);
                                                }
                                            }
                                             HardwareSpendAuthorizationCompletion::PrivateUnshieldSelfBroadcast {
                                                 key,
                                                 vault_password,
                                                 authorization_limit,
                                                 execution,
                                             } => {
                                                root.generate_unshield_calldata_authorized_with_gas_password(
                                                    key,
                                                     authorization,
                                                     Some(vault_password),
                                                     authorization_limit,
                                                     window,
                                                    cx,
                                                );
                                                if let Some(execution) = execution {
                                                    root.reject_gateway_private_authorization(&execution, cx);
                                                }
                                            }
                                            HardwareSpendAuthorizationCompletion::BlockedShieldRefund {
                                                utxo_id,
                                                vault_password,
                                            } => {
                                                root.submit_blocked_shield_refund_authorized(
                                                    utxo_id,
                                                    authorization,
                                                    Some(vault_password),
                                                    window,
                                                    cx,
                                                );
                                            }
                                        }
                                    });
                                }
                                Ok(Err(error)) => {
                                    let message = hardware_spend_authorization_error_message(&error);
                                    let root = dialog.root.clone();
                                    root.update(cx, |root, cx| {
                                        root.discard_active_trezor_session_if_stale(&message, cx);
                                    });
                                    dialog.error = Some(Arc::from(message));
                                    cx.notify();
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "desktop hardware spend authorization task failed");
                                    dialog.error = Some(Arc::from(
                                        "Hardware spend authorization failed. See logs for non-sensitive diagnostics.",
                                    ));
                                    cx.notify();
                                }
                            }
                        });
                    })
                    .detach();
                }
                Err(message) => {
                    self.pending = false;
                    self.error = Some(message);
                    cx.notify();
                }
            }
        }
    }
}

impl SpendAuthorizationDialogContent {
    fn new(
        root: Entity<WalletRoot>,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        initial_lifetime: SpendAuthorizationLifetime,
        lease: Weak<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let password_input = new_masked_input(window, cx, "Vault password");
        cx.subscribe_in(
            &password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        let lifetime_select = new_spend_authorization_lifetime_select(initial_lifetime, window, cx);
        cx.subscribe(
            &lifetime_select,
            |this, _select, event: &SelectEvent<SearchableVec<SpendAuthorizationLifetime>>, cx| {
                if let SelectEvent::Confirm(Some(lifetime)) = event {
                    this.set_lifetime(*lifetime, cx);
                }
            },
        )
        .detach();
        Self {
            root,
            intent,
            summary,
            password_input,
            lifetime: initial_lifetime,
            lifetime_select,
            payload_open: false,
            row_group_open: false,
            details_open: false,
            error: None,
            pending: false,
            cancelled: false,
            review_authorization: None,
            review_focus: cx.focus_handle(),
            device_auth: None,
            device_auth_pending: false,
            lease,
        }
    }

    fn is_open(&self) -> bool {
        !self.cancelled && self.lease.upgrade().is_some_and(|open| open.get())
    }

    fn focus_password(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.review_authorization.is_some() {
            self.review_focus.focus(window, cx);
            return;
        }
        self.password_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.pending || !self.is_open() || self.device_auth_pending {
            return;
        }
        if let Some((scope, authorization)) = self.review_authorization.take() {
            let intent = self.intent.clone();
            self.root.update(cx, |root, cx| {
                if root.current_spend_authorization_scope() != scope
                    || !intent.approve_gateway_review(root)
                {
                    window.close_dialog(cx);
                    return;
                }
                window.close_dialog(cx);
                root.continue_authorized_spend(intent, authorization, window, cx);
            });
            return;
        }
        let password = Zeroizing::new(self.password_input.read(cx).value().to_string());
        self.password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if password.trim().is_empty() {
            self.error = Some(Arc::from(
                "Enter the vault password to authorize this spend",
            ));
            cx.notify();
            return;
        }
        self.submit_password(password, self.lifetime, window, cx);
    }

    fn submit_with_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.pending
            || !self.is_open()
            || self.device_auth_pending
            || self.review_authorization.is_some()
        {
            return;
        }
        let Some(prompt) = self.device_auth.clone() else {
            return;
        };
        self.device_auth_pending = true;
        self.error = None;
        cx.notify();
        prompt.run(
            method,
            DEVICE_AUTH_REASON_SPEND,
            window,
            cx,
            move |dialog, outcome, window, cx| {
                dialog.finish_device_auth(method, outcome, window, cx);
            },
        );
    }

    fn finish_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        outcome: DeviceAuthPassword,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.device_auth_pending = false;
        if !self.is_open() {
            return;
        }
        match outcome {
            DeviceAuthPassword::Password(password) => {
                // Watch approval is for this action only. It must never seed
                // the reusable password authorization cache.
                let lifetime = match method {
                    DeviceAuthMethod::AppleWatch => SpendAuthorizationLifetime::Once,
                    DeviceAuthMethod::TouchId => self.lifetime,
                };
                self.submit_password(password, lifetime, window, cx);
            }
            DeviceAuthPassword::Cancelled => self.focus_password(window, cx),
            DeviceAuthPassword::Failed(message) => {
                self.device_auth = self
                    .device_auth
                    .take()
                    .and_then(|prompt| prompt.without(method));
                self.error = Some(message);
                self.focus_password(window, cx);
            }
        }
        cx.notify();
    }

    fn submit_password(
        &mut self,
        password: Zeroizing<String>,
        lifetime: SpendAuthorizationLifetime,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.is_open() {
            return;
        }
        let root = self.root.read(cx);
        let Some(store) = root.vault_store.clone() else {
            self.error = Some("Wallet vault storage is unavailable".into());
            cx.notify();
            return;
        };
        let approved_scope = root.current_spend_authorization_scope();
        let approved_generation = root.active_wallet_generation;
        // Check the password before dismissing the review or starting an operation.
        // The operation still obtains its own scoped spend grant when it runs.
        let join = root.runtime.spawn_blocking(move || {
            store.create_spend_grant(&password).map(drop)?;
            WalletRoot::renew_device_auth(&store, &password);
            Ok::<_, VaultError>(password)
        });
        self.pending = true;
        self.error = None;
        let root = self.root.downgrade();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = match join.await {
                Ok(result) => result.map_err(|error| match error {
                    VaultError::UnlockFailed => Arc::from("Incorrect vault password. Try again."),
                    error => error.to_string().into(),
                }),
                Err(_) => Err("Password check failed. Try again.".into()),
            };
            // Password verification can reseal device enrollments even after the review closes.
            let _ = root.update(cx, |root, cx| {
                root.refresh_device_auth_status();
                cx.notify();
            });
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.pending = false;
                if !dialog.is_open() {
                    return;
                }
                let root = dialog.root.clone();
                if root.read(cx).active_wallet_generation != approved_generation
                    || root.read(cx).current_spend_authorization_scope() != approved_scope
                {
                    dialog.cancel(cx);
                    dialog.error = Some(
                        "The wallet session changed. Close this dialog and authorize the action again."
                            .into(),
                    );
                    cx.notify();
                    return;
                }
                match result {
                    Ok(password) => {
                        let intent = dialog.intent.clone();
                        if let Err(error) = root.update(cx, |root, cx| {
                            root.finish_spend_authorization(intent, password, lifetime, window, cx)
                        }) {
                            dialog.error = Some(error);
                            dialog.focus_password(window, cx);
                        }
                    }
                    Err(error) => {
                        root.update(cx, WalletRoot::clear_spend_authorization);
                        dialog.error = Some(error);
                        dialog.focus_password(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<'_, Self>) {
        self.cancelled = true;
        self.root.update(cx, |root, cx| {
            root.cancel_spend_authorization(&self.intent, cx);
        });
        cx.notify();
    }

    fn set_lifetime(&mut self, lifetime: SpendAuthorizationLifetime, cx: &mut Context<'_, Self>) {
        if !self.pending && !self.device_auth_pending && self.lifetime != lifetime {
            self.lifetime = lifetime;
            cx.notify();
        }
    }

    fn toggle_payload(&mut self, cx: &mut Context<'_, Self>) {
        self.payload_open = !self.payload_open;
        cx.notify();
    }

    fn toggle_row_group(&mut self, cx: &mut Context<'_, Self>) {
        self.row_group_open = !self.row_group_open;
        cx.notify();
    }

    fn toggle_details(&mut self, cx: &mut Context<'_, Self>) {
        self.details_open = !self.details_open;
        cx.notify();
    }

    /// The dialog's footer, which stays in view while the content scrolls.
    fn render_footer(&self, dialog: &Entity<Self>) -> gpui::Div {
        let cancel_dialog = dialog.clone();
        let dialog = dialog.clone();
        div()
            .flex()
            .flex_wrap()
            .justify_end()
            .gap_2()
            .child(
                app_button("wallet-spend-auth-cancel", "Cancel")
                    .flex_none()
                    .on_click(move |_event, window, cx| {
                        cancel_dialog.update(cx, Self::cancel);
                        window.close_dialog(cx);
                    }),
            )
            .child(
                app_button(
                    "wallet-spend-auth-submit",
                    self.summary.confirm_label.to_string(),
                )
                .track_focus(&self.review_focus)
                .primary()
                .flex_none()
                .loading(self.pending)
                .disabled(self.pending || self.cancelled || self.device_auth_pending)
                .on_click(move |_event, window, cx| {
                    dialog.update(cx, |dialog, cx| dialog.submit(window, cx));
                }),
            )
    }
}

impl gpui::Render for SpendAuthorizationDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        let payload_dialog = dialog.clone();
        let row_group_dialog = dialog.clone();
        let device_auth_dialog = dialog.clone();
        let payload = self.summary.payload.as_ref().map(|payload| {
            render_spend_authorization_payload(payload, self.payload_open, move |_, _, cx| {
                payload_dialog.update(cx, Self::toggle_payload);
            })
        });
        let row_group = self.summary.row_group.as_ref().map(|group| {
            render_spend_authorization_details(
                SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE,
                group,
                self.row_group_open,
                move |_, _, cx| {
                    row_group_dialog.update(cx, Self::toggle_row_group);
                },
                cx,
            )
            .into_any_element()
        });
        let details = self.summary.details.as_ref().map(|details| {
            render_spend_authorization_details(
                SPEND_AUTHORIZATION_DETAILS_TOGGLE,
                details,
                self.details_open,
                move |_, _, cx| {
                    dialog.update(cx, Self::toggle_details);
                },
                cx,
            )
            .into_any_element()
        });
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .children(
                self.summary
                    .steps
                    .as_ref()
                    .map(render_spend_authorization_steps),
            )
            .when(!self.summary.detail.is_empty(), |this| {
                this.child(app_muted_text(self.summary.detail.to_string()).whitespace_normal())
            })
            .child(render_spend_authorization_summary(
                &self.summary,
                row_group,
                details,
                cx,
            ))
            .when_some(self.summary.context.as_ref(), |this, context| {
                this.child(app_muted_text(context.to_string()).whitespace_normal())
            })
            .children(
                self.summary
                    .warnings
                    .iter()
                    .enumerate()
                    .map(|(index, warning)| {
                        Alert::warning(
                            SharedString::from(format!("wallet-spend-auth-warning-{index}")),
                            warning.to_string(),
                        )
                        .small()
                    }),
            )
            .children(payload)
            .when(self.review_authorization.is_none(), |this| {
                let device_auth_dialog = device_auth_dialog.clone();
                this.child(masked_input_with_device_auth(
                    &self.password_input,
                    self.pending || self.cancelled || self.device_auth_pending,
                    device_auth_buttons(
                        self.device_auth.as_ref(),
                        "wallet-spend-auth-touch-id",
                        self.device_auth_pending,
                        self.pending || self.cancelled,
                        move |method, window, cx| {
                            device_auth_dialog.update(cx, |dialog, cx| {
                                dialog.submit_with_device_auth(method, window, cx);
                            });
                        },
                    ),
                ))
                .child(render_spend_authorization_lifetime_row(
                    &self.lifetime_select,
                    self.pending || self.cancelled || self.device_auth_pending,
                ))
                .when(
                    self.device_auth
                        .as_ref()
                        .is_some_and(|prompt| prompt.includes(DeviceAuthMethod::AppleWatch)),
                    |this| {
                        this.child(
                            app_muted_text("Apple Watch always authorizes only this spend.")
                                .text_xs()
                                .whitespace_normal(),
                        )
                    },
                )
            })
            .when(
                self.review_authorization.is_none()
                    && self.lifetime.requires_reusable_authorization_warning(),
                |this| {
                    this.child(
                        Alert::warning(
                            "wallet-spend-auth-session-warning",
                            SPEND_AUTHORIZATION_SESSION_WARNING,
                        )
                        .small(),
                    )
                },
            )
            .when_some(self.error.as_ref(), |this, error| {
                this.child(
                    app_muted_text(error.to_string())
                        .whitespace_normal()
                        .text_color(rgb(theme::DANGER))
                        .debug_selector(|| "wallet-spend-auth-error".into()),
                )
            })
    }
}

impl gpui::Render for HardwareSpendAuthorizationDialogContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let dialog = cx.entity();
        let payload_dialog = dialog.clone();
        let row_group_dialog = dialog.clone();
        let payload = self.summary.payload.as_ref().map(|payload| {
            render_spend_authorization_payload(payload, self.payload_open, move |_, _, cx| {
                payload_dialog.update(cx, Self::toggle_payload);
            })
        });
        let row_group = self.summary.row_group.as_ref().map(|group| {
            render_spend_authorization_details(
                SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE,
                group,
                self.row_group_open,
                move |_, _, cx| {
                    row_group_dialog.update(cx, Self::toggle_row_group);
                },
                cx,
            )
            .into_any_element()
        });
        let details = self.summary.details.as_ref().map(|details| {
            render_spend_authorization_details(
                SPEND_AUTHORIZATION_DETAILS_TOGGLE,
                details,
                self.details_open,
                move |_, _, cx| {
                    dialog.update(cx, Self::toggle_details);
                },
                cx,
            )
            .into_any_element()
        });
        let pending = self.pending;
        let device = self.device_label;
        let show_trezor_app_passphrase = self
            .root
            .read(cx)
            .current_session_needs_trezor_app_passphrase();
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase_input = self.root.read(cx).trezor_app_passphrase_input.clone();
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_prompt = {
            let root = self.root.read(cx);
            root.hardware_profile_unlock
                .trezor_pin_matrix_prompt
                .as_ref()
                .map(|prompt| {
                    super::vault_ui::render_trezor_pin_matrix_prompt(&self.root, prompt)
                        .into_any_element()
                })
        };
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_prompt: Option<AnyElement> = None;

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .children(
                self.summary
                    .steps
                    .as_ref()
                    .map(render_spend_authorization_steps),
            )
            .when(!self.summary.detail.is_empty(), |this| {
                this.child(app_muted_text(self.summary.detail.to_string()).whitespace_normal())
            })
            .child(render_spend_authorization_summary(
                &self.summary,
                row_group,
                details,
                cx,
            ))
            .when_some(self.summary.context.as_ref(), |this, context| {
                this.child(app_muted_text(context.to_string()).whitespace_normal())
            })
            .children(self.summary.warnings.iter().enumerate().map(|(index, warning)| {
                Alert::warning(
                    SharedString::from(format!("wallet-hardware-spend-auth-warning-{index}")),
                    warning.to_string(),
                )
                .small()
            }))
            .children(payload)
            .child(Alert::warning(
                "wallet-hardware-spend-custody-warning",
                format!(
                    "Your {device} will not show the details of this action. Check them here before you approve."
                ),
            ).small())
            .child(
                app_muted_text(hardware_spend_authorization_instruction(self.device_label))
                    .whitespace_normal(),
            )
            .when(show_trezor_app_passphrase, |this| {
                #[cfg(feature = "hardware")]
                {
                    this.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(app_strong_text("Trezor app passphrase"))
                            .child(
                                app_muted_text(
                                    "The Trezor session expired. Enter the passphrase for the wallet you intend to spend from.",
                                )
                                .whitespace_normal(),
                            )
                            .child(app_masked_input(&trezor_app_passphrase_input, pending)),
                    )
                }
                #[cfg(not(feature = "hardware"))]
                {
                    this
                }
            })
            .children(trezor_pin_matrix_prompt)
            .when(pending, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(Spinner::new().small())
                        .child(app_muted_text(format!("Waiting for {device}…"))),
                )
            })
            .when_some(self.error.as_ref(), |this, error| {
                this.child(app_muted_text(error.to_string()).text_color(rgb(theme::DANGER)))
            })
    }
}

/// The summary's cards, row group, rows, details disclosure and disclosure alert. The row
/// group, compact rows and the details disclosure form one list.
fn render_spend_authorization_summary(
    summary: &SpendAuthorizationSummary,
    row_group: Option<AnyElement>,
    details: Option<AnyElement>,
    cx: &App,
) -> gpui::Div {
    // The rows count on from the row group's, so each row's controls keep their own ids.
    let first_row = summary.grouped_rows().len();
    let rows =
        if summary.compact_rows {
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .children(summary.rows.iter().enumerate().map(|(row_index, row)| {
                    render_spend_authorization_compact_row(first_row + row_index, row, cx)
                }))
                .into_any_element()
        } else {
            DescriptionList::vertical()
                .large()
                .bordered(false)
                .columns(1)
                .children(
                    summary.rows.iter().enumerate().map(|(row_index, row)| {
                        spend_authorization_summary_item(row_index, row, cx)
                    }),
                )
                .into_any_element()
        };
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_3()
        .when_some(summary.cards.as_ref(), |this, cards| {
            this.child(div().w_full().min_w_0().flex().flex_col().gap_1().children(
                cards.iter().enumerate().map(|(card_index, card)| {
                    render_spend_authorization_card(card_index, card, cx)
                }),
            ))
        })
        .map(|this| {
            if row_group.is_none() && details.is_none() {
                return this.child(rows);
            }
            this.child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .children(row_group)
                    .child(rows)
                    .children(details),
            )
        })
        .when_some(summary.disclosure.as_ref(), |this, disclosure| {
            this.child(render_spend_authorization_disclosure(disclosure))
        })
}

/// A change's text at the small size: in the danger colour when adverse, else the success
/// colour.
fn spend_authorization_delta_text(delta: &SpendAuthorizationAmountDelta, cx: &App) -> gpui::Div {
    app_text(delta.text.clone())
        .flex_none()
        .text_xs()
        .whitespace_nowrap()
        .text_color(if delta.adverse {
            cx.theme().danger
        } else {
            cx.theme().success
        })
}

/// A card's label, its icon and amount with the amount's change beside it and the USD value at
/// the right edge, then its lines.
fn render_spend_authorization_card(
    card_index: usize,
    card: &SpendAuthorizationCard,
    cx: &App,
) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE))
        .child(spend_authorization_label(
            &card.label,
            rems(0.75).into(),
            |text| app_muted_text(text).text_xs(),
        ))
        .child(
            // In a narrow dialog the USD value wraps under the amount, and the change under
            // the amount before that. The amount itself never breaks.
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_x_2()
                .child(
                    div()
                        .flex_auto()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .children(match card.network {
                            // Larger than the plain icon, so the network chip stays readable
                            // beside the card's amount.
                            Some(chain_id) => Some(
                                network_token_icon(card.icon.clone(), chain_id, 1.75)
                                    .into_any_element(),
                            ),
                            None => card.icon.clone().map(|icon| {
                                img(icon)
                                    .size(px(20.0))
                                    .rounded_full()
                                    .flex_none()
                                    .into_any_element()
                            }),
                        })
                        .child(
                            div()
                                .flex_auto()
                                .min_w_0()
                                .flex()
                                .flex_wrap()
                                .items_baseline()
                                .gap_x_2()
                                .child(
                                    app_amount_text(card.amount.to_string())
                                        .text_color(rgb(theme::TEXT))
                                        .whitespace_nowrap(),
                                )
                                .children(
                                    card.delta
                                        .as_ref()
                                        .map(|delta| spend_authorization_delta_text(delta, cx)),
                                ),
                        ),
                )
                .children(card.usd.as_ref().map(|usd| {
                    app_muted_text(usd.to_string())
                        .flex_none()
                        .ml_auto()
                        .whitespace_nowrap()
                })),
        )
        .children(card.lines.iter().map(|line| {
            match line {
                SpendAuthorizationCardLine::Text {
                    before,
                    strong,
                    after,
                } => div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .items_baseline()
                    .gap_x_1()
                    .when(!before.is_empty(), |this| {
                        this.child(app_muted_text(before.to_string()).text_xs())
                    })
                    .when(!strong.is_empty(), |this| {
                        this.child(app_strong_text(strong.to_string()).text_xs())
                    })
                    .when(!after.is_empty(), |this| {
                        this.child(app_muted_text(after.to_string()).text_xs())
                    }),
                SpendAuthorizationCardLine::Receiver { address, label } => {
                    spend_authorization_card_address_line(
                        card_index,
                        "to",
                        None,
                        address,
                        label.clone(),
                        "Copy receiver",
                    )
                }
                SpendAuthorizationCardLine::Account { name, address } => {
                    spend_authorization_card_address_line(
                        card_index,
                        "from",
                        Some(name),
                        address,
                        None,
                        "Copy address",
                    )
                }
            }
        }))
}

/// A card's address line: `lead`, the account's `name` when the line shows it, and the
/// shortened `address` with a copy button. Hovering the address shows it in full, after
/// `hover_label` when it has one.
fn spend_authorization_card_address_line(
    card_index: usize,
    lead: &'static str,
    name: Option<&Arc<str>>,
    address: &Arc<str>,
    hover_label: Option<Arc<str>>,
    copy_tooltip: &'static str,
) -> gpui::Div {
    let full_address = address.to_string();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .child(app_muted_text(lead).text_xs())
        .children(name.map(|name| {
            app_text(name.to_string())
                .text_xs()
                .text_color(rgb(theme::TEXT))
        }))
        .child(
            div()
                .id(("wallet-spend-auth-card-receiver", card_index))
                .min_w_0()
                .tooltip(move |window, cx| {
                    let (full_address, label) = (full_address.clone(), hover_label.clone());
                    Tooltip::element(move |_, _| {
                        div()
                            .flex()
                            .flex_col()
                            .children(label.as_ref().map(ToString::to_string))
                            .child(
                                div()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .child(full_address.clone()),
                            )
                    })
                    .build(window, cx)
                })
                .child(
                    app_text(spend_authorization_recipient_display(address))
                        .text_xs()
                        .text_color(rgb(theme::TEXT))
                        .font_family(APP_MONO_FONT_FAMILY),
                ),
        )
        .child(
            div()
                .id(("wallet-spend-auth-card-copy-action", card_index))
                .flex_none()
                .tooltip(move |window, cx| Tooltip::new(copy_tooltip).build(window, cx))
                .child(clipboard_with_toast(
                    ("wallet-spend-auth-card-copy", card_index),
                    address.to_string(),
                )),
        )
}

/// A row on one line: its label, its value at the right, the value's change, and a slot for
/// its info button. The slot stays empty without a hint, so the values line up. A value too wide
/// to sit beside its label wraps under it, with its change and info button.
fn render_spend_authorization_compact_row(
    row_index: usize,
    row: &SpendAuthorizationSummaryRow,
    cx: &App,
) -> gpui::Div {
    let value = if row.shortened_copyable {
        let copy_tooltip = row.copy_tooltip();
        div()
            .flex_auto()
            .min_w_0()
            .flex()
            .items_center()
            .justify_end()
            .gap_1()
            .child(
                app_text(row.shortened_value())
                    .min_w_0()
                    .text_color(rgb(theme::TEXT))
                    .font_family(APP_MONO_FONT_FAMILY),
            )
            .child(
                div()
                    .id(("wallet-spend-auth-copy-action", row_index))
                    .flex_none()
                    .tooltip(move |window, cx| Tooltip::new(copy_tooltip.clone()).build(window, cx))
                    .child(clipboard_with_toast(
                        ("wallet-spend-auth-copy", row_index),
                        row.value.to_string(),
                    )),
            )
    } else {
        let value = match &row.value_network {
            // The network wraps under the text before it, at the right edge too.
            Some(value) => spend_authorization_row_label(value, app_text)
                .flex_wrap()
                .justify_end(),
            None => app_text(row.value.to_string()),
        };
        value
            .flex_auto()
            .min_w_0()
            .text_right()
            .text_color(rgb(theme::TEXT))
            .whitespace_normal()
    };
    let label = match &row.label_network {
        Some(label) => spend_authorization_row_label(label, app_muted_text),
        None => app_muted_text(row.label.to_string()),
    };
    div()
        .w_full()
        .min_w_0()
        .min_h(SUMMARY_COMPACT_ROW_MIN_HEIGHT)
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x_2()
        .child(label.flex_none().whitespace_nowrap())
        .child(value)
        .children(
            row.delta
                .as_ref()
                .map(|delta| spend_authorization_delta_text(delta, cx)),
        )
        .child(
            div()
                .flex_none()
                .size_5()
                .flex()
                .items_center()
                .justify_center()
                .children(row.hint.as_ref().map(|hint| {
                    spend_authorization_hint(
                        format!("wallet-spend-auth-row-hint-{row_index}"),
                        hint,
                        None,
                    )
                })),
        )
}

/// The disclosure alert: the hint's title, the summary as its message, and an info button
/// inside its right edge. The whole alert shows the hint's card on hover and opens it on click.
/// A hint with a warning makes it a warning alert.
fn render_spend_authorization_disclosure(disclosure: &SpendAuthorizationDisclosure) -> gpui::Div {
    let id = "wallet-spend-auth-disclosure-alert";
    let summary = disclosure.summary.to_string();
    let alert = if disclosure.hint.warning.is_some() {
        Alert::warning(id, summary)
    } else {
        Alert::info(id, summary)
    };
    div().w_full().min_w_0().child(spend_authorization_hint(
        "wallet-spend-auth-disclosure",
        &disclosure.hint,
        Some(
            alert
                .title(disclosure.hint.title.to_string())
                .small()
                // Room for the info button.
                .pr_10()
                .into_any_element(),
        ),
    ))
}

/// An info button for `hint`: the hint's card as a tooltip on hover, and in a popover on click
/// or from the keyboard. Escape closes the popover and returns focus to the button. With
/// `line`, the button sits inside that element's right edge, and the whole element shows and
/// opens the card.
fn spend_authorization_hint(
    id: impl Into<SharedString>,
    hint: &SpendAuthorizationHint,
    line: Option<AnyElement>,
) -> Popover {
    let id = id.into();
    let content = hint.clone();
    Popover::new(id.clone())
        .anchor(Anchor::TopRight)
        .trigger(SpendAuthorizationHintTrigger {
            id,
            hint: hint.clone(),
            line,
            open: false,
        })
        .content(move |_, window, _| spend_authorization_hint_card(&content, window))
}

/// A hint popover's trigger. The popover marks it selected while open, which drops the hover
/// tooltip so the card doesn't show twice.
#[derive(IntoElement)]
struct SpendAuthorizationHintTrigger {
    id: SharedString,
    hint: SpendAuthorizationHint,
    line: Option<AnyElement>,
    open: bool,
}

impl Selectable for SpendAuthorizationHintTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.open = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.open
    }
}

impl RenderOnce for SpendAuthorizationHintTrigger {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            hint,
            line,
            open,
        } = self;
        let button = app_button_base(SharedString::from(format!("{id}-button")))
            .ghost()
            .xsmall()
            .icon(IconName::Info)
            .selected(open)
            .accessibility_label(hint.title.to_string());
        let selector = id.to_string();
        div()
            .id(SharedString::from(format!("{id}-hover")))
            .debug_selector(move || selector)
            .map(|this| match line {
                Some(line) => this
                    .relative()
                    .w_full()
                    .min_w_0()
                    .cursor_pointer()
                    .child(line)
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_2()
                            .flex()
                            .items_center()
                            .child(button),
                    ),
                None => this.flex_none().child(button),
            })
            .when(!open, |this| {
                this.tooltip(move |window, cx| {
                    let hint = hint.clone();
                    Tooltip::element(move |window, _| spend_authorization_hint_card(&hint, window))
                        .build(window, cx)
                })
            })
    }
}

/// `hint` as a hint card: its paragraphs under the title, then its warning in the warning
/// colour, which the title takes too, then its fact under a rule.
fn spend_authorization_hint_card(hint: &SpendAuthorizationHint, window: &Window) -> gpui::Div {
    let title_color = if hint.warning.is_some() {
        theme::WARNING
    } else {
        theme::INFO
    };
    hint_card(hint.title.to_string(), title_color, window)
        .children(
            hint.paragraphs
                .iter()
                .map(|paragraph| div().whitespace_normal().child(paragraph.to_string())),
        )
        .children(hint.warning.as_ref().map(|warning| {
            div()
                .whitespace_normal()
                .text_color(rgb(theme::WARNING))
                .child(warning.to_string())
        }))
        .children(hint.fact.as_ref().map(|(label, value)| {
            div()
                .w_full()
                .pt_1()
                .border_t_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .flex()
                .items_baseline()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(label.to_string()),
                )
                .child(div().flex_none().child(value.to_string()))
        }))
}

/// `label` as the text `styled` makes of a string. The mark of a network it names is `mark`,
/// that text's size, square.
fn spend_authorization_label(
    label: &SpendAuthorizationLabel,
    mark: gpui::AbsoluteLength,
    styled: impl FnOnce(String) -> gpui::Div,
) -> gpui::Div {
    let text = styled(label.prefix.to_string());
    let Some(chain_id) = label.network else {
        return text;
    };
    text.flex().items_center().gap_x_1().child(
        network_mention(chain_id, mark).when(!label.suffix.is_empty(), |mention| {
            mention.child(label.suffix.to_string())
        }),
    )
}

/// A compact row's label or value that names a network, as the text `styled` makes of a
/// string. The network's mark is the size of the one in a card's label.
fn spend_authorization_row_label(
    label: &SpendAuthorizationLabel,
    styled: impl FnOnce(String) -> gpui::Div,
) -> gpui::Div {
    spend_authorization_label(label, rems(0.75).into(), styled)
}

/// A dialog title, with a chip naming the summary's network beside it when it has one.
fn spend_authorization_title(title: &str, network: Option<u64>) -> gpui::Div {
    let title = app_strong_text(title.to_owned());
    let Some(chain_id) = network else {
        return title;
    };
    div()
        .min_w_0()
        // Clear of the dialog's close button, so the chip wraps under the title instead.
        .pr_6()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(title)
        .child(
            network_mention(chain_id, rems(0.75).into())
                .flex_none()
                .text_xs()
                .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                .text_color(rgb(theme::TEXT_MUTED))
                .px_2()
                .rounded_full()
                .border_1()
                .border_color(rgb(theme::BORDER)),
        )
}

/// A stepper: numbered badges and their labels joined by a connector, with an info button at
/// the end. A completed step shows a check in the success colour, the current one a filled
/// badge, and a later one an outlined badge and a muted label.
/// A step that doesn't fit beside the one before it wraps to the next line, and a label wider
/// than the strip truncates.
pub(super) fn render_spend_authorization_steps(steps: &SpendAuthorizationSteps) -> gpui::Div {
    let mut strip = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .py_2()
        .pl_3()
        .pr_2()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_HOVER_SUBTLE));
    for (index, label) in steps.labels.iter().enumerate() {
        let number = index + 1;
        let (done, current) = (number < steps.current, number == steps.current);
        if index > 0 {
            // The connector out of a completed step.
            strip = strip.child(div().flex_1().min_w(px(16.0)).h(px(1.0)).bg(rgb(
                if index < steps.current {
                    theme::SUCCESS
                } else {
                    theme::BORDER_STRONG
                },
            )));
        }
        let badge = div()
            .flex_none()
            .size(px(22.0))
            .rounded_full()
            .border_1()
            .flex()
            .items_center()
            .justify_center();
        let badge = if done {
            badge
                .border_color(rgb(theme::SUCCESS))
                .bg(rgb(theme::SUCCESS))
                .child(
                    Icon::new(IconName::Check)
                        .xsmall()
                        .text_color(rgb(theme::PRIMARY_FOREGROUND)),
                )
        } else if current {
            badge
                .border_color(rgb(theme::PRIMARY))
                .bg(rgb(theme::PRIMARY))
                .child(
                    app_text(number.to_string())
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(theme::PRIMARY_FOREGROUND)),
                )
        } else {
            badge.border_color(rgb(theme::BORDER_STRONG)).child(
                app_muted_text(number.to_string())
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD),
            )
        };
        let label_color = if done {
            theme::SUCCESS
        } else if current {
            theme::TEXT
        } else {
            theme::TEXT_MUTED
        };
        strip = strip.child(
            div()
                .min_w_0()
                .max_w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(badge)
                .child(
                    spend_authorization_label(label, theme::APP_TEXT_SIZE.into(), app_text)
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(label_color)),
                ),
        );
    }
    strip.child(spend_authorization_hint(
        "wallet-spend-auth-steps-hint",
        &steps.hint,
        None,
    ))
}

/// A details disclosure as one more compact row: its title, its summary at the right, and a
/// chevron where the rows have their info button. Open, its lines and note or its rows follow,
/// indented, and a row group's summary is left out, since its rows carry the amounts. `id`
/// names its toggle.
fn render_spend_authorization_details(
    id: &'static str,
    details: &SpendAuthorizationDetails,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Collapsible {
    let list = div()
        .min_w_0()
        .ml_1()
        .pl_3()
        .flex()
        .flex_col()
        .border_l_1()
        .border_color(rgb(theme::BORDER_SUBTLE));
    let content = div().w_full().min_w_0().flex().flex_col().gap_1().py_1();
    let content = match &details.content {
        SpendAuthorizationDetailsContent::Lines { rows, note } => content
            // The rows end where the values above end, clear of the chevron's column.
            .pr(px(28.0))
            .child(list.gap_1().children(rows.iter().map(|(label, value)| {
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .justify_between()
                    .gap_x_3()
                    .child(
                        app_muted_text(label.to_string())
                            .flex_none()
                            .whitespace_nowrap(),
                    )
                    .child(
                        app_text(value.to_string())
                            .flex_auto()
                            .min_w_0()
                            .text_right()
                            .text_color(rgb(theme::TEXT))
                            .whitespace_normal(),
                    )
            })))
            .when_some(note.as_ref(), |this, note| {
                this.child(
                    app_muted_text(note.to_string())
                        .pl_4()
                        .text_xs()
                        .whitespace_normal(),
                )
            }),
        // Each row's info button sits in the chevron's column.
        SpendAuthorizationDetailsContent::Rows(rows) => content.child(
            list.children(rows.iter().enumerate().map(|(row_index, row)| {
                render_spend_authorization_compact_row(row_index, row, cx)
            })),
        ),
    };
    let summarized = !open || !matches!(details.content, SpendAuthorizationDetailsContent::Rows(_));
    Collapsible::new()
        .open(open)
        .w_full()
        .child(
            div()
                .id(id)
                .w_full()
                .min_w_0()
                .min_h(SUMMARY_COMPACT_ROW_MIN_HEIGHT)
                .flex()
                .flex_wrap()
                .items_center()
                .gap_x_2()
                .cursor_pointer()
                .on_click(on_toggle)
                .child(
                    app_muted_text(details.title.to_string())
                        .flex_none()
                        .whitespace_nowrap(),
                )
                // The summary and its chevron wrap under the title together.
                .child(
                    div()
                        .flex_auto()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap_x_2()
                        .when(summarized, |this| {
                            this.child(
                                app_text(details.collapsed_summary.to_string())
                                    .min_w_0()
                                    .text_right()
                                    .text_color(rgb(theme::TEXT))
                                    .whitespace_normal(),
                            )
                        })
                        .child(
                            div()
                                .flex_none()
                                .size_5()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    Icon::new(if open {
                                        IconName::ChevronUp
                                    } else {
                                        IconName::ChevronDown
                                    })
                                    .xsmall(),
                                ),
                        ),
                ),
        )
        .content(content)
}

fn render_spend_authorization_payload(
    payload: &SpendAuthorizationPayload,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Collapsible {
    let label = payload.label.to_string();
    let value = payload.value.to_string();
    Collapsible::new()
        .open(open)
        .w_full()
        .child(
            div()
                .id("wallet-spend-auth-payload-toggle")
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .cursor_pointer()
                .on_click(on_toggle)
                .child(app_strong_text(if open {
                    format!("Hide full {label}")
                } else {
                    format!("Show full {label}")
                }))
                .child(
                    gpui_component::Icon::new(if open {
                        gpui_component::IconName::ChevronUp
                    } else {
                        gpui_component::IconName::ChevronDown
                    })
                    .xsmall(),
                ),
        )
        .content(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .items_start()
                .gap_2()
                .pt(px(6.0))
                .child(
                    app_text(value.clone())
                        .flex_1()
                        .min_w(px(0.0))
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_size(px(12.0))
                        .whitespace_normal(),
                )
                .child(clipboard_with_toast(
                    "wallet-spend-auth-payload-copy",
                    value,
                )),
        )
}

fn spend_authorization_summary_item(
    row_index: usize,
    row: &SpendAuthorizationSummaryRow,
    cx: &App,
) -> DescriptionItem {
    DescriptionItem::new(row.label.to_string())
        .value(spend_authorization_summary_value(row_index, row, cx))
}

fn spend_authorization_summary_value(
    row_index: usize,
    row: &SpendAuthorizationSummaryRow,
    cx: &App,
) -> AnyElement {
    if let Some(icon_path) = row.icon_path.clone() {
        return div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .gap_1()
            .py(px(2.0))
            .text_color(rgb(theme::TEXT))
            .child(img(icon_path).size(px(20.0)).rounded_full().flex_none())
            .child(
                app_text(row.value.to_string())
                    .flex_1()
                    .min_w(px(0.0))
                    .whitespace_normal(),
            )
            .into_any_element();
    }

    if row.shortened_copyable {
        let display_value = row.shortened_value();
        let copy_tooltip = row.copy_tooltip();
        return div()
            .w_full()
            .flex()
            .items_start()
            .gap_2()
            .py(px(2.0))
            .child(
                app_text(display_value)
                    .min_w(px(0.0))
                    .line_height(px(17.0))
                    .text_color(rgb(theme::TEXT))
                    .font_family(APP_MONO_FONT_FAMILY)
                    .whitespace_normal(),
            )
            .child(
                div()
                    .id(("wallet-spend-auth-copy-action", row_index))
                    .flex_none()
                    .tooltip(move |window, cx| Tooltip::new(copy_tooltip.clone()).build(window, cx))
                    .child(clipboard_with_toast(
                        ("wallet-spend-auth-copy", row_index),
                        row.value.to_string(),
                    )),
            )
            .into_any_element();
    }

    if let Some(delta) = &row.delta {
        return div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_wrap()
            .items_baseline()
            .gap_x_2()
            .py(px(2.0))
            .child(
                app_text(row.value.to_string())
                    .min_w(px(0.0))
                    .whitespace_normal()
                    .text_color(rgb(theme::TEXT)),
            )
            .child(
                app_muted_text(delta.text.clone())
                    .min_w(px(0.0))
                    .whitespace_normal()
                    .when(delta.adverse, |this| this.text_color(cx.theme().danger)),
            )
            .into_any_element();
    }

    app_text(row.value.to_string())
        .w_full()
        .min_w(px(0.0))
        .py(px(2.0))
        .text_color(rgb(theme::TEXT))
        .whitespace_normal()
        .into_any_element()
}

pub(in crate::root) fn spend_authorization_recipient_display(value: &str) -> String {
    if value.chars().count() <= SUMMARY_RECIPIENT_SHORTEN_THRESHOLD_CHARS {
        return value.to_string();
    }
    let prefix: String = value.chars().take(SUMMARY_RECIPIENT_PREFIX_CHARS).collect();
    let suffix_chars: Vec<char> = value
        .chars()
        .rev()
        .take(SUMMARY_RECIPIENT_SUFFIX_CHARS)
        .collect();
    let suffix: String = suffix_chars.into_iter().rev().collect();
    format!("{prefix}...{suffix}")
}

fn new_spend_authorization_lifetime_select<T: 'static>(
    initial: SpendAuthorizationLifetime,
    window: &mut Window,
    cx: &mut Context<'_, T>,
) -> Entity<SpendAuthorizationLifetimeSelect> {
    let selected = SpendAuthorizationLifetime::ALL
        .iter()
        .position(|lifetime| *lifetime == initial)
        .map(IndexPath::new);
    cx.new(|cx| {
        SelectState::new(
            SearchableVec::new(SpendAuthorizationLifetime::ALL),
            selected,
            window,
            cx,
        )
    })
}

/// "Remember authorization" and its select on one row. The select drops under the label when
/// the row is too narrow for both.
fn render_spend_authorization_lifetime_row(
    select: &Entity<SpendAuthorizationLifetimeSelect>,
    disabled: bool,
) -> gpui::Div {
    let control = div()
        .flex_none()
        .w(SPEND_AUTHORIZATION_LIFETIME_SELECT_WIDTH)
        .max_w_full()
        .child(
            Select::new(select)
                .small()
                .w_full()
                .menu_width(SPEND_AUTHORIZATION_LIFETIME_SELECT_WIDTH)
                .disabled(disabled),
        );
    #[cfg(test)]
    let control = control.debug_selector(|| "wallet-spend-auth-lifetime-select".to_owned());
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_x_3()
        .gap_y_1()
        .child(app_muted_text("Remember authorization").whitespace_nowrap())
        .child(control)
}

impl WalletRoot {
    pub(super) fn request_spend_authorization(
        &mut self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let summary = if self.selected_wallet_source().is_hardware_derived()
            && intent.hardware_executor_action(self).is_some()
        {
            summary.requiring_explicit_review()
        } else {
            summary
        };
        let summary = if intent.gateway_execution().is_some()
            || matches!(
                &intent,
                SpendAuthorizationIntent::PublicSwap(..)
                    | SpendAuthorizationIntent::PublicSwapSource { .. }
            ) {
            summary.requiring_explicit_review()
        } else {
            summary
        };
        if self.selected_wallet_source().is_hardware_derived()
            && (intent.uses_private_wallet() || intent.hardware_executor_action(self).is_some())
        {
            intent.private_attention(
                "Approve on your hardware wallet",
                "Use the desktop app to approve this private spend on your device.",
            );
            self.clear_spend_authorization(cx);
            let key = match &intent {
                SpendAuthorizationIntent::PrepareExecutorUnshield(key, ..)
                | SpendAuthorizationIntent::ExecutorUnshield(key, ..) => Some(*key),
                _ => None,
            };
            if let Some(key) = key
                && let Some(draft) = self.unshield_spend_draft(key, cx)
                && draft.delivery_mode == DeliveryMode::SelfBroadcast
            {
                let Some(payer) = self.selected_self_broadcast_gas_payer_account(
                    draft.self_broadcast_public_account_uuid.as_deref(),
                ) else {
                    return;
                };
                if !matches!(
                    payer.source,
                    PublicAccountSource::Derived | PublicAccountSource::Imported
                ) {
                    self.set_unshield_form_error(key, "Select a software or imported gas payer, or a broadcaster, for this executor action.", cx);
                    return;
                }
                let payer = payer.public_account_uuid.clone();
                self.open_spend_authorization_dialog(
                    SpendAuthorizationIntent::ExecutorGasPassword {
                        intent: Box::new(intent),
                        summary: Box::new(summary.clone()),
                        payer,
                    },
                    summary.requiring_explicit_review(),
                    window,
                    cx,
                );
                return;
            }
            self.open_hardware_spend_authorization_dialog(
                HardwareSpendAuthorizationCompletion::Continue(intent),
                summary,
                window,
                cx,
            );
            return;
        }
        if spend_authorization_can_use_cached_password(&summary)
            && let Some(password) = self.valid_spend_authorization_password(cx)
        {
            match self.desktop_spend_authorization(password) {
                Ok(authorization) => {
                    if !intent.approve_gateway_review(self) {
                        return;
                    }
                    self.continue_authorized_spend(intent, authorization, window, cx);
                }
                Err(message) => self.set_vault_error(message, cx),
            }
            return;
        }

        intent.private_attention(
            "Authorize in the desktop app",
            "Enter your vault password in the desktop app to continue.",
        );
        self.open_spend_authorization_dialog(intent, summary, window, cx);
    }

    fn open_spend_authorization_dialog(
        &self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_spend_authorization_dialog_with_review(intent, summary, None, window, cx);
    }

    pub(in crate::root) fn open_prepared_spend_review(
        &self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        intent.private_attention(
            "Review in the desktop app",
            "Review the updated transaction terms before signing.",
        );
        self.open_spend_authorization_dialog_with_review(
            intent,
            summary,
            Some((self.current_spend_authorization_scope(), authorization)),
            window,
            cx,
        );
    }

    fn open_spend_authorization_dialog_with_review(
        &self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        review_authorization: Option<(SpendAuthorizationScope, DesktopPrivateSpendAuthorization)>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<SpendAuthorizationDialogContent> {
        let root = cx.entity();
        let initial_lifetime = self.spend_authorization_lifetime;
        let dialog_title = summary.title.to_string();
        let title_network = summary.title_network;
        let device_auth = self.device_auth_prompt_cached();
        let lease = Rc::new(Cell::new(true));
        let identity = Rc::downgrade(&lease);
        let content = cx.new(|cx| {
            let mut content = SpendAuthorizationDialogContent::new(
                root,
                intent,
                summary,
                initial_lifetime,
                identity,
                window,
                cx,
            );
            content.review_authorization = review_authorization;
            content.device_auth = device_auth;
            content
        });
        let focus_content = content.clone();
        let dialog_content = content.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let dialog_width =
                (window.viewport_size().width * 0.92).min(SPEND_AUTHORIZATION_DIALOG_WIDTH);
            let content_width = secondary_dialog_content_width(dialog_width);
            let close_content = dialog_content.clone();
            let identity = Rc::downgrade(&lease);
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height(window))
                .title(spend_authorization_title(&dialog_title, title_network))
                .on_close(move |_event, _window, cx| {
                    if let Some(open) = identity.upgrade() {
                        open.set(false);
                    }
                    close_content.update(cx, SpendAuthorizationDialogContent::cancel);
                })
                .child(div().w(content_width).child(dialog_content.clone()))
                // The footer stays in view while the content scrolls.
                .footer(
                    dialog_content
                        .read(cx)
                        .render_footer(&dialog_content)
                        .w(content_width),
                )
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_content.update(cx, |content, cx| content.focus_password(window, cx));
        });
        content
    }

    pub(super) fn open_hardware_public_action_authorization_dialog(
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = cx.entity();
        let payload_disclosure = summary
            .payload
            .clone()
            .map(|payload| cx.new(|_cx| SpendAuthorizationPayloadDisclosure::new(payload)));
        let row_group_disclosure = summary.row_group.clone().map(|group| {
            cx.new(|_cx| {
                SpendAuthorizationDetailsDisclosure::new(
                    SPEND_AUTHORIZATION_ROW_GROUP_TOGGLE,
                    group,
                )
            })
        });
        let details_disclosure = summary.details.clone().map(|details| {
            cx.new(|_cx| {
                SpendAuthorizationDetailsDisclosure::new(
                    SPEND_AUTHORIZATION_DETAILS_TOGGLE,
                    details,
                )
            })
        });
        let handed_off = Rc::new(Cell::new(false));
        window.open_dialog(cx, move |dialog, window, cx| {
            let dialog_width =
                (window.viewport_size().width * 0.92).min(SPEND_AUTHORIZATION_DIALOG_WIDTH);
            let content_width = secondary_dialog_content_width(dialog_width);
            let close_root = root.clone();
            let submit_root = root.clone();
            let close_intent = intent.clone();
            let show_trezor_app_passphrase = root
                .read(cx)
                .current_session_needs_trezor_app_passphrase();
            #[cfg(feature = "hardware")]
            let trezor_app_passphrase_input = root.read(cx).trezor_app_passphrase_input.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height(window))
                .title(app_strong_text("Authorize hardware public action"))
                .footer(dialog_footer("Approve on device", true))
                .on_close({
                    let handed_off = handed_off.clone();
                    move |_event, window, cx| {
                        close_root.update(cx, |root, cx| {
                            root.clear_trezor_app_passphrase_input(window, cx);
                            if !handed_off.get() {
                                root.cancel_spend_authorization(&close_intent, cx);
                            }
                        });
                    }
                })
                .on_ok({
                    let intent = intent.clone();
                    let handed_off = handed_off.clone();
                    move |_event, window, cx| {
                        let intent = intent.clone();
                        if !intent.approve_gateway_review(submit_root.read(cx)) { return true; }
                        handed_off.set(true);
                        submit_root.update(cx, |root, cx| {
                            if let SpendAuthorizationIntent::PublicSwapSource {
                                view,
                                command,
                                private_authorization,
                            } = intent
                            {
                                let Some(private_authorization) = private_authorization.borrow_mut().take() else {
                                    return;
                                };
                                // Closing this dialog clears the passphrase input before the deferred handoff.
                                #[cfg(feature = "hardware")]
                                let (trezor_app_passphrase, trezor_pin_matrix_provider) = {
                                    let session = root.view_session.as_ref()
                                        .and_then(|view| view.hardware_profile_session()).cloned();
                                    let passphrase = session.as_ref().and_then(|session| {
                                        root.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
                                    });
                                    let pin = session.as_ref()
                                        .filter(|session| session.device_kind == HardwareDeviceKind::Trezor)
                                        .map(|_| root.trezor_pin_matrix_provider_for_operation(window, cx));
                                    (passphrase, pin)
                                };
                                #[cfg(not(feature = "hardware"))]
                                let (trezor_app_passphrase, trezor_pin_matrix_provider) = (None, None);
                                window.defer(cx, move |window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.continue_authorized_public_swap(
                                            &command,
                                            private_authorization,
                                            Some(DesktopPrivateSpendAuthorization::HardwarePublic),
                                            trezor_app_passphrase,
                                            trezor_pin_matrix_provider,
                                            window,
                                            cx,
                                        );
                                    });
                                });
                            } else {
                                root.continue_authorized_spend(
                                    intent,
                                    DesktopPrivateSpendAuthorization::HardwarePublic,
                                    window,
                                    cx,
                                );
                            }
                        });
                        true
                    }
                })
                .child(div()
                    .w(content_width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(spend_authorization_title(&summary.title, summary.title_network))
                    .children(summary.steps.as_ref().map(render_spend_authorization_steps))
                    .child(app_muted_text(summary.detail.to_string()).whitespace_normal())
                    .child(render_spend_authorization_summary(
                        &summary,
                        row_group_disclosure.clone().map(IntoElement::into_any_element),
                        details_disclosure.clone().map(IntoElement::into_any_element),
                        cx,
                    ))
                    .children(summary.warnings.iter().enumerate().map(|(index, warning)| {
                        Alert::warning(
                            SharedString::from(format!(
                                "wallet-hardware-public-action-warning-{index}"
                            )),
                            warning.to_string(),
                        )
                        .small()
                    }))
                    .children(payload_disclosure.clone())
                    .when(show_trezor_app_passphrase, |this| {
                        #[cfg(feature = "hardware")]
                        {
                            this.child(
                                div()
                                    .w_full()
                                    .p(px(12.0))
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(theme::BORDER))
                                    .bg(rgb(theme::SURFACE))
                                    .child(app_strong_text("Trezor app passphrase"))
                                    .child(
                                        app_muted_text(
                                            "If the Trezor session expired, enter the app passphrase for this public account request.",
                                        )
                                        .whitespace_normal(),
                                    )
                                    .child(app_masked_input(&trezor_app_passphrase_input, false)),
                            )
                        }
                        #[cfg(not(feature = "hardware"))]
                        {
                            this
                        }
                    })
                    .child(
                        app_muted_text("The app will verify the stored public account address against the connected device before signing.")
                            .whitespace_normal(),
                    ))
        });
    }

    fn hardware_gas_payment_review(
        &mut self,
        completion: &HardwareSpendAuthorizationCompletion,
        cx: &mut Context<'_, Self>,
    ) -> Option<HardwareGasPaymentReview> {
        let intent = completion.private_intent()?;
        if !matches!(
            intent.hardware_executor_action(self),
            Some(wallet_ops::HardwareExecutorAction::GasPayment { .. })
        ) {
            return None;
        }
        match intent {
            SpendAuthorizationIntent::PrivateSend(key, ..) => {
                let draft = self.send_spend_draft(key, cx)?;
                Some(HardwareGasPaymentReview {
                    form: self.send_forms.get(&key)?.recipient_input.entity_id(),
                    recipient: draft.recipient,
                    amount: draft.amount,
                    payer: draft.self_broadcast_public_account_uuid,
                    funding: draft.self_broadcast_funding,
                    gas_fee: draft.self_broadcast_gas_fee,
                    incentive: draft.sponsored_incentive,
                    fee_mode: draft.fee_mode,
                    unwrap: false,
                    top_up: None,
                })
            }
            SpendAuthorizationIntent::PrivateUnshield(key, ..) => {
                let draft = self.unshield_spend_draft(key, cx)?;
                Some(HardwareGasPaymentReview {
                    form: self.unshield_forms.get(&key)?.recipient_input.entity_id(),
                    recipient: draft.recipient.to_string(),
                    amount: draft.amount,
                    payer: draft.self_broadcast_public_account_uuid,
                    funding: draft.self_broadcast_funding,
                    gas_fee: draft.self_broadcast_gas_fee,
                    incentive: draft.sponsored_incentive,
                    fee_mode: draft.fee_mode,
                    unwrap: draft.unwrap,
                    top_up: draft.native_top_up,
                })
            }
            _ => None,
        }
    }

    pub(super) fn open_hardware_spend_authorization_dialog(
        &mut self,
        completion: HardwareSpendAuthorizationCompletion,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(descriptor) = self.selected_hardware_descriptor() else {
            self.set_vault_error(
                "Selected wallet is missing its hardware derivation descriptor",
                cx,
            );
            return;
        };
        let root = cx.entity();
        let device_label = hardware_device_label(descriptor.device_kind);
        let gas_review = self.hardware_gas_payment_review(&completion, cx);
        let dialog_title = summary.title.to_string();
        let title_network = summary.title_network;
        let content = cx.new(|_cx| {
            HardwareSpendAuthorizationDialogContent::new(
                root.clone(),
                completion,
                gas_review,
                summary,
                device_label,
            )
        });
        window.open_dialog(cx, move |dialog, window, cx| {
            let dialog_width =
                (window.viewport_size().width * 0.92).min(SPEND_AUTHORIZATION_DIALOG_WIDTH);
            let content_width = secondary_dialog_content_width(dialog_width);
            let close_content = content.clone();
            let close_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height(window))
                .title(spend_authorization_title(&dialog_title, title_network))
                .on_ok({
                    let content = content.clone();
                    move |_event, window, cx| {
                        content.update(cx, |content, cx| content.start(window, cx));
                        false
                    }
                })
                .on_close(move |_event, window, cx| {
                    close_content.update(cx, HardwareSpendAuthorizationDialogContent::cancel);
                    close_root.update(cx, |root, cx| {
                        root.clear_trezor_app_passphrase_input(window, cx);
                        root.clear_trezor_pin_matrix_prompt(cx);
                    });
                })
                .child(div().w(content_width).child(content.clone()))
                // The footer stays in view while the content scrolls.
                .footer(content.read(cx).render_footer(&content).w(content_width))
        });
    }

    fn selected_hardware_descriptor(&self) -> Option<HardwareDerivationDescriptor> {
        let selected_wallet_id = self.selected_wallet_id.as_ref()?;
        self.wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == selected_wallet_id.as_ref())
            .and_then(|metadata| metadata.hardware_derivation_descriptor().cloned())
    }

    #[cfg(feature = "hardware")]
    fn start_hardware_spend_authorization_task(
        &mut self,
        completion: &HardwareSpendAuthorizationCompletion,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<tokio::task::JoinHandle<HardwareSpendAuthorizationTaskOutput>, Arc<str>> {
        if let Some(intent @ SpendAuthorizationIntent::PublicSwap(..)) = completion.private_intent()
            && !intent.private_review_current(self)
        {
            return Err(Arc::from(
                "The swap source or destination changed. Close this dialog and review the swap again.",
            ));
        }
        let Some(descriptor) = self.selected_hardware_descriptor() else {
            return Err(Arc::from(
                "Selected wallet is missing its hardware derivation descriptor",
            ));
        };
        let Some(store) = self.vault_store.clone() else {
            return Err(Arc::from("Wallet vault storage is unavailable"));
        };
        let Some(view_session) = self.view_session.clone() else {
            return Err(Arc::from(
                "Unlock the wallet vault before authorizing a spend",
            ));
        };
        let Some(hardware_session) = view_session.hardware_profile_session().cloned() else {
            return Err(Arc::from(
                "Unlock the matching hardware profile before authorizing a spend",
            ));
        };
        let executor_action = match completion.private_intent() {
            Some(SpendAuthorizationIntent::WalletConnectRequest {
                request_key,
                review_token,
                ..
            }) => {
                let action = self
                    .walletconnect_hardware_executor_action(&request_key, review_token)
                    .ok_or_else(|| {
                        Arc::<str>::from(
                            "The request changed or its chain is unavailable. Review it again.",
                        )
                    })?;
                Some(action)
            }
            Some(intent) => {
                // A swap's setup sent again on its destination network is approved by that
                // network's owner.
                let chain_id = match &intent {
                    SpendAuthorizationIntent::PrivateSwap(_, command) => {
                        command.hardware_executor_chain()
                    }
                    SpendAuthorizationIntent::PublicSwap(_, command) => {
                        command.hardware_executor_chain()
                    }
                    _ => self.selected_chain,
                };
                intent
                    .hardware_executor_action(self)
                    .map(|action| (chain_id, action))
            }
            None => None,
        };
        let executor_request = executor_action
            .map(|(chain_id, action)| {
                self.executor_owner_for_public_chain(chain_id)
                    .ok_or_else(|| {
                        Arc::<str>::from(
                            "Open the wallet and chain before authorizing this account",
                        )
                    })?
                    .hardware_authorization_request(Arc::clone(&view_session), action)
                    .map_err(|error| Arc::<str>::from(error.to_string()))
            })
            .transpose()?;
        // A private Bridge swap also signs for its stealth account on the destination network,
        // approved in the same device session.
        let destination_request = match completion.private_intent() {
            Some(SpendAuthorizationIntent::PrivateSwap(_, command)) => command
                .hardware_destination_action()
                .map(|(chain_id, action)| {
                    self.executor_owner_for_public_chain(chain_id)
                        .ok_or_else(|| {
                            Arc::<str>::from(
                                "Wait for the swap's destination network to load before authorizing this swap",
                            )
                        })?
                        .hardware_authorization_request(Arc::clone(&view_session), action)
                        .map_err(|error| Arc::<str>::from(error.to_string()))
                })
                .transpose()?,
            _ => None,
        };
        let gas_payer = if let HardwareSpendAuthorizationCompletion::ExecutorWithGasPayer {
            payer,
            password,
            seed_session,
            ..
        } = completion
        {
            Some((payer.clone(), password.clone(), seed_session.clone()))
        } else {
            None
        };
        let trezor_app_passphrase =
            self.read_trezor_app_passphrase_for_hardware_session(&hardware_session, window, cx);
        let trezor_pin_matrix_provider =
            if hardware_session.device_kind == HardwareDeviceKind::Trezor {
                Some(self.trezor_pin_matrix_provider_for_operation(window, cx))
            } else {
                None
            };
        Ok(self.runtime.spawn(async move {
            let executor_request = if let Some((payer, password, seed_session)) = gas_payer {
                let request = executor_request.ok_or_else(|| {
                    HardwareSpendAuthorizationError::Executor(
                        "Executor approval is unavailable".into(),
                    )
                })?;
                Some(
                    tokio::task::spawn_blocking(move || {
                        request.with_gas_payer(payer, password, seed_session)
                    })
                    .await
                    .map_err(|_| {
                        HardwareSpendAuthorizationError::Executor(
                            "Gas-payer authorization task failed. Try again.".into(),
                        )
                    })?
                    .map_err(|error| {
                        HardwareSpendAuthorizationError::Executor(error.to_string())
                    })?,
                )
            } else {
                executor_request
            };
            derive_hardware_spend_authorization(
                store,
                view_session,
                hardware_session,
                descriptor,
                trezor_app_passphrase,
                trezor_pin_matrix_provider,
                executor_request.map(|request| (request, destination_request)),
            )
            .await
        }))
    }

    fn valid_spend_authorization_password(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Option<Zeroizing<String>> {
        let now = Instant::now();
        let scope = self.current_spend_authorization_scope();
        if self
            .spend_authorization_cache
            .as_ref()
            .is_some_and(|authorization| authorization.is_valid_at(&scope, now))
        {
            return self
                .spend_authorization_cache
                .as_ref()
                .map(|authorization| authorization.password.clone());
        }
        if self.spend_authorization_cache.take().is_some() {
            cx.notify();
        }
        None
    }

    fn cancel_spend_authorization(
        &mut self,
        intent: &SpendAuthorizationIntent,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(execution) = intent.gateway_execution() {
            execution.cancel_review();
            self.release_gateway_private_form(execution, cx);
        }
        self.cancel_governance_authorization(intent, cx);
        if let SpendAuthorizationIntent::StealthAccounts(view, command) = intent {
            let view = view.clone();
            let command = command.clone();
            cx.defer(move |cx| {
                view.update(cx, |view, cx| view.cancel_authorization(&command, cx));
            });
        }
        if let SpendAuthorizationIntent::PrivateSwap(view, command) = intent {
            let view = view.clone();
            let command = command.clone();
            cx.defer(move |cx| {
                view.update(cx, |view, cx| view.cancel_authorization(&command, cx));
            });
        }
        if let SpendAuthorizationIntent::PublicSwap(view, command)
        | SpendAuthorizationIntent::PublicSwapSource { view, command, .. } = intent
        {
            if let SpendAuthorizationIntent::PublicSwapSource {
                private_authorization,
                ..
            } = intent
            {
                private_authorization.borrow_mut().take();
            }
            let view = view.clone();
            let command = command.clone();
            cx.defer(move |cx| {
                view.update(cx, |view, cx| {
                    view.cancel_public_authorization(&command, cx);
                });
            });
        }
    }

    pub(super) fn finish_spend_authorization(
        &mut self,
        intent: SpendAuthorizationIntent,
        password: Zeroizing<String>,
        lifetime: SpendAuthorizationLifetime,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), Arc<str>> {
        let authorization = self.desktop_spend_authorization(password.clone())?;
        if !intent.approve_gateway_review(self) {
            if matches!(
                &intent,
                SpendAuthorizationIntent::PublicSwap(..)
                    | SpendAuthorizationIntent::PublicSwapSource { .. }
            ) {
                self.cancel_spend_authorization(&intent, cx);
            }
            window.close_dialog(cx);
            return Ok(());
        }
        self.spend_authorization_lifetime = lifetime;
        self.spend_authorization_cache = SpendAuthorizationCache::new(
            password,
            lifetime,
            self.current_spend_authorization_scope(),
            Instant::now(),
        );
        window.close_dialog(cx);
        self.continue_authorized_spend(intent, authorization, window, cx);
        Ok(())
    }

    pub(super) fn clear_spend_authorization(&mut self, cx: &mut Context<'_, Self>) {
        if self.spend_authorization_cache.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn clear_protected_software_seed_session(&mut self, cx: &mut Context<'_, Self>) {
        if clear_protected_software_seed_session_state(
            &mut self.protected_software_seed_session,
            &mut self.spend_authorization_cache,
        ) {
            cx.notify();
        }
    }

    fn current_spend_authorization_scope(&self) -> SpendAuthorizationScope {
        let wallet_uuid = self.selected_wallet_id.as_deref().unwrap_or("");
        let base_profile_uuid = self
            .wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_uuid)
            .and_then(|metadata| metadata.software_context.as_ref())
            .map_or(wallet_uuid, |context| context.base_profile_uuid.as_str());
        SpendAuthorizationScope::new(
            base_profile_uuid,
            wallet_uuid,
            self.protected_software_seed_session
                .as_ref()
                .map(|session| session.binding().clone()),
        )
    }

    fn desktop_spend_authorization(
        &self,
        password: Zeroizing<String>,
    ) -> Result<DesktopPrivateSpendAuthorization, Arc<str>> {
        let wallet_uuid = self
            .selected_wallet_id
            .as_deref()
            .ok_or_else(|| Arc::from("Select a wallet before authorizing a spend"))?;
        let metadata = self
            .wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_uuid)
            .ok_or_else(|| Arc::from("Selected wallet metadata is unavailable"))?;
        let Some(context) = metadata.software_context.as_ref() else {
            return Ok(DesktopPrivateSpendAuthorization::VaultPassword(password));
        };
        if context.kind != WalletSoftwareContextKind::Passphrase {
            return Ok(DesktopPrivateSpendAuthorization::VaultPassword(password));
        }
        let session = self
            .protected_software_seed_session
            .as_ref()
            .ok_or_else(|| {
                Arc::from("Open the selected passphrase wallet again before spending")
            })?;
        if session.binding().base_profile_uuid() != context.base_profile_uuid
            || session.binding().context_wallet_uuid() != wallet_uuid
        {
            return Err(Arc::from(
                "The selected passphrase wallet session is stale; open it again before spending",
            ));
        }
        Ok(DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed {
            password,
            session: Arc::clone(session),
        })
    }

    pub(super) fn continue_authorized_spend(
        &mut self,
        intent: SpendAuthorizationIntent,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.continue_authorized_spend_with_destination(intent, authorization, None, window, cx);
    }

    /// [`Self::continue_authorized_spend`] with `destination`, the authorization a hardware
    /// wallet's device session also gave for a private Bridge swap's destination network. Only
    /// a private swap takes one.
    fn continue_authorized_spend_with_destination(
        &mut self,
        intent: SpendAuthorizationIntent,
        authorization: DesktopPrivateSpendAuthorization,
        destination: Option<DesktopPrivateSpendAuthorization>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !intent.private_review_current(self) {
            if matches!(
                &intent,
                SpendAuthorizationIntent::PublicSwap(..)
                    | SpendAuthorizationIntent::PublicSwapSource { .. }
            ) {
                self.cancel_spend_authorization(&intent, cx);
            }
            return;
        }
        match intent {
            SpendAuthorizationIntent::ExecutorGasPassword {
                intent,
                summary,
                payer,
            } => {
                let seed_session = authorization.protected_seed_session();
                let (DesktopPrivateSpendAuthorization::VaultPassword(password)
                | DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed { password, .. }) =
                    authorization
                else {
                    self.set_vault_error(
                        "Authorize the selected software gas payer with its vault password",
                        cx,
                    );
                    return;
                };
                self.clear_spend_authorization(cx);
                self.open_hardware_spend_authorization_dialog(
                    HardwareSpendAuthorizationCompletion::ExecutorWithGasPayer {
                        intent: *intent,
                        payer,
                        password,
                        seed_session,
                    },
                    *summary,
                    window,
                    cx,
                );
            }
            SpendAuthorizationIntent::StealthAccounts(view, command) => {
                // The panel reads WalletRoot to validate its session. Release this update first.
                window.defer(cx, move |window, cx| {
                    view.update(cx, |view, cx| {
                        view.continue_authorized(command, authorization, window, cx);
                    });
                });
            }
            SpendAuthorizationIntent::PrivateSwap(view, command) => {
                // The swap view reads WalletRoot to validate its session. Release this update first.
                window.defer(cx, move |window, cx| {
                    view.update(cx, |view, cx| {
                        view.continue_authorized(&command, authorization, destination, window, cx);
                    });
                });
            }
            SpendAuthorizationIntent::PublicSwap(view, command) => {
                let public_authorization = match command.source().source {
                    PublicAccountSource::HardwareDerived => None,
                    PublicAccountSource::Derived | PublicAccountSource::Imported => {
                        match &authorization {
                            DesktopPrivateSpendAuthorization::VaultPassword(password) => Some(
                                DesktopPrivateSpendAuthorization::VaultPassword(password.clone()),
                            ),
                            DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed {
                                password,
                                session,
                            } => Some(DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed {
                                password: password.clone(),
                                session: Arc::clone(session),
                            }),
                            _ => None,
                        }
                    }
                    PublicAccountSource::ExecutorDerived(_) => {
                        self.cancel_spend_authorization(
                            &SpendAuthorizationIntent::PublicSwap(view, command),
                            cx,
                        );
                        return;
                    }
                };
                if let Some(public_authorization) = public_authorization {
                    window.defer(cx, move |window, cx| {
                        view.update(cx, |view, cx| {
                            view.continue_authorized_public_swap(
                                &command,
                                authorization,
                                Some(public_authorization),
                                None,
                                None,
                                window,
                                cx,
                            );
                        });
                    });
                } else {
                    let hardware_public =
                        command.source().source == PublicAccountSource::HardwareDerived;
                    // The review was approved for the wallet's device, which also holds this
                    // account. A second dialog would repeat it and prompt nothing: the device
                    // asks when it signs. Only an app passphrase still has to be entered.
                    if hardware_public && !self.current_session_needs_trezor_app_passphrase() {
                        #[cfg(feature = "hardware")]
                        let trezor_pin_matrix_provider = self
                            .view_session
                            .as_ref()
                            .and_then(|view| view.hardware_profile_session())
                            .filter(|session| session.device_kind == HardwareDeviceKind::Trezor)
                            .cloned()
                            .map(|_| self.trezor_pin_matrix_provider_for_operation(window, cx));
                        #[cfg(not(feature = "hardware"))]
                        let trezor_pin_matrix_provider = None;
                        window.defer(cx, move |window, cx| {
                            view.update(cx, |view, cx| {
                                view.continue_authorized_public_swap(
                                    &command,
                                    authorization,
                                    Some(DesktopPrivateSpendAuthorization::HardwarePublic),
                                    None,
                                    trezor_pin_matrix_provider,
                                    window,
                                    cx,
                                );
                            });
                        });
                        return;
                    }
                    let summary = command.public_authorization_summary();
                    let intent = SpendAuthorizationIntent::PublicSwapSource {
                        view,
                        command,
                        private_authorization: Rc::new(RefCell::new(Some(authorization))),
                    };
                    if hardware_public {
                        Self::open_hardware_public_action_authorization_dialog(
                            intent, summary, window, cx,
                        );
                    } else {
                        self.request_spend_authorization(intent, summary, window, cx);
                    }
                }
            }
            SpendAuthorizationIntent::PublicSwapSource {
                view,
                command,
                private_authorization,
            } => {
                let Some(private_authorization) = private_authorization.borrow_mut().take() else {
                    return;
                };
                window.defer(cx, move |window, cx| {
                    view.update(cx, |view, cx| {
                        view.continue_authorized_public_swap(
                            &command,
                            private_authorization,
                            Some(authorization),
                            None,
                            None,
                            window,
                            cx,
                        );
                    });
                });
            }
            SpendAuthorizationIntent::PrepareExecutorUnshield(key, approval, execution) => {
                self.prepare_executor_unshield_review(key, approval, authorization, window, cx);
                if !self
                    .unshield_forms
                    .get(&key)
                    .is_some_and(|form| form.generating)
                    && let Some(execution) = execution
                {
                    self.reject_gateway_private_authorization(&execution, cx);
                }
            }
            SpendAuthorizationIntent::ExecutorUnshield(key, review, execution) => {
                let current = self.unshield_spend_draft(key, cx);
                if current.as_ref().is_some_and(|draft| review.matches(draft)) {
                    self.generate_unshield_calldata_authorized(
                        key,
                        authorization,
                        None,
                        window,
                        cx,
                    );
                } else {
                    self.set_unshield_form_error(
                        key,
                        "The prepared action changed. Refresh and review it again.",
                        cx,
                    );
                }
                if let Some(execution) = execution {
                    self.reject_gateway_private_authorization(&execution, cx);
                }
            }
            SpendAuthorizationIntent::PrivateSend(key, authorization_limit, execution, _) => {
                self.generate_send_calldata_authorized(
                    key,
                    authorization,
                    authorization_limit,
                    window,
                    cx,
                );
                if let Some(execution) = execution {
                    self.reject_gateway_private_authorization(&execution, cx);
                }
            }
            SpendAuthorizationIntent::PrivateSendSelfBroadcastGasPassword(
                key,
                authorization_limit,
                execution,
            ) => {
                let DesktopPrivateSpendAuthorization::VaultPassword(password) = authorization
                else {
                    self.set_vault_error(
                        "Self-broadcast software gas-payer authorization requires the vault password",
                        cx,
                    );
                    return;
                };
                self.request_private_send_hardware_authorization_with_gas_password(
                    key,
                    password,
                    authorization_limit,
                    execution,
                    window,
                    cx,
                );
            }
            SpendAuthorizationIntent::PrivateUnshield(key, authorization_limit, execution, _) => {
                self.generate_unshield_calldata_authorized(
                    key,
                    authorization,
                    authorization_limit,
                    window,
                    cx,
                );
                if let Some(execution) = execution {
                    self.reject_gateway_private_authorization(&execution, cx);
                }
            }
            SpendAuthorizationIntent::PrivateUnshieldSelfBroadcastGasPassword(
                key,
                authorization_limit,
                execution,
            ) => {
                let DesktopPrivateSpendAuthorization::VaultPassword(password) = authorization
                else {
                    self.set_vault_error(
                        "Self-broadcast software gas-payer authorization requires the vault password",
                        cx,
                    );
                    return;
                };
                self.request_private_unshield_hardware_authorization_with_gas_password(
                    key,
                    password,
                    authorization_limit,
                    execution,
                    window,
                    cx,
                );
            }
            SpendAuthorizationIntent::BlockedShieldRefund(utxo_id) => {
                self.submit_blocked_shield_refund_authorized(
                    utxo_id,
                    authorization,
                    None,
                    window,
                    cx,
                );
            }
            SpendAuthorizationIntent::BlockedShieldRefundGasPassword(utxo_id) => {
                let DesktopPrivateSpendAuthorization::VaultPassword(password) = authorization
                else {
                    self.set_vault_error(
                        "Blocked Shield refund gas-payer authorization requires the vault password",
                        cx,
                    );
                    return;
                };
                self.request_blocked_shield_refund_hardware_authorization(
                    utxo_id, password, window, cx,
                );
            }
            SpendAuthorizationIntent::PublicSend(draft) => {
                self.submit_public_send_authorized(*draft, authorization, window, cx);
            }
            SpendAuthorizationIntent::PublicShield(draft) => {
                self.submit_public_shield_authorized(*draft, authorization, window, cx);
            }
            SpendAuthorizationIntent::Governance(draft) => {
                self.revalidate_governance_authorized(&draft, authorization, window, cx);
            }
            SpendAuthorizationIntent::WalletConnectRequest {
                request_key,
                review_token,
                reviewed_fee,
            } => {
                self.submit_walletconnect_request_authorized(
                    &request_key,
                    review_token,
                    reviewed_fee,
                    authorization,
                    window,
                    cx,
                );
            }
        }
    }

    #[cfg(feature = "hardware")]
    pub(super) fn refresh_active_hardware_profile_session(
        &mut self,
        hardware_session: HardwareProfileSession,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(view_session) = self.view_session.as_ref() else {
            return;
        };
        if view_session.hardware_profile_session().is_none() {
            return;
        }
        let refreshed =
            Arc::new(view_session.clone_with_hardware_profile_session(hardware_session));
        self.gateway.drafts.borrow_mut().refresh_hardware_session(
            view_session,
            &refreshed,
            self.active_wallet_generation,
        );
        self.view_session = Some(refreshed);
        cx.notify();
    }

    fn request_private_send_hardware_authorization_with_gas_password(
        &mut self,
        key: UnshieldAssetKey,
        vault_password: Zeroizing<String>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(mut draft) = self.send_spend_draft(key, cx) else {
            if let Some(execution) = execution {
                self.reject_gateway_private_authorization(&execution, cx);
            }
            return;
        };
        draft.sponsored_authorization_limit = authorization_limit;
        self.open_hardware_spend_authorization_dialog(
            HardwareSpendAuthorizationCompletion::PrivateSendSelfBroadcast {
                key,
                vault_password,
                authorization_limit,
                execution,
            },
            super::private_action::private_send_authorization_summary(&draft),
            window,
            cx,
        );
    }

    fn request_private_unshield_hardware_authorization_with_gas_password(
        &mut self,
        key: UnshieldAssetKey,
        vault_password: Zeroizing<String>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(mut draft) = self.unshield_spend_draft(key, cx) else {
            if let Some(execution) = execution {
                self.reject_gateway_private_authorization(&execution, cx);
            }
            return;
        };
        draft.sponsored_authorization_limit = authorization_limit;
        self.open_hardware_spend_authorization_dialog(
            HardwareSpendAuthorizationCompletion::PrivateUnshieldSelfBroadcast {
                key,
                vault_password,
                authorization_limit,
                execution,
            },
            super::private_action::private_unshield_authorization_summary(&draft),
            window,
            cx,
        );
    }
}

pub(in crate::root) fn hardware_spend_authorization_instruction(device_label: &str) -> String {
    format!("Approve the Railgun derivation request on your {device_label}.")
}

#[cfg(feature = "hardware")]
fn hardware_spend_authorization_error_message(error: &HardwareSpendAuthorizationError) -> String {
    match error {
        HardwareSpendAuthorizationError::Hardware(error) => {
            format!("Hardware spend authorization failed: {error}")
        }
        HardwareSpendAuthorizationError::Vault(error) => format!("Vault error: {error}"),
        HardwareSpendAuthorizationError::Executor(error) => error.clone(),
    }
}

#[cfg(feature = "hardware")]
async fn derive_hardware_spend_authorization(
    store: Arc<DesktopVaultStore>,
    view_session: Arc<DesktopViewSession>,
    mut hardware_session: HardwareProfileSession,
    descriptor: HardwareDerivationDescriptor,
    trezor_app_passphrase: Option<Zeroizing<String>>,
    trezor_pin_matrix_provider: Option<TrezorPinMatrixProvider>,
    executor_requests: Option<(
        wallet_ops::HardwareExecutorAuthorizationRequest,
        Option<wallet_ops::HardwareExecutorAuthorizationRequest>,
    )>,
) -> HardwareSpendAuthorizationTaskOutput {
    if let Some((request, destination)) = &executor_requests {
        for request in std::iter::once(request).chain(destination) {
            request
                .ensure_active()
                .map_err(|error| HardwareSpendAuthorizationError::Executor(error.to_string()))?;
        }
    }
    hardware_session.verify_descriptor(&descriptor)?;
    let entropy = match descriptor.device_kind {
        HardwareDeviceKind::Ledger => {
            let client = LedgerHardwareDerivationClient::connect().await?;
            let active = client.active_profile_session(&descriptor.path).await?;
            active.verify_descriptor(&descriptor)?;
            let output = client.eip1024_shared_secret(&descriptor.path, true).await?;
            synthetic_entropy_from_hardware_output(&descriptor, output)?
        }
        HardwareDeviceKind::Trezor => {
            let mut client = TrezorHardwareDerivationClient::connect_with_session(
                hardware_session.trezor_session_id.clone(),
            )?;
            client.set_passphrase_mode(hardware_session.trezor_passphrase_mode());
            if let Some(passphrase) = trezor_app_passphrase {
                client.set_app_passphrase_zeroizing(passphrase);
            }
            if let Some(provider) = trezor_pin_matrix_provider {
                client.set_pin_matrix_provider(provider);
            }
            let active = client.active_profile_session(&descriptor.path)?;
            active.verify_descriptor(&descriptor)?;
            hardware_session
                .trezor_session_id
                .clone_from(&active.trezor_session_id);
            hardware_session.set_trezor_passphrase_mode(active.trezor_passphrase_mode());
            let output = client.cipher_key_value(&descriptor)?;
            synthetic_entropy_from_hardware_output(&descriptor, output)?
        }
    };
    if let Some((request, destination)) = executor_requests {
        // A private Bridge swap's two requests are completed from this one device response.
        let (authorization, destination) = match destination {
            Some(destination) => {
                let (authorization, destination) = request
                    .complete_with_destination(destination, &descriptor, entropy.expose_secret())
                    .map_err(|error| {
                        HardwareSpendAuthorizationError::Executor(error.to_string())
                    })?;
                (
                    authorization,
                    Some(DesktopPrivateSpendAuthorization::HardwareExecutor(
                        Box::new(destination),
                    )),
                )
            }
            None => (
                request
                    .complete(&descriptor, entropy.expose_secret())
                    .map_err(|error| {
                        HardwareSpendAuthorizationError::Executor(error.to_string())
                    })?,
                None,
            ),
        };
        return Ok((
            DesktopPrivateSpendAuthorization::HardwareExecutor(Box::new(authorization)),
            destination,
            hardware_session,
        ));
    }
    let signer = store.hardware_railgun_spend_signer_from_entropy(
        view_session.as_ref(),
        &descriptor,
        entropy.expose_secret(),
    )?;
    Ok((
        DesktopPrivateSpendAuthorization::PreauthorizedSigner(signer),
        None,
        hardware_session,
    ))
}

pub(super) fn is_spend_authorization_failure_error(error: &str) -> bool {
    error.contains("authorize ") && error.ends_with("unlock failed")
}

#[cfg(test)]
pub(super) fn remembered_spend_authorization_valid_for_test(
    lifetime: SpendAuthorizationLifetime,
    elapsed: Duration,
) -> bool {
    let now = Instant::now();
    let scope = SpendAuthorizationScope::new("base", "wallet", None);
    let Some(cache) = SpendAuthorizationCache::new(
        Zeroizing::new("password".to_string()),
        lifetime,
        scope.clone(),
        now,
    ) else {
        return false;
    };
    cache.is_valid_at(&scope, now + elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(feature = "hardware"))]
    use ui::controls::app_masked_input;

    struct DialogWindow;

    impl gpui::Render for DialogWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            div()
                .size_full()
                .children(crate::root::startup::render_wallet_overlay_layers(
                    window, cx,
                ))
        }
    }

    #[gpui::test]
    fn spend_device_auth_is_inside_the_password_field_and_activates_from_the_keyboard(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui_kit::test::TestWindowExt as _;
        use wallet_ops::vault::DeviceAuthStatus;

        let directory = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        cx.executor().allow_parking();
        cx.update(gpui_component::init);
        cx.update(crate::root::install_wallet_action_bindings);
        let mut root = None;
        let (_host, cx) = cx.add_window_view(|window, cx| {
            root = Some(crate::root::tests::public_accounts::fixture_root(
                directory.path(),
                &runtime,
                window,
                cx,
            ));
            let view = cx.new(|_| DialogWindow);
            gpui_component::Root::new(view, window, cx)
        });
        let root = root.unwrap();
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));

        for (method, key, button_id) in [
            (
                DeviceAuthMethod::TouchId,
                "enter",
                "wallet-spend-auth-touch-id",
            ),
            (
                DeviceAuthMethod::AppleWatch,
                "space",
                "wallet-spend-auth-apple-watch",
            ),
        ] {
            let dialog = cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    root.open_spend_authorization_dialog_with_review(
                        SpendAuthorizationIntent::WalletConnectRequest {
                            request_key: "removed-request".into(),
                            review_token: 0,
                            reviewed_fee: None,
                        },
                        SpendAuthorizationSummary::new("Review", "", Vec::new()),
                        None,
                        window,
                        cx,
                    )
                })
            });
            cx.update(|window, cx| {
                let prompt = root.update(cx, |root, _| {
                    // Show the action without enrolling or opening a native device_auth prompt.
                    root.touch_id_supported = true;
                    root.apple_watch_supported = true;
                    root.apple_watch_status = DeviceAuthStatus::Enabled;
                    root.touch_id_status = DeviceAuthStatus::Enabled;
                    root.device_auth_prompt_cached()
                });
                dialog.update(cx, |dialog, cx| {
                    dialog.device_auth = prompt;
                    dialog.password_input.update(cx, |input, cx| {
                        input.set_value("unsubmitted password", window, cx);
                    });
                    dialog.focus_password(window, cx);
                    cx.notify();
                });
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.render_frame(cx);
                let input_id = dialog.read(cx).password_input.entity_id();
                let group_id = ("vault-password-device-auth", input_id);
                let group = window.find(group_id).bounds();
                let button = window.within(group_id).find(button_id);
                let bounds = button.bounds();
                assert!(
                    group.left() <= bounds.left()
                        && group.top() <= bounds.top()
                        && bounds.right() <= group.right()
                        && bounds.bottom() <= group.bottom()
                );
                assert_eq!(
                    bounds.size.width, bounds.size.height,
                    "the action is icon-only"
                );
                let fingerprint = window
                    .within(group_id)
                    .find("wallet-spend-auth-touch-id")
                    .bounds();
                let watch = window
                    .within(group_id)
                    .find("wallet-spend-auth-apple-watch")
                    .bounds();
                assert!(
                    fingerprint.right() <= watch.left(),
                    "authentication buttons overlap"
                );
                window.press("tab", cx);
                if method == DeviceAuthMethod::AppleWatch {
                    window.press("tab", cx);
                }
                assert_eq!(window.find(button_id).focused(), Some(true));
            });
            let keystroke = gpui::Keystroke::parse(key).unwrap();
            cx.simulate_event(gpui::KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            });
            cx.simulate_event(gpui::KeyUpEvent { keystroke });
            cx.run_until_parked();
            let deadline = Instant::now() + Duration::from_secs(5);
            while dialog.read_with(cx, |dialog, _| dialog.device_auth_pending) {
                assert!(
                    Instant::now() < deadline,
                    "device authentication did not finish"
                );
                runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
                cx.run_until_parked();
            }
            cx.update(|window, cx| {
                let dialog = dialog.read(cx);
                // The fixture has no sealed password, so activation takes the failure path.
                assert!(
                    dialog
                        .device_auth
                        .as_ref()
                        .is_some_and(|prompt| !prompt.includes(method)
                            && prompt.includes(match method {
                                DeviceAuthMethod::TouchId => DeviceAuthMethod::AppleWatch,
                                DeviceAuthMethod::AppleWatch => DeviceAuthMethod::TouchId,
                            })),
                    "the selected method must fail without removing the other method"
                );
                assert!(dialog.error.is_some());
                assert_eq!(
                    dialog.password_input.read(cx).value(),
                    "unsubmitted password"
                );
                assert!(!dialog.pending);
                assert!(root.read(cx).spend_authorization_cache.is_none());
                assert!(window.has_active_dialog(cx));
                window.close_all_dialogs(cx);
            });
        }
        cx.update(|window, _| window.remove_window());
    }

    #[gpui::test]
    fn spend_device_auth_requires_open_review_and_watch_never_caches_approval(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        cx.executor().allow_parking();
        cx.update(gpui_component::init);
        let mut root = None;
        let (_host, cx) = cx.add_window_view(|window, cx| {
            root = Some(crate::root::tests::public_accounts::fixture_root(
                directory.path(),
                &runtime,
                window,
                cx,
            ));
            let view = cx.new(|_| DialogWindow);
            gpui_component::Root::new(view, window, cx)
        });
        let root = root.unwrap();
        cx.simulate_resize(gpui::size(px(1000.), px(800.)));
        let open_review = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                root.update(cx, |root, cx| {
                    root.spend_authorization_lifetime = SpendAuthorizationLifetime::FiveMinutes;
                    root.open_spend_authorization_dialog_with_review(
                        // The removed request prevents transaction submission while
                        // authorization caching still exercises the real completion.
                        SpendAuthorizationIntent::WalletConnectRequest {
                            request_key: "removed-request".into(),
                            review_token: 0,
                            reviewed_fee: None,
                        },
                        SpendAuthorizationSummary::new("Review", "", Vec::new()),
                        None,
                        window,
                        cx,
                    )
                })
            })
        };
        let complete = |dialog: &Entity<SpendAuthorizationDialogContent>,
                        method: DeviceAuthMethod,
                        window: &mut Window,
                        cx: &mut gpui::App| {
            dialog.update(cx, |dialog, cx| {
                dialog.finish_device_auth(
                    method,
                    DeviceAuthPassword::Password(Zeroizing::new(
                        "public list test password".into(),
                    )),
                    window,
                    cx,
                );
            });
        };
        let wait_for_password = |dialog: &Entity<SpendAuthorizationDialogContent>,
                                 cx: &mut gpui::VisualTestContext| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while dialog.read_with(cx, |dialog, _| dialog.pending) {
                assert!(Instant::now() < deadline, "password check did not finish");
                runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
                cx.run_until_parked();
            }
        };
        for completion in ["device_auth", "password_check"] {
            // Retain the entity as rendered button callbacks can do until redraw.
            let dialog = open_review(cx);
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.update(|window, cx| {
                dialog.update(cx, |dialog, _| dialog.device_auth_pending = true);
                if completion == "password_check" {
                    complete(&dialog, DeviceAuthMethod::TouchId, window, cx);
                }
                root.update(cx, |root, cx| root.open_settings_from_shortcut(window, cx));
                if completion == "device_auth" {
                    complete(&dialog, DeviceAuthMethod::TouchId, window, cx);
                }
            });
            wait_for_password(&dialog, cx);
            assert!(
                root.read_with(cx, |root, _| root.spend_authorization_cache.is_none()),
                "dismissed review populated the authorization cache"
            );
            cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
        }
        // Both completions pass the real password check. The selected five-minute
        // lifetime applies to Touch ID, but Watch must leave no reusable approval,
        // including when a previous Touch ID approval is still cached.
        for method in DeviceAuthMethod::ALL {
            let dialog = open_review(cx);
            cx.update(|window, cx| complete(&dialog, method, window, cx));
            wait_for_password(&dialog, cx);
            assert!(dialog.read_with(cx, |dialog, _| dialog.error.is_none()));
            assert_eq!(
                root.update(cx, |root, cx| {
                    root.valid_spend_authorization_password(cx).is_some()
                }),
                method == DeviceAuthMethod::TouchId
            );
        }
        cx.update(|window, _| window.remove_window());
    }

    #[gpui::test]
    fn password_check_refreshes_device_auth_without_authorizing_stale_requests(
        cx: &mut gpui::TestAppContext,
    ) {
        // Password verification wakes GPUI from Tokio's blocking pool.
        cx.executor().allow_parking();
        let path = std::env::temp_dir().join(format!(
            "spend-auth-ui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        cx.update(gpui_component::init);
        let mut root = None;
        let (host, cx) = cx.add_window_view(|window, cx| {
            let wallet =
                crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
            root = Some(wallet.clone());
            gpui_component::Root::new(wallet, window, cx)
        });
        let root = root.unwrap();
        let current_touch_id_status = root.read_with(cx, |root, _| {
            root.vault_store
                .as_ref()
                .unwrap()
                .device_auth_status(wallet_ops::device_auth::DeviceAuthMethod::TouchId)
                .unwrap()
        });
        for interruption in ["cancel", "wallet", "drop"] {
            let lease = Rc::new(Cell::new(true));
            let dialog = cx.update(|window, cx| {
                cx.new(|cx| {
                    SpendAuthorizationDialogContent::new(
                        root.clone(),
                        // A removed request cannot submit a transaction. The remembered
                        // authorization would still be populated if the stale check ran.
                        SpendAuthorizationIntent::WalletConnectRequest {
                            request_key: "removed-request".into(),
                            review_token: 0,
                            reviewed_fee: None,
                        },
                        SpendAuthorizationSummary::new("Review", "", Vec::new()),
                        SpendAuthorizationLifetime::FiveMinutes,
                        Rc::downgrade(&lease),
                        window,
                        cx,
                    )
                })
            });
            cx.update(|window, cx| {
                root.update(cx, |root, _| {
                    root.touch_id_status = wallet_ops::vault::DeviceAuthStatus::NeedsReenrollment;
                });
                dialog.update(cx, |dialog, cx| {
                    dialog.password_input.update(cx, |input, cx| {
                        input.set_value("public list test password", window, cx);
                    });
                    dialog.submit(window, cx);
                    assert!(dialog.pending);
                    if interruption == "wallet" {
                        root.update(cx, |root, _| root.advance_active_wallet_generation());
                    } else if interruption == "cancel" {
                        dialog.cancel(cx);
                    }
                });
            });
            let old_dialog = dialog.downgrade();
            let dialog = (interruption != "drop").then_some(dialog);
            let deadline = Instant::now() + Duration::from_secs(5);
            while dialog.as_ref().map_or_else(
                || {
                    root.read_with(cx, |root, _| {
                        root.touch_id_status != current_touch_id_status
                    })
                },
                |dialog| dialog.read_with(cx, |dialog, _| dialog.pending),
            ) {
                assert!(Instant::now() < deadline, "password check did not finish");
                runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
                cx.run_until_parked();
            }
            assert_eq!(
                root.read_with(cx, |root, _| root.touch_id_status),
                current_touch_id_status,
                "password verification did not refresh persisted device_auth status"
            );
            assert!(root.read_with(cx, |root, _| root.spend_authorization_cache.is_none()));
            if interruption == "drop" {
                assert!(old_dialog.upgrade().is_none());
            }
            if interruption == "wallet" {
                assert!(
                    dialog
                        .unwrap()
                        .read_with(cx, |dialog, _| dialog.error.is_some())
                );
            }
        }
        cx.update(|window, _| window.remove_window());
        drop(host);
        drop(root);
        cx.run_until_parked();
        drop(entered);
        drop(runtime);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn executor_quote_deltas_preserve_small_changes_and_distinguish_adverse_direction() {
        let approved = U256::from(100);
        for (current, higher_is_worse, sign, adverse) in [
            (101, true, "+", true),
            (99, true, "−", false),
            (101, false, "+", false),
            (99, false, "−", true),
        ] {
            let row = SpendAuthorizationSummaryRow::new("", "").with_amount_change(
                Some(approved),
                U256::from(current),
                higher_is_worse,
                |amount| crate::root::format_unshield_amount_input(amount, Some(18)),
            );
            let delta = row.delta.expect("one wei change remains visible");
            assert_eq!(delta.text, format!("{sign}0.000000000000000001"));
            assert_eq!(delta.adverse, adverse);
        }
        for previous in [None, Some(approved)] {
            let row = SpendAuthorizationSummaryRow::new("", "").with_amount_change(
                previous,
                approved,
                true,
                |_| panic!("initial and unchanged amounts have no delta"),
            );
            assert!(row.delta.is_none());
        }
    }

    struct LifetimePickerProbe {
        focus: gpui::FocusHandle,
        password_input: Entity<InputState>,
        lifetime_select: Entity<SpendAuthorizationLifetimeSelect>,
        width: gpui::Pixels,
    }

    impl gpui::Render for LifetimePickerProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            gpui_kit::base::Dialog::new(cx)
                .focus_handle(self.focus.clone())
                .on_ok(|_, _, _| false)
                .popup(
                    div()
                        .w(self.width)
                        .flex()
                        .flex_col()
                        .gap_3()
                        .debug_selector(|| "lifetime-content".to_owned())
                        .child(
                            div()
                                .debug_selector(|| "lifetime-password".to_owned())
                                .child(app_masked_input(&self.password_input, false)),
                        )
                        .child(
                            div()
                                .w_full()
                                .debug_selector(|| "lifetime-row".to_owned())
                                .child(render_spend_authorization_lifetime_row(
                                    &self.lifetime_select,
                                    false,
                                )),
                        )
                        .child(
                            div()
                                .debug_selector(|| "lifetime-footer".to_owned())
                                .w_full()
                                .flex()
                                .flex_wrap()
                                .justify_end()
                                .gap_2()
                                .child(app_button("lifetime-cancel", "Cancel").flex_none())
                                .child(
                                    app_button("lifetime-submit", "Authorize and continue")
                                        .primary()
                                        .flex_none(),
                                ),
                        ),
                )
        }
    }

    #[gpui::test]
    fn lifetime_select_stays_between_password_and_footer(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(ui::theme::apply_zenburn_component_theme);
        let (probe, cx) = cx.add_window_view(|window, cx| LifetimePickerProbe {
            focus: cx.focus_handle(),
            password_input: new_masked_input(window, cx, "Vault password"),
            lifetime_select: new_spend_authorization_lifetime_select(
                SpendAuthorizationLifetime::UntilVaultLock,
                window,
                cx,
            ),
            width: px(400.0),
        });
        for width in [400.0, 280.0] {
            probe.update(cx, |probe, cx| {
                probe.width = px(width);
                cx.notify();
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let content = cx.debug_bounds("lifetime-content").expect("dialog content");
            let password = cx
                .debug_bounds("lifetime-password")
                .expect("password input");
            let row = cx.debug_bounds("lifetime-row").expect("lifetime row");
            let select = cx
                .debug_bounds("wallet-spend-auth-lifetime-select")
                .expect("lifetime select");
            let footer = cx.debug_bounds("lifetime-footer").expect("dialog footer");
            assert!(
                password.bottom() <= row.top(),
                "row overlaps password at {width}"
            );
            assert!(
                row.bottom() <= footer.top(),
                "row overlaps footer at {width}"
            );
            assert!(select.size.height > px(0.0), "select collapsed at {width}");
            assert!(
                select.top() >= row.top() && select.bottom() <= row.bottom(),
                "row does not contain the select at {width}"
            );
            assert!(
                select.left() >= content.left() && select.right() <= content.right(),
                "select overflows the dialog at {width}"
            );
        }
    }

    #[gpui::test]
    fn choosing_a_lifetime_in_the_select_updates_the_dialog(cx: &mut gpui::TestAppContext) {
        let path = std::env::temp_dir().join(format!(
            "spend-auth-lifetime-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        cx.update(gpui_component::init);
        let mut root = None;
        let (host, cx) = cx.add_window_view(|window, cx| {
            let wallet =
                crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
            root = Some(wallet.clone());
            gpui_component::Root::new(wallet, window, cx)
        });
        let root = root.unwrap();
        let lease = Rc::new(Cell::new(true));
        let dialog = cx.update(|window, cx| {
            cx.new(|cx| {
                SpendAuthorizationDialogContent::new(
                    root.clone(),
                    SpendAuthorizationIntent::WalletConnectRequest {
                        request_key: "lifetime-request".into(),
                        review_token: 0,
                        reviewed_fee: None,
                    },
                    SpendAuthorizationSummary::new("Review", "", Vec::new()),
                    SpendAuthorizationLifetime::Once,
                    Rc::downgrade(&lease),
                    window,
                    cx,
                )
            })
        });
        let select = dialog.read_with(cx, |dialog, _| dialog.lifetime_select.clone());
        select.update(cx, |_, cx| {
            cx.emit(
                SelectEvent::<SearchableVec<SpendAuthorizationLifetime>>::Confirm(Some(
                    SpendAuthorizationLifetime::FifteenMinutes,
                )),
            );
        });
        cx.run_until_parked();
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog.lifetime),
            SpendAuthorizationLifetime::FifteenMinutes
        );
        cx.update(|window, _| window.remove_window());
        drop(dialog);
        drop(host);
        drop(root);
        cx.run_until_parked();
        drop(entered);
        drop(runtime);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn remembered_authorization_is_bound_to_exact_wallet_scope() {
        let now = Instant::now();
        let first_scope = SpendAuthorizationScope::new("base-a", "wallet-a", None);
        let second_scope = SpendAuthorizationScope::new("base-a", "wallet-b", None);
        let session_scope = SpendAuthorizationScope::new(
            "base-a",
            "wallet-a",
            Some(SoftwareSeedSessionBinding::new(
                "base-a",
                "wallet-a",
                wallet_ops::vault::VaultSessionId::from_bytes([7; 16]),
            )),
        );
        let cache = SpendAuthorizationCache::new(
            Zeroizing::new("password".to_owned()),
            SpendAuthorizationLifetime::UntilVaultLock,
            first_scope.clone(),
            now,
        )
        .expect("remembered cache");

        assert!(cache.is_valid_at(&first_scope, now));
        assert!(!cache.is_valid_at(&second_scope, now));
        assert!(!cache.is_valid_at(&session_scope, now));
    }

    #[test]
    fn context_cleanup_drops_protected_seed_and_remembered_spend_authorization() {
        let created = wallet_ops::vault::create_with_params(
            "test-vault-password",
            wallet_ops::vault::KdfParams::default(),
        )
        .expect("create test vault");
        let binding = SoftwareSeedSessionBinding::new(
            "base-profile",
            "child-context",
            wallet_ops::vault::VaultSessionId::from_bytes([8; 16]),
        );
        let protected = created
            .spend
            .seal_software_seed_session(binding.clone(), &[7; 64])
            .expect("seal protected seed");
        let scope = SpendAuthorizationScope::new("base-profile", "child-context", Some(binding));
        let mut protected = Some(Arc::new(protected));
        let mut remembered = SpendAuthorizationCache::new(
            Zeroizing::new("test-vault-password".to_owned()),
            SpendAuthorizationLifetime::UntilVaultLock,
            scope,
            Instant::now(),
        );

        assert!(clear_protected_software_seed_session_state(
            &mut protected,
            &mut remembered,
        ));
        assert!(protected.is_none());
        assert!(remembered.is_none());
    }
}
