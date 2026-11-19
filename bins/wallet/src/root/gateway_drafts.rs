//! Extension drafts are ephemeral, peer-scoped inputs to the existing Public action owner.
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy::primitives::{Address, U256};
use gpui::{Context, Window};
use gpui_component::WindowExt as _;
use wallet_ops::{
    EvmFeeModel, HttpContext, NativeShieldPayer, NativeShieldPreparation, NativeShieldRoute,
    OperationHttpClientSlot, PublicActionGasFeeMode, PublicActionGasFeeQuote,
    PublicActionGasFeeQuoteBundle, PublicActionGasFeeSelection, PublicActionKind, PublicAssetId,
    PublicBalanceAmount, PublicShieldApprovalMode, PublicShieldTransactionProfile,
    PublicTransactionIntent, RAILGUN_PROTOCOL_FEE_BPS, TokenAnchorRateCache,
    estimate_public_action_gas_cost_with_profile_and_ceiling,
    gateway::{
        GatewayDraftApproval, GatewayDraftCommand, GatewayDraftEstimate,
        GatewayDraftEstimatePayload, GatewayDraftExecution, GatewayDraftFee, GatewayDraftGasQuote,
        GatewayDraftInput, GatewayDraftKind, GatewayDraftPayload, GatewayDraftRecipient,
        GatewayDraftStatus, GatewayDraftView,
    },
    parse_send_amount, prepare_native_shield, prepare_native_shield_max,
    public_shield_protocol_fee_amount, public_shield_recipient,
    quote_public_action_gas_fee_bundle_with_profile, resolve_public_ens_recipient,
    select_native_shield_route,
    settings::{EffectiveChainConfig, EffectiveTokenRegistry},
    vault::{DesktopVaultStore, DesktopViewSession, PublicAccountMetadata, PublicAccountSource},
};

use super::gas_fee::{format_gwei, parse_gwei_to_wei};
use super::public_action::{
    NativeShieldKey, PublicActionFeeDisplay, PublicSendDraft, PublicShieldDraft,
    authorized_public_action_gas_fee_selection, bind_native_shield_operation, format_gas_limit,
    native_shield_gas_cost_projection, native_shield_payer, public_action_max_amount_after_reserve,
    public_action_protocol_fee_label, public_send_authorization_summary,
    public_shield_authorization_summary,
};
use super::public_balances::public_asset_icon_path;
use super::spend_authorization::SpendAuthorizationIntent;
use super::{WalletRoot, format_send_amount_input, public_balance_amount_label};

mod draft_book;
mod draft_commands;
mod private;
mod self_broadcast;
use private::valid_private_input_size;

const ESTIMATE_LIFETIME: Duration = Duration::from_secs(30);
const MAX_DRAFT_PEERS: usize = 8;

#[derive(Default)]
pub(super) struct GatewayDraftBook {
    records: HashMap<String, DraftRecord>,
}

struct DraftRecord {
    view: GatewayDraftView,
    wallet: Arc<DesktopViewSession>,
    wallet_generation: u64,
    prepared: Option<PreparedDraft>,
    context_binding: Option<DraftContextBinding>,
    estimated_at: Option<Instant>,
    estimation: Option<tokio::task::AbortHandle>,
    execution: Option<GatewayDraftExecution>,
    picker: Option<private::PrivateDraftPicker>,
    /// The draft's own operation client, opened by its first account-bearing request.
    operation: Option<OperationHttpClientSlot>,
}

impl DraftRecord {
    fn operation(&mut self, http: &HttpContext) -> OperationHttpClientSlot {
        self.operation
            .get_or_insert_with(|| OperationHttpClientSlot::new(http.clone()))
            .clone()
    }
}

#[derive(Clone)]
enum PreparedDraft {
    Send(Box<PublicSendDraft>),
    Shield(Box<PublicShieldDraft>),
    Private(Box<private::PreparedPrivateDraft>),
    PrivateSelfBroadcast(Box<self_broadcast::PreparedSelfBroadcastDraft>),
}

struct EstimationContext {
    input: GatewayDraftInput,
    account: PublicAccountMetadata,
    native_payer: NativeShieldPayer,
    asset: PublicAssetId,
    symbol: String,
    decimals: u8,
    balance: U256,
    native_balance: U256,
    recipient: Option<Address>,
    effective_chain: EffectiveChainConfig,
    ethereum: Option<EffectiveChainConfig>,
    registry: EffectiveTokenRegistry,
    anchors: Arc<TokenAnchorRateCache>,
    http: HttpContext,
    wallet: Arc<DesktopViewSession>,
    store: Arc<DesktopVaultStore>,
    icon: Option<crate::assets::WalletIconSource>,
}

#[derive(PartialEq, Eq)]
struct DraftContextBinding {
    account: PublicAccountMetadata,
    balance: U256,
    native_balance: U256,
    recipient: Option<Address>,
}
impl EstimationContext {
    fn binding(&self) -> DraftContextBinding {
        DraftContextBinding {
            account: self.account.clone(),
            balance: self.balance,
            native_balance: self.native_balance,
            recipient: self.recipient,
        }
    }
}

fn retire_record(record: &mut DraftRecord) {
    if let Some(job) = record.estimation.take() {
        job.abort();
    }
    if let Some(execution) = &record.execution {
        execution.cancel_review();
    }
}

#[derive(PartialEq, Eq)]
enum DraftGasQuoteIdentity<'a> {
    /// Account, chain, kind, asset and Mimic: the asset selects a Mimic fee route.
    Public(&'a str, u64, GatewayDraftKind, &'a str, bool),
    Private(&'a str, u64),
}

fn quote_identity(input: &GatewayDraftPayload) -> Option<DraftGasQuoteIdentity<'_>> {
    match input {
        GatewayDraftPayload::Public(input) => Some(DraftGasQuoteIdentity::Public(
            input.account.as_str(),
            input.chain_id,
            input.kind,
            input.asset.as_str(),
            input.mimic_railway,
        )),
        GatewayDraftPayload::Private(input) => {
            input
                .delivery
                .broadcaster()
                .is_none()
                .then_some(DraftGasQuoteIdentity::Private(
                    &input.wallet,
                    input.chain_id,
                ))
        }
    }
}

fn valid_input_size(input: &GatewayDraftPayload) -> bool {
    let input = match input {
        GatewayDraftPayload::Public(input) => input,
        GatewayDraftPayload::Private(input) => return valid_private_input_size(input),
    };
    input.account.len() <= 128
        && input.asset.len() <= 128
        && input.amount.len() <= 100
        && input.recipient.len() <= 1024
        && input
            .address_book_entry
            .as_ref()
            .is_none_or(|id| id.len() <= 128)
        && match &input.fee {
            GatewayDraftFee::Custom {
                max_fee_gwei,
                priority_fee_gwei,
            } => max_fee_gwei.len() <= 100 && priority_fee_gwei.len() <= 100,
            _ => true,
        }
}

fn draft_gas_fee(
    fee: &GatewayDraftFee,
    quote: PublicActionGasFeeQuote,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    chain_id: u64,
) -> Result<PublicActionGasFeeSelection, String> {
    let custom = |max_fee_per_gas, max_priority_fee_per_gas| {
        if max_fee_per_gas == 0 || max_priority_fee_per_gas > max_fee_per_gas {
            return Err(
                "Enter a positive maximum fee at least as large as the priority fee".into(),
            );
        }
        Ok(PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        })
    };
    match fee {
        GatewayDraftFee::Normal => authorized_public_action_gas_fee_selection(
            PublicActionGasFeeSelection::Auto,
            Some(quote),
            profile,
            asset,
            chain_id,
        ),
        GatewayDraftFee::Custom {
            max_fee_gwei,
            priority_fee_gwei,
        } => custom(
            parse_gwei_to_wei(max_fee_gwei)?,
            parse_gwei_to_wei(priority_fee_gwei)?,
        ),
        GatewayDraftFee::Slow | GatewayDraftFee::Fast => {
            let numerator = if matches!(fee, GatewayDraftFee::Slow) {
                3
            } else {
                5
            };
            let scale = |value: u128| {
                value
                    .checked_mul(numerator)
                    .map(|value| value / 4)
                    .ok_or_else(|| "Gas fee is too large".to_owned())
            };
            if profile.uses_legacy_envelope(chain_id, asset) {
                return custom(scale(quote.rpc_gas_price)?.max(1), 0);
            }
            let tip = scale(quote.suggested_max_priority_fee_per_gas)?;
            let cap = quote
                .suggested_max_fee_per_gas
                .saturating_sub(quote.suggested_max_priority_fee_per_gas)
                .checked_add(tip)
                .ok_or("Gas fee is too large")?;
            custom(cap, tip)
        }
    }
}

struct DraftEstimation {
    gas_quote: Option<GatewayDraftGasQuote>,
    result: Result<(Option<PreparedDraft>, GatewayDraftEstimate), String>,
}

async fn estimate_draft(
    mut context: EstimationContext,
    operation: OperationHttpClientSlot,
) -> DraftEstimation {
    // Resolve first so a slow ENS lookup cannot age the subsequently captured quote.
    // A resolution failure still permits the gas hint to be returned for editing.
    let recipient_error = if context.input.kind == GatewayDraftKind::Send
        && context.recipient.is_none()
        && !context.input.recipient.trim().is_empty()
    {
        if let Some(ethereum) = context.ethereum.as_ref()
            && let Ok(client) = operation.client().await
            && let Ok(recipient) = resolve_public_ens_recipient(
                context.input.recipient.trim(),
                ethereum,
                &context.http,
                &client,
            )
            .await
        {
            context.recipient = Some(recipient);
            None
        } else {
            Some("Recipient could not be resolved. Check the address or ENS name.".to_owned())
        }
    } else {
        None
    };
    let profile = if context.input.kind == GatewayDraftKind::Shield && context.input.mimic_railway {
        PublicShieldTransactionProfile::Railway
    } else {
        PublicShieldTransactionProfile::Railoxide
    };
    let Ok(bundle) = quote_public_action_gas_fee_bundle_with_profile(
        context.input.chain_id,
        &context.effective_chain,
        profile,
        context.asset,
        &context.http,
    )
    .await
    else {
        return DraftEstimation {
            gas_quote: None,
            result: Err(
                "Fee estimate unavailable. Check the desktop connection and try again.".into(),
            ),
        };
    };
    // A gas price hint does not depend on a valid amount, recipient, or custom fee.
    // Keep it separate from the prepared transaction and its submission admission.
    let gas_quote = Some(GatewayDraftGasQuote::new(
        format_gwei(bundle.standard.suggested_max_fee_per_gas),
        format_gwei(bundle.standard.suggested_max_priority_fee_per_gas),
    ));
    let native_preparation = if context.input.kind == GatewayDraftKind::Shield
        && context.asset == PublicAssetId::Native
    {
        let prepared = match draft_native_shield_route(
            context.input.chain_id,
            &context.effective_chain,
            context.native_payer,
            profile,
            bundle.standard.observed_fee_model,
        ) {
            Ok(NativeShieldRoute::Stealth(_)) => {
                prepare_native_shield_draft(&context, &operation, &bundle, profile)
                    .await
                    .map(|preparation| Some(Arc::new(preparation)))
            }
            // The wrap fallback needs no preparation; a draft without one takes it.
            Ok(NativeShieldRoute::WrapFallback { .. }) => Ok(None),
            Err(message) => Err(message),
        };
        match prepared {
            Ok(preparation) => preparation,
            Err(message) => {
                return DraftEstimation {
                    gas_quote,
                    result: Err(message),
                };
            }
        }
    } else {
        None
    };
    DraftEstimation {
        gas_quote,
        result: recipient_error.map_or_else(
            || prepare_draft(context, bundle, profile, native_preparation, operation),
            Err,
        ),
    }
}

/// A native Shield draft's route, chosen as the Shield form chooses it, or why it is refused.
/// Mimic Railway is refused rather than switched to the wrap fallback.
fn draft_native_shield_route(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    payer: NativeShieldPayer,
    profile: PublicShieldTransactionProfile,
    observed_fee_model: Option<EvmFeeModel>,
) -> Result<NativeShieldRoute, String> {
    select_native_shield_route(chain_id, effective_chain, payer, profile, observed_fee_model)
        .map_err(|reason| match profile {
            PublicShieldTransactionProfile::Railway => format!(
                "Mimic Railway is unavailable for this shield: {reason}. Turn off Mimic Railway to shield in the Railoxide profile."
            ),
            PublicShieldTransactionProfile::Railoxide => {
                format!("Native shielding is unavailable: {reason}.")
            }
        })
}

/// Prepares a stealth-route native shield draft in `profile`.
async fn prepare_native_shield_draft(
    context: &EstimationContext,
    operation: &OperationHttpClientSlot,
    bundle: &PublicActionGasFeeQuoteBundle,
    profile: PublicShieldTransactionProfile,
) -> Result<NativeShieldPreparation, String> {
    let chain_id = context.input.chain_id;
    let PublicActionGasFeeSelection::Custom {
        max_fee_per_gas, ..
    } = draft_gas_fee(
        &context.input.fee,
        bundle.standard,
        profile,
        PublicAssetId::Native,
        chain_id,
    )?
    else {
        return Err("Gas fee is unavailable".into());
    };
    let recipient = public_shield_recipient(&context.wallet)
        .map_err(|_| "The private wallet's receive address is unavailable".to_owned())?;
    let client = operation
        .client()
        .await
        .map_err(|error| format!("The native shield could not be prepared: {error}"))?;
    let prepared = if context.input.max {
        prepare_native_shield_max(
            chain_id,
            &context.effective_chain,
            profile,
            context.native_payer,
            context.account.address,
            recipient,
            context.balance,
            max_fee_per_gas,
            &context.http,
            &client,
        )
        .await
        .map_err(|error| format!("Max is unavailable: {error}"))?
    } else {
        let amount = parse_send_amount(&context.input.amount, Some(context.decimals))
            .map_err(|error| error.to_string())?;
        if amount.is_zero() {
            return Err("Amount must be greater than zero".into());
        }
        if amount > context.balance {
            return Err("Amount exceeds the available balance".into());
        }
        prepare_native_shield(
            chain_id,
            &context.effective_chain,
            profile,
            context.native_payer,
            context.account.address,
            recipient,
            amount,
            &context.http,
            &client,
        )
        .await
        .map_err(|error| format!("The native shield could not be prepared: {error}"))?
    };
    Ok(prepared)
}

fn prepare_draft(
    context: EstimationContext,
    bundle: PublicActionGasFeeQuoteBundle,
    profile: PublicShieldTransactionProfile,
    native_preparation: Option<Arc<NativeShieldPreparation>>,
    operation: OperationHttpClientSlot,
) -> Result<(Option<PreparedDraft>, GatewayDraftEstimate), String> {
    let EstimationContext {
        input,
        account,
        native_payer: _,
        asset,
        symbol,
        decimals,
        balance,
        native_balance,
        recipient,
        effective_chain,
        ethereum: _,
        registry,
        anchors,
        http: _,
        wallet,
        store,
        icon,
    } = context;
    let gas_fee = draft_gas_fee(&input.fee, bundle.standard, profile, asset, input.chain_id)?;
    let gas_fee_mode = if matches!(input.fee, GatewayDraftFee::Normal) {
        PublicActionGasFeeMode::Auto
    } else {
        PublicActionGasFeeMode::Custom
    };
    let ceiling = if input.kind == GatewayDraftKind::Shield
        && profile == PublicShieldTransactionProfile::Railway
        && matches!(asset, PublicAssetId::Erc20(_))
        && gas_fee_mode == PublicActionGasFeeMode::Auto
    {
        bundle.authorization_ceiling
    } else {
        gas_fee
    };
    let unestimable = || "This action cannot be estimated on the selected network.".to_owned();
    let costs = match &native_preparation {
        // A stealth-route native shield is priced from its own prepared raw and buffered gas.
        Some(preparation) => native_shield_gas_cost_projection(
            input.chain_id,
            profile,
            preparation.raw_gas_limit(),
            preparation.gas_limit(),
            gas_fee,
            Some(bundle.standard),
        )
        .ok_or_else(unestimable)?,
        None => estimate_public_action_gas_cost_with_profile_and_ceiling(
            input.chain_id,
            &effective_chain,
            if input.kind == GatewayDraftKind::Shield {
                PublicActionKind::Shield
            } else {
                PublicActionKind::Send
            },
            asset,
            profile,
            gas_fee,
            Some(bundle.standard),
            Some(ceiling),
        )
        .map_err(|_| unestimable())?,
    };
    let amount = match &native_preparation {
        Some(preparation) => checked_draft_amount(
            preparation.amount(),
            asset,
            balance,
            native_balance,
            costs.maximum_cost,
        )?,
        None => draft_amount(
            &input,
            asset,
            decimals,
            balance,
            native_balance,
            costs.maximum_cost,
        )?,
    };
    let fee_display = PublicActionFeeDisplay::from_estimate(
        Some(&effective_chain),
        Some(costs),
        native_preparation
            .as_ref()
            .map(|preparation| preparation.gas_limit()),
        (input.kind == GatewayDraftKind::Shield)
            .then(|| (asset, public_shield_protocol_fee_amount(amount))),
        &registry,
        &anchors,
    );
    let account_label = account
        .label
        .clone()
        .unwrap_or_else(|| railgun_ui::short_address(&account.address));
    let estimate = GatewayDraftEstimate {
        amount: format_send_amount_input(amount, Some(decimals)),
        amount_label: format!(
            "{} {symbol}",
            public_balance_amount_label(&PublicBalanceAmount::Available(amount), decimals)
        ),
        amount_value: match asset {
            PublicAssetId::Native => anchors.cached_native_usd_micro_value(input.chain_id, amount),
            PublicAssetId::Erc20(token) => {
                anchors.cached_token_usd_micro_value(input.chain_id, token, amount)
            }
        }
        .map(railgun_ui::format_usd_micro_value),
        max_amount_label: (asset == PublicAssetId::Native)
            .then(|| public_action_max_amount_after_reserve(balance, costs.maximum_cost))
            .flatten()
            .map(|amount| {
                format!(
                    "{} {symbol} after est. gas",
                    public_balance_amount_label(&PublicBalanceAmount::Available(amount), decimals)
                )
            }),
        recipient: recipient.map(|address| address.to_checksum(None)),
        gas_limit: fee_display.gas_limit.map(format_gas_limit),
        expected_gas_cost: fee_display.expected_gas_cost.clone().unwrap_or_default(),
        maximum_gas_cost: fee_display.maximum_gas_cost.clone().unwrap_or_default(),
        show_maximum_gas_cost: fee_display.show_maximum_gas_cost,
        protocol_fee: fee_display.protocol_fee.clone(),
        protocol_fee_label: fee_display
            .protocol_fee
            .as_ref()
            .map(|_| public_action_protocol_fee_label(RAILGUN_PROTOCOL_FEE_BPS)),
    };
    // Fee/Max presentation is useful before a destination is chosen. It never admits
    // submission: only a fully prepared draft can become Ready.
    if input.kind == GatewayDraftKind::Send && recipient.is_none() {
        return Ok((None, estimate));
    }
    let prepared = match input.kind {
        GatewayDraftKind::Send => {
            let recipient = recipient.ok_or("Enter a recipient")?;
            PreparedDraft::Send(Box::new(PublicSendDraft {
                chain_id: input.chain_id,
                asset,
                asset_label: symbol,
                asset_icon_path: icon,
                asset_decimals: Some(decimals),
                public_account_uuid: account.public_account_uuid.into(),
                public_account_label: account_label,
                public_account_source: account.source,
                view_session: wallet,
                vault_store: store,
                amount,
                recipient,
                intent: PublicTransactionIntent::Transfer {
                    asset,
                    amount,
                    recipient,
                },
                advanced_estimate: None,
                gas_fee,
                fee_display,
                gateway_execution: None,
                operation,
            }))
        }
        GatewayDraftKind::Shield => PreparedDraft::Shield(Box::new(PublicShieldDraft {
            chain_id: input.chain_id,
            asset,
            asset_label: symbol,
            asset_icon_path: icon,
            asset_decimals: Some(decimals),
            public_account_uuid: account.public_account_uuid.into(),
            public_account_label: account_label,
            public_account_source: account.source,
            view_session: wallet,
            vault_store: store,
            amount,
            profile,
            approval_mode: draft_approval_mode(profile, input.approval),
            native_preparation,
            native_operation: None,
            gas_fee,
            gas_fee_mode,
            authorized_fee_ceiling: ceiling,
            fee_display,
            gateway_execution: None,
            operation,
        })),
    };
    Ok((Some(prepared), estimate))
}

/// The approval mode a Shield draft signs with: the extension's choice, or Mimic Railway's own.
const fn draft_approval_mode(
    profile: PublicShieldTransactionProfile,
    approval: GatewayDraftApproval,
) -> PublicShieldApprovalMode {
    profile.approval_mode(match approval {
        GatewayDraftApproval::Exact => PublicShieldApprovalMode::Exact,
        GatewayDraftApproval::Unlimited => PublicShieldApprovalMode::UnlimitedWhenShort,
    })
}

/// The stealth operation a Shield draft binds before authorization: one when it has a
/// stealth-route native preparation, none for the wrap fallback or an ERC-20 shield.
fn bind_draft_native_operation(
    bound: &mut Option<(NativeShieldKey, wallet_ops::vault::ExecutorOperationId)>,
    native_prepared: bool,
    key: NativeShieldKey,
) -> Result<Option<wallet_ops::vault::ExecutorOperationId>, wallet_ops::vault::ExecutorStoreError> {
    if !native_prepared {
        return Ok(None);
    }
    bind_native_shield_operation(bound, key).map(Some)
}

fn draft_amount(
    input: &GatewayDraftInput,
    asset: PublicAssetId,
    decimals: u8,
    balance: U256,
    native_balance: U256,
    maximum_cost: U256,
) -> Result<U256, String> {
    let amount = if input.max {
        if asset == PublicAssetId::Native {
            balance
                .checked_sub(maximum_cost)
                .ok_or("Not enough native balance for gas")?
        } else {
            balance
        }
    } else {
        parse_send_amount(&input.amount, Some(decimals)).map_err(|error| error.to_string())?
    };
    checked_draft_amount(amount, asset, balance, native_balance, maximum_cost)
}

fn checked_draft_amount(
    amount: U256,
    asset: PublicAssetId,
    balance: U256,
    native_balance: U256,
    maximum_cost: U256,
) -> Result<U256, String> {
    if amount.is_zero() {
        return Err("Amount must be greater than zero".into());
    }
    if amount > balance {
        return Err("Amount exceeds the available balance".into());
    }
    let needed_native = if asset == PublicAssetId::Native {
        amount
            .checked_add(maximum_cost)
            .ok_or("Amount is too large")?
    } else {
        maximum_cost
    };
    if needed_native > native_balance {
        return Err("Not enough native balance for the maximum gas cost".into());
    }
    Ok(amount)
}

#[cfg(test)]
mod tests;
