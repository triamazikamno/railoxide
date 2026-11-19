use super::*;

mod funding_estimate;
mod funding_selection;
mod public_balance_estimates;
mod sponsored_estimates;

pub(in crate::root) use ui::private_action::self_broadcast::SPONSORSHIP_LABEL as BLOCK_BUILDER_SPONSORSHIP_LABEL;
const SPONSORED_FUNDING_ESTIMATE_DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum SponsoredEstimateRefreshMode {
    Invalidate,
    Retain,
}

impl SponsoredEstimateRefreshMode {
    pub(in crate::root) const fn clears_current(self, can_schedule: bool) -> bool {
        !can_schedule || matches!(self, Self::Invalidate)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) struct SponsoredAssetFee {
    pub(in crate::root) token: Address,
    pub(in crate::root) amount: U256,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) struct SponsoredFundingEstimate {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) wrapped_native_token: Address,
    pub(in crate::root) expected_payment: SponsorshipPayment,
    pub(in crate::root) maximum_payment: SponsorshipPayment,
    pub(in crate::root) primary_unshield_protocol_fee: Option<SponsoredAssetFee>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct PublicBalanceFundingEstimate {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) cost: DesktopSelfBroadcastCostEstimate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) enum SponsoredFundingEstimateState {
    PublicBalanceReady(Box<PublicBalanceFundingEstimate>),
    PublicBalanceUnavailable,
    Ready(Box<SponsoredFundingEstimate>),
    InsufficientWrappedNative {
        chain_id: u64,
        token: Option<Address>,
        available: U256,
        required: U256,
    },
    InsufficientWrappedNativeForQuote {
        chain_id: u64,
        token: Option<Address>,
        available: U256,
    },
    Unavailable,
}

pub(in crate::root) fn sponsored_estimate_allows_submission(
    funding: SelfBroadcastFundingMode,
    state: Option<&SponsoredFundingEstimateState>,
    _pending: bool,
) -> bool {
    funding != SelfBroadcastFundingMode::PrivateSponsorship
        || matches!(state, Some(SponsoredFundingEstimateState::Ready(_)))
}

pub(in crate::root) fn sponsored_estimate_failure_state(
    chain_id: u64,
    wrapped_native_token: Option<Address>,
    error: &eyre::Report,
) -> SponsoredFundingEstimateState {
    match error.downcast_ref::<SponsorshipError>() {
        Some(SponsorshipError::InsufficientWrappedNative {
            available,
            required,
        }) => SponsoredFundingEstimateState::InsufficientWrappedNative {
            chain_id,
            token: wrapped_native_token,
            available: *available,
            required: *required,
        },
        Some(SponsorshipError::InsufficientWrappedNativeForQuote { available }) => {
            SponsoredFundingEstimateState::InsufficientWrappedNativeForQuote {
                chain_id,
                token: wrapped_native_token,
                available: *available,
            }
        }
        _ => SponsoredFundingEstimateState::Unavailable,
    }
}

pub(in crate::root) fn sponsored_estimate_from_authorization_limit(
    chain_id: u64,
    limit: SponsoredAuthorizationLimit,
    expected_fee_per_gas: u128,
    primary_unshield_protocol_fee: Option<SponsoredAssetFee>,
) -> SponsoredFundingEstimateState {
    let Ok(expected_payment) = sponsorship_payment(
        limit.expected_transaction_gas_limit,
        expected_fee_per_gas,
        limit.signer_native_balance_snapshot,
        limit.incentive,
    ) else {
        return SponsoredFundingEstimateState::Unavailable;
    };
    let Ok(maximum_payment) = limit.maximum_payment() else {
        return SponsoredFundingEstimateState::Unavailable;
    };
    SponsoredFundingEstimateState::Ready(Box::new(SponsoredFundingEstimate {
        chain_id,
        wrapped_native_token: limit.wrapped_native_token,
        expected_payment,
        maximum_payment,
        primary_unshield_protocol_fee,
    }))
}

pub(in crate::root) use ui::private_action::self_broadcast::FeeDisplay as SponsoredFundingEstimateDisplay;

pub(in crate::root) fn sponsored_excess_deposit_breakdown_visible(
    excess_deposit: U256,
    usd_micro_value: Option<U256>,
) -> bool {
    const MINIMUM_USD_MICRO_VALUE: u64 = 10_000;
    !excess_deposit.is_zero()
        && usd_micro_value.is_none_or(|value| value >= U256::from(MINIMUM_USD_MICRO_VALUE))
}

impl SponsoredFundingEstimate {
    pub(in crate::root) const fn builder_premium(&self) -> U256 {
        self.maximum_payment
            .gross_wrapped_native_spend
            .saturating_sub(self.maximum_payment.reimbursement_base)
    }

    pub(in crate::root) const fn expected_excess_deposit(&self) -> U256 {
        self.maximum_payment
            .funding_principal
            .saturating_sub(self.expected_payment.outer_gas_cap)
    }

    pub(in crate::root) const fn expected_network_gas_cost(&self) -> U256 {
        self.expected_payment
            .outer_gas_cap
            .saturating_add(self.maximum_payment.funding_gas_cap)
    }

    pub(in crate::root) const fn expected_sponsorship_cost(&self) -> U256 {
        self.expected_network_gas_cost()
            .saturating_add(self.builder_premium())
    }
}

pub(in crate::root) const fn sponsored_self_broadcast_availability_reason(
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
) -> Option<&'static str> {
    let Some(chain) = chain else {
        return Some("Selected chain settings are unavailable.");
    };
    if railgun_ui::is_testnet_chain(chain.chain_id) {
        return Some("Sponsored self-broadcast is unavailable on testnets.");
    }
    let Some(private) = &chain.railgun else {
        return Some("This chain does not support Railgun.");
    };
    if private.sponsored_bundle_relays.is_empty() {
        return Some("No compatible sponsored relay is configured for this chain.");
    }
    if chain.wrapped_native_token.is_none() {
        return Some("This chain has no configured wrapped-native token.");
    }
    if private.coinbase_payer.is_none() {
        return Some("This chain has no configured reviewed coinbase payer.");
    }
    if private.deployment.relay_adapt_contract.is_none() {
        return Some("This chain has no legacy RelayAdapt contract.");
    }
    None
}

pub(in crate::root) fn sponsored_funding_choice_visible(
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
) -> bool {
    chain.is_some_and(|chain| {
        !railgun_ui::is_testnet_chain(chain.chain_id)
            && chain
                .railgun
                .as_ref()
                .is_some_and(|private| !private.sponsored_bundle_relays.is_empty())
    })
}

pub(in crate::root) const fn sponsored_funding_enabled(
    choice_visible: bool,
    unavailable_reason: Option<&str>,
) -> bool {
    choice_visible && unavailable_reason.is_none()
}

pub(in crate::root) fn effective_self_broadcast_funding_mode(
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    selected: SelfBroadcastFundingMode,
) -> SelfBroadcastFundingMode {
    if sponsored_funding_choice_visible(chain) {
        selected
    } else {
        SelfBroadcastFundingMode::PublicBalance
    }
}

pub(in crate::root) fn effective_delivery_funding_mode(
    delivery_mode: DeliveryMode,
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    selected: SelfBroadcastFundingMode,
) -> SelfBroadcastFundingMode {
    if delivery_mode == DeliveryMode::SelfBroadcast {
        effective_self_broadcast_funding_mode(chain, selected)
    } else {
        SelfBroadcastFundingMode::PublicBalance
    }
}

pub(in crate::root) fn sponsored_incentive_from_text(
    selected: SponsoredIncentive,
    custom: &str,
) -> Result<SponsoredIncentive, &'static str> {
    if !matches!(selected, SponsoredIncentive::Custom(_)) {
        return Ok(selected);
    }
    let percent = custom
        .trim()
        .parse::<u8>()
        .map_err(|_| "Custom builder incentive must be an integer from 1% through 100%.")?;
    let incentive = SponsoredIncentive::Custom(percent);
    incentive
        .percent()
        .map_err(|_| "Custom builder incentive must be an integer from 1% through 100%.")?;
    Ok(incentive)
}

pub(in crate::root) fn default_self_broadcast_gas_payer_uuid(
    accounts: &[PublicAccountMetadata],
) -> Option<Arc<str>> {
    (accounts.len() == 1).then(|| Arc::from(accounts[0].public_account_uuid.as_str()))
}

#[cfg(test)]
pub(in crate::root) fn self_broadcast_gas_payer_matches_search(
    account: &PublicAccountMetadata,
    query: &str,
) -> bool {
    self_broadcast_gas_payer_fields_match(
        public_account_display_label(account).as_deref(),
        &account.address,
        query,
    )
}

pub(in crate::root) fn self_broadcast_gas_payer_fields_match(
    label: Option<&str>,
    address: &Address,
    query: &str,
) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    let full_address = address.to_checksum(None).to_ascii_lowercase();
    let lower_hex_address = format!("{address:#x}");
    let short = short_address(address).to_ascii_lowercase();
    label.is_some_and(|label| label.to_ascii_lowercase().contains(&query))
        || full_address.contains(&query)
        || lower_hex_address.contains(&query)
        || short.contains(&query)
}

pub(in crate::root) fn self_broadcast_gas_payer_label(account: &PublicAccountMetadata) -> String {
    public_account_display_label(account).unwrap_or_else(|| short_address(&account.address))
}

pub(in crate::root) fn self_broadcast_native_balance_entry(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
) -> Option<PublicBalanceEntry> {
    public_balance_entry_for_chain(
        snapshot,
        chain_id,
        public_account_uuid,
        PublicAssetId::Native,
        PublicAccountStatus::Active,
    )
}

pub(in crate::root) fn self_broadcast_native_balance_amount(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
) -> U256 {
    self_broadcast_native_balance_entry(snapshot, chain_id, public_account_uuid)
        .and_then(|entry| entry.amount.amount())
        .unwrap_or_default()
}

pub(in crate::root) fn sponsored_signer_balance_snapshot_changed(
    previous: Option<&PublicBalanceSnapshot>,
    current: &PublicBalanceSnapshot,
    chain_id: u64,
    public_account_uuid: &str,
) -> bool {
    let previous = self_broadcast_native_balance_entry(previous, chain_id, public_account_uuid)
        .map(|entry| entry.amount);
    let current = self_broadcast_native_balance_entry(Some(current), chain_id, public_account_uuid)
        .map(|entry| entry.amount);
    previous != current
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum SelfBroadcastNativeBalanceState {
    Unknown,
    Zero,
    Positive,
}

pub(in crate::root) fn self_broadcast_native_balance_state(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
) -> SelfBroadcastNativeBalanceState {
    match self_broadcast_native_balance_entry(snapshot, chain_id, public_account_uuid)
        .map(|entry| entry.amount)
    {
        Some(PublicBalanceAmount::Available(amount)) if amount.is_zero() => {
            SelfBroadcastNativeBalanceState::Zero
        }
        Some(PublicBalanceAmount::Available(_)) => SelfBroadcastNativeBalanceState::Positive,
        Some(PublicBalanceAmount::Unavailable) | None => SelfBroadcastNativeBalanceState::Unknown,
    }
}

pub(in crate::root) fn self_broadcast_native_balance_label(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
) -> String {
    if public_account_balances_for_chain(
        snapshot,
        chain_id,
        public_account_uuid,
        PublicAccountStatus::Active,
    )
    .is_none()
    {
        return "Not fetched".to_owned();
    }
    match self_broadcast_native_balance_entry(snapshot, chain_id, public_account_uuid) {
        Some(entry) if matches!(entry.amount, PublicBalanceAmount::Available(_)) => format!(
            "{} {}",
            public_balance_amount_label(&entry.amount, entry.asset.decimals),
            native_token_display_label(chain_id)
        ),
        // The native read failed.
        _ => "unavailable".to_owned(),
    }
}

#[cfg(test)]
pub(in crate::root) fn random_self_broadcast_gas_payer_uuid(
    accounts: &[PublicAccountMetadata],
    selected_uuid: Option<&str>,
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
) -> Option<Arc<str>> {
    random_self_broadcast_gas_payer_uuid_for_funding(
        accounts,
        selected_uuid,
        chain_id,
        snapshot,
        SelfBroadcastFundingMode::PublicBalance,
    )
}

pub(in crate::root) fn random_self_broadcast_gas_payer_uuid_for_funding(
    accounts: &[PublicAccountMetadata],
    selected_uuid: Option<&str>,
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
    funding: SelfBroadcastFundingMode,
) -> Option<Arc<str>> {
    let candidates = accounts
        .iter()
        .filter(|account| {
            Some(account.public_account_uuid.as_str()) != selected_uuid
                && (funding == SelfBroadcastFundingMode::PrivateSponsorship
                    || self_broadcast_native_balance_state(
                        snapshot,
                        chain_id,
                        &account.public_account_uuid,
                    ) != SelfBroadcastNativeBalanceState::Zero)
        })
        .collect::<Vec<_>>();
    candidates
        .choose(&mut rand::rng())
        .map(|account| Arc::from(account.public_account_uuid.as_str()))
}

pub(in crate::root) fn self_broadcast_initial_gas_values(
    selection: &SelfBroadcastGasFeeSelection,
    quote: Option<SelfBroadcastGasFeeQuote>,
) -> Option<(u128, u128)> {
    match *selection {
        SelfBroadcastGasFeeSelection::Auto => quote.map(|quote| {
            (
                quote.suggested_max_fee_per_gas,
                quote.suggested_max_priority_fee_per_gas,
            )
        }),
        SelfBroadcastGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } => Some((max_fee_per_gas, max_priority_fee_per_gas)),
    }
}

pub(in crate::root) fn self_broadcast_gas_payer_random_candidate(
    account: &PublicAccountMetadata,
    selected_uuid: Option<&str>,
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
) -> bool {
    Some(account.public_account_uuid.as_str()) != selected_uuid
        && self_broadcast_native_balance_state(snapshot, chain_id, &account.public_account_uuid)
            != SelfBroadcastNativeBalanceState::Zero
}

pub(in crate::root) fn normalized_self_broadcast_gas_payer_uuid(
    selected_uuid: Option<&Arc<str>>,
    accounts: &[PublicAccountMetadata],
) -> Option<Arc<str>> {
    selected_uuid
        .filter(|uuid| {
            accounts
                .iter()
                .any(|account| account.public_account_uuid.as_str() == uuid.as_ref())
        })
        .cloned()
        .or_else(|| default_self_broadcast_gas_payer_uuid(accounts))
}

pub(in crate::root) fn self_broadcast_gas_payer_select_items(
    accounts: &[PublicAccountMetadata],
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
    root: &gpui::WeakEntity<WalletRoot>,
) -> Vec<SelfBroadcastGasPayerSelectItem> {
    accounts
        .iter()
        .map(|account| {
            let uuid = account.public_account_uuid.as_str();
            let status = PublicAccountStatus::Active;
            SelfBroadcastGasPayerSelectItem {
                public_account_uuid: Arc::from(uuid),
                label: Arc::from(self_broadcast_gas_payer_label(account)),
                address: account.address,
                chain_id,
                balance_label: Arc::from(self_broadcast_native_balance_label(
                    snapshot, chain_id, uuid,
                )),
                fetchable: public_account_balances_for_chain(snapshot, chain_id, uuid, status)
                    .is_none()
                    || public_account_stale_for_chain(snapshot, chain_id, uuid, status),
                root: root.clone(),
            }
        })
        .collect()
}

pub(in crate::root) fn self_broadcast_gas_payer_select_index(
    items: &[SelfBroadcastGasPayerSelectItem],
    selected_uuid: Option<&str>,
) -> Option<IndexPath> {
    let selected_uuid = selected_uuid?;
    items
        .iter()
        .position(|item| item.public_account_uuid.as_ref() == selected_uuid)
        .map(|index| IndexPath::default().row(index))
}

pub(in crate::root) fn sync_self_broadcast_gas_payer_select_entity(
    select: &Entity<SelectState<FullWidthSelectItems<SelfBroadcastGasPayerSelectItem>>>,
    accounts: &[PublicAccountMetadata],
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
    selected_uuid: Option<&Arc<str>>,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) {
    let items =
        self_broadcast_gas_payer_select_items(accounts, chain_id, snapshot, &cx.weak_entity());
    select.update(cx, |select, cx| {
        select.set_items(FullWidthSelectItems::new(items), window, cx);
        if let Some(uuid) = selected_uuid {
            select.set_selected_value(uuid, window, cx);
        } else {
            select.set_selected_index(None, window, cx);
        }
    });
}

pub(in crate::root) fn private_action_asset_select_items(
    assets: &[UnshieldAsset],
) -> Vec<PrivateActionAssetSelectItem> {
    assets
        .iter()
        .map(|asset| PrivateActionAssetSelectItem {
            token: asset.token,
            label: Arc::from(asset.label.as_str()),
            icon_path: asset.icon_path.clone(),
        })
        .collect()
}

pub(in crate::root) fn private_action_asset_select_index(
    items: &[PrivateActionAssetSelectItem],
    selected_token: Address,
) -> Option<IndexPath> {
    items
        .iter()
        .position(|item| item.token == selected_token)
        .map(|index| IndexPath::default().row(index))
}

pub(in crate::root) fn sync_private_action_asset_select_entity(
    select: &Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>,
    assets: &[UnshieldAsset],
    selected_token: Address,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) {
    let items = private_action_asset_select_items(assets);
    let selected_index = private_action_asset_select_index(&items, selected_token);
    select.update(cx, |select, cx| {
        select.set_items(SearchableVec::new(items), window, cx);
        select.set_selected_index(selected_index, window, cx);
    });
}
