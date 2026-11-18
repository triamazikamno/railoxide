//! Public-paid swaps. The origin is public; every private capability and persisted use belongs
//! to the independently selected destination network.

use super::*;
use crate::root::private_swap::public_progress::PublicSwapAction;
use crate::root::private_swap::ui_fixture;
use crate::root::private_swap::{local_date_time_label, local_time_label};
use crate::root::spend_authorization::SpendAuthorizationIntent;
use crate::root::ui_helpers::{dialog_footer, now_unix};
use crate::root::vault::hardware_device_label;
use alloy::primitives::B256;
use ui::dialog::WalletDialogExt as _;
use wallet_ops::{
    AcrossPrivateDelivery, AuthorizedPublicSwapSource, PublicActionGasFeeSelection,
    PublicSwapBatchTerms, PublicSwapDelivery, PublicSwapDeliverySigning, PublicSwapOrderOutcome,
    PublicSwapOrderRequest, PublicSwapPermitTerms, PublicSwapReview, PublicSwapReviewRequest,
    PublicSwapSource, PublicSwapTracking, PublicSwapUseClaim, PublicSwapWithdrawalReview,
    SwapSetupStatus,
    bridge::{
        AcrossClient, AcrossRoute, PublicBridgeDestination, PublicSellAsset,
        public_across_destination_tokens,
    },
    cow::OrderLimit,
    new_public_swap_batch_nonce, quote_public_action_gas_fee,
    vault::{
        AcrossOrderTerms, ExecutorStoreError, PublicAccountMetadata, PublicAccountScope,
        PublicAccountSource, PublicAccountStatus, PublicSwapApproval, PublicSwapSourceTerms,
        SwapUseRole,
    },
};

mod public_fixtures;
mod public_jobs;
mod public_labels;
mod public_quotes;
mod public_render;
mod public_review;
mod public_routes;
mod public_signing;
mod public_tracking;

/// The hardware review's title: the step whose signatures it decodes.
const DEVICE_SIGNATURES_TITLE: &str = "Approve and place order";

#[derive(Clone)]
struct PublicSwapClients {
    across: AcrossClient,
    orderbook: Option<CowOrderbookClient>,
    /// The form's route, which both providers' clients are on, for its RPC reads.
    http: wallet_ops::OperationHttpClient,
}

pub(super) struct PublicSwapForm {
    pub(super) account: PublicAccountMetadata,
    pub(super) routes: HashMap<(Address, u64), Vec<PublicBridgeDestination>>,
    pub(super) route_errors: HashMap<(Address, u64), String>,
    pub(super) route_tasks: HashMap<(Address, u64), Task<()>>,
    /// What Across answered for each entry of `routes`, with the sell asset it was asked for:
    /// what `routes` is listed from again once the chain is known to take orders.
    across_routes: HashMap<(Address, u64), (PublicSellAsset, Vec<AcrossRoute>)>,
    /// Whether the account's chain takes orders, read once for this form: its math contract is
    /// deployed there. `None` until the read answers, which lists what a deposit serves, as a
    /// failed read does.
    pub(super) orders_available: Option<bool>,
    orders_task: Option<Task<()>>,
    pub(super) review: Option<Arc<PublicSwapReview>>,
    pub(super) operation: Option<ExecutorOperationId>,
    pub(super) swap_use: Option<SwapUseId>,
    clients: Option<PublicSwapClients>,
    quote_terms: Option<PublicQuoteTerms>,
    /// The open swap that keeps this draft's pair from being reviewed: what the destination
    /// owner's source check names, or what a claim was refused for.
    conflict: Option<PublicSwapConflict>,
    /// The form's error that names the account's `CoW` proxy, and the proxy, to copy while that
    /// error shows.
    proxy_holds: Option<(String, Address)>,
    /// What changed between the last review and its signing, which the next review says.
    review_change: Option<SwapReviewChange>,
    /// The signing-time delivery quote is retained until the draft terms change.
    delivery_review: bool,
}

/// What the gas strip shows of an order paid from a Public account: its limit at the form's
/// gas share, priced from the reviewed quote until the bridge leg is previewed again.
#[derive(Clone, Copy)]
pub(super) struct PublicStrip {
    /// The order's limit at the shown share, in the bought token on the network it is paid on.
    pub(super) limit: OrderLimit,
    /// The shown share: the form's, or the review's when the quote can't support the form's.
    pub(super) share_bps: u16,
    /// The share the review's bridge leg was previewed at.
    reviewed_bps: u16,
    /// From the order's bought token to the destination token, at the review's ratio of the
    /// minimum received there to the order's buy amount.
    pub(super) scale: StripScale,
    pub(super) bar: GasBar,
}

impl PublicStrip {
    /// The strip shows a share the bridge leg wasn't previewed at.
    pub(super) const fn pending(&self) -> bool {
        self.share_bps != self.reviewed_bps
    }
}

/// The gas strip of the form's order, once a Public account's swap has a review. `None` for a
/// direct deposit, which has no order.
pub(super) fn public_strip(form: &SwapForm) -> Option<PublicStrip> {
    let review = form.public.as_ref()?.review.as_ref()?;
    let reviewed_bps = review.gas_share_bps()?;
    let reviewed = review.order_limit_at(reviewed_bps).ok()?;
    // A share the quote can't support shows the quote as it was priced.
    let (limit, share_bps) = review
        .order_limit_at(form.gas_share_bps)
        .map_or((reviewed, reviewed_bps), |limit| {
            (limit, form.gas_share_bps)
        });
    let scale = if reviewed.min_received.is_zero() {
        StripScale {
            shown: U256::ONE,
            ordered: U256::ONE,
        }
    } else {
        StripScale {
            shown: review.bridge().received_minimum(),
            ordered: reviewed.min_received,
        }
    };
    Some(PublicStrip {
        limit,
        share_bps,
        reviewed_bps,
        scale,
        bar: GasBar::new(limit.best_case, review.slippage_bps(), limit.gas_estimate),
    })
}

/// Another swap paid from the same Public account still trades one of the draft's tokens.
#[derive(Clone, Copy)]
struct PublicSwapConflict {
    /// The draft's pair. The refusal stands until either changes.
    sell: Address,
    buy: Option<Address>,
    /// The open swap.
    swap: SwapUseId,
    /// The token both swaps trade on the network they are paid on.
    token: Address,
    /// The open swap buys the token; otherwise its order sells it.
    buys: bool,
    /// Unix seconds: when the token can be bought again, or when the open order expires.
    until: Option<u64>,
}

/// Why a swap paid from a Public account can't be quoted or reviewed, under the Sell card.
pub(super) struct PublicFormReason {
    pub(super) text: String,
    /// Review stays unavailable. An informational reason leaves the form working.
    pub(super) blocks_review: bool,
    /// The open swap the reason names, for "Open it…".
    open: Option<super::super::model::SwapIdentity>,
    retry_routes: bool,
}

/// One row of the details section.
pub(super) struct PublicDetail {
    pub(super) label: String,
    value: PublicDetailValue,
    /// A cost taken from what the swap delivers or from the Public account.
    indented: bool,
    help: Option<String>,
}

enum PublicDetailValue {
    /// A value, and a muted note after it.
    Text {
        value: String,
        note: Option<String>,
    },
    Price(PriceDelta),
    /// The price tolerance select.
    Tolerance,
}

impl PublicDetail {
    fn cost(label: impl Into<String>, value: String, note: Option<String>, help: String) -> Self {
        Self {
            label: label.into(),
            value: PublicDetailValue::Text { value, note },
            indented: true,
            help: Some(help),
        }
    }

    /// A text row's value and the note after it.
    #[cfg(test)]
    pub(super) fn text_for_test(&self) -> Option<(&str, Option<&str>)> {
        match &self.value {
            PublicDetailValue::Text { value, note } => Some((value, note.as_deref())),
            _ => None,
        }
    }
}

/// One row of a group the hardware review decodes: its label, its value, and the address the
/// value names, shortened.
pub(super) struct PublicSignatureRow {
    pub(super) label: String,
    pub(super) value: String,
    pub(super) address: Option<String>,
}

impl PublicSignatureRow {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            address: None,
        }
    }

    fn naming(mut self, address: Address) -> Self {
        self.address = Some(railgun_ui::short_address(&address));
        self
    }
}

impl PublicSwapForm {
    pub(super) fn invalidate_quote_route(&mut self) {
        self.clients = None;
        self.review = None;
        self.quote_terms = None;
        self.delivery_review = false;
    }

    #[cfg(test)]
    pub(super) const fn quote_route_is_invalidated_for_tests(&self) -> bool {
        self.clients.is_none() && self.review.is_none() && self.quote_terms.is_none()
    }

    /// Whether the account's chain was read to take orders. Not read yet, and a failed read,
    /// are both no.
    const fn takes_orders(&self) -> bool {
        matches!(self.orders_available, Some(true))
    }

    pub(super) fn new(account: PublicAccountMetadata) -> Self {
        Self {
            account,
            routes: HashMap::new(),
            route_errors: HashMap::new(),
            route_tasks: HashMap::new(),
            across_routes: HashMap::new(),
            orders_available: None,
            orders_task: None,
            review: None,
            operation: None,
            swap_use: None,
            clients: None,
            quote_terms: None,
            conflict: None,
            proxy_holds: None,
            review_change: None,
            delivery_review: false,
        }
    }
}

#[derive(Clone)]
struct PublicQuoteTerms {
    source: PublicAccountMetadata,
    origin: EffectiveChainConfig,
    network: u64,
    sell: Address,
    buy: Address,
    amount: U256,
    slippage_bps: u32,
    gas_share_bps: u16,
    shield_failure: BridgeShieldFailure,
}

impl PublicQuoteTerms {
    fn matches(&self, form: &SwapForm, origin: &EffectiveChainConfig, amount: U256) -> bool {
        form.public
            .as_ref()
            .is_some_and(|public| public.account == self.source)
            && origin == &self.origin
            && form.network == Some(self.network)
            && form.sell == self.sell
            && form.buy == Some(self.buy)
            && amount == self.amount
            && form.slippage_bps == self.slippage_bps
            && form.gas_share_bps == self.gas_share_bps
            && form.bridge.shield_failure == self.shield_failure
    }
}

/// An immutable review, bound to the source metadata and the destination's actual session.
pub(in crate::root) struct PublicSwapAuthorization {
    source: PublicAccountMetadata,
    origin: EffectiveChainConfig,
    destination_session: Arc<WalletSession>,
    owner: Arc<ExecutorOwner>,
    operation: ExecutorOperationId,
    swap_use: SwapUseId,
    action: PublicSwapCommand,
    summary: SpendAuthorizationSummary,
    revision: u64,
    /// The route of the command's reads and transactions: the form's, or the recovery
    /// review's.
    client: wallet_ops::OperationHttpClient,
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum PublicSwapCommand {
    Swap {
        account: SwapAccountChoice,
        destination_token: Address,
        candidate: Option<PublicBroadcasterCandidate>,
        review: Arc<PublicSwapReview>,
        quote_terms: PublicQuoteTerms,
        approval: PublicSwapApproval,
        clients: PublicSwapClients,
        waku: Option<PublicBroadcasterWaku>,
        /// What prices the bridge leg again when signing returns the swap to review.
        anchors: Arc<TokenAnchorRateCache>,
        registry: EffectiveTokenRegistry,
    },
    Withdraw {
        review: PublicSwapWithdrawalReview,
        fee: PublicActionGasFeeSelection,
    },
    Cancel {
        fee: PublicActionGasFeeSelection,
        gas_limit: u64,
        orderbook: CowOrderbookClient,
    },
}

impl PublicSwapAuthorization {
    const fn identity(&self) -> super::super::model::SwapIdentity {
        super::super::model::SwapIdentity {
            operation: self.operation,
            swap_use: self.swap_use,
        }
    }

    pub(in crate::root) const fn source(&self) -> &PublicAccountMetadata {
        &self.source
    }
    pub(in crate::root) fn public_authorization_summary(&self) -> SpendAuthorizationSummary {
        self.summary.clone()
    }
    pub(in crate::root) const fn hardware_executor_action(
        &self,
    ) -> wallet_ops::HardwareExecutorAction {
        wallet_ops::HardwareExecutorAction::Execute(self.operation)
    }
    #[cfg(feature = "hardware")]
    pub(in crate::root) fn hardware_executor_chain(&self) -> u64 {
        self.destination_session.chain_id
    }
    pub(in crate::root) fn sessions_are_current(&self, root: &WalletRoot) -> bool {
        root.view_session.is_some()
            && root
                .public_accounts
                .iter()
                .any(|account| account == &self.source)
            && matches!(root.chain_states.get(&self.destination_session.chain_id),
                Some(ChainUtxoState::Ready { session, .. } | ChainUtxoState::Syncing { session, .. })
                if Arc::ptr_eq(session, &self.destination_session))
            && root
                .effective_chain_configs
                .get(self.origin.chain_id)
                .is_some_and(|chain| chain == &self.origin)
    }
}

pub(in crate::root::private_swap) struct PublicSwapExecution {
    command: Arc<PublicSwapAuthorization>,
    private_authorization: Option<DesktopPrivateSpendAuthorization>,
    signer: AuthorizedPublicSwapSource,
    signed: Option<PublicSwapSigned>,
}

struct PublicSwapSigned {
    delivery: AcrossPrivateDelivery,
    terms: AcrossOrderTerms,
    valid_to: u32,
    nonce: B256,
    batch: Option<PublicSwapBatchTerms>,
    /// What the order's signed approval grants, for an order that carries one.
    permit: Option<PublicSwapPermitTerms>,
}

async fn public_swap_clients(
    origin: &EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
) -> eyre::Result<PublicSwapClients> {
    let bridge = origin
        .bridge_origin_profile()
        .ok_or_else(|| eyre::eyre!("Swaps aren't available from this network."))?;
    let route = http.operation_http_client().await?;
    let across = AcrossClient::new(route.clone(), bridge.across_api_base().parse()?)?;
    let orderbook = match origin.public_swap_profile() {
        Some(profile) => Some(CowOrderbookClient::new(
            route.clone(),
            profile.orderbook_api_base().parse()?,
            origin.chain_id,
        )?),
        None => None,
    };
    Ok(PublicSwapClients {
        across,
        orderbook,
        http: route,
    })
}

fn public_sell(origin: &EffectiveChainConfig, token: Address) -> eyre::Result<PublicSellAsset> {
    if token == Address::ZERO {
        Ok(PublicSellAsset::Native {
            wrapped: origin.wrapped_native_token.ok_or_else(|| {
                eyre::eyre!("This network has no configured wrapped native token.")
            })?,
        })
    } else {
        Ok(PublicSellAsset::Erc20(token))
    }
}

const fn public_gas_fee(review: &PublicSwapReview) -> PublicActionGasFeeSelection {
    PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: review.gas_plan().max_fee_per_gas,
        max_priority_fee_per_gas: review.gas_plan().max_priority_fee_per_gas,
    }
}

impl PublicSwapConflict {
    /// The open swap a backend refusal names, for the draft `form` that `command` reviewed.
    fn of(
        error: &eyre::Report,
        command: &PublicSwapAuthorization,
        form: &SwapForm,
    ) -> Option<Self> {
        let PublicSwapCommand::Swap {
            review, approval, ..
        } = &command.action
        else {
            return None;
        };
        Self::naming(
            error.downcast_ref::<ExecutorStoreError>()?,
            form,
            approval.sell_token,
            review.intent().bridged_token,
        )
    }

    /// The open swap `refusal` names, for the draft `form` that sells `sell_token` and buys
    /// `bridged` on the network it is paid on.
    const fn naming(
        refusal: &ExecutorStoreError,
        form: &SwapForm,
        sell_token: Address,
        bridged: Address,
    ) -> Option<Self> {
        let (swap, token, buys, until) = match refusal {
            ExecutorStoreError::PublicSwapBuysSameToken {
                swap, available_at, ..
            } => (*swap, bridged, true, *available_at),
            ExecutorStoreError::PublicSwapSellsSameToken {
                swap, expires_at, ..
            } => (*swap, sell_token, false, *expires_at),
            _ => return None,
        };
        Some(Self {
            sell: form.sell,
            buy: form.buy,
            swap,
            token,
            buys,
            until,
        })
    }
}

/// The route of the form's pair, once its network's routes are read.
/// A failed quote of a swap paid from a Public account, in the form's words. An amount that
/// the bridge or the shield's gas on `network` rules out says so and what to do, without
/// details. Any other failure reads as the private form's does, with `cost` formatting its
/// estimated cost.
pub(super) fn public_quote_failure(
    error: &eyre::Report,
    network: Option<u64>,
    cost: impl FnOnce(Address, U256) -> String,
) -> QuoteFailure {
    let there = network.map_or_else(|| "the destination network".to_owned(), network_name);
    if matches!(
        error.downcast_ref::<OrderLimitError>(),
        Some(OrderLimitError::DeliveryAllowanceExceedsOutput { .. })
    ) {
        return QuoteFailure {
            message: format!(
                "This amount is too small to bridge to your private balance on {there}. The shield's gas there costs more than the amount. Try a larger amount."
            ),
            details: None,
        };
    }
    if matches!(
        error.downcast_ref::<BridgeApiError>(),
        Some(BridgeApiError::AmountTooLow)
    ) {
        return QuoteFailure {
            message: format!(
                "This amount is too small for Across to bridge to {there}. Try a larger amount."
            ),
            details: None,
        };
    }
    map_quote_failure(error, cost)
}

fn public_route(form: &SwapForm) -> Option<&PublicBridgeDestination> {
    let buy = form.buy?;
    form.public
        .as_ref()?
        .routes
        .get(&(form.sell, form.network?))?
        .iter()
        .find(|route| route.destination.destination_token == buy)
}

/// The bounds `review` binds, whichever account it is approved for: its gas share,
/// estimate, allowance and price, and its validity.
fn public_bounds(review: &PublicSwapReview) -> Option<SwapApprovedBounds> {
    review
        .approval(
            SwapApprovedAccount {
                address: None,
                setup: false,
            },
            None,
            true,
        )
        .ok()
        .map(|approval| approval.bounds)
}

/// The warning on a swap `bps` of which may go to costs.
fn public_cost_warning(bps: u64) -> AuthorizedCostWarning {
    AuthorizedCostWarning {
        headline: format!("{}% of this swap may go to costs.", bps / 100),
        details: "The received minimum accounts for bridge, destination gas and shield costs. Source gas and any destination setup fee are paid separately.".into(),
    }
}

/// "Sign on Ledger…": the hardware review's button, which opens the device's prompts.
pub(super) fn public_sign_label(device: &str) -> String {
    format!("Sign on {device}…")
}

enum PublicExecutionResult {
    /// The swap's record exists, and nothing was prepared or sent for it yet.
    Claimed(PublicSwapExecution),
    Waiting(PublicSwapExecution),
    Ready(PublicSwapExecution),
    Changed {
        change: SwapReviewChange,
        review: Option<Arc<PublicSwapReview>>,
    },
    /// The account's `CoW` proxy holds the bought token, or its balance there couldn't be read:
    /// no order was placed.
    ProxyHolds {
        proxy: Address,
        balance: Option<U256>,
    },
    Finished(super::super::model::SwapIdentity),
}

/// Debug UI fixture: the scripted answer that stands in for a real job of `execution`.
#[cfg(debug_assertions)]
async fn ui_fixture_job(
    execution: PublicSwapExecution,
    (delay, step): (std::time::Duration, ui_fixture::Step),
) -> eyre::Result<PublicExecutionResult> {
    use ui_fixture::Step;
    tokio::time::sleep(delay).await;
    Ok(match step {
        Step::Hold => std::future::pending().await,
        Step::Claimed => PublicExecutionResult::Claimed(execution),
        Step::Waiting => PublicExecutionResult::Waiting(execution),
        Step::Ready => PublicExecutionResult::Ready(execution),
        Step::Signatures => PublicExecutionResult::Ready(ui_fixture_signed(execution)?),
        Step::Changed(change) => PublicExecutionResult::Changed {
            change,
            review: None,
        },
        Step::Fail(message) => return Err(eyre::eyre!(message)),
    })
}

/// Debug UI fixture: `execution` with the signatures its reviewed order would ask for, staged
/// from the review alone. The delivery and its terms are stand-ins that are never sent.
#[cfg(debug_assertions)]
fn ui_fixture_signed(mut execution: PublicSwapExecution) -> eyre::Result<PublicSwapExecution> {
    let command = &execution.command;
    let PublicSwapCommand::Swap {
        destination_token,
        review,
        approval,
        ..
    } = &command.action
    else {
        return Err(eyre::eyre!("Fixture: this action signs no order."));
    };
    let (Some(proxy), Some(bought)) = (review.proxy(), review.buy_amount()) else {
        return Err(eyre::eyre!(
            "Fixture: a deposit has no signatures to review. Sell a token that needs an order."
        ));
    };
    let valid_to = u32::try_from(
        now_unix().saturating_add(u64::from(approval.bounds.valid_for_secs.unwrap_or(600))),
    )?;
    let bridged = review.intent().bridged_token;
    let destination_chain = command.destination_session.chain_id;
    let permit = review
        .signs_permit()
        .then(|| command.origin.public_swap_profile())
        .flatten()
        .map(|profile| PublicSwapPermitTerms {
            token: approval.sell_token,
            spender: profile.vault_relayer(),
            amount: review.sell_amount(),
            deadline: valid_to,
        });
    execution.signed = Some(PublicSwapSigned {
        delivery: AcrossPrivateDelivery {
            handler: Address::ZERO,
            destination_executor: Address::ZERO,
            shield_multicall: alloy::primitives::Bytes::new(),
            fallback: None,
        },
        terms: AcrossOrderTerms {
            spoke_pool: Address::ZERO,
            input_token: bridged,
            output_token: *destination_token,
            input_amount: bought,
            output_amount: review.bridge().destination_minimum,
            quote_timestamp: 0,
            fill_deadline: valid_to,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
            recipient: None,
            message_hash: None,
        },
        valid_to,
        nonce: B256::ZERO,
        batch: Some(PublicSwapBatchTerms {
            proxy,
            guard_token: bridged,
            guard_amount: bought,
            depositor: command.source.address,
            recipient: Address::ZERO,
            input_token: bridged,
            output_token: *destination_token,
            destination_chain,
            scale_numerator: review.bridge().destination_minimum,
            scale_denominator: bought,
            deadline: valid_to,
            nonce: B256::ZERO,
        }),
        permit,
    });
    Ok(execution)
}

async fn prepare_public_execution(
    execution: PublicSwapExecution,
) -> eyre::Result<PublicExecutionResult> {
    // Debug UI fixture: a scripted answer stands in, and nothing is claimed or sent.
    #[cfg(debug_assertions)]
    {
        if let Some(script) = ui_fixture::step(ui_fixture::Phase::Approved) {
            return ui_fixture_job(execution, script).await;
        }
    }
    let command = &execution.command;
    match &command.action {
        PublicSwapCommand::Swap {
            account,
            destination_token,
            review,
            approval,
            ..
        } => {
            let record = command.owner.claim_public_swap(PublicSwapUseClaim {
                id: command.swap_use,
                origin_chain: command.origin.chain_id,
                source: command.source.address,
                source_scope: command.source.scope.clone(),
                account: *account,
                destination_token: *destination_token,
                intent: review.intent(),
                approval: approval.clone(),
            })?;
            if record
                .public_swap_use(command.swap_use)
                .is_some_and(|(_, saved)| saved.approval() != approval)
            {
                command.owner.reapprove_public_swap(
                    command.operation,
                    command.swap_use,
                    approval.clone(),
                )?;
            }
            Ok(PublicExecutionResult::Claimed(execution))
        }
        PublicSwapCommand::Withdraw { review, fee } => {
            let result = command
                .owner
                .submit_public_swap_withdrawal(
                    command.operation,
                    command.swap_use,
                    &command.origin,
                    &execution.signer,
                    *fee,
                    review,
                    true,
                    &command.client,
                    &mut |_| {},
                )
                .await?;
            if matches!(
                result,
                wallet_ops::PublicSwapTransactionOutcome::Reverted { .. }
            ) {
                return Err(eyre::eyre!(
                    "The withdrawal reverted. Check status before retrying."
                ));
            }
            Ok(PublicExecutionResult::Finished(
                super::super::model::SwapIdentity {
                    operation: command.operation,
                    swap_use: command.swap_use,
                },
            ))
        }
        PublicSwapCommand::Cancel {
            fee,
            gas_limit,
            orderbook,
        } => {
            let result = command
                .owner
                .submit_public_swap_invalidation(
                    command.operation,
                    command.swap_use,
                    &command.origin,
                    &execution.signer,
                    *fee,
                    orderbook,
                    *gas_limit,
                    &command.client,
                    &mut |_| {},
                )
                .await?;
            if matches!(
                result,
                wallet_ops::PublicSwapTransactionOutcome::Reverted { .. }
            ) {
                return Err(eyre::eyre!(
                    "The order cancellation reverted. Check status before retrying."
                ));
            }
            Ok(PublicExecutionResult::Finished(
                super::super::model::SwapIdentity {
                    operation: command.operation,
                    swap_use: command.swap_use,
                },
            ))
        }
    }
}

/// What follows a swap's claim: its destination account is prepared and, when that gives a
/// setup, the setup is sent. The swap then waits for its destination.
async fn prepare_public_destination(
    execution: PublicSwapExecution,
) -> eyre::Result<PublicExecutionResult> {
    // Debug UI fixture: a scripted answer stands in, and nothing is prepared or sent.
    #[cfg(debug_assertions)]
    {
        if let Some(script) = ui_fixture::step(ui_fixture::Phase::Claimed) {
            return ui_fixture_job(execution, script).await;
        }
    }
    let command = &execution.command;
    let PublicSwapCommand::Swap {
        account,
        candidate,
        approval,
        waku,
        ..
    } = &command.action
    else {
        return Err(eyre::eyre!("This action has no destination delivery."));
    };
    // A resumed fresh account whose setup is pending or confirmed is not prepared
    // or sent again. Its retained use continues from canonical setup observation.
    if candidate.is_some() || matches!(account, SwapAccountChoice::Existing(_)) {
        let side = command
            .owner
            .prepare_public_swap_destination(
                command.operation,
                command.swap_use,
                candidate.clone(),
                execution
                    .private_authorization
                    .as_ref()
                    .expect("destination authorization"),
                &command.client,
            )
            .await?;
        if let SwapPairSide::Setup(prepared) = side {
            let outcome = command
                .owner
                .submit_swap_setup(
                    &prepared,
                    SwapSetupRequest {
                        maximum_private_fee: approval
                            .bounds
                            .destination_setup_fee
                            .map(|limit| limit.amount)
                            .ok_or_else(|| eyre::eyre!("The review has no setup fee ceiling."))?,
                        session: command.destination_session.clone(),
                        authorization: execution
                            .private_authorization
                            .as_ref()
                            .expect("destination authorization"),
                        waku: waku
                            .clone()
                            .ok_or_else(|| eyre::eyre!("The broadcaster network isn't ready."))?,
                        verify_proof: true,
                        progress_tx: None,
                        route_tx: None,
                        response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                        republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                        client: command.client.clone(),
                    },
                )
                .await?;
            if let PublicBroadcasterResultKind::Failed { error } = outcome.result {
                return Err(eyre::eyre!(
                    "The broadcaster couldn't submit the destination setup: {error}"
                ));
            }
        }
    }
    Ok(PublicExecutionResult::Waiting(execution))
}

/// How often signing a delivery is tried again after a background read changed the account's
/// record under it, and how long it waits before each try.
const PUBLIC_SIGNING_RETRIES: u32 = 3;
const PUBLIC_SIGNING_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(750);

async fn changed_public_delivery(
    command: &PublicSwapAuthorization,
    review: &PublicSwapReview,
    change: SwapReviewChange,
    quote: Option<&wallet_ops::PublicSwapDeliveryQuote>,
) -> eyre::Result<PublicExecutionResult> {
    let review = match quote {
        Some(quote) => {
            let PublicSwapCommand::Swap {
                anchors, registry, ..
            } = &command.action
            else {
                return Err(eyre::eyre!("This action has no destination delivery."));
            };
            Some(Arc::new(
                command
                    .owner
                    .requote_public_swap_delivery(
                        review,
                        quote,
                        &command.origin,
                        command.source.address,
                        &command.client,
                        Some(anchors),
                        registry,
                    )
                    .await?,
            ))
        }
        None => None,
    };
    Ok(PublicExecutionResult::Changed { change, review })
}

async fn advance_public_execution(
    mut execution: PublicSwapExecution,
    confirmed: u64,
) -> eyre::Result<PublicExecutionResult> {
    // Debug UI fixture: a scripted answer stands in, and nothing is read, signed or sent.
    #[cfg(debug_assertions)]
    {
        if let Some(script) = ui_fixture::step(ui_fixture::Phase::Setup) {
            return ui_fixture_job(execution, script).await;
        }
    }
    let command = &execution.command;
    let PublicSwapCommand::Swap {
        review,
        approval,
        clients,
        ..
    } = &command.action
    else {
        return Err(eyre::eyre!("This action has no destination delivery."));
    };
    if approval.destination.setup {
        // One read of the destination account at its confirmed block. A setup that is still
        // pending is read again on the next pass.
        match command
            .owner
            .observe_swap_setup(command.operation, &command.client)
            .await?
            .status
        {
            SwapSetupStatus::Pending => return Ok(PublicExecutionResult::Waiting(execution)),
            SwapSetupStatus::MissingDelegation => {
                return Err(eyre::eyre!(
                    "The destination setup failed. Review its retry before continuing."
                ));
            }
            SwapSetupStatus::Delegated(_) => {}
        }
    }
    let notes = Some(command.destination_session.as_ref() as &dyn wallet_ops::SwapShieldNotes);
    let secs = approval.bounds.valid_for_secs.unwrap_or(600);
    let valid_to = || u32::try_from(now_unix().saturating_add(u64::from(secs)));
    let (across, authorization) = (
        &clients.across,
        execution
            .private_authorization
            .as_ref()
            .expect("destination authorization"),
    );
    let input_amount = review.buy_amount().unwrap_or_else(|| review.sell_amount());
    // A background read of the account can update its record while the delivery is signed,
    // most often just after its setup confirmed. Nothing is signed then, so the account is
    // read again and the delivery signed once more.
    let sign = |valid_to: u32| async move {
        let mut retries = 0;
        loop {
            let delegated = command
                .owner
                .delegated_public_swap_destination(
                    command.operation,
                    confirmed,
                    command.swap_use,
                    notes,
                    &command.client,
                )
                .await?;
            match command
                .owner
                .sign_public_swap_delivery(PublicSwapDeliverySigning {
                    operation: command.operation,
                    swap_use: command.swap_use,
                    delegated,
                    origin: &command.origin,
                    across,
                    input_amount,
                    valid_to,
                    authorization,
                    notes,
                    client: &command.client,
                })
                .await
            {
                Err(error)
                    if error.is::<wallet_ops::ExecutorRecordChanged>()
                        && retries < PUBLIC_SIGNING_RETRIES =>
                {
                    retries += 1;
                    tokio::time::sleep(PUBLIC_SIGNING_RETRY_DELAY).await;
                }
                signed => break signed,
            }
        }
    };
    // Across is asked before the account pays for an approval: an amount it won't bridge, a
    // fill that fails in its simulation or changed terms stop the swap with nothing sent. The
    // delivery signed here is not used: its quote is taken again once the approvals confirm.
    if !review.gas_plan().approval_gas_limits.is_empty()
        && let PublicSwapDelivery::ReviewRequired { change, quote } = sign(valid_to()?).await?
    {
        return changed_public_delivery(command, review, change, quote.as_ref()).await;
    }
    if let wallet_ops::PublicSwapApprovalsOutcome::ReviewRequired { transactions } = command
        .owner
        .submit_public_swap_approvals(
            command.operation,
            command.swap_use,
            &command.origin,
            &execution.signer,
            public_gas_fee(review),
            &command.client,
            &mut |_| {},
        )
        .await?
    {
        // Transactions the review didn't price return to it with their gas. A permit it
        // didn't price takes a fresh quote.
        let review = if transactions {
            Some(Arc::new(
                command
                    .owner
                    .requote_public_swap_approvals(
                        review,
                        &command.origin,
                        command.source.address,
                        &command.client,
                    )
                    .await?,
            ))
        } else {
            None
        };
        return Ok(PublicExecutionResult::Changed {
            change: SwapReviewChange::ApprovalPlan,
            review,
        });
    }
    let valid_to = valid_to()?;
    match sign(valid_to).await? {
        PublicSwapDelivery::ReviewRequired { change, quote } => {
            changed_public_delivery(command, review, change, quote.as_ref()).await
        }
        PublicSwapDelivery::Signed {
            delivery, terms, ..
        } => {
            let nonce = new_public_swap_batch_nonce()?;
            let batch = if review.intent().order {
                Some(command.owner.public_swap_order_batch_terms(
                    command.operation,
                    command.swap_use,
                    &command.origin,
                    &delivery,
                    &terms,
                    valid_to,
                    nonce,
                )?)
            } else {
                None
            };
            // `None` for a deposit, and for an order approved without a permit.
            let permit = command.owner.public_swap_order_permit_terms(
                command.operation,
                command.swap_use,
                &command.origin,
                valid_to,
            )?;
            execution.private_authorization = None;
            execution.signed = Some(PublicSwapSigned {
                delivery,
                terms,
                valid_to,
                nonce,
                batch,
                permit,
            });
            Ok(PublicExecutionResult::Ready(execution))
        }
    }
}

async fn submit_public_execution(
    mut execution: PublicSwapExecution,
    hash_fallback_confirmed: bool,
) -> eyre::Result<PublicExecutionResult> {
    // Debug UI fixture: a scripted answer stands in, and nothing is placed or deposited.
    #[cfg(debug_assertions)]
    {
        if let Some(script) = ui_fixture::step(ui_fixture::Phase::Submit) {
            return ui_fixture_job(execution, script).await;
        }
    }
    let signed = execution
        .signed
        .take()
        .ok_or_else(|| eyre::eyre!("Review the signed delivery first."))?;
    let command = &execution.command;
    let PublicSwapCommand::Swap {
        review, clients, ..
    } = &command.action
    else {
        return Err(eyre::eyre!("This action has no signed delivery."));
    };
    if review.intent().order {
        let orderbook = clients
            .orderbook
            .as_ref()
            .ok_or_else(|| eyre::eyre!("The origin network has no orderbook."))?;
        match command
            .owner
            .submit_public_swap_order(PublicSwapOrderRequest {
                operation: command.operation,
                swap_use: command.swap_use,
                origin: &command.origin,
                source: &execution.signer,
                orderbook,
                delivery: &signed.delivery,
                terms: &signed.terms,
                valid_to: signed.valid_to,
                batch_nonce: signed.nonce,
                quote_id: review.quote_id(),
                hash_fallback_confirmed,
            })
            .await?
        {
            PublicSwapOrderOutcome::Submitted { .. } => {}
            PublicSwapOrderOutcome::ProxyHoldsBoughtToken { proxy, balance } => {
                return Ok(PublicExecutionResult::ProxyHolds { proxy, balance });
            }
        }
    } else {
        // A deposit no RPC accepted returns without a receipt, handed off: tracking reads its
        // inclusion, resends it or rules it out.
        let result = command
            .owner
            .submit_public_swap_deposit(
                command.operation,
                command.swap_use,
                &command.origin,
                &execution.signer,
                public_gas_fee(review),
                &signed.delivery,
                &signed.terms,
                &command.client,
                &mut |_| {},
            )
            .await?;
        if matches!(
            result,
            wallet_ops::PublicSwapTransactionOutcome::Reverted { .. }
        ) {
            return Err(eyre::eyre!(
                "The Across deposit reverted. Check status before retrying."
            ));
        }
    }
    Ok(PublicExecutionResult::Finished(
        super::super::model::SwapIdentity {
            operation: command.operation,
            swap_use: command.swap_use,
        },
    ))
}
