//! Public Tron accounts in the Public tab: their state, balance refreshes, and the add, import
//! and receive flows. The shared Public list shows them through the `listed_*` accessors as
//! `PublicAccountMetadata` rows, which never enter `public_accounts`, so no EVM flow sees them.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Weak;
use std::sync::Arc;
use std::time::SystemTime;

use gpui::{
    Context, Entity, InteractiveElement as _, ParentElement, Pixels, Styled, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{Disableable, Sizable, WindowExt, alert::Alert, button::ButtonVariants};
use ui::controls::{app_button, app_muted_text};
use wallet_ops::{
    PublicAccountBalance, PublicBalanceCache, PublicBalanceRefreshTicket, PublicBalanceSnapshot,
    RpcChainRoute,
    settings::{NativeUsdPricing, TronSettings},
    tron::{
        TRON_CHAIN_ID,
        address::encode_tron_address,
        balances::{TronAccountBalance, TronAccountResources, refresh_tron_balances_at_least},
        tron_rpc_route,
    },
    vault::{
        PublicAccountMetadata, PublicAccountSource, PublicAccountStatus, TronAccountMetadata,
        TronAccountSource, VaultError, public_account_default_label,
    },
};
use zeroize::Zeroizing;

use super::tron_send::TronSendState;
use super::{next_public_account_label_number, public_account_display_label};
use crate::root::device_auth::masked_input_with_device_auth;
use crate::root::dialogs::PublicAccountDialogKind;
use crate::root::public_action::format_gas_limit;
use crate::root::vault::vault_error_kind;
use crate::root::{SelectedNetwork, WalletRoot, format_report_chain};

/// Tron accounts, their balances and Tron's route for the current settings.
pub(in crate::root) struct TronPublicState {
    /// The installed Tron settings.
    pub(in crate::root) settings: TronSettings,
    /// Built once per installed settings, so its clones share endpoint identity checks.
    pub(in crate::root) route: Option<RpcChainRoute>,
    /// Tron has been selected this session. Until then nothing reads from Tron.
    pub(in crate::root) activated: bool,
    /// The current identity warmup has activated `route`.
    pub(in crate::root) route_warmed: bool,
    pub(in crate::root) accounts: Vec<TronAccountMetadata>,
    /// `accounts` as rows of the shared Public list.
    pub(in crate::root) listed: Vec<PublicAccountMetadata>,
    pub(in crate::root) selected: Option<Arc<str>>,
    /// Apart from the EVM cache, so a custom EVM chain with Tron's chain id never shares it.
    pub(in crate::root) balance_cache: PublicBalanceCache,
    pub(in crate::root) snapshot: Option<Arc<PublicBalanceSnapshot>>,
    /// Each account's resources from its latest accepted refresh, by `tron_account_uuid`.
    /// `None` when that read failed; an account without an entry hasn't been read.
    pub(in crate::root) resources: HashMap<String, Option<TronAccountResources>>,
    pub(in crate::root) error: Option<Arc<str>>,
    pub(in crate::root) inactive_error: Option<Arc<str>>,
    pub(in crate::root) send: TronSendState,
}

impl TronPublicState {
    pub(in crate::root) fn new(settings: TronSettings, send: TronSendState) -> Self {
        Self {
            route: tron_rpc_route(&settings),
            settings,
            activated: false,
            route_warmed: false,
            accounts: Vec::new(),
            listed: Vec::new(),
            selected: None,
            balance_cache: PublicBalanceCache::default(),
            snapshot: None,
            resources: HashMap::new(),
            error: None,
            inactive_error: None,
            send,
        }
    }

    fn clear_wallet_state(&mut self) {
        self.accounts.clear();
        self.listed.clear();
        self.selected = None;
        self.balance_cache.clear();
        self.snapshot = None;
        self.resources.clear();
        self.error = None;
        self.inactive_error = None;
        self.send.form = None;
    }
}

/// Receive's note on a Tron account's address.
pub(in crate::root) const TRON_RECEIVE_WARNING: &str =
    "This is a Tron address. Send only TRX and Tron tokens to it, on Tron.";

/// A Tron account source as a source of the shared Public list.
pub(super) const fn tron_list_source(source: TronAccountSource) -> PublicAccountSource {
    match source {
        TronAccountSource::Derived { .. } => PublicAccountSource::Derived,
        TronAccountSource::HardwareDerived { .. } => PublicAccountSource::HardwareDerived,
        TronAccountSource::Imported => PublicAccountSource::Imported,
    }
}

/// A Tron account as a row of the shared Public list.
pub(super) fn tron_list_account(account: &TronAccountMetadata) -> PublicAccountMetadata {
    PublicAccountMetadata {
        public_account_uuid: account.tron_account_uuid.clone(),
        address: account.address,
        label: account.label.clone(),
        source: tron_list_source(account.source),
        scope: account.scope.clone(),
        derivation_index: account.source.derivation_index(),
        hardware_descriptor: None,
        status: account.status,
        display_order: account.display_order,
    }
}

impl WalletRoot {
    pub(in crate::root) fn tron_selected(&self) -> bool {
        self.selected_network == SelectedNetwork::Tron
    }

    /// The Public list's accounts: Tron's with Tron selected, otherwise the EVM accounts.
    pub(in crate::root) fn listed_public_accounts(&self) -> &[PublicAccountMetadata] {
        if self.tron_selected() {
            &self.tron_public.listed
        } else {
            &self.public_accounts
        }
    }

    pub(in crate::root) fn listed_public_chain(&self) -> u64 {
        if self.tron_selected() {
            TRON_CHAIN_ID
        } else {
            self.last_evm_chain
        }
    }

    pub(in crate::root) fn listed_public_snapshot(&self) -> Option<&PublicBalanceSnapshot> {
        if self.tron_selected() {
            self.tron_public.snapshot.as_deref()
        } else {
            self.public_balance_snapshot.as_deref()
        }
    }

    pub(in crate::root) fn listed_selected_uuid(&self) -> Option<&Arc<str>> {
        if self.tron_selected() {
            self.tron_public.selected.as_ref()
        } else {
            self.public_form.selected_account_uuid.as_ref()
        }
    }

    pub(in crate::root) fn set_listed_selected_uuid(&mut self, uuid: Option<Arc<str>>) {
        if self.tron_selected() {
            self.tron_public.selected = uuid;
        } else {
            self.public_form.selected_account_uuid = uuid;
        }
    }

    pub(in crate::root) fn listed_account_for_uuid(
        &self,
        uuid: Option<&str>,
    ) -> Option<&PublicAccountMetadata> {
        if !self.tron_selected() {
            return self.public_account_for_uuid(uuid);
        }
        let uuid = uuid?;
        self.tron_public
            .listed
            .iter()
            .find(|account| account.public_account_uuid == uuid)
    }

    pub(in crate::root) fn listed_selected_account(&self) -> Option<&PublicAccountMetadata> {
        self.listed_account_for_uuid(self.listed_selected_uuid().map(AsRef::as_ref))
    }

    /// The full address as the list shows and copies it: base58 on Tron, hex otherwise.
    pub(in crate::root) fn listed_address(&self, account: &PublicAccountMetadata) -> String {
        if self.tron_selected() {
            encode_tron_address(account.address)
        } else {
            super::public_address_qr_payload(account.address)
        }
    }

    pub(in crate::root) fn listed_short_address(&self, account: &PublicAccountMetadata) -> String {
        if self.tron_selected() {
            railgun_ui::short_non_evm(&encode_tron_address(account.address))
        } else {
            railgun_ui::short_address(&account.address)
        }
    }

    pub(in crate::root) fn listed_account_matches_search(
        &self,
        account: &PublicAccountMetadata,
    ) -> bool {
        if !self.tron_selected() {
            return super::public_account_matches_search(account, &self.public_form.search_query);
        }
        super::components::account_matches_search(
            account,
            &encode_tron_address(account.address),
            &self.public_form.search_query,
        )
    }

    pub(in crate::root) fn listed_refreshing_accounts(&self) -> BTreeSet<String> {
        if !self.tron_selected() {
            return self.public_refreshing_accounts();
        }
        self.tron_balance_scope()
            .map(|scope| self.tron_public.balance_cache.refreshing_accounts(&scope))
            .unwrap_or_default()
    }

    pub(in crate::root) fn listed_failed_accounts(&self) -> BTreeSet<String> {
        if !self.tron_selected() {
            return self.public_failed_accounts();
        }
        self.tron_balance_scope()
            .map(|scope| self.tron_public.balance_cache.failed_accounts(&scope))
            .unwrap_or_default()
    }

    pub(in crate::root) fn listed_balance_error(
        &self,
        status: PublicAccountStatus,
    ) -> Option<&str> {
        let (active, inactive) = if self.tron_selected() {
            (&self.tron_public.error, &self.tron_public.inactive_error)
        } else {
            (
                &self.public_balance_error,
                &self.public_inactive_balance_error,
            )
        };
        match status {
            PublicAccountStatus::Active => active.as_deref(),
            PublicAccountStatus::Inactive => inactive.as_deref(),
        }
    }

    /// A Tron row's resource line; `None` until the account's resources have been read.
    pub(super) fn tron_resources_line(&self, uuid: &str) -> Option<String> {
        Some(match self.tron_public.resources.get(uuid)? {
            None => "Resources unavailable".to_owned(),
            Some(TronAccountResources::NotOnChain) => {
                "Not on chain. Receive TRX before sending.".to_owned()
            }
            Some(TronAccountResources::OnChain(resources)) => {
                let staked = if resources.staked_bandwidth == 0 {
                    String::new()
                } else {
                    format!("{} staked, ", format_gas_limit(resources.staked_bandwidth))
                };
                format!(
                    "Energy {} · Bandwidth {staked}{} free",
                    format_gas_limit(resources.energy),
                    format_gas_limit(resources.free_bandwidth)
                )
            }
        })
    }

    pub(in crate::root) const fn tron_has_pricing(&self) -> bool {
        self.tron_public.route.is_some()
            && !matches!(
                self.tron_public.settings.native_usd,
                NativeUsdPricing::Disabled
            )
    }

    /// Tron's balance authority, once Tron has been selected this session.
    pub(super) fn tron_balance_scope(&self) -> Option<wallet_ops::PublicBalanceScope> {
        if !self.tron_public.activated {
            return None;
        }
        self.public_balance_scope_for_route(self.tron_public.route.clone()?)
    }

    pub(in crate::root) fn clear_tron_wallet_state(&mut self) {
        self.tron_public.clear_wallet_state();
    }

    /// Installs saved Tron settings. An unchanged route keeps its instance and identity checks;
    /// a changed one drops the balances read over the old one.
    pub(in crate::root) fn install_tron_settings(&mut self, settings: &TronSettings) {
        let route = tron_rpc_route(settings);
        if route != self.tron_public.route {
            self.tron_public.route = route;
            self.tron_public.route_warmed = false;
            self.tron_public.balance_cache.clear();
            self.tron_public.snapshot = None;
            self.tron_public.resources.clear();
        }
        self.tron_public.settings = settings.clone();
        self.public_broadcaster_anchor_refresh
            .reconcile_tron(settings, self.tron_public.route.as_ref());
        self.sync_active_pricing_chains();
    }

    /// Selecting Tron starts its pricing, route warmup and balance reads for the session.
    pub(in crate::root) fn activate_tron(&mut self) {
        self.tron_public.activated = true;
        self.sync_active_pricing_chains();
    }

    /// Reloads Tron accounts from the vault. Listing them reads nothing from the network.
    pub(in crate::root) fn reload_tron_accounts(&mut self) {
        #[cfg(debug_assertions)]
        if self.stage_tron_ui_fixture() {
            return;
        }
        let accounts = match (self.vault_store.as_ref(), self.view_session.as_ref()) {
            (Some(store), Some(view_session)) => {
                match store.list_tron_accounts_for_session(view_session.as_ref(), true) {
                    Ok(accounts) => accounts,
                    Err(error) => {
                        tracing::warn!(
                            error_kind = vault_error_kind(&error),
                            "load Tron accounts failed"
                        );
                        self.public_form.error = Some(Arc::from(error.to_string()));
                        return;
                    }
                }
            }
            _ => Vec::new(),
        };
        let listed: Vec<_> = accounts.iter().map(tron_list_account).collect();
        if listed != self.tron_public.listed {
            self.tron_public.balance_cache.retain_accounts(&listed);
        }
        if !listed.iter().any(|account| {
            Some(account.public_account_uuid.as_str()) == self.tron_public.selected.as_deref()
        }) {
            self.tron_public.selected = None;
        }
        self.tron_public.accounts = accounts;
        self.tron_public.listed = listed;
        self.restore_tron_balance_snapshot();
    }

    fn restore_tron_balance_snapshot(&mut self) {
        self.tron_public.snapshot = self
            .tron_balance_scope()
            .and_then(|scope| self.tron_public.balance_cache.snapshot(&scope))
            .map(Arc::new);
    }

    /// Exactly the named Tron accounts, ignoring the toggle and exclusions. Nothing is read
    /// before Tron has been selected this session.
    pub(in crate::root) fn schedule_tron_balance_refresh<I, S>(
        &self,
        accounts: I,
        cx: &mut Context<'_, Self>,
    ) where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        // The UI fixture's balances are its own and are never read.
        #[cfg(debug_assertions)]
        if crate::root::private_swap::ui_fixture_tron().is_some() {
            return;
        }
        let Some(scope) = self.tron_balance_scope() else {
            return;
        };
        let cache = &self.tron_public.balance_cache;
        cache.set_scope(scope.clone());
        if let Some(ticket) = cache.begin_refresh(&scope, accounts) {
            self.run_tron_balance_refresh(ticket, cx);
        }
        cx.notify();
    }

    fn run_tron_balance_refresh(&self, ticket: PublicBalanceRefreshTicket, cx: &Context<'_, Self>) {
        let cache = self.tron_public.balance_cache.clone();
        // A changed wallet or route replaces the namespace, so the ticket isn't current.
        let current = self.tron_balance_scope().is_some_and(|scope| {
            cache.set_scope(scope);
            cache.is_current_scope(ticket.scope())
        });
        let account = self
            .tron_public
            .accounts
            .iter()
            .find(|account| ticket.includes_account(&account.tron_account_uuid))
            .cloned();
        let (true, Some(account), Some(route)) = (current, account, self.tron_public.route.clone())
        else {
            if let Some(follow_up) = cache.finish_refresh(ticket, None).follow_up {
                self.run_tron_balance_follow_up(follow_up, cx);
            }
            return;
        };
        let http = self.http.clone();
        let registry = self.effective_token_registry.clone();
        let minimum = ticket.minimum_block();
        let join = self.runtime.spawn(async move {
            refresh_tron_balances_at_least(&account, &route, &registry, &http, minimum).await
        });
        cx.spawn(async move |this, cx| {
            let result = join.await;
            Self::apply_balance_ticket(&this, cx, &cache, ticket, |root, ticket, cx| {
                root.apply_tron_balance_refresh_result(ticket, result, cx);
            });
        })
        .detach();
    }

    /// Runs a follow-up after a random 0.5-3 s, as EVM follow-ups do.
    fn run_tron_balance_follow_up(
        &self,
        ticket: PublicBalanceRefreshTicket,
        cx: &Context<'_, Self>,
    ) {
        Self::run_balance_follow_up(
            self.tron_public.balance_cache.clone(),
            ticket,
            cx,
            |root, ticket, cx| root.run_tron_balance_refresh(ticket, cx),
        );
    }

    pub(in crate::root) fn apply_tron_balance_refresh_result(
        &mut self,
        ticket: PublicBalanceRefreshTicket,
        result: Result<eyre::Result<TronAccountBalance>, tokio::task::JoinError>,
        cx: &mut Context<'_, Self>,
    ) {
        let status = self
            .tron_public
            .accounts
            .iter()
            .find(|account| ticket.includes_account(&account.tron_account_uuid))
            .map(|account| account.status);
        let resources = match &result {
            Ok(Ok(read)) => Some((read.account.tron_account_uuid.clone(), read.resources)),
            _ => None,
        };
        let (snapshot, error) = match result {
            Ok(Ok(read)) => (
                Some(PublicBalanceSnapshot {
                    chain_id: TRON_CHAIN_ID,
                    refreshed_at: SystemTime::now(),
                    accounts: vec![PublicAccountBalance {
                        account: tron_list_account(&read.account),
                        balances: read.balances,
                        observed_at: Some(read.observed_at),
                        observed_block: Some(read.observed_block),
                    }],
                }),
                None,
            ),
            Ok(Err(error)) => (None, Some(format_report_chain(&error))),
            Err(error) => (None, Some(format!("Tron balance refresh failed: {error}"))),
        };
        let cache = self.tron_public.balance_cache.clone();
        let scope = ticket.scope().clone();
        let completion = cache.finish_refresh(ticket, snapshot);
        if completion.accepted {
            // Keep the focused tile and any open asset menu across the refresh, as on EVM.
            let focused = self.tron_selected().then(|| self.public_focused_asset());
            self.tron_public.snapshot = cache.snapshot(&scope).map(Arc::new);
            if let Some((uuid, resources)) = resources {
                self.tron_public.resources.insert(uuid, resources);
            }
            if let Some(focused) = focused
                && self.revalidate_public_asset_focus(focused)
            {
                // The closed menu held keyboard focus.
                let list_focus = self.public_form.list_focus.clone();
                let window = self.window_handle;
                cx.defer(move |cx| {
                    let _ = window.update(cx, |_, window, cx| list_focus.focus(window, cx));
                });
            }
            if let Some(error) = error {
                let error = Some(Arc::from(error));
                match status {
                    Some(PublicAccountStatus::Active) => self.tron_public.error = error,
                    Some(PublicAccountStatus::Inactive) => self.tron_public.inactive_error = error,
                    None => {}
                }
            }
        }
        if let Some(follow_up) = completion.follow_up {
            self.run_tron_balance_follow_up(follow_up, cx);
        }
        cx.notify();
    }

    fn tron_next_label(&self) -> String {
        public_account_default_label(next_public_account_label_number(
            self.tron_public.accounts.len(),
        ))
    }

    /// Lists and selects an added Tron account, then reads it if Tron is active.
    fn tron_account_added(
        &mut self,
        account: &TronAccountMetadata,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.tron_public.selected = Some(Arc::from(account.tron_account_uuid.as_str()));
        self.public_form.open_section = PublicAccountStatus::Active;
        self.reload_public_accounts(window, cx);
        self.schedule_tron_balance_refresh([account.tron_account_uuid.clone()], cx);
        window.close_all_dialogs(cx);
    }

    /// Adds the wallet's next derived Tron account: with the vault password for a software
    /// wallet, or with fresh device approval for a hardware-derived one.
    pub(super) fn add_derived_tron_account(
        &mut self,
        device_auth_password: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.importing_account {
            return;
        }
        if self.selected_hardware_public_device_kind().is_some() {
            self.add_hardware_derived_tron_account(window, cx);
            return;
        }
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            self.public_form.error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        let password = device_auth_password.unwrap_or_else(|| {
            Self::read_and_clear_input(&self.public_form.add_password_input, window, cx)
        });
        if password.trim().is_empty() {
            self.public_form.error = Some(Arc::from("Enter the vault password to add an account"));
            cx.notify();
            return;
        }
        self.public_form.error = None;
        let label = self.tron_next_label();
        match store.add_derived_tron_account(
            password.as_str(),
            view_session.as_ref(),
            Some(&label),
            self.protected_software_seed_session.as_deref(),
        ) {
            Ok(account) => self.tron_account_added(&account, window, cx),
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    fn add_hardware_derived_tron_account(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let prepared = match (
            self.selected_hardware_descriptor(),
            self.vault_store.clone(),
            self.view_session.clone(),
        ) {
            (None, _, _) => Err("Selected wallet is missing its hardware derivation descriptor"),
            (Some(descriptor), Some(store), Some(view_session)) => view_session
                .hardware_profile_session()
                .cloned()
                .map(|session| (descriptor, store, view_session.clone(), session))
                .ok_or("Unlock the matching hardware profile before adding an account"),
            _ => Err("Wallet vault is locked"),
        };
        let (descriptor, store, view_session, hardware_session) = match prepared {
            Ok(prepared) => prepared,
            Err(message) => {
                self.public_form.error = Some(Arc::from(message));
                cx.notify();
                return;
            }
        };
        let entropy = self.tron_hardware_entropy(hardware_session, &descriptor, window, cx);
        let label = self.tron_next_label();
        let generation = self.active_wallet_generation;
        self.public_form.importing_account = true;
        self.public_form.error = None;
        let join = self.runtime.spawn(async move {
            let (entropy, hardware_session) = entropy.await.map_err(|error| error.to_string())?;
            let account = store
                .add_hardware_derived_tron_account(
                    view_session.as_ref(),
                    &descriptor,
                    entropy.expose_secret(),
                    Some(&label),
                )
                .map_err(|error| error.to_string());
            // The synthetic seed lives only for this derivation.
            drop(entropy);
            Ok::<_, String>((account?, hardware_session))
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.public_form.importing_account = false;
                root.clear_trezor_pin_matrix_prompt(cx);
                if root.active_wallet_generation != generation {
                    return;
                }
                match result {
                    Ok(Ok((account, session))) => {
                        root.refresh_active_hardware_profile_session(session, cx);
                        root.tron_account_added(&account, window, cx);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!("hardware Tron account add failed");
                        root.discard_active_trezor_session_if_stale(&error, cx);
                        root.public_form.error = Some(Arc::from(error));
                    }
                    Err(_) => {
                        root.public_form.error =
                            Some(Arc::from("Adding the account failed. Try again."));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Takes the Trezor app passphrase and prepares any PIN prompt now. The returned future has
    /// the device approve the wallet's synthetic seed and yields it with the refreshed session.
    #[cfg(feature = "hardware")]
    pub(super) fn tron_hardware_entropy(
        &mut self,
        mut hardware_session: wallet_ops::vault::HardwareProfileSession,
        descriptor: &wallet_ops::hardware::HardwareDerivationDescriptor,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl Future<
        Output = eyre::Result<(
            wallet_ops::hardware::SyntheticRailgunEntropy,
            wallet_ops::vault::HardwareProfileSession,
        )>,
    > + Send
    + use<> {
        use crate::root::spend_authorization::{
            HardwareSpendAuthorizationError, hardware_synthetic_entropy,
        };
        use wallet_ops::hardware::HardwareDeviceKind;

        let trezor_app_passphrase =
            self.read_trezor_app_passphrase_for_hardware_session(&hardware_session, window, cx);
        let trezor_pin_matrix_provider = (hardware_session.device_kind
            == HardwareDeviceKind::Trezor)
            .then(|| self.trezor_pin_matrix_provider_for_operation(window, cx));
        let descriptor = descriptor.clone();
        async move {
            let entropy = hardware_synthetic_entropy(
                &mut hardware_session,
                &descriptor,
                trezor_app_passphrase,
                trezor_pin_matrix_provider,
            )
            .await
            .map_err(|error| match error {
                HardwareSpendAuthorizationError::Hardware(error) => {
                    eyre::eyre!("Hardware approval failed: {error}")
                }
                HardwareSpendAuthorizationError::Vault(error) => eyre::Report::new(error),
                HardwareSpendAuthorizationError::Executor(error) => eyre::eyre!(error),
            })?;
            Ok((entropy, hardware_session))
        }
    }

    #[cfg(not(feature = "hardware"))]
    fn add_hardware_derived_tron_account(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.error = Some(Arc::from(
            "This build has no hardware wallet support, so it can't add this account",
        ));
        cx.notify();
    }

    /// Imports `private_key` as a Tron account only. A duplicate names the existing account.
    pub(super) fn import_tron_account(
        &mut self,
        password: &str,
        private_key: &str,
        label: &str,
        global: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            return;
        };
        match store.import_tron_account(
            password,
            view_session.as_ref(),
            private_key,
            Some(label),
            global,
        ) {
            Ok(account) => {
                self.public_form
                    .import_label_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.public_form.import_global = false;
                self.tron_account_added(&account, window, cx);
            }
            Err(VaultError::DuplicatePublicAccountAddress) => {
                self.public_form.error = Some(self.tron_duplicate_import_message(private_key));
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
    }

    fn tron_duplicate_import_message(&self, private_key: &str) -> Arc<str> {
        let existing = wallet_ops::vault::parse_public_evm_private_key(private_key)
            .ok()
            .and_then(|key| wallet_ops::vault::public_evm_address_from_private_key(&key).ok())
            .and_then(|address| {
                self.tron_public.accounts.iter().find(|account| {
                    account.address == address && account.status == PublicAccountStatus::Active
                })
            });
        existing.map_or_else(
            || Arc::from("This key's Tron address is already an active Tron account."),
            |account| {
                let label = public_account_display_label(&tron_list_account(account))
                    .unwrap_or_else(|| encode_tron_address(account.address));
                Arc::from(format!("This key is already added on Tron as {label}."))
            },
        )
    }

    pub(in crate::root) fn set_tron_account_active(
        &mut self,
        uuid: &str,
        active: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            return;
        };
        let neighbour = self.public_account_neighbour(uuid);
        match store.set_derived_tron_account_active(view_session.as_ref(), uuid, active) {
            Ok(_) if active => {
                self.tron_public.selected = Some(Arc::from(uuid));
                self.reload_public_accounts(window, cx);
                self.schedule_tron_balance_refresh([uuid], cx);
            }
            Ok(_) => {
                if self.tron_public.selected.as_deref() == Some(uuid) {
                    self.tron_public.selected = neighbour;
                }
                self.reload_public_accounts(window, cx);
                self.reset_public_asset_focus();
                self.scroll_selected_public_row_into_view(window);
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    /// Derive from private on Tron: the next Tron account, with no address preview.
    pub(super) fn render_tron_derive_content(
        &self,
        root: Entity<Self>,
        content_width: Pixels,
        device_auth_lease: Weak<Cell<bool>>,
    ) -> gpui::Div {
        let busy = self.public_form.importing_account;
        let mut content = div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .debug_selector(|| "public-tron-add".into());
        content = match self.selected_hardware_public_device_kind() {
            None => content
                .child(
                    app_muted_text(
                        "Adds the next Tron account derived from this wallet's recovery phrase, \
                         on Tron's path m/44'/195'/0'/0.",
                    )
                    .whitespace_normal(),
                )
                .child(masked_input_with_device_auth(
                    &self.public_form.add_password_input,
                    self.device_auth_in_progress || busy,
                    self.public_account_device_auth_buttons(
                        root.clone(),
                        PublicAccountDialogKind::Derive,
                        "wallet-public-tron-add-touch-id",
                        busy,
                        device_auth_lease,
                    ),
                )),
            Some(device_kind) => content
                .child(
                    app_muted_text(format!(
                        "Approve on your {} to derive the next Tron account from this wallet's \
                         synthetic seed. This is hardware-derived software custody: the app \
                         holds the account's key, and each Send asks the device again.",
                        crate::root::vault::hardware_device_label(device_kind)
                    ))
                    .whitespace_normal(),
                )
                .when(self.current_session_needs_trezor_app_passphrase(), |this| {
                    #[cfg(feature = "hardware")]
                    {
                        this.child(super::derive_preview::trezor_app_passphrase_panel(
                            ui::controls::app_masked_input(&self.trezor_app_passphrase_input, busy),
                        ))
                    }
                    #[cfg(not(feature = "hardware"))]
                    {
                        this
                    }
                })
                .children(self.trezor_pin_matrix_prompt(&root)),
        };
        content
            .children(self.public_form.error.as_ref().map(|message| {
                Alert::error("wallet-public-tron-add-error", message.to_string()).small()
            }))
            .child(
                app_button(
                    "wallet-public-tron-add-submit",
                    if busy {
                        "Adding..."
                    } else {
                        "Add Tron account"
                    },
                )
                .primary()
                .small()
                .loading(busy)
                .disabled(busy || self.device_auth_in_progress)
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        root.add_public_derived_account_from_input(window, cx);
                    });
                }),
            )
    }
}
