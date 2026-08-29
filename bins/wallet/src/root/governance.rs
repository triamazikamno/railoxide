use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256};
use futures_util::{StreamExt, future::BoxFuture};
use gpui::{
    App, AppContext as _, Context, Entity, Focusable, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, WindowExt,
    alert::Alert,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    popover::Popover,
    scroll::{ScrollableElement, Scrollbar},
    spinner::Spinner,
    tab::{Tab, TabBar},
    table::{Column, Table, TableDelegate, TableState},
    tooltip::Tooltip,
};
use railgun_ui::{format_usd_micro_value, governance_contracts, short_address};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{
    app_button, app_button_base, app_input, app_muted_text, app_strong_text, app_text,
};
use ui::format::format_compact_duration;
use ui::theme::{self, APP_MONO_FONT_FAMILY};
use wallet_ops::{
    AccountStakeResult, GovernorRewardsIntervalMetadata, RewardEvidence, StakePosition, StakeState,
    StakingGlobalMetrics, TokenAnchorRateCache, fetch_account_snapshots,
    fetch_account_snapshots_multi, fetch_account_stakes, fetch_interval_metadata,
    fetch_reward_batch_evidence, fetch_reward_evidence, fetch_reward_evidence_multi,
    fetch_staking_global_metrics, validate_governance_deployment,
    vault::{PublicAccountMetadata, PublicAccountStatus},
};
use wallet_ops::{
    GovernanceActionContext, GovernanceActionIntent, GovernanceContractKind,
    PublicActionGasFeeSelection, PublicAdvancedTransactionEstimateRequest,
    fetch_governance_token_balance_allowance, plan_delegate, plan_principal_claim, plan_stake,
    plan_undelegate, plan_unlock,
};

use super::governance_action::{
    GovernanceContinuation, GovernanceDraftRecipe, GovernanceRefreshTarget, GovernanceSpendDraft,
    GovernanceStakingReviewProjection, ProposalActionKind, ProposalActionSelection,
    ProposalParticipationKey, ProposalParticipationState, build_typed_governance_spend_draft,
};
use super::participant::normalize_participant_ids;
use super::proposals::{format_compact_rail_amount, format_date_short};
use super::public_account::public_account_display_label;
use super::tokens::{
    format_send_amount_input, format_token_amount_for_display, token_display_metadata,
};
use super::ui_helpers::token_label_row;
use super::{WalletRoot, app_refresh_button, app_status_tag};
use crate::assets::{PIGGY_BANK_ICON_PATH, RailgunSidebarIcon, USERS_ICON_PATH};

const GOVERNANCE_HEADER_HEIGHT: gpui::Pixels = px(52.0);
const GOVERNANCE_COMPACT_HEADER_BREAKPOINT: gpui::Pixels = px(1100.0);
const GOVERNANCE_PARTICIPANT_TRIGGER_WIDTH: gpui::Pixels = px(200.0);
const GOVERNANCE_COMPACT_PARTICIPANT_TRIGGER_WIDTH: gpui::Pixels = px(120.0);
const GOVERNANCE_PARTICIPANT_POPUP_WIDTH: gpui::Pixels = px(280.0);
const GOVERNANCE_CONTENT_WIDTH: gpui::Pixels = px(1080.0);
const GOVERNANCE_TIME_TICK: Duration = Duration::from_mins(1);
const STAKING_TABLE_COMPACT_BREAKPOINT: gpui::Pixels = px(720.0);
const STAKING_TABLE_COMPACT_ACTIONS_WIDTH: gpui::Pixels = px(224.0);
const STAKING_ACCOUNT_HEADER_STACK_BREAKPOINT: gpui::Pixels = px(860.0);
const STAKING_COLLAPSED_SIDEBAR_WIDTH: gpui::Pixels = px(48.0);
const STAKING_CARD_HORIZONTAL_CHROME: gpui::Pixels = px(34.0);
const STAKING_TABLE_STAKE_WIDTH: gpui::Pixels = px(76.0);
const STAKING_TABLE_AMOUNT_WIDTH: gpui::Pixels = px(120.0);
const STAKING_TABLE_COMPACT_DELEGATE_MIN_WIDTH: gpui::Pixels = px(88.0);
const STAKING_TABLE_STATE_WIDTH: gpui::Pixels = px(118.0);
const STAKING_TABLE_ACTIONS_WIDTH: gpui::Pixels = px(288.0);
const STAKING_TABLE_MIN_WIDTH: gpui::Pixels = px(76.0 + 120.0 + 88.0 + 118.0 + 224.0);
const STAKING_OVERVIEW_ROW_HEIGHT: gpui::Pixels = px(40.0);
const STAKING_TABLE_SCROLLBAR_WIDTH: gpui::Pixels = px(16.0);
const STAKING_REWARD_EVIDENCE_CONCURRENCY: usize = 4;
const STAKING_REWARD_EVIDENCE_TTL: Duration = Duration::from_hours(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RewardIntervalCountdown {
    interval: U256,
    boundary: U256,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum GovernanceTab {
    #[default]
    Proposals,
    Staking,
}

pub(super) async fn build_staking_action_draft(
    selection: StakingActionSelection,
    context_key: GovernanceContextKey,
    wallet_id: Arc<str>,
    actor_source: wallet_ops::vault::PublicAccountSource,
    amount_input: String,
    delegate_input: String,
    view_session: Arc<wallet_ops::vault::DesktopViewSession>,
    vault_store: Arc<wallet_ops::vault::DesktopVaultStore>,
    token_decimals: Option<u8>,
    effective_chain: Option<wallet_ops::settings::EffectiveChainConfig>,
    http: wallet_ops::HttpContext,
    reward_evidence_mode: RewardEvidenceMode,
    gas_fee_selection: PublicActionGasFeeSelection,
) -> Result<GovernanceSpendDraft, String> {
    validate_governance_deployment(context_key.chain_id, effective_chain.as_ref(), &http)
        .await
        .map_err(|error| error.to_string())?;
    let recipe = GovernanceDraftRecipe::Staking {
        selection: selection.clone(),
        context_key: context_key.clone(),
        amount_input: amount_input.clone(),
        delegate_input: delegate_input.clone(),
        token_decimals,
    };
    let chain_id = context_key.chain_id;
    let actor_uuid: Arc<str> = Arc::from(selection.actor_uuid.clone());
    let contracts = governance_contracts(chain_id)
        .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
    let actor = selection.actor;
    let target = GovernanceRefreshTarget::Staking(context_key);
    let (contract, contract_kind, observed_state, intent, workflow, continuation, staking_review) =
        match selection.kind {
            StakingActionKind::Stake => {
                let amount = wallet_ops::parse_send_amount(amount_input.trim(), token_decimals)
                    .map_err(|error| error.to_string())?;
                let amount = (!amount.is_zero())
                    .then_some(amount)
                    .ok_or_else(|| "Amount must be greater than zero".to_owned())?;
                let (balance, allowance) = fetch_governance_token_balance_allowance(
                    chain_id,
                    actor,
                    effective_chain.as_ref(),
                    &http,
                )
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Governance token is not deployed on this chain".to_owned())?;
                let plan = plan_stake(actor, contracts.staking, balance, allowance, amount)
                    .map_err(|error| error.to_string())?;
                let (contract, intent, workflow) = if let Some(approval) = plan.approval.clone() {
                    (
                        contracts.governance_token,
                        approval,
                        Some(wallet_ops::GovernanceWorkflow::StakeApproval(plan.clone())),
                    )
                } else {
                    (
                        contracts.staking,
                        plan.stake
                            .ok_or_else(|| "Stake plan did not produce a stake call".to_owned())?,
                        None,
                    )
                };
                (
                    contract,
                    if contract == contracts.governance_token {
                        GovernanceContractKind::GovernanceToken
                    } else {
                        GovernanceContractKind::Staking
                    },
                    plan.observed_state,
                    intent,
                    workflow,
                    None,
                    Some(GovernanceStakingReviewProjection::Stake { amount }),
                )
            }
            StakingActionKind::Delegate { stake_id } => {
                let delegate = Address::from_str(delegate_input.trim())
                    .map_err(|_| "Enter a valid delegate address".to_owned())?;
                let position = fresh_stake_position(
                    chain_id,
                    actor,
                    stake_id,
                    effective_chain.as_ref(),
                    &http,
                )
                .await?;
                let plan =
                    plan_delegate(actor, &position, delegate).map_err(|error| error.to_string())?;
                (
                    contracts.staking,
                    GovernanceContractKind::Staking,
                    plan.observed_state,
                    plan.intent,
                    None,
                    None,
                    Some(GovernanceStakingReviewProjection::Delegation(plan.evidence)),
                )
            }
            StakingActionKind::Undelegate { stake_id } => {
                let position = fresh_stake_position(
                    chain_id,
                    actor,
                    stake_id,
                    effective_chain.as_ref(),
                    &http,
                )
                .await?;
                let plan = plan_undelegate(actor, &position).map_err(|error| error.to_string())?;
                (
                    contracts.staking,
                    GovernanceContractKind::Staking,
                    plan.observed_state,
                    plan.intent,
                    None,
                    None,
                    Some(GovernanceStakingReviewProjection::Delegation(plan.evidence)),
                )
            }
            StakingActionKind::Unlock { stake_id } => {
                let position = fresh_stake_position(
                    chain_id,
                    actor,
                    stake_id,
                    effective_chain.as_ref(),
                    &http,
                )
                .await?;
                let metrics =
                    fetch_staking_global_metrics(chain_id, effective_chain.as_ref(), &http)
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
                let projected_claim_timestamp = metrics
                    .chain_time
                    .checked_add(metrics.stake_locktime)
                    .ok_or_else(|| "Unlock claim timestamp overflowed".to_owned())?;
                let plan = plan_unlock(actor, &position).map_err(|error| error.to_string())?;
                let (intent, observed_state, workflow, previous_delegate, owner, amount) =
                    match plan {
                        wallet_ops::UnlockPlan::Direct(plan) => (
                            plan.intent,
                            plan.observed_state,
                            None,
                            plan.delegate,
                            plan.owner,
                            plan.amount,
                        ),
                        wallet_ops::UnlockPlan::UndelegateFirst(plan) => (
                            plan.intent.clone(),
                            plan.observed_state,
                            Some(wallet_ops::GovernanceWorkflow::UndelegateThenUnlock(plan)),
                            position.delegate,
                            position.owner,
                            position.amount,
                        ),
                    };
                (
                    contracts.staking,
                    GovernanceContractKind::Staking,
                    observed_state,
                    intent,
                    workflow,
                    None,
                    Some(GovernanceStakingReviewProjection::Unlock {
                        owner,
                        stake_id,
                        amount,
                        previous_delegate,
                        stake_locktime: metrics.stake_locktime,
                        projected_claim_timestamp,
                    }),
                )
            }
            StakingActionKind::PrincipalClaim { stake_id } => {
                let position = fresh_stake_position(
                    chain_id,
                    actor,
                    stake_id,
                    effective_chain.as_ref(),
                    &http,
                )
                .await?;
                let metrics =
                    fetch_staking_global_metrics(chain_id, effective_chain.as_ref(), &http)
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
                let plan = plan_principal_claim(actor, &position, metrics.chain_time)
                    .map_err(|error| error.to_string())?;
                let review = GovernanceStakingReviewProjection::PrincipalClaim(plan.clone());
                (
                    contracts.staking,
                    GovernanceContractKind::Staking,
                    plan.observed_state,
                    plan.intent,
                    None,
                    None,
                    Some(review),
                )
            }
            StakingActionKind::RewardClaim { token } => {
                let (plan, step, evidence) = plan_reward_claim_draft(
                    chain_id,
                    actor,
                    Some(token),
                    effective_chain.as_ref(),
                    &http,
                    reward_evidence_mode.clone(),
                )
                .await?;
                (
                    contracts.governor_rewards,
                    GovernanceContractKind::GovernorRewards,
                    plan.fingerprint,
                    step.intent,
                    None,
                    Some(GovernanceContinuation::Reward {
                        progress: wallet_ops::RewardClaimProgress::new(plan),
                        evidence,
                    }),
                    None,
                )
            }
            StakingActionKind::RewardClaimAll => {
                let (plan, step, evidence) = plan_reward_claim_draft(
                    chain_id,
                    actor,
                    None,
                    effective_chain.as_ref(),
                    &http,
                    reward_evidence_mode,
                )
                .await?;
                (
                    contracts.governor_rewards,
                    GovernanceContractKind::GovernorRewards,
                    plan.fingerprint,
                    step.intent,
                    None,
                    Some(GovernanceContinuation::Reward {
                        progress: wallet_ops::RewardClaimProgress::new(plan),
                        evidence,
                    }),
                    None,
                )
            }
        };
    build_typed_governance_spend_draft(
        target,
        wallet_id,
        actor_uuid,
        actor,
        actor_source,
        contract,
        contract_kind,
        observed_state,
        intent,
        view_session,
        vault_store,
        effective_chain,
        http,
        gas_fee_selection,
        workflow,
        continuation,
        staking_review,
        recipe,
    )
    .await
}

async fn plan_reward_claim_draft(
    chain_id: u64,
    actor: Address,
    token: Option<Address>,
    effective_chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    http: &wallet_ops::HttpContext,
    reward_evidence_mode: RewardEvidenceMode,
) -> Result<
    (
        wallet_ops::RewardClaimPlan,
        wallet_ops::RewardClaimStepPlan,
        wallet_ops::RewardBatchEvidence,
    ),
    String,
> {
    let mut tokens = governance_contracts(chain_id)
        .ok_or_else(|| "Reward contracts unavailable".to_owned())?
        .reward_tokens
        .iter()
        .map(|entry| entry.token)
        .collect::<Vec<_>>();
    tokens.sort();
    let requested_tokens = token.map_or_else(|| tokens.clone(), |token| vec![token]);
    let evidence = match reward_evidence_mode {
        RewardEvidenceMode::CachedInitial { evidence } => {
            if evidence.reward_tokens != requested_tokens {
                return Err(
                    "Cached reward evidence token set changed; refresh before reviewing again"
                        .to_owned(),
                );
            }
            evidence
        }
        RewardEvidenceMode::Fresh => {
            let metadata =
                fetch_interval_metadata(chain_id, &requested_tokens, effective_chain, http)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "Reward metadata unavailable".to_owned())?;
            let snapshots = fetch_account_snapshots(
                chain_id,
                actor,
                effective_chain,
                http,
                wallet_ops::MulticallChunkSize::default(),
            )
            .await
            .map_err(|error| error.to_string())?;
            if let Some(token) = token {
                let evidence = fetch_reward_evidence(
                    chain_id,
                    actor,
                    token,
                    &metadata,
                    &snapshots,
                    effective_chain,
                    http,
                    wallet_ops::MulticallChunkSize::default(),
                )
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "No unclaimed reward is available".to_owned())?;
                wallet_ops::RewardBatchEvidence {
                    reward_tokens: vec![token],
                    starting_interval: evidence.starting_interval,
                    ending_interval: evidence.ending_interval,
                    staking_intervals: evidence.staking_intervals,
                    hints: evidence.hints,
                    claimed_intervals: vec![evidence.claimed_intervals],
                    expected_amounts: vec![evidence.amount],
                }
            } else {
                fetch_reward_batch_evidence(
                    chain_id,
                    actor,
                    &requested_tokens,
                    &metadata,
                    &snapshots,
                    effective_chain,
                    http,
                    wallet_ops::MulticallChunkSize::default(),
                )
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "No unclaimed reward is available".to_owned())?
            }
        }
    };
    let steps =
        plan_reward_steps_for_evidence(chain_id, actor, &evidence, effective_chain, http).await?;
    let plan = wallet_ops::plan_reward_claim_batch(chain_id, actor, actor, true, &evidence, &steps)
        .map_err(|error| error.to_string())?;
    let step = plan
        .steps
        .first()
        .cloned()
        .ok_or_else(|| "Reward claim plan is empty".to_owned())?;
    Ok((plan, step, evidence))
}

async fn plan_reward_steps_for_evidence(
    chain_id: u64,
    actor: Address,
    evidence: &wallet_ops::RewardBatchEvidence,
    effective_chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    http: &wallet_ops::HttpContext,
) -> Result<Vec<wallet_ops::RewardClaimStep>, String> {
    let gas_ceiling = wallet_ops::fetch_latest_block_gas_limit(chain_id, effective_chain, http)
        .await
        .map_err(|error| error.to_string())?;
    let contract = contracts_for_reward(chain_id)?;
    let aggregate_action = GovernanceActionIntent::RewardClaim {
        reward_tokens: evidence.reward_tokens.clone(),
        starting_interval: evidence.starting_interval,
        ending_interval: evidence.ending_interval,
        snapshot_hints: evidence.hints.clone(),
        expected_amounts: evidence.expected_amounts.clone(),
    };
    if let Ok(estimate) = estimate_reward_intent(
        chain_id,
        actor,
        contract,
        aggregate_action,
        effective_chain,
        http,
    )
    .await
        && estimate.gas_limit <= gas_ceiling
    {
        return Ok(vec![wallet_ops::RewardClaimStep {
            starting_interval: evidence.starting_interval,
            ending_interval: evidence.ending_interval,
            reward_tokens: evidence.reward_tokens.clone(),
            subtotal: U256::ZERO,
            expected_amounts: evidence.expected_amounts.clone(),
            estimated_gas: estimate.gas_limit,
        }]);
    }
    exact_reward_steps(
        chain_id,
        actor,
        evidence,
        gas_ceiling,
        effective_chain,
        http,
    )
    .await
}

fn contracts_for_reward(chain_id: u64) -> Result<Address, String> {
    governance_contracts(chain_id)
        .map(|contracts| contracts.governor_rewards)
        .ok_or_else(|| "Reward contracts unavailable".to_owned())
}

async fn estimate_reward_intent(
    chain_id: u64,
    actor: Address,
    contract: Address,
    action: GovernanceActionIntent,
    effective_chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    http: &wallet_ops::HttpContext,
) -> Result<wallet_ops::PublicAdvancedTransactionEstimate, String> {
    let context = GovernanceActionContext {
        private_wallet_uuid: String::new(),
        chain_id,
        public_account_uuid: String::new(),
        actor,
        contract,
        contract_kind: GovernanceContractKind::GovernorRewards,
        observed_state: B256::ZERO,
    };
    let resolved = action
        .resolve(&context)
        .map_err(|error| error.to_string())?;
    wallet_ops::estimate_public_advanced_transaction(
        PublicAdvancedTransactionEstimateRequest {
            chain_id,
            effective_chain: effective_chain.cloned(),
            from: actor,
            intent: resolved.raw,
            gas_fee: PublicActionGasFeeSelection::Auto,
            access_list: None,
        },
        http,
    )
    .await
    .map_err(|error| error.to_string())
}

async fn exact_reward_steps(
    chain_id: u64,
    actor: Address,
    evidence: &wallet_ops::RewardBatchEvidence,
    gas_ceiling: u64,
    effective_chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    http: &wallet_ops::HttpContext,
) -> Result<Vec<wallet_ops::RewardClaimStep>, String> {
    let amounts = wallet_ops::fetch_reward_batch_interval_amounts(
        chain_id,
        actor,
        evidence,
        effective_chain,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    if amounts.is_empty() {
        return Err("Reward interval amounts are empty".to_owned());
    }
    for (index, amount) in amounts.iter().enumerate() {
        let expected = evidence.starting_interval + U256::from(index);
        if amount.interval != expected {
            return Err("Reward interval amounts are not consecutive".to_owned());
        }
    }
    let contract = contracts_for_reward(chain_id)?;
    let mut pending = vec![(0_usize, amounts.len())];
    let mut accepted = Vec::new();
    while let Some((start, end)) = pending.pop() {
        let mut expected_amounts = vec![U256::ZERO; evidence.reward_tokens.len()];
        for amount in &amounts[start..end] {
            for (total, value) in expected_amounts.iter_mut().zip(&amount.subtotals) {
                *total = total
                    .checked_add(*value)
                    .ok_or_else(|| "Reward interval subtotal overflowed".to_owned())?;
            }
        }
        if expected_amounts.iter().all(U256::is_zero) {
            continue;
        }
        let action = GovernanceActionIntent::RewardClaim {
            reward_tokens: evidence.reward_tokens.clone(),
            starting_interval: amounts[start].interval,
            ending_interval: amounts[end - 1].interval,
            snapshot_hints: evidence.hints[start..end].to_vec(),
            expected_amounts: expected_amounts.clone(),
        };
        match estimate_reward_intent(chain_id, actor, contract, action, effective_chain, http).await
        {
            Ok(estimate) if estimate.gas_limit <= gas_ceiling => {
                accepted.push(wallet_ops::RewardClaimStep {
                    starting_interval: amounts[start].interval,
                    ending_interval: amounts[end - 1].interval,
                    reward_tokens: evidence.reward_tokens.clone(),
                    subtotal: U256::ZERO,
                    expected_amounts,
                    estimated_gas: estimate.gas_limit,
                });
            }
            Ok(_) | Err(_) => {
                let Some(boundary) =
                    wallet_ops::reward_batch_positive_split_boundary(&amounts, start, end)
                else {
                    return Err("Reward range does not fit the gas ceiling".to_owned());
                };
                pending.push((boundary + 1, end));
                pending.push((start, boundary + 1));
            }
        }
    }
    accepted.sort_by_key(|step| step.starting_interval);
    let mut expected = evidence.starting_interval;
    let mut total = vec![U256::ZERO; evidence.reward_tokens.len()];
    for step in &accepted {
        if step.starting_interval != expected || step.expected_amounts.iter().all(U256::is_zero) {
            return Err("Reward ranges are not consecutive positive coverage".to_owned());
        }
        expected = step
            .ending_interval
            .checked_add(U256::from(1_u8))
            .ok_or_else(|| "Reward interval overflowed".to_owned())?;
        for (total, amount) in total.iter_mut().zip(&step.expected_amounts) {
            *total = total
                .checked_add(*amount)
                .ok_or_else(|| "Reward subtotal overflowed".to_owned())?;
        }
    }
    if expected != evidence.ending_interval + U256::from(1_u8) || total != evidence.expected_amounts
    {
        return Err("Reward ranges do not cover the reviewed evidence".to_owned());
    }
    Ok(accepted)
}

async fn fresh_stake_position(
    chain_id: u64,
    actor: Address,
    stake_id: U256,
    effective_chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    http: &wallet_ops::HttpContext,
) -> Result<StakePosition, String> {
    let metrics = fetch_staking_global_metrics(chain_id, effective_chain, http)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
    let result = fetch_account_stakes(
        chain_id,
        &[actor],
        metrics.chain_time,
        effective_chain,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?
    .into_iter()
    .next()
    .ok_or_else(|| "Stake owner read returned no account".to_owned())?;
    let stakes = result.stakes?;
    stakes
        .into_iter()
        .find(|stake| stake.id == stake_id)
        .ok_or_else(|| "Stake no longer exists; refresh before authorizing".to_owned())
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct GovernanceParticipantIdentity {
    pub uuid: String,
    pub address: Address,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BulkRewardReadiness {
    tokens: Vec<Address>,
    ready: bool,
}

#[derive(Clone, Debug)]
pub(super) enum RewardEvidenceMode {
    Fresh,
    CachedInitial {
        evidence: wallet_ops::RewardBatchEvidence,
    },
}

enum StakingRewardRefreshResult {
    PerToken {
        participant: GovernanceParticipantIdentity,
        tokens: Vec<Address>,
        rewards: Result<Vec<wallet_ops::RewardEvidenceResult>, Arc<str>>,
    },
    Bulk {
        participant: GovernanceParticipantIdentity,
        tokens: Vec<Address>,
        evidence: Result<Option<wallet_ops::RewardBatchEvidence>, Arc<str>>,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct GovernanceContextKey {
    pub wallet_id: Option<String>,
    pub chain_id: u64,
    pub participants: Vec<GovernanceParticipantIdentity>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ParticipantSummary {
    pub selected: usize,
    pub inactive: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ParticipantChoice {
    pub uuid: String,
    pub label: String,
    pub address: Address,
    pub global: bool,
    pub inactive: bool,
    pub selected: bool,
}

pub(super) fn participant_summary(
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

pub(super) fn participant_choices(
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
            selected: selected.contains(&account.public_account_uuid),
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StakingAccountView {
    pub voting_power: Result<U256, Arc<str>>,
    pub balance: Result<U256, Arc<str>>,
    pub stakes: Result<Vec<StakePosition>, Arc<str>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RewardView {
    Zero,
    Positive {
        amount: U256,
        evidence: Box<RewardEvidence>,
    },
    Unavailable(Arc<str>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RewardUsdState {
    Loading,
    Unavailable,
    Value(U256),
}

fn cached_reward_usd_value(
    chain_id: u64,
    token: Address,
    amount: U256,
    anchor_cache: &TokenAnchorRateCache,
) -> Option<U256> {
    anchor_cache.cached_token_usd_micro_value(chain_id, token, amount)
}

fn reward_usd_state(
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

fn reward_usd_label(
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

fn reward_usd_total(
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum StakingRefreshStatus {
    #[default]
    Idle,
    Loading,
    Fresh,
    Stale,
    Error(Arc<str>),
}

#[derive(Clone, Debug)]
pub(super) struct StakingReadState {
    pub key: Option<GovernanceContextKey>,
    pub generation: u64,
    pub status: StakingRefreshStatus,
    pub global_ready: bool,
    pub current_account_ids: BTreeSet<String>,
    pub current_reward_keys: BTreeSet<(String, Address)>,
    pub metrics: Option<StakingGlobalMetrics>,
    pub accounts: BTreeMap<String, StakingAccountView>,
    pub rewards: BTreeMap<(String, Address), RewardView>,
    bulk_reward_readiness: BTreeMap<String, BulkRewardReadiness>,
    bulk_reward_evidence: BTreeMap<String, (wallet_ops::RewardBatchEvidence, Instant)>,
    positive_reward_captured_at: BTreeMap<(String, Address), Instant>,
    current_bulk_reward_keys: BTreeSet<String>,
    pub chain_time_anchor: Option<(U256, Instant)>,
    reward_interval_countdown: Option<RewardIntervalCountdown>,
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
    pub(super) fn begin(&mut self, key: GovernanceContextKey) -> u64 {
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

    fn current(&self, key: &GovernanceContextKey, generation: u64) -> bool {
        self.generation == generation && self.key.as_ref() == Some(key)
    }

    pub(super) fn apply_global(
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

    fn apply_reward_interval(
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

    pub(super) fn apply_account(
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

    pub(super) fn apply_reward(
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

    pub(super) fn apply_bulk_reward(
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

    pub(super) const fn global_action_ready(&self) -> bool {
        self.global_ready
    }

    pub(super) fn account_action_ready(&self, uuid: &str) -> bool {
        self.global_ready
            && self.current_account_ids.contains(uuid)
            && self
                .accounts
                .get(uuid)
                .is_some_and(|account| account.stakes.is_ok())
    }

    pub(super) fn reward_action_ready(&self, uuid: &str, token: Address) -> bool {
        self.global_ready
            && self.current_reward_keys.contains(&(uuid.to_owned(), token))
            && matches!(
                self.rewards.get(&(uuid.to_owned(), token)),
                Some(RewardView::Positive { .. })
            )
    }

    pub(super) fn reward_claim_all_ready(&self, uuid: &str, tokens: &[Address]) -> bool {
        self.global_ready
            && !tokens.is_empty()
            && self.current_bulk_reward_keys.contains(uuid)
            && self
                .bulk_reward_readiness
                .get(uuid)
                .is_some_and(|readiness| readiness.tokens == tokens && readiness.ready)
    }

    pub(super) fn cached_reward_evidence_at(
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
            return Some(wallet_ops::RewardBatchEvidence {
                reward_tokens: vec![token],
                starting_interval: evidence.starting_interval,
                ending_interval: evidence.ending_interval,
                staking_intervals: evidence.staking_intervals.clone(),
                hints: evidence.hints.clone(),
                claimed_intervals: vec![evidence.claimed_intervals.clone()],
                expected_amounts: vec![evidence.amount],
            });
        }
        if !self.current_bulk_reward_keys.contains(uuid) {
            return None;
        }
        let (evidence, captured_at) = self.bulk_reward_evidence.get(uuid)?;
        if now
            .checked_duration_since(*captured_at)
            .is_none_or(|age| age > STAKING_REWARD_EVIDENCE_TTL)
            || evidence.reward_tokens != tokens
            || evidence.expected_amounts.len() != tokens.len()
            || evidence.expected_amounts.iter().all(U256::is_zero)
            || !self
                .bulk_reward_readiness
                .get(uuid)
                .is_some_and(|readiness| readiness.tokens == tokens && readiness.ready)
        {
            return None;
        }
        Some(evidence.clone())
    }

    pub(super) fn action_selection_ready(&self, selection: &StakingActionSelection) -> bool {
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
        match selection.kind {
            StakingActionKind::Stake => true,
            StakingActionKind::RewardClaim { token } => self.reward_action_ready(uuid, token),
            StakingActionKind::RewardClaimAll => {
                let mut tokens = governance_contracts(key.chain_id)
                    .map_or(&[][..], |contracts| contracts.reward_tokens)
                    .iter()
                    .map(|token| token.token)
                    .collect::<Vec<_>>();
                tokens.sort();
                self.reward_claim_all_ready(uuid, &tokens)
            }
            StakingActionKind::Delegate { .. }
            | StakingActionKind::Undelegate { .. }
            | StakingActionKind::Unlock { .. }
            | StakingActionKind::PrincipalClaim { .. } => self.account_action_ready(uuid),
        }
    }

    pub(super) fn finish(&mut self, key: &GovernanceContextKey, generation: u64) -> bool {
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

    pub(super) fn invalidate(&mut self) {
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

    fn chain_time(&self) -> Option<U256> {
        let (chain_time, captured_at) = self.chain_time_anchor.as_ref()?;
        chain_time.checked_add(U256::from(captured_at.elapsed().as_secs()))
    }
}

fn reward_interval_countdown(
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

fn remaining_reward_interval_seconds(
    countdown: RewardIntervalCountdown,
    chain_time: U256,
) -> Option<u64> {
    let remaining = countdown.boundary.checked_sub(chain_time)?;
    remaining.try_into().ok()
}

fn nearest_unlock_boundary(state: &StakingReadState) -> Option<U256> {
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

fn governance_boundary_crossed(state: &StakingReadState, chain_time: U256) -> bool {
    let interval_crossed = state
        .reward_interval_countdown
        .is_some_and(|countdown| chain_time >= countdown.boundary);
    interval_crossed
        || nearest_unlock_boundary(state).is_some_and(|boundary| chain_time >= boundary)
}

fn arm_governance_time_tick(owner: Option<u64>, generation: u64) -> (Option<u64>, bool) {
    if owner == Some(generation) {
        (owner, false)
    } else {
        (Some(generation), true)
    }
}

fn claim_governance_time_tick(owner: Option<u64>, generation: u64) -> (Option<u64>, bool) {
    if owner == Some(generation) {
        (None, true)
    } else {
        (owner, false)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct StakingAccountProjection {
    unclaimed_principal: U256,
    active_self_delegated: U256,
    active_external_delegated: U256,
}

impl StakingAccountProjection {
    const fn received_voting_power(self, voting_power: U256) -> U256 {
        voting_power.saturating_sub(self.active_self_delegated)
    }
}

fn staking_account_projection(stakes: &[StakePosition]) -> StakingAccountProjection {
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

pub(super) struct GovernanceState {
    pub tab: GovernanceTab,
    pub staking: StakingReadState,
    pub proposal_participation: ProposalParticipationState,
    pub proposal_action_amount_input: Entity<InputState>,
    pub action_flow: GovernanceActionFlowState,
    pub staking_delegate_input: Entity<InputState>,
    pub participant_picker_open: bool,
    pub compact_position_details: Option<(String, U256)>,
    pub participant_search_input: Entity<InputState>,
    staking_tables: BTreeMap<String, Entity<TableState<StakeTableDelegate>>>,
    participant_time_tick_generation: Option<u64>,
}

impl GovernanceState {
    pub(super) fn new(
        participant_search_input: Entity<InputState>,
        proposal_action_amount_input: Entity<InputState>,
        staking_delegate_input: Entity<InputState>,
    ) -> Self {
        Self {
            tab: GovernanceTab::default(),
            staking: StakingReadState::default(),
            proposal_participation: ProposalParticipationState::default(),
            proposal_action_amount_input,
            action_flow: GovernanceActionFlowState::default(),
            staking_delegate_input,
            participant_picker_open: false,
            compact_position_details: None,
            participant_search_input,
            staking_tables: BTreeMap::new(),
            participant_time_tick_generation: None,
        }
    }

    pub(super) fn invalidate_action(&mut self) {
        self.action_flow.invalidate();
    }

    fn clear_stale_position_details(&mut self) {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GovernanceActionSelection {
    Proposal(ProposalActionSelection),
    Staking(StakingActionSelection),
}

#[derive(Clone, Default)]
pub(super) struct GovernanceActionFlowState {
    pub selection: Option<GovernanceActionSelection>,
    pub recipe: Option<GovernanceDraftRecipe>,
    pub draft: Option<GovernanceSpendDraft>,
    pub pending: bool,
    pub error: Option<Arc<str>>,
    pub generation: u64,
}

impl GovernanceActionFlowState {
    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.recipe = None;
    }

    pub(super) fn set_proposal_selection(&mut self, selection: ProposalActionSelection) {
        self.selection = Some(GovernanceActionSelection::Proposal(selection));
    }

    pub(super) fn set_staking_selection(&mut self, selection: StakingActionSelection) {
        self.selection = Some(GovernanceActionSelection::Staking(selection));
    }

    pub(super) const fn staking_selection(&self) -> Option<&StakingActionSelection> {
        match self.selection.as_ref() {
            Some(GovernanceActionSelection::Staking(selection)) => Some(selection),
            _ => None,
        }
    }

    pub(super) fn proposal_recipe_matches(
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

    pub(super) fn staking_recipe_matches(
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StakingActionKind {
    Stake,
    Delegate { stake_id: U256 },
    Undelegate { stake_id: U256 },
    Unlock { stake_id: U256 },
    PrincipalClaim { stake_id: U256 },
    RewardClaim { token: Address },
    RewardClaimAll,
}

impl StakingActionKind {
    const fn stake_id(self) -> Option<U256> {
        match self {
            Self::Delegate { stake_id }
            | Self::Undelegate { stake_id }
            | Self::Unlock { stake_id }
            | Self::PrincipalClaim { stake_id } => Some(stake_id),
            _ => None,
        }
    }

    const fn is_compose_action(self) -> bool {
        matches!(self, Self::Stake | Self::Delegate { .. })
    }

    const fn is_reward_action(self) -> bool {
        matches!(self, Self::RewardClaim { .. } | Self::RewardClaimAll)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StakingActionSelection {
    pub actor_uuid: String,
    pub actor: Address,
    pub kind: StakingActionKind,
}

pub(super) const fn staking_action_title(kind: StakingActionKind) -> &'static str {
    match kind {
        StakingActionKind::Stake => "Stake RAIL",
        StakingActionKind::Delegate { .. } => "Delegate stake",
        StakingActionKind::Undelegate { .. } => "Undelegate stake",
        StakingActionKind::Unlock { .. } => "Unlock stake",
        StakingActionKind::PrincipalClaim { .. } => "Claim principal",
        StakingActionKind::RewardClaim { .. } => "Claim rewards",
        StakingActionKind::RewardClaimAll => "Claim all rewards",
    }
}

fn staking_action_dialog_title(
    kind: StakingActionKind,
    chain_id: u64,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
) -> String {
    match kind {
        StakingActionKind::RewardClaim { token } => {
            let symbol = token_display_metadata(Some(registry), chain_id, &token)
                .map(|metadata| metadata.symbol)
                .or_else(|| {
                    governance_contracts(chain_id).and_then(|contracts| {
                        contracts
                            .reward_tokens
                            .iter()
                            .find(|entry| entry.token == token)
                            .map(|entry| entry.symbol.to_owned())
                    })
                });
            symbol.map_or_else(
                || staking_action_title(kind).to_owned(),
                |symbol| format!("Claim {symbol} rewards"),
            )
        }
        _ => staking_action_title(kind).to_owned(),
    }
}

impl WalletRoot {
    pub(super) fn clean_governance_participants(&mut self) {
        let Some(wallet_id) = self.selected_wallet_id.as_deref() else {
            return;
        };
        if self.public_accounts.is_empty() {
            return;
        }
        let persisted = self
            .ui_state
            .governance_participants
            .get(wallet_id)
            .cloned()
            .unwrap_or_default();
        let resolution = normalize_participant_ids(&persisted, &self.public_accounts, wallet_id);
        if resolution.changed {
            self.ui_state
                .governance_participants
                .insert(wallet_id.to_owned(), resolution.uuids);
            self.save_ui_state();
        }
    }

    pub(super) fn governance_participants(&self) -> Vec<PublicAccountMetadata> {
        let Some(wallet_id) = self.selected_wallet_id.as_deref() else {
            return Vec::new();
        };
        let persisted = self
            .ui_state
            .governance_participants
            .get(wallet_id)
            .map_or(&[][..], Vec::as_slice);
        let resolution = normalize_participant_ids(persisted, &self.public_accounts, wallet_id);
        resolution
            .uuids
            .iter()
            .filter_map(|uuid| {
                self.public_accounts
                    .iter()
                    .find(|account| {
                        account.public_account_uuid == *uuid
                            && account.is_scoped_to_wallet(wallet_id)
                    })
                    .cloned()
            })
            .collect()
    }

    pub(super) fn governance_context_key(&self) -> GovernanceContextKey {
        let participants = self
            .governance_participants()
            .into_iter()
            .map(|account| GovernanceParticipantIdentity {
                uuid: account.public_account_uuid,
                address: account.address,
            })
            .collect();
        GovernanceContextKey {
            wallet_id: self.selected_wallet_id.as_ref().map(ToString::to_string),
            chain_id: self.selected_chain,
            participants,
        }
    }

    pub(super) fn invalidate_governance_context(&mut self) {
        self.governance.staking.invalidate();
        self.governance.proposal_participation.invalidate();
        self.governance.staking_tables.clear();
        self.governance.compact_position_details = None;
        self.governance.invalidate_action();
        self.governance.action_flow.selection = None;
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        self.governance.participant_time_tick_generation = None;
    }

    pub(super) fn open_proposal_action(
        &mut self,
        actor: Address,
        kind: ProposalActionKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.governance.invalidate_action();
        self.governance
            .action_flow
            .set_proposal_selection(ProposalActionSelection { actor, kind });
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        self.governance
            .proposal_action_amount_input
            .update(cx, |input, cx| {
                input.set_value("", window, cx);
                cx.notify();
            });
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(440.0));
        let dialog_max_height = super::dialog_max_height(window);
        let content_max_height = super::dialog_content_max_height(window);
        let content_width = super::secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(super::proposals::proposal_action_title(kind))
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.close_proposal_action(cx);
                    });
                })
                .child(super::scrollable_dialog_content(
                    content_max_height,
                    content_root.read(cx).render_proposal_action_dialog_content(
                        content_root.clone(),
                        content_width,
                        cx,
                    ),
                ))
        });
        if matches!(kind, ProposalActionKind::CallVote)
            && let Some(proposal) = self.proposals.selected_proposal().cloned()
        {
            self.review_proposal_action(
                &proposal,
                ProposalActionSelection { actor, kind },
                None,
                window,
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn close_proposal_action(&mut self, cx: &mut Context<'_, Self>) {
        self.governance.invalidate_action();
        self.governance.action_flow.selection = None;
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        cx.notify();
    }

    pub(super) fn open_staking_action(
        &mut self,
        actor_uuid: &str,
        actor: Address,
        kind: StakingActionKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.governance.invalidate_action();
        let selection = StakingActionSelection {
            actor_uuid: actor_uuid.to_owned(),
            actor,
            kind,
        };
        self.governance.action_flow.set_staking_selection(selection);
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        self.governance
            .proposal_action_amount_input
            .update(cx, |input, cx| {
                input.set_value("", window, cx);
                cx.notify();
            });
        self.governance
            .staking_delegate_input
            .update(cx, |input, cx| {
                input.set_value("", window, cx);
                cx.notify();
            });
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(440.0));
        let dialog_max_height = super::dialog_max_height(window);
        let content_max_height = super::dialog_content_max_height(window);
        let content_width = super::secondary_dialog_content_width(dialog_width);
        let dialog_title =
            staking_action_dialog_title(kind, self.selected_chain, &self.effective_token_registry);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(dialog_title.clone()))
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.close_staking_action(cx);
                    });
                })
                .child(super::scrollable_dialog_content(
                    content_max_height,
                    content_root.read(cx).render_staking_action_dialog_content(
                        content_root.clone(),
                        content_width,
                        cx,
                    ),
                ))
        });
        let focus_input = match kind {
            StakingActionKind::Stake => Some(self.governance.proposal_action_amount_input.clone()),
            StakingActionKind::Delegate { .. } => {
                Some(self.governance.staking_delegate_input.clone())
            }
            _ => None,
        };
        if let Some(focus_input) = focus_input {
            cx.defer_in(window, move |_root, window, cx| {
                focus_input.read(cx).focus_handle(cx).focus(window);
            });
        }
        if !matches!(
            kind,
            StakingActionKind::Delegate { .. } | StakingActionKind::Stake
        ) {
            self.review_staking_action(window, cx);
        }
        cx.notify();
    }

    pub(super) fn render_staking_action_dialog_content(
        &self,
        root: Entity<Self>,
        content_width: gpui::Pixels,
        cx: &App,
    ) -> gpui::Div {
        let stale_content = |close_root: Entity<Self>| {
            div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .child(Alert::error(
                    "governance-staking-action-stale-selection",
                    "The selected staking action is no longer available. Refresh before trying again.",
                ))
                .child(
                    div().flex().justify_end().child(
                        app_button_base("governance-staking-action-stale-cancel")
                            .ghost()
                            .small()
                            .child("Cancel")
                            .on_click(move |_event, window, cx| {
                                close_root.update(cx, Self::close_staking_action);
                                window.close_dialog(cx);
                            }),
                    ),
                )
        };
        let Some(selection) = self.governance.action_flow.staking_selection() else {
            return stale_content(root);
        };
        let context_key = self.governance_context_key();
        if self.governance.staking.key.as_ref() != Some(&context_key)
            || !self.governance.staking.action_selection_ready(selection)
        {
            return stale_content(root);
        }
        render_staking_action_form(&root, self, content_width, cx)
            .unwrap_or_else(|| stale_content(root))
    }

    pub(super) fn review_staking_action(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if self.governance.action_flow.pending {
            return;
        }
        let Some(selection) = self.governance.action_flow.staking_selection().cloned() else {
            return;
        };
        let context_key = self.governance_context_key();
        if self.governance.staking.key.as_ref() != Some(&context_key)
            || !self.governance.staking.action_selection_ready(&selection)
        {
            self.governance.action_flow.error = Some(Arc::from(
                "Refresh staking data before reviewing this action",
            ));
            cx.notify();
            return;
        }
        let Some(wallet_id) = self.selected_wallet_id.clone() else {
            self.governance.action_flow.error =
                Some(Arc::from("Select a wallet before reviewing this action"));
            cx.notify();
            return;
        };
        let Some(account) = self.governance_participants().into_iter().find(|account| {
            account.public_account_uuid == selection.actor_uuid
                && account.address == selection.actor
        }) else {
            self.governance.action_flow.error =
                Some(Arc::from("Selected Public account is no longer enrolled"));
            cx.notify();
            return;
        };
        if account.status != PublicAccountStatus::Active {
            self.governance.action_flow.error =
                Some(Arc::from("Inactive participant accounts are read-only"));
            cx.notify();
            return;
        }
        let Some(view_session) = self.view_session.clone() else {
            self.governance.action_flow.error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            self.governance.action_flow.error =
                Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let chain_id = self.selected_chain;
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let http = self.http.clone();
        let actor_source = account.source;
        let amount_input = self
            .governance
            .proposal_action_amount_input
            .read(cx)
            .value()
            .to_string();
        let delegate_input = self
            .governance
            .staking_delegate_input
            .read(cx)
            .value()
            .to_string();
        let token_decimals = governance_contracts(chain_id)
            .and_then(|contracts| {
                self.effective_token_registry
                    .get(chain_id, &contracts.governance_token)
            })
            .map(|token| token.decimals);
        let reward_evidence_mode = match selection.kind {
            StakingActionKind::RewardClaim { token } => self
                .governance
                .staking
                .cached_reward_evidence_at(
                    &context_key,
                    &selection.actor_uuid,
                    selection.actor,
                    &[token],
                    Instant::now(),
                )
                .map_or(RewardEvidenceMode::Fresh, |evidence| {
                    RewardEvidenceMode::CachedInitial { evidence }
                }),
            StakingActionKind::RewardClaimAll => {
                let mut tokens = governance_contracts(chain_id)
                    .map_or(&[][..], |contracts| contracts.reward_tokens)
                    .iter()
                    .map(|token| token.token)
                    .collect::<Vec<_>>();
                tokens.sort();
                self.governance
                    .staking
                    .cached_reward_evidence_at(
                        &context_key,
                        &selection.actor_uuid,
                        selection.actor,
                        &tokens,
                        Instant::now(),
                    )
                    .map_or(RewardEvidenceMode::Fresh, |evidence| {
                        RewardEvidenceMode::CachedInitial { evidence }
                    })
            }
            _ => RewardEvidenceMode::Fresh,
        };
        let generation = self.governance.action_flow.generation.wrapping_add(1);
        self.governance.action_flow.generation = generation;
        self.governance.action_flow.recipe = Some(GovernanceDraftRecipe::Staking {
            selection: selection.clone(),
            context_key: context_key.clone(),
            amount_input: amount_input.clone(),
            delegate_input: delegate_input.clone(),
            token_decimals,
        });
        self.governance.action_flow.pending = true;
        self.governance.action_flow.error = None;
        self.governance.action_flow.draft = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = build_staking_action_draft(
                selection.clone(),
                context_key.clone(),
                wallet_id.clone(),
                actor_source,
                amount_input.clone(),
                delegate_input.clone(),
                view_session,
                vault_store,
                token_decimals,
                effective_chain.clone(),
                http.clone(),
                reward_evidence_mode,
                PublicActionGasFeeSelection::Auto,
            )
            .await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.governance.action_flow.generation != generation
                    || !root.governance.action_flow.staking_recipe_matches(
                        &selection,
                        &context_key,
                        &amount_input,
                        &delegate_input,
                        token_decimals,
                    )
                    || root.governance_context_key() != context_key.clone()
                {
                    return;
                }
                root.governance.action_flow.pending = false;
                match result {
                    Ok(draft) => root.authorize_prepared_governance_draft(draft, window, cx),
                    Err(error) => root.governance.action_flow.error = Some(Arc::from(error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn close_staking_action(&mut self, cx: &mut Context<'_, Self>) {
        self.governance.invalidate_action();
        self.governance.compact_position_details = None;
        self.governance.action_flow.selection = None;
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        cx.notify();
    }

    fn set_compact_position_details(
        &mut self,
        key: Option<(String, U256)>,
        cx: &mut Context<'_, Self>,
    ) {
        self.governance.compact_position_details = key;
        let tables = self
            .governance
            .staking_tables
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for table in tables {
            table.update(cx, TableState::refresh);
        }
        cx.notify();
    }

    pub(super) fn start_governance_continuation(
        &mut self,
        continuation: GovernanceContinuation,
        target: GovernanceRefreshTarget,
        actor_source: wallet_ops::vault::PublicAccountSource,
        context: GovernanceActionContext,
        view_session: Arc<wallet_ops::vault::DesktopViewSession>,
        vault_store: Arc<wallet_ops::vault::DesktopVaultStore>,
        recipe: GovernanceDraftRecipe,
        confirmed_hash: Option<B256>,
        #[cfg(feature = "hardware")] window: &mut Window,
        #[cfg(not(feature = "hardware"))] window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let GovernanceContinuation::Reward {
            mut progress,
            evidence,
        } = continuation;
        let GovernanceRefreshTarget::Staking(context_key) = target.clone() else {
            self.refresh_governance_after_target(&target, cx);
            return;
        };
        let GovernanceDraftRecipe::Staking { selection, .. } = &recipe else {
            return;
        };
        let actor_uuid: Arc<str> = Arc::from(selection.actor_uuid.clone());
        let effective_chain = self.effective_chain_configs.get(&context.chain_id).cloned();
        let http = self.http.clone();
        let generation = self.governance.action_flow.generation.wrapping_add(1);
        self.governance.action_flow.generation = generation;
        self.governance.action_flow.recipe = Some(recipe.clone());
        self.governance.action_flow.pending = true;
        self.governance.action_flow.draft = None;
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<Option<GovernanceSpendDraft>, String> = async {
                validate_governance_deployment(context.chain_id, effective_chain.as_ref(), &http)
                    .await
                    .map_err(|error| error.to_string())?;
                let contracts = governance_contracts(context.chain_id)
                    .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
                let confirmed_hash = confirmed_hash
                    .ok_or_else(|| "Reward transaction hash was unavailable".to_owned())?;
                let step = wallet_ops::next_reward_claim_batch_step(&progress, &evidence)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "Reward claim sequence is complete".to_owned())?;
                wallet_ops::confirm_reward_claim_batch_step(
                    &mut progress,
                    &step,
                    &evidence,
                    confirmed_hash,
                )
                .map_err(|error| error.to_string())?;
                let metadata = fetch_interval_metadata(
                    context.chain_id,
                    &progress.plan.reward_tokens,
                    effective_chain.as_ref(),
                    &http,
                )
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Reward metadata unavailable".to_owned())?;
                let snapshots = fetch_account_snapshots(
                    context.chain_id,
                    context.actor,
                    effective_chain.as_ref(),
                    &http,
                    wallet_ops::MulticallChunkSize::default(),
                )
                .await
                .map_err(|error| error.to_string())?;
                let fresh = fetch_reward_batch_evidence(
                    context.chain_id,
                    context.actor,
                    &progress.plan.reward_tokens,
                    &metadata,
                    &snapshots,
                    effective_chain.as_ref(),
                    &http,
                    wallet_ops::MulticallChunkSize::default(),
                )
                .await
                .map_err(|error| error.to_string())?;
                let Some(fresh) = fresh else {
                    return Ok(None);
                };
                let fresh_steps = plan_reward_steps_for_evidence(
                    context.chain_id,
                    context.actor,
                    &fresh,
                    effective_chain.as_ref(),
                    &http,
                )
                .await?;
                let rebuilt = wallet_ops::rebuild_reward_claim_batch_continuation(
                    &progress,
                    &fresh,
                    &fresh_steps,
                )
                .map_err(|error| error.to_string())?;
                let next = wallet_ops::next_reward_claim_batch_step(&rebuilt, &fresh)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "Reward claim sequence is complete".to_owned())?;
                build_typed_governance_spend_draft(
                    target.clone(),
                    Arc::from(context.private_wallet_uuid.clone()),
                    actor_uuid,
                    context.actor,
                    actor_source,
                    contracts.governor_rewards,
                    GovernanceContractKind::GovernorRewards,
                    rebuilt.plan.fingerprint,
                    next.intent,
                    view_session,
                    vault_store,
                    effective_chain,
                    http,
                    PublicActionGasFeeSelection::Auto,
                    None,
                    Some(GovernanceContinuation::Reward {
                        progress: rebuilt,
                        evidence: fresh,
                    }),
                    None,
                    recipe,
                )
                .await
                .map(Some)
            }
            .await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.selected_chain != context.chain_id
                    || root.selected_wallet_id.as_deref()
                        != Some(context.private_wallet_uuid.as_str())
                    || root.governance_context_key() != context_key
                    || root.governance.action_flow.generation != generation
                {
                    return;
                }
                root.governance.action_flow.pending = false;
                match result {
                    Ok(Some(draft)) => {
                        if draft.continuation.as_ref().is_some_and(|continuation| {
                            matches!(
                                continuation,
                                GovernanceContinuation::Reward { progress, .. }
                                    if !progress.confirmed().is_empty()
                            )
                        }) {
                            root.replace_public_action_dialog_for_confirmed_history(true);
                        }
                        root.authorize_prepared_governance_draft(draft, window, cx);
                    }
                    Ok(None) => {
                        root.governance.action_flow.error = None;
                        root.refresh_governance_after_target(&target, cx);
                    }
                    Err(error) => {
                        root.governance.action_flow.error = Some(Arc::from(error));
                        root.refresh_governance_after_target(&target, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn start_proposal_participation(
        &mut self,
        proposal: &wallet_ops::GovernanceProposal,
        cx: &Context<'_, Self>,
    ) {
        if self.governance.tab != GovernanceTab::Proposals {
            return;
        }
        self.clean_governance_participants();
        let key = super::governance_action::proposal_participation_key(
            proposal,
            self.governance_context_key(),
        );
        let proposal_action_key_changed =
            self.governance
                .action_flow
                .recipe
                .as_ref()
                .is_some_and(|recipe| match recipe {
                    GovernanceDraftRecipe::Proposal {
                        key: action_key, ..
                    } => action_key != &key,
                    GovernanceDraftRecipe::Staking { .. } => true,
                })
                || self
                    .governance
                    .action_flow
                    .draft
                    .as_ref()
                    .is_some_and(|draft| {
                        !matches!(
                            &draft.target,
                            super::governance_action::GovernanceRefreshTarget::Proposal(action_key)
                                if action_key == &key
                        )
                    });
        if proposal_action_key_changed {
            self.governance.invalidate_action();
            self.governance.action_flow.selection = None;
            self.governance.action_flow.draft = None;
            self.governance.action_flow.pending = false;
            self.governance.action_flow.error = None;
        }
        if matches!(
            self.governance.action_flow.selection,
            Some(GovernanceActionSelection::Proposal(_))
        ) {
            self.governance.action_flow.selection = None;
        }
        let generation = self.governance.proposal_participation.begin(&key);
        let accounts = key
            .context
            .participants
            .iter()
            .map(|participant| participant.address)
            .collect::<Vec<_>>();
        let effective_chain = self
            .effective_chain_configs
            .get(&key.context.chain_id)
            .cloned();
        let http = self.http.clone();
        let proposal = proposal.clone();
        cx.spawn(async move |this, cx| {
            let result = wallet_ops::fetch_governance_participation(
                key.context.chain_id,
                &proposal,
                &accounts,
                effective_chain.as_ref(),
                &http,
            )
            .await
            .map_err(|error| error.to_string());
            let _ = this.update(cx, |root, cx| {
                root.governance.proposal_participation.apply(
                    &key,
                    generation,
                    result.unwrap_or_else(|error| {
                        accounts
                            .iter()
                            .map(|&account| wallet_ops::GovernanceParticipationRow {
                                account,
                                state: Err(wallet_ops::GovernanceParticipationError::Read {
                                    field: "participation",
                                    reason: error.clone(),
                                }),
                            })
                            .collect()
                    }),
                );
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn select_governance_tab(&mut self, tab: GovernanceTab, cx: &mut Context<'_, Self>) {
        if self.governance.tab == tab {
            return;
        }
        self.governance.tab = tab;
        self.invalidate_governance_context();
        match tab {
            GovernanceTab::Proposals if !self.proposals.checked && !self.proposals.loading => {
                self.start_proposals_refresh(false, cx);
            }
            GovernanceTab::Proposals => {
                let selected = self.proposals.selected_proposal().cloned();
                if let Some(proposal) = selected.as_ref() {
                    self.start_proposal_participation(proposal, cx);
                }
            }
            GovernanceTab::Staking => self.start_staking_refresh(cx),
        }
        cx.notify();
    }

    pub(super) fn toggle_governance_participant(&mut self, uuid: &str, cx: &mut Context<'_, Self>) {
        let Some(wallet_id) = self.selected_wallet_id.as_deref() else {
            return;
        };
        let ids = self
            .ui_state
            .governance_participants
            .entry(wallet_id.to_owned())
            .or_default();
        if let Some(index) = ids.iter().position(|candidate| candidate == uuid) {
            ids.remove(index);
        } else if self.public_accounts.iter().any(|account| {
            account.public_account_uuid == uuid && account.is_scoped_to_wallet(wallet_id)
        }) {
            ids.push(uuid.to_owned());
        }
        self.save_ui_state();
        self.invalidate_governance_context();
        if self.governance.tab == GovernanceTab::Staking {
            self.start_staking_refresh(cx);
        } else {
            let selected = self.proposals.selected_proposal().cloned();
            if let Some(proposal) = selected.as_ref() {
                self.start_proposal_participation(proposal, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn start_staking_refresh(&mut self, cx: &mut Context<'_, Self>) {
        if self.governance.tab != GovernanceTab::Staking {
            return;
        }
        self.clean_governance_participants();
        let key = self.governance_context_key();
        let generation = self.governance.staking.begin(key.clone());
        let chain_id = key.chain_id;
        let participant_ids = key.participants.clone();
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let http = self.http.clone();
        let mut tokens = governance_contracts(chain_id)
            .map(|contracts| {
                contracts
                    .reward_tokens
                    .iter()
                    .map(|token| token.token)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        tokens.sort();
        cx.spawn(async move |this, cx| {
            let global = fetch_staking_global_metrics(chain_id, effective_chain.as_ref(), &http)
                .await
                .map_err(|error| Arc::from(error.to_string()))
                .and_then(|metrics| {
                    metrics.ok_or_else(|| Arc::from("Staking is not deployed on this chain"))
                });
            let global_metrics = global.as_ref().ok().cloned();
            let Ok(accepted) = this.update(cx, |root, cx| {
                let accepted = root
                    .governance
                    .staking
                    .apply_global(&key, generation, global);
                cx.notify();
                accepted
            }) else {
                return;
            };
            if !accepted {
                return;
            }
            let Some(metrics) = global_metrics else {
                return;
            };
            let metadata =
                fetch_interval_metadata(chain_id, &tokens, effective_chain.as_ref(), &http)
                    .await
                    .ok()
                    .flatten();
            let countdown = metadata
                .as_ref()
                .and_then(|metadata| reward_interval_countdown(metadata, metrics.chain_time));
            let Ok(accepted) = this.update(cx, |root, cx| {
                let accepted = root
                    .governance
                    .staking
                    .apply_reward_interval(&key, generation, countdown);
                cx.notify();
                accepted
            }) else {
                return;
            };
            if !accepted {
                return;
            }
            if participant_ids.is_empty() {
                let _ = this.update(cx, |root, cx| {
                    let finished = root.governance.staking.finish(&key, generation);
                    if finished {
                        root.start_governance_time_tick(generation, cx);
                    }
                    cx.notify();
                });
                return;
            }
            let addresses = participant_ids
                .iter()
                .map(|participant| participant.address)
                .collect::<Vec<_>>();
            let account_results = fetch_account_stakes(
                chain_id,
                &addresses,
                metrics.chain_time,
                effective_chain.as_ref(),
                &http,
                wallet_ops::MulticallChunkSize::default(),
            )
            .await;
            match account_results {
                Ok(results) => {
                    for (participant, account) in participant_ids.iter().zip(results) {
                        let _ = this.update(cx, |root, cx| {
                            root.governance.staking.apply_account(
                                &key,
                                generation,
                                participant.uuid.clone(),
                                Ok(account),
                            );
                            cx.notify();
                        });
                    }
                }
                Err(error) => {
                    let message = Arc::from(error.to_string());
                    for participant in &participant_ids {
                        let error = Arc::clone(&message);
                        let _ = this.update(cx, |root, cx| {
                            root.governance.staking.apply_account(
                                &key,
                                generation,
                                participant.uuid.clone(),
                                Err(error),
                            );
                            cx.notify();
                        });
                    }
                }
            }
            let addresses = participant_ids
                .iter()
                .map(|participant| participant.address)
                .collect::<Vec<_>>();
            let snapshot_results = fetch_account_snapshots_multi(
                chain_id,
                &addresses,
                effective_chain.as_ref(),
                &http,
                wallet_ops::MulticallChunkSize::default(),
            )
            .await;
            let snapshot_results = snapshot_results.unwrap_or_else(|error| {
                addresses
                    .iter()
                    .map(|&account| wallet_ops::AccountSnapshotsResult {
                        account,
                        snapshots: Err(error.to_string()),
                    })
                    .collect()
            });
            let reward_jobs = participant_ids
                .iter()
                .cloned()
                .zip(snapshot_results)
                .flat_map(|(participant, snapshot_result)| {
                    let snapshots = snapshot_result.snapshots;
                    let per_token_participant = participant.clone();
                    let per_token_metadata = metadata.clone();
                    let per_token_tokens = tokens.clone();
                    let per_token_effective_chain = effective_chain.clone();
                    let per_token_http = http.clone();
                    let per_token_snapshots = snapshots.clone();
                    let per_token_job: BoxFuture<'static, StakingRewardRefreshResult> =
                        Box::pin(async move {
                            let rewards = match (per_token_metadata.as_ref(), per_token_snapshots) {
                                (Some(metadata), Ok(snapshots)) => fetch_reward_evidence_multi(
                                    chain_id,
                                    per_token_participant.address,
                                    &per_token_tokens,
                                    metadata,
                                    &snapshots,
                                    per_token_effective_chain.as_ref(),
                                    &per_token_http,
                                    wallet_ops::MulticallChunkSize::default(),
                                )
                                .await
                                .map_err(|error| Arc::from(error.to_string())),
                                (None, _) => Err(Arc::from("Reward interval metadata unavailable")),
                                (_, Err(error)) => Err(Arc::from(error)),
                            };
                            StakingRewardRefreshResult::PerToken {
                                participant: per_token_participant,
                                tokens: per_token_tokens,
                                rewards,
                            }
                        });

                    let bulk_participant = participant;
                    let bulk_metadata = metadata.clone();
                    let bulk_tokens = tokens.clone();
                    let bulk_effective_chain = effective_chain.clone();
                    let bulk_http = http.clone();
                    let bulk_job: BoxFuture<'static, StakingRewardRefreshResult> =
                        Box::pin(async move {
                            let evidence = match (bulk_metadata.as_ref(), snapshots) {
                                (Some(metadata), Ok(snapshots)) => fetch_reward_batch_evidence(
                                    chain_id,
                                    bulk_participant.address,
                                    &bulk_tokens,
                                    metadata,
                                    &snapshots,
                                    bulk_effective_chain.as_ref(),
                                    &bulk_http,
                                    wallet_ops::MulticallChunkSize::default(),
                                )
                                .await
                                .map_err(|error| Arc::from(error.to_string())),
                                (None, _) => Err(Arc::from("Reward interval metadata unavailable")),
                                (_, Err(error)) => Err(Arc::from(error)),
                            };
                            StakingRewardRefreshResult::Bulk {
                                participant: bulk_participant,
                                tokens: bulk_tokens,
                                evidence,
                            }
                        });
                    [per_token_job, bulk_job]
                })
                .collect::<Vec<_>>();
            let mut reward_results = futures_util::stream::iter(reward_jobs)
                .buffer_unordered(STAKING_REWARD_EVIDENCE_CONCURRENCY.saturating_mul(2));
            while let Some(result) = reward_results.next().await {
                match result {
                    StakingRewardRefreshResult::PerToken {
                        participant,
                        tokens,
                        rewards,
                    } => {
                        for (token_index, &token) in tokens.iter().enumerate() {
                            let reward = match &rewards {
                                Ok(results) => results.get(token_index).map_or_else(
                                    || Err(Arc::from("Reward result is missing")),
                                    |result| result.evidence.clone().map_err(Arc::from),
                                ),
                                Err(error) => Err(Arc::clone(error)),
                            };
                            let _ = this.update(cx, |root, cx| {
                                root.governance.staking.apply_reward(
                                    &key,
                                    generation,
                                    participant.uuid.clone(),
                                    token,
                                    reward,
                                );
                                cx.notify();
                            });
                        }
                    }
                    StakingRewardRefreshResult::Bulk {
                        participant,
                        tokens,
                        evidence,
                    } => {
                        let _ = this.update(cx, |root, cx| {
                            root.governance.staking.apply_bulk_reward(
                                &key,
                                generation,
                                participant.uuid,
                                tokens,
                                &evidence,
                            );
                            cx.notify();
                        });
                    }
                }
            }
            let _ = this.update(cx, |root, cx| {
                let finished = root.governance.staking.finish(&key, generation);
                if finished {
                    root.start_governance_time_tick(generation, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_governance_time_tick(&mut self, generation: u64, cx: &Context<'_, Self>) {
        if generation != self.governance.staking.generation
            || self.active_activity != super::sidebar::Activity::Proposals
            || self.governance.tab != GovernanceTab::Staking
        {
            return;
        }
        let (owner, armed) =
            arm_governance_time_tick(self.governance.participant_time_tick_generation, generation);
        self.governance.participant_time_tick_generation = owner;
        if !armed {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(GOVERNANCE_TIME_TICK).await;
            let _ = this.update(cx, |root, cx| {
                let (owner, claimed) = claim_governance_time_tick(
                    root.governance.participant_time_tick_generation,
                    generation,
                );
                root.governance.participant_time_tick_generation = owner;
                if !claimed {
                    return;
                }
                if generation != root.governance.staking.generation
                    || root.governance.tab != GovernanceTab::Staking
                    || root.active_activity != super::sidebar::Activity::Proposals
                {
                    return;
                }
                let crossed =
                    root.governance.staking.chain_time().is_some_and(|now| {
                        governance_boundary_crossed(&root.governance.staking, now)
                    });
                if crossed {
                    root.start_staking_refresh(cx);
                } else {
                    root.start_governance_time_tick(generation, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_governance_workspace(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let compact_header = window.viewport_size().width < GOVERNANCE_COMPACT_HEADER_BREAKPOINT;
        let participant_trigger_width = if compact_header {
            GOVERNANCE_COMPACT_PARTICIPANT_TRIGGER_WIDTH
        } else {
            GOVERNANCE_PARTICIPANT_TRIGGER_WIDTH
        };
        let refresh_root = root.clone();
        let refreshing = match self.governance.tab {
            GovernanceTab::Proposals => self.proposals.refreshing,
            GovernanceTab::Staking => matches!(
                self.governance.staking.status,
                StakingRefreshStatus::Loading
            ),
        };
        let selected_index = usize::from(self.governance.tab == GovernanceTab::Staking);
        let tab_root = root.clone();
        let participant_root = root.clone();
        let participant_query = self.governance.participant_search_input.read(cx).value();
        let wallet_id = self.selected_wallet_id.as_deref();
        let participants = self.governance_participants();
        let selected_ids = participants
            .iter()
            .map(|account| account.public_account_uuid.clone())
            .collect::<BTreeSet<_>>();
        let choices = participant_choices(
            &self.public_accounts,
            &selected_ids,
            wallet_id,
            &participant_query,
        );
        let summary = participant_summary(&self.public_accounts, &selected_ids, wallet_id);
        let trigger_label = if summary.selected == 0 {
            app_muted_text("Enroll accounts...")
                .min_w(px(0.0))
                .truncate()
                .into_any_element()
        } else {
            let account_word = if summary.selected == 1 {
                "account"
            } else {
                "accounts"
            };
            app_text(format!("{} {account_word} enrolled", summary.selected))
                .min_w(px(0.0))
                .truncate()
                .into_any_element()
        };
        let trigger_content = div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .flex_nowrap()
            .overflow_hidden()
            .child(trigger_label);
        let trigger = div()
            .w(participant_trigger_width)
            .h(px(24.0))
            .min_w(px(0.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .justify_between()
            .gap_1()
            .child(
                Icon::empty()
                    .path(USERS_ICON_PATH)
                    .size_4()
                    .flex_none()
                    .text_color(rgb(theme::TEXT_MUTED)),
            )
            .child(trigger_content)
            .child(
                Icon::new(IconName::ChevronDown)
                    .xsmall()
                    .flex_none()
                    .text_color(rgb(theme::TEXT_PLACEHOLDER)),
            );
        let picker_choices = choices;
        let picker_root = participant_root.clone();
        let participant_search_input = self.governance.participant_search_input.clone();
        let participant_search_focus = participant_search_input.read(cx).focus_handle(cx);
        let picker = Popover::new("governance-participant-picker")
            .open(self.governance.participant_picker_open)
            .track_focus(&participant_search_focus)
            .on_open_change({
                let open_root = participant_root;
                let search_input = participant_search_input.clone();
                move |open, window, cx| {
                    if !*open {
                        search_input.update(cx, |input, cx| {
                            input.set_value("", window, cx);
                        });
                    }
                    open_root.update(cx, |root, cx| {
                        root.governance.participant_picker_open = *open;
                        cx.notify();
                    });
                }
            })
            .w(GOVERNANCE_PARTICIPANT_POPUP_WIDTH)
            .max_w_full()
            .p_1()
            .trigger(
                app_button_base("governance-participants")
                    .text()
                    .small()
                    .px_0()
                    .justify_start()
                    .w(participant_trigger_width)
                    .max_w_full()
                    .child(trigger),
            )
            .content(move |_state, _window, _cx| {
                let mut list = div()
                    .id("governance-participant-picker-list")
                    .max_h(px(300.0))
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(picker_choices.iter().map(|choice| {
                        let uuid = choice.uuid.clone();
                        let toggle_root = picker_root.clone();
                        let scope_suffix = if choice.global { " · Global" } else { "" };
                        div()
                            .id(SharedString::from(format!(
                                "governance-participant-{}",
                                choice.uuid
                            )))
                            .flex()
                            .items_center()
                            .w_full()
                            .min_w(px(0.0))
                            .p_1()
                            .rounded_sm()
                            .when(choice.selected, |this| {
                                this.bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                            })
                            .hover(|this| this.bg(rgb(theme::SURFACE_HOVER_SUBTLE)))
                            .child(
                                div()
                                    .flex()
                                    .w_full()
                                    .items_center()
                                    .justify_between()
                                    .gap_1()
                                    .child(
                                        div().flex().w_full().min_w(px(0.0)).items_center().child(
                                            div()
                                                .w_full()
                                                .min_w(px(0.0))
                                                .overflow_hidden()
                                                .flex()
                                                .flex_col()
                                                .child(
                                                    app_text(choice.label.clone())
                                                        .whitespace_nowrap(),
                                                )
                                                .child(
                                                    app_muted_text(format!(
                                                        "{}{}{}",
                                                        short_address(&choice.address),
                                                        scope_suffix,
                                                        if choice.inactive {
                                                            " · Inactive"
                                                        } else {
                                                            ""
                                                        }
                                                    ))
                                                    .text_size(px(11.0))
                                                    .whitespace_nowrap(),
                                                ),
                                        ),
                                    )
                                    .child(
                                        Icon::new(IconName::Check)
                                            .xsmall()
                                            .flex_none()
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .when(!choice.selected, gpui::Styled::invisible),
                                    ),
                            )
                            .on_click(move |_event, _window, cx| {
                                toggle_root.update(cx, |root, cx| {
                                    root.toggle_governance_participant(&uuid, cx);
                                });
                            })
                    }));
                if picker_choices.is_empty() {
                    list = list.child(app_muted_text("No visible accounts match this search."));
                }
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .px(px(6.0))
                            .border_b_1()
                            .border_color(rgb(theme::BORDER))
                            .child(
                                Input::new(&participant_search_input)
                                    .small()
                                    .prefix(
                                        Icon::new(IconName::Search)
                                            .text_color(rgb(theme::TEXT_MUTED)),
                                    )
                                    .cleanable(true)
                                    .p_0()
                                    .appearance(false),
                            ),
                    )
                    .child(list)
            });
        let governance_tab = |label: &'static str, icon: Icon| {
            Tab::new().min_w(px(92.0)).child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(icon.size_4())
                    .child(label),
            )
        };
        let tabs = TabBar::new("governance-tabs")
            .underline()
            .w_full()
            .flex_none()
            .px(px(14.0))
            .selected_index(selected_index)
            .on_click(move |index, _window, cx| {
                let tab = if *index == 1 {
                    GovernanceTab::Staking
                } else {
                    GovernanceTab::Proposals
                };
                tab_root.update(cx, |root, cx| root.select_governance_tab(tab, cx));
            })
            .children([
                governance_tab("Proposals", Icon::new(IconName::File)),
                governance_tab(
                    "Staking",
                    Icon::empty().path(PIGGY_BANK_ICON_PATH).mr(px(2.0)),
                ),
            ]);
        let body = match self.governance.tab {
            GovernanceTab::Proposals => self
                .render_proposals_view(root, window, cx)
                .into_any_element(),
            GovernanceTab::Staking => self
                .render_staking_body(root, window, cx)
                .into_any_element(),
        };
        let body_container = div().flex_1().min_w(px(0.0)).min_h(px(0.0)).child(body);
        let body_container = if self.governance.tab == GovernanceTab::Staking {
            body_container.overflow_y_scrollbar().into_any_element()
        } else {
            body_container.into_any_element()
        };
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .child(
                div()
                    .h(GOVERNANCE_HEADER_HEIGHT)
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(if compact_header { px(8.0) } else { px(12.0) })
                    .px(px(14.0))
                    .bg(rgb(theme::SURFACE))
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .child(
                        Icon::new(RailgunSidebarIcon::Landmark)
                            .size_5()
                            .text_color(rgb(theme::PRIMARY)),
                    )
                    .when(!compact_header, |this| {
                        this.child(
                            app_strong_text("Governance")
                                .text_size(px(20.0))
                                .font_weight(FontWeight::SEMIBOLD),
                        )
                    })
                    .child(self.render_wallet_selector())
                    .child(picker)
                    .when(summary.inactive > 0, |this| {
                        this.child(app_status_tag(
                            format!("{} inactive", summary.inactive),
                            theme::WARNING,
                        ))
                    })
                    .child(self.render_chain_selector())
                    .child(div().flex_1().min_w(px(0.0)))
                    .child(app_refresh_button(
                        "governance-refresh",
                        "Refresh active governance tab",
                        refreshing,
                        true,
                        move |_window, cx| {
                            refresh_root.update(cx, |root, cx| match root.governance.tab {
                                GovernanceTab::Proposals => root.start_proposals_refresh(true, cx),
                                GovernanceTab::Staking => root.start_staking_refresh(cx),
                            });
                        },
                    )),
            )
            .child(tabs)
            .child(body_container)
    }

    fn render_staking_body(
        &mut self,
        root: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> gpui::Div {
        let mut content = div()
            .w(GOVERNANCE_CONTENT_WIDTH)
            .max_w_full()
            .mx_auto()
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap_3();
        let viewport_width = window.viewport_size().width;
        let sidebar_is_narrow = viewport_width < super::SIDEBAR_AUTO_COLLAPSE_WIDTH;
        let sidebar_collapsed = if sidebar_is_narrow {
            !self.sidebar_narrow_expanded
        } else {
            self.sidebar_manually_collapsed
        };
        let sidebar_width = if sidebar_collapsed {
            STAKING_COLLAPSED_SIDEBAR_WIDTH
        } else {
            super::SIDEBAR_WIDTH
        };
        let workspace_width = (viewport_width - sidebar_width).max(px(0.0));
        let table_available_width = (workspace_width.min(GOVERNANCE_CONTENT_WIDTH)
            - STAKING_CARD_HORIZONTAL_CHROME)
            .max(px(0.0));
        let table_layout = staking_table_layout(table_available_width);
        self.governance.clear_stale_position_details();
        let account_header_compact =
            staking_account_header_compact(table_available_width, table_layout.compact);
        let state = &self.governance.staking;
        let table_horizontal_scrollbar = table_available_width < STAKING_TABLE_MIN_WIDTH;
        if governance_contracts(self.selected_chain).is_none() {
            return content
                .child(app_status_tag(
                    "Staking is not deployed on this chain",
                    theme::TEXT_MUTED,
                ))
                .child(app_muted_text(
                    "Switch to Ethereum, BSC, or Polygon to view staking and rewards.",
                ));
        }
        match &state.status {
            StakingRefreshStatus::Error(error) => {
                return content
                    .child(Alert::error("governance-staking-error", error.to_string()).small())
                    .child(
                        app_button("governance-staking-retry", "Retry")
                            .small()
                            .on_click({
                                let root = root.clone();
                                move |_event, _window, cx| {
                                    root.update(cx, Self::start_staking_refresh);
                                }
                            }),
                    );
            }
            StakingRefreshStatus::Idle | StakingRefreshStatus::Loading
                if state.metrics.is_none() =>
            {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(app_muted_text("Loading staking data..."));
            }
            _ => {}
        }
        if let Some(metrics) = state.metrics.as_ref() {
            let token = governance_contracts(self.selected_chain)
                .map_or(Address::ZERO, |contracts| contracts.governance_token);
            let reward_interval = state.reward_interval_countdown;
            let remaining = state.chain_time().and_then(|now| {
                reward_interval
                    .and_then(|countdown| remaining_reward_interval_seconds(countdown, now))
            });
            let total_staked = format_staking_amount(
                self.selected_chain,
                token,
                metrics.total_staked,
                &self.effective_token_registry,
            );
            let voting_power = format_staking_amount(
                self.selected_chain,
                token,
                metrics.total_voting_power,
                &self.effective_token_registry,
            );
            let next_reward_interval = remaining.map_or_else(
                || "Unavailable".to_owned(),
                |seconds| format_compact_duration(Duration::from_secs(seconds)),
            );
            let interval_tooltip = reward_interval.map_or_else(
                || "Reward interval unavailable".to_owned(),
                |countdown| {
                    format!(
                        "Reward interval {} ends in {}",
                        countdown.interval, next_reward_interval
                    )
                },
            );
            let next_reward_interval_text = div()
                .id("governance-next-reward-interval-tooltip")
                .tooltip(move |window, cx| Tooltip::new(interval_tooltip.clone()).build(window, cx))
                .child(app_muted_text("next reward interval"));
            content = content.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_1()
                            .child(app_strong_text(total_staked))
                            .child(app_muted_text("staked")),
                    )
                    .child(app_muted_text("·"))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_1()
                            .child(app_strong_text(voting_power))
                            .child(app_muted_text("active voting power")),
                    )
                    .child(app_muted_text("·"))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_1()
                            .child(next_reward_interval_text)
                            .child(app_muted_text("in"))
                            .child(app_strong_text(next_reward_interval)),
                    ),
            );
            if matches!(state.status, StakingRefreshStatus::Stale) {
                content = content.child(app_status_tag(
                    "Showing the last complete refresh",
                    theme::WARNING,
                ));
            }
        }
        let participants = self.governance_participants();
        content = content.child(app_strong_text("Positions & rewards").text_size(px(16.0)));
        if participants.is_empty() {
            let picker_root = root.clone();
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .p_4()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .child(app_strong_text("Enroll accounts to get started"))
                    .child(app_muted_text(
                        "Choose Public accounts to view their positions and rewards.",
                    ))
                    .child(
                        app_button_base("governance-staking-enroll")
                            .outline()
                            .small()
                            .child("Enroll accounts")
                            .on_click(move |_event, _window, cx| {
                                picker_root.update(cx, |root, cx| {
                                    root.governance.participant_picker_open = true;
                                    cx.notify();
                                });
                            }),
                    ),
            );
        }
        for account in participants {
            let uuid = account.public_account_uuid.clone();
            let inactive = account.status == PublicAccountStatus::Inactive;
            let actor_uuid = uuid.clone();
            let account_action_ready = state.account_action_ready(&uuid);
            let token = governance_contracts(self.selected_chain)
                .map_or(Address::ZERO, |contracts| contracts.governance_token);
            let account_view = state.accounts.get(&uuid);
            let mut rows = Vec::new();
            let projection = account_view
                .and_then(|view| view.stakes.as_ref().ok())
                .map_or_else(StakingAccountProjection::default, |stakes| {
                    staking_account_projection(stakes)
                });
            let stakes_available = account_view.is_some_and(|view| view.stakes.is_ok());
            if let Some(view) = account_view
                && let Ok(stakes) = &view.stakes
            {
                for stake in stakes {
                    rows.push(stake_table_row(
                        stake,
                        account.address,
                        inactive,
                        account_action_ready,
                        self.selected_chain,
                        token,
                        &self.effective_token_registry,
                        state.chain_time(),
                    ));
                }
            }
            let voting_power_display = match account_view {
                None => "Loading...".to_owned(),
                Some(view) => match &view.voting_power {
                    Ok(amount) => format_staking_amount(
                        self.selected_chain,
                        token,
                        *amount,
                        &self.effective_token_registry,
                    ),
                    Err(_) => "Unavailable".to_owned(),
                },
            };
            let qualifier = account_view.and_then(|view| {
                let (Ok(voting_power), Ok(_)) = (&view.voting_power, &view.stakes) else {
                    return None;
                };
                let received = projection.received_voting_power(*voting_power);
                let own_display = format_staking_amount(
                    self.selected_chain,
                    token,
                    projection.active_self_delegated,
                    &self.effective_token_registry,
                );
                let received_display = format_staking_amount(
                    self.selected_chain,
                    token,
                    received,
                    &self.effective_token_registry,
                );
                let delegated_display = format_staking_amount(
                    self.selected_chain,
                    token,
                    projection.active_external_delegated,
                    &self.effective_token_registry,
                );
                match (
                    received.is_zero(),
                    projection.active_external_delegated.is_zero(),
                ) {
                    (true, true) => None,
                    (false, true) => Some(format!(
                        "includes {received_display} received from other accounts"
                    )),
                    (true, false) => Some(format!(
                        "{} staked · delegated away",
                        format_staking_amount(
                            self.selected_chain,
                            token,
                            projection.unclaimed_principal,
                            &self.effective_token_registry,
                        )
                    )),
                    (false, false) => Some(format!(
                        "{own_display} own · {received_display} received · {delegated_display} delegated away"
                    )),
                }
            });
            let identity = div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap_2()
                .child(
                    app_strong_text(
                        public_account_display_label(&account)
                            .unwrap_or_else(|| "Public account".to_owned()),
                    )
                    .min_w(px(0.0))
                    .truncate(),
                )
                .child(app_muted_text(short_address(&account.address)).flex_none())
                .child(clipboard_with_toast(
                    staking_control_id(&actor_uuid, "account-copy", "address"),
                    format!("{:#x}", account.address),
                ))
                .when(inactive, |this| {
                    this.child(app_status_tag("Inactive · read-only", theme::TEXT_MUTED))
                });
            let header = render_staking_account_header(
                identity,
                voting_power_metric(&voting_power_display, qualifier),
                account_header_compact,
                None,
            )
            .p_3();
            let mut card = div()
                .flex()
                .flex_col()
                .rounded_md()
                .bg(rgb(theme::SURFACE))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .child(header);
            let wallet_balance = state
                .current_account_ids
                .contains(&uuid)
                .then_some(account_view)
                .flatten()
                .and_then(|view| view.balance.as_ref().ok())
                .copied()
                .filter(|balance| !balance.is_zero());
            if let Some(_view) = account_view {
                if stakes_available && !rows.is_empty() {
                    let row_count = rows.len();
                    let table_height = STAKING_OVERVIEW_ROW_HEIGHT * row_count.clamp(1, 5);
                    let table_scrollbar = row_count > 5;
                    let table = {
                        let table_key =
                            format!("governance-stakes-{}-{}", self.selected_chain, uuid);
                        if let Some(table) = self.governance.staking_tables.get(&table_key) {
                            let table = table.clone();
                            table.update(cx, |table_state, cx| {
                                let (rows_changed, widths_changed) = {
                                    let delegate = table_state.delegate_mut();
                                    let rows_changed = delegate.set_rows(rows);
                                    let widths_changed = delegate.set_layout(&table_layout);
                                    (rows_changed, widths_changed)
                                };
                                if widths_changed {
                                    table_state.refresh(cx);
                                } else if rows_changed {
                                    cx.notify();
                                }
                            });
                            table
                        } else {
                            let delegate_root = root.downgrade();
                            let delegate_uuid = actor_uuid.clone();
                            let table = cx.new(|cx| {
                                TableState::new(
                                    StakeTableDelegate::new(
                                        delegate_root,
                                        delegate_uuid,
                                        account.address,
                                        rows,
                                        table_layout.clone(),
                                    ),
                                    window,
                                    cx,
                                )
                                .sortable(false)
                                .row_selectable(false)
                                .col_selectable(false)
                                .col_movable(false)
                                .col_resizable(false)
                            });
                            self.governance
                                .staking_tables
                                .insert(table_key, table.clone());
                            table
                        }
                    };
                    let vertical_scroll_handle = table.read(cx).vertical_scroll_handle.clone();
                    let table_scroller = div()
                        .w_full()
                        .h(table_height)
                        .min_w(px(0.0))
                        .min_h(px(0.0))
                        .relative()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .id(staking_control_id(&actor_uuid, "table-scroll", "positions"));
                    let table_scroller = table_scroller.child(
                        Table::new(&table)
                            .large()
                            .bordered(false)
                            .scrollbar_visible(false, table_horizontal_scrollbar),
                    );
                    let table_scroller = if table_scrollbar {
                        table_scroller.child(
                            div()
                                .occlude()
                                .absolute()
                                .top_0()
                                .right_0()
                                .bottom_0()
                                .w(STAKING_TABLE_SCROLLBAR_WIDTH)
                                .child(Scrollbar::vertical(&vertical_scroll_handle)),
                        )
                    } else {
                        table_scroller
                    };
                    card = card.child(table_scroller);
                } else if !stakes_available {
                    card = card.child(
                        div()
                            .px_3()
                            .py_2()
                            .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                            .flex()
                            .items_center()
                            .border_t_1()
                            .border_color(rgb(theme::BORDER_SUBTLE))
                            .child(app_muted_text("Stake rows unavailable for this account.")),
                    );
                } else if wallet_balance.is_none() {
                    card = card.child(
                        div()
                            .px_3()
                            .py_2()
                            .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                            .flex()
                            .items_center()
                            .border_t_1()
                            .border_color(rgb(theme::BORDER_SUBTLE))
                            .child(app_muted_text("No staking positions yet.")),
                    );
                }
            } else {
                card = card.child(
                    div()
                        .px_3()
                        .py_2()
                        .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                        .flex()
                        .items_center()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .child(app_muted_text("Loading account stakes...")),
                );
            }
            if !inactive && let Some(balance) = wallet_balance {
                let stake_root = root.clone();
                let stake_uuid = actor_uuid.clone();
                let stake_actor = account.address;
                let stake_button =
                    app_button_base(staking_control_id(&actor_uuid, "stake", "account"))
                        .primary()
                        .small()
                        .disabled(!state.global_action_ready());
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .child(app_muted_text(format!(
                            "{} in wallet",
                            format_token_amount_for_display(
                                self.selected_chain,
                                token,
                                balance,
                                Some(&self.effective_token_registry),
                            )
                        )))
                        .child(
                            div().flex_1().min_w(px(0.0)).flex().justify_end().child(
                                stake_button
                                    .child("Stake")
                                    .on_click(move |_event, window, cx| {
                                        stake_root.update(cx, |root, cx| {
                                            root.open_staking_action(
                                                stake_uuid.as_str(),
                                                stake_actor,
                                                StakingActionKind::Stake,
                                                window,
                                                cx,
                                            );
                                        });
                                    }),
                            ),
                        ),
                );
            }
            let reward_tokens = governance_contracts(self.selected_chain)
                .map_or(&[][..], |contracts| contracts.reward_tokens);
            let mut reward_token_addresses = reward_tokens
                .iter()
                .map(|token| token.token)
                .collect::<Vec<_>>();
            reward_token_addresses.sort();
            let all_rewards_resolved_zero = !reward_tokens.is_empty()
                && reward_tokens.iter().all(|token| {
                    let key = (uuid.clone(), token.token);
                    state.current_reward_keys.contains(&key)
                        && matches!(state.rewards.get(&key), Some(RewardView::Zero))
                });
            if all_rewards_resolved_zero {
                card = card.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .px_3()
                        .py_2()
                        .child(app_strong_text("Rewards").text_size(px(12.0)))
                        .child(app_muted_text("Nothing to claim yet.").text_size(px(12.0))),
                );
            } else {
                let positive_token_count = reward_tokens
                    .iter()
                    .filter(|token| {
                        let key = (uuid.clone(), token.token);
                        state.current_reward_keys.contains(&key)
                            && matches!(state.rewards.get(&key), Some(RewardView::Positive { .. }))
                    })
                    .count();
                let total_unclaimed = match reward_usd_total(
                    self.selected_chain,
                    &uuid,
                    &reward_token_addresses,
                    &state.rewards,
                    &state.current_reward_keys,
                    &self.public_broadcaster_anchor_cache,
                ) {
                    RewardUsdState::Loading => "Loading...".to_owned(),
                    RewardUsdState::Unavailable => "USD unavailable".to_owned(),
                    RewardUsdState::Value(value) => {
                        format!("{} unclaimed", format_usd_micro_value(value))
                    }
                };
                let claim_all_ready = positive_token_count >= 2
                    && state.reward_claim_all_ready(&uuid, &reward_token_addresses)
                    && !inactive
                    && self.governance.action_flow.selection.is_none();
                let claim_all_root = root.clone();
                let claim_all_uuid = uuid.clone();
                let claim_all_actor = account.address;
                let claim_all_button =
                    app_button_base(staking_control_id(&uuid, "reward-claim-all", "account"))
                        .primary()
                        .small()
                        .child("Claim all")
                        .on_click(move |_event, window, cx| {
                            claim_all_root.update(cx, |root, cx| {
                                root.open_staking_action(
                                    claim_all_uuid.as_str(),
                                    claim_all_actor,
                                    StakingActionKind::RewardClaimAll,
                                    window,
                                    cx,
                                );
                            });
                        });
                let reward_rows = reward_tokens.iter().filter_map(|token| {
                    let reward_key = (uuid.clone(), token.token);
                    let reward = state.rewards.get(&reward_key);
                    let resolved_reward = state
                        .current_reward_keys
                        .contains(&reward_key)
                        .then_some(reward)
                        .flatten();
                    if matches!(resolved_reward, Some(RewardView::Zero)) {
                        return None;
                    }
                    Some(render_reward_row(
                        self.selected_chain,
                        account.address,
                        &uuid,
                        inactive,
                        token.symbol,
                        token.token,
                        resolved_reward,
                        resolved_reward,
                        &self.effective_token_registry,
                        state.reward_action_ready(&uuid, token.token),
                        &self.public_broadcaster_anchor_cache,
                        root.clone(),
                    ))
                });
                let reward_header = div()
                    .flex()
                    .items_center()
                    .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w(px(0.0))
                            .child(app_strong_text("Rewards").text_size(px(12.0)))
                            .child(app_muted_text(total_unclaimed).text_size(px(11.0))),
                    )
                    .when(claim_all_ready, |this| {
                        this.child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .justify_end()
                                .child(claim_all_button),
                        )
                    });
                card = card.child(
                    div()
                        .flex()
                        .flex_col()
                        .border_t_1()
                        .border_color(rgb(theme::BORDER_SUBTLE))
                        .px_3()
                        .child(reward_header)
                        .children(reward_rows),
                );
            }
            content = content.child(card);
        }
        content
    }
}

fn voting_power_metric(value: &str, qualifier: Option<String>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w(px(0.0))
        .items_end()
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_1()
                .child(app_strong_text(value.to_owned()))
                .child(app_muted_text("voting power")),
        )
        .when_some(qualifier, |this, qualifier| {
            this.child(
                app_muted_text(qualifier)
                    .text_size(px(11.0))
                    .whitespace_normal(),
            )
        })
}

fn staking_account_header_compact(available_width: gpui::Pixels, table_compact: bool) -> bool {
    table_compact || available_width < STAKING_ACCOUNT_HEADER_STACK_BREAKPOINT
}

fn render_staking_account_header(
    identity: gpui::Div,
    voting_power: gpui::Div,
    compact: bool,
    debug_prefix: Option<&str>,
) -> gpui::Div {
    let identity = identity.when(compact, |this| this.w_full().flex_initial());
    let identity = if let Some(prefix) = debug_prefix {
        let selector = format!("{prefix}-identity");
        identity.debug_selector(move || selector)
    } else {
        identity
    };
    let voting_power = if let Some(prefix) = debug_prefix {
        let selector = format!("{prefix}-metric-0");
        voting_power.debug_selector(move || selector)
    } else {
        voting_power
    };
    let voting_power = voting_power
        .flex_none()
        .when(compact, |this| this.w_full().items_end());
    if compact {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(identity)
            .child(voting_power)
    } else {
        div()
            .flex()
            .items_center()
            .flex_wrap()
            .justify_between()
            .gap_3()
            .child(identity)
            .child(voting_power)
    }
}

fn format_staking_amount(
    chain_id: u64,
    token: Address,
    amount: U256,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
) -> String {
    if governance_contracts(chain_id).is_some_and(|contracts| contracts.governance_token == token) {
        let symbol = token_display_metadata(Some(registry), chain_id, &token)
            .map(|metadata| metadata.symbol)
            .or_else(|| {
                governance_contracts(chain_id).and_then(|contracts| {
                    contracts
                        .reward_tokens
                        .iter()
                        .find(|reward_token| reward_token.token == token)
                        .map(|reward_token| reward_token.symbol.to_owned())
                })
            })
            .unwrap_or_else(|| short_address(&token));
        format!("{} {symbol}", format_compact_rail_amount(amount))
    } else {
        format_token_amount_for_display(chain_id, token, amount, Some(registry))
    }
}

fn format_reward_amount(
    chain_id: u64,
    token: Address,
    amount: U256,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
) -> String {
    if governance_contracts(chain_id).is_some_and(|contracts| contracts.governance_token == token) {
        return format_compact_rail_amount(amount);
    }
    token_display_metadata(Some(registry), chain_id, &token).map_or_else(
        || amount.to_string(),
        |metadata| railgun_ui::format_token_amount(amount, metadata.decimals),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StakeTableAction {
    Delegate,
    Undelegate,
    Unlock,
    PrincipalClaim,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StakeTableRow {
    id: U256,
    amount: String,
    created: String,
    delegate: Address,
    delegate_label: String,
    state: String,
    state_detail: String,
    externally_delegated: bool,
    actions: Vec<StakeTableAction>,
    disabled: bool,
}

fn staking_control_id(
    actor_uuid: &str,
    action: &str,
    subject: impl std::fmt::Display,
) -> SharedString {
    SharedString::from(format!(
        "governance-staking-{action}-{actor_uuid}-{subject}"
    ))
}

fn compact_stake_id_preview(id: U256) -> String {
    let decimal = id.to_string();
    if decimal.len() <= 8 {
        return decimal;
    }
    format!("{}…{}", &decimal[..3], &decimal[decimal.len() - 2..])
}

fn stake_state_color(state: &str) -> u32 {
    match state {
        "Active" => theme::SUCCESS,
        "Unlocking" => theme::WARNING,
        "Claimable" => theme::INFO,
        _ => theme::TEXT_MUTED,
    }
}

fn stake_table_row(
    stake: &StakePosition,
    actor: Address,
    inactive: bool,
    account_action_ready: bool,
    chain_id: u64,
    token: Address,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
    chain_time: Option<U256>,
) -> StakeTableRow {
    let state = match stake.state {
        StakeState::Active => "Active",
        StakeState::Unlocking => "Unlocking",
        StakeState::Claimable => "Claimable",
        StakeState::Claimed => "Claimed",
    };
    let state_detail = match stake.state {
        StakeState::Active | StakeState::Claimed => String::new(),
        StakeState::Unlocking if stake.locktime.is_zero() => {
            "Claimable date unavailable".to_owned()
        }
        StakeState::Unlocking => {
            let date = format_date_short(&stake.locktime);
            let remaining = chain_time
                .and_then(|now| stake.locktime.checked_sub(now))
                .and_then(|seconds| seconds.try_into().ok());
            remaining.map_or_else(
                || format!("Claimable after {date}"),
                |seconds: u64| {
                    format!(
                        "{} remaining · Claimable {date}",
                        format_compact_duration(Duration::from_secs(seconds)),
                    )
                },
            )
        }
        StakeState::Claimable if stake.locktime.is_zero() => "Unlocked".to_owned(),
        StakeState::Claimable => {
            format!("Unlocked · {}", format_date_short(&stake.locktime))
        }
    };
    let actions = match stake.state {
        StakeState::Active if stake.delegate == actor => {
            vec![StakeTableAction::Delegate, StakeTableAction::Unlock]
        }
        StakeState::Active => vec![StakeTableAction::Undelegate, StakeTableAction::Unlock],
        StakeState::Unlocking | StakeState::Claimed => vec![],
        StakeState::Claimable => vec![StakeTableAction::PrincipalClaim],
    };
    StakeTableRow {
        id: stake.id,
        amount: format_staking_amount(chain_id, token, stake.amount, registry),
        created: format_date_short(&stake.staketime),
        delegate: stake.delegate,
        delegate_label: if stake.delegate == actor {
            "Self-delegated".to_owned()
        } else {
            short_address(&stake.delegate)
        },
        state: state.to_owned(),
        state_detail,
        externally_delegated: stake.delegate != actor,
        actions,
        disabled: inactive || !account_action_ready,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StakeTableColumnKind {
    Stake,
    Amount,
    Delegate,
    State,
    Actions,
}

#[derive(Clone, Debug)]
struct StakingTableLayout {
    compact: bool,
    columns: Vec<(StakeTableColumnKind, Column)>,
}

struct StakeTableDelegate {
    root: WeakEntity<WalletRoot>,
    actor_uuid: String,
    actor: Address,
    rows: Arc<[StakeTableRow]>,
    columns: Vec<(StakeTableColumnKind, Column)>,
    compact: bool,
    pending_keyboard_focus: Option<U256>,
}

fn staking_table_layout(available_width: gpui::Pixels) -> StakingTableLayout {
    let target_width = available_width.max(STAKING_TABLE_MIN_WIDTH);
    let compact = available_width < STAKING_TABLE_COMPACT_BREAKPOINT;
    let (kinds, widths) = if compact {
        let fixed_width = STAKING_TABLE_STAKE_WIDTH
            + STAKING_TABLE_AMOUNT_WIDTH
            + STAKING_TABLE_STATE_WIDTH
            + STAKING_TABLE_COMPACT_ACTIONS_WIDTH;
        let delegate_width =
            (target_width - fixed_width).max(STAKING_TABLE_COMPACT_DELEGATE_MIN_WIDTH);
        let widths = vec![
            STAKING_TABLE_STAKE_WIDTH,
            STAKING_TABLE_AMOUNT_WIDTH,
            delegate_width,
            STAKING_TABLE_STATE_WIDTH,
            STAKING_TABLE_COMPACT_ACTIONS_WIDTH,
        ];
        (
            vec![
                StakeTableColumnKind::Stake,
                StakeTableColumnKind::Amount,
                StakeTableColumnKind::Delegate,
                StakeTableColumnKind::State,
                StakeTableColumnKind::Actions,
            ],
            widths,
        )
    } else {
        let fixed_width = STAKING_TABLE_STAKE_WIDTH
            + STAKING_TABLE_AMOUNT_WIDTH
            + STAKING_TABLE_STATE_WIDTH
            + STAKING_TABLE_ACTIONS_WIDTH;
        let delegate_width = (target_width - fixed_width).max(px(0.0));
        let widths = vec![
            STAKING_TABLE_STAKE_WIDTH,
            STAKING_TABLE_AMOUNT_WIDTH,
            delegate_width,
            STAKING_TABLE_STATE_WIDTH,
            STAKING_TABLE_ACTIONS_WIDTH,
        ];
        (
            vec![
                StakeTableColumnKind::Stake,
                StakeTableColumnKind::Amount,
                StakeTableColumnKind::Delegate,
                StakeTableColumnKind::State,
                StakeTableColumnKind::Actions,
            ],
            widths,
        )
    };
    let columns = kinds
        .into_iter()
        .zip(widths)
        .map(|(kind, width)| {
            let (key, name) = match kind {
                StakeTableColumnKind::Stake => ("stake", "Stake"),
                StakeTableColumnKind::Amount => ("amount", "Amount"),
                StakeTableColumnKind::Delegate => ("delegate", "Delegate"),
                StakeTableColumnKind::State => ("state", "State"),
                StakeTableColumnKind::Actions => ("actions", "Actions"),
            };
            let column = Column::new(key, name).width(width).movable(false);
            let column = if compact && kind != StakeTableColumnKind::Actions {
                column.paddings(gpui::Edges::all(px(2.0)))
            } else if kind == StakeTableColumnKind::Actions {
                column.paddings(gpui::Edges {
                    top: px(4.0),
                    right: px(12.0),
                    bottom: px(4.0),
                    left: px(4.0),
                })
            } else {
                column
            };
            (kind, column)
        })
        .collect();
    StakingTableLayout { compact, columns }
}

impl StakeTableDelegate {
    fn new(
        root: WeakEntity<WalletRoot>,
        actor_uuid: String,
        actor: Address,
        rows: Vec<StakeTableRow>,
        layout: StakingTableLayout,
    ) -> Self {
        Self {
            root,
            actor_uuid,
            actor,
            rows: Arc::from(rows),
            compact: layout.compact,
            columns: layout.columns,
            pending_keyboard_focus: None,
        }
    }

    fn set_rows(&mut self, rows: Vec<StakeTableRow>) -> bool {
        let rows: Arc<[StakeTableRow]> = Arc::from(rows);
        if self
            .pending_keyboard_focus
            .is_some_and(|id| !rows.iter().any(|row| row.id == id))
        {
            self.pending_keyboard_focus = None;
        }
        if self.rows.as_ref() == rows.as_ref() {
            return false;
        }
        self.rows = rows;
        true
    }

    fn set_layout(&mut self, layout: &StakingTableLayout) -> bool {
        if self.compact != layout.compact
            || self.columns.len() != layout.columns.len()
            || self.columns.iter().zip(&layout.columns).any(
                |((kind, column), (next_kind, next_column))| {
                    kind != next_kind || column.width != next_column.width
                },
            )
        {
            if self.compact && !layout.compact {
                self.pending_keyboard_focus = None;
            }
            self.compact = layout.compact;
            self.columns.clone_from(&layout.columns);
            true
        } else {
            false
        }
    }
}

fn render_staking_table_row(row_ix: usize) -> gpui::Stateful<gpui::Div> {
    div().id(("row", row_ix)).relative().child(
        div()
            .id(("row-pointer-guard", row_ix))
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .on_click(|_event, _window, cx| cx.stop_propagation()),
    )
}

impl TableDelegate for StakeTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        &self.columns[col_ix].1
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        render_staking_table_row(row_ix)
    }

    fn render_header(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("header")
            .absolute()
            .h(px(0.0))
            .max_h(px(0.0))
            .border_0()
            .overflow_hidden()
    }

    fn render_th(
        &mut self,
        _col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        div().h(px(0.0)).max_h(px(0.0)).overflow_hidden()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        let table = cx.entity();
        match self.columns[col_ix].0 {
            StakeTableColumnKind::Stake => {
                let key = (self.actor_uuid.clone(), row.id);
                let details_root = self.root.clone();
                let details_root_for_keyboard = self.root.clone();
                let details_key = key.clone();
                let details_key_for_keyboard = key.clone();
                let details_key_for_content = key.clone();
                let table_for_keyboard = table.clone();
                let table_for_open_change = table;
                let details_open = self
                    .root
                    .read_with(cx, |root, _| {
                        root.governance.compact_position_details.as_ref() == Some(&key)
                    })
                    .unwrap_or(false);
                let focus_on_materialize =
                    if details_open && self.pending_keyboard_focus == Some(row.id) {
                        self.pending_keyboard_focus = None;
                        true
                    } else {
                        false
                    };
                let delegate = row.delegate.to_checksum(None);
                let delegate_for_copy = delegate.clone();
                let full_id = row.id.to_string();
                let amount = row.amount.clone();
                let created = row.created.clone();
                let state = row.state.clone();
                let state_detail = row.state_detail.clone();
                let trigger = Button::new(staking_control_id(
                    &self.actor_uuid,
                    "stake-details-trigger",
                    row.id,
                ))
                .text()
                .xsmall()
                .child(
                    app_muted_text(format!("#{}", compact_stake_id_preview(row.id)))
                        .font_family(APP_MONO_FONT_FAMILY),
                )
                .on_key_down(move |event: &KeyDownEvent, _window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                        if let Some(root) = details_root_for_keyboard.upgrade() {
                            let currently_open =
                                root.read(cx).governance.compact_position_details.as_ref()
                                    == Some(&details_key_for_keyboard);
                            table_for_keyboard.update(cx, |table, _| {
                                table.delegate_mut().pending_keyboard_focus =
                                    (!currently_open).then_some(details_key_for_keyboard.1);
                            });
                            root.update(cx, |root, cx| {
                                let next = if root.governance.compact_position_details.as_ref()
                                    == Some(&details_key_for_keyboard)
                                {
                                    None
                                } else {
                                    Some(details_key_for_keyboard.clone())
                                };
                                root.set_compact_position_details(next, cx);
                            });
                        }
                        cx.stop_propagation();
                    }
                });
                let popover = Popover::new(staking_control_id(
                    &self.actor_uuid,
                    "stake-details-popover",
                    row.id,
                ))
                .open(details_open)
                .on_open_change(move |open, _window, cx| {
                    table_for_open_change.update(cx, |table, _| {
                        table.delegate_mut().pending_keyboard_focus = None;
                    });
                    if let Some(root) = details_root.upgrade() {
                        root.update(cx, |root, cx| {
                            let next = if *open {
                                Some(details_key.clone())
                            } else if root.governance.compact_position_details.as_ref()
                                == Some(&details_key)
                            {
                                None
                            } else {
                                root.governance.compact_position_details.clone()
                            };
                            root.set_compact_position_details(next, cx);
                        });
                    }
                })
                .trigger(trigger)
                .content(move |popover_state, window, cx| {
                    let focus_handle = popover_state.focus_handle(cx);
                    if focus_on_materialize {
                        focus_handle.focus(window);
                    }
                    let full_id = full_id.clone();
                    let amount = amount.clone();
                    let created = created.clone();
                    let delegate = delegate.clone();
                    let delegate_for_copy = delegate_for_copy.clone();
                    let state = state.clone();
                    let state_detail = state_detail.clone();
                    let details_key = details_key_for_content.clone();
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_2()
                        .w(px(300.0))
                        .max_w(px(300.0))
                        .child(app_muted_text("Stake").text_size(px(11.0)))
                        .child(app_text(format!("#{full_id}")).font_family(APP_MONO_FONT_FAMILY))
                        .child(app_muted_text("Amount").text_size(px(11.0)))
                        .child(app_text(amount).whitespace_normal())
                        .child(app_muted_text("Created").text_size(px(11.0)))
                        .child(app_text(created).whitespace_normal())
                        .child(app_muted_text("Delegate").text_size(px(11.0)))
                        .child(
                            div()
                                .flex()
                                .items_start()
                                .gap_1()
                                .child(
                                    app_text(delegate)
                                        .font_family(APP_MONO_FONT_FAMILY)
                                        .whitespace_normal()
                                        .min_w(px(0.0))
                                        .flex_1(),
                                )
                                .child(clipboard_with_toast(
                                    staking_control_id(
                                        &details_key.0,
                                        "stake-details-delegate-copy",
                                        details_key.1,
                                    ),
                                    delegate_for_copy,
                                )),
                        )
                        .child(app_muted_text("State").text_size(px(11.0)))
                        .child(app_text(state))
                        .when(!state_detail.is_empty(), |this| {
                            this.child(app_muted_text(state_detail).whitespace_normal())
                        })
                });
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .child(popover)
                    .into_any_element()
            }
            StakeTableColumnKind::Amount => div()
                .h_full()
                .flex()
                .items_center()
                .child(
                    app_strong_text(row.amount.clone()).when(self.compact, gpui::Styled::truncate),
                )
                .into_any_element(),
            StakeTableColumnKind::Delegate => {
                let copy_id = staking_control_id(&self.actor_uuid, "delegate-copy", row.id);
                div()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(app_muted_text(row.delegate_label.clone()).truncate())
                    .when(row.externally_delegated, |this| {
                        this.child(clipboard_with_toast(
                            copy_id,
                            format!("{:#x}", row.delegate),
                        ))
                    })
                    .into_any_element()
            }
            StakeTableColumnKind::State => div()
                .h_full()
                .flex()
                .items_center()
                .gap_1()
                .min_w(px(0.0))
                .child(div().flex_none().child(app_status_tag(
                    row.state.clone(),
                    stake_state_color(&row.state),
                )))
                .when(!row.state_detail.is_empty(), |this| {
                    let detail_id = staking_control_id(&self.actor_uuid, "state-detail", row.id);
                    let detail = row.state_detail.clone();
                    let tooltip_detail = detail.clone();
                    this.child(
                        div()
                            .id(detail_id)
                            .flex_1()
                            .min_w(px(0.0))
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip_detail.clone()).build(window, cx)
                            })
                            .child(app_muted_text(detail).text_size(px(11.0)).truncate()),
                    )
                })
                .into_any_element(),
            StakeTableColumnKind::Actions => {
                let mut actions = div().h_full().flex().items_center().justify_end().gap_1();
                for action in &row.actions {
                    let (label, discriminator, kind) = match action {
                        StakeTableAction::Delegate => (
                            "Delegate",
                            "delegate",
                            StakingActionKind::Delegate { stake_id: row.id },
                        ),
                        StakeTableAction::Undelegate => (
                            "Undelegate",
                            "undelegate",
                            StakingActionKind::Undelegate { stake_id: row.id },
                        ),
                        StakeTableAction::Unlock => {
                            let label = if row.externally_delegated && !self.compact {
                                "Unlock · 2 steps"
                            } else {
                                "Unlock"
                            };
                            (
                                label,
                                "unlock",
                                StakingActionKind::Unlock { stake_id: row.id },
                            )
                        }
                        StakeTableAction::PrincipalClaim => (
                            "Claim principal",
                            "principal-claim",
                            StakingActionKind::PrincipalClaim { stake_id: row.id },
                        ),
                    };
                    let root = self.root.clone();
                    let actor = self.actor;
                    let actor_uuid = self.actor_uuid.clone();
                    let id = staking_control_id(&actor_uuid, discriminator, row.id);
                    let is_primary = matches!(action, StakeTableAction::PrincipalClaim);
                    let button = app_button_base(id).small().disabled(row.disabled);
                    let button = if is_primary {
                        button.primary()
                    } else {
                        button.outline()
                    };
                    let button = button.child(label).on_click(move |_event, window, cx| {
                        if let Some(root) = root.upgrade() {
                            root.update(cx, |root, cx| {
                                root.open_staking_action(
                                    actor_uuid.as_str(),
                                    actor,
                                    kind,
                                    window,
                                    cx,
                                );
                            });
                        }
                    });
                    actions = actions.child(button);
                }
                if row.state == "Unlocking" {
                    actions = actions.child(
                        app_button_base(staking_control_id(
                            &self.actor_uuid,
                            "claim-principal",
                            row.id,
                        ))
                        .outline()
                        .small()
                        .disabled(true)
                        .child("Claim principal"),
                    );
                }
                actions.into_any_element()
            }
        }
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        app_muted_text("No staking positions yet.").into_any_element()
    }
}

fn render_reward_row(
    chain_id: u64,
    actor: Address,
    actor_uuid: &str,
    inactive: bool,
    symbol: &str,
    token: Address,
    reward: Option<&RewardView>,
    resolved_reward: Option<&RewardView>,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
    reward_action_ready: bool,
    anchor_cache: &TokenAnchorRateCache,
    root: Entity<WalletRoot>,
) -> gpui::Div {
    let metadata = token_display_metadata(Some(registry), chain_id, &token);
    let display_symbol = metadata
        .as_ref()
        .map_or_else(|| symbol.to_owned(), |info| info.symbol.clone());
    let icon_path = metadata.and_then(|info| info.icon_path);
    let display_amount = |amount| format_reward_amount(chain_id, token, amount, registry);
    let (amount, detail, available) = match reward {
        None => (
            "Loading...".to_owned(),
            "Calculation pending".to_owned(),
            false,
        ),
        Some(RewardView::Zero) => (
            display_amount(U256::ZERO),
            "No completed unclaimed intervals".to_owned(),
            false,
        ),
        Some(RewardView::Positive { amount, .. }) => (display_amount(*amount), String::new(), true),
        Some(RewardView::Unavailable(error)) => {
            ("Unavailable".to_owned(), error.to_string(), false)
        }
    };
    let usd_label = reward_usd_label(chain_id, token, resolved_reward, anchor_cache);
    let claim_root = root;
    let actor_uuid = actor_uuid.to_owned();
    let claim_button = app_button_base(staking_control_id(&actor_uuid, "reward-claim", token))
        .outline()
        .small()
        .disabled(!available || inactive || !reward_action_ready);
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .min_w(px(0.0))
        .py_2()
        .min_h(STAKING_OVERVIEW_ROW_HEIGHT)
        .border_t_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(
            token_label_row(SharedString::from(display_symbol), icon_path, px(16.0))
                .w(px(110.0))
                .truncate(),
        )
        .child(
            div()
                .w(px(180.0))
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(app_strong_text(amount).truncate()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .min_w(px(0.0))
                .when_some(usd_label, |column, usd_label| {
                    column.child(app_muted_text(usd_label).whitespace_nowrap())
                })
                .when(!detail.is_empty(), |column| {
                    column.child(app_muted_text(detail).truncate())
                }),
        )
        .child(
            div()
                .ml_auto()
                .flex_none()
                .child(
                    claim_button
                        .child("Claim")
                        .on_click(move |_event, window, cx| {
                            claim_root.update(cx, |root, cx| {
                                root.open_staking_action(
                                    actor_uuid.as_str(),
                                    actor,
                                    StakingActionKind::RewardClaim { token },
                                    window,
                                    cx,
                                );
                            });
                        }),
                ),
        )
}

pub(super) fn unlock_period_label(seconds: U256) -> String {
    let day = U256::from(86_400u64);
    if !seconds.is_zero() && seconds % day == U256::ZERO {
        let days = seconds / day;
        return format!(
            "{} day{}",
            days,
            if days == U256::from(1) { "" } else { "s" }
        );
    }
    u64::try_from(seconds).ok().map_or_else(
        || format!("{seconds} seconds"),
        |seconds| format_compact_duration(Duration::from_secs(seconds)),
    )
}

fn render_staking_action_form(
    root: &Entity<WalletRoot>,
    wallet: &WalletRoot,
    content_width: gpui::Pixels,
    cx: &App,
) -> Option<gpui::Div> {
    let selection = wallet.governance.action_flow.staking_selection()?;
    let selection_ready = wallet.governance.staking.action_selection_ready(selection);
    if !selection.kind.is_compose_action() {
        let pending = wallet.governance.action_flow.pending;
        let error = wallet.governance.action_flow.error.as_ref();
        let cancel_root = root.clone();
        let retry_root = root.clone();
        let status = if pending || error.is_none() {
            div()
                .w_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .py(px(8.0))
                .child(Spinner::new().small())
                .child(
                    app_muted_text(if selection.kind.is_reward_action() {
                        "Refreshing rewards and estimating the exact call…"
                    } else {
                        "Checking the stake and estimating the exact call…"
                    })
                    .text_size(px(12.0)),
                )
        } else {
            div().w_full().flex().flex_col().gap_3().child(
                Alert::error(
                    "governance-staking-action-error",
                    error.map_or_else(String::new, ToString::to_string),
                )
                .small(),
            )
        };
        return Some(
            div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .child(status)
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            app_button_base("governance-staking-action-cancel")
                                .ghost()
                                .small()
                                .child("Cancel")
                                .on_click(move |_event, window, cx| {
                                    cancel_root.update(cx, WalletRoot::close_staking_action);
                                    window.close_dialog(cx);
                                }),
                        )
                        .when(!pending && error.is_some(), |this| {
                            this.child(
                                app_button_base("governance-staking-action-retry")
                                    .small()
                                    .child("Try again")
                                    .on_click(move |_event, window, cx| {
                                        retry_root.update(cx, |root, cx| {
                                            root.review_staking_action(window, cx);
                                        });
                                    }),
                            )
                        }),
                ),
        );
    }

    let amount_input = &wallet.governance.proposal_action_amount_input;
    let delegate_input = &wallet.governance.staking_delegate_input;
    let account = wallet
        .governance_participants()
        .into_iter()
        .find(|account| {
            account.public_account_uuid == selection.actor_uuid
                && account.address == selection.actor
        });
    let actor_label = account
        .as_ref()
        .and_then(public_account_display_label)
        .unwrap_or_else(|| "Public account".to_owned());
    let chain_id = wallet.selected_chain;
    let token = governance_contracts(chain_id)
        .map_or(Address::ZERO, |contracts| contracts.governance_token);
    let decimals = governance_contracts(chain_id)
        .and_then(|contracts| {
            wallet
                .effective_token_registry
                .get(chain_id, &contracts.governance_token)
        })
        .map(|token| token.decimals);
    let amount = wallet_ops::parse_send_amount(amount_input.read(cx).value().as_ref(), decimals)
        .ok()
        .filter(|amount| !amount.is_zero());
    let cached_balance = wallet
        .governance
        .staking
        .accounts
        .get(&selection.actor_uuid)
        .and_then(|view| view.balance.as_ref().ok())
        .copied();
    let balance_display = cached_balance.map_or_else(
        || "Balance unavailable".to_owned(),
        |balance| format_staking_amount(chain_id, token, balance, &wallet.effective_token_registry),
    );
    let review_root = root.clone();
    let cancel_root = root.clone();
    let mut content = div().w(content_width).flex().flex_col().gap_3();
    match selection.kind {
        StakingActionKind::Stake => {
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(app_strong_text(actor_label))
                            .child(
                                app_muted_text(short_address(&selection.actor))
                                    .font_family(APP_MONO_FONT_FAMILY),
                            ),
                    )
                    .child(app_muted_text(balance_display)),
            );
            let max_root = root.clone();
            let max_input = amount_input.clone();
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().flex().items_center().justify_end().when_some(
                        cached_balance,
                        |this, balance| {
                            this.child(
                                app_button_base("governance-staking-max")
                                    .link()
                                    .xsmall()
                                    .compact()
                                    .child(format!(
                                        "Max: {} RAIL",
                                        format_send_amount_input(balance, decimals)
                                    ))
                                    .on_click(move |_event, window, cx| {
                                        max_input.update(cx, |input, cx| {
                                            input.set_value(
                                                format_send_amount_input(balance, decimals),
                                                window,
                                                cx,
                                            );
                                            cx.notify();
                                        });
                                        max_root.update(cx, |_, cx| cx.notify());
                                    }),
                            )
                        },
                    ))
                    .child(
                        app_input(amount_input)
                            .small()
                            .w_full()
                            .suffix(app_muted_text("RAIL").text_size(px(11.0))),
                    ),
            );
        }
        StakingActionKind::Delegate { stake_id } => {
            let position = wallet
                .governance
                .staking
                .accounts
                .get(&selection.actor_uuid)
                .and_then(|account| account.stakes.as_ref().ok())
                .and_then(|stakes| stakes.iter().find(|stake| stake.id == stake_id));
            let current_delegate = position.map_or(Address::ZERO, |position| position.delegate);
            let delegation_label = if current_delegate == selection.actor {
                "self-delegated".to_owned()
            } else {
                format!("delegated to {}", short_address(&current_delegate))
            };
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                    .child(app_strong_text(format!(
                        "Stake #{} · {}",
                        stake_id,
                        position.map_or_else(
                            || "Unknown amount".to_owned(),
                            |position| format_staking_amount(
                                chain_id,
                                token,
                                position.amount,
                                &wallet.effective_token_registry
                            )
                        )
                    )))
                    .child(app_muted_text(format!(
                        "{} {} · {}",
                        actor_label,
                        short_address(&selection.actor),
                        delegation_label
                    ))),
            );
            let parsed_delegate = Address::from_str(delegate_input.read(cx).value().trim())
                .ok()
                .filter(|address| *address != Address::ZERO);
            let delegate_error = if !delegate_input.read(cx).value().trim().is_empty()
                && parsed_delegate.is_none()
            {
                Some("Enter a valid Ethereum address")
            } else if parsed_delegate == Some(current_delegate) {
                Some("This stake is already delegated to that address.")
            } else {
                None
            };
            content = content.child(app_input(delegate_input).small().w_full());
            if let Some(error) = delegate_error {
                content = content.child(
                    app_muted_text(error)
                        .text_color(rgb(theme::DANGER))
                        .text_size(px(11.0)),
                );
            }
            content = content.child(
                app_muted_text(
                    "The delegate gets the voting power and rewards. The stake remains yours.",
                )
                .text_size(px(11.0))
                .whitespace_normal(),
            );
        }
        _ => return None,
    }
    let delegate_ready = if let StakingActionKind::Delegate { .. } = selection.kind {
        let position = wallet
            .governance
            .staking
            .accounts
            .get(&selection.actor_uuid)
            .and_then(|account| account.stakes.as_ref().ok())
            .and_then(|stakes| {
                selection
                    .kind
                    .stake_id()
                    .and_then(|id| stakes.iter().find(|stake| stake.id == id))
            });
        let current = position.map(|position| position.delegate);
        Address::from_str(delegate_input.read(cx).value().trim())
            .ok()
            .filter(|address| *address != Address::ZERO)
            .is_some_and(|address| Some(address) != current)
    } else {
        amount.is_some()
    };
    let ready = selection_ready && delegate_ready && !wallet.governance.action_flow.pending;
    content = content.child(
        div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                app_button_base("governance-staking-cancel")
                    .ghost()
                    .small()
                    .child("Cancel")
                    .on_click(move |_event, window, cx| {
                        cancel_root.update(cx, WalletRoot::close_staking_action);
                        window.close_dialog(cx);
                    }),
            )
            .child(
                app_button_base("governance-staking-review")
                    .primary()
                    .small()
                    .loading(wallet.governance.action_flow.pending)
                    .disabled(!ready)
                    .child(if wallet.governance.action_flow.pending {
                        "Preparing authorization…"
                    } else {
                        "Prepare authorization"
                    })
                    .on_click(move |_event, window, cx| {
                        review_root.update(cx, |root, cx| root.review_staking_action(window, cx));
                    }),
            ),
    );
    if let Some(error) = wallet.governance.action_flow.error.as_ref() {
        content = content
            .child(Alert::error("governance-staking-action-error", error.to_string()).small());
    }
    Some(content)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use gpui::{
        AppContext as _, Bounds, InteractiveElement as _, Render, TestAppContext,
        VisualTestContext, WindowBounds, WindowOptions, point, size,
    };

    use super::*;
    use gpui_component::scroll::ScrollbarHandle;

    struct TableProbeDelegate {
        columns: Vec<(StakeTableColumnKind, Column)>,
        compact: bool,
        row_count: usize,
        action_clicked: Option<Arc<AtomicBool>>,
    }

    impl TableProbeDelegate {
        fn new() -> Self {
            Self::with_rows(5)
        }

        fn with_rows(row_count: usize) -> Self {
            let layout = staking_table_layout(STAKING_TABLE_MIN_WIDTH);
            Self {
                compact: layout.compact,
                columns: layout.columns,
                row_count,
                action_clicked: None,
            }
        }

        fn with_action_callback(action_clicked: Arc<AtomicBool>) -> Self {
            let mut delegate = Self::new();
            delegate.action_clicked = Some(action_clicked);
            delegate
        }
    }

    impl TableDelegate for TableProbeDelegate {
        fn columns_count(&self, _: &App) -> usize {
            self.columns.len()
        }

        fn rows_count(&self, _: &App) -> usize {
            self.row_count
        }

        fn column(&self, col_ix: usize, _: &App) -> &Column {
            &self.columns[col_ix].1
        }

        fn render_tr(
            &mut self,
            row_ix: usize,
            _window: &mut Window,
            _cx: &mut Context<'_, TableState<Self>>,
        ) -> gpui::Stateful<gpui::Div> {
            let row = render_staking_table_row(row_ix);
            if row_ix == 5 {
                row.debug_selector(|| "staking-test-row-six".to_owned())
            } else if row_ix == 0 {
                row.debug_selector(|| "staking-test-row".to_owned())
            } else {
                row
            }
        }

        fn render_header(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, TableState<Self>>,
        ) -> gpui::Stateful<gpui::Div> {
            div()
                .id("header")
                .absolute()
                .h(px(0.0))
                .max_h(px(0.0))
                .border_0()
                .overflow_hidden()
        }

        fn render_th(
            &mut self,
            _col_ix: usize,
            _window: &mut Window,
            _cx: &mut Context<'_, TableState<Self>>,
        ) -> impl IntoElement {
            div().h(px(0.0)).max_h(px(0.0)).overflow_hidden()
        }

        fn render_td(
            &mut self,
            row_ix: usize,
            col_ix: usize,
            _window: &mut Window,
            _cx: &mut Context<'_, TableState<Self>>,
        ) -> impl IntoElement {
            if self.columns[col_ix].0 == StakeTableColumnKind::Stake {
                div()
                    .debug_selector(|| "staking-stake-content".to_owned())
                    .h_full()
                    .flex()
                    .items_center()
                    .child(
                        Popover::new(SharedString::from(format!(
                            "staking-test-stake-popover-{row_ix}"
                        )))
                        .trigger(
                            Button::new(SharedString::from(format!(
                                "staking-test-stake-trigger-{row_ix}"
                            )))
                            .debug_selector(|| "staking-test-stake-trigger".to_owned())
                            .text()
                            .xsmall()
                            .child("#123…90"),
                        )
                        .content(|_state, _window, _cx| div().child("stake details")),
                    )
            } else if self.columns[col_ix].0 == StakeTableColumnKind::Actions {
                div()
                    .debug_selector(|| "staking-actions-content".to_owned())
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_1()
                    .child(
                        app_button_base(SharedString::from(format!(
                            "staking-test-undelegate-{row_ix}"
                        )))
                        .debug_selector(|| "staking-test-undelegate".to_owned())
                        .small()
                        .outline()
                        .child("Undelegate")
                        .on_click({
                            let action_clicked = self.action_clicked.clone();
                            move |_event, _window, _cx| {
                                if let Some(action_clicked) = &action_clicked {
                                    action_clicked.store(true, Ordering::SeqCst);
                                }
                            }
                        }),
                    )
                    .child(
                        app_button_base(SharedString::from(format!(
                            "staking-test-unlock-{row_ix}"
                        )))
                        .debug_selector(|| "staking-test-unlock".to_owned())
                        .small()
                        .outline()
                        .child(if self.compact {
                            "Unlock"
                        } else {
                            "Unlock · 2 steps"
                        }),
                    )
            } else if self.columns[col_ix].0 == StakeTableColumnKind::State {
                div()
                    .debug_selector(|| "staking-state-content".to_owned())
                    .h_full()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .debug_selector(|| "staking-state-label".to_owned())
                            .child(app_status_tag("Unlocking", theme::WARNING)),
                    )
            } else {
                div().size_full()
            }
        }
    }

    struct TableRenderProbe {
        table: Entity<TableState<TableProbeDelegate>>,
        table_height: gpui::Pixels,
        external_scrollbar: bool,
    }

    impl Render for TableRenderProbe {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            let vertical_scroll_handle = self.table.read(cx).vertical_scroll_handle.clone();
            let viewport = div()
                .debug_selector(|| "staking-visible-probe-viewport".to_owned())
                .w_full()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .child(
                    Table::new(&self.table)
                        .large()
                        .bordered(false)
                        .scrollbar_visible(false, false),
                );
            div()
                .relative()
                .w_full()
                .h(self.table_height)
                .min_w(px(0.0))
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .child(viewport)
                .when(self.external_scrollbar, |this| {
                    this.child(
                        div()
                            .occlude()
                            .debug_selector(|| "staking-test-external-scrollbar".to_owned())
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom_0()
                            .w(STAKING_TABLE_SCROLLBAR_WIDTH)
                            .child(Scrollbar::vertical(&vertical_scroll_handle)),
                    )
                })
        }
    }

    struct AccountHeaderProbe {
        compact: bool,
        width: gpui::Pixels,
        height: gpui::Pixels,
        prefix: &'static str,
    }

    impl Render for AccountHeaderProbe {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut Context<'_, Self>,
        ) -> impl IntoElement {
            let identity = div()
                .flex()
                .items_center()
                .child(app_strong_text("Imported account"));
            div()
                .debug_selector(|| "staking-account-header-card".to_owned())
                .w(self.width)
                .h(self.height)
                .p_3()
                .child(render_staking_account_header(
                    identity,
                    voting_power_metric("123456789012345678901234 RAIL", None),
                    self.compact,
                    Some(self.prefix),
                ))
        }
    }

    #[gpui::test]
    fn staking_table_actions_and_stake_disclosures_fit_at_compact_width(cx: &TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx
            .update(|app| {
                app.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(0.0), px(0.0)),
                            size: size(STAKING_TABLE_MIN_WIDTH, px(120.0)),
                        })),
                        ..Default::default()
                    },
                    |window, cx| {
                        let table = cx.new(|cx| {
                            TableState::new(TableProbeDelegate::new(), window, cx)
                                .sortable(false)
                                .row_selectable(false)
                                .col_selectable(false)
                                .col_movable(false)
                                .col_resizable(false)
                        });
                        cx.new(|_| TableRenderProbe {
                            table,
                            table_height: STAKING_OVERVIEW_ROW_HEIGHT,
                            external_scrollbar: false,
                        })
                    },
                )
            })
            .expect("open staking table probe window");
        let mut cx = VisualTestContext::from_window(*window, cx);
        cx.refresh().expect("refresh staking table probe window");
        cx.run_until_parked();

        let viewport_bounds = cx
            .debug_bounds("staking-visible-probe-viewport")
            .expect("staking visible probe viewport bounds");
        assert_eq!(viewport_bounds.size.height, STAKING_OVERVIEW_ROW_HEIGHT);
        assert!(cx.debug_bounds("staking-test-external-scrollbar").is_none());

        let assert_widths = |available_width: f32, expected: [gpui::Pixels; 5]| {
            let layout = staking_table_layout(px(available_width));
            assert_eq!(layout.columns.len(), expected.len());
            assert_eq!(layout.compact, available_width < 720.0);
            for ((_, column), expected) in layout.columns.iter().zip(expected) {
                assert_eq!(column.width, expected);
            }
        };
        assert_widths(480.0, [px(76.0), px(120.0), px(88.0), px(118.0), px(224.0)]);
        assert_widths(626.0, [px(76.0), px(120.0), px(88.0), px(118.0), px(224.0)]);
        assert_widths(
            719.0,
            [px(76.0), px(120.0), px(181.0), px(118.0), px(224.0)],
        );
        assert_widths(
            720.0,
            [px(76.0), px(120.0), px(118.0), px(118.0), px(288.0)],
        );

        let actions_bounds = cx
            .debug_bounds("staking-actions-content")
            .expect("staking Actions content bounds");
        for selector in ["staking-test-undelegate", "staking-test-unlock"] {
            let button_bounds = cx
                .debug_bounds(selector)
                .expect("staking action button bounds");
            // Stock Large Table centers 24px buttons in 40px content boxes; allow
            // 1px subpixel tolerance vertically.
            assert!(
                button_bounds.origin.x >= actions_bounds.origin.x
                    && button_bounds.origin.y >= actions_bounds.origin.y - px(1.0)
                    && button_bounds.origin.x + button_bounds.size.width
                        <= actions_bounds.origin.x + actions_bounds.size.width
                    && button_bounds.origin.y + button_bounds.size.height
                        <= actions_bounds.origin.y + actions_bounds.size.height + px(1.0),
                "{selector} bounds {button_bounds:?} should be contained by Actions content {actions_bounds:?}"
            );
        }
        let stake_bounds = cx
            .debug_bounds("staking-stake-content")
            .expect("staking Stake content bounds");
        let trigger_bounds = cx
            .debug_bounds("staking-test-stake-trigger")
            .expect("staking stake disclosure trigger bounds");
        let stake_center_y =
            f32::from(stake_bounds.origin.y) + f32::from(stake_bounds.size.height) / 2.0;
        let trigger_center_y =
            f32::from(trigger_bounds.origin.y) + f32::from(trigger_bounds.size.height) / 2.0;
        assert!(
            trigger_bounds.origin.x >= stake_bounds.origin.x
                && trigger_bounds.origin.x + trigger_bounds.size.width
                    <= stake_bounds.origin.x + stake_bounds.size.width
                && (trigger_center_y - stake_center_y).abs() <= 1.0,
            "stake trigger bounds {trigger_bounds:?} should be horizontally contained and vertically centered in Stake content {stake_bounds:?}"
        );
        let state_bounds = cx
            .debug_bounds("staking-state-content")
            .expect("staking State content bounds");
        let state_label_bounds = cx
            .debug_bounds("staking-state-label")
            .expect("staking state label bounds");
        assert!(
            state_label_bounds.origin.x >= state_bounds.origin.x
                && state_label_bounds.origin.y >= state_bounds.origin.y - px(1.0)
                && state_label_bounds.origin.x + state_label_bounds.size.width
                    <= state_bounds.origin.x + state_bounds.size.width
                && state_label_bounds.origin.y + state_label_bounds.size.height
                    <= state_bounds.origin.y + state_bounds.size.height + px(1.0),
            "state label bounds {state_label_bounds:?} should be contained by State content {state_bounds:?}"
        );
        for (selector, bounds) in [
            ("staking-stake-content", stake_bounds),
            ("staking-actions-content", actions_bounds),
            ("staking-state-content", state_bounds),
        ] {
            assert!(
                bounds.origin.y >= viewport_bounds.origin.y - px(1.0)
                    && bounds.origin.y + bounds.size.height
                        <= viewport_bounds.origin.y + viewport_bounds.size.height + px(1.0),
                "{selector} bounds {bounds:?} should be vertically contained by visible viewport {viewport_bounds:?}"
            );
        }
    }

    #[gpui::test]
    fn staking_table_pointer_clicks_do_not_select_rows_but_keyboard_does(cx: &TestAppContext) {
        cx.update(gpui_component::init);
        let action_clicked = Arc::new(AtomicBool::new(false));
        let callback = action_clicked.clone();
        let window = cx
            .update(|app| {
                app.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(0.0), px(0.0)),
                            size: size(STAKING_TABLE_MIN_WIDTH, px(120.0)),
                        })),
                        ..Default::default()
                    },
                    |window, cx| {
                        let table = cx.new(|cx| {
                            TableState::new(
                                TableProbeDelegate::with_action_callback(callback),
                                window,
                                cx,
                            )
                            .sortable(false)
                            .row_selectable(false)
                            .col_selectable(false)
                            .col_movable(false)
                            .col_resizable(false)
                        });
                        cx.new(|_| TableRenderProbe {
                            table,
                            table_height: STAKING_OVERVIEW_ROW_HEIGHT,
                            external_scrollbar: false,
                        })
                    },
                )
            })
            .expect("open staking table pointer probe window");
        let mut cx = VisualTestContext::from_window(*window, cx);
        cx.refresh()
            .expect("refresh staking table pointer probe window");
        cx.run_until_parked();

        let probe = window.root(&mut cx).expect("staking pointer probe root");
        let table = cx.update(|_, app| probe.read(app).table.clone());
        let row_bounds = cx
            .debug_bounds("staking-test-row")
            .expect("staking first row bounds");
        let blank_position = point(row_bounds.origin.x + px(2.0), row_bounds.center().y);
        cx.simulate_click(blank_position, gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(cx.update(|_, app| table.read(app).selected_row()), None);

        let action_bounds = cx
            .debug_bounds("staking-test-undelegate")
            .expect("staking action button bounds");
        cx.simulate_click(action_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(action_clicked.load(Ordering::SeqCst));
        assert_eq!(cx.update(|_, app| table.read(app).selected_row()), None);

        cx.update_window(*window, |_, window, app| {
            window.focus(&table.read(app).focus_handle(app));
        })
        .expect("focus staking table");
        cx.simulate_keystrokes("down");
        assert_eq!(cx.update(|_, app| table.read(app).selected_row()), Some(0));
    }

    #[gpui::test]
    fn staking_table_external_scrollbar_tracks_full_height_and_clamps_bottom(cx: &TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx
            .update(|app| {
                app.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(0.0), px(0.0)),
                            size: size(STAKING_TABLE_MIN_WIDTH, px(200.0)),
                        })),
                        ..Default::default()
                    },
                    |window, cx| {
                        let table = cx.new(|cx| {
                            TableState::new(TableProbeDelegate::with_rows(6), window, cx)
                                .sortable(false)
                                .row_selectable(false)
                                .col_selectable(false)
                                .col_movable(false)
                                .col_resizable(false)
                        });
                        cx.new(|_| TableRenderProbe {
                            table,
                            table_height: px(200.0),
                            external_scrollbar: true,
                        })
                    },
                )
            })
            .expect("open staking external scrollbar probe window");
        let mut cx = VisualTestContext::from_window(*window, cx);
        cx.refresh()
            .expect("refresh staking external scrollbar probe window");
        cx.run_until_parked();

        let probe = window
            .root(&mut cx)
            .expect("staking external scrollbar probe root");
        let table = cx.update(|_, app| probe.read(app).table.clone());
        let vertical_scroll_handle =
            cx.update(|_, app| table.read(app).vertical_scroll_handle.clone());
        let scroll_handle = vertical_scroll_handle.0.borrow().base_handle.clone();
        let viewport_bounds = cx
            .debug_bounds("staking-visible-probe-viewport")
            .expect("staking visible probe viewport bounds");
        let scrollbar_bounds = cx
            .debug_bounds("staking-test-external-scrollbar")
            .expect("staking external scrollbar bounds");
        assert_eq!(scrollbar_bounds.size.width, STAKING_TABLE_SCROLLBAR_WIDTH);
        assert_eq!(scrollbar_bounds.size.height, px(200.0));
        assert_eq!(scrollbar_bounds.origin.y, viewport_bounds.origin.y);
        assert_eq!(
            scrollbar_bounds.origin.y + scrollbar_bounds.size.height,
            viewport_bounds.origin.y + viewport_bounds.size.height
        );
        assert_eq!(
            ScrollbarHandle::content_size(&vertical_scroll_handle).height,
            px(240.0)
        );
        assert_eq!(scroll_handle.max_offset().height, px(40.0));

        ScrollbarHandle::set_offset(&vertical_scroll_handle, point(px(0.0), px(-40.0)));
        cx.update(|_, app| probe.update(app, |_, cx| cx.notify()));
        cx.refresh()
            .expect("refresh staking table at external scrollbar bottom");
        cx.run_until_parked();
        assert_eq!(scroll_handle.offset().y, px(-40.0));
        let row_six_bounds = cx
            .debug_bounds("staking-test-row-six")
            .expect("staking sixth row bounds at bottom");
        assert_eq!(row_six_bounds.size.height, STAKING_OVERVIEW_ROW_HEIGHT);
        assert!(row_six_bounds.origin.y >= viewport_bounds.origin.y - px(1.0));
        assert!(
            row_six_bounds.origin.y + row_six_bounds.size.height
                <= viewport_bounds.origin.y + viewport_bounds.size.height + px(1.0)
        );
        assert!(
            (row_six_bounds.origin.y + row_six_bounds.size.height)
                >= viewport_bounds.origin.y + viewport_bounds.size.height - px(1.0)
        );

        ScrollbarHandle::set_offset(&vertical_scroll_handle, point(px(0.0), px(-40.0)));
        cx.update(|_, app| probe.update(app, |_, cx| cx.notify()));
        cx.refresh()
            .expect("refresh staking table at repeated external scrollbar bottom");
        cx.run_until_parked();
        assert_eq!(scroll_handle.offset().y, px(-40.0));
    }

    #[gpui::test]
    fn staking_account_header_handles_compact_and_wide_metrics(cx: &TestAppContext) {
        cx.update(gpui_component::init);
        for (compact, width, height, prefix) in [
            (true, px(514.0), px(260.0), "staking-account-header"),
            (false, px(960.0), px(180.0), "staking-mid-account-header"),
        ] {
            if !compact {
                assert!(!staking_account_header_compact(width, false));
            }
            let window = cx
                .update(|app| {
                    app.open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(Bounds {
                                origin: point(px(0.0), px(0.0)),
                                size: size(width, height),
                            })),
                            ..Default::default()
                        },
                        |_window, cx| {
                            cx.new(|_| AccountHeaderProbe {
                                compact,
                                width,
                                height,
                                prefix,
                            })
                        },
                    )
                })
                .expect("open staking account header probe window");
            let mut cx = VisualTestContext::from_window(*window, cx);
            cx.refresh()
                .expect("refresh staking account header probe window");
            cx.run_until_parked();

            let card = cx
                .debug_bounds("staking-account-header-card")
                .expect("staking account header card bounds");
            let (identity_selector, metric_selector) = if compact {
                (
                    "staking-account-header-identity",
                    "staking-account-header-metric-0",
                )
            } else {
                (
                    "staking-mid-account-header-identity",
                    "staking-mid-account-header-metric-0",
                )
            };
            let identity = cx
                .debug_bounds(identity_selector)
                .expect("staking account identity bounds");
            let metric = cx
                .debug_bounds(metric_selector)
                .expect("voting power metric bounds");
            if compact {
                assert!(identity.origin.y + identity.size.height <= metric.origin.y + px(1.0));
                assert!(metric.origin.x >= card.origin.x);
                assert!(metric.origin.y >= card.origin.y);
                assert!(metric.origin.x + metric.size.width <= card.origin.x + card.size.width);
                assert!(metric.origin.y + metric.size.height <= card.origin.y + card.size.height);
                assert!(identity.origin.x >= card.origin.x);
                assert!(identity.origin.y >= card.origin.y);
                assert!(identity.origin.x + identity.size.width <= card.origin.x + card.size.width);
                assert!(
                    identity.origin.y + identity.size.height <= card.origin.y + card.size.height
                );
            } else {
                let card_padding = px(12.0);
                assert!(metric.origin.x >= card.origin.x + card_padding);
                assert!(
                    metric.origin.x + metric.size.width
                        <= card.origin.x + card.size.width - card_padding
                );
                assert!(identity.origin.x >= card.origin.x + card_padding);
                assert!(identity.origin.x + identity.size.width <= metric.origin.x);
            }
        }
    }

    fn account(uuid: &str, status: PublicAccountStatus, global: bool) -> PublicAccountMetadata {
        PublicAccountMetadata {
            public_account_uuid: uuid.to_owned(),
            address: Address::from([uuid.as_bytes()[0]; 20]),
            label: Some(uuid.to_owned()),
            source: wallet_ops::vault::PublicAccountSource::Imported,
            scope: if global {
                wallet_ops::vault::PublicAccountScope::Global
            } else {
                wallet_ops::vault::PublicAccountScope::PrivateWallet {
                    wallet_uuid: "wallet".to_owned(),
                }
            },
            derivation_index: None,
            hardware_descriptor: None,
            status,
            display_order: 0,
        }
    }

    fn key(wallet: &str, chain: u64, uuid: &str, address: Address) -> GovernanceContextKey {
        GovernanceContextKey {
            wallet_id: Some(wallet.to_owned()),
            chain_id: chain,
            participants: vec![GovernanceParticipantIdentity {
                uuid: uuid.to_owned(),
                address,
            }],
        }
    }

    fn positive_reward(token: Address, amount: U256) -> RewardView {
        RewardView::Positive {
            amount,
            evidence: Box::new(RewardEvidence {
                token,
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                staking_intervals: vec![U256::ZERO],
                hints: vec![U256::ZERO],
                claimed_intervals: Vec::new(),
                amount,
            }),
        }
    }

    #[test]
    fn priced_positive_reward_has_formatted_usd_value() {
        let token = Address::from([1; 20]);
        let cache = TokenAnchorRateCache::new();
        cache.store_rate(1, token, U256::from(10).pow(U256::from(18)));
        cache.store_native_usd_rate(1, U256::from(2_000_000));

        assert_eq!(
            reward_usd_label(
                1,
                token,
                Some(&positive_reward(
                    token,
                    U256::from(3) * U256::from(10).pow(U256::from(18)),
                )),
                &cache,
            ),
            Some("$6.00".to_owned())
        );
    }

    #[test]
    fn resolved_reward_total_adds_rows_and_zero_needs_no_price() {
        let priced_token = Address::from([1; 20]);
        let zero_token = Address::from([2; 20]);
        let cache = TokenAnchorRateCache::new();
        cache.store_rate(1, priced_token, U256::from(10).pow(U256::from(18)));
        cache.store_native_usd_rate(1, U256::from(2_000_000));
        let rewards = BTreeMap::from([
            (
                (String::from("account"), priced_token),
                positive_reward(
                    priced_token,
                    U256::from(3) * U256::from(10).pow(U256::from(18)),
                ),
            ),
            ((String::from("account"), zero_token), RewardView::Zero),
        ]);
        let resolved_reward_keys = rewards.keys().cloned().collect();

        assert_eq!(
            reward_usd_total(
                1,
                "account",
                &[priced_token, zero_token],
                &rewards,
                &resolved_reward_keys,
                &cache,
            ),
            RewardUsdState::Value(U256::from(6_000_000))
        );
        assert_eq!(
            reward_usd_label(
                1,
                zero_token,
                rewards.get(&(String::from("account"), zero_token)),
                &cache,
            ),
            Some("$0.00".to_owned())
        );
    }

    #[test]
    fn reward_total_fails_closed_for_unresolved_or_overflowed_rows() {
        let first = Address::from([1; 20]);
        let second = Address::from([2; 20]);
        let unpriced = Address::from([3; 20]);
        let cache = TokenAnchorRateCache::new();
        cache.store_rate(1, first, U256::ONE);
        cache.store_rate(1, second, U256::ONE);
        cache.store_native_usd_rate(1, U256::ONE);
        let unpriced_reward = positive_reward(unpriced, U256::ONE);
        assert_eq!(
            reward_usd_label(1, unpriced, Some(&unpriced_reward), &cache),
            Some("USD unavailable".to_owned())
        );

        let cases = [
            (BTreeMap::new(), &[first][..], RewardUsdState::Loading),
            (
                BTreeMap::from([(
                    (String::from("account"), first),
                    RewardView::Unavailable("failed".into()),
                )]),
                &[first][..],
                RewardUsdState::Unavailable,
            ),
            (
                BTreeMap::from([(
                    (String::from("account"), unpriced),
                    positive_reward(unpriced, U256::ONE),
                )]),
                &[unpriced][..],
                RewardUsdState::Unavailable,
            ),
        ];
        for (rewards, tokens, expected) in cases {
            let resolved_reward_keys = rewards.keys().cloned().collect();
            assert_eq!(
                reward_usd_total(
                    1,
                    "account",
                    tokens,
                    &rewards,
                    &resolved_reward_keys,
                    &cache,
                ),
                expected
            );
        }

        let rewards = BTreeMap::from([
            (
                (String::from("account"), first),
                positive_reward(first, U256::MAX),
            ),
            (
                (String::from("account"), second),
                positive_reward(second, U256::MAX),
            ),
        ]);
        assert_eq!(
            reward_usd_total(
                1,
                "account",
                &[first, second],
                &rewards,
                &rewards.keys().cloned().collect(),
                &cache,
            ),
            RewardUsdState::Unavailable
        );
    }

    #[test]
    fn staking_formatter_uses_deployment_symbols_and_effective_override() {
        let amount = U256::from(15) * U256::from(10).pow(U256::from(17));
        let empty_registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        for (chain_id, symbol) in [(1, "RAIL"), (56, "RAILBSC"), (137, "RAILPOLY")] {
            let token = governance_contracts(chain_id)
                .expect("supported governance deployment")
                .governance_token;
            assert_eq!(
                format_staking_amount(chain_id, token, amount, &empty_registry),
                format!("1.5 {symbol}")
            );
        }

        let token = governance_contracts(1)
            .expect("supported governance deployment")
            .governance_token;
        let override_registry = wallet_ops::settings::EffectiveTokenRegistry {
            tokens: BTreeMap::from([(
                (1, token.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: token.to_string(),
                    symbol: "CUSTOM".to_owned(),
                    decimals: 18,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            )]),
        };
        assert_eq!(
            format_staking_amount(1, token, amount, &override_registry),
            "1.5 CUSTOM"
        );
    }

    #[test]
    fn staking_account_projection_tracks_received_voting_power() {
        let owner = Address::from([1; 20]);
        let external = Address::from([2; 20]);
        let stakes = vec![
            StakePosition {
                owner,
                id: U256::ZERO,
                delegate: owner,
                amount: U256::from(10),
                staketime: U256::ZERO,
                locktime: U256::ZERO,
                claimed_time: U256::ZERO,
                state: StakeState::Active,
            },
            StakePosition {
                owner,
                id: U256::from(1),
                delegate: external,
                amount: U256::from(5),
                staketime: U256::ZERO,
                locktime: U256::ZERO,
                claimed_time: U256::ZERO,
                state: StakeState::Unlocking,
            },
            StakePosition {
                owner,
                id: U256::from(2),
                delegate: external,
                amount: U256::from(7),
                staketime: U256::ZERO,
                locktime: U256::ZERO,
                claimed_time: U256::ZERO,
                state: StakeState::Claimable,
            },
            StakePosition {
                owner,
                id: U256::from(3),
                delegate: external,
                amount: U256::from(11),
                staketime: U256::ZERO,
                locktime: U256::ZERO,
                claimed_time: U256::from(1),
                state: StakeState::Claimed,
            },
        ];

        let projection = staking_account_projection(&stakes);
        assert_eq!(projection.unclaimed_principal, U256::from(22));
        assert_eq!(projection.active_self_delegated, U256::from(10));
        assert_eq!(projection.active_external_delegated, U256::ZERO);
        assert_eq!(
            projection.received_voting_power(U256::from(30)),
            U256::from(20)
        );
    }

    #[test]
    fn stale_generation_and_key_results_are_rejected() {
        let address = Address::from([1; 20]);
        let mut state = StakingReadState::default();
        let first = key("wallet", 1, "a", address);
        let generation = state.begin(first.clone());
        let second = key("wallet", 1, "b", address);
        let next_generation = state.begin(second.clone());
        assert!(!state.apply_account(&first, generation, "a".into(), Err(Arc::from("stale"))));
        assert!(state.apply_account(
            &second,
            next_generation,
            "b".into(),
            Err(Arc::from("account"))
        ));
        assert!(!state.accounts.contains_key("a"));
    }

    #[test]
    fn reward_evidence_cache_requires_current_identity_tokens_and_ttl() {
        let address = Address::from([1; 20]);
        let token = Address::from([2; 20]);
        let token_two = Address::from([3; 20]);
        let key = key("wallet", 1, "a", address);
        let mut state = StakingReadState::default();
        let generation = state.begin(key.clone());
        assert!(state.apply_global(
            &key,
            generation,
            Ok(StakingGlobalMetrics {
                total_staked: U256::ZERO,
                total_voting_power: U256::ZERO,
                deploy_time: U256::ZERO,
                snapshot_interval: U256::ONE,
                current_interval: U256::ZERO,
                stake_locktime: U256::ZERO,
                chain_time: U256::ZERO,
            })
        ));
        assert!(state.apply_reward(
            &key,
            generation,
            "a".into(),
            token,
            Ok(Some(RewardEvidence {
                token,
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                staking_intervals: vec![U256::ZERO],
                hints: vec![U256::ZERO],
                claimed_intervals: Vec::new(),
                amount: U256::ONE,
            }))
        ));
        assert!(state.apply_bulk_reward(
            &key,
            generation,
            "a".into(),
            vec![token, token_two],
            &Ok(Some(wallet_ops::RewardBatchEvidence {
                reward_tokens: vec![token, token_two],
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                staking_intervals: vec![U256::ZERO],
                hints: vec![U256::ZERO],
                claimed_intervals: vec![Vec::new(), Vec::new()],
                expected_amounts: vec![U256::ONE, U256::ZERO],
            })),
        ));
        let now = Instant::now();
        assert!(
            state
                .cached_reward_evidence_at(&key, "a", address, &[token], now)
                .is_some()
        );
        assert!(
            state
                .cached_reward_evidence_at(&key, "a", address, &[token, token_two], now)
                .is_some()
        );
        assert!(
            state
                .cached_reward_evidence_at(&key, "missing", address, &[token], now)
                .is_none()
        );
        assert!(
            state
                .cached_reward_evidence_at(&key, "a", address, &[token_two, token], now)
                .is_none()
        );
        assert!(
            state
                .cached_reward_evidence_at(
                    &key,
                    "a",
                    address,
                    &[token],
                    now + STAKING_REWARD_EVIDENCE_TTL + Duration::from_secs(1),
                )
                .is_none()
        );
        state.begin(key.clone());
        assert!(
            state
                .cached_reward_evidence_at(&key, "a", address, &[token], Instant::now())
                .is_none()
        );
    }

    #[test]
    fn scoped_successes_survive_account_and_asset_failures() {
        let address = Address::from([1; 20]);
        let token = Address::from([2; 20]);
        let token_two = Address::from([3; 20]);
        let mut state = StakingReadState::default();
        let key = GovernanceContextKey {
            wallet_id: Some(String::from("wallet")),
            chain_id: 1,
            participants: vec![
                GovernanceParticipantIdentity {
                    uuid: String::from("a"),
                    address,
                },
                GovernanceParticipantIdentity {
                    uuid: String::from("good"),
                    address,
                },
                GovernanceParticipantIdentity {
                    uuid: String::from("pending"),
                    address: Address::from([8; 20]),
                },
            ],
        };
        let generation = state.begin(key.clone());
        let metrics = StakingGlobalMetrics {
            total_staked: U256::from(3),
            total_voting_power: U256::from(2),
            deploy_time: U256::ZERO,
            snapshot_interval: U256::from(1),
            current_interval: U256::ZERO,
            stake_locktime: U256::ZERO,
            chain_time: U256::ZERO,
        };
        assert!(state.apply_global(&key, generation, Ok(metrics.clone())));
        assert!(state.apply_account(
            &key,
            generation,
            "a".into(),
            Err(Arc::from("account failed"))
        ));
        assert!(state.apply_reward(
            &key,
            generation,
            "a".into(),
            token,
            Ok(Some(RewardEvidence {
                token,
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                staking_intervals: vec![U256::ZERO],
                hints: vec![U256::ZERO],
                claimed_intervals: Vec::new(),
                amount: U256::from(1),
            }))
        ));
        assert!(state.apply_account(
            &key,
            generation,
            "good".into(),
            Ok(AccountStakeResult {
                account: address,
                voting_power: Ok(U256::from(4)),
                balance: Ok(U256::from(5)),
                stakes: Ok(Vec::new()),
            })
        ));
        assert!(state.apply_reward(
            &key,
            generation,
            "good".into(),
            token,
            Ok(Some(RewardEvidence {
                token,
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                staking_intervals: vec![U256::ZERO],
                hints: vec![U256::ZERO],
                claimed_intervals: Vec::new(),
                amount: U256::from(1),
            }))
        ));
        assert!(state.apply_reward(&key, generation, "good".into(), token_two, Ok(None),));
        assert!(state.apply_bulk_reward(
            &key,
            generation,
            "a".into(),
            vec![token, token_two],
            &Err(Arc::from("bulk asset failed")),
        ));
        assert!(state.apply_bulk_reward(
            &key,
            generation,
            "good".into(),
            vec![token, token_two],
            &Ok(Some(wallet_ops::RewardBatchEvidence {
                reward_tokens: vec![token, token_two],
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                staking_intervals: vec![U256::ZERO],
                hints: vec![U256::ZERO],
                claimed_intervals: vec![Vec::new(), Vec::new()],
                expected_amounts: vec![U256::from(1), U256::ZERO],
            })),
        ));
        assert_eq!(state.metrics.as_ref(), Some(&metrics));
        assert!(state.accounts["a"].stakes.is_err());
        assert!(matches!(
            state.rewards[&(String::from("a"), token)],
            RewardView::Positive { .. }
        ));
        assert!(matches!(
            state.rewards[&(String::from("good"), token)],
            RewardView::Positive { .. }
        ));
        assert_eq!(state.status, StakingRefreshStatus::Loading);
        assert!(state.global_action_ready());
        assert!(state.account_action_ready("good"));
        assert!(!state.account_action_ready("a"));
        assert!(!state.account_action_ready("pending"));
        assert!(state.reward_action_ready("good", token));
        assert!(state.reward_claim_all_ready("good", &[token, token_two]));
        assert!(state.reward_action_ready("a", token));
        assert!(!state.reward_claim_all_ready("a", &[token, token_two]));
        assert!(!state.reward_action_ready("pending", token));
        assert!(!state.reward_claim_all_ready("pending", &[token, token_two]));
        assert!(state.action_selection_ready(&StakingActionSelection {
            actor_uuid: String::from("good"),
            actor: address,
            kind: StakingActionKind::RewardClaim { token },
        }));
        assert!(!state.action_selection_ready(&StakingActionSelection {
            actor_uuid: String::from("a"),
            actor: address,
            kind: StakingActionKind::Delegate {
                stake_id: U256::ZERO,
            },
        }));
        assert!(state.apply_reward(
            &key,
            generation,
            "good".into(),
            token_two,
            Err(Arc::from("second asset failed")),
        ));
        assert!(state.reward_claim_all_ready("good", &[token, token_two]));
        assert!(!state.reward_claim_all_ready("good", &[token_two, token]));
        assert!(!state.action_selection_ready(&StakingActionSelection {
            actor_uuid: String::from("good"),
            actor: Address::from([9; 20]),
            kind: StakingActionKind::RewardClaim { token },
        }));

        let next_generation = state.begin(key.clone());
        assert_ne!(generation, next_generation);
        assert_eq!(state.metrics.as_ref(), Some(&metrics));
        assert!(state.accounts.contains_key("good"));
        assert!(state.rewards.contains_key(&(String::from("good"), token)));
        assert!(!state.global_action_ready());
        assert!(!state.account_action_ready("good"));
        assert!(!state.reward_action_ready("good", token));
        assert!(!state.apply_bulk_reward(
            &key,
            generation,
            "good".into(),
            vec![token, token_two],
            &Ok(Some(wallet_ops::RewardBatchEvidence {
                reward_tokens: vec![token, token_two],
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                staking_intervals: vec![U256::ZERO],
                hints: vec![U256::ZERO],
                claimed_intervals: vec![Vec::new(), Vec::new()],
                expected_amounts: vec![U256::from(1), U256::ZERO],
            })),
        ));
        assert!(!state.reward_claim_all_ready("good", &[token, token_two]));

        assert!(state.apply_global(
            &key,
            next_generation,
            Err(Arc::from("global refresh failed"))
        ));
        assert_eq!(state.status, StakingRefreshStatus::Stale);
        assert!(!state.global_action_ready());
    }

    #[test]
    fn empty_participants_finish_fresh_after_global_metrics() {
        let mut state = StakingReadState::default();
        let key = GovernanceContextKey {
            wallet_id: Some(String::from("wallet")),
            chain_id: 1,
            participants: Vec::new(),
        };
        let generation = state.begin(key.clone());
        let metrics = StakingGlobalMetrics {
            total_staked: U256::from(3),
            total_voting_power: U256::from(2),
            deploy_time: U256::ZERO,
            snapshot_interval: U256::from(1),
            current_interval: U256::ZERO,
            stake_locktime: U256::ZERO,
            chain_time: U256::from(10),
        };
        assert!(state.apply_global(&key, generation, Ok(metrics.clone())));
        assert!(state.finish(&key, generation));
        assert_eq!(state.status, StakingRefreshStatus::Fresh);
        assert_eq!(state.metrics, Some(metrics));
        assert!(state.accounts.is_empty());
        assert!(state.rewards.is_empty());
    }

    #[test]
    fn participant_states_cover_scopes_inactive_search_and_empty_results() {
        let accounts = vec![
            account("a", PublicAccountStatus::Active, false),
            account("b", PublicAccountStatus::Inactive, false),
            account("g", PublicAccountStatus::Active, true),
            account("i", PublicAccountStatus::Inactive, true),
        ];
        let selected = BTreeSet::from([String::from("b"), String::from("g")]);
        let summary = participant_summary(&accounts, &selected, Some("wallet"));
        assert_eq!(
            summary,
            ParticipantSummary {
                selected: 2,
                inactive: 1
            }
        );
        let choices = participant_choices(&accounts, &selected, Some("wallet"), "");
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "g"]
        );
        assert!(!choices.iter().any(|choice| choice.uuid == "i"));
        assert!(participant_choices(&accounts, &selected, Some("wallet"), "missing").is_empty());
        assert_eq!(
            participant_choices(&accounts, &BTreeSet::new(), Some("wallet"), "alice").len(),
            0
        );
        assert_eq!(
            participant_choices(&accounts, &selected, Some("other"), "")
                .iter()
                .map(|choice| choice.uuid.as_str())
                .collect::<Vec<_>>(),
            vec!["g"]
        );
    }

    #[test]
    fn default_tab_and_context_invalidation_are_explicit() {
        assert_eq!(GovernanceTab::default(), GovernanceTab::Proposals);
        let mut state = StakingReadState::default();
        let key = key("wallet", 1, "a", Address::from([1; 20]));
        let generation = state.begin(key.clone());
        state.invalidate();
        assert!(!state.finish(&key, generation));
        assert_eq!(state.status, StakingRefreshStatus::Idle);
    }

    #[test]
    fn staking_control_ids_bind_actor_subject_and_action() {
        let first = staking_control_id("actor-a", "reward-claim", "token");
        let second = staking_control_id("actor-b", "reward-claim", "token");
        let different_action = staking_control_id("actor-a", "delegate", "token");

        assert_ne!(first, second);
        assert_ne!(first, different_action);
    }

    fn reward_metadata(
        staking_deploy_time: U256,
        distribution_interval: U256,
    ) -> GovernorRewardsIntervalMetadata {
        GovernorRewardsIntervalMetadata {
            multiplier: U256::ONE,
            staking_deploy_time,
            distribution_interval,
            current_interval: U256::ZERO,
            next_earmark_intervals: BTreeMap::new(),
        }
    }

    #[test]
    fn reward_interval_countdown_uses_fourteen_day_boundaries() {
        let fourteen_days = U256::from(14 * 24 * 60 * 60);
        let metadata = reward_metadata(U256::from(100), fourteen_days);
        let countdown = reward_interval_countdown(
            &metadata,
            U256::from(100) + fourteen_days * U256::from(3) + U256::from(42),
        )
        .expect("deployed reward interval");

        assert_eq!(countdown.interval, U256::from(3));
        assert_eq!(
            countdown.boundary,
            U256::from(100) + fourteen_days * U256::from(4)
        );
        assert_eq!(
            remaining_reward_interval_seconds(
                countdown,
                U256::from(100) + fourteen_days * U256::from(3) + U256::from(42),
            ),
            Some(fourteen_days.to::<u64>() - 42)
        );
    }

    #[test]
    fn reward_interval_countdown_rejects_zero_interval_and_predeployment_time() {
        let zero_interval = reward_metadata(U256::from(100), U256::ZERO);
        assert!(reward_interval_countdown(&zero_interval, U256::from(100)).is_none());

        let metadata = reward_metadata(U256::from(100), U256::from(14));
        assert!(reward_interval_countdown(&metadata, U256::from(99)).is_none());
    }

    #[test]
    fn reward_interval_countdown_rejects_checked_arithmetic_overflow() {
        let increment_overflow = reward_metadata(U256::ZERO, U256::ONE);
        assert!(reward_interval_countdown(&increment_overflow, U256::MAX).is_none());

        let multiplication_overflow = reward_metadata(U256::ZERO, U256::MAX);
        assert!(reward_interval_countdown(&multiplication_overflow, U256::MAX).is_none());

        let addition_overflow = reward_metadata(U256::MAX, U256::ONE);
        assert!(reward_interval_countdown(&addition_overflow, U256::MAX).is_none());
    }

    #[test]
    fn governance_boundary_crossing_uses_the_stored_reward_boundary() {
        let metadata = reward_metadata(U256::from(10), U256::from(5));
        let countdown =
            reward_interval_countdown(&metadata, U256::from(20)).expect("deployed reward interval");
        let state = StakingReadState {
            reward_interval_countdown: Some(countdown),
            ..StakingReadState::default()
        };

        assert!(!governance_boundary_crossed(&state, U256::from(24)));
        assert!(governance_boundary_crossed(&state, U256::from(25)));
    }

    #[test]
    fn reward_interval_state_is_generation_scoped_and_cleared_on_refresh() {
        let address = Address::from([1; 20]);
        let key = key("wallet", 1, "a", address);
        let mut state = StakingReadState::default();
        let generation = state.begin(key.clone());
        let countdown = RewardIntervalCountdown {
            interval: U256::ZERO,
            boundary: U256::ONE,
        };
        assert!(state.apply_reward_interval(&key, generation, Some(countdown)));

        let next_generation = state.begin(key.clone());
        assert_ne!(generation, next_generation);
        assert_eq!(state.reward_interval_countdown, None);
        assert!(!state.apply_reward_interval(&key, generation, Some(countdown)));
        assert!(state.apply_reward_interval(&key, next_generation, Some(countdown)));
        state.invalidate();
        assert_eq!(state.reward_interval_countdown, None);
    }

    #[test]
    fn governance_time_tick_new_generation_supersedes_previous_owner() {
        let (owner, armed) = arm_governance_time_tick(None, 10);
        assert_eq!(owner, Some(10));
        assert!(armed);

        let (owner, armed) = arm_governance_time_tick(owner, 10);
        assert_eq!(owner, Some(10));
        assert!(!armed);

        let (owner, armed) = arm_governance_time_tick(owner, 11);
        assert_eq!(owner, Some(11));
        assert!(armed);
    }

    #[test]
    fn stale_governance_time_tick_claim_leaves_new_owner_intact() {
        let (owner, claimed) = claim_governance_time_tick(Some(11), 10);
        assert_eq!(owner, Some(11));
        assert!(!claimed);
    }

    #[test]
    fn governance_time_tick_owner_claims_only_once() {
        let (owner, claimed) = claim_governance_time_tick(Some(11), 11);
        assert_eq!(owner, None);
        assert!(claimed);

        let (owner, claimed) = claim_governance_time_tick(owner, 11);
        assert_eq!(owner, None);
        assert!(!claimed);
    }

    #[test]
    fn non_crossed_governance_time_tick_claim_can_rearm_generation() {
        let (owner, claimed) = claim_governance_time_tick(Some(11), 11);
        assert!(claimed);
        assert_eq!(owner, None);

        let (owner, armed) = arm_governance_time_tick(owner, 11);
        assert_eq!(owner, Some(11));
        assert!(armed);
    }

    #[test]
    fn unlock_boundary_is_checked_at_equality_and_ignores_terminal_history() {
        let owner = Address::from([4; 20]);
        let mut state = StakingReadState::default();
        state.accounts.insert(
            "a".into(),
            StakingAccountView {
                voting_power: Ok(U256::ZERO),
                balance: Ok(U256::ZERO),
                stakes: Ok(vec![
                    StakePosition {
                        owner,
                        id: U256::ZERO,
                        delegate: owner,
                        amount: U256::from(1),
                        staketime: U256::ZERO,
                        locktime: U256::from(20),
                        claimed_time: U256::ZERO,
                        state: StakeState::Unlocking,
                    },
                    StakePosition {
                        owner,
                        id: U256::from(1),
                        delegate: owner,
                        amount: U256::from(1),
                        staketime: U256::ZERO,
                        locktime: U256::from(10),
                        claimed_time: U256::from(11),
                        state: StakeState::Claimed,
                    },
                ]),
            },
        );
        assert_eq!(nearest_unlock_boundary(&state), Some(U256::from(20)));
        assert!(!governance_boundary_crossed(&state, U256::from(19)));
        assert!(governance_boundary_crossed(&state, U256::from(20)));
        state.invalidate();
        assert!(!governance_boundary_crossed(&state, U256::from(20)));
    }
}
