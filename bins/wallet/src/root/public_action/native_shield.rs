//! Native shields in the Public Shield form: the route the payer can use, the background
//! preparation that review and submission of a stealth-route shield require, and its Max.
//! wallet-ops sends a stealth-route shield from a stealth account of the selected private
//! wallet. The Railoxide wrap fallback needs no preparation.

use std::time::Duration;

use super::*;

/// Typing settles before the shield is estimated.
const NATIVE_SHIELD_PREPARATION_DELAY: Duration = Duration::from_millis(400);

/// A Public Shield state staged by `RAILOXIDE_UI_FIXTURE`; see the fixture module doc.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PublicShieldUiFixture {
    EstimateError,
    FundingError,
    MimicPrepared,
    NativePrepared,
    MimicUnavailable,
    NativePending,
    Fallback,
    FallbackPending,
}

#[cfg(debug_assertions)]
impl PublicShieldUiFixture {
    /// The payer a staged native form classifies the scratch account as.
    const fn native_payer(self) -> Option<NativeShieldPayer> {
        match self {
            Self::MimicPrepared | Self::NativePrepared => Some(NativeShieldPayer::Software),
            Self::MimicUnavailable | Self::Fallback => Some(NativeShieldPayer::Hardware),
            _ => None,
        }
    }
}

#[cfg(debug_assertions)]
pub(in crate::root) fn public_shield_ui_fixture() -> Option<PublicShieldUiFixture> {
    static FIXTURE: std::sync::OnceLock<Option<PublicShieldUiFixture>> = std::sync::OnceLock::new();
    *FIXTURE.get_or_init(
        || match std::env::var("RAILOXIDE_UI_FIXTURE").ok()?.as_str() {
            "shield:estimate-error" => Some(PublicShieldUiFixture::EstimateError),
            "shield:funding-error" => Some(PublicShieldUiFixture::FundingError),
            "shield:mimic-prepared" => Some(PublicShieldUiFixture::MimicPrepared),
            "shield:native-prepared" => Some(PublicShieldUiFixture::NativePrepared),
            "shield:mimic-unavailable" => Some(PublicShieldUiFixture::MimicUnavailable),
            "shield:native-pending" => Some(PublicShieldUiFixture::NativePending),
            "shield:fallback" => Some(PublicShieldUiFixture::Fallback),
            "shield:fallback-pending" => Some(PublicShieldUiFixture::FallbackPending),
            _ => None,
        },
    )
}

/// The raw and buffered gas of the `shield:mimic-prepared` and `shield:native-prepared` fee
/// cards.
#[cfg(debug_assertions)]
const PUBLIC_SHIELD_UI_FIXTURE_NATIVE_GAS: (u64, u64) = (241_380, 289_656);

/// What a stealth-route native shield preparation needs besides its amount.
struct NativeShieldTarget {
    chain_id: u64,
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
    profile: PublicShieldTransactionProfile,
    payer: NativeShieldPayer,
    payer_address: Address,
    view_session: Arc<DesktopViewSession>,
    http: wallet_ops::HttpContext,
    operation: OperationHttpClientSlot,
}

impl NativeShieldTarget {
    async fn prepare(self, amount: U256) -> eyre::Result<NativeShieldPreparation> {
        let recipient = public_shield_recipient(&self.view_session)?;
        let client = self.operation.client().await?;
        prepare_native_shield(
            self.chain_id,
            &self.effective_chain,
            self.profile,
            self.payer,
            self.payer_address,
            recipient,
            amount,
            &self.http,
            &client,
        )
        .await
    }

    async fn prepare_max(
        self,
        balance: U256,
        max_fee_per_gas: u128,
    ) -> eyre::Result<NativeShieldPreparation> {
        let recipient = public_shield_recipient(&self.view_session)?;
        let client = self.operation.client().await?;
        prepare_native_shield_max(
            self.chain_id,
            &self.effective_chain,
            self.profile,
            self.payer,
            self.payer_address,
            recipient,
            balance,
            max_fee_per_gas,
            &self.http,
            &client,
        )
        .await
    }
}

impl WalletRoot {
    /// The transaction profile of the Shield form.
    pub(in crate::root) fn public_shield_profile(&self) -> PublicShieldTransactionProfile {
        if self.mimic_railway_shield_effective() {
            PublicShieldTransactionProfile::Railway
        } else {
            PublicShieldTransactionProfile::Railoxide
        }
    }

    /// The Mimic Railway preference, off while the native asset is selected and native Mimic is
    /// unavailable. The preference itself is kept for other assets and accounts.
    pub(in crate::root) fn mimic_railway_shield_effective(&self) -> bool {
        self.public_form.mimic_railway_shield && self.mimic_railway_native_unavailable().is_none()
    }

    fn selected_native_shield_payer(&self) -> Option<(Address, NativeShieldPayer)> {
        let account = self.selected_public_account();
        // A staged form needs no account; nothing is prepared or sent from it.
        #[cfg(debug_assertions)]
        if let Some(payer) =
            public_shield_ui_fixture().and_then(PublicShieldUiFixture::native_payer)
        {
            return Some((
                account.map_or(Address::ZERO, |account| account.address),
                payer,
            ));
        }
        let account = account?;
        Some((
            account.address,
            native_shield_payer(
                account.source,
                self.selected_wallet_source().is_hardware_derived(),
            ),
        ))
    }

    /// The selected payer's native shield route in `profile`, whenever the native asset is
    /// selected.
    fn native_shield_route_in(
        &self,
        profile: PublicShieldTransactionProfile,
    ) -> Option<Result<NativeShieldRoute, NativeShieldUnavailable>> {
        if self.public_form.selected_asset != Some(PublicAssetId::Native) {
            return None;
        }
        let (_, payer) = self.selected_native_shield_payer()?;
        let chain = self.effective_chain_configs.get(self.selected_chain)?;
        // The fee model is the chain's, whichever profile the quote was taken for.
        let observed_fee_model = self
            .public_form
            .shield_gas_fee
            .quote
            .and_then(|quote| quote.observed_fee_model);
        Some(select_native_shield_route(
            self.selected_chain,
            chain,
            payer,
            profile,
            observed_fee_model,
        ))
    }

    /// The form's native shield route in its current profile.
    pub(in crate::root) fn native_shield_route(
        &self,
    ) -> Option<Result<NativeShieldRoute, NativeShieldUnavailable>> {
        self.native_shield_route_in(self.public_shield_profile())
    }

    /// The Shield form is on a stealth-route native shield, which needs a preparation.
    pub(in crate::root) fn native_shield_stealth_applies(&self, mode: PublicActionMode) -> bool {
        mode == PublicActionMode::Shield
            && matches!(
                self.native_shield_route(),
                Some(Ok(NativeShieldRoute::Stealth(_)))
            )
    }

    /// The Shield form's action may sign an ERC-20 approval: an ERC-20 shield or a native
    /// wrap fallback.
    pub(in crate::root) fn public_shield_may_approve(&self) -> bool {
        match self.public_form.selected_asset {
            Some(PublicAssetId::Erc20(_)) => true,
            Some(PublicAssetId::Native) => matches!(
                self.native_shield_route(),
                Some(Ok(NativeShieldRoute::WrapFallback { .. }))
            ),
            None => false,
        }
    }

    /// Why native Mimic can't shield from the selected account on this chain, whenever the
    /// native asset is selected, regardless of the Mimic Railway preference.
    pub(in crate::root) fn mimic_railway_native_unavailable(
        &self,
    ) -> Option<NativeShieldUnavailable> {
        self.native_shield_route_in(PublicShieldTransactionProfile::Railway)?
            .err()
    }

    /// Why the Shield form's native shield can't be sent at all.
    pub(in crate::root) fn native_shield_unavailable(
        &self,
        mode: PublicActionMode,
    ) -> Option<NativeShieldUnavailable> {
        if mode != PublicActionMode::Shield {
            return None;
        }
        self.native_shield_route()?.err()
    }

    /// The inputs the current preparation must be bound to, when the form needs one.
    pub(in crate::root) fn native_shield_key(&self, cx: &App) -> Option<NativeShieldKey> {
        if !self.native_shield_stealth_applies(PublicActionMode::Shield) {
            return None;
        }
        self.selected_public_account()?;
        let decimals = public_asset_decimals(
            self.effective_chain_configs.get(self.selected_chain),
            PublicAssetId::Native,
            Some(&self.effective_token_registry),
        );
        let amount = parse_send_amount(
            self.public_form
                .shield_amount_input
                .read(cx)
                .value()
                .as_ref(),
            decimals,
        )
        .ok()
        .filter(|amount| !amount.is_zero())?;
        Some(NativeShieldKey {
            wallet_id: self.selected_wallet_id.clone(),
            chain_id: self.selected_chain,
            public_account_uuid: self.public_form.selected_account_uuid.clone()?,
            profile: self.public_shield_profile(),
            amount,
        })
    }

    /// The preparation bound to the form's current inputs.
    pub(in crate::root) fn current_native_shield_preparation(
        &self,
        cx: &App,
    ) -> Option<Arc<NativeShieldPreparation>> {
        let key = self.native_shield_key(cx);
        self.public_form
            .native_shield_preparation
            .prepared(key.as_ref())
            .cloned()
    }

    /// The current preparation's raw estimate and buffered signing limit.
    pub(in crate::root) fn native_shield_gas(&self, cx: &App) -> Option<(u64, u64)> {
        #[cfg(debug_assertions)]
        if matches!(
            public_shield_ui_fixture(),
            Some(PublicShieldUiFixture::MimicPrepared | PublicShieldUiFixture::NativePrepared)
        ) {
            return Some(PUBLIC_SHIELD_UI_FIXTURE_NATIVE_GAS);
        }
        self.current_native_shield_preparation(cx)
            .map(|preparation| (preparation.raw_gas_limit(), preparation.gas_limit()))
    }

    fn native_shield_target(&mut self) -> Option<NativeShieldTarget> {
        let (payer_address, payer) = self.selected_native_shield_payer()?;
        let account = self.public_form.selected_account_uuid.clone()?;
        Some(NativeShieldTarget {
            chain_id: self.selected_chain,
            effective_chain: self
                .effective_chain_configs
                .enabled(self.selected_chain)
                .ok()?
                .clone(),
            profile: self.public_shield_profile(),
            payer,
            payer_address,
            view_session: self.view_session.clone()?,
            http: self.http.clone(),
            operation: self.public_form_operation(&account),
        })
    }

    /// Starts preparing the form's stealth-route native shield, or drops a preparation that no
    /// longer applies. A result is admitted only if nothing it is bound to changed meanwhile.
    pub(in crate::root) fn refresh_native_shield_preparation(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        #[cfg(debug_assertions)]
        if public_shield_ui_fixture().is_some() {
            return;
        }
        if self.public_form.shielding {
            return;
        }
        let key = self.native_shield_key(cx);
        if self
            .public_form
            .native_shield_preparation
            .prepared(key.as_ref())
            .is_some()
        {
            return;
        }
        let target = self.native_shield_target();
        let (Some(key), Some(target)) = (key, target) else {
            self.public_form.native_shield_preparation.invalidate();
            cx.notify();
            return;
        };
        let generation = self.public_form.native_shield_preparation.begin();
        let runtime = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(NATIVE_SHIELD_PREPARATION_DELAY)
                .await;
            let current = this
                .update(cx, |root, _| {
                    root.public_form.native_shield_preparation.generation == generation
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            let amount = key.amount;
            let result = runtime.spawn(target.prepare(amount)).await;
            let _ = this.update(cx, |root, cx| {
                let result = match result {
                    Ok(Ok(preparation)) => Ok(Arc::new(preparation)),
                    Ok(Err(error)) => Err(Arc::from(format_report_chain(&error))),
                    Err(error) => Err(Arc::from(error.to_string())),
                };
                let current = root.native_shield_key(cx);
                if root.public_form.native_shield_preparation.complete(
                    generation,
                    key,
                    current.as_ref(),
                    result,
                ) {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Stealth-route native Max: the balance less the prepared route's maximum gas at the
    /// selected fee cap. The amount and its preparation are set together.
    pub(in crate::root) fn set_native_shield_amount_to_max(
        &mut self,
        balance: U256,
        decimals: u8,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        #[cfg(debug_assertions)]
        if public_shield_ui_fixture().is_some() {
            return;
        }
        let mode = PublicActionMode::Shield;
        let (Some(target), Some(public_account_uuid)) = (
            self.native_shield_target(),
            self.public_form.selected_account_uuid.clone(),
        ) else {
            return;
        };
        let profile = target.profile;
        let max_fee_per_gas = match self.public_action_authorized_gas_fee_selection(
            mode,
            profile,
            PublicAssetId::Native,
            cx,
        ) {
            Ok(PublicActionGasFeeSelection::Custom {
                max_fee_per_gas, ..
            }) => max_fee_per_gas,
            Ok(PublicActionGasFeeSelection::Auto) => return,
            Err(error) => {
                self.set_public_action_error(mode, Some(Arc::from(error)));
                cx.notify();
                return;
            }
        };
        let wallet_id = self.selected_wallet_id.clone();
        let chain_id = self.selected_chain;
        let generation = self.public_form.native_shield_preparation.begin();
        self.set_public_action_error(mode, None);
        let join = self
            .runtime
            .spawn(target.prepare_max(balance, max_fee_per_gas));
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.public_form.native_shield_preparation.generation != generation {
                    return;
                }
                if root.public_form.action_mode != mode
                    || !root.native_shield_stealth_applies(mode)
                    || root.public_shield_profile() != profile
                    || root.selected_wallet_id != wallet_id
                    || root.selected_chain != chain_id
                    || root.public_form.selected_account_uuid.as_deref()
                        != Some(public_account_uuid.as_ref())
                {
                    root.public_form.native_shield_preparation.invalidate();
                    cx.notify();
                    return;
                }
                let result = match result {
                    Ok(Ok(preparation)) => Ok(preparation),
                    Ok(Err(error)) => Err(format_report_chain(&error)),
                    Err(error) => Err(error.to_string()),
                };
                match result {
                    Ok(preparation) => {
                        let key = NativeShieldKey {
                            wallet_id,
                            chain_id,
                            public_account_uuid,
                            profile,
                            amount: preparation.amount(),
                        };
                        root.set_public_action_amount_input(
                            mode,
                            preparation.amount(),
                            decimals,
                            window,
                            cx,
                        );
                        root.public_form.native_shield_preparation.complete(
                            generation,
                            key.clone(),
                            Some(&key),
                            Ok(Arc::new(preparation)),
                        );
                    }
                    Err(error) => {
                        root.public_form.native_shield_preparation.invalidate();
                        root.set_public_action_error(
                            mode,
                            Some(Arc::from(format!("Max is unavailable: {error}"))),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Why the Shield form's native shield is unavailable, or its preparation problem.
    pub(in crate::root) fn render_native_shield_status(&self, cx: &App) -> Option<AnyElement> {
        let mode = self.public_form.action_mode;
        if let Some(reason) = self.native_shield_unavailable(mode) {
            return Some(
                Alert::error(
                    "wallet-public-shield-native-unavailable",
                    format!("Native shielding is unavailable: {reason}."),
                )
                .small()
                .into_any_element(),
            );
        }
        if !self.native_shield_stealth_applies(mode) {
            return None;
        }
        let key = self.native_shield_key(cx);
        self.public_form
            .native_shield_preparation
            .error(key.as_ref())
            .map(|error| {
                Alert::error(
                    "wallet-public-shield-native-preparation-error",
                    format!("Could not prepare the native shield: {error}"),
                )
                .small()
                .into_any_element()
            })
    }
}

#[cfg(debug_assertions)]
impl WalletRoot {
    /// `shield:mimic-prepared`, `shield:native-prepared`, `shield:mimic-unavailable` and
    /// `shield:fallback` stage the native Shield form with synthesized state. Nothing is
    /// prepared or sent.
    pub(in crate::root) fn stage_native_shield_form_ui_fixture(
        &mut self,
        fixture: PublicShieldUiFixture,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.action_mode = PublicActionMode::Shield;
        self.public_form.selected_asset = Some(PublicAssetId::Native);
        self.public_form.mimic_railway_shield = matches!(
            fixture,
            PublicShieldUiFixture::MimicPrepared | PublicShieldUiFixture::MimicUnavailable
        );
        self.public_form
            .shield_amount_input
            .update(cx, |input, cx| input.set_value("0.5", window, cx));
        self.public_form.shield_gas_fee.quote = Some(PublicActionGasFeeQuote {
            observed_fee_model: None,
            rpc_gas_price: 2_000_000_000,
            current_base_fee_per_gas: Some(1_800_000_000),
            suggested_max_fee_per_gas: 2_080_000_000,
            suggested_max_priority_fee_per_gas: 100_000_000,
        });
        self.public_form.shield_gas_fee_route =
            Some((PublicAssetId::Native, self.public_shield_profile()));
    }

    /// `shield:native-pending` holds a stealth-route native Shield step waiting for
    /// confirmation. `shield:fallback-pending` holds a hardware payer's wrap fallback with its
    /// Wrap done and its Approve waiting for confirmation.
    pub(in crate::root) fn open_native_shield_pending_ui_fixture(
        &mut self,
        fixture: PublicShieldUiFixture,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let asset = PublicAssetId::Native;
        let state = |step, status, tx_hash: Option<&str>| PublicActionStepState {
            step,
            status,
            tx_hash: tx_hash.map(Arc::from),
            message: None,
            interval: None,
        };
        let (states, source) = if fixture == PublicShieldUiFixture::FallbackPending {
            (
                vec![
                    state(
                        PublicActionProgressStep::ShieldKey,
                        PublicActionStepStatus::Done,
                        None,
                    ),
                    state(
                        PublicActionProgressStep::Wrap,
                        PublicActionStepStatus::Done,
                        Some("0x4b1a7c3e9d2f8a6b5c4d3e2f1a0b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a2b"),
                    ),
                    state(
                        PublicActionProgressStep::Approve,
                        PublicActionStepStatus::Pending,
                        Some("0x7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a2b4b1a7c3e9d2f8a6b5c4d3e2f1a0b9c8d"),
                    ),
                    state(
                        PublicActionProgressStep::Shield,
                        PublicActionStepStatus::NotStarted,
                        None,
                    ),
                ],
                PublicAccountSource::HardwareDerived,
            )
        } else {
            (
                vec![state(
                    PublicActionProgressStep::Shield,
                    PublicActionStepStatus::Pending,
                    Some("0x9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4d3c2b1a0f9e8d"),
                )],
                PublicAccountSource::Derived,
            )
        };
        self.start_public_action_progress_with_states(
            PublicActionMode::Shield,
            states,
            public_asset_label(
                self.effective_chain_configs.get(self.selected_chain),
                asset,
                Some(&self.effective_token_registry),
            ),
            public_asset_icon_path(
                self.selected_chain,
                asset,
                Some(&self.effective_token_registry),
            ),
            source,
            None,
            Some((2_080_000_000, 100_000_000)),
            false,
        );
        self.public_form.action_stop_available = false;
        self.show_public_action_progress_dialog(window, cx);
        cx.notify();
    }
}
