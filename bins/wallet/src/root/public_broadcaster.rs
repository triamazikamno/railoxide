use std::time::Duration;

use alloy::primitives::{Address, U256, address};
use gpui::Context;
use wallet_ops::{
    BroadcasterFeePolicy, BroadcasterFeePolicyStatus, FeeHandlingMode, ListUtxosOutput,
    PublicBroadcasterCandidate, PublicBroadcasterCostEstimate, PublicBroadcasterTrustFilter,
    RAILGUN_PROTOCOL_FEE_BPS, eligible_public_broadcasters_for_asset,
    fee_policy_eligible_public_broadcasters, filter_public_broadcasters_by_trust,
    max_broadcaster_fee_token_amount_from_outputs as planner_max_broadcaster_fee_token_amount_from_outputs,
    public_broadcaster_candidates_for_asset,
    settings::{
        EffectiveChainConfig, EffectiveChainRegistry, EffectiveTokenRegistry, ExecutorProfile,
    },
};

use crate::assets::WalletIconSource;

use super::private_assets::format_private_asset_rows;
use super::{
    DeliveryFormKind, DeliveryMode, SendFormState, UnshieldAssetKey, UnshieldFormState, WalletRoot,
};

const ETHEREUM_CHAIN_ID: u64 = 1;
const ETHEREUM_WETH_TOKEN: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PublicBroadcasterFeeTokenOption {
    pub(super) token: Address,
    pub(super) label: String,
    pub(super) decimals: Option<u8>,
    pub(super) max_spendable: U256,
    pub(super) eligible_broadcaster_count: usize,
    pub(super) icon_path: Option<WalletIconSource>,
}

impl WalletRoot {
    pub(super) fn monitor_fee_rows(&self) -> Vec<broadcaster_monitor::FeeRow> {
        self.monitor_state.read().fee_rows()
    }

    pub(super) const fn public_broadcaster_fee_policy(
        &self,
        allow_suspicious_broadcasters: bool,
    ) -> BroadcasterFeePolicy {
        self.public_broadcaster_policy
            .with_allow_suspicious_broadcasters(allow_suspicious_broadcasters)
    }

    pub(super) fn public_broadcaster_trust_filter(
        &self,
        favorites_only: bool,
    ) -> PublicBroadcasterTrustFilter {
        PublicBroadcasterTrustFilter {
            preferences: self.broadcaster_preferences.clone(),
            favorites_only,
        }
    }

    pub(super) fn current_public_broadcaster_candidates(
        &self,
        chain_id: u64,
        token: Address,
        unwrap: bool,
        native_top_up: bool,
        favorites_only: bool,
        policy: BroadcasterFeePolicy,
    ) -> Vec<PublicBroadcasterCandidate> {
        public_broadcaster_candidates_for_route(
            &self.monitor_fee_rows(),
            chain_id,
            token,
            required_relay_adapt_for_unshield(
                &self.effective_chain_configs,
                chain_id,
                unwrap,
                native_top_up,
            ),
            self.public_broadcaster_executor_profile(chain_id, unwrap, native_top_up),
            policy,
            self.public_broadcaster_anchor_cache
                .cached_rate(chain_id, token),
            &self.public_broadcaster_trust_filter(favorites_only),
        )
    }

    fn public_broadcaster_executor_profile(
        &self,
        chain_id: u64,
        unwrap: bool,
        native_top_up: bool,
    ) -> Option<ExecutorProfile> {
        if (unwrap || native_top_up) && !self.selected_wallet_source().is_hardware_derived() {
            self.effective_chain_configs
                .get(chain_id)
                .and_then(EffectiveChainConfig::accepted_executor_profile)
        } else {
            None
        }
    }

    pub(super) fn current_public_broadcaster_fee_token_options(
        &self,
        chain_id: u64,
        unwrap: bool,
        native_top_up: bool,
        favorites_only: bool,
        policy: BroadcasterFeePolicy,
    ) -> Vec<PublicBroadcasterFeeTokenOption> {
        let Some(snapshot) = self
            .chain_states
            .get(&chain_id)
            .and_then(|state| state.snapshot())
        else {
            return Vec::new();
        };
        let fee_rows = self.monitor_fee_rows();
        let required_relay_adapt = required_relay_adapt_for_unshield(
            &self.effective_chain_configs,
            chain_id,
            unwrap,
            native_top_up,
        );
        public_broadcaster_fee_token_options_from_snapshot(
            snapshot,
            &fee_rows,
            required_relay_adapt,
            self.public_broadcaster_executor_profile(chain_id, unwrap, native_top_up),
            policy,
            &self.public_broadcaster_trust_filter(favorites_only),
            Some(&self.effective_token_registry),
            |token| {
                self.public_broadcaster_anchor_cache
                    .cached_rate(chain_id, token)
            },
        )
    }

    pub(super) fn default_public_broadcaster_fee_token(
        &self,
        chain_id: u64,
        action_token: Address,
        unwrap: bool,
        allow_suspicious_broadcasters: bool,
    ) -> Address {
        let policy = self.public_broadcaster_fee_policy(allow_suspicious_broadcasters);
        let options = self
            .current_public_broadcaster_fee_token_options(chain_id, unwrap, false, false, policy);
        resolve_selected_public_broadcaster_fee_token(action_token, action_token, &options)
    }

    pub(super) fn refresh_public_broadcaster_anchor(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &Context<'_, Self>,
    ) {
        let Some((_chain_id, _token)) = (match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .map(|form| (form.asset.chain_id, form.selected_fee_token)),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .map(|form| (form.asset.chain_id, form.selected_fee_token)),
        }) else {
            return;
        };
        self.public_broadcaster_anchor_refresh.wake();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |_root, cx| cx.notify());
        })
        .detach();
    }
}

pub(super) const fn broadcaster_candidate_anchor_rate(
    candidate: &PublicBroadcasterCandidate,
) -> Option<U256> {
    match candidate.fee_policy_status {
        BroadcasterFeePolicyStatus::Normal { anchor_rate, .. }
        | BroadcasterFeePolicyStatus::Suspicious { anchor_rate, .. } => Some(anchor_rate),
        BroadcasterFeePolicyStatus::UnknownAnchor => None,
    }
}

pub(super) fn should_show_distinct_amount(entered_amount: U256, amount: U256) -> bool {
    amount != entered_amount
}

fn max_entered_amount_for_fee_handling_mode(
    max_receiver_amount: U256,
    fee_amount: U256,
    same_token_fee: bool,
    protocol_fee_bps: U256,
    fee_mode: FeeHandlingMode,
) -> U256 {
    match fee_mode {
        FeeHandlingMode::DeductFromAmount => {
            if same_token_fee {
                max_receiver_amount + fee_amount
            } else {
                max_receiver_amount
            }
        }
        FeeHandlingMode::AddToAmount => max_receiver_amount
            .saturating_sub(max_receiver_amount * protocol_fee_bps / U256::from(10_000)),
    }
}

pub(super) fn unshield_max_entered_amount_for_mode(
    max_receiver_amount: U256,
    fee_mode: FeeHandlingMode,
) -> U256 {
    max_entered_amount_for_fee_handling_mode(
        max_receiver_amount,
        U256::ZERO,
        false,
        RAILGUN_PROTOCOL_FEE_BPS,
        fee_mode,
    )
}

fn cost_estimate_max_entered_amount_for_mode(
    estimate: &PublicBroadcasterCostEstimate,
    fee_mode: FeeHandlingMode,
) -> U256 {
    max_entered_amount_for_fee_handling_mode(
        estimate.max_receiver_amount,
        estimate.fee_amount,
        estimate.action_token == estimate.fee_token,
        estimate.protocol_fee_bps,
        fee_mode,
    )
}

pub(super) fn send_form_max_entered_amount(
    form: &SendFormState,
    delivery_mode: DeliveryMode,
    fee_mode: FeeHandlingMode,
) -> Option<U256> {
    match delivery_mode {
        DeliveryMode::ManualCalldata | DeliveryMode::SelfBroadcast => Some(form.asset.max_batched),
        DeliveryMode::PublicBroadcaster => form
            .cost_estimate
            .as_ref()
            .map(|estimate| cost_estimate_max_entered_amount_for_mode(estimate, fee_mode)),
    }
}

pub(super) fn unshield_form_max_entered_amount(
    form: &UnshieldFormState,
    delivery_mode: DeliveryMode,
    fee_mode: FeeHandlingMode,
) -> Option<U256> {
    match delivery_mode {
        DeliveryMode::ManualCalldata | DeliveryMode::SelfBroadcast => Some(
            unshield_max_entered_amount_for_mode(form.asset.max_batched, fee_mode),
        ),
        DeliveryMode::PublicBroadcaster => form
            .cost_estimate
            .as_ref()
            .map(|estimate| cost_estimate_max_entered_amount_for_mode(estimate, fee_mode)),
    }
}

fn max_broadcaster_fee_token_amount_from_snapshot(
    snapshot: &ListUtxosOutput,
    token: Address,
) -> U256 {
    planner_max_broadcaster_fee_token_amount_from_outputs(&snapshot.utxos, token)
}

pub(super) fn public_broadcaster_candidates_for_route(
    fee_rows: &[broadcaster_monitor::FeeRow],
    chain_id: u64,
    token: Address,
    required_relay_adapt: Option<Address>,
    executor_profile: Option<ExecutorProfile>,
    policy: BroadcasterFeePolicy,
    anchor_rate: Option<U256>,
    trust_filter: &PublicBroadcasterTrustFilter,
) -> Vec<PublicBroadcasterCandidate> {
    let mut candidates = public_broadcaster_candidates_for_asset(
        fee_rows,
        chain_id,
        token,
        required_relay_adapt.filter(|_| executor_profile.is_none()),
        policy,
        anchor_rate,
    )
    .unwrap_or_default();
    if let Some(profile) = executor_profile {
        candidates.retain(|candidate| {
            wallet_ops::ExecutorDelivery::PublicBroadcaster(Box::new(candidate.clone()))
                .admit(profile)
                .is_ok()
        });
    }
    filter_public_broadcasters_by_trust(&candidates, trust_filter)
}

pub(super) fn public_broadcaster_fee_token_options_from_snapshot(
    snapshot: &ListUtxosOutput,
    fee_rows: &[broadcaster_monitor::FeeRow],
    required_relay_adapt: Option<Address>,
    executor_profile: Option<ExecutorProfile>,
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
    registry: Option<&EffectiveTokenRegistry>,
    mut anchor_rate_for_token: impl FnMut(Address) -> Option<U256>,
) -> Vec<PublicBroadcasterFeeTokenOption> {
    format_private_asset_rows(snapshot.chain_id, &snapshot.totals, registry, None)
        .into_iter()
        .filter_map(|asset| {
            let token = asset.token?;
            let poi_verified_total = asset.poi_verified_total?;
            if poi_verified_total.is_zero() {
                return None;
            }
            let max_spendable = max_broadcaster_fee_token_amount_from_snapshot(snapshot, token);
            if max_spendable.is_zero() {
                return None;
            }
            let candidates = public_broadcaster_candidates_for_route(
                fee_rows,
                snapshot.chain_id,
                token,
                required_relay_adapt,
                executor_profile,
                policy,
                anchor_rate_for_token(token),
                trust_filter,
            );
            let eligible_broadcaster_count =
                fee_policy_eligible_public_broadcasters(&candidates, policy).len();
            Some(PublicBroadcasterFeeTokenOption {
                token,
                label: asset.label,
                decimals: asset.decimals,
                max_spendable,
                eligible_broadcaster_count,
                icon_path: asset.icon_path,
            })
        })
        .collect()
}

pub(super) fn fee_token_option_has_eligible_broadcaster(
    options: &[PublicBroadcasterFeeTokenOption],
    token: Address,
) -> bool {
    options
        .iter()
        .any(|option| option.token == token && option.eligible_broadcaster_count > 0)
}

pub(super) fn ethereum_weth_public_broadcaster_count(
    fee_rows: &[broadcaster_monitor::FeeRow],
) -> usize {
    eligible_public_broadcasters_for_asset(fee_rows, ETHEREUM_CHAIN_ID, ETHEREUM_WETH_TOKEN, None)
        .map_or(0, |candidates| candidates.len())
}

fn selected_fee_token_eligible_broadcaster_count(
    options: &[PublicBroadcasterFeeTokenOption],
    token: Address,
) -> Option<usize> {
    options
        .iter()
        .find(|option| option.token == token)
        .map(|option| option.eligible_broadcaster_count)
}

pub(super) fn public_broadcaster_submit_disabled_for_fee_token_options(
    options: &[PublicBroadcasterFeeTokenOption],
    selected_fee_token: Address,
) -> bool {
    selected_fee_token_eligible_broadcaster_count(options, selected_fee_token).unwrap_or_default()
        == 0
}

pub(super) fn public_broadcaster_fee_token_warning(
    fee_rows: &[broadcaster_monitor::FeeRow],
    chain_id: u64,
    options: &[PublicBroadcasterFeeTokenOption],
    selected_fee_token: Address,
    trust_filter: &PublicBroadcasterTrustFilter,
) -> Option<&'static str> {
    if selected_fee_token_eligible_broadcaster_count(options, selected_fee_token)
        .unwrap_or_default()
        > 0
    {
        return None;
    }
    if !fee_rows.iter().any(|row| row.chain_id == chain_id) {
        return Some("Searching for public broadcasters");
    }
    if trust_filter.favorites_only && trust_filter.preferences.favorites.is_empty() {
        return Some("Favorites-only mode is on, but no favorite broadcasters are saved yet.");
    }
    if trust_filter.favorites_only {
        return Some("No favorite broadcaster currently supports your spendable fee tokens.");
    }
    if !trust_filter.preferences.banned.is_empty() {
        return Some("No non-banned broadcaster currently supports your spendable fee tokens.");
    }
    if options
        .iter()
        .any(|option| option.eligible_broadcaster_count > 0)
    {
        return Some(
            "Choose a fee token with at least one eligible public broadcaster before submitting.",
        );
    }
    Some("No detected public broadcaster supports your spendable fee tokens")
}

pub(super) fn resolve_selected_public_broadcaster_fee_token(
    current_fee_token: Address,
    action_token: Address,
    options: &[PublicBroadcasterFeeTokenOption],
) -> Address {
    if fee_token_option_has_eligible_broadcaster(options, current_fee_token) {
        return current_fee_token;
    }
    if fee_token_option_has_eligible_broadcaster(options, action_token) {
        return action_token;
    }
    options
        .iter()
        .find(|option| option.eligible_broadcaster_count > 0)
        .map_or(current_fee_token, |option| option.token)
}

pub(super) fn effective_fee_handling_mode(
    kind: DeliveryFormKind,
    action_token: Address,
    fee_token: Address,
    fee_mode: FeeHandlingMode,
) -> FeeHandlingMode {
    if matches!(kind, DeliveryFormKind::Send) && action_token != fee_token {
        FeeHandlingMode::AddToAmount
    } else {
        fee_mode
    }
}

pub(super) fn should_show_fee_mode_toggle(
    kind: DeliveryFormKind,
    action_token: Address,
    fee_token: Address,
) -> bool {
    matches!(kind, DeliveryFormKind::Unshield) || action_token == fee_token
}

pub(super) fn required_relay_adapt_for_unshield(
    effective_chain_configs: &EffectiveChainRegistry,
    chain_id: u64,
    unwrap: bool,
    native_top_up: bool,
) -> Option<Address> {
    (unwrap || native_top_up)
        .then(|| {
            effective_chain_configs.get(chain_id).and_then(|chain| {
                chain
                    .railgun
                    .as_ref()
                    .map(|private| private.deployment.relay_adapt_contract)
            })
        })
        .flatten()
}
