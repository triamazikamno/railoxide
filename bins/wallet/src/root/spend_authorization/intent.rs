use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use gpui::Entity;
use wallet_ops::vault::PublicAccountSource;
use wallet_ops::{
    BlockedShieldRescueUtxoId, DesktopPrivateSpendAuthorization, PublicBroadcasterApprovalBounds,
    PublicBroadcasterCostEstimate, SponsoredAuthorizationLimit,
    default_public_broadcaster_fee_limit,
};

use super::super::governance_action::GovernanceSpendDraft;
use super::super::private_action::UnshieldAssetKey;
use super::super::public_action::{PublicSendDraft, PublicShieldDraft};
use super::super::walletconnect::WalletConnectReviewedFeeProjection;
use super::super::{DeliveryMode, WalletRoot};
use super::summary::SpendAuthorizationSummary;

#[derive(Clone)]
pub(in crate::root) enum SpendAuthorizationIntent {
    ExecutorGasPassword {
        intent: Box<Self>,
        summary: Box<SpendAuthorizationSummary>,
        payer: String,
    },
    StealthAccounts(
        Entity<super::super::stealth_accounts::StealthAccountsView>,
        Arc<super::super::stealth_accounts::StealthAuthorization>,
    ),
    PrivateSwap(
        Entity<super::super::private_swap::PrivateSwapsView>,
        Arc<super::super::private_swap::SwapAuthorization>,
    ),
    PublicSwap(
        Entity<super::super::private_swap::PrivateSwapsView>,
        Arc<super::super::private_swap::PublicSwapAuthorization>,
    ),
    PublicSwapSource {
        view: Entity<super::super::private_swap::PrivateSwapsView>,
        command: Arc<super::super::private_swap::PublicSwapAuthorization>,
        private_authorization: Rc<RefCell<Option<DesktopPrivateSpendAuthorization>>>,
    },
    PrepareExecutorUnshield(
        UnshieldAssetKey,
        Arc<super::super::private_action::ExecutorUnshieldApproval>,
        Option<wallet_ops::gateway::GatewayDraftExecution>,
    ),
    ExecutorUnshield(
        UnshieldAssetKey,
        Arc<super::super::private_action::ExecutorUnshieldReview>,
        Option<wallet_ops::gateway::GatewayDraftExecution>,
    ),
    PrivateSend(
        UnshieldAssetKey,
        Option<SponsoredAuthorizationLimit>,
        Option<wallet_ops::gateway::GatewayDraftExecution>,
        Option<(alloy::primitives::Address, alloy::primitives::U256)>,
        Option<Arc<PrivateFeeReview>>,
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
        Option<Arc<PrivateFeeReview>>,
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
    TronSend(Box<super::super::public_account::TronSendIntent>),
    Governance(Box<GovernanceSpendDraft>),
    WalletConnectRequest {
        request_key: String,
        review_token: u64,
        reviewed_fee: Option<WalletConnectReviewedFeeProjection>,
    },
}

impl SpendAuthorizationIntent {
    pub(in crate::root) fn hardware_executor_action(
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
            Self::PublicShield(draft) => {
                // A stealth-route native shield signs from a new stealth account, whatever pays.
                if let Some(operation) = draft.native_operation {
                    return Some(HardwareExecutorAction::Execute(operation));
                }
                (
                    draft.public_account_source,
                    draft.public_account_uuid.to_string(),
                )
            }
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

    pub(super) fn gateway_execution(&self) -> Option<&wallet_ops::gateway::GatewayDraftExecution> {
        match self {
            Self::ExecutorGasPassword { intent, .. } => intent.gateway_execution(),
            Self::PrivateSend(_, _, execution, _, _)
            | Self::PrepareExecutorUnshield(_, _, execution)
            | Self::ExecutorUnshield(_, _, execution)
            | Self::PrivateUnshield(_, _, execution, _, _)
            | Self::PrivateSendSelfBroadcastGasPassword(_, _, execution)
            | Self::PrivateUnshieldSelfBroadcastGasPassword(_, _, execution) => execution.as_ref(),
            Self::PublicSend(draft) => draft.gateway_execution.as_ref(),
            Self::PublicShield(draft) => draft.gateway_execution.as_ref(),
            _ => None,
        }
    }

    pub(super) fn private_attention(&self, step: &str, message: &str) {
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

    pub(super) fn private_review_current(&self, root: &WalletRoot) -> bool {
        if let Self::ExecutorGasPassword { intent, .. } = self {
            return intent.private_review_current(root);
        }
        if self.private_fee_change(root).is_some() {
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
            Self::PrivateSend(key, _, _, _, _)
            | Self::PrivateSendSelfBroadcastGasPassword(key, _, _) => root
                .send_forms
                .get(key)
                .map(|form| form.gateway_execution.as_ref()),
            Self::PrivateUnshield(key, _, _, _, _)
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

    /// Why the private fee shown in the review no longer matches its form, if it does not.
    pub(super) fn private_fee_change(&self, root: &WalletRoot) -> Option<String> {
        let (key, reviewed_custom, reviewed_quote, current) = match self {
            Self::ExecutorGasPassword { intent, .. } => return intent.private_fee_change(root),
            Self::PrivateSend(key, _, _, custom, quote) => (
                key,
                custom,
                quote,
                root.send_forms.get(key).map(|form| {
                    (
                        form.custom_fee_amount
                            .map(|amount| (form.selected_fee_token, amount)),
                        form.cost_estimate.as_ref(),
                    )
                }),
            ),
            Self::PrivateUnshield(key, _, _, custom, quote) => (
                key,
                custom,
                quote,
                root.unshield_forms.get(key).map(|form| {
                    (
                        form.custom_fee_amount
                            .map(|amount| (form.selected_fee_token, amount)),
                        form.cost_estimate.as_ref(),
                    )
                }),
            ),
            _ => return None,
        };
        let Some((current_custom, current_quote)) = current else {
            return Some("The action is no longer available.".into());
        };
        private_fee_review_change(
            *reviewed_custom,
            current_custom,
            reviewed_quote.as_deref(),
            current_quote,
            |token, amount| {
                super::super::private_action::fee_editor::custom_fee_label(
                    key.chain_id,
                    token,
                    amount,
                    &root.effective_token_registry,
                )
            },
        )
    }

    pub(in crate::root) fn approve_gateway_review(&self, root: &WalletRoot) -> bool {
        self.private_review_current(root)
            && self
                .gateway_execution()
                .is_none_or(wallet_ops::gateway::GatewayDraftExecution::approve_review)
    }

    pub(super) const fn uses_private_wallet(&self) -> bool {
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

/// The broadcaster quote a review approved, with the limits submission enforces.
pub(in crate::root) struct PrivateFeeReview {
    pub(in crate::root) quote: PublicBroadcasterCostEstimate,
    pub(in crate::root) bounds: PublicBroadcasterApprovalBounds,
}

impl PrivateFeeReview {
    /// A custom fee is its own maximum; an automatic fee gets the default allowance.
    pub(in crate::root) fn new(
        quote: PublicBroadcasterCostEstimate,
        custom_fee: Option<U256>,
    ) -> eyre::Result<Self> {
        let bounds = match custom_fee {
            Some(fee) => quote.approval_bounds(fee)?,
            None => automatic_private_fee_bounds(&quote)?,
        };
        Ok(Self { quote, bounds })
    }
}

/// The limits submission enforces for an automatic fee; the estimate card shows their maximum.
pub(in crate::root) fn automatic_private_fee_bounds(
    quote: &PublicBroadcasterCostEstimate,
) -> eyre::Result<PublicBroadcasterApprovalBounds> {
    quote.approval_bounds(default_public_broadcaster_fee_limit(quote.fee_amount))
}

/// Submission enforces the reviewed limits, so a changed custom fee, a cleared estimate, or a
/// quote outside the approved bounds needs a new review. Returns why, or `None` while it is current.
/// A background refresh keeps the shown estimate, which is compared like any other quote.
pub(in crate::root) fn private_fee_review_change(
    reviewed_custom: Option<(Address, U256)>,
    current_custom: Option<(Address, U256)>,
    review: Option<&PrivateFeeReview>,
    current_quote: Option<&PublicBroadcasterCostEstimate>,
    fee_label: impl Fn(Address, U256) -> String,
) -> Option<String> {
    if reviewed_custom != current_custom {
        return Some("The custom transaction fee changed. Review it again.".into());
    }
    let PrivateFeeReview {
        quote: reviewed,
        bounds,
    } = review?;
    let Some(current) = current_quote else {
        return Some(
            "The transaction fee estimate is refreshing. Review it again once it is ready.".into(),
        );
    };
    // A refreshed offer from the same broadcaster is fine; the bounds decide the amounts.
    if current.broadcaster.railgun_address != reviewed.broadcaster.railgun_address
        || current.action_token != reviewed.action_token
        || current.fee_token != reviewed.fee_token
        || current.fee_mode != reviewed.fee_mode
        || current.entered_amount != reviewed.entered_amount
        || current.native_top_up != reviewed.native_top_up
    {
        return Some("The broadcaster quote changed. Review it again.".into());
    }
    if bounds.covers(current) {
        return None;
    }
    let reviewed_fee = fee_label(reviewed.fee_token, reviewed.fee_amount);
    let current_fee = fee_label(current.fee_token, current.fee_amount);
    Some(if current.fee_amount > bounds.maximum_fee() {
        format!(
            "The transaction fee rose from {reviewed_fee} to {current_fee}, above the approved maximum of {}. Review it again.",
            fee_label(reviewed.fee_token, bounds.maximum_fee()),
        )
    } else {
        format!(
            "The transaction fee changed from {reviewed_fee} to {current_fee}. Review it again."
        )
    })
}
