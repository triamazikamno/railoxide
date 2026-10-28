//! Derive from private: one authorization opens an in-memory address preview. A software
//! wallet authorizes with the vault password and adds the chosen indexes in one vault write. A
//! hardware wallet reads the account key from its device and adds the chosen indexes one at a
//! time, each confirmed on the device. The preview can also check the native balances of its
//! selected indexes on request.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as _,
    rc::Weak,
    sync::Arc,
};

use alloy::primitives::{Address, U256};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement, Pixels, SharedString, StatefulInteractiveElement as _, Styled, Subscription,
    Window, div, prelude::FluentBuilder as _, px, rems,
};
use gpui_component::{
    Disableable, IconName, Sizable,
    alert::Alert,
    button::ButtonVariants,
    checkbox::Checkbox,
    description_list::{DescriptionItem, DescriptionList},
    dialog::Confirm,
    input::InputState,
    select::{SelectEvent, SelectState},
};
use railgun_ui::short_address;
use rand::seq::SliceRandom as _;
use ui::chain_select::{ChainSelectItem, ChainSelectItems, chain_select, chain_select_state};
use ui::controls::{app_button, app_input, app_muted_text, app_text};
use wallet_ops::hardware::{
    ConfirmedHardwarePublicAccount, HardwareDeviceKind, HardwarePublicAccountDescriptor,
};
use wallet_ops::settings::EffectiveChainConfig;
use wallet_ops::vault::{
    DerivedPublicAccountPreview, DesktopVaultStore, DesktopViewSession, HardwareProfileBinding,
    HardwareProfileSession, PreviewedPublicAccountSkip, PreviewedPublicAccountsAdded,
    PublicAccountMetadata, PublicAccountScope, PublicAccountSource, PublicAccountStatus,
    VaultError,
};
use wallet_ops::{RpcTrafficClass, RpcTransport, WalletNetworkMode};
use zeroize::Zeroizing;

use super::hardware::{
    ConnectedHardwareDevice, HardwareDeviceRequest, HardwareDeviceResult,
    HardwarePublicAccountDevice, hardware_public_account_setup_copy, hardware_public_device_label,
    render_hardware_public_account_confirmation_wait,
};
use crate::root::device_auth::masked_input_with_device_auth;
use crate::root::dialogs::PublicAccountDialogKind;
use crate::root::public_balances::isolated_read_jitter;
use crate::root::ui_helpers::{count_label, input_enter_scope};
use crate::root::vault::{vault_error_kind, vault_error_message};
use crate::root::{WalletRoot, new_text_input};

#[cfg(test)]
mod tests;

const PAGE_SIZE: u32 = DerivedPublicAccountPreview::PAGE_SIZE;
const INDEX_LIMIT: u32 = DerivedPublicAccountPreview::INDEX_LIMIT;

pub(in crate::root) struct DerivePreviewState {
    pub(super) go_to_input: Entity<InputState>,
    /// Holds focus in the open preview so Enter reaches Add.
    pub(super) focus: FocusHandle,
    /// The open Derive dialog's lease.
    pub(super) lease: Weak<Cell<bool>>,
    /// Advanced by every authorization and reset, so a late result can't open a later preview.
    pub(super) authorization: u64,
    pub(super) authorizing: bool,
    /// Advanced by every balance check start, stop and reset, so a stopped check sends no
    /// further address and its in-flight result is discarded.
    pub(super) check_run: u64,
    /// Advanced by every hardware add start and reset, so a stopped add asks the device for
    /// nothing more and an in-flight confirmation isn't stored.
    pub(super) add_run: u64,
    pub(super) preview: Option<DerivePreview>,
    /// Stands in for the Ledger or Trezor.
    #[cfg(test)]
    pub(in crate::root) device_stub: Option<Arc<dyn HardwarePublicAccountDevice>>,
}

/// An authorized preview. Its key links every derived address, so it lives only as long as
/// the dialog's authorization.
pub(in crate::root) struct DerivePreview {
    pub(super) keys: DerivedPublicAccountPreview,
    /// The hardware profile a hardware wallet's preview was read under.
    pub(super) hardware: Option<HardwarePreviewIdentity>,
    /// The running hardware add.
    pub(super) adding: Option<HardwareAdd>,
    pub(super) page_start: u32,
    pub(super) selected: BTreeSet<u32>,
    pub(super) go_to_error: Option<Arc<str>>,
    pub(super) last_add: Option<PreviewedPublicAccountsAdded>,
    /// The chain a balance check reads. Changing it sends nothing.
    pub(super) chain_id: u64,
    pub(super) chain_select: Entity<SelectState<ChainSelectItems>>,
    _chain_changes: Subscription,
    /// The summary shown before a check sends anything is open.
    pub(super) reviewing_check: bool,
    /// The indexes of the running check.
    pub(super) checking: Option<Vec<u32>>,
    /// Balance checks by chain ID and index. A missing entry is unchecked. Kept only here.
    pub(super) results: HashMap<(u64, u32), BalanceCheck>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BalanceCheck {
    Queued,
    Loading,
    Done(U256),
    Failed,
}

/// The hardware profile a preview was read under. A refreshed Trezor session ID keeps the
/// same profile, so it isn't part of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HardwarePreviewIdentity {
    pub(super) device_kind: HardwareDeviceKind,
    profile_id: Option<String>,
    binding: HardwareProfileBinding,
}

impl HardwarePreviewIdentity {
    fn of(session: &HardwareProfileSession) -> Self {
        Self {
            device_kind: session.device_kind,
            profile_id: session.profile_id.clone(),
            binding: session.binding.clone(),
        }
    }
}

/// A hardware add: the selected indexes in ascending order, each confirmed on the device and
/// stored before the next.
pub(super) struct HardwareAdd {
    indexes: Vec<u32>,
    /// The position of the next index to take.
    next: usize,
    added: Vec<PublicAccountMetadata>,
    skipped: Vec<(u32, PreviewedPublicAccountSkip)>,
    /// The index the device is asked about, with its preview address.
    confirming: Option<(u32, Address)>,
}

impl DerivePreviewState {
    pub(in crate::root) fn new(window: &mut Window, cx: &mut Context<'_, WalletRoot>) -> Self {
        Self {
            go_to_input: new_text_input(window, cx, "Go to index"),
            focus: cx.focus_handle(),
            lease: Weak::new(),
            authorization: 0,
            authorizing: false,
            check_run: 0,
            add_run: 0,
            preview: None,
            #[cfg(test)]
            device_stub: None,
        }
    }

    /// Returns to the authorize state and orphans any pending authorization, balance check or
    /// hardware add.
    pub(in crate::root) fn reset(&mut self) {
        self.authorization = self.authorization.wrapping_add(1);
        self.check_run = self.check_run.wrapping_add(1);
        self.add_run = self.add_run.wrapping_add(1);
        self.authorizing = false;
        self.preview = None;
    }

    /// Ends a hardware preview, and any add it runs, unless `session` is the hardware profile
    /// session it was read under.
    #[cfg(any(feature = "hardware", test))]
    pub(in crate::root) fn end_hardware_preview_unless(
        &mut self,
        session: Option<&HardwareProfileSession>,
    ) {
        let read_under = self
            .preview
            .as_ref()
            .and_then(|preview| preview.hardware.as_ref());
        if read_under.is_some_and(|identity| {
            session.is_none_or(|session| *identity != HardwarePreviewIdentity::of(session))
        }) {
            self.reset();
        }
    }

    fn adding(&self) -> bool {
        self.preview
            .as_ref()
            .is_some_and(|preview| preview.adding.is_some())
    }

    /// Stops a balance check before its next address. The address in progress may finish, but
    /// its result is discarded, and every row without a result returns to unchecked.
    fn stop_balance_check(&mut self) {
        self.check_run = self.check_run.wrapping_add(1);
        if let Some(preview) = self.preview.as_mut() {
            preview.checking = None;
            preview
                .results
                .retain(|_, check| matches!(check, BalanceCheck::Done(_) | BalanceCheck::Failed));
        }
    }
}

const fn page_start(index: u32) -> u32 {
    index - index % PAGE_SIZE
}

/// The most indexes the summary lists before summarizing the rest.
const SUMMARY_LISTED_INDEXES: usize = 8;

/// The rows and warning of the summary shown before a balance check of `indexes` sends any
/// address.
fn balance_check_summary(
    chain: &EffectiveChainConfig,
    mode: WalletNetworkMode,
    indexes: &BTreeSet<u32>,
) -> ([(&'static str, String); 5], &'static str) {
    let count = indexes.len();
    let noun = if count == 1 { "address" } else { "addresses" };
    let mut listed = indexes
        .iter()
        .take(SUMMARY_LISTED_INDEXES)
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if count > SUMMARY_LISTED_INDEXES {
        let _ = write!(listed, " and {} more", count - SUMMARY_LISTED_INDEXES);
    }
    // The mean 0.5–3 s wait between addresses plus about 2 s for each address's reads.
    let seconds = (count as u64 * 3_750).div_ceil(1_000);
    let time = if seconds < 60 {
        format!("About {seconds} s")
    } else {
        format!("About {} min", (seconds + 30) / 60)
    };
    // Endpoints set to Direct bypass Tor and the proxy, so they see the addresses with your IP.
    let (direct, total) = chain
        .rpc_route
        .for_class(RpcTrafficClass::Sensitive)
        .map_or((0, 0), |route| {
            let direct = route
                .endpoints()
                .iter()
                .filter(|url| route.endpoint_transport(url.expose_url()) == RpcTransport::Direct)
                .count();
            (direct, route.endpoints().len())
        });
    let (route, warning) = match mode {
        WalletNetworkMode::Tor | WalletNetworkMode::Proxy if direct > 0 && direct == total => (
            "Directly to a randomly chosen provider, because this chain's Sensitive endpoints are set to Direct. The addresses aren't isolated from each other",
            "RPC providers receive these addresses and can link them to each other and to you.",
        ),
        WalletNetworkMode::Tor if direct > 0 => (
            "Each address on its own Tor route to a randomly chosen provider. A provider set to Direct gets its addresses directly instead",
            "RPC providers receive these addresses and can still link them by timing. A provider set to Direct also sees your IP and can link the addresses it receives to each other and to you.",
        ),
        WalletNetworkMode::Tor => (
            "Each address on its own Tor route to a randomly chosen provider",
            "RPC providers receive these addresses and can still link them by timing. Tor doesn't prevent that.",
        ),
        WalletNetworkMode::Proxy if direct > 0 => (
            "Over your proxy to a randomly chosen provider, or directly to a provider set to Direct. The addresses aren't isolated from each other",
            "RPC providers receive these addresses and can link them to each other and to you. The proxy doesn't prevent that, and a provider set to Direct also sees your IP.",
        ),
        WalletNetworkMode::Proxy => (
            "Over your proxy to a randomly chosen provider. The addresses aren't isolated from each other",
            "RPC providers receive these addresses and can link them to each other and to you. The proxy doesn't prevent that.",
        ),
        WalletNetworkMode::Direct => (
            "Directly to a randomly chosen provider. The addresses aren't isolated from each other",
            "RPC providers receive these addresses and can link them to each other and to you.",
        ),
    };
    (
        [
            ("Chain", chain.name.clone()),
            ("Addresses", format!("{listed} ({count} {noun})")),
            (
                "Sent as",
                "One request per address, in random order, 0.5–3 s apart".to_owned(),
            ),
            ("Route", route.to_owned()),
            ("Expected time", time),
        ],
        warning,
    )
}

fn add_result_message(result: &PreviewedPublicAccountsAdded) -> String {
    let mut message = format!("Added {}.", count_label(result.added.len(), "account"));
    for (index, reason) in &result.skipped {
        let reason = match reason {
            PreviewedPublicAccountSkip::AlreadyAdded => "already added",
            PreviewedPublicAccountSkip::DuplicateAddress => "address already in Public accounts",
        };
        let _ = write!(message, " Skipped index {index}: {reason}.");
    }
    message
}

/// Why a hardware add stopped, with the indexes it added and those it didn't. Kept outside
/// the preview, which a stale Trezor session ends.
fn hardware_add_stopped_message(error: &str, added: &[u32], not_added: &[u32]) -> String {
    let list = |indexes: &[u32]| {
        let noun = if indexes.len() == 1 {
            "index"
        } else {
            "indexes"
        };
        let indexes: Vec<String> = indexes.iter().map(u32::to_string).collect();
        format!("{noun} {}", indexes.join(", "))
    };
    let mut message = error.to_owned();
    if !message.ends_with('.') {
        message.push('.');
    }
    if !added.is_empty() {
        let _ = write!(message, " Added {}.", list(added));
    }
    if !not_added.is_empty() {
        let _ = write!(message, " Not added: {}.", list(not_added));
    }
    message
}

impl WalletRoot {
    /// Whether the open Derive dialog can start an authorization.
    fn derive_preview_authorizable(&self) -> bool {
        let state = &self.public_form.derive_preview;
        !state.authorizing
            && state.preview.is_none()
            && state.lease.upgrade().is_some_and(|open| open.get())
    }

    /// Starts an authorization and returns its number, wallet generation and dialog lease.
    fn begin_derive_preview_authorization(&mut self) -> (u64, u64, Weak<Cell<bool>>) {
        let state = &mut self.public_form.derive_preview;
        state.authorization = state.authorization.wrapping_add(1);
        state.authorizing = true;
        self.public_form.error = None;
        (
            state.authorization,
            self.active_wallet_generation,
            state.lease.clone(),
        )
    }

    /// Ends an authorization and reports whether its result may still open the preview.
    fn end_derive_preview_authorization(
        &mut self,
        authorization: u64,
        generation: u64,
        lease: &Weak<Cell<bool>>,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let state = &mut self.public_form.derive_preview;
        if state.authorization != authorization {
            return false;
        }
        state.authorizing = false;
        cx.notify();
        self.active_wallet_generation == generation
            && lease.upgrade().is_some_and(|open| open.get())
    }

    /// Authorizes the preview with the typed vault password or one from device authentication.
    pub(super) fn authorize_derive_preview(
        &mut self,
        device_auth_password: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.derive_preview_authorizable() {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        let password = match device_auth_password {
            Some(password) => password,
            None => Self::read_and_clear_input(&self.public_form.add_password_input, window, cx),
        };
        if password.trim().is_empty() {
            self.public_form.error = Some(Arc::from("Enter the vault password to show addresses"));
            cx.notify();
            return;
        }
        let protected_seed_session = self.protected_software_seed_session.clone();
        let (authorization, generation, lease) = self.begin_derive_preview_authorization();
        // The vault KDF is slow, so it runs off the UI thread.
        let join = self.runtime.spawn_blocking(move || {
            store.derived_public_account_preview(
                password.as_str(),
                &view_session,
                protected_seed_session.as_deref(),
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.finish_derive_preview_authorization(
                    authorization,
                    generation,
                    &lease,
                    result,
                    window,
                    cx,
                );
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn finish_derive_preview_authorization(
        &mut self,
        authorization: u64,
        generation: u64,
        lease: &Weak<Cell<bool>>,
        result: Result<Result<DerivedPublicAccountPreview, VaultError>, tokio::task::JoinError>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.end_derive_preview_authorization(authorization, generation, lease, cx) {
            return;
        }
        match result {
            Ok(Ok(keys)) => self.open_derive_preview(keys, None, window, cx),
            Ok(Err(error)) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    "derived public account preview failed"
                );
                self.public_form.error = Some(vault_error_message(&error));
            }
            Err(error) => {
                tracing::warn!(%error, "derived public account preview task failed");
                self.public_form.error = Some(Arc::from("Showing addresses failed. Try again."));
            }
        }
    }

    /// The connected device, or the test's or the UI fixture's stand-in.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn hardware_public_account_device(&self) -> Arc<dyn HardwarePublicAccountDevice> {
        #[cfg(test)]
        if let Some(stub) = &self.public_form.derive_preview.device_stub {
            return Arc::clone(stub);
        }
        #[cfg(debug_assertions)]
        if crate::root::private_swap::ui_fixture_hardware_derive() {
            return Arc::new(super::hardware::UiFixtureHardwareDevice);
        }
        Arc::new(ConnectedHardwareDevice)
    }

    /// A device request under the current view session's hardware profile session, with a
    /// fresh PIN matrix provider for a Trezor.
    #[cfg_attr(
        not(feature = "hardware"),
        allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)
    )]
    fn hardware_device_request(
        &mut self,
        device_kind: HardwareDeviceKind,
        view_session: &DesktopViewSession,
        trezor_app_passphrase: Option<Zeroizing<String>>,
        window: &Window,
        cx: &Context<'_, Self>,
    ) -> Option<HardwareDeviceRequest> {
        let hardware_session = match view_session.hardware_profile_session() {
            Some(session) => session.clone(),
            // The `hardware-derive` UI fixture's stand-in Ledger needs no unlocked profile.
            None if crate::root::private_swap::ui_fixture_hardware_derive() => {
                HardwareProfileSession::unmatched(
                    HardwareDeviceKind::Ledger,
                    HardwareProfileBinding::evm_address_fingerprint("ui-fixture"),
                    None,
                )
            }
            None => return None,
        };
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider = (device_kind == HardwareDeviceKind::Trezor)
            .then(|| self.trezor_pin_matrix_provider_for_operation(window, cx));
        #[cfg(not(feature = "hardware"))]
        let _ = (window, cx);
        Some(HardwareDeviceRequest {
            device_kind,
            wallet_account_index: view_session.derivation_index(),
            hardware_session,
            trezor_app_passphrase,
            #[cfg(feature = "hardware")]
            trezor_pin_matrix_provider,
        })
    }

    /// Reads the hardware wallet's account key from its device and builds the preview from it,
    /// both off the UI thread. Nothing is shown on the device.
    #[cfg_attr(not(feature = "hardware"), allow(clippy::needless_pass_by_ref_mut))]
    pub(super) fn authorize_hardware_derive_preview(
        &mut self,
        device_kind: HardwareDeviceKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.derive_preview_authorizable() {
            return;
        }
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase = view_session.hardware_profile_session().and_then(|session| {
            self.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
        });
        #[cfg(not(feature = "hardware"))]
        let trezor_app_passphrase = None;
        let Some(request) = self.hardware_device_request(
            device_kind,
            &view_session,
            trezor_app_passphrase,
            window,
            cx,
        ) else {
            self.public_form.error = Some(Arc::from(
                "Unlock the matching hardware profile before showing addresses",
            ));
            cx.notify();
            return;
        };
        let identity = HardwarePreviewIdentity::of(&request.hardware_session);
        let device = self.hardware_public_account_device();
        let wallet_id = view_session.wallet_id().to_owned();
        let (authorization, generation, lease) = self.begin_derive_preview_authorization();
        let join = self.runtime.spawn(async move {
            let (key, session) = device.read_account_key(request).await?;
            let keys = DerivedPublicAccountPreview::from_account_public_key(
                &wallet_id,
                &key.public_key,
                key.chain_code,
            )
            .map_err(|error| {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    "hardware public account preview failed"
                );
                vault_error_message(&error).to_string()
            })?;
            Ok::<_, String>((keys, session))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.end_derive_preview_authorization(authorization, generation, &lease, cx) {
                    root.finish_hardware_derive_preview_authorization(identity, result, window, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_hardware_derive_preview_authorization(
        &mut self,
        identity: HardwarePreviewIdentity,
        result: Result<
            Result<(DerivedPublicAccountPreview, HardwareProfileSession), String>,
            tokio::task::JoinError,
        >,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.clear_trezor_pin_matrix_prompt(cx);
        match result {
            Ok(Ok((keys, session))) => {
                self.open_derive_preview(keys, Some(identity), window, cx);
                #[cfg(feature = "hardware")]
                self.refresh_active_hardware_profile_session(session, cx);
                #[cfg(not(feature = "hardware"))]
                let _ = session;
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "hardware public account key read failed");
                self.discard_active_trezor_session_if_stale(&error, cx);
                self.public_form.error = Some(Arc::from(error));
            }
            Err(error) => {
                tracing::warn!(%error, "hardware public account key read task failed");
                self.public_form.error = Some(Arc::from("Showing addresses failed. Try again."));
            }
        }
    }

    /// Opens an authorized preview on the page of the next unused index, which it selects.
    fn open_derive_preview(
        &mut self,
        keys: DerivedPublicAccountPreview,
        hardware: Option<HardwarePreviewIdentity>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let next = self
            .public_form
            .next_derived_index
            .filter(|index| *index < INDEX_LIMIT);
        let chain_id = self.selected_chain;
        let chains = self
            .effective_chain_configs
            .enabled_chains()
            .map(|chain| ChainSelectItem {
                chain_id: chain.chain_id,
                label: chain.name.clone().into(),
            })
            .collect();
        let chain_select = cx.new(|cx| chain_select_state(chains, Some(chain_id), window, cx));
        let chain_changes = cx.subscribe_in(
            &chain_select,
            window,
            |root, _, event: &SelectEvent<ChainSelectItems>, window, cx| {
                if let SelectEvent::Confirm(Some(chain_id)) = event {
                    root.set_derive_check_chain(*chain_id, window, cx);
                }
            },
        );
        let state = &mut self.public_form.derive_preview;
        state.preview = Some(DerivePreview {
            keys,
            hardware,
            adding: None,
            page_start: next.map_or(0, page_start),
            selected: next.into_iter().collect(),
            go_to_error: None,
            last_add: None,
            chain_id,
            chain_select,
            _chain_changes: chain_changes,
            reviewing_check: false,
            checking: None,
            results: HashMap::new(),
        });
        state.focus.focus(window, cx);
        self.public_form.error = None;
    }

    /// The status of each derivation index used by this wallet's derived accounts.
    fn derived_index_statuses(&self) -> BTreeMap<u32, PublicAccountStatus> {
        let mut statuses = BTreeMap::new();
        let Some(wallet_id) = self
            .view_session
            .as_ref()
            .map(|session| session.wallet_id())
        else {
            return statuses;
        };
        for account in &self.public_accounts {
            let (Some(index), PublicAccountScope::PrivateWallet { wallet_uuid }) =
                (account.derivation_index, &account.scope)
            else {
                continue;
            };
            if wallet_uuid != wallet_id
                || !matches!(
                    account.source,
                    PublicAccountSource::Derived | PublicAccountSource::HardwareDerived
                )
            {
                continue;
            }
            let status = statuses.entry(index).or_insert(account.status);
            if account.status == PublicAccountStatus::Active {
                *status = PublicAccountStatus::Active;
            }
        }
        statuses
    }

    /// Selection and paging return focus to the preview, so Enter keeps adding.
    pub(super) fn toggle_derive_preview_index(
        &mut self,
        index: u32,
        checked: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        // A hardware add keeps the selection and page it started with.
        let Some(preview) = state
            .preview
            .as_mut()
            .filter(|preview| preview.adding.is_none())
        else {
            return;
        };
        if checked {
            if preview.selected.len() < DerivedPublicAccountPreview::MAX_ADD_INDEXES {
                preview.selected.insert(index);
            }
        } else {
            preview.selected.remove(&index);
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    /// Selects every Available index on the visible page, in order until the cap, or clears them.
    pub(super) fn select_derive_preview_page(
        &mut self,
        select: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let statuses = self.derived_index_statuses();
        let state = &mut self.public_form.derive_preview;
        let Some(preview) = state
            .preview
            .as_mut()
            .filter(|preview| preview.adding.is_none())
        else {
            return;
        };
        let start = preview.page_start;
        let end = start.saturating_add(PAGE_SIZE).min(INDEX_LIMIT);
        for index in (start..end).filter(|index| !statuses.contains_key(index)) {
            if !select {
                preview.selected.remove(&index);
            } else if preview.selected.len() < DerivedPublicAccountPreview::MAX_ADD_INDEXES {
                preview.selected.insert(index);
            }
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn show_derive_preview_page(
        &mut self,
        start: u32,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        let Some(preview) = state
            .preview
            .as_mut()
            .filter(|preview| preview.adding.is_none())
        else {
            return;
        };
        if start < INDEX_LIMIT {
            preview.page_start = page_start(start);
            preview.go_to_error = None;
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn set_derive_check_chain(
        &mut self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        if state
            .preview
            .as_ref()
            .is_none_or(|preview| preview.chain_id == chain_id)
        {
            return;
        }
        state.stop_balance_check();
        if let Some(preview) = state.preview.as_mut() {
            preview.chain_id = chain_id;
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    /// Opens or closes the summary shown before a balance check. Opening it sends nothing.
    pub(super) fn review_derive_balance_check(
        &mut self,
        open: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        if let Some(preview) = state.preview.as_mut() {
            preview.reviewing_check = open && preview.checking.is_none();
        }
        state.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn stop_derive_balance_check(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        state.stop_balance_check();
        state.focus.focus(window, cx);
        cx.notify();
    }

    /// Checks the selected indexes' native balances on the selected chain, one address at a
    /// time in random order with a random wait between them. Later selection edits and page
    /// changes don't affect a running check.
    pub(super) fn start_derive_balance_check(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        state.focus.focus(window, cx);
        cx.notify();
        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        preview.reviewing_check = false;
        let Some(chain) = self.effective_chain_configs.get(preview.chain_id).cloned() else {
            return;
        };
        if preview.checking.is_some() || preview.selected.is_empty() {
            return;
        }
        let mut rows: Vec<_> = preview
            .selected
            .iter()
            .filter_map(|index| Some((*index, preview.keys.address(*index).ok()?)))
            .collect();
        rows.shuffle(&mut rand::rng());
        let chain_id = chain.chain_id;
        for (index, _) in &rows {
            preview
                .results
                .insert((chain_id, *index), BalanceCheck::Queued);
        }
        preview.checking = Some(rows.iter().map(|(index, _)| *index).collect());
        state.check_run = state.check_run.wrapping_add(1);
        let run = state.check_run;
        let lease = state.lease.clone();
        let generation = self.active_wallet_generation;
        let http = self.http.clone();
        let runtime = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            for (position, (index, address)) in rows.into_iter().enumerate() {
                if position > 0 {
                    cx.background_executor().timer(isolated_read_jitter()).await;
                }
                let key = (chain_id, index);
                let started = this.update(cx, |root, cx| {
                    root.record_derive_balance_check(
                        run,
                        generation,
                        &lease,
                        key,
                        BalanceCheck::Loading,
                        cx,
                    )
                });
                if !started.unwrap_or(false) {
                    return;
                }
                let (http, chain) = (http.clone(), chain.clone());
                let result =
                    runtime
                        .spawn(async move {
                            wallet_ops::check_native_balance(&http, &chain, address).await
                        })
                        .await;
                let check = match result {
                    Ok(Ok((balance, _))) => BalanceCheck::Done(balance),
                    _ => BalanceCheck::Failed,
                };
                let recorded = this.update(cx, |root, cx| {
                    root.record_derive_balance_check(run, generation, &lease, key, check, cx)
                });
                if !recorded.unwrap_or(false) {
                    return;
                }
            }
            let _ = this.update(cx, |root, cx| {
                let state = &mut root.public_form.derive_preview;
                if state.check_run == run
                    && let Some(preview) = state.preview.as_mut()
                {
                    preview.checking = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Records one address's check while its run is current, and reports whether it is.
    fn record_derive_balance_check(
        &mut self,
        run: u64,
        generation: u64,
        lease: &Weak<Cell<bool>>,
        key: (u64, u32),
        check: BalanceCheck,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let current = self.active_wallet_generation == generation
            && lease.upgrade().is_some_and(|open| open.get())
            && self.public_form.derive_preview.check_run == run;
        let Some(preview) = self
            .public_form
            .derive_preview
            .preview
            .as_mut()
            .filter(|_| current)
        else {
            return false;
        };
        preview.results.insert(key, check);
        cx.notify();
        true
    }

    pub(super) fn go_to_derive_preview_index(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let index = self
            .public_form
            .derive_preview
            .go_to_input
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|index| *index < INDEX_LIMIT);
        match index {
            Some(index) => self.show_derive_preview_page(index, window, cx),
            None => {
                if let Some(preview) = self.public_form.derive_preview.preview.as_mut() {
                    preview.go_to_error = Some(Arc::from(format!(
                        "Enter an index from 0 to {}.",
                        INDEX_LIMIT - 1
                    )));
                    cx.notify();
                }
            }
        }
    }

    /// Adds the selected indexes. A software wallet adds them in one vault write on the UI
    /// thread, so adds stay strictly one at a time.
    pub(super) fn add_selected_derived_public_accounts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            return;
        };
        let Some(preview) = self.public_form.derive_preview.preview.as_mut() else {
            return;
        };
        if preview.selected.is_empty() || preview.adding.is_some() {
            return;
        }
        if preview.hardware.is_some() {
            self.start_hardware_derived_public_account_add(window, cx);
            return;
        }
        let indexes: Vec<u32> = preview.selected.iter().copied().collect();
        match store.add_previewed_public_accounts(&view_session, &preview.keys, &indexes) {
            Ok(result) => {
                preview.selected.clear();
                let added = result
                    .added
                    .iter()
                    .map(|account| account.public_account_uuid.clone())
                    .collect();
                preview.last_add = Some(result);
                self.public_form.error = None;
                self.show_added_public_accounts(added, window, cx);
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    "add previewed public accounts failed"
                );
                self.public_form.error = Some(vault_error_message(&error));
            }
        }
        cx.notify();
    }

    /// Selects the first added account and refreshes the added accounts' balances.
    fn show_added_public_accounts(
        &mut self,
        added: Vec<String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(first) = added.first() {
            self.public_form.selected_account_uuid = Some(Arc::from(first.as_str()));
        }
        self.reload_public_accounts(window, cx);
        if !added.is_empty() {
            self.schedule_account_public_balance_refresh(self.selected_chain, added, None, cx);
        }
    }

    /// Adds a hardware wallet's selected indexes in ascending order. The device confirms each
    /// address, and each account is stored before the next index is asked for, so a stopped
    /// add keeps the accounts already confirmed.
    fn start_hardware_derived_public_account_add(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let state = &mut self.public_form.derive_preview;
        let Some(preview) = state.preview.as_mut() else {
            return;
        };
        preview.adding = Some(HardwareAdd {
            indexes: preview.selected.iter().copied().collect(),
            next: 0,
            added: Vec::new(),
            skipped: Vec::new(),
            confirming: None,
        });
        preview.last_add = None;
        state.add_run = state.add_run.wrapping_add(1);
        let run = state.add_run;
        let lease = state.lease.clone();
        let generation = self.active_wallet_generation;
        self.public_form.error = None;
        let runtime = self.runtime.clone();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let confirm = this.update_in(cx, |root, window, cx| {
                    root.next_hardware_add_confirmation(run, generation, &lease, window, cx)
                });
                let Ok(Some(confirm)) = confirm else {
                    return;
                };
                let result = runtime.spawn(confirm).await;
                let recorded = this.update_in(cx, |root, window, cx| {
                    root.record_hardware_add_confirmation(
                        run, generation, &lease, result, window, cx,
                    )
                });
                if !recorded.unwrap_or(false) {
                    return;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Whether hardware add `run` is still the dialog's current one.
    fn hardware_add_current(&self, run: u64, generation: u64, lease: &Weak<Cell<bool>>) -> bool {
        let state = &self.public_form.derive_preview;
        state.add_run == run
            && state.adding()
            && self.active_wallet_generation == generation
            && lease.upgrade().is_some_and(|open| open.get())
    }

    /// Skips the next indexes that already have an account and asks the device to confirm the
    /// first one that doesn't. Finishes the add when none is left or a check fails.
    fn next_hardware_add_confirmation(
        &mut self,
        run: u64,
        generation: u64,
        lease: &Weak<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<HardwareDeviceResult<ConfirmedHardwarePublicAccount>> {
        if !self.hardware_add_current(run, generation, lease) {
            return None;
        }
        let (store, view_session) = (self.vault_store.clone()?, self.view_session.clone()?);
        let (device_kind, descriptor) = match self.next_hardware_add_index(&store, &view_session) {
            Ok(Some(next)) => next,
            Ok(None) => {
                self.finish_hardware_add(None, window, cx);
                return None;
            }
            Err(error) => {
                self.finish_hardware_add(Some(error.as_str()), window, cx);
                return None;
            }
        };
        let Some(request) =
            self.hardware_device_request(device_kind, &view_session, None, window, cx)
        else {
            self.finish_hardware_add(
                Some("Unlock the matching hardware profile before adding addresses"),
                window,
                cx,
            );
            return None;
        };
        cx.notify();
        Some(
            self.hardware_public_account_device()
                .confirm_account(request, descriptor),
        )
    }

    /// Takes the next index to confirm, recording any skipped on the way.
    fn next_hardware_add_index(
        &mut self,
        store: &DesktopVaultStore,
        view_session: &DesktopViewSession,
    ) -> Result<Option<(HardwareDeviceKind, HardwarePublicAccountDescriptor)>, String> {
        let Some(preview) = self.public_form.derive_preview.preview.as_mut() else {
            return Ok(None);
        };
        let (Some(identity), Some(adding)) = (preview.hardware.as_ref(), preview.adding.as_mut())
        else {
            return Ok(None);
        };
        while let Some(&index) = adding.indexes.get(adding.next) {
            adding.next += 1;
            let skip = store
                .previewed_public_account_skip(view_session, &preview.keys, index)
                .map_err(|error| {
                    tracing::warn!(
                        error_kind = vault_error_kind(&error),
                        "hardware public account skip check failed"
                    );
                    vault_error_message(&error).to_string()
                })?;
            if let Some(skip) = skip {
                adding.skipped.push((index, skip));
                continue;
            }
            let address = preview
                .keys
                .address(index)
                .map_err(|error| vault_error_message(&error).to_string())?;
            let descriptor = HardwarePublicAccountDescriptor::for_wallet_public_index(
                identity.device_kind,
                view_session.derivation_index(),
                index,
            )
            .map_err(|error| error.to_string())?;
            adding.confirming = Some((index, address));
            return Ok(Some((identity.device_kind, descriptor)));
        }
        Ok(None)
    }

    /// Stores a confirmed account while its add is current, and reports whether the add goes
    /// on. A rejection, a device error or an address other than the preview's stops it.
    fn record_hardware_add_confirmation(
        &mut self,
        run: u64,
        generation: u64,
        lease: &Weak<Cell<bool>>,
        result: Result<
            Result<(ConfirmedHardwarePublicAccount, HardwareProfileSession), String>,
            tokio::task::JoinError,
        >,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !self.hardware_add_current(run, generation, lease) {
            return false;
        }
        let confirming = self
            .public_form
            .derive_preview
            .preview
            .as_mut()
            .and_then(|preview| preview.adding.as_mut()?.confirming.take());
        let (Some((index, address)), Some(store), Some(view_session)) = (
            confirming,
            self.vault_store.clone(),
            self.view_session.clone(),
        ) else {
            return false;
        };
        let (confirmed, session) = match result {
            Ok(Ok(confirmed)) => confirmed,
            Ok(Err(error)) => {
                tracing::warn!(%error, "hardware public account confirmation failed");
                self.finish_hardware_add(Some(error.as_str()), window, cx);
                self.discard_active_trezor_session_if_stale(&error, cx);
                return false;
            }
            Err(error) => {
                tracing::warn!(%error, "hardware public account confirmation task failed");
                self.finish_hardware_add(
                    Some("Confirming the address failed. Try again."),
                    window,
                    cx,
                );
                return false;
            }
        };
        if confirmed.address() != address {
            let error = format!(
                "The device showed a different address than the preview for index {index}. It was not added."
            );
            self.finish_hardware_add(Some(error.as_str()), window, cx);
            return false;
        }
        match store.add_hardware_public_account(&view_session, &confirmed, None) {
            Ok(account) => {
                if let Some(adding) = self
                    .public_form
                    .derive_preview
                    .preview
                    .as_mut()
                    .and_then(|preview| preview.adding.as_mut())
                {
                    adding.added.push(account);
                }
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    "add hardware public account failed"
                );
                self.finish_hardware_add(Some(vault_error_message(&error).as_ref()), window, cx);
                return false;
            }
        }
        #[cfg(feature = "hardware")]
        self.refresh_active_hardware_profile_session(session, cx);
        #[cfg(not(feature = "hardware"))]
        let _ = session;
        self.reload_public_accounts(window, cx);
        cx.notify();
        true
    }

    /// Ends the hardware add, reports what it added and skipped, and leaves the indexes it
    /// didn't reach selected. `error` is why it stopped early.
    fn finish_hardware_add(
        &mut self,
        error: Option<&str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(preview) = self.public_form.derive_preview.preview.as_mut() else {
            return;
        };
        let Some(adding) = preview.adding.take() else {
            return;
        };
        let added: Vec<u32> = adding
            .added
            .iter()
            .filter_map(|account| account.derivation_index)
            .collect();
        let done = |index: &u32| {
            added.contains(index) || adding.skipped.iter().any(|(skipped, _)| skipped == index)
        };
        preview.selected.retain(|index| !done(index));
        let not_added: Vec<u32> = adding
            .indexes
            .iter()
            .copied()
            .filter(|index| !done(index))
            .collect();
        self.public_form.error =
            error.map(|error| Arc::from(hardware_add_stopped_message(error, &added, &not_added)));
        let uuids = adding
            .added
            .iter()
            .map(|account| account.public_account_uuid.clone())
            .collect();
        preview.last_add = Some(PreviewedPublicAccountsAdded {
            added: adding.added,
            skipped: adding.skipped,
        });
        self.clear_trezor_pin_matrix_prompt(cx);
        self.show_added_public_accounts(uuids, window, cx);
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    fn trezor_pin_matrix_prompt(&self, root: &Entity<Self>) -> Option<gpui::AnyElement> {
        self.hardware_profile_unlock
            .trezor_pin_matrix_prompt
            .as_ref()
            .map(|prompt| {
                crate::root::vault_ui::render_trezor_pin_matrix_prompt(root, prompt)
                    .into_any_element()
            })
    }

    #[cfg(not(feature = "hardware"))]
    #[allow(clippy::unused_self)]
    const fn trezor_pin_matrix_prompt(&self, _root: &Entity<Self>) -> Option<gpui::AnyElement> {
        None
    }

    /// A hardware wallet's authorize state: no password, since the addresses come from the
    /// device.
    fn render_hardware_derive_authorize(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
        device_kind: HardwareDeviceKind,
        error: Option<Alert>,
    ) -> gpui::Div {
        let state = &self.public_form.derive_preview;
        let show = {
            let root = root.clone();
            move |window: &mut Window, cx: &mut App| {
                root.update(cx, |root, cx| {
                    root.add_public_derived_account_from_input(window, cx);
                });
            }
        };
        let show_on_enter = show.clone();
        div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "public-derive-hardware-authorize".into())
            .track_focus(&state.focus)
            .on_action(move |_: &Confirm, window, cx| {
                cx.stop_propagation();
                show_on_enter(window, cx);
            })
            .child(app_muted_text(hardware_public_account_setup_copy(device_kind)))
            .when(self.current_session_needs_trezor_app_passphrase(), |this| {
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
                            .border_color(gpui::rgb(ui::theme::BORDER))
                            .bg(gpui::rgb(ui::theme::SURFACE))
                            .child(ui::controls::app_strong_text("Trezor app passphrase"))
                            .child(
                                app_muted_text(
                                    "If the Trezor session expired, enter the app passphrase for this account request.",
                                )
                                .whitespace_normal(),
                            )
                            .child(input_enter_scope(!state.authorizing, show.clone()).child(
                                ui::controls::app_masked_input(
                                    &self.trezor_app_passphrase_input,
                                    state.authorizing,
                                ),
                            )),
                    )
                }
                #[cfg(not(feature = "hardware"))]
                {
                    this
                }
            })
            .children(self.trezor_pin_matrix_prompt(root))
            .children(error)
            .child(
                app_button(
                    "wallet-public-add-derived-submit",
                    if state.authorizing {
                        format!("Checking {}...", hardware_public_device_label(device_kind))
                    } else {
                        "Show addresses".to_owned()
                    },
                )
                .primary()
                .small()
                .loading(state.authorizing)
                .disabled(state.authorizing)
                .on_click(move |_event, window, cx| show(window, cx)),
            )
    }

    pub(super) fn render_derive_preview_content(
        &self,
        root: Entity<Self>,
        content_width: Pixels,
        device_auth_lease: Weak<Cell<bool>>,
    ) -> gpui::Div {
        let error = self.public_form.error.as_ref().map(|message| {
            Alert::error("wallet-public-add-derived-error", message.to_string()).small()
        });
        let state = &self.public_form.derive_preview;
        let Some(preview) = state.preview.as_ref() else {
            if let Some(device_kind) = self.selected_hardware_public_device_kind() {
                return self.render_hardware_derive_authorize(
                    &root,
                    content_width,
                    device_kind,
                    error,
                );
            }
            let busy = state.authorizing || self.device_auth_in_progress;
            return div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .debug_selector(|| "public-derive-authorize".into())
                .child(app_muted_text(
                    "Enter the vault password to show the Public addresses derived from the selected Private wallet. Nothing is saved until you add accounts.",
                ))
                .child(
                    masked_input_with_device_auth(
                        &self.public_form.add_password_input,
                        busy,
                        self.public_account_device_auth_buttons(
                            root.clone(),
                            PublicAccountDialogKind::Derive,
                            "wallet-public-add-derived-touch-id",
                            state.authorizing,
                            device_auth_lease,
                        ),
                    )
                    .debug_selector(|| "public-derive-authorize-password".into()),
                )
                .children(error)
                .child(
                    app_button(
                        "wallet-public-add-derived-submit",
                        if state.authorizing {
                            "Authorizing..."
                        } else {
                            "Show addresses"
                        },
                    )
                    .primary()
                    .small()
                    .loading(state.authorizing)
                    .disabled(busy)
                    .on_click(move |_event, window, cx| {
                        root.update(cx, |root, cx| {
                            root.add_public_derived_account_from_input(window, cx);
                        });
                    }),
                );
        };

        let statuses = self.derived_index_statuses();
        let selected = preview.selected.len();
        let at_cap = selected >= DerivedPublicAccountPreview::MAX_ADD_INDEXES;
        // A hardware add holds the selection, paging and Add until it ends.
        let adding = preview.adding.as_ref();
        let busy = adding.is_some();
        let can_add = selected > 0 && !busy;
        let start = preview.page_start;
        let rows = preview.keys.page(start).unwrap_or_default();
        let end = rows.last().map_or(start, |(index, _)| *index);
        let mut page_available = rows
            .iter()
            .map(|(index, _)| *index)
            .filter(|index| !statuses.contains_key(index))
            .peekable();
        let page_has_available = page_available.peek().is_some();
        let page_selected =
            page_has_available && page_available.all(|index| preview.selected.contains(&index));
        let add = {
            let root = root.clone();
            move |window: &mut Window, cx: &mut App| {
                root.update(cx, |root, cx| {
                    root.add_selected_derived_public_accounts(window, cx);
                });
            }
        };
        let add_on_enter = add.clone();
        let go_to = {
            let root = root.clone();
            move |window: &mut Window, cx: &mut App| {
                root.update(cx, |root, cx| root.go_to_derive_preview_index(window, cx));
            }
        };
        let go_to_on_enter = go_to.clone();
        let chain = self.effective_chain_configs.get(preview.chain_id);
        let check = {
            let root = root.clone();
            move |window: &mut Window, cx: &mut App| {
                root.update(cx, |root, cx| root.start_derive_balance_check(window, cx));
            }
        };
        let review_button = |id: &'static str, label: &'static str, open: bool| {
            let root = root.clone();
            app_button(id, label)
                .small()
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        root.review_derive_balance_check(open, window, cx);
                    });
                })
        };
        if let (true, Some(chain)) = (preview.reviewing_check, chain) {
            let (summary, warning) =
                balance_check_summary(chain, self.http.network_mode(), &preview.selected);
            let check_on_enter = check.clone();
            return div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .debug_selector(|| "public-derive-check-summary".into())
                .track_focus(&state.focus)
                .on_action(move |_: &Confirm, window, cx| {
                    cx.stop_propagation();
                    check_on_enter(window, cx);
                })
                .child(app_muted_text(
                    "Checks the native balance of each selected address. Nothing is added or saved.",
                ))
                .child(
                    DescriptionList::horizontal()
                        .small()
                        .bordered(false)
                        .columns(1)
                        .label_width(rems(8.))
                        .children(summary.into_iter().map(|(label, value)| {
                            DescriptionItem::new(label).value(app_text(value).into_any_element())
                        })),
                )
                .child(Alert::warning("wallet-public-derive-check-warning", warning).small())
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(review_button(
                            "wallet-public-derive-check-cancel",
                            "Cancel",
                            false,
                        ))
                        .child(
                            app_button("wallet-public-derive-check-confirm", "Check balances")
                                .primary()
                                .small()
                                .on_click(move |_event, window, cx| check(window, cx)),
                        ),
                );
        }
        let progress = preview.checking.as_ref().map(|indexes| {
            let done = indexes
                .iter()
                .filter(|index| {
                    matches!(
                        preview.results.get(&(preview.chain_id, **index)),
                        Some(BalanceCheck::Done(_) | BalanceCheck::Failed)
                    )
                })
                .count();
            app_muted_text(format!("Checked {done} of {}", indexes.len()))
        });
        let check_action = if preview.checking.is_some() {
            let root = root.clone();
            app_button("wallet-public-derive-check-stop", "Stop")
                .small()
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| root.stop_derive_balance_check(window, cx));
                })
        } else {
            review_button("wallet-public-derive-check", "Check native balances", true)
                .disabled(chain.is_none() || !can_add)
        };
        let page_button =
            |id: &'static str, icon: IconName, label: &'static str, target: u32, disabled: bool| {
                let root = root.clone();
                super::components::public_account_icon_button(id, icon, label)
                    .disabled(disabled || busy)
                    .on_click(move |_event, window, cx| {
                        root.update(cx, |root, cx| {
                            root.show_derive_preview_page(target, window, cx);
                        });
                    })
            };

        div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "public-derive-preview".into())
            .track_focus(&state.focus)
            .on_action(move |_: &Confirm, window, cx| {
                cx.stop_propagation();
                if can_add {
                    add_on_enter(window, cx);
                }
            })
            .child(app_muted_text(
                "Select the addresses to add as Public accounts.",
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(page_button(
                        "wallet-public-derive-previous",
                        IconName::ChevronLeft,
                        "Previous page",
                        start.saturating_sub(PAGE_SIZE),
                        start == 0,
                    ))
                    .child(page_button(
                        "wallet-public-derive-next",
                        IconName::ChevronRight,
                        "Next page",
                        start.saturating_add(PAGE_SIZE),
                        start.saturating_add(PAGE_SIZE) >= INDEX_LIMIT,
                    ))
                    .child(app_muted_text(format!("{start}–{end}")))
                    .child(div().flex_1())
                    // Wraps as one unit in a very narrow window instead of shrinking.
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .child(
                                input_enter_scope(!busy, go_to_on_enter)
                                    .w(px(112.0))
                                    .child(app_input(&state.go_to_input).small().disabled(busy)),
                            )
                            .child(
                                app_button("wallet-public-derive-go-to", "Go")
                                    .small()
                                    .disabled(busy)
                                    .on_click(move |_event, window, cx| go_to(window, cx)),
                            ),
                    ),
            )
            .children(preview.go_to_error.as_ref().map(|message| {
                Alert::error("wallet-public-derive-go-to-error", message.to_string()).small()
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        chain_select(&preview.chain_select)
                            .small()
                            .w(px(180.0))
                            .menu_width(px(240.0)),
                    )
                    .child(div().flex_1())
                    .children(progress)
                    .child(check_action),
            )
            .child(div().flex().child({
                let root = root.clone();
                Checkbox::new("wallet-public-derive-select-page")
                    .label("Select all on page")
                    .checked(page_selected)
                    .small()
                    .disabled(busy || !page_has_available || (at_cap && !page_selected))
                    .on_click(move |checked, window, cx| {
                        let checked = *checked;
                        root.update(cx, |root, cx| {
                            root.select_derive_preview_page(checked, window, cx);
                        });
                    })
            }))
            // While the device confirms, its panel takes the rows' place so the address to
            // compare stays in view.
            .when(adding.is_none(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(rows.into_iter().map(|(index, address)| {
                            let checked = preview.selected.contains(&index);
                            // Clicking the index or address follows the checkbox's rules.
                            let selectable =
                                (checked || !at_cap) && !statuses.contains_key(&index) && !busy;
                            let status = match statuses.get(&index) {
                                Some(PublicAccountStatus::Active) => {
                                    app_muted_text("Added").into_any_element()
                                }
                                Some(PublicAccountStatus::Inactive) => {
                                    app_muted_text("Inactive").into_any_element()
                                }
                                None => {
                                    let root = root.clone();
                                    Checkbox::new(("wallet-public-derive-select", index))
                                        .checked(checked)
                                        .small()
                                        .disabled(busy || (at_cap && !checked))
                                        .on_click(move |checked, window, cx| {
                                            let checked = *checked;
                                            root.update(cx, |root, cx| {
                                                root.toggle_derive_preview_index(
                                                    index, checked, window, cx,
                                                );
                                            });
                                        })
                                        .into_any_element()
                                }
                            };
                            let balance =
                                preview
                                    .results
                                    .get(&(preview.chain_id, index))
                                    .map(|check| match check {
                                        BalanceCheck::Queued => app_muted_text("Queued"),
                                        BalanceCheck::Loading => app_muted_text("Checking…"),
                                        BalanceCheck::Done(amount) => app_text(chain.map_or_else(
                                            || amount.to_string(),
                                            |chain| chain.native_currency.format_amount(*amount),
                                        )),
                                        BalanceCheck::Failed => app_muted_text("Failed"),
                                    });
                            let copied =
                                SharedString::from(super::public_address_qr_payload(address));
                            let toggle_root = root.clone();
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .id(("wallet-public-derive-row", index))
                                        .debug_selector(move || {
                                            format!("public-derive-row-{index}")
                                        })
                                        .flex()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .items_center()
                                        .gap_3()
                                        .when(selectable, |area| {
                                            area.cursor_pointer().on_click(
                                                move |_event, window, cx| {
                                                    toggle_root.update(cx, |root, cx| {
                                                        root.toggle_derive_preview_index(
                                                            index, !checked, window, cx,
                                                        );
                                                    });
                                                },
                                            )
                                        })
                                        .child(
                                            app_muted_text(index.to_string())
                                                .w(px(88.0))
                                                .flex_none(),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .items_center()
                                                .gap_1()
                                                .child(
                                                    app_text(short_address(&address))
                                                        .font_family(
                                                            ui::theme::APP_MONO_FONT_FAMILY,
                                                        )
                                                        .flex_none(),
                                                )
                                                .child(
                                                    super::components::public_account_icon_button(
                                                        ("wallet-public-derive-copy", index),
                                                        IconName::Copy,
                                                        "Copy address",
                                                    )
                                                    .on_click(move |_event, window, cx| {
                                                        cx.stop_propagation();
                                                        ui::clipboard::copy_to_clipboard_with_toast(
                                                            copied.clone(),
                                                            window,
                                                            cx,
                                                        );
                                                    }),
                                                ),
                                        ),
                                )
                                .child(
                                    div().w(px(96.0)).flex_none().flex().justify_end().children(
                                        balance.map(|text| text.min_w(px(0.0)).truncate()),
                                    ),
                                )
                                .child(div().w(px(64.0)).flex().justify_end().child(status))
                        })),
                )
            })
            .children(adding.and_then(|adding| {
                let (index, address) = adding.confirming?;
                let device_kind = preview.hardware.as_ref()?.device_kind;
                Some(render_hardware_public_account_confirmation_wait(
                    device_kind,
                    index,
                    adding.next,
                    adding.indexes.len(),
                    address,
                ))
            }))
            .children(self.trezor_pin_matrix_prompt(&root))
            .children(preview.last_add.as_ref().map(|result| {
                let message = add_result_message(result);
                if result.skipped.is_empty() {
                    Alert::success("wallet-public-derive-added", message).small()
                } else {
                    Alert::warning("wallet-public-derive-added", message).small()
                }
            }))
            .children(error)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        app_muted_text(format!(
                            "{selected} of {} selected",
                            DerivedPublicAccountPreview::MAX_ADD_INDEXES
                        ))
                        .flex_1(),
                    )
                    .child(
                        app_button(
                            "wallet-public-derive-add",
                            format!("Add {}", count_label(selected, "account")),
                        )
                        .primary()
                        .small()
                        .loading(busy)
                        .disabled(!can_add)
                        .on_click(move |_event, window, cx| add(window, cx)),
                    ),
            )
    }
}
