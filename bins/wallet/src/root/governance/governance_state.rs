use super::*;

/// Pick the evidence preflight starts from for a claim selection: the estimate the form already
/// planned for exactly these tokens, otherwise the cached dashboard evidence, otherwise a fresh
/// read.
pub(super) fn reward_evidence_mode_for_selection(
    estimate: Option<&RewardSelectionEstimateState>,
    tokens: &[Address],
    cached: Option<wallet_ops::RewardBatchEvidence>,
) -> RewardEvidenceMode {
    if let Some(RewardSelectionEstimateState::Estimated(estimate)) = estimate
        && let Some(evidence) = &estimate.evidence
        && evidence.reward_tokens.as_slice() == tokens
    {
        return RewardEvidenceMode::CachedInitial {
            evidence: Box::new(evidence.clone()),
            steps: Some(estimate.steps.clone()),
        };
    }
    cached.map_or(RewardEvidenceMode::Fresh, |evidence| {
        RewardEvidenceMode::CachedInitial {
            evidence: Box::new(evidence),
            steps: None,
        }
    })
}

impl SearchableListItem for ParticipantChoice {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .w_full()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .child(app_text(self.label.clone()).truncate())
            .child(
                app_muted_text(format!(
                    "{}{}{}",
                    short_address(&self.address),
                    if self.global { " · Global" } else { "" },
                    if self.inactive { " · Inactive" } else { "" }
                ))
                .text_size(px(11.0))
                .truncate(),
            )
    }

    fn value(&self) -> &String {
        &self.uuid
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_ascii_lowercase();
        query.is_empty()
            || self.label.to_ascii_lowercase().contains(&query)
            || format!("{:#x}", self.address).contains(&query)
    }
}

// Change carries the complete selection, including choices hidden by the query.
// Confirm is dismissal only; callers perform persistence and refresh only on Some.
pub(super) fn participant_picker_change(
    event: &ComboboxEvent<SearchableVec<ParticipantChoice>>,
    accounts: &[PublicAccountMetadata],
    wallet_id: Option<&str>,
    picker_wallet_id: Option<&str>,
    saved: &[String],
) -> Option<Vec<String>> {
    let ComboboxEvent::Change(values) = event else {
        return None;
    };
    let wallet_id = wallet_id.filter(|wallet| Some(*wallet) == picker_wallet_id)?;
    let selected = saved.iter().cloned().collect();
    let eligible = participant_choices(accounts, &selected, Some(wallet_id), "");
    let values = values
        .iter()
        .filter(|value| eligible.iter().any(|item| item.uuid == **value))
        .cloned()
        .collect::<Vec<_>>();
    let normalized = normalize_participant_ids(&values, accounts, wallet_id).uuids;
    (normalized != saved).then_some(normalized)
}

pub(in crate::root) fn participant_summary(
    accounts: &[PublicAccountMetadata],
    selected: &BTreeSet<String>,
    wallet_id: Option<&str>,
) -> ParticipantSummary {
    accounts
        .iter()
        .filter(|account| {
            wallet_id.is_some_and(|wallet| account.is_scoped_to_wallet(wallet))
                && selected.contains(&account.public_account_uuid)
        })
        .fold(ParticipantSummary::default(), |mut summary, account| {
            summary.selected += 1;
            summary.inactive += usize::from(account.status == PublicAccountStatus::Inactive);
            summary
        })
}

pub(in crate::root) fn participant_choices(
    accounts: &[PublicAccountMetadata],
    selected: &BTreeSet<String>,
    wallet_id: Option<&str>,
    query: &str,
) -> Vec<ParticipantChoice> {
    let query = query.trim().to_ascii_lowercase();
    accounts
        .iter()
        .filter(|account| {
            wallet_id.is_some_and(|wallet| account.is_scoped_to_wallet(wallet))
                && (account.status == PublicAccountStatus::Active
                    || selected.contains(&account.public_account_uuid))
                && (query.is_empty()
                    || account
                        .label
                        .as_deref()
                        .is_some_and(|label| label.to_ascii_lowercase().contains(&query))
                    || format!("{:#x}", account.address).contains(&query))
        })
        .map(|account| ParticipantChoice {
            uuid: account.public_account_uuid.clone(),
            label: public_account_display_label(account)
                .unwrap_or_else(|| "Public account".to_owned()),
            address: account.address,
            global: account.is_global(),
            inactive: account.status == PublicAccountStatus::Inactive,
        })
        .collect()
}

fn cached_reward_usd_value(
    chain_id: u64,
    token: Address,
    amount: U256,
    anchor_cache: &TokenAnchorRateCache,
) -> Option<U256> {
    anchor_cache.cached_token_usd_micro_value(chain_id, token, amount)
}

pub(super) fn reward_usd_state(
    chain_id: u64,
    token: Address,
    reward: Option<&RewardView>,
    anchor_cache: &TokenAnchorRateCache,
) -> RewardUsdState {
    match reward {
        None => RewardUsdState::Loading,
        Some(RewardView::Zero) => RewardUsdState::Value(U256::ZERO),
        Some(RewardView::Positive { amount, .. }) => {
            cached_reward_usd_value(chain_id, token, *amount, anchor_cache)
                .map_or(RewardUsdState::Unavailable, RewardUsdState::Value)
        }
        Some(RewardView::Unavailable(_)) => RewardUsdState::Unavailable,
    }
}

pub(super) fn reward_usd_label(
    chain_id: u64,
    token: Address,
    reward: Option<&RewardView>,
    anchor_cache: &TokenAnchorRateCache,
) -> Option<String> {
    match (
        reward,
        reward_usd_state(chain_id, token, reward, anchor_cache),
    ) {
        (Some(RewardView::Positive { .. }), RewardUsdState::Unavailable) => {
            Some("USD unavailable".to_owned())
        }
        (_, RewardUsdState::Value(value)) => Some(format_usd_micro_value(value)),
        _ => None,
    }
}

pub(super) fn reward_usd_total(
    chain_id: u64,
    actor_uuid: &str,
    tokens: &[Address],
    rewards: &BTreeMap<(String, Address), RewardView>,
    resolved_reward_keys: &BTreeSet<(String, Address)>,
    anchor_cache: &TokenAnchorRateCache,
) -> RewardUsdState {
    let mut total = U256::ZERO;
    let mut unavailable = false;
    for token in tokens {
        let state = reward_usd_state(
            chain_id,
            *token,
            resolved_reward_keys
                .contains(&(actor_uuid.to_owned(), *token))
                .then(|| rewards.get(&(actor_uuid.to_owned(), *token)))
                .flatten(),
            anchor_cache,
        );
        let RewardUsdState::Value(value) = state else {
            if state == RewardUsdState::Loading {
                return RewardUsdState::Loading;
            }
            unavailable = true;
            continue;
        };
        if let Some(next_total) = total.checked_add(value) {
            total = next_total;
        } else {
            unavailable = true;
        }
    }
    if unavailable {
        RewardUsdState::Unavailable
    } else {
        RewardUsdState::Value(total)
    }
}

fn reward_is_below_fee(reward_usd: RewardUsdState, fee_usd: Option<U256>) -> bool {
    matches!(
        (fee_usd, reward_usd),
        (Some(fee_usd), RewardUsdState::Value(reward_usd)) if fee_usd > reward_usd
    )
}

pub(super) fn reward_cost_presentation(
    chain_id: u64,
    cost: U256,
    reward_usd: RewardUsdState,
    anchor_cache: &TokenAnchorRateCache,
) -> RewardFeeEstimate {
    let usd = anchor_cache.cached_native_usd_micro_value(chain_id, cost);
    let label = usd.map_or_else(
        || crate::root::tokens::format_native_token_amount_for_display(chain_id, cost),
        format_usd_micro_value,
    );
    RewardFeeEstimate {
        label,
        usd,
        below_fee: reward_is_below_fee(reward_usd, usd),
    }
}

pub(super) fn reward_added_fee_presentation(
    chain_id: u64,
    estimate: &RewardSelectionEstimate,
    token: Address,
    fee_per_gas: u128,
    reward_usd: RewardUsdState,
    anchor_cache: &TokenAnchorRateCache,
) -> RewardFeePresentation {
    estimate.added_gas.get(&token).copied().flatten().map_or(
        RewardFeePresentation::Unavailable,
        |gas| {
            RewardFeePresentation::Estimated(reward_cost_presentation(
                chain_id,
                U256::from(gas) * U256::from(fee_per_gas),
                reward_usd,
                anchor_cache,
            ))
        },
    )
}

/// Select positive rewards unless their priced added fee exceeds their value. An unavailable
/// or unpriced fee is no evidence of a loss.
pub(super) fn default_reward_claim_selection_for(
    state: &StakingReadState,
    chain_id: u64,
    uuid: &str,
    tokens: &[Address],
    estimate: &RewardSelectionEstimate,
    quote: Option<wallet_ops::SelfBroadcastGasFeeQuote>,
    anchor_cache: &TokenAnchorRateCache,
) -> Vec<Address> {
    let fee_per_gas = reward_claim_fee_per_gas(
        PublicActionGasFeeSelection::Auto,
        quote,
        estimate.expected_fee_per_gas,
    );
    let mut selected = tokens
        .iter()
        .copied()
        .filter(|&token| {
            let reward_usd = reward_usd_state(
                chain_id,
                token,
                state.resolved_reward(uuid, token),
                anchor_cache,
            );
            let fee_usd = estimate
                .added_gas
                .get(&token)
                .copied()
                .flatten()
                .and_then(|gas| {
                    anchor_cache.cached_native_usd_micro_value(
                        chain_id,
                        U256::from(gas) * U256::from(fee_per_gas),
                    )
                });
            !reward_is_below_fee(reward_usd, fee_usd)
        })
        .collect::<Vec<_>>();
    selected.sort_unstable();
    selected
}

impl Default for StakingReadState {
    fn default() -> Self {
        Self {
            key: None,
            generation: 0,
            status: StakingRefreshStatus::Idle,
            global_ready: false,
            current_account_ids: BTreeSet::new(),
            current_reward_keys: BTreeSet::new(),
            metrics: None,
            accounts: BTreeMap::new(),
            rewards: BTreeMap::new(),
            bulk_reward_readiness: BTreeMap::new(),
            bulk_reward_evidence: BTreeMap::new(),
            positive_reward_captured_at: BTreeMap::new(),
            current_bulk_reward_keys: BTreeSet::new(),
            chain_time_anchor: None,
            reward_interval_countdown: None,
        }
    }
}

impl StakingReadState {
    pub(in crate::root) fn begin(&mut self, key: GovernanceContextKey) -> u64 {
        let same_context = self.key.as_ref() == Some(&key);
        self.generation = self.generation.wrapping_add(1);
        self.key = Some(key);
        self.status = StakingRefreshStatus::Loading;
        self.global_ready = false;
        self.current_account_ids.clear();
        self.current_reward_keys.clear();
        self.current_bulk_reward_keys.clear();
        self.bulk_reward_readiness.clear();
        self.bulk_reward_evidence.clear();
        self.positive_reward_captured_at.clear();
        self.reward_interval_countdown = None;
        if !same_context {
            self.metrics = None;
            self.accounts.clear();
            self.rewards.clear();
            self.chain_time_anchor = None;
        }
        self.generation
    }

    pub(super) fn current(&self, key: &GovernanceContextKey, generation: u64) -> bool {
        self.generation == generation && self.key.as_ref() == Some(key)
    }

    pub(in crate::root) fn apply_global(
        &mut self,
        key: &GovernanceContextKey,
        generation: u64,
        result: Result<StakingGlobalMetrics, Arc<str>>,
    ) -> bool {
        if !self.current(key, generation) {
            return false;
        }
        match result {
            Ok(metrics) => {
                self.global_ready = true;
                self.chain_time_anchor = Some((metrics.chain_time, Instant::now()));
                self.metrics = Some(metrics);
            }
            Err(error) => {
                self.global_ready = false;
                self.status = if self.metrics.is_some() {
                    StakingRefreshStatus::Stale
                } else {
                    StakingRefreshStatus::Error(error)
                };
            }
        }
        true
    }

    pub(super) fn apply_reward_interval(
        &mut self,
        key: &GovernanceContextKey,
        generation: u64,
        countdown: Option<RewardIntervalCountdown>,
    ) -> bool {
        if !self.current(key, generation) {
            return false;
        }
        self.reward_interval_countdown = countdown;
        true
    }

    pub(in crate::root) fn apply_account(
        &mut self,
        key: &GovernanceContextKey,
        generation: u64,
        uuid: String,
        result: Result<AccountStakeResult, Arc<str>>,
    ) -> bool {
        if !self.current(key, generation)
            || !key
                .participants
                .iter()
                .any(|participant| participant.uuid == uuid)
        {
            return false;
        }
        let view = match result {
            Ok(account) => StakingAccountView {
                voting_power: account.voting_power.map_err(Arc::from),
                balance: account.balance.map_err(Arc::from),
                stakes: account.stakes.map_err(Arc::from),
            },
            Err(error) => StakingAccountView {
                voting_power: Err(Arc::clone(&error)),
                balance: Err(Arc::clone(&error)),
                stakes: Err(error),
            },
        };
        self.accounts.insert(uuid.clone(), view);
        self.current_account_ids.insert(uuid);
        true
    }

    pub(in crate::root) fn apply_reward(
        &mut self,
        key: &GovernanceContextKey,
        generation: u64,
        uuid: String,
        token: Address,
        result: Result<Option<RewardEvidence>, Arc<str>>,
    ) -> bool {
        if !self.current(key, generation)
            || !key
                .participants
                .iter()
                .any(|participant| participant.uuid == uuid)
        {
            return false;
        }
        let reward = match result {
            Ok(None) => RewardView::Zero,
            Ok(Some(evidence)) if evidence.amount.is_zero() => RewardView::Zero,
            Ok(Some(evidence)) => RewardView::Positive {
                amount: evidence.amount,
                evidence: Box::new(evidence),
            },
            Err(error) => RewardView::Unavailable(error),
        };
        let reward_key = (uuid, token);
        self.positive_reward_captured_at.remove(&reward_key);
        if matches!(&reward, RewardView::Positive { .. }) {
            self.positive_reward_captured_at
                .insert(reward_key.clone(), Instant::now());
        }
        self.rewards.insert(reward_key.clone(), reward);
        self.current_reward_keys.insert(reward_key);
        true
    }

    /// Evidence behind a current positive row, used to build the claim selection.
    pub(in crate::root) fn positive_reward_evidence(
        &self,
        uuid: &str,
        token: Address,
    ) -> Option<&RewardEvidence> {
        match self.resolved_reward(uuid, token)? {
            RewardView::Positive { evidence, .. } => Some(evidence),
            RewardView::Zero | RewardView::Unavailable(_) => None,
        }
    }

    pub(in crate::root) fn apply_bulk_reward(
        &mut self,
        key: &GovernanceContextKey,
        generation: u64,
        uuid: String,
        tokens: Vec<Address>,
        result: &Result<Option<wallet_ops::RewardBatchEvidence>, Arc<str>>,
    ) -> bool {
        if !self.current(key, generation)
            || !key
                .participants
                .iter()
                .any(|participant| participant.uuid == uuid)
        {
            return false;
        }
        self.bulk_reward_evidence.remove(&uuid);
        if let Ok(Some(evidence)) = result
            && !evidence.expected_amounts.iter().all(U256::is_zero)
            && evidence.reward_tokens == tokens
        {
            self.bulk_reward_evidence
                .insert(uuid.clone(), (evidence.clone(), Instant::now()));
        }
        self.bulk_reward_readiness.insert(
            uuid.clone(),
            BulkRewardReadiness {
                tokens,
                ready: matches!(result, Ok(Some(_))),
            },
        );
        self.current_bulk_reward_keys.insert(uuid);
        true
    }

    /// Fails every cell of one participant so none stays loading.
    pub(in crate::root) fn fail_participant(
        &mut self,
        key: &GovernanceContextKey,
        generation: u64,
        uuid: String,
        tokens: &[Address],
        error: Arc<str>,
    ) {
        self.apply_account(key, generation, uuid.clone(), Err(Arc::clone(&error)));
        for &token in tokens {
            self.apply_reward(
                key,
                generation,
                uuid.clone(),
                token,
                Err(Arc::clone(&error)),
            );
        }
        self.apply_bulk_reward(key, generation, uuid, tokens.to_vec(), &Err(error));
    }

    /// The row this read produced for an account and token, or `None` while it is still loading.
    pub(in crate::root) fn resolved_reward(
        &self,
        uuid: &str,
        token: Address,
    ) -> Option<&RewardView> {
        let reward_key = (uuid.to_owned(), token);
        self.current_reward_keys
            .contains(&reward_key)
            .then(|| self.rewards.get(&reward_key))
            .flatten()
    }

    pub(in crate::root) const fn global_action_ready(&self) -> bool {
        self.global_ready
    }

    pub(in crate::root) fn account_action_ready(&self, uuid: &str) -> bool {
        self.global_ready
            && self.current_account_ids.contains(uuid)
            && self
                .accounts
                .get(uuid)
                .is_some_and(|account| account.stakes.is_ok())
    }

    pub(in crate::root) fn reward_action_ready(&self, uuid: &str, token: Address) -> bool {
        self.global_ready && self.positive_reward_evidence(uuid, token).is_some()
    }

    /// Whether any resolved reward row of this account is positive, which is what the account's
    /// claim selection needs before it can be opened with nothing selected yet.
    pub(in crate::root) fn account_has_positive_reward(&self, uuid: &str) -> bool {
        self.global_ready
            && self.current_reward_keys.iter().any(|reward_key| {
                reward_key.0 == uuid
                    && matches!(
                        self.rewards.get(reward_key),
                        Some(RewardView::Positive { .. })
                    )
            })
    }

    pub(in crate::root) fn cached_reward_evidence_at(
        &self,
        key: &GovernanceContextKey,
        uuid: &str,
        actor: Address,
        tokens: &[Address],
        now: Instant,
    ) -> Option<wallet_ops::RewardBatchEvidence> {
        if !self.global_ready
            || self.key.as_ref() != Some(key)
            || !key
                .participants
                .iter()
                .any(|participant| participant.uuid == uuid && participant.address == actor)
            || tokens.is_empty()
        {
            return None;
        }
        if tokens.len() == 1 {
            let token = tokens[0];
            let reward_key = (uuid.to_owned(), token);
            if !self.current_reward_keys.contains(&reward_key) {
                return None;
            }
            let captured_at = self.positive_reward_captured_at.get(&reward_key)?;
            if now
                .checked_duration_since(*captured_at)
                .is_none_or(|age| age > STAKING_REWARD_EVIDENCE_TTL)
            {
                return None;
            }
            let RewardView::Positive { evidence, .. } = self.rewards.get(&reward_key)? else {
                return None;
            };
            if evidence.token != token {
                return None;
            }
            return Some(evidence.as_ref().into());
        }
        if !self.current_bulk_reward_keys.contains(uuid) {
            return None;
        }
        let (evidence, captured_at) = self.bulk_reward_evidence.get(uuid)?;
        if now
            .checked_duration_since(*captured_at)
            .is_none_or(|age| age > STAKING_REWARD_EVIDENCE_TTL)
            || !self
                .bulk_reward_readiness
                .get(uuid)
                .is_some_and(|readiness| {
                    readiness.tokens == evidence.reward_tokens && readiness.ready
                })
        {
            return None;
        }
        // The cached read covers the configured token set; a claim carries the selection only.
        let projected = evidence.project(tokens).ok()?;
        (!projected.expected_amounts.iter().all(U256::is_zero)).then_some(projected)
    }

    pub(in crate::root) fn action_selection_ready(
        &self,
        selection: &StakingActionSelection,
    ) -> bool {
        if !self.global_ready {
            return false;
        }
        let Some(key) = self.key.as_ref() else {
            return false;
        };
        let Some(participant) = key
            .participants
            .iter()
            .find(|participant| participant.uuid == selection.actor_uuid)
        else {
            return false;
        };
        if participant.address != selection.actor {
            return false;
        }
        let uuid = participant.uuid.as_str();
        match &selection.kind {
            StakingActionKind::Stake => true,
            StakingActionKind::RewardClaimSelected { tokens } => {
                self.account_has_positive_reward(uuid)
                    && tokens
                        .iter()
                        .all(|&token| self.reward_action_ready(uuid, token))
            }
            StakingActionKind::Delegate { .. }
            | StakingActionKind::Undelegate { .. }
            | StakingActionKind::Unlock { .. }
            | StakingActionKind::PrincipalClaim { .. } => self.account_action_ready(uuid),
        }
    }

    pub(in crate::root) fn finish(&mut self, key: &GovernanceContextKey, generation: u64) -> bool {
        if !self.current(key, generation) {
            return false;
        }
        if !matches!(
            self.status,
            StakingRefreshStatus::Error(_) | StakingRefreshStatus::Stale
        ) {
            self.status = StakingRefreshStatus::Fresh;
        }
        true
    }

    pub(in crate::root) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.key = None;
        self.status = StakingRefreshStatus::Idle;
        self.global_ready = false;
        self.current_account_ids.clear();
        self.current_reward_keys.clear();
        self.current_bulk_reward_keys.clear();
        self.metrics = None;
        self.accounts.clear();
        self.rewards.clear();
        self.bulk_reward_readiness.clear();
        self.bulk_reward_evidence.clear();
        self.positive_reward_captured_at.clear();
        self.chain_time_anchor = None;
        self.reward_interval_countdown = None;
    }

    pub(super) fn chain_time(&self) -> Option<U256> {
        let (chain_time, captured_at) = self.chain_time_anchor.as_ref()?;
        chain_time.checked_add(U256::from(captured_at.elapsed().as_secs()))
    }
}

pub(super) fn reward_interval_countdown(
    metadata: &GovernorRewardsIntervalMetadata,
    chain_time: U256,
) -> Option<RewardIntervalCountdown> {
    if metadata.distribution_interval.is_zero() {
        return None;
    }
    let elapsed = chain_time.checked_sub(metadata.staking_deploy_time)?;
    let interval = elapsed.checked_div(metadata.distribution_interval)?;
    let next_interval = interval.checked_add(U256::ONE)?;
    let offset = next_interval.checked_mul(metadata.distribution_interval)?;
    let boundary = metadata.staking_deploy_time.checked_add(offset)?;
    Some(RewardIntervalCountdown { interval, boundary })
}

pub(super) fn remaining_reward_interval_seconds(
    countdown: RewardIntervalCountdown,
    chain_time: U256,
) -> Option<u64> {
    let remaining = countdown.boundary.checked_sub(chain_time)?;
    remaining.try_into().ok()
}

pub(super) fn nearest_unlock_boundary(state: &StakingReadState) -> Option<U256> {
    state
        .accounts
        .values()
        .filter_map(|account| account.stakes.as_ref().ok())
        .flat_map(|stakes| stakes.iter())
        .filter_map(|stake| {
            (stake.state == StakeState::Unlocking && !stake.locktime.is_zero())
                .then_some(stake.locktime)
        })
        .min()
}

pub(super) fn governance_boundary_crossed(state: &StakingReadState, chain_time: U256) -> bool {
    let interval_crossed = state
        .reward_interval_countdown
        .is_some_and(|countdown| chain_time >= countdown.boundary);
    interval_crossed
        || nearest_unlock_boundary(state).is_some_and(|boundary| chain_time >= boundary)
}

pub(super) fn arm_governance_time_tick(owner: Option<u64>, generation: u64) -> (Option<u64>, bool) {
    if owner == Some(generation) {
        (owner, false)
    } else {
        (Some(generation), true)
    }
}

pub(super) fn claim_governance_time_tick(
    owner: Option<u64>,
    generation: u64,
) -> (Option<u64>, bool) {
    if owner == Some(generation) {
        (None, true)
    } else {
        (owner, false)
    }
}

impl StakingAccountProjection {
    pub(super) const fn received_voting_power(self, voting_power: U256) -> U256 {
        voting_power.saturating_sub(self.active_self_delegated)
    }
}

pub(super) fn staking_account_projection(stakes: &[StakePosition]) -> StakingAccountProjection {
    stakes.iter().fold(
        StakingAccountProjection::default(),
        |mut projection, stake| {
            if stake.state != StakeState::Claimed {
                projection.unclaimed_principal =
                    projection.unclaimed_principal.saturating_add(stake.amount);
            }
            if stake.state == StakeState::Active {
                if stake.delegate == stake.owner {
                    projection.active_self_delegated = projection
                        .active_self_delegated
                        .saturating_add(stake.amount);
                } else {
                    projection.active_external_delegated = projection
                        .active_external_delegated
                        .saturating_add(stake.amount);
                }
            }
            projection
        },
    )
}

impl GovernanceState {
    pub(in crate::root) fn new(
        participant_picker: Entity<ParticipantPickerState>,
        proposal_action_amount_input: Entity<InputState>,
        staking_delegate_input: Entity<InputState>,
        reward_gas_fee: Eip1559GasFeeEditorState,
    ) -> Self {
        Self {
            tab: GovernanceTab::default(),
            staking: StakingReadState::default(),
            proposal_participation: ProposalParticipationState::default(),
            proposal_participation_reader: None,
            proposal_action_amount_input,
            action_flow: GovernanceActionFlowState::default(),
            staking_delegate_input,
            reward_gas_fee,
            compact_position_details: None,
            participant_picker,
            participant_picker_wallet: None,
            participant_picker_items: Vec::new(),
            staking_tables: BTreeMap::new(),
            participant_time_tick_generation: None,
        }
    }

    pub(super) fn sync_participant_picker(
        &mut self,
        wallet_id: Option<String>,
        items: Vec<ParticipantChoice>,
        values: &[String],
        window: &mut Window,
        cx: &mut App,
    ) {
        let wallet_changed = self.participant_picker_wallet != wallet_id;
        if !wallet_changed
            && items == self.participant_picker_items
            && self.participant_picker.read(cx).selected_values() == values
        {
            return;
        }
        self.participant_picker_wallet = wallet_id;
        self.participant_picker_items.clone_from(&items);
        self.participant_picker.update(cx, |picker, cx| {
            let query = if wallet_changed {
                SharedString::default()
            } else {
                picker.query(cx)
            };
            picker.set_items(SearchableVec::new(items), window, cx);
            picker.set_selected_values(values, window, cx);
            if !query.is_empty() {
                picker.set_query(query, window, cx);
            }
        });
    }

    pub(in crate::root) fn invalidate_action(&mut self) {
        self.action_flow.invalidate();
        self.reward_gas_fee.refresh_id = self.reward_gas_fee.refresh_id.wrapping_add(1);
        self.reward_gas_fee.refreshing = false;
    }

    pub(super) fn clear_stale_position_details(&mut self) {
        let Some((actor_uuid, stake_id)) = self.compact_position_details.as_ref() else {
            return;
        };
        let still_exists = self
            .staking
            .accounts
            .get(actor_uuid)
            .and_then(|account| account.stakes.as_ref().ok())
            .is_some_and(|stakes| stakes.iter().any(|stake| stake.id == *stake_id));
        if !still_exists {
            self.compact_position_details = None;
        }
    }
}

impl GovernanceActionFlowState {
    pub(in crate::root) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.recipe = None;
        self.operation = None;
        self.reward_selection_estimate = None;
        self.seed_reward_selection = false;
        // Bumped rather than reset so an estimate still in flight can never match a later one.
        self.reward_selection_generation = self.reward_selection_generation.wrapping_add(1);
    }

    /// The open action's operation client for `actor`. Another actor replaces it.
    pub(in crate::root) fn operation(
        &mut self,
        actor: Address,
        http: &wallet_ops::HttpContext,
    ) -> wallet_ops::OperationHttpClientSlot {
        match &self.operation {
            Some((owner, operation)) if *owner == actor => operation.clone(),
            _ => {
                let operation = wallet_ops::OperationHttpClientSlot::new(http.clone());
                self.operation = Some((actor, operation.clone()));
                operation
            }
        }
    }

    /// The live claim selection, when the open action is one.
    pub(in crate::root) fn reward_claim_selection_tokens(&self) -> Option<&[Address]> {
        match &self.staking_selection()?.kind {
            StakingActionKind::RewardClaimSelected { tokens } => Some(tokens),
            StakingActionKind::Stake
            | StakingActionKind::Delegate { .. }
            | StakingActionKind::Undelegate { .. }
            | StakingActionKind::Unlock { .. }
            | StakingActionKind::PrincipalClaim { .. } => None,
        }
    }

    /// Add or drop one token in the open claim selection, keeping it unique and ascending by
    /// address. Returns whether a claim selection was open to change.
    pub(in crate::root) fn set_reward_claim_selection_token(
        &mut self,
        token: Address,
        selected: bool,
    ) -> bool {
        let Some(tokens) = self.reward_claim_selection_tokens_mut() else {
            return false;
        };
        match (tokens.binary_search(&token), selected) {
            (Err(index), true) => tokens.insert(index, token),
            (Ok(index), false) => {
                tokens.remove(index);
            }
            (Ok(_), true) | (Err(_), false) => {}
        }
        true
    }

    /// Replace the open claim selection, keeping it unique and ascending by address.
    pub(in crate::root) fn set_reward_claim_selection_all(
        &mut self,
        mut selected: Vec<Address>,
    ) -> bool {
        let Some(tokens) = self.reward_claim_selection_tokens_mut() else {
            return false;
        };
        selected.sort_unstable();
        selected.dedup();
        *tokens = selected;
        true
    }

    fn reward_claim_selection_tokens_mut(&mut self) -> Option<&mut Vec<Address>> {
        match self.selection.as_mut()? {
            GovernanceActionSelection::Staking(StakingActionSelection {
                kind: StakingActionKind::RewardClaimSelected { tokens },
                ..
            }) => Some(tokens),
            GovernanceActionSelection::Staking(_) | GovernanceActionSelection::Proposal(_) => None,
        }
    }

    /// Start a selection estimate: drop the prepared recipe and draft, clear the error, and mark
    /// the estimate pending, including the baseline for an empty selection. Returns the generation its result must
    /// carry to be stored.
    pub(in crate::root) fn begin_reward_selection_estimate(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.recipe = None;
        self.draft = None;
        self.error = None;
        self.reward_selection_generation = self.reward_selection_generation.wrapping_add(1);
        self.reward_selection_estimate = self
            .reward_claim_selection_tokens()
            .is_some()
            .then_some(RewardSelectionEstimateState::Pending);
        self.reward_selection_generation
    }

    /// Store a selection estimate result, dropping one whose selection has already moved on.
    pub(in crate::root) fn apply_reward_selection_estimate(
        &mut self,
        generation: u64,
        result: Result<RewardSelectionEstimate, Arc<str>>,
    ) -> bool {
        if self.reward_selection_generation != generation {
            return false;
        }
        self.reward_selection_estimate = Some(
            result.map_or_else(RewardSelectionEstimateState::Unavailable, |estimate| {
                RewardSelectionEstimateState::Estimated(Box::new(estimate))
            }),
        );
        true
    }

    pub(in crate::root) fn set_proposal_selection(&mut self, selection: ProposalActionSelection) {
        self.selection = Some(GovernanceActionSelection::Proposal(selection));
    }

    pub(in crate::root) fn set_staking_selection(&mut self, selection: StakingActionSelection) {
        self.selection = Some(GovernanceActionSelection::Staking(selection));
    }

    pub(in crate::root) const fn staking_selection(&self) -> Option<&StakingActionSelection> {
        match self.selection.as_ref() {
            Some(GovernanceActionSelection::Staking(selection)) => Some(selection),
            _ => None,
        }
    }

    pub(in crate::root) fn proposal_recipe_matches(
        &self,
        key: &ProposalParticipationKey,
        selection: ProposalActionSelection,
        amount: Option<U256>,
    ) -> bool {
        matches!(
            self.recipe.as_ref(),
            Some(GovernanceDraftRecipe::Proposal {
                key: current_key,
                selection: current_selection,
                amount: current_amount,
                ..
            }) if current_key == key && *current_selection == selection && *current_amount == amount
        )
    }

    pub(in crate::root) fn staking_recipe_matches(
        &self,
        selection: &StakingActionSelection,
        context_key: &GovernanceContextKey,
        amount_input: &str,
        delegate_input: &str,
        token_decimals: Option<u8>,
    ) -> bool {
        matches!(
            self.recipe.as_ref(),
            Some(GovernanceDraftRecipe::Staking {
                selection: current_selection,
                context_key: current_key,
                amount_input: current_amount,
                delegate_input: current_delegate,
                token_decimals: current_decimals,
            }) if current_selection == selection
                && current_key == context_key
                && current_amount == amount_input
                && current_delegate == delegate_input
                && *current_decimals == token_decimals
        )
    }
}

impl StakingActionKind {
    pub(super) const fn stake_id(&self) -> Option<U256> {
        match self {
            Self::Delegate { stake_id }
            | Self::Undelegate { stake_id }
            | Self::Unlock { stake_id }
            | Self::PrincipalClaim { stake_id } => Some(*stake_id),
            _ => None,
        }
    }

    pub(super) const fn is_compose_action(&self) -> bool {
        matches!(self, Self::Stake | Self::Delegate { .. })
    }

    pub(super) const fn is_reward_action(&self) -> bool {
        matches!(self, Self::RewardClaimSelected { .. })
    }
}

pub(in crate::root) const fn staking_action_title(kind: &StakingActionKind) -> &'static str {
    match kind {
        StakingActionKind::Stake => "Stake RAIL",
        StakingActionKind::Delegate { .. } => "Delegate stake",
        StakingActionKind::Undelegate { .. } => "Undelegate stake",
        StakingActionKind::Unlock { .. } => "Unlock stake",
        StakingActionKind::PrincipalClaim { .. } => "Claim principal",
        StakingActionKind::RewardClaimSelected { .. } => "Claim rewards",
    }
}
