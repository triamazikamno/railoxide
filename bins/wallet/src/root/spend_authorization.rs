use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use super::DeliveryMode;
use super::auto_lock::AutoLockTimestamp;
use gpui::{AppContext, Context, Entity, ParentElement, SharedString, Styled, Window, div, px};
use gpui_component::{
    WindowExt,
    select::{SearchableVec, SelectItem, SelectState},
};
use ui::dialog::WalletDialogExt as _;
use wallet_ops::DesktopPrivateSpendAuthorization;
#[cfg(feature = "hardware")]
use wallet_ops::hardware::HardwareDeviceKind;
use wallet_ops::vault::{
    PublicAccountSource, SoftwareSeedSessionBinding, WalletSoftwareContextKind,
};
use zeroize::Zeroizing;

use super::{WalletRoot, dialog_max_height, secondary_dialog_content_width};

mod authorization_state;
mod authorized_spend;
mod dialog;
mod hardware;
mod intent;
mod request_flow;
mod review_dialogs;
mod summary;
mod summary_view;

use dialog::SpendAuthorizationDialogContent;
pub(super) use hardware::HardwareSpendAuthorizationCompletion;
#[cfg(feature = "hardware")]
pub(super) use hardware::{HardwareSpendAuthorizationError, hardware_synthetic_entropy};
#[cfg(test)]
pub(super) use intent::private_fee_review_change;
pub(super) use intent::{PrivateFeeReview, SpendAuthorizationIntent, automatic_private_fee_bounds};
pub(super) use summary::{
    SpendAuthorizationCard, SpendAuthorizationHint, SpendAuthorizationLabel,
    SpendAuthorizationSteps, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
    spend_authorization_can_use_cached_password,
};
use summary_view::spend_authorization_title;
pub(super) use summary_view::{
    render_spend_authorization_steps, spend_authorization_recipient_display,
};

const SPEND_AUTHORIZATION_DIALOG_WIDTH: gpui::Pixels = px(560.0);
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
    // Dual-clock so suspend time counts toward the lifetime.
    granted_at: AutoLockTimestamp,
    lifetime: Option<Duration>,
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
        now: AutoLockTimestamp,
    ) -> Option<Self> {
        match lifetime {
            SpendAuthorizationLifetime::Once => None,
            SpendAuthorizationLifetime::UntilVaultLock
            | SpendAuthorizationLifetime::FiveMinutes
            | SpendAuthorizationLifetime::FifteenMinutes => Some(Self {
                password,
                scope,
                granted_at: now,
                lifetime: lifetime.duration(),
            }),
        }
    }

    fn is_valid_at(&self, scope: &SpendAuthorizationScope, now: AutoLockTimestamp) -> bool {
        self.scope == *scope
            && self
                .lifetime
                .is_none_or(|lifetime| now.elapsed_since(self.granted_at) < lifetime)
    }
}

pub(super) fn is_spend_authorization_failure_error(error: &str) -> bool {
    error.contains("authorize ") && error.ends_with("unlock failed")
}

#[cfg(test)]
pub(super) fn remembered_spend_authorization_valid_for_test(
    lifetime: SpendAuthorizationLifetime,
    elapsed: Duration,
) -> bool {
    let now = AutoLockTimestamp::now();
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
mod tests;
