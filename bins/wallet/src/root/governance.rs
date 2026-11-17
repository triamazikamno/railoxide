use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256};
use futures_util::StreamExt;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable, FontWeight, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, WindowExt,
    alert::Alert,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    combobox::{Combobox, ComboboxEvent, ComboboxState},
    description_list::{DescriptionItem, DescriptionList},
    input::InputState,
    popover::Popover,
    scroll::{ScrollableElement, Scrollbar},
    searchable_list::{SearchableListItem, SearchableVec},
    spinner::Spinner,
    tab::{Tab, TabBar},
    table::{Column, DataTable, TableDelegate, TableState},
    tooltip::Tooltip,
};
use railgun_ui::{format_usd_micro_value, governance_contracts, short_address};
use rand::seq::SliceRandom as _;
use ui::clipboard::clipboard_with_toast;
use ui::controls::{
    app_button, app_button_base, app_button_label, app_input, app_muted_shimmer_text,
    app_muted_text, app_shimmer_text, app_strong_text, app_text,
};
use ui::dialog::WalletDialogExt as _;
use ui::format::format_compact_duration;
use ui::theme::{self, APP_MONO_FONT_FAMILY};
use wallet_ops::{
    AccountStakeResult, GovernorRewardsIntervalMetadata, RewardEvidence, StakePosition, StakeState,
    StakingGlobalMetrics, TokenAnchorRateCache, fetch_account_snapshots, fetch_account_stakes,
    fetch_interval_metadata, fetch_reward_batch_evidence, fetch_reward_evidence,
    fetch_reward_evidence_multi, fetch_staking_global_metrics, validate_governance_deployment,
    vault::{PublicAccountMetadata, PublicAccountStatus},
};
use wallet_ops::{
    GovernanceActionContext, GovernanceActionIntent, GovernanceContractKind,
    PublicActionGasFeeSelection, PublicAdvancedTransactionEstimateRequest,
    fetch_governance_token_balance_allowance, plan_delegate, plan_principal_claim, plan_stake,
    plan_undelegate, plan_unlock,
};

use super::gas_fee::{
    Eip1559GasFeeEditTarget, Eip1559GasFeeEditorState, Eip1559GasFeeMode, Eip1559GasFeeTarget,
    render_eip1559_gas_fee_editor,
};
use super::governance_action::{
    GovernanceContinuation, GovernanceDraftRecipe, GovernanceRefreshTarget, GovernanceSpendDraft,
    GovernanceStakingReviewProjection, ProposalActionKind, ProposalActionSelection,
    ProposalParticipationKey, ProposalParticipationState, build_typed_governance_spend_draft,
};
use super::participant::normalize_participant_ids;
use super::proposals::{format_compact_rail_amount, format_date_short};
use super::public_account::public_account_display_label;
use super::public_balances::isolated_read_jitter;
use super::tokens::{
    format_send_amount_input, format_token_amount_for_display, token_display_metadata,
};
use super::ui_helpers::token_label_row;
use super::{WalletRoot, app_refresh_button, app_status_tag};
use crate::assets::{PIGGY_BANK_ICON_PATH, RailgunSidebarIcon, USERS_ICON_PATH, WalletIconSource};

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
/// Rapid clicks in the claim selection form cost one estimate, not one per click.
const REWARD_SELECTION_ESTIMATE_DEBOUNCE: Duration = Duration::from_millis(300);
const REWARD_SELECTION_DIALOG_WIDTH: gpui::Pixels = px(520.0);
const STAKING_ACTION_DIALOG_WIDTH: gpui::Pixels = px(440.0);
const REWARD_SELECTION_CHECKBOX_WIDTH: gpui::Pixels = px(20.0);
const REWARD_SELECTION_ASSET_WIDTH: gpui::Pixels = px(100.0);
const REWARD_SELECTION_FEE_WIDTH: gpui::Pixels = px(92.0);
const REWARD_SELECTION_NET_WIDTH: gpui::Pixels = px(80.0);

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

/// The shared route for one traffic class of `chain_id`, as used by dashboard General reads.
pub(super) fn governance_route(
    chain_id: u64,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    class: wallet_ops::RpcTrafficClass,
) -> Result<wallet_ops::ClassRoute, String> {
    wallet_ops::settings::resolve_effective_chain_rpc_route(chain_id, effective_chain)
        .map_err(|error| error.to_string())?
        .for_class(class)
        .map_err(|error| error.to_string())
}

/// Opens an isolated Sensitive route for one participant or acting account. A failure is
/// returned as is; there is no fallback to the shared route.
pub(super) async fn open_isolated_governance_route(
    chain_id: u64,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
) -> Result<wallet_ops::ClassRoute, String> {
    let route = governance_route(
        chain_id,
        effective_chain,
        wallet_ops::RpcTrafficClass::Sensitive,
    )?;
    let client = http
        .operation_http_client()
        .await
        .map_err(|error| error.to_string())?;
    Ok(route.isolated(client))
}

/// The acting account's Sensitive route over its action's operation client, and that client for
/// the action's estimates and submissions. A failure is returned as is, without a shared-route
/// fallback.
pub(super) async fn action_governance_route(
    chain_id: u64,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    operation: &wallet_ops::OperationHttpClientSlot,
) -> Result<(wallet_ops::ClassRoute, wallet_ops::OperationHttpClient), String> {
    let route = governance_route(
        chain_id,
        effective_chain,
        wallet_ops::RpcTrafficClass::Sensitive,
    )?;
    let client = operation
        .client()
        .await
        .map_err(|error| error.to_string())?;
    Ok((route.isolated(client.clone()), client))
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
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
    http: wallet_ops::HttpContext,
    operation: wallet_ops::OperationHttpClientSlot,
    reward_evidence_mode: RewardEvidenceMode,
    gas_fee_selection: PublicActionGasFeeSelection,
) -> Result<GovernanceSpendDraft, String> {
    // The action's one client carries every governance read and estimate of this preparation.
    let (route, client) =
        action_governance_route(context_key.chain_id, &effective_chain, &operation).await?;
    validate_governance_deployment(context_key.chain_id, &route, &http)
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
                let (balance, allowance) =
                    fetch_governance_token_balance_allowance(chain_id, actor, &route, &http)
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| {
                            "Governance token is not deployed on this chain".to_owned()
                        })?;
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
                let position =
                    fresh_stake_position(chain_id, actor, stake_id, &route, &http).await?;
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
                let position =
                    fresh_stake_position(chain_id, actor, stake_id, &route, &http).await?;
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
                let position =
                    fresh_stake_position(chain_id, actor, stake_id, &route, &http).await?;
                let metrics = fetch_staking_global_metrics(chain_id, &route, &http)
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
                let position =
                    fresh_stake_position(chain_id, actor, stake_id, &route, &http).await?;
                let metrics = fetch_staking_global_metrics(chain_id, &route, &http)
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
            StakingActionKind::RewardClaimSelected { tokens } => {
                if tokens.is_empty() {
                    return Err("Select at least one reward to claim".to_owned());
                }
                if tokens.windows(2).any(|pair| pair[0] >= pair[1]) {
                    return Err(
                        "Selected reward tokens must be unique and ascending by address".to_owned(),
                    );
                }
                let (plan, step, evidence) = plan_reward_claim_draft(
                    chain_id,
                    actor,
                    &tokens,
                    &route,
                    &effective_chain,
                    &http,
                    &client,
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
        operation,
        gas_fee_selection,
        workflow,
        continuation,
        staking_review,
        recipe,
    )
    .await
}

/// Plan a claim for `requested_tokens`, which must already be non-empty and ascending by address.
async fn plan_reward_claim_draft(
    chain_id: u64,
    actor: Address,
    requested_tokens: &[Address],
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
    reward_evidence_mode: RewardEvidenceMode,
) -> Result<
    (
        wallet_ops::RewardClaimPlan,
        wallet_ops::RewardClaimStepPlan,
        wallet_ops::RewardBatchEvidence,
    ),
    String,
> {
    let (evidence, reviewed_steps) = match reward_evidence_mode {
        RewardEvidenceMode::CachedInitial { evidence, steps } => {
            if evidence.reward_tokens.as_slice() != requested_tokens {
                return Err(
                    "Cached reward evidence token set changed; refresh before reviewing again"
                        .to_owned(),
                );
            }
            (*evidence, steps.filter(|steps| !steps.is_empty()))
        }
        RewardEvidenceMode::Fresh => (
            fetch_reward_selection_evidence(chain_id, actor, requested_tokens, route, http).await?,
            None,
        ),
    };
    // The selection form planned these steps against this same evidence, so the split does not
    // have to be estimated again; `plan_reward_claim_batch` still validates them below.
    let steps = match reviewed_steps {
        Some(steps) => steps,
        None => {
            plan_reward_steps_for_evidence(
                chain_id,
                actor,
                &evidence,
                route,
                effective_chain,
                http,
                client,
            )
            .await?
        }
    };
    let plan = wallet_ops::plan_reward_claim_batch(chain_id, actor, actor, true, &evidence, &steps)
        .map_err(|error| error.to_string())?;
    let step = plan
        .steps
        .first()
        .cloned()
        .ok_or_else(|| "Reward claim plan is empty".to_owned())?;
    Ok((plan, step, evidence))
}

/// Read fresh shared-range evidence for `requested_tokens`, which must already be non-empty and
/// ascending by address. One token reads the single-token path and is widened to a batch shape.
async fn fetch_reward_selection_evidence(
    chain_id: u64,
    actor: Address,
    requested_tokens: &[Address],
    route: &wallet_ops::ClassRoute,
    http: &wallet_ops::HttpContext,
) -> Result<wallet_ops::RewardBatchEvidence, String> {
    let metadata = fetch_interval_metadata(chain_id, requested_tokens, route, http)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Reward metadata unavailable".to_owned())?;
    let snapshots = fetch_account_snapshots(
        chain_id,
        actor,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    if let [token] = requested_tokens[..] {
        let evidence = fetch_reward_evidence(
            chain_id,
            actor,
            token,
            &metadata,
            &snapshots,
            route,
            http,
            wallet_ops::MulticallChunkSize::default(),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "No unclaimed reward is available".to_owned())?;
        return Ok(evidence.into());
    }
    fetch_reward_batch_evidence(
        chain_id,
        actor,
        requested_tokens,
        &metadata,
        &snapshots,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "No unclaimed reward is available".to_owned())
}

async fn plan_reward_steps_for_evidence(
    chain_id: u64,
    actor: Address,
    evidence: &wallet_ops::RewardBatchEvidence,
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<Vec<wallet_ops::RewardClaimStep>, String> {
    plan_reward_steps_with_fee(
        chain_id,
        actor,
        evidence,
        route,
        effective_chain,
        http,
        client,
    )
    .await
    .map(|(steps, _)| steps)
}

/// Plan the claim steps for this evidence and report the fee per gas the estimates were priced
/// at, which is what the selection form multiplies by the planned gas. Governance reads use
/// `route`, and gas estimates use `client`, the action's client that `route` is isolated with.
async fn plan_reward_steps_with_fee(
    chain_id: u64,
    actor: Address,
    evidence: &wallet_ops::RewardBatchEvidence,
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<(Vec<wallet_ops::RewardClaimStep>, u128), String> {
    let gas_ceiling = wallet_ops::fetch_latest_block_gas_limit(route, http)
        .await
        .map_err(|error| error.to_string())?;
    let contract = contracts_for_reward(chain_id)?;
    if let Ok(estimate) = estimate_reward_intent(
        chain_id,
        actor,
        contract,
        evidence.to_claim_intent(),
        effective_chain,
        http,
        client,
    )
    .await
        && estimate.gas_limit <= gas_ceiling
    {
        return Ok((
            vec![wallet_ops::RewardClaimStep {
                starting_interval: evidence.starting_interval,
                ending_interval: evidence.ending_interval,
                reward_tokens: evidence.reward_tokens.clone(),
                subtotal: U256::ZERO,
                expected_amounts: evidence.expected_amounts.clone(),
                estimated_gas: estimate.raw_gas_limit,
            }],
            estimate.expected_fee_per_gas,
        ));
    }
    exact_reward_steps(
        chain_id,
        actor,
        evidence,
        gas_ceiling,
        route,
        effective_chain,
        http,
        client,
    )
    .await
}

/// Estimate the selection and its one-token alternatives against one shared evidence range.
async fn estimate_reward_selection(
    chain_id: u64,
    actor: Address,
    tokens: &[Address],
    available: wallet_ops::RewardBatchEvidence,
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<RewardSelectionEstimate, Arc<str>> {
    let evidence = (!tokens.is_empty())
        .then(|| available.project(tokens))
        .transpose()
        .map_err(|error| Arc::from(error.to_string()))?;
    let baseline = if tokens.len() <= 1 {
        wallet_ops::estimate_reward_claim_baseline(
            chain_id,
            actor,
            &available,
            effective_chain,
            http,
            client,
        )
        .await
        .ok()
    } else {
        None
    };
    let (steps, expected_fee_per_gas) = match &evidence {
        Some(evidence) => plan_reward_steps_with_fee(
            chain_id,
            actor,
            evidence,
            route,
            effective_chain,
            http,
            client,
        )
        .await
        .map_err(Arc::from)?,
        None => (
            Vec::new(),
            baseline
                .as_ref()
                .map_or(0, |estimate| estimate.expected_fee_per_gas),
        ),
    };
    let selected_gas =
        reward_selection_gas(&steps).ok_or_else(|| Arc::from("Estimated claim gas overflowed"))?;
    let comparison_gas = if tokens.is_empty() {
        baseline.as_ref().map(|estimate| estimate.raw_gas_limit)
    } else {
        Some(selected_gas)
    };
    let comparisons = available.reward_tokens.iter().copied().map(|token| {
        let available = &available;
        let baseline = baseline.as_ref();
        async move {
            let mut alternative = tokens.to_vec();
            let selected = match alternative.binary_search(&token) {
                Ok(index) => {
                    alternative.remove(index);
                    true
                }
                Err(index) => {
                    alternative.insert(index, token);
                    false
                }
            };
            let alternative_gas = if alternative.is_empty() {
                baseline.map(|estimate| estimate.raw_gas_limit)
            } else if let Ok(evidence) = available.project(&alternative) {
                plan_reward_steps_with_fee(
                    chain_id,
                    actor,
                    &evidence,
                    route,
                    effective_chain,
                    http,
                    client,
                )
                .await
                .ok()
                .and_then(|(steps, _)| reward_selection_gas(&steps))
            } else {
                None
            };
            let added = comparison_gas
                .zip(alternative_gas)
                .and_then(|(current, alternative)| {
                    reward_claim_added_gas(selected, current, alternative)
                });
            (token, added)
        }
    });
    let added_gas = futures_util::stream::iter(comparisons)
        .buffer_unordered(STAKING_REWARD_EVIDENCE_CONCURRENCY)
        .collect::<BTreeMap<_, _>>()
        .await;
    let shared_gas = reward_claim_shared_gas(tokens, comparison_gas, &added_gas);
    Ok(RewardSelectionEstimate {
        evidence,
        steps,
        expected_fee_per_gas,
        added_gas,
        shared_gas,
    })
}

fn reward_selection_gas(steps: &[wallet_ops::RewardClaimStep]) -> Option<u64> {
    steps
        .iter()
        .try_fold(0_u64, |total, step| total.checked_add(step.estimated_gas))
}

const fn reward_claim_added_gas(selected: bool, current: u64, alternative: u64) -> Option<u64> {
    if selected {
        current.checked_sub(alternative)
    } else {
        alternative.checked_sub(current)
    }
}

fn reward_claim_shared_gas(
    selected: &[Address],
    total_gas: Option<u64>,
    added_gas: &BTreeMap<Address, Option<u64>>,
) -> Option<u64> {
    selected.iter().try_fold(total_gas?, |remaining, token| {
        remaining.checked_sub((*added_gas.get(token)?)?)
    })
}

/// The estimated cost of one planned claim selection at the fee per gas its steps were estimated
/// at. It fails closed rather than showing a wrapped number.
fn reward_selection_cost(steps: &[wallet_ops::RewardClaimStep], fee_per_gas: u128) -> Option<U256> {
    let gas = reward_selection_gas(steps)?;
    U256::from(gas).checked_mul(U256::from(fee_per_gas))
}

/// Reprice cached gas with the current quote; retain the estimate's price until a quote arrives.
fn reward_claim_fee_per_gas(
    selection: PublicActionGasFeeSelection,
    quote: Option<wallet_ops::SelfBroadcastGasFeeQuote>,
    auto_fee_per_gas: u128,
) -> u128 {
    match selection {
        PublicActionGasFeeSelection::Auto => quote.map_or(auto_fee_per_gas, |quote| {
            wallet_ops::expected_eip1559_fee_per_gas(
                quote,
                quote.suggested_max_fee_per_gas,
                quote.suggested_max_priority_fee_per_gas,
            )
        }),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } => quote.map_or(max_fee_per_gas, |quote| {
            wallet_ops::expected_eip1559_fee_per_gas(
                quote,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            )
        }),
    }
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
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
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
            effective_chain: effective_chain.clone(),
            from: actor,
            intent: resolved.raw,
            gas_fee: PublicActionGasFeeSelection::Auto,
            access_list: None,
        },
        http,
        client,
    )
    .await
    .map_err(|error| error.to_string())
}

async fn exact_reward_steps(
    chain_id: u64,
    actor: Address,
    evidence: &wallet_ops::RewardBatchEvidence,
    gas_ceiling: u64,
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<(Vec<wallet_ops::RewardClaimStep>, u128), String> {
    let amounts = wallet_ops::fetch_reward_batch_interval_amounts(
        chain_id,
        actor,
        evidence,
        route,
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
    let mut expected_fee_per_gas = 0_u128;
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
        match estimate_reward_intent(
            chain_id,
            actor,
            contract,
            action,
            effective_chain,
            http,
            client,
        )
        .await
        {
            Ok(estimate) if estimate.gas_limit <= gas_ceiling => {
                expected_fee_per_gas = estimate.expected_fee_per_gas;
                accepted.push(wallet_ops::RewardClaimStep {
                    starting_interval: amounts[start].interval,
                    ending_interval: amounts[end - 1].interval,
                    reward_tokens: evidence.reward_tokens.clone(),
                    subtotal: U256::ZERO,
                    expected_amounts,
                    estimated_gas: estimate.raw_gas_limit,
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
    Ok((accepted, expected_fee_per_gas))
}

async fn fresh_stake_position(
    chain_id: u64,
    actor: Address,
    stake_id: U256,
    route: &wallet_ops::ClassRoute,
    http: &wallet_ops::HttpContext,
) -> Result<StakePosition, String> {
    let metrics = fetch_staking_global_metrics(chain_id, route, http)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
    let result = fetch_account_stakes(
        chain_id,
        actor,
        metrics.chain_time,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let stakes = result.stakes?;
    stakes
        .into_iter()
        .find(|stake| stake.id == stake_id)
        .ok_or_else(|| "Stake no longer exists; refresh before authorizing".to_owned())
}

/// Fresh evidence and claim steps for continuing a reward claim after a confirmed split step,
/// read over the actor's `route`. `None` means nothing is left to claim.
async fn read_reward_continuation(
    chain_id: u64,
    actor: Address,
    reward_tokens: &[Address],
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<
    Option<(
        wallet_ops::RewardBatchEvidence,
        Vec<wallet_ops::RewardClaimStep>,
    )>,
    String,
> {
    validate_governance_deployment(chain_id, route, http)
        .await
        .map_err(|error| error.to_string())?;
    let metadata = fetch_interval_metadata(chain_id, reward_tokens, route, http)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Reward metadata unavailable".to_owned())?;
    let snapshots = fetch_account_snapshots(
        chain_id,
        actor,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let fresh = fetch_reward_batch_evidence(
        chain_id,
        actor,
        reward_tokens,
        &metadata,
        &snapshots,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let Some(fresh) = fresh else {
        return Ok(None);
    };
    let steps = plan_reward_steps_for_evidence(
        chain_id,
        actor,
        &fresh,
        route,
        effective_chain,
        http,
        client,
    )
    .await?;
    Ok(Some((fresh, steps)))
}

/// Reads each participant over a route of its own, one at a time in random order. Before every
/// participant after the first it awaits `jitter`, and it stops once `is_current` reports that
/// the refresh was superseded. A route that fails to open reaches `read` as that participant's
/// error, with no shared-route fallback. `read` owns the route, so it closes when `read` ends.
async fn read_participants_isolated<P>(
    mut participants: Vec<P>,
    mut jitter: impl AsyncFnMut(),
    mut is_current: impl FnMut() -> bool,
    mut open_route: impl AsyncFnMut() -> Result<wallet_ops::ClassRoute, String>,
    mut read: impl AsyncFnMut(P, Result<wallet_ops::ClassRoute, String>),
) {
    participants.shuffle(&mut rand::rng());
    for (index, participant) in participants.into_iter().enumerate() {
        if index > 0 {
            jitter().await;
        }
        if !is_current() {
            return;
        }
        let route = open_route().await;
        read(participant, route).await;
    }
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
        evidence: Box<wallet_ops::RewardBatchEvidence>,
        /// Steps already planned against this evidence by the selection form. Preflight validates
        /// them again and plans fresh ones when they are absent.
        steps: Option<Vec<wallet_ops::RewardClaimStep>>,
    },
}

/// Pick the evidence preflight starts from for a claim selection: the estimate the form already
/// planned for exactly these tokens, otherwise the cached dashboard evidence, otherwise a fresh
/// read.
fn reward_evidence_mode_for_selection(
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

pub(super) type ParticipantPickerState = ComboboxState<SearchableVec<ParticipantChoice>>;

// Change carries the complete selection, including choices hidden by the query.
// Confirm is dismissal only; callers perform persistence and refresh only on Some.
fn participant_picker_change(
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum RewardFeePresentation {
    Pending,
    Estimated(RewardFeeEstimate),
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RewardFeeEstimate {
    /// Fee text without the leading estimate marker, in USD when the native token is priced and
    /// in native units otherwise.
    label: String,
    /// Micro-USD value of the estimated fee when the native token has a cached rate.
    usd: Option<U256>,
    /// Whether the added fee exceeds the reward's value.
    below_fee: bool,
}

fn reward_is_below_fee(reward_usd: RewardUsdState, fee_usd: Option<U256>) -> bool {
    matches!(
        (fee_usd, reward_usd),
        (Some(fee_usd), RewardUsdState::Value(reward_usd)) if fee_usd > reward_usd
    )
}

fn reward_cost_presentation(
    chain_id: u64,
    cost: U256,
    reward_usd: RewardUsdState,
    anchor_cache: &TokenAnchorRateCache,
) -> RewardFeeEstimate {
    let usd = anchor_cache.cached_native_usd_micro_value(chain_id, cost);
    let label = usd.map_or_else(
        || super::tokens::format_native_token_amount_for_display(chain_id, cost),
        format_usd_micro_value,
    );
    RewardFeeEstimate {
        label,
        usd,
        below_fee: reward_is_below_fee(reward_usd, usd),
    }
}

fn reward_added_fee_presentation(
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
fn default_reward_claim_selection_for(
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

    /// Evidence behind a current positive row, used to build the claim selection.
    pub(super) fn positive_reward_evidence(
        &self,
        uuid: &str,
        token: Address,
    ) -> Option<&RewardEvidence> {
        match self.resolved_reward(uuid, token)? {
            RewardView::Positive { evidence, .. } => Some(evidence),
            RewardView::Zero | RewardView::Unavailable(_) => None,
        }
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

    /// Fails every cell of one participant so none stays loading.
    pub(super) fn fail_participant(
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
    pub(super) fn resolved_reward(&self, uuid: &str, token: Address) -> Option<&RewardView> {
        let reward_key = (uuid.to_owned(), token);
        self.current_reward_keys
            .contains(&reward_key)
            .then(|| self.rewards.get(&reward_key))
            .flatten()
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
        self.global_ready && self.positive_reward_evidence(uuid, token).is_some()
    }

    /// Whether any resolved reward row of this account is positive, which is what the account's
    /// claim selection needs before it can be opened with nothing selected yet.
    pub(super) fn account_has_positive_reward(&self, uuid: &str) -> bool {
        self.global_ready
            && self.current_reward_keys.iter().any(|reward_key| {
                reward_key.0 == uuid
                    && matches!(
                        self.rewards.get(reward_key),
                        Some(RewardView::Positive { .. })
                    )
            })
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
    /// The latest participation reader and whether it has finished.
    proposal_participation_reader: Option<(gpui::Task<()>, Rc<Cell<bool>>)>,
    pub proposal_action_amount_input: Entity<InputState>,
    pub action_flow: GovernanceActionFlowState,
    pub staking_delegate_input: Entity<InputState>,
    pub reward_gas_fee: Eip1559GasFeeEditorState,
    pub compact_position_details: Option<(String, U256)>,
    pub participant_picker: Entity<ParticipantPickerState>,
    participant_picker_wallet: Option<String>,
    participant_picker_items: Vec<ParticipantChoice>,
    staking_tables: BTreeMap<String, Entity<TableState<StakeTableDelegate>>>,
    participant_time_tick_generation: Option<u64>,
}

impl GovernanceState {
    pub(super) fn new(
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

    fn sync_participant_picker(
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

    pub(super) fn invalidate_action(&mut self) {
        self.action_flow.invalidate();
        self.reward_gas_fee.refresh_id = self.reward_gas_fee.refresh_id.wrapping_add(1);
        self.reward_gas_fee.refreshing = false;
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

/// What the claim selection form knows about the cost of the exact selected call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RewardSelectionEstimateState {
    Pending,
    Estimated(Box<RewardSelectionEstimate>),
    Unavailable(Arc<str>),
}

/// The planned selected claim: the evidence it was planned from, the steps it splits into, and
/// the fee those steps were estimated at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RewardSelectionEstimate {
    pub evidence: Option<wallet_ops::RewardBatchEvidence>,
    pub steps: Vec<wallet_ops::RewardClaimStep>,
    pub expected_fee_per_gas: u128,
    pub added_gas: BTreeMap<Address, Option<u64>>,
    pub shared_gas: Option<u64>,
}

#[derive(Clone, Default)]
pub(super) struct GovernanceActionFlowState {
    pub selection: Option<GovernanceActionSelection>,
    pub recipe: Option<GovernanceDraftRecipe>,
    pub draft: Option<GovernanceSpendDraft>,
    pub pending: bool,
    pub error: Option<Arc<str>>,
    pub generation: u64,
    pub reward_selection_estimate: Option<RewardSelectionEstimateState>,
    pub reward_selection_generation: u64,
    pub seed_reward_selection: bool,
    /// The open action's operation client and the actor it was opened for. A submitted draft
    /// keeps its own clone through its last step.
    pub operation: Option<(Address, wallet_ops::OperationHttpClientSlot)>,
}

impl GovernanceActionFlowState {
    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.recipe = None;
        self.operation = None;
        self.reward_selection_estimate = None;
        self.seed_reward_selection = false;
        // Bumped rather than reset so an estimate still in flight can never match a later one.
        self.reward_selection_generation = self.reward_selection_generation.wrapping_add(1);
    }

    /// The open action's operation client for `actor`. Another actor replaces it.
    pub(super) fn operation(
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
    pub(super) fn reward_claim_selection_tokens(&self) -> Option<&[Address]> {
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
    pub(super) fn set_reward_claim_selection_token(
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
    pub(super) fn set_reward_claim_selection_all(&mut self, mut selected: Vec<Address>) -> bool {
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
    pub(super) fn begin_reward_selection_estimate(&mut self) -> u64 {
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
    pub(super) fn apply_reward_selection_estimate(
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum StakingActionKind {
    Stake,
    Delegate {
        stake_id: U256,
    },
    Undelegate {
        stake_id: U256,
    },
    Unlock {
        stake_id: U256,
    },
    PrincipalClaim {
        stake_id: U256,
    },
    /// The live selection of positive reward tokens, ascending by numeric address. It may be
    /// empty while the selection form is open; review and the draft builder reject that.
    RewardClaimSelected {
        tokens: Vec<Address>,
    },
}

impl StakingActionKind {
    const fn stake_id(&self) -> Option<U256> {
        match self {
            Self::Delegate { stake_id }
            | Self::Undelegate { stake_id }
            | Self::Unlock { stake_id }
            | Self::PrincipalClaim { stake_id } => Some(*stake_id),
            _ => None,
        }
    }

    const fn is_compose_action(&self) -> bool {
        matches!(self, Self::Stake | Self::Delegate { .. })
    }

    const fn is_reward_action(&self) -> bool {
        matches!(self, Self::RewardClaimSelected { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StakingActionSelection {
    pub actor_uuid: String,
    pub actor: Address,
    pub kind: StakingActionKind,
}

pub(super) const fn staking_action_title(kind: &StakingActionKind) -> &'static str {
    match kind {
        StakingActionKind::Stake => "Stake RAIL",
        StakingActionKind::Delegate { .. } => "Delegate stake",
        StakingActionKind::Undelegate { .. } => "Undelegate stake",
        StakingActionKind::Unlock { .. } => "Unlock stake",
        StakingActionKind::PrincipalClaim { .. } => "Claim principal",
        StakingActionKind::RewardClaimSelected { .. } => "Claim rewards",
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
            chain_id: self.last_evm_chain,
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
        let content_width = super::secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(super::proposals::proposal_action_title(kind))
                .on_ok(|_, _, _| false)
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.close_proposal_action(cx);
                    });
                })
                .child(content_root.read(cx).render_proposal_action_dialog_content(
                    content_root.clone(),
                    content_width,
                    cx,
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
        let dialog_title = staking_action_title(&kind);
        let focus_input = match &kind {
            StakingActionKind::Stake => Some(self.governance.proposal_action_amount_input.clone()),
            StakingActionKind::Delegate { .. } => {
                Some(self.governance.staking_delegate_input.clone())
            }
            _ => None,
        };
        let selects_rewards = matches!(kind, StakingActionKind::RewardClaimSelected { .. });
        let reviews_immediately = !selects_rewards
            && !matches!(
                kind,
                StakingActionKind::Delegate { .. } | StakingActionKind::Stake
            );
        let selection = StakingActionSelection {
            actor_uuid: actor_uuid.to_owned(),
            actor,
            kind,
        };
        self.governance.action_flow.set_staking_selection(selection);
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        if selects_rewards {
            self.governance.action_flow.seed_reward_selection = true;
            self.governance.reward_gas_fee.reset_for_request(window, cx);
        }
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
        let dialog_width = (window.viewport_size().width * 0.92).min(if selects_rewards {
            REWARD_SELECTION_DIALOG_WIDTH
        } else {
            STAKING_ACTION_DIALOG_WIDTH
        });
        let dialog_max_height = super::dialog_max_height(window);
        let content_width = super::secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(dialog_title))
                .on_ok(|_, _, _| false)
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.close_staking_action(cx);
                    });
                })
                .child(content_root.read(cx).render_staking_action_dialog_content(
                    content_root.clone(),
                    content_width,
                    cx,
                ))
        });
        if let Some(focus_input) = focus_input {
            cx.defer_in(window, move |_root, window, cx| {
                focus_input.read(cx).focus_handle(cx).focus(window, cx);
            });
        }
        if reviews_immediately {
            self.review_staking_action(window, cx);
        } else if selects_rewards {
            self.refresh_governance_gas_fee_quote(cx);
            self.start_reward_selection_estimate(Duration::ZERO, cx);
        }
        cx.notify();
    }

    pub(super) fn governance_gas_fee_changed(&mut self, cx: &mut Context<'_, Self>) {
        if self.governance.action_flow.pending
            || self
                .governance
                .action_flow
                .reward_claim_selection_tokens()
                .is_none()
        {
            return;
        }
        self.governance.reward_gas_fee.error = self
            .governance
            .reward_gas_fee
            .selection(cx)
            .err()
            .map(Arc::from);
        self.governance.action_flow.error = None;
        // The form derives costs from cached gas. Price edits leave the plan and selection intact.
        cx.notify();
    }

    pub(super) fn set_governance_gas_fee_mode(
        &mut self,
        mode: Eip1559GasFeeMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.governance.action_flow.pending
            || self
                .governance
                .action_flow
                .reward_claim_selection_tokens()
                .is_none()
            || self.governance.reward_gas_fee.mode == mode
        {
            return;
        }
        let gas_fee = &mut self.governance.reward_gas_fee;
        if mode == Eip1559GasFeeMode::Custom {
            gas_fee.seed_custom_from_auto_if_empty(window, cx);
        }
        gas_fee.mode = mode;
        self.governance_gas_fee_changed(cx);
    }

    pub(super) fn customize_governance_gas_fee_from_auto(
        &mut self,
        target: Eip1559GasFeeEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.governance.action_flow.pending
            || self
                .governance
                .action_flow
                .reward_claim_selection_tokens()
                .is_none()
        {
            return;
        }
        let gas_fee = &mut self.governance.reward_gas_fee;
        if !gas_fee.overwrite_custom_from_auto(window, cx) {
            return;
        }
        let focus_input = match target {
            Eip1559GasFeeEditTarget::MaxFee => gas_fee.max_fee_input.clone(),
            Eip1559GasFeeEditTarget::MaxTip => gas_fee.max_priority_fee_input.clone(),
        };
        gas_fee.mode = Eip1559GasFeeMode::Custom;
        self.governance_gas_fee_changed(cx);
        focus_input.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub(super) fn refresh_governance_gas_fee_quote(&mut self, cx: &mut Context<'_, Self>) {
        if self.governance.action_flow.pending
            || self
                .governance
                .action_flow
                .reward_claim_selection_tokens()
                .is_none()
            || self.governance.reward_gas_fee.refreshing
        {
            return;
        }
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        let context_key = self.governance_context_key();
        let gas_fee = &mut self.governance.reward_gas_fee;
        gas_fee.refresh_id = gas_fee.refresh_id.wrapping_add(1);
        gas_fee.refreshing = true;
        gas_fee.quote_error = None;
        let refresh_id = gas_fee.refresh_id;
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let result = wallet_ops::quote_public_action_gas_fee_bundle_with_profile(
                chain_id,
                &effective_chain,
                wallet_ops::PublicShieldTransactionProfile::Railoxide,
                wallet_ops::PublicAssetId::Native,
                &http,
            )
            .await;
            let _ = this.update(cx, |root, cx| {
                if root.governance.reward_gas_fee.refresh_id != refresh_id
                    || root.governance_context_key() != context_key
                {
                    return;
                }
                let gas_fee = &mut root.governance.reward_gas_fee;
                gas_fee.refreshing = false;
                match result {
                    Ok(bundle) => gas_fee.quote = Some(bundle.standard),
                    Err(_) => {
                        gas_fee.quote_error = Some(Arc::from(if gas_fee.quote.is_some() {
                            "Gas quote refresh failed; using the last successful quote."
                        } else {
                            "Gas quote is unavailable."
                        }));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Add or drop one reward in the open claim selection and re-estimate the selected call.
    pub(super) fn set_reward_claim_selection_token(
        &mut self,
        token: Address,
        selected: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .governance
            .action_flow
            .set_reward_claim_selection_token(token, selected)
        {
            return;
        }
        self.governance.action_flow.seed_reward_selection = false;
        self.start_reward_selection_estimate(REWARD_SELECTION_ESTIMATE_DEBOUNCE, cx);
        cx.notify();
    }

    /// Replace the open claim selection and re-estimate the selected call.
    pub(super) fn set_reward_claim_selection_all(
        &mut self,
        tokens: Vec<Address>,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .governance
            .action_flow
            .set_reward_claim_selection_all(tokens)
        {
            return;
        }
        self.governance.action_flow.seed_reward_selection = false;
        self.start_reward_selection_estimate(REWARD_SELECTION_ESTIMATE_DEBOUNCE, cx);
        cx.notify();
    }

    /// Plan and price the exact selected claim. A result is stored only while the selection it
    /// was requested for is still the open one.
    fn start_reward_selection_estimate(&mut self, debounce: Duration, cx: &Context<'_, Self>) {
        let generation = self
            .governance
            .action_flow
            .begin_reward_selection_estimate();
        let Some(selection) = self.governance.action_flow.staking_selection().cloned() else {
            return;
        };
        let StakingActionKind::RewardClaimSelected { tokens } = selection.kind else {
            return;
        };
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        let context_key = self.governance_context_key();
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let http = self.http.clone();
        let actor = selection.actor;
        let operation = self.governance.action_flow.operation(actor, &self.http);
        let actor_uuid = selection.actor_uuid;
        let mut available_tokens = governance_contracts(chain_id)
            .map_or(&[][..], |contracts| contracts.reward_tokens)
            .iter()
            .filter(|entry| {
                self.governance
                    .staking
                    .positive_reward_evidence(&actor_uuid, entry.token)
                    .is_some()
            })
            .map(|entry| entry.token)
            .collect::<Vec<_>>();
        available_tokens.sort_unstable();
        cx.spawn(async move |this, cx| {
            if !debounce.is_zero() {
                cx.background_executor().timer(debounce).await;
            }
            let current = this.update(cx, |root, _cx| {
                (root.governance.action_flow.reward_selection_generation == generation
                    && root.governance.action_flow.reward_claim_selection_tokens()
                        == Some(tokens.as_slice()))
                .then(|| {
                    root.governance.staking.cached_reward_evidence_at(
                        &context_key,
                        &actor_uuid,
                        actor,
                        &available_tokens,
                        Instant::now(),
                    )
                })
            });
            let Ok(Some(cached)) = current else {
                return;
            };
            let result = async {
                // The action's one client carries every governance read and estimate.
                let (route, client) =
                    action_governance_route(chain_id, &effective_chain, &operation)
                        .await
                        .map_err(Arc::<str>::from)?;
                let available = match cached {
                    Some(evidence) => evidence,
                    None => fetch_reward_selection_evidence(
                        chain_id,
                        actor,
                        &available_tokens,
                        &route,
                        &http,
                    )
                    .await
                    .map_err(Arc::<str>::from)?,
                };
                estimate_reward_selection(
                    chain_id,
                    actor,
                    &tokens,
                    available,
                    &route,
                    &effective_chain,
                    &http,
                    &client,
                )
                .await
            }
            .await;
            let _ = this.update(cx, |root, cx| {
                if root
                    .governance
                    .action_flow
                    .apply_reward_selection_estimate(generation, result)
                {
                    if root.governance.action_flow.seed_reward_selection {
                        root.governance.action_flow.seed_reward_selection = false;
                        if let Some(RewardSelectionEstimateState::Estimated(estimate)) =
                            &root.governance.action_flow.reward_selection_estimate
                        {
                            let selected = default_reward_claim_selection_for(
                                &root.governance.staking,
                                chain_id,
                                &actor_uuid,
                                &available_tokens,
                                estimate,
                                root.governance.reward_gas_fee.quote,
                                &root.public_broadcaster_anchor_cache,
                            );
                            if selected != tokens {
                                root.governance
                                    .action_flow
                                    .set_reward_claim_selection_all(selected);
                                root.start_reward_selection_estimate(Duration::ZERO, cx);
                            }
                        }
                    }
                    cx.notify();
                }
            });
        })
        .detach();
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
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
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
        let reward_evidence_mode = match &selection.kind {
            StakingActionKind::RewardClaimSelected { tokens } => {
                if tokens.is_empty() {
                    self.governance.action_flow.error =
                        Some(Arc::from("Select at least one reward to claim"));
                    cx.notify();
                    return;
                }
                reward_evidence_mode_for_selection(
                    self.governance
                        .action_flow
                        .reward_selection_estimate
                        .as_ref(),
                    tokens,
                    self.governance.staking.cached_reward_evidence_at(
                        &context_key,
                        &selection.actor_uuid,
                        selection.actor,
                        tokens,
                        Instant::now(),
                    ),
                )
            }
            _ => RewardEvidenceMode::Fresh,
        };
        let gas_fee_selection = if matches!(
            selection.kind,
            StakingActionKind::RewardClaimSelected { .. }
        ) {
            match self.governance.reward_gas_fee.selection(cx) {
                Ok(selection) => selection,
                Err(error) => {
                    self.governance.reward_gas_fee.error = Some(Arc::from(error));
                    cx.notify();
                    return;
                }
            }
        } else {
            PublicActionGasFeeSelection::Auto
        };
        let operation = self
            .governance
            .action_flow
            .operation(selection.actor, &self.http);
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
            let result = Box::pin(build_staking_action_draft(
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
                operation,
                reward_evidence_mode,
                gas_fee_selection,
            ))
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
        operation: wallet_ops::OperationHttpClientSlot,
        confirmed_hash: Option<B256>,
        #[cfg(feature = "hardware")] window: &Window,
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
        let Ok(effective_chain) = self
            .effective_chain_configs
            .enabled(context.chain_id)
            .cloned()
        else {
            return;
        };
        let http = self.http.clone();
        let generation = self.governance.action_flow.generation.wrapping_add(1);
        self.governance.action_flow.generation = generation;
        self.governance.action_flow.recipe = Some(recipe.clone());
        self.governance.action_flow.pending = true;
        self.governance.action_flow.draft = None;
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<Option<GovernanceSpendDraft>, String> = Box::pin(async {
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
                // The continuation reads fresh state over the submitted action's client.
                let (route, client) =
                    action_governance_route(context.chain_id, &effective_chain, &operation).await?;
                let continued = read_reward_continuation(
                    context.chain_id,
                    context.actor,
                    &progress.plan.reward_tokens,
                    &route,
                    &effective_chain,
                    &http,
                    &client,
                )
                .await?;
                let Some((fresh, fresh_steps)) = continued else {
                    return Ok(None);
                };
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
                    operation,
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
            })
            .await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.selected_evm_chain() != Some(context.chain_id)
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

    /// Shows the memoized participation for `proposal` without reading.
    pub(super) fn show_proposal_participation(
        &mut self,
        proposal: &wallet_ops::GovernanceProposal,
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
                            GovernanceRefreshTarget::Proposal(action_key)
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
        self.governance.proposal_participation.show(&key);
    }

    /// Reads the selected proposal's participation for every participant when `refresh`, or
    /// only for those without a loaded row.
    pub(super) fn request_proposal_participation(
        &mut self,
        refresh: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(proposal) = self.proposals.selected_proposal().cloned() else {
            return;
        };
        self.show_proposal_participation(&proposal);
        let accounts = if refresh {
            self.governance
                .proposal_participation
                .key
                .as_ref()
                .map_or_else(Vec::new, |key| {
                    key.context
                        .participants
                        .iter()
                        .map(|participant| participant.address)
                        .collect()
                })
        } else {
            self.governance.proposal_participation.unloaded(&proposal)
        };
        self.start_proposal_participation(&proposal, accounts, cx);
        cx.notify();
    }

    fn start_proposal_participation(
        &mut self,
        proposal: &wallet_ops::GovernanceProposal,
        accounts: Vec<Address>,
        cx: &Context<'_, Self>,
    ) {
        if self.governance.tab != GovernanceTab::Proposals {
            return;
        }
        let key = super::governance_action::proposal_participation_key(
            proposal,
            self.governance_context_key(),
        );
        if self.governance.proposal_participation.key.as_ref() != Some(&key) || accounts.is_empty()
        {
            return;
        }
        let generation = self
            .governance
            .proposal_participation
            .begin(&key, &accounts);
        let Ok(effective_chain) = self
            .effective_chain_configs
            .enabled(key.context.chain_id)
            .cloned()
        else {
            return;
        };
        let http = self.http.clone();
        let proposal = proposal.clone();
        let chain_id = key.context.chain_id;
        // `begin` superseded the previous reader, which stops after any read in flight. Waiting
        // for it, then for jitter if it was still running, keeps participant reads one at a time.
        let previous = self
            .governance
            .proposal_participation_reader
            .take()
            .map(|(task, finished)| (task, !finished.get()));
        let finished = Rc::new(Cell::new(false));
        let reader_finished = Rc::clone(&finished);
        let reader = cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            if let Some((previous, running)) = previous {
                previous.await;
                if running {
                    executor.timer(isolated_read_jitter()).await;
                }
            }
            let current_cx = cx.clone();
            read_participants_isolated(
                accounts,
                async || executor.timer(isolated_read_jitter()).await,
                || {
                    this.read_with(&current_cx, |root, _| {
                        root.governance
                            .proposal_participation
                            .current(&key, generation)
                    })
                    .unwrap_or(false)
                },
                async || open_isolated_governance_route(chain_id, &effective_chain, &http).await,
                async |account, route| {
                    let row = match route {
                        Ok(route) => wallet_ops::fetch_governance_participation(
                            chain_id, &proposal, account, &route, &http,
                        )
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|row| row.state.map_err(|error| error.to_string())),
                        Err(error) => Err(error),
                    };
                    let _ = this.update(&mut *cx, |root, cx| {
                        root.governance.proposal_participation.apply(
                            &key,
                            generation,
                            account,
                            row.map_err(Arc::from),
                        );
                        cx.notify();
                    });
                },
            )
            .await;
            reader_finished.set(true);
        });
        self.governance.proposal_participation_reader = Some((reader, finished));
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
                    self.show_proposal_participation(proposal);
                }
            }
            GovernanceTab::Staking => self.start_staking_refresh(cx),
        }
        cx.notify();
    }

    pub(super) fn participant_picker_event(
        &mut self,
        event: &ComboboxEvent<SearchableVec<ParticipantChoice>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if matches!(event, ComboboxEvent::Confirm(_)) {
            self.governance
                .participant_picker
                .update(cx, |picker, cx| picker.set_query("", window, cx));
            return;
        }
        let saved = self
            .selected_wallet_id
            .as_deref()
            .and_then(|wallet| self.ui_state.governance_participants.get(wallet))
            .map_or(&[][..], Vec::as_slice);
        let Some(values) = participant_picker_change(
            event,
            &self.public_accounts,
            self.selected_wallet_id.as_deref(),
            self.governance.participant_picker_wallet.as_deref(),
            saved,
        ) else {
            return;
        };
        let Some(wallet_id) = self.selected_wallet_id.clone() else {
            return;
        };
        self.ui_state
            .governance_participants
            .insert(wallet_id.to_string(), values);
        self.save_ui_state();
        self.invalidate_governance_context();
        if self.governance.tab == GovernanceTab::Staking {
            self.start_staking_refresh(cx);
        } else {
            let selected = self.proposals.selected_proposal().cloned();
            if let Some(proposal) = selected.as_ref() {
                self.show_proposal_participation(proposal);
            }
        }
        cx.notify();
    }

    pub(super) fn start_staking_refresh(&mut self, cx: &mut Context<'_, Self>) {
        if self.governance.tab != GovernanceTab::Staking || self.selected_evm_chain().is_none() {
            return;
        }
        self.clean_governance_participants();
        let key = self.governance_context_key();
        let generation = self.governance.staking.begin(key.clone());
        let chain_id = key.chain_id;
        let participant_ids = key.participants.clone();
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
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
            let general = governance_route(
                chain_id,
                &effective_chain,
                wallet_ops::RpcTrafficClass::General,
            );
            let global = match &general {
                Ok(route) => fetch_staking_global_metrics(chain_id, route, &http)
                    .await
                    .map_err(|error| Arc::from(error.to_string()))
                    .and_then(|metrics| {
                        metrics.ok_or_else(|| Arc::from("Staking is not deployed on this chain"))
                    }),
                Err(error) => Err(Arc::from(error.as_str())),
            };
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
            let (Some(metrics), Ok(general)) = (global_metrics, general) else {
                return;
            };
            let metadata = fetch_interval_metadata(chain_id, &tokens, &general, &http)
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
            let executor = cx.background_executor().clone();
            let current_cx = cx.clone();
            let chunk_size = wallet_ops::MulticallChunkSize::default();
            read_participants_isolated(
                participant_ids,
                async || executor.timer(isolated_read_jitter()).await,
                || {
                    this.read_with(&current_cx, |root, _| {
                        root.governance.staking.current(&key, generation)
                    })
                    .unwrap_or(false)
                },
                async || open_isolated_governance_route(chain_id, &effective_chain, &http).await,
                async |participant: GovernanceParticipantIdentity, route| {
                    let route = match route {
                        Ok(route) => route,
                        Err(error) => {
                            let _ = this.update(&mut *cx, |root, cx| {
                                root.governance.staking.fail_participant(
                                    &key,
                                    generation,
                                    participant.uuid.clone(),
                                    &tokens,
                                    Arc::from(error),
                                );
                                cx.notify();
                            });
                            return;
                        }
                    };
                    let account = fetch_account_stakes(
                        chain_id,
                        participant.address,
                        metrics.chain_time,
                        &route,
                        &http,
                        chunk_size,
                    )
                    .await
                    .map_err(|error| Arc::<str>::from(error.to_string()));
                    let _ = this.update(&mut *cx, |root, cx| {
                        root.governance.staking.apply_account(
                            &key,
                            generation,
                            participant.uuid.clone(),
                            account,
                        );
                        cx.notify();
                    });
                    let snapshots = fetch_account_snapshots(
                        chain_id,
                        participant.address,
                        &route,
                        &http,
                        chunk_size,
                    )
                    .await
                    .map_err(|error| Arc::<str>::from(error.to_string()));
                    let per_token = async {
                        match (metadata.as_ref(), &snapshots) {
                            (Some(metadata), Ok(snapshots)) => fetch_reward_evidence_multi(
                                chain_id,
                                participant.address,
                                &tokens,
                                metadata,
                                snapshots,
                                &route,
                                &http,
                                chunk_size,
                            )
                            .await
                            .map_err(|error| Arc::<str>::from(error.to_string())),
                            (None, _) => Err(Arc::from("Reward interval metadata unavailable")),
                            (_, Err(error)) => Err(Arc::clone(error)),
                        }
                    };
                    let bulk = async {
                        match (metadata.as_ref(), &snapshots) {
                            (Some(metadata), Ok(snapshots)) => fetch_reward_batch_evidence(
                                chain_id,
                                participant.address,
                                &tokens,
                                metadata,
                                snapshots,
                                &route,
                                &http,
                                chunk_size,
                            )
                            .await
                            .map_err(|error| Arc::<str>::from(error.to_string())),
                            (None, _) => Err(Arc::from("Reward interval metadata unavailable")),
                            (_, Err(error)) => Err(Arc::clone(error)),
                        }
                    };
                    let (rewards, evidence) = futures_util::future::join(per_token, bulk).await;
                    drop(route);
                    let _ = this.update(&mut *cx, |root, cx| {
                        for (token_index, &token) in tokens.iter().enumerate() {
                            let reward = match &rewards {
                                Ok(results) => results.get(token_index).map_or_else(
                                    || Err(Arc::from("Reward result is missing")),
                                    |result| result.evidence.clone().map_err(Arc::from),
                                ),
                                Err(error) => Err(Arc::clone(error)),
                            };
                            root.governance.staking.apply_reward(
                                &key,
                                generation,
                                participant.uuid.clone(),
                                token,
                                reward,
                            );
                        }
                        root.governance.staking.apply_bulk_reward(
                            &key,
                            generation,
                            participant.uuid.clone(),
                            tokens.clone(),
                            &evidence,
                        );
                        cx.notify();
                    });
                },
            )
            .await;
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

    fn sync_governance_participant_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let values = self
            .governance_participants()
            .iter()
            .map(|account| account.public_account_uuid.clone())
            .collect::<Vec<_>>();
        let selected = values.iter().cloned().collect();
        let items = participant_choices(
            &self.public_accounts,
            &selected,
            self.selected_wallet_id.as_deref(),
            "",
        );
        self.governance.sync_participant_picker(
            self.selected_wallet_id.as_deref().map(str::to_owned),
            items,
            &values,
            window,
            cx,
        );
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
        self.sync_governance_participant_picker(window, cx);
        let selected_ids = self
            .governance_participants()
            .iter()
            .map(|account| account.public_account_uuid.clone())
            .collect::<BTreeSet<_>>();
        let summary = participant_summary(
            &self.public_accounts,
            &selected_ids,
            self.selected_wallet_id.as_deref(),
        );
        let picker = Combobox::new(&self.governance.participant_picker)
            .small()
            .w(participant_trigger_width)
            .max_w_full()
            .p_0()
            .appearance(false)
            .menu_width(GOVERNANCE_PARTICIPANT_POPUP_WIDTH)
            .menu_max_h(px(350.0))
            .search_placeholder("search participating accounts")
            .empty(|_, _| app_muted_text("No visible accounts match this search."))
            .render_trigger(move |_, _, _| {
                let label = match summary.selected {
                    0 => "Enroll accounts...".to_owned(),
                    1 => "1 account enrolled".to_owned(),
                    count => format!("{count} accounts enrolled"),
                };
                div()
                    .w_full()
                    .h(px(24.0))
                    .min_w(px(0.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        Icon::empty()
                            .path(USERS_ICON_PATH)
                            .size_4()
                            .flex_none()
                            .text_color(rgb(theme::TEXT_MUTED)),
                    )
                    .child(
                        app_text(label)
                            .min_w(px(0.0))
                            .flex_1()
                            .truncate()
                            .when(summary.selected == 0, |this| {
                                this.text_color(rgb(theme::TEXT_MUTED))
                            }),
                    )
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .xsmall()
                            .flex_none()
                            .text_color(rgb(theme::TEXT_PLACEHOLDER)),
                    )
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
                        this.child(app_strong_text("Governance").text_size(px(20.0)))
                    })
                    .child(self.render_wallet_selector())
                    .child(
                        div()
                            .w(participant_trigger_width)
                            .h(px(24.0))
                            .flex_none()
                            .child(picker),
                    )
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
        if self
            .selected_evm_chain()
            .and_then(governance_contracts)
            .is_none()
        {
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
                    .child(app_muted_shimmer_text("Loading staking data..."));
            }
            _ => {}
        }
        if let Some(metrics) = state.metrics.as_ref() {
            let token = governance_contracts(self.last_evm_chain)
                .map_or(Address::ZERO, |contracts| contracts.governance_token);
            let reward_interval = state.reward_interval_countdown;
            let remaining = state.chain_time().and_then(|now| {
                reward_interval
                    .and_then(|countdown| remaining_reward_interval_seconds(countdown, now))
            });
            let total_staked = format_staking_amount(
                self.last_evm_chain,
                token,
                metrics.total_staked,
                &self.effective_token_registry,
            );
            let voting_power = format_staking_amount(
                self.last_evm_chain,
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
            content = content.child(render_staking_enrollment_prompt(root));
        }
        for account in participants {
            let uuid = account.public_account_uuid.clone();
            let inactive = account.status == PublicAccountStatus::Inactive;
            let actor_uuid = uuid.clone();
            let account_action_ready = state.account_action_ready(&uuid);
            let token = governance_contracts(self.last_evm_chain)
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
                        self.last_evm_chain,
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
                        self.last_evm_chain,
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
                    self.last_evm_chain,
                    token,
                    projection.active_self_delegated,
                    &self.effective_token_registry,
                );
                let received_display = format_staking_amount(
                    self.last_evm_chain,
                    token,
                    received,
                    &self.effective_token_registry,
                );
                let delegated_display = format_staking_amount(
                    self.last_evm_chain,
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
                            self.last_evm_chain,
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
                voting_power_metric(&voting_power_display, account_view.is_none(), qualifier),
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
                            format!("governance-stakes-{}-{}", self.last_evm_chain, uuid);
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
                                .row_selectable(true)
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
                        DataTable::new(&table)
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
                        .child(app_muted_shimmer_text("Loading account stakes...")),
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
                                self.last_evm_chain,
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
            let reward_tokens = governance_contracts(self.last_evm_chain)
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
                let positive_reward_tokens = reward_token_addresses
                    .iter()
                    .copied()
                    .filter(|&token| state.positive_reward_evidence(&uuid, token).is_some())
                    .collect::<Vec<_>>();
                let positive_token_count = positive_reward_tokens.len();
                let (total_unclaimed, total_loading) = match reward_usd_total(
                    self.last_evm_chain,
                    &uuid,
                    &reward_token_addresses,
                    &state.rewards,
                    &state.current_reward_keys,
                    &self.public_broadcaster_anchor_cache,
                ) {
                    RewardUsdState::Loading => ("Loading...".to_owned(), true),
                    RewardUsdState::Unavailable => ("USD unavailable".to_owned(), false),
                    RewardUsdState::Value(value) => (
                        format!("{} unclaimed", format_usd_micro_value(value)),
                        false,
                    ),
                };
                let claim_selection = StakingActionSelection {
                    actor_uuid: uuid.clone(),
                    actor: account.address,
                    kind: StakingActionKind::RewardClaimSelected {
                        tokens: positive_reward_tokens.clone(),
                    },
                };
                let claim_ready = positive_token_count >= 1
                    && state.action_selection_ready(&claim_selection)
                    && !inactive
                    && self.governance.action_flow.selection.is_none();
                let claim_root = root.clone();
                let claim_uuid = uuid.clone();
                let claim_actor = account.address;
                let claim_tokens = positive_reward_tokens;
                let claim_button =
                    app_button_base(staking_control_id(&uuid, "reward-claim", "account"))
                        .primary()
                        .small()
                        .child("Claim…")
                        .on_click(move |_event, window, cx| {
                            let tokens = claim_tokens.clone();
                            claim_root.update(cx, |root, cx| {
                                root.open_staking_action(
                                    claim_uuid.as_str(),
                                    claim_actor,
                                    StakingActionKind::RewardClaimSelected { tokens },
                                    window,
                                    cx,
                                );
                            });
                        });
                let reward_rows = reward_tokens.iter().filter_map(|token| {
                    let resolved_reward = state.resolved_reward(&uuid, token.token);
                    if matches!(resolved_reward, Some(RewardView::Zero)) {
                        return None;
                    }
                    Some(render_reward_row(
                        self.last_evm_chain,
                        token.symbol,
                        token.token,
                        resolved_reward,
                        &self.effective_token_registry,
                        &self.public_broadcaster_anchor_cache,
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
                            .child(
                                if total_loading {
                                    app_muted_shimmer_text(total_unclaimed)
                                } else {
                                    app_muted_text(total_unclaimed)
                                }
                                .text_size(px(11.0)),
                            ),
                    )
                    .when(claim_ready, |this| {
                        this.child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .justify_end()
                                .child(claim_button),
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

fn render_staking_enrollment_prompt(root: &Entity<WalletRoot>) -> gpui::Div {
    let picker_root = root.clone();
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
                .on_click(move |_event, window, cx| {
                    picker_root.update(cx, |root, cx| {
                        root.governance
                            .participant_picker
                            .read(cx)
                            .focus_handle(cx)
                            .focus(window, cx);
                        window.dispatch_action(Box::new(gpui_kit::base::actions::SelectDown), cx);
                    });
                }),
        )
}

fn voting_power_metric(value: &str, loading: bool, qualifier: Option<String>) -> gpui::Div {
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
                .child(if loading {
                    app_shimmer_text(value.to_owned())
                        .text_color(rgb(theme::TEXT))
                        .font_weight(FontWeight::MEDIUM)
                } else {
                    app_strong_text(value.to_owned())
                })
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

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].1.clone()
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

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        render_staking_table_row(row_ix)
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
                    app_button_label(format!("#{}", compact_stake_id_preview(row.id)))
                        .text_color(rgb(theme::TEXT_MUTED))
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
                        focus_handle.focus(window, cx);
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
                                    kind.clone(),
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
    symbol: &str,
    token: Address,
    reward: Option<&RewardView>,
    registry: &wallet_ops::settings::EffectiveTokenRegistry,
    anchor_cache: &TokenAnchorRateCache,
) -> gpui::Div {
    let metadata = token_display_metadata(Some(registry), chain_id, &token);
    let display_symbol = metadata
        .as_ref()
        .map_or_else(|| symbol.to_owned(), |info| info.symbol.clone());
    let icon_path = metadata.and_then(|info| info.icon_path);
    let display_amount = |amount| format_reward_amount(chain_id, token, amount, registry);
    let (amount, detail) = match reward {
        None => ("Loading...".to_owned(), "Calculation pending".to_owned()),
        Some(RewardView::Zero) => (
            display_amount(U256::ZERO),
            "No completed unclaimed intervals".to_owned(),
        ),
        Some(RewardView::Positive { amount, .. }) => (display_amount(*amount), String::new()),
        Some(RewardView::Unavailable(error)) => ("Unavailable".to_owned(), error.to_string()),
    };
    let usd_label = reward_usd_label(chain_id, token, reward, anchor_cache);
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
                .child(
                    if reward.is_none() {
                        app_shimmer_text(amount)
                            .text_color(rgb(theme::TEXT))
                            .font_weight(FontWeight::MEDIUM)
                    } else {
                        app_strong_text(amount)
                    }
                    .truncate(),
                ),
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

/// One positive reward row of the claim selection form, with everything the row, the totals, and
/// the default selection read.
struct RewardSelectionRow {
    token: Address,
    symbol: String,
    icon_path: Option<WalletIconSource>,
    amount: String,
    usd: RewardUsdState,
    fee: RewardFeePresentation,
    gas: Option<u64>,
    selected: bool,
}

/// Gas in the compact form the fee sub-lines use.
fn compact_gas(gas: u64) -> String {
    format!("{}k", gas.saturating_add(500) / 1_000)
}

/// The estimate's fee per gas in gwei, to one decimal.
fn gwei_label(fee_per_gas: u128) -> String {
    let whole = fee_per_gas / 1_000_000_000;
    if whole >= 10 {
        return format!("{whole} gwei");
    }
    let hundredths = fee_per_gas % 1_000_000_000 / 10_000_000;
    format!("{whole}.{hundredths:02} gwei")
}

/// The gas sub-line under the selection fee: the single call, or the split and its per-step gas.
fn reward_selection_gas_summary(
    steps: &[wallet_ops::RewardClaimStep],
    fee_per_gas: u128,
) -> String {
    if let [step] = steps {
        return format!(
            "{} gas · {}",
            compact_gas(step.estimated_gas),
            gwei_label(fee_per_gas)
        );
    }
    format!(
        "{} transactions · {} gas",
        steps.len(),
        steps
            .iter()
            .map(|step| compact_gas(step.estimated_gas))
            .collect::<Vec<_>>()
            .join(" + ")
    )
}

/// Reward value minus claim fee, and whether the fee was the larger of the two.
fn reward_net_label(reward_usd: U256, fee_usd: U256) -> (String, bool) {
    if fee_usd > reward_usd {
        (
            format!("-{}", format_usd_micro_value(fee_usd - reward_usd)),
            true,
        )
    } else {
        (format_usd_micro_value(reward_usd - fee_usd), false)
    }
}

/// The claim selection form: the account's positive rewards with their fee and net value, the
/// totals for the exact selected call, and Review into the existing preflight.
fn render_reward_claim_selection_form(
    root: &Entity<WalletRoot>,
    wallet: &WalletRoot,
    selection: &StakingActionSelection,
    tokens: &[Address],
    content_width: gpui::Pixels,
    cx: &App,
) -> gpui::Div {
    let chain_id = wallet.last_evm_chain;
    let uuid = selection.actor_uuid.as_str();
    let state = &wallet.governance.staking;
    let anchor_cache = &wallet.public_broadcaster_anchor_cache;
    let action_flow = &wallet.governance.action_flow;
    let registry = &wallet.effective_token_registry;
    let gas_fee = &wallet.governance.reward_gas_fee;
    let gas_selection = gas_fee.selection(cx);
    let estimate = action_flow.reward_selection_estimate.as_ref();
    let estimated = match estimate {
        Some(RewardSelectionEstimateState::Estimated(estimate)) => Some(estimate),
        _ => None,
    };
    let selected_fee_per_gas =
        estimated
            .zip(gas_selection.as_ref().ok())
            .map(|(estimate, selection)| {
                reward_claim_fee_per_gas(*selection, gas_fee.quote, estimate.expected_fee_per_gas)
            });
    let rows = governance_contracts(chain_id)
        .map_or(&[][..], |contracts| contracts.reward_tokens)
        .iter()
        .filter_map(|entry| {
            let token = entry.token;
            let resolved = state.resolved_reward(uuid, token);
            let Some(RewardView::Positive { amount, .. }) = resolved else {
                return None;
            };
            let metadata = token_display_metadata(Some(registry), chain_id, &token);
            let usd = reward_usd_state(chain_id, token, resolved, anchor_cache);
            let fee = match (estimated, selected_fee_per_gas) {
                (Some(estimate), Some(price)) => reward_added_fee_presentation(
                    chain_id,
                    estimate,
                    token,
                    price,
                    usd,
                    anchor_cache,
                ),
                _ if gas_selection.is_ok()
                    && matches!(estimate, Some(RewardSelectionEstimateState::Pending)) =>
                {
                    RewardFeePresentation::Pending
                }
                _ => RewardFeePresentation::Unavailable,
            };
            Some(RewardSelectionRow {
                token,
                symbol: metadata
                    .as_ref()
                    .map_or_else(|| entry.symbol.to_owned(), |info| info.symbol.clone()),
                icon_path: metadata.and_then(|info| info.icon_path),
                amount: format_reward_amount(chain_id, token, *amount, registry),
                usd,
                fee,
                gas: estimated
                    .and_then(|estimate| estimate.added_gas.get(&token).copied().flatten()),
                selected: tokens.contains(&token),
            })
        })
        .collect::<Vec<_>>();
    let selected_count = rows.iter().filter(|row| row.selected).count();
    let all_selected = !rows.is_empty() && selected_count == rows.len();
    let selected_usd = rows
        .iter()
        .filter(|row| row.selected)
        .try_fold(U256::ZERO, |total, row| match row.usd {
            RewardUsdState::Value(value) => total.checked_add(value),
            RewardUsdState::Loading | RewardUsdState::Unavailable => None,
        });
    let selected_value = format!(
        "{selected_count} of {} · {}",
        rows.len(),
        if selected_count == 0 {
            "—".to_owned()
        } else {
            selected_usd.map_or_else(|| "USD unavailable".to_owned(), format_usd_micro_value)
        }
    );
    let selected_cost = estimated
        .zip(selected_fee_per_gas)
        .and_then(|(estimate, price)| reward_selection_cost(&estimate.steps, price))
        .filter(|_| !tokens.is_empty());
    let fee_usd = selected_cost
        .and_then(|cost| anchor_cache.cached_native_usd_micro_value(chain_id, cost))
        .filter(|_| selected_count > 0);
    let header_root = root.clone();
    let header_tokens = rows.iter().map(|row| row.token).collect::<Vec<_>>();
    let mut list = div()
        .flex()
        .flex_col()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .bg(rgb(theme::SURFACE_ELEVATED))
                .child(
                    div().w(REWARD_SELECTION_CHECKBOX_WIDTH).flex_none().child(
                        Checkbox::new("governance-reward-select-all")
                            .checked(all_selected)
                            .on_click(move |checked: &bool, _window, cx| {
                                let tokens = if *checked {
                                    header_tokens.clone()
                                } else {
                                    Vec::new()
                                };
                                header_root.update(cx, |root, cx| {
                                    root.set_reward_claim_selection_all(tokens, cx);
                                });
                            }),
                    ),
                )
                .child(
                    app_muted_text("Asset")
                        .text_size(px(11.0))
                        .w(REWARD_SELECTION_ASSET_WIDTH)
                        .flex_none(),
                )
                .child(
                    app_muted_text("Unclaimed")
                        .text_size(px(11.0))
                        .flex_1()
                        .min_w(px(0.0)),
                )
                .child(
                    app_muted_text("Added fee")
                        .text_size(px(11.0))
                        .w(REWARD_SELECTION_FEE_WIDTH)
                        .flex_none()
                        .text_right(),
                )
                .child(
                    app_muted_text("Net")
                        .text_size(px(11.0))
                        .w(REWARD_SELECTION_NET_WIDTH)
                        .flex_none()
                        .text_right(),
                ),
        );
    for row in rows {
        let toggle_root = root.clone();
        let token = row.token;
        let selected = row.selected;
        let (row_fee_usd, below_fee) = match &row.fee {
            RewardFeePresentation::Estimated(fee) => (fee.usd, fee.below_fee),
            RewardFeePresentation::Pending | RewardFeePresentation::Unavailable => (None, false),
        };
        let dimmed = below_fee && !selected;
        let fee_cell = match row.fee {
            RewardFeePresentation::Pending => div()
                .w(REWARD_SELECTION_FEE_WIDTH)
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .gap_1()
                .child(Spinner::new().small())
                .child(
                    app_muted_shimmer_text("estimating…")
                        .text_size(px(11.0))
                        .whitespace_nowrap(),
                ),
            RewardFeePresentation::Estimated(fee) => div()
                .w(REWARD_SELECTION_FEE_WIDTH)
                .flex_none()
                .flex()
                .flex_col()
                .items_end()
                .child(app_text(format!("≈ {}", fee.label)).whitespace_nowrap())
                .children(row.gas.map(|gas| {
                    app_muted_text(format!("{} gas", compact_gas(gas)))
                        .text_size(px(11.0))
                        .whitespace_nowrap()
                })),
            RewardFeePresentation::Unavailable => div()
                .w(REWARD_SELECTION_FEE_WIDTH)
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .child(
                    app_muted_text("unavailable")
                        .text_size(px(11.0))
                        .whitespace_nowrap(),
                ),
        };
        let net_cell = match (row.usd, row_fee_usd) {
            (RewardUsdState::Value(reward_usd), Some(row_fee_usd)) => {
                let (label, negative) = reward_net_label(reward_usd, row_fee_usd);
                app_text(label)
                    .whitespace_nowrap()
                    .when(negative, |value| value.text_color(rgb(theme::DANGER)))
            }
            _ => app_muted_text("—"),
        };
        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_2()
                .border_t_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .when(dimmed, |row| row.opacity(0.6))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(REWARD_SELECTION_CHECKBOX_WIDTH)
                                .flex_none()
                                .child(
                                    Checkbox::new(staking_control_id(
                                        uuid,
                                        "reward-select",
                                        token,
                                    ))
                                    .checked(selected)
                                    .on_click(move |checked: &bool, _window, cx| {
                                        let checked = *checked;
                                        toggle_root.update(cx, |root, cx| {
                                            root.set_reward_claim_selection_token(
                                                token, checked, cx,
                                            );
                                        });
                                    }),
                                ),
                        )
                        .child(
                            token_label_row(
                                SharedString::from(row.symbol),
                                row.icon_path,
                                px(16.0),
                            )
                            .w(REWARD_SELECTION_ASSET_WIDTH)
                            .flex_none()
                            .truncate(),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .child(app_strong_text(row.amount).whitespace_nowrap())
                                .child(
                                    app_muted_text(match row.usd {
                                        RewardUsdState::Value(value) => {
                                            format_usd_micro_value(value)
                                        }
                                        RewardUsdState::Loading | RewardUsdState::Unavailable => {
                                            "USD unavailable".to_owned()
                                        }
                                    })
                                    .text_size(px(11.0))
                                    .whitespace_nowrap(),
                                ),
                        )
                        .child(fee_cell)
                        .child(
                            div()
                                .w(REWARD_SELECTION_NET_WIDTH)
                                .flex_none()
                                .flex()
                                .justify_end()
                                .child(net_cell),
                        ),
                )
                .when(below_fee, |row| {
                    row.child(
                        app_muted_text(
                            "Added fee exceeds the reward. Rewards keep accruing and can be claimed later.",
                        )
                        .text_size(px(11.0))
                        .whitespace_normal(),
                    )
                }),
        );
    }
    let split_notice = estimated.filter(|estimate| estimate.steps.len() > 1).map(|estimate| {
        let intervals = estimate
            .evidence
            .as_ref()
            .and_then(|evidence| evidence.ending_interval.checked_sub(evidence.starting_interval))
            .and_then(|span| span.checked_add(U256::ONE));
        Alert::warning(
            "governance-reward-selection-split",
            intervals.map_or_else(
                || {
                    format!(
                        "This claim does not fit one block. It will be sent as {} transactions, each needing its own signature.",
                        estimate.steps.len()
                    )
                },
                |intervals| {
                    format!(
                        "This claim covers {intervals} intervals and does not fit one block. It will be sent as {} transactions, each needing its own signature.",
                        estimate.steps.len()
                    )
                },
            ),
        )
        .small()
    });
    let fee_value = match (estimate, selected_fee_per_gas, selected_cost) {
        _ if tokens.is_empty() => app_muted_text("—").into_any_element(),
        (None, _, _) => app_muted_text("—").into_any_element(),
        (Some(RewardSelectionEstimateState::Pending), _, _) => div()
            .flex()
            .items_center()
            .gap_1()
            .child(Spinner::new().small())
            .child(app_muted_shimmer_text("estimating…"))
            .into_any_element(),
        (Some(RewardSelectionEstimateState::Unavailable(reason)), _, _) => div()
            .flex()
            .flex_col()
            .child(app_text("unavailable"))
            .child(
                app_muted_text(reason.to_string())
                    .text_size(px(11.0))
                    .whitespace_normal(),
            )
            .into_any_element(),
        (Some(RewardSelectionEstimateState::Estimated(estimate)), Some(price), Some(cost)) => div()
            .flex()
            .items_baseline()
            .justify_end()
            .gap_1p5()
            .child(
                app_muted_text(reward_selection_gas_summary(&estimate.steps, price))
                    .text_size(px(11.0))
                    .whitespace_nowrap(),
            )
            .child(
                app_text(format!(
                    "≈ {}",
                    fee_usd.map_or_else(
                        || super::tokens::format_native_token_amount_for_display(chain_id, cost),
                        format_usd_micro_value
                    )
                ))
                .whitespace_nowrap(),
            )
            .into_any_element(),
        (Some(RewardSelectionEstimateState::Estimated(_)), _, _) => {
            app_muted_text("unavailable").into_any_element()
        }
    };
    let net_value = match (selected_usd, fee_usd) {
        (Some(reward_usd), Some(fee_usd)) => {
            let (label, negative) = reward_net_label(reward_usd, fee_usd);
            app_strong_text(format!("≈ {label}"))
                .when(negative, |value| value.text_color(rgb(theme::DANGER)))
                .into_any_element()
        }
        _ => app_muted_text("—").into_any_element(),
    };
    let shared_fee = estimated
        .zip(selected_fee_per_gas)
        .and_then(|(estimate, price)| {
            estimate.shared_gas.map(|gas| {
                let fee = reward_cost_presentation(
                    chain_id,
                    U256::from(gas) * U256::from(price),
                    RewardUsdState::Unavailable,
                    anchor_cache,
                );
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(app_muted_text(format!("{} gas", compact_gas(gas))).text_size(px(11.0)))
                    .child(app_text(format!("≈ {}", fee.label)))
                    .into_any_element()
            })
        })
        .unwrap_or_else(|| {
            app_muted_text(
                if matches!(estimate, Some(RewardSelectionEstimateState::Pending)) {
                    "Estimating…"
                } else {
                    "Unavailable"
                },
            )
            .into_any_element()
        });
    // Totals read as label left, value right, like the authorization summary rows.
    let totals_value = |value: AnyElement| {
        div()
            .w_full()
            .flex()
            .justify_end()
            .text_right()
            .child(value)
            .into_any_element()
    };
    let review_root = root.clone();
    let cancel_root = root.clone();
    let pending = action_flow.pending;
    let review_ready = !tokens.is_empty() && selected_cost.is_some() && !pending;
    div()
        .w(content_width)
        .flex()
        .flex_col()
        .gap_3()
        .child(list)
        .child(render_eip1559_gas_fee_editor(
            root.clone(),
            &Eip1559GasFeeTarget::Governance,
            gas_fee,
            pending,
        ))
        .children(split_notice)
        .child(
            DescriptionList::horizontal()
                .bordered(false)
                .columns(1)
                .label_width(px(120.0))
                .child(
                    DescriptionItem::new(app_muted_text("Selected").into_any_element())
                        .value(totals_value(app_text(selected_value).into_any_element())),
                )
                .child(
                    DescriptionItem::new(app_muted_text("Shared fee").into_any_element())
                        .value(totals_value(shared_fee)),
                )
                .child(
                    DescriptionItem::new(app_muted_text("Estimated fee").into_any_element())
                        .value(totals_value(fee_value)),
                )
                .separator()
                .child(
                    DescriptionItem::new(app_strong_text("Net").into_any_element())
                        .value(totals_value(net_value)),
                ),
        )
        .children(action_flow.error.as_ref().map(|error| {
            Alert::error("governance-staking-action-error", error.to_string()).small()
        }))
        .child(if pending {
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .py(px(8.0))
                .child(Spinner::new().small())
                .child(
                    app_muted_shimmer_text("Refreshing rewards and estimating the exact call…")
                        .text_size(px(12.0)),
                )
        } else {
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
                .child(
                    app_button_base("governance-staking-action-review")
                        .primary()
                        .small()
                        .disabled(!review_ready)
                        .child("Review")
                        .on_click(move |_event, window, cx| {
                            review_root.update(cx, |root, cx| {
                                root.review_staking_action(window, cx);
                            });
                        }),
                )
        })
}

fn render_staking_action_form(
    root: &Entity<WalletRoot>,
    wallet: &WalletRoot,
    content_width: gpui::Pixels,
    cx: &App,
) -> Option<gpui::Div> {
    let selection = wallet.governance.action_flow.staking_selection()?;
    let selection_ready = wallet.governance.staking.action_selection_ready(selection);
    if let StakingActionKind::RewardClaimSelected { tokens } = &selection.kind {
        return Some(render_reward_claim_selection_form(
            root,
            wallet,
            selection,
            tokens,
            content_width,
            cx,
        ));
    }
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
                    app_muted_shimmer_text(if selection.kind.is_reward_action() {
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
    let chain_id = wallet.last_evm_chain;
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
    let prepare = move |window: &mut Window, cx: &mut App| {
        review_root.update(cx, |root, cx| root.review_staking_action(window, cx));
    };
    let prepare_on_enter = prepare.clone();
    content = content.on_action(move |_: &gpui_component::dialog::Confirm, window, cx| {
        cx.stop_propagation();
        if ready {
            prepare_on_enter(window, cx);
        }
    });
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
                    .on_click(move |_event, window, cx| prepare(window, cx)),
            ),
    );
    if let Some(error) = wallet.governance.action_flow.error.as_ref() {
        content = content
            .child(Alert::error("governance-staking-action-error", error.to_string()).small());
    }
    Some(content)
}

#[cfg(test)]
mod tests;
