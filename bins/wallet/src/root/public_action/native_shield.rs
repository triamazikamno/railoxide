//! Native Mimic Railway shields in the Public Shield form: availability, the background
//! preparation that review and submission require, and its Max. wallet-ops sends each from a
//! stealth account of the selected private wallet.

use std::time::Duration;

use super::*;

/// Typing settles before the shield is estimated.
const NATIVE_MIMIC_PREPARATION_DELAY: Duration = Duration::from_millis(400);

/// A Public Shield state staged by `RAILOXIDE_UI_FIXTURE`; see the fixture module doc.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum PublicShieldUiFixture {
    EstimateError,
    FundingError,
    MimicPrepared,
    MimicUnavailable,
    MimicPending,
}

#[cfg(debug_assertions)]
pub(in crate::root) fn public_shield_ui_fixture() -> Option<PublicShieldUiFixture> {
    static FIXTURE: std::sync::OnceLock<Option<PublicShieldUiFixture>> = std::sync::OnceLock::new();
    *FIXTURE.get_or_init(
        || match std::env::var("RAILOXIDE_UI_FIXTURE").ok()?.as_str() {
            "shield:estimate-error" => Some(PublicShieldUiFixture::EstimateError),
            "shield:funding-error" => Some(PublicShieldUiFixture::FundingError),
            "shield:mimic-prepared" => Some(PublicShieldUiFixture::MimicPrepared),
            "shield:mimic-unavailable" => Some(PublicShieldUiFixture::MimicUnavailable),
            "shield:mimic-pending" => Some(PublicShieldUiFixture::MimicPending),
            _ => None,
        },
    )
}

/// The raw and buffered gas of the `shield:mimic-prepared` fee card.
#[cfg(debug_assertions)]
const PUBLIC_SHIELD_UI_FIXTURE_NATIVE_GAS: (u64, u64) = (241_380, 289_656);

/// What a native Mimic preparation needs besides its amount.
struct NativeMimicShieldTarget {
    chain_id: u64,
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
    payer: RailwayNativeShieldPayer,
    payer_address: Address,
    view_session: Arc<DesktopViewSession>,
    http: wallet_ops::HttpContext,
}

impl NativeMimicShieldTarget {
    async fn prepare(self, amount: U256) -> eyre::Result<RailwayNativeShieldPreparation> {
        let recipient = public_shield_recipient(&self.view_session)?;
        prepare_railway_native_shield(
            self.chain_id,
            &self.effective_chain,
            self.payer,
            self.payer_address,
            recipient,
            amount,
            &self.http,
        )
        .await
    }

    async fn prepare_max(
        self,
        balance: U256,
        max_fee_per_gas: u128,
    ) -> eyre::Result<RailwayNativeShieldPreparation> {
        let recipient = public_shield_recipient(&self.view_session)?;
        prepare_railway_native_shield_max(
            self.chain_id,
            &self.effective_chain,
            self.payer,
            self.payer_address,
            recipient,
            balance,
            max_fee_per_gas,
            &self.http,
        )
        .await
    }
}

impl WalletRoot {
    /// The Shield form is on a native Mimic Railway shield.
    pub(in crate::root) fn native_mimic_shield_applies(&self, mode: PublicActionMode) -> bool {
        mode == PublicActionMode::Shield
            && self.public_form.selected_asset == Some(PublicAssetId::Native)
            && self.mimic_railway_shield_effective()
    }

    /// The Mimic Railway preference, off while the native asset is selected and native Mimic is
    /// unavailable. The preference itself is kept for other assets and accounts.
    pub(in crate::root) fn mimic_railway_shield_effective(&self) -> bool {
        self.public_form.mimic_railway_shield && self.native_mimic_shield_unavailable().is_none()
    }

    fn native_mimic_shield_payer(&self) -> Option<(Address, RailwayNativeShieldPayer)> {
        let account = self.selected_public_account()?;
        Some((
            account.address,
            railway_native_shield_payer(
                account.source,
                self.selected_wallet_source().is_hardware_derived(),
            ),
        ))
    }

    /// Why native Mimic can't shield from the selected account on this chain, whenever the
    /// native asset is selected, regardless of the Mimic Railway preference.
    pub(in crate::root) fn native_mimic_shield_unavailable(
        &self,
    ) -> Option<RailwayNativeShieldUnavailable> {
        if self.public_form.selected_asset != Some(PublicAssetId::Native) {
            return None;
        }
        #[cfg(debug_assertions)]
        match public_shield_ui_fixture() {
            Some(PublicShieldUiFixture::MimicUnavailable) => {
                return Some(RailwayNativeShieldUnavailable::HardwarePayer);
            }
            Some(PublicShieldUiFixture::MimicPrepared) => return None,
            _ => {}
        }
        let (_, payer) = self.native_mimic_shield_payer()?;
        let chain = self.effective_chain_configs.get(self.selected_chain)?;
        railway_native_shield_profile(self.selected_chain, chain, payer).err()
    }

    /// The inputs the current preparation must be bound to, when the form needs one.
    pub(in crate::root) fn native_mimic_shield_key(
        &self,
        cx: &App,
    ) -> Option<NativeMimicShieldKey> {
        if !self.native_mimic_shield_applies(PublicActionMode::Shield) {
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
        Some(NativeMimicShieldKey {
            wallet_id: self.selected_wallet_id.clone(),
            chain_id: self.selected_chain,
            public_account_uuid: self.public_form.selected_account_uuid.clone()?,
            amount,
        })
    }

    /// The preparation bound to the form's current inputs.
    pub(in crate::root) fn current_native_mimic_shield_preparation(
        &self,
        cx: &App,
    ) -> Option<Arc<RailwayNativeShieldPreparation>> {
        let key = self.native_mimic_shield_key(cx);
        self.public_form
            .native_mimic_shield
            .prepared(key.as_ref())
            .cloned()
    }

    /// The current preparation's raw estimate and buffered signing limit.
    pub(in crate::root) fn native_mimic_shield_gas(&self, cx: &App) -> Option<(u64, u64)> {
        #[cfg(debug_assertions)]
        if public_shield_ui_fixture() == Some(PublicShieldUiFixture::MimicPrepared) {
            return Some(PUBLIC_SHIELD_UI_FIXTURE_NATIVE_GAS);
        }
        self.current_native_mimic_shield_preparation(cx)
            .map(|preparation| (preparation.raw_gas_limit(), preparation.gas_limit()))
    }

    fn native_mimic_shield_target(&self) -> Option<NativeMimicShieldTarget> {
        let (payer_address, payer) = self.native_mimic_shield_payer()?;
        Some(NativeMimicShieldTarget {
            chain_id: self.selected_chain,
            effective_chain: self
                .effective_chain_configs
                .enabled(self.selected_chain)
                .ok()?
                .clone(),
            payer,
            payer_address,
            view_session: self.view_session.clone()?,
            http: self.http.clone(),
        })
    }

    /// Starts preparing the form's native Mimic shield, or drops a preparation that no longer
    /// applies. A result is admitted only if nothing it is bound to changed meanwhile.
    pub(in crate::root) fn refresh_native_mimic_shield_preparation(
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
        let key = self.native_mimic_shield_key(cx);
        if self
            .public_form
            .native_mimic_shield
            .prepared(key.as_ref())
            .is_some()
        {
            return;
        }
        let target = self.native_mimic_shield_target();
        let (Some(key), Some(target)) = (key, target) else {
            self.public_form.native_mimic_shield.invalidate();
            cx.notify();
            return;
        };
        let generation = self.public_form.native_mimic_shield.begin();
        let runtime = self.runtime.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(NATIVE_MIMIC_PREPARATION_DELAY)
                .await;
            let current = this
                .update(cx, |root, _| {
                    root.public_form.native_mimic_shield.generation == generation
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
                let current = root.native_mimic_shield_key(cx);
                if root.public_form.native_mimic_shield.complete(
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

    /// Native Mimic Max: the balance less the prepared route's maximum gas at the selected
    /// fee cap. The amount and its preparation are set together.
    pub(in crate::root) fn set_native_mimic_shield_amount_to_max(
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
        let max_fee_per_gas = match self.public_action_authorized_gas_fee_selection(
            mode,
            PublicShieldTransactionProfile::Railway,
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
        let (Some(target), Some(public_account_uuid)) = (
            self.native_mimic_shield_target(),
            self.public_form.selected_account_uuid.clone(),
        ) else {
            return;
        };
        let wallet_id = self.selected_wallet_id.clone();
        let chain_id = self.selected_chain;
        let generation = self.public_form.native_mimic_shield.begin();
        self.set_public_action_error(mode, None);
        let join = self
            .runtime
            .spawn(target.prepare_max(balance, max_fee_per_gas));
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.public_form.native_mimic_shield.generation != generation {
                    return;
                }
                if root.public_form.action_mode != mode
                    || !root.native_mimic_shield_applies(mode)
                    || root.selected_wallet_id != wallet_id
                    || root.selected_chain != chain_id
                    || root.public_form.selected_account_uuid.as_deref()
                        != Some(public_account_uuid.as_ref())
                {
                    root.public_form.native_mimic_shield.invalidate();
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
                        let key = NativeMimicShieldKey {
                            wallet_id,
                            chain_id,
                            public_account_uuid,
                            amount: preparation.amount(),
                        };
                        root.set_public_action_amount_input(
                            mode,
                            preparation.amount(),
                            decimals,
                            window,
                            cx,
                        );
                        root.public_form.native_mimic_shield.complete(
                            generation,
                            key.clone(),
                            Some(&key),
                            Ok(Arc::new(preparation)),
                        );
                    }
                    Err(error) => {
                        root.public_form.native_mimic_shield.invalidate();
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

    /// The native Mimic preparation problem of the Shield form.
    pub(in crate::root) fn render_native_mimic_shield_status(
        &self,
        cx: &App,
    ) -> Option<AnyElement> {
        if !self.native_mimic_shield_applies(self.public_form.action_mode) {
            return None;
        }
        let key = self.native_mimic_shield_key(cx);
        self.public_form
            .native_mimic_shield
            .error(key.as_ref())
            .map(|error| {
                Alert::error(
                    "wallet-public-shield-mimic-preparation-error",
                    format!("Could not prepare the Mimic Railway shield: {error}"),
                )
                .small()
                .into_any_element()
            })
    }
}

#[cfg(debug_assertions)]
impl WalletRoot {
    /// `shield:mimic-prepared` and `shield:mimic-unavailable` stage the native Mimic Shield
    /// form with synthesized state. Nothing is prepared or sent.
    pub(in crate::root) fn stage_native_mimic_form_ui_fixture(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.action_mode = PublicActionMode::Shield;
        self.public_form.selected_asset = Some(PublicAssetId::Native);
        self.public_form.mimic_railway_shield = true;
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
        self.public_form.shield_gas_fee_route = Some((
            PublicAssetId::Native,
            PublicShieldTransactionProfile::Railway,
        ));
    }

    /// `shield:mimic-pending` holds a native Mimic Shield step waiting for confirmation.
    pub(in crate::root) fn open_native_mimic_pending_ui_fixture(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let asset = PublicAssetId::Native;
        let states = vec![PublicActionStepState {
            step: PublicActionProgressStep::Shield,
            status: PublicActionStepStatus::Pending,
            tx_hash: Some(Arc::from(
                "0x9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4d3c2b1a0f9e8d",
            )),
            message: None,
            interval: None,
        }];
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
            PublicAccountSource::Derived,
            None,
            Some((2_080_000_000, 100_000_000)),
            false,
        );
        self.public_form.action_stop_available = false;
        self.show_public_action_progress_dialog(window, cx);
        cx.notify();
    }
}
