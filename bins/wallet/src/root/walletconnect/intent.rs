use alloy::primitives::{Address, U256};
use gpui::{Pixels, px};
use railgun_ui::{chain_icon_asset_path, format_usd_micro_value};
use serde_json::Value;
use wallet_ops::{
    TokenAnchorRateCache, WalletConnectDecodedCallKind, WalletConnectDecodedTransaction,
    WalletConnectEvmTransaction, WalletConnectParsedRequest,
    settings::{EffectiveChainConfig, EffectiveTokenRegistry},
    vault::{PublicAccountMetadata, PublicAddressBookEntry},
};

use crate::assets::WalletIconSource;
use crate::root::{
    format_native_token_amount_for_display, format_token_amount_for_display,
    native_token_display_label, token_display_metadata,
};

use super::{
    WalletConnectRequestUi, fee::WalletConnectFeeStatus,
    helpers::walletconnect_transaction_selector, requests::walletconnect_personal_message_bytes,
};

const PERSONAL_SIGN_PREVIEW_MAX_CHARS: usize = 160;
pub(super) const WALLETCONNECT_CRITICAL_PARTY_FULL_ADDRESS_MIN_WIDTH: Pixels = px(500.0);

#[derive(Clone, Copy)]
pub(super) struct WalletConnectIntentContext<'a> {
    pub(super) chain: &'a EffectiveChainConfig,
    pub(super) token_registry: &'a EffectiveTokenRegistry,
    pub(super) anchor_rates: &'a TokenAnchorRateCache,
    pub(super) public_accounts: &'a [PublicAccountMetadata],
    pub(super) public_address_book: &'a [PublicAddressBookEntry],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectIntentAction {
    NativeTransfer,
    TokenTransfer,
    Approve,
    TransferFrom,
    Wrap,
    Unwrap,
    ContractCall,
    ContractCreation,
    PersonalSign,
    TypedDataSign,
    AccountRequest,
    ChainSwitch,
}

impl WalletConnectIntentAction {
    #[must_use]
    pub(super) const fn verb(self) -> &'static str {
        match self {
            Self::NativeTransfer | Self::TokenTransfer | Self::TransferFrom => "Send",
            Self::Approve => "Allow spending",
            Self::Wrap => "Wrap",
            Self::Unwrap => "Unwrap",
            Self::ContractCall => "Contract call",
            Self::ContractCreation => "Create contract",
            Self::PersonalSign => "Sign message",
            Self::TypedDataSign => "Sign typed data",
            Self::AccountRequest => "Request account access",
            Self::ChainSwitch => "Switch chain",
        }
    }

    #[must_use]
    pub(super) const fn allows_party_connector(self) -> bool {
        matches!(
            self,
            Self::NativeTransfer
                | Self::TokenTransfer
                | Self::TransferFrom
                | Self::Wrap
                | Self::Unwrap
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectAmount {
    KnownToken {
        token: Address,
        raw: U256,
        decimals: u8,
        symbol: String,
        display: String,
        usd: Option<String>,
    },
    Native {
        raw: U256,
        symbol: String,
        display: String,
        usd: Option<String>,
    },
    Unlimited {
        token: Address,
        raw: U256,
        decimals: Option<u8>,
        symbol: Option<String>,
        display: String,
        unrecognised_contract: bool,
    },
    RawToken {
        token: Address,
        raw: U256,
        display: String,
        unrecognised_contract: bool,
        decimals_unknown: bool,
    },
    None,
}

impl WalletConnectAmount {
    #[must_use]
    fn usd(&self) -> Option<&str> {
        match self {
            Self::KnownToken { usd, .. } | Self::Native { usd, .. } => usd.as_deref(),
            Self::Unlimited { .. } | Self::RawToken { .. } | Self::None => None,
        }
    }

    #[must_use]
    fn authorization_label(&self) -> Option<String> {
        match self {
            Self::KnownToken { display, .. } | Self::Native { display, .. } => {
                Some(display.clone())
            }
            Self::Unlimited {
                raw,
                symbol,
                unrecognised_contract,
                ..
            } => Some(match (symbol, *unrecognised_contract) {
                (Some(symbol), false) => format!("Unlimited {symbol}"),
                _ => format!(
                    "Unlimited allowance ({raw} raw token units; unrecognised contract, decimals unknown)"
                ),
            }),
            Self::RawToken {
                raw,
                token,
                unrecognised_contract,
                decimals_unknown,
                ..
            } => Some(if *unrecognised_contract && *decimals_unknown {
                format!("{raw} raw token units ({token}; unrecognised contract, decimals unknown)")
            } else {
                format!("{raw} raw token units ({token})")
            }),
            Self::None => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WalletConnectNativeAmount {
    pub(super) raw: U256,
    pub(super) symbol: String,
    pub(super) display: String,
    pub(super) usd: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectHeroSummary {
    Amount,
    PersonalMessage(WalletConnectPersonalMessageSummary),
    TypedData(WalletConnectTypedDataSummary),
    UndecodedCall { selector: Option<[u8; 4]> },
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WalletConnectPersonalMessageSummary {
    pub(super) bytes: usize,
    pub(super) preview: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WalletConnectTypedDataSummary {
    pub(super) domain_name: Option<String>,
    pub(super) primary_type: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WalletConnectEip2612Permit {
    token: Address,
    spender: Address,
    value: U256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WalletConnectHero {
    pub(super) verb: &'static str,
    pub(super) summary: WalletConnectHeroSummary,
    pub(super) approval_effect: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectPartyRole {
    Sender,
    Recipient,
    Spender,
    Source,
    Caller,
    Contract,
    Creator,
    Signer,
    WrappedNativeContract,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectPartyBadge {
    ThisWallet,
    YourAccount { label: Option<String> },
    AddressBook { label: String },
    NotInAddressBook,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WalletConnectParty {
    pub(super) role: WalletConnectPartyRole,
    pub(super) address: Address,
    pub(super) badge: WalletConnectPartyBadge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectRisk {
    UnlimitedAllowance { spender: Address },
    ForeignTransferSource { source: Address },
    AttachedNativeValue(WalletConnectNativeAmount),
    UndecodedContractCall { selector: Option<[u8; 4]> },
    WouldRevert { reason: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WalletConnectFeeSignificance {
    Normal,
    High,
    ExceedsAmount,
}

impl WalletConnectFeeSignificance {
    pub(super) const fn warning_message(self) -> Option<&'static str> {
        match self {
            Self::Normal => None,
            Self::High => Some("The network fee is high relative to the amount being sent."),
            Self::ExceedsAmount => Some("The network fee is larger than the amount being sent."),
        }
    }
}

pub(super) fn classify_walletconnect_fee_significance(
    expected_fee_usd: Option<U256>,
    moving_value_usd: Option<U256>,
) -> WalletConnectFeeSignificance {
    let (Some(expected_fee_usd), Some(moving_value_usd)) = (expected_fee_usd, moving_value_usd)
    else {
        return WalletConnectFeeSignificance::Normal;
    };
    if moving_value_usd.is_zero() {
        return WalletConnectFeeSignificance::Normal;
    }
    if expected_fee_usd >= moving_value_usd {
        return WalletConnectFeeSignificance::ExceedsAmount;
    }

    let quarter = moving_value_usd / U256::from(4_u8);
    let quarter = if moving_value_usd % U256::from(4_u8) == U256::ZERO {
        quarter
    } else {
        quarter + U256::from(1_u8)
    };
    if expected_fee_usd >= quarter {
        WalletConnectFeeSignificance::High
    } else {
        WalletConnectFeeSignificance::Normal
    }
}

fn walletconnect_amount_usd_micro_value(
    chain_id: u64,
    amount: &WalletConnectAmount,
    anchor_rates: &TokenAnchorRateCache,
) -> Option<U256> {
    match amount {
        WalletConnectAmount::KnownToken { token, raw, .. } => (!raw.is_zero())
            .then_some(())
            .and_then(|()| anchor_rates.cached_token_usd_micro_value(chain_id, *token, *raw)),
        WalletConnectAmount::Native { raw, .. } => (!raw.is_zero())
            .then_some(())
            .and_then(|()| anchor_rates.cached_native_usd_micro_value(chain_id, *raw)),
        WalletConnectAmount::Unlimited { .. }
        | WalletConnectAmount::RawToken { .. }
        | WalletConnectAmount::None => None,
    }
}

pub(super) fn walletconnect_moving_usd_micro_value(
    chain_id: u64,
    action: WalletConnectIntentAction,
    amount: &WalletConnectAmount,
    attached_native: Option<&WalletConnectNativeAmount>,
    anchor_rates: &TokenAnchorRateCache,
) -> Option<U256> {
    let attached_value = |native: &WalletConnectNativeAmount| {
        (!native.raw.is_zero())
            .then_some(())
            .and_then(|()| anchor_rates.cached_native_usd_micro_value(chain_id, native.raw))
            .filter(|value| !value.is_zero())
    };
    match action {
        WalletConnectIntentAction::ContractCall | WalletConnectIntentAction::ContractCreation => {
            attached_native.and_then(attached_value)
        }
        WalletConnectIntentAction::NativeTransfer | WalletConnectIntentAction::Wrap => {
            walletconnect_amount_usd_micro_value(chain_id, amount, anchor_rates)
        }
        WalletConnectIntentAction::TokenTransfer
        | WalletConnectIntentAction::TransferFrom
        | WalletConnectIntentAction::Unwrap => {
            let amount_value =
                walletconnect_amount_usd_micro_value(chain_id, amount, anchor_rates)?;
            let Some(native) = attached_native.filter(|native| !native.raw.is_zero()) else {
                return Some(amount_value);
            };
            amount_value.checked_add(attached_value(native)?)
        }
        WalletConnectIntentAction::Approve
        | WalletConnectIntentAction::PersonalSign
        | WalletConnectIntentAction::TypedDataSign
        | WalletConnectIntentAction::AccountRequest
        | WalletConnectIntentAction::ChainSwitch => None,
    }
}

impl WalletConnectRisk {
    #[must_use]
    pub(super) fn authorization_label(&self) -> String {
        match self {
            Self::UnlimitedAllowance { spender } => format!(
                "Unlimited allowance: spender {} keeps continuing token authority",
                spender.to_checksum(None)
            ),
            Self::ForeignTransferSource { source } => {
                format!("Foreign transfer source: {}", source.to_checksum(None))
            }
            Self::AttachedNativeValue(amount) => {
                format!("Attached native value: {}", amount.display)
            }
            Self::UndecodedContractCall { selector } => format!(
                "Undecoded contract call{}; inspect raw details",
                selector_label(*selector)
                    .map_or_else(String::new, |selector| format!(" ({selector})"))
            ),
            Self::WouldRevert { reason } => format!("Simulation would revert: {reason}"),
        }
    }
}

pub(super) fn walletconnect_simulation_risk(
    status: &WalletConnectFeeStatus,
    reason: Option<&str>,
) -> Option<WalletConnectRisk> {
    matches!(status, WalletConnectFeeStatus::WouldRevert).then(|| WalletConnectRisk::WouldRevert {
        reason: reason.unwrap_or("the transaction may fail").to_owned(),
    })
}

#[derive(Debug)]
pub(super) struct WalletConnectTransactionDetails<'a> {
    pub(super) chain_id: u64,
    pub(super) transaction: &'a WalletConnectEvmTransaction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WalletConnectPeerProvenance {
    pub(super) site: String,
    pub(super) dapp_name: Option<String>,
}

#[derive(Debug)]
pub(super) struct WalletConnectIntentView<'a> {
    pub(super) action: WalletConnectIntentAction,
    pub(super) hero: WalletConnectHero,
    pub(super) amount: WalletConnectAmount,
    pub(super) icon: Option<WalletIconSource>,
    pub(super) usd_context: Option<String>,
    pub(super) attached_native: Option<WalletConnectNativeAmount>,
    pub(super) parties: Vec<WalletConnectParty>,
    pub(super) risks: Vec<WalletConnectRisk>,
    pub(super) transaction: Option<WalletConnectTransactionDetails<'a>>,
    pub(super) raw_request: &'a Value,
    pub(super) authorization: String,
    pub(super) provenance: WalletConnectPeerProvenance,
}

pub(super) fn build_walletconnect_intent<'a>(
    request: &'a WalletConnectRequestUi,
    context: WalletConnectIntentContext<'_>,
) -> WalletConnectIntentView<'a> {
    let selected_account = request.item.account;
    let mut amount = WalletConnectAmount::None;
    let mut icon = None;
    let mut attached_native = None;
    let mut parties = Vec::new();
    let mut risks = Vec::new();
    let mut transaction = None;
    let action;
    let hero_summary;

    match &request.parsed {
        WalletConnectParsedRequest::PersonalSign { message, .. } => {
            action = WalletConnectIntentAction::PersonalSign;
            let bytes = walletconnect_personal_message_bytes(message);
            let summary = personal_message_summary(&bytes);
            hero_summary = WalletConnectHeroSummary::PersonalMessage(summary);
            parties.push(party_for(
                WalletConnectPartyRole::Signer,
                selected_account,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
        }
        WalletConnectParsedRequest::EthSignTypedData { typed_data, .. }
        | WalletConnectParsedRequest::EthSignTypedDataV4 { typed_data, .. } => {
            let summary = typed_data_summary(typed_data);
            hero_summary = WalletConnectHeroSummary::TypedData(summary);
            if let Some(permit) = parse_eip2612_permit(typed_data, selected_account) {
                action = WalletConnectIntentAction::Approve;
                let (resolved_amount, resolved_icon) =
                    resolve_token_amount(context, Some(permit.token), permit.value, true);
                amount = resolved_amount;
                icon = resolved_icon;
                parties.push(party_for(
                    WalletConnectPartyRole::Spender,
                    permit.spender,
                    selected_account,
                    context.public_accounts,
                    context.public_address_book,
                ));
                if permit.value == U256::MAX {
                    risks.push(WalletConnectRisk::UnlimitedAllowance {
                        spender: permit.spender,
                    });
                }
            } else {
                action = WalletConnectIntentAction::TypedDataSign;
                parties.push(party_for(
                    WalletConnectPartyRole::Signer,
                    selected_account,
                    selected_account,
                    context.public_accounts,
                    context.public_address_book,
                ));
            }
        }
        WalletConnectParsedRequest::EthSendTransaction { transaction: tx } => {
            let decoded = request.item.decoded_transaction.as_ref();
            let resolved = resolve_transaction(request, context, tx, decoded);
            action = resolved.action;
            amount = resolved.amount;
            icon = resolved.icon;
            attached_native = resolved.attached_native;
            parties = resolved.parties;
            risks = resolved.risks;
            hero_summary = resolved.hero_summary;
            transaction = Some(WalletConnectTransactionDetails {
                chain_id: context.chain.chain_id,
                transaction: tx,
            });
        }
        WalletConnectParsedRequest::EthAccounts
        | WalletConnectParsedRequest::EthRequestAccounts => {
            action = WalletConnectIntentAction::AccountRequest;
            hero_summary = WalletConnectHeroSummary::None;
        }
        WalletConnectParsedRequest::WalletSwitchEthereumChain { .. } => {
            action = WalletConnectIntentAction::ChainSwitch;
            hero_summary = WalletConnectHeroSummary::None;
        }
    }

    let hero = WalletConnectHero {
        verb: action.verb(),
        summary: hero_summary,
        approval_effect: walletconnect_approval_effect(action, &amount),
    };
    let usd_context = amount.usd().map(ToOwned::to_owned);
    let authorization = authorization_projection(action, &hero, &amount);
    WalletConnectIntentView {
        action,
        hero,
        amount,
        icon,
        usd_context,
        attached_native,
        parties,
        risks,
        transaction,
        raw_request: &request.item.raw_details,
        authorization,
        provenance: walletconnect_peer_provenance(&request.session.peer_metadata),
    }
}

fn walletconnect_peer_provenance(
    metadata: &wallet_ops::vault::WalletConnectPeerMetadata,
) -> WalletConnectPeerProvenance {
    let site = reqwest::Url::parse(&metadata.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "Unknown peer site".to_owned());
    WalletConnectPeerProvenance {
        site,
        dapp_name: sanitize_walletconnect_dapp_name(&metadata.name),
    }
}

fn sanitize_walletconnect_dapp_name(value: &str) -> Option<String> {
    const MAX_DAPP_NAME_CHARS: usize = 80;
    let mut sanitized = String::new();
    let mut pending_space = false;
    for character in value.chars() {
        let bidi_control = matches!(
            character,
            '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200e}'
                | '\u{200f}'
        );
        if character.is_control() || bidi_control || character.is_whitespace() {
            pending_space = !sanitized.is_empty();
            continue;
        }
        if pending_space {
            sanitized.push(' ');
            pending_space = false;
        }
        sanitized.push(character);
    }
    let sanitized = sanitized
        .chars()
        .take(MAX_DAPP_NAME_CHARS)
        .collect::<String>();
    (!sanitized.is_empty()).then_some(sanitized)
}

pub(super) fn walletconnect_selected_account_provenance_visible(
    selected_account: Address,
    parties: &[WalletConnectParty],
) -> bool {
    !parties
        .iter()
        .any(|party| party.address == selected_account)
}

pub(super) fn walletconnect_checksummed_address_label(address: &Address) -> String {
    let address = address.to_checksum(None);
    format!("{}…{}", &address[..10], &address[34..])
}

pub(super) fn walletconnect_party_address_label(
    role: WalletConnectPartyRole,
    address: &Address,
    content_width: Pixels,
) -> String {
    if matches!(
        role,
        WalletConnectPartyRole::Spender
            | WalletConnectPartyRole::Recipient
            | WalletConnectPartyRole::Source
    ) && content_width >= WALLETCONNECT_CRITICAL_PARTY_FULL_ADDRESS_MIN_WIDTH
    {
        address.to_checksum(None)
    } else {
        walletconnect_checksummed_address_label(address)
    }
}

pub(super) fn walletconnect_party_badge_label(
    role: WalletConnectPartyRole,
    badge: &WalletConnectPartyBadge,
) -> Option<String> {
    if matches!(
        role,
        WalletConnectPartyRole::Spender | WalletConnectPartyRole::Contract
    ) && matches!(badge, WalletConnectPartyBadge::NotInAddressBook)
    {
        return None;
    }
    match badge {
        WalletConnectPartyBadge::ThisWallet => Some("This wallet".to_owned()),
        WalletConnectPartyBadge::YourAccount { label } => label.as_deref().map_or_else(
            || Some("Your account".to_owned()),
            |label| Some(format!("Your account · {label}")),
        ),
        WalletConnectPartyBadge::AddressBook { label } => Some(label.clone()),
        WalletConnectPartyBadge::NotInAddressBook => Some("Not in address book".to_owned()),
    }
}

pub(super) fn walletconnect_approximate_usd_label(usd: &str) -> String {
    format!("≈ {usd}")
}

pub(super) const fn walletconnect_should_render_token_contract(
    action: WalletConnectIntentAction,
) -> bool {
    !matches!(action, WalletConnectIntentAction::Approve)
}

pub(super) const fn walletconnect_token_contract_recognition(
    amount: &WalletConnectAmount,
) -> &'static str {
    match amount {
        WalletConnectAmount::KnownToken { .. }
        | WalletConnectAmount::Unlimited {
            unrecognised_contract: false,
            ..
        } => "In token list",
        _ => "Unrecognised token contract; decimals unknown",
    }
}

fn walletconnect_approval_effect(
    action: WalletConnectIntentAction,
    amount: &WalletConnectAmount,
) -> Option<String> {
    if !matches!(action, WalletConnectIntentAction::Approve) {
        return None;
    }
    match amount {
        WalletConnectAmount::KnownToken { display, .. } => Some(format!(
            "Spender can withdraw up to {display} across one or more transactions until the allowance is used or revoked."
        )),
        WalletConnectAmount::Unlimited {
            symbol: Some(symbol),
            unrecognised_contract: false,
            ..
        } => Some(format!(
            "No fixed limit in {symbol} until the allowance is revoked."
        )),
        WalletConnectAmount::Unlimited { .. } => Some(
            "No fixed limit until the allowance is revoked; this ERC-20-shaped call targets an unknown contract, so allowance semantics are not verified."
                .to_owned(),
        ),
        WalletConnectAmount::RawToken {
            raw,
            unrecognised_contract: true,
            decimals_unknown: true,
            ..
        } => Some(format!(
            "{raw} raw token units; this ERC-20-shaped call targets an unknown contract, so allowance semantics are not verified."
        )),
        WalletConnectAmount::RawToken { .. }
        | WalletConnectAmount::Native { .. }
        | WalletConnectAmount::None => None,
    }
}

struct ResolvedTransaction {
    action: WalletConnectIntentAction,
    hero_summary: WalletConnectHeroSummary,
    amount: WalletConnectAmount,
    icon: Option<WalletIconSource>,
    attached_native: Option<WalletConnectNativeAmount>,
    parties: Vec<WalletConnectParty>,
    risks: Vec<WalletConnectRisk>,
}

fn resolve_transaction(
    request: &WalletConnectRequestUi,
    context: WalletConnectIntentContext<'_>,
    tx: &WalletConnectEvmTransaction,
    decoded: Option<&WalletConnectDecodedTransaction>,
) -> ResolvedTransaction {
    let target = decoded.map_or(tx.to, |decoded| decoded.target);
    let native_value = decoded.map_or_else(
        || tx.value.unwrap_or(U256::ZERO),
        |decoded| decoded.native_value,
    );
    let selector = decoded
        .and_then(|decoded| decoded.kind.selector())
        .or_else(|| walletconnect_transaction_selector(tx));
    let attached_native = (!native_value.is_zero()).then(|| native_amount(context, native_value));
    let mut resolved = ResolvedTransaction {
        action: WalletConnectIntentAction::ContractCall,
        hero_summary: WalletConnectHeroSummary::UndecodedCall { selector },
        amount: WalletConnectAmount::None,
        icon: None,
        attached_native,
        parties: Vec::new(),
        risks: Vec::new(),
    };
    let selected_account = request.item.account;

    let Some(kind) = decoded.map(|decoded| &decoded.kind) else {
        if resolved.attached_native.is_some() {
            resolved.icon =
                chain_icon_asset_path(context.chain.chain_id).map(WalletIconSource::embedded);
        }
        if let Some(target) = target {
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Caller,
                tx.from,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Contract,
                target,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
        }
        resolved
            .risks
            .push(WalletConnectRisk::UndecodedContractCall { selector });
        return resolved;
    };

    match kind {
        WalletConnectDecodedCallKind::NativeTransfer
            if target.is_some() && !native_value.is_zero() =>
        {
            let Some(target) = target else {
                add_undecoded_call(
                    &mut resolved,
                    context,
                    tx.from,
                    target,
                    selected_account,
                    selector,
                );
                return resolved;
            };
            resolved.action = WalletConnectIntentAction::NativeTransfer;
            resolved.amount = WalletConnectAmount::Native {
                raw: native_value,
                symbol: native_token_display_label(context.chain.chain_id).to_owned(),
                display: format_native_token_amount_for_display(
                    context.chain.chain_id,
                    native_value,
                ),
                usd: context
                    .anchor_rates
                    .cached_native_usd_micro_value(context.chain.chain_id, native_value)
                    .map(format_usd_micro_value),
            };
            resolved.icon =
                chain_icon_asset_path(context.chain.chain_id).map(WalletIconSource::embedded);
            resolved.hero_summary = WalletConnectHeroSummary::Amount;
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Sender,
                tx.from,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Recipient,
                target,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
        }
        WalletConnectDecodedCallKind::ContractCreation => {
            resolved.action = WalletConnectIntentAction::ContractCreation;
            resolved.hero_summary = WalletConnectHeroSummary::None;
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Creator,
                tx.from,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
        }
        WalletConnectDecodedCallKind::Erc20Approve {
            spender,
            amount: raw,
        } => {
            resolved.action = WalletConnectIntentAction::Approve;
            let (token_amount, token_icon) =
                resolve_token_amount(context, target, *raw, *raw == U256::MAX);
            resolved.amount = token_amount;
            resolved.icon = token_icon;
            resolved.hero_summary = WalletConnectHeroSummary::Amount;
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Spender,
                *spender,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
        }
        WalletConnectDecodedCallKind::Erc20Transfer {
            recipient,
            amount: raw,
        } => {
            resolved.action = WalletConnectIntentAction::TokenTransfer;
            let (token_amount, token_icon) = resolve_token_amount(context, target, *raw, false);
            resolved.amount = token_amount;
            resolved.icon = token_icon;
            resolved.hero_summary = WalletConnectHeroSummary::Amount;
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Sender,
                tx.from,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Recipient,
                *recipient,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
        }
        WalletConnectDecodedCallKind::Erc20TransferFrom {
            from,
            to,
            amount: raw,
        } => {
            resolved.action = WalletConnectIntentAction::TransferFrom;
            let (token_amount, token_icon) = resolve_token_amount(context, target, *raw, false);
            resolved.amount = token_amount;
            resolved.icon = token_icon;
            resolved.hero_summary = WalletConnectHeroSummary::Amount;
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Source,
                *from,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
            resolved.parties.push(party_for(
                WalletConnectPartyRole::Recipient,
                *to,
                selected_account,
                context.public_accounts,
                context.public_address_book,
            ));
        }
        WalletConnectDecodedCallKind::WrappedDeposit => {
            if is_trusted_wrapped_native(context.chain, target) {
                resolved.action = WalletConnectIntentAction::Wrap;
                resolved.amount = native_amount_as_amount(context, native_value);
                resolved.icon =
                    chain_icon_asset_path(context.chain.chain_id).map(WalletIconSource::embedded);
                resolved.hero_summary = WalletConnectHeroSummary::Amount;
                add_wrap_parties(
                    &mut resolved.parties,
                    context,
                    tx.from,
                    target,
                    selected_account,
                );
            } else {
                add_undecoded_call(
                    &mut resolved,
                    context,
                    tx.from,
                    target,
                    selected_account,
                    selector,
                );
            }
        }
        WalletConnectDecodedCallKind::WrappedWithdraw { amount: raw } => {
            if is_trusted_wrapped_native(context.chain, target) {
                resolved.action = WalletConnectIntentAction::Unwrap;
                let (token_amount, token_icon) = resolve_token_amount(context, target, *raw, false);
                resolved.amount = token_amount;
                resolved.icon = token_icon;
                resolved.hero_summary = WalletConnectHeroSummary::Amount;
                add_wrap_parties(
                    &mut resolved.parties,
                    context,
                    tx.from,
                    target,
                    selected_account,
                );
            } else {
                add_undecoded_call(
                    &mut resolved,
                    context,
                    tx.from,
                    target,
                    selected_account,
                    selector,
                );
            }
        }
        WalletConnectDecodedCallKind::ContractCall { .. }
        | WalletConnectDecodedCallKind::NativeTransfer => {
            add_undecoded_call(
                &mut resolved,
                context,
                tx.from,
                target,
                selected_account,
                selector,
            );
        }
    }
    resolved.risks.extend(token_operation_risks(
        kind,
        resolved.attached_native.as_ref(),
        selected_account,
    ));
    resolved
}

fn token_operation_risks(
    kind: &WalletConnectDecodedCallKind,
    attached_native: Option<&WalletConnectNativeAmount>,
    selected_account: Address,
) -> Vec<WalletConnectRisk> {
    let mut risks = Vec::new();
    match kind {
        WalletConnectDecodedCallKind::Erc20Approve { spender, amount } if *amount == U256::MAX => {
            risks.push(WalletConnectRisk::UnlimitedAllowance { spender: *spender });
        }
        WalletConnectDecodedCallKind::Erc20TransferFrom { from, .. }
            if *from != selected_account =>
        {
            risks.push(WalletConnectRisk::ForeignTransferSource { source: *from });
        }
        WalletConnectDecodedCallKind::Erc20Approve { .. }
        | WalletConnectDecodedCallKind::Erc20Transfer { .. }
        | WalletConnectDecodedCallKind::Erc20TransferFrom { .. } => {}
        _ => return risks,
    }
    add_attached_native_risk(&mut risks, attached_native);
    risks
}

fn add_attached_native_risk(
    risks: &mut Vec<WalletConnectRisk>,
    attached_native: Option<&WalletConnectNativeAmount>,
) {
    if let Some(amount) = attached_native {
        risks.push(WalletConnectRisk::AttachedNativeValue(amount.clone()));
    }
}

fn add_undecoded_call(
    resolved: &mut ResolvedTransaction,
    context: WalletConnectIntentContext<'_>,
    caller: Address,
    target: Option<Address>,
    selected_account: Address,
    selector: Option<[u8; 4]>,
) {
    resolved.action = WalletConnectIntentAction::ContractCall;
    if resolved.attached_native.is_some() {
        resolved.icon =
            chain_icon_asset_path(context.chain.chain_id).map(WalletIconSource::embedded);
    }
    resolved.hero_summary = WalletConnectHeroSummary::UndecodedCall { selector };
    resolved.parties.push(party_for(
        WalletConnectPartyRole::Caller,
        caller,
        selected_account,
        context.public_accounts,
        context.public_address_book,
    ));
    if let Some(target) = target {
        resolved.parties.push(party_for(
            WalletConnectPartyRole::Contract,
            target,
            selected_account,
            context.public_accounts,
            context.public_address_book,
        ));
    }
    resolved
        .risks
        .push(WalletConnectRisk::UndecodedContractCall { selector });
}

fn add_wrap_parties(
    parties: &mut Vec<WalletConnectParty>,
    context: WalletConnectIntentContext<'_>,
    sender: Address,
    target: Option<Address>,
    selected_account: Address,
) {
    parties.push(party_for(
        WalletConnectPartyRole::Sender,
        sender,
        selected_account,
        context.public_accounts,
        context.public_address_book,
    ));
    if let Some(target) = target {
        parties.push(party_for(
            WalletConnectPartyRole::WrappedNativeContract,
            target,
            selected_account,
            context.public_accounts,
            context.public_address_book,
        ));
    }
}

fn resolve_token_amount(
    context: WalletConnectIntentContext<'_>,
    target: Option<Address>,
    raw: U256,
    allowance: bool,
) -> (WalletConnectAmount, Option<WalletIconSource>) {
    let Some(token) = target else {
        return (
            WalletConnectAmount::RawToken {
                token: Address::ZERO,
                raw,
                display: format!("{raw} raw token units (missing token contract)"),
                unrecognised_contract: true,
                decimals_unknown: true,
            },
            None,
        );
    };
    let metadata =
        token_display_metadata(Some(context.token_registry), context.chain.chain_id, &token);
    if raw == U256::MAX && allowance {
        return (
            WalletConnectAmount::Unlimited {
                token,
                raw,
                decimals: metadata.as_ref().map(|metadata| metadata.decimals),
                symbol: metadata.as_ref().map(|metadata| metadata.symbol.clone()),
                display: metadata.as_ref().map_or_else(
                    || format!(
                        "Unlimited ({raw} raw token units; unrecognised contract, decimals unknown)"
                    ),
                    |metadata| format!("Unlimited {}", metadata.symbol),
                ),
                unrecognised_contract: metadata.is_none(),
            },
            metadata.and_then(|metadata| metadata.icon_path),
        );
    }
    let Some(metadata) = metadata else {
        return (
            WalletConnectAmount::RawToken {
                token,
                raw,
                display: format!(
                    "{raw} raw token units ({token}; unrecognised contract, decimals unknown)"
                ),
                unrecognised_contract: true,
                decimals_unknown: true,
            },
            None,
        );
    };
    let usd = context
        .anchor_rates
        .cached_token_usd_micro_value(context.chain.chain_id, token, raw)
        .map(format_usd_micro_value);
    (
        WalletConnectAmount::KnownToken {
            token,
            raw,
            decimals: metadata.decimals,
            symbol: metadata.symbol.clone(),
            display: format_token_amount_for_display(
                context.chain.chain_id,
                token,
                raw,
                Some(context.token_registry),
            ),
            usd,
        },
        metadata.icon_path,
    )
}

fn native_amount(context: WalletConnectIntentContext<'_>, raw: U256) -> WalletConnectNativeAmount {
    WalletConnectNativeAmount {
        raw,
        symbol: native_token_display_label(context.chain.chain_id).to_owned(),
        display: format_native_token_amount_for_display(context.chain.chain_id, raw),
        usd: context
            .anchor_rates
            .cached_native_usd_micro_value(context.chain.chain_id, raw)
            .map(format_usd_micro_value),
    }
}

fn native_amount_as_amount(
    context: WalletConnectIntentContext<'_>,
    raw: U256,
) -> WalletConnectAmount {
    let native = native_amount(context, raw);
    WalletConnectAmount::Native {
        raw: native.raw,
        symbol: native.symbol,
        display: native.display,
        usd: native.usd,
    }
}

fn is_trusted_wrapped_native(config: &EffectiveChainConfig, target: Option<Address>) -> bool {
    let Some(target) = target else {
        return false;
    };
    config
        .wrapped_native_token
        .as_deref()
        .and_then(|address| address.parse::<Address>().ok())
        .is_some_and(|wrapped| wrapped == target)
}

fn party_for(
    role: WalletConnectPartyRole,
    address: Address,
    selected_account: Address,
    public_accounts: &[PublicAccountMetadata],
    public_address_book: &[PublicAddressBookEntry],
) -> WalletConnectParty {
    let badge = if address == selected_account {
        WalletConnectPartyBadge::ThisWallet
    } else if let Some(account) = public_accounts
        .iter()
        .find(|account| account.address == address)
    {
        WalletConnectPartyBadge::YourAccount {
            label: account
                .label
                .as_deref()
                .filter(|label| !label.is_empty())
                .map(ToOwned::to_owned),
        }
    } else if let Some(entry) = public_address_book
        .iter()
        .find(|entry| entry.address == address)
    {
        WalletConnectPartyBadge::AddressBook {
            label: entry.label.clone(),
        }
    } else {
        WalletConnectPartyBadge::NotInAddressBook
    };
    WalletConnectParty {
        role,
        address,
        badge,
    }
}

fn personal_message_summary(bytes: &[u8]) -> WalletConnectPersonalMessageSummary {
    let preview = std::str::from_utf8(bytes)
        .ok()
        .and_then(safe_bounded_preview);
    WalletConnectPersonalMessageSummary {
        bytes: bytes.len(),
        preview,
    }
}

// Keep the hero preview bounded to 160 Unicode scalar values; raw request details retain all bytes.
fn safe_bounded_preview(value: &str) -> Option<String> {
    let chars = value.chars().collect::<Vec<_>>();
    let control_count = chars
        .iter()
        .filter(|character| character.is_control())
        .count();
    let disallowed_control = chars
        .iter()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'));
    if disallowed_control || control_count.saturating_mul(10) > chars.len().max(1) {
        return None;
    }
    if chars.len() <= PERSONAL_SIGN_PREVIEW_MAX_CHARS {
        return Some(value.to_owned());
    }
    let mut preview = chars[..PERSONAL_SIGN_PREVIEW_MAX_CHARS - 3]
        .iter()
        .collect::<String>();
    preview.push_str("...");
    Some(preview)
}

fn typed_data_summary(value: &Value) -> WalletConnectTypedDataSummary {
    let domain_name = value
        .get("domain")
        .and_then(Value::as_object)
        .and_then(|domain| domain.get("name"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let primary_type = value
        .get("primaryType")
        .and_then(Value::as_str)
        .filter(|primary_type| !primary_type.is_empty())
        .unwrap_or("Unknown primary type")
        .to_owned();
    WalletConnectTypedDataSummary {
        domain_name,
        primary_type,
    }
}

fn parse_eip2612_permit(
    value: &Value,
    selected_account: Address,
) -> Option<WalletConnectEip2612Permit> {
    if value.get("primaryType")?.as_str()? != "Permit" {
        return None;
    }
    let fields = value.get("types")?.get("Permit")?.as_array()?;
    let expected_fields = [
        ("owner", "address"),
        ("spender", "address"),
        ("value", "uint256"),
        ("nonce", "uint256"),
        ("deadline", "uint256"),
    ];
    if fields.len() != expected_fields.len()
        || !fields.iter().zip(expected_fields).all(|(field, expected)| {
            let Some(field) = field.as_object() else {
                return false;
            };
            field.len() == 2
                && field.get("name").and_then(Value::as_str) == Some(expected.0)
                && field.get("type").and_then(Value::as_str) == Some(expected.1)
        })
    {
        return None;
    }

    let domain = value.get("domain")?.as_object()?;
    let token = domain
        .get("verifyingContract")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<Address>().ok())?;
    let message = value.get("message")?.as_object()?;
    if message.len() != expected_fields.len() {
        return None;
    }
    let owner = message
        .get("owner")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<Address>().ok())?;
    if owner != selected_account {
        return None;
    }
    let spender = message
        .get("spender")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<Address>().ok())?;
    let value = parse_permit_u256(message.get("value")?)?;
    parse_permit_u256(message.get("nonce")?)?;
    parse_permit_u256(message.get("deadline")?)?;
    Some(WalletConnectEip2612Permit {
        token,
        spender,
        value,
    })
}

fn parse_permit_u256(value: &Value) -> Option<U256> {
    if let Some(value) = value.as_u64() {
        return Some(U256::from(value));
    }
    let value = value.as_str()?;
    if let Some(value) = value.strip_prefix("0x") {
        U256::from_str_radix(value, 16).ok()
    } else {
        U256::from_str_radix(value, 10).ok()
    }
}

fn selector_label(selector: Option<[u8; 4]>) -> Option<String> {
    selector.map(|selector| format!("0x{}", alloy::hex::encode(selector)))
}

pub(super) const fn walletconnect_party_role_label(role: WalletConnectPartyRole) -> &'static str {
    match role {
        WalletConnectPartyRole::Sender => "From",
        WalletConnectPartyRole::Recipient => "To",
        WalletConnectPartyRole::Spender => "Spender",
        WalletConnectPartyRole::Source => "Source",
        WalletConnectPartyRole::Caller => "Caller",
        WalletConnectPartyRole::Contract => "Contract",
        WalletConnectPartyRole::Creator => "Creator",
        WalletConnectPartyRole::Signer => "Signer",
        WalletConnectPartyRole::WrappedNativeContract => "Wrapped-native contract",
    }
}

fn authorization_projection(
    action: WalletConnectIntentAction,
    hero: &WalletConnectHero,
    amount: &WalletConnectAmount,
) -> String {
    let mut projection = action.verb().to_owned();
    if let Some(amount_label) = amount.authorization_label() {
        if action == WalletConnectIntentAction::Approve
            && !matches!(amount, WalletConnectAmount::Unlimited { .. })
        {
            projection.push_str(" up to");
        }
        projection.push(' ');
        projection.push_str(&amount_label);
    }
    let mut parts = vec![projection];
    match &hero.summary {
        WalletConnectHeroSummary::PersonalMessage(summary) => {
            parts.push(summary.preview.as_ref().map_or_else(
                || format!("Personal message ({} bytes)", summary.bytes),
                |preview| format!("Personal message: {preview:?}"),
            ));
        }
        WalletConnectHeroSummary::TypedData(_)
        | WalletConnectHeroSummary::Amount
        | WalletConnectHeroSummary::UndecodedCall { .. }
        | WalletConnectHeroSummary::None => {}
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use wallet_ops::settings::{
        EffectiveChainGasSettings, EffectiveTokenInfo, IndexedArtifactSourceModeSetting,
    };
    use wallet_ops::vault::{
        PublicAccountScope, PublicAccountSource, PublicAccountStatus, WalletConnectPeerMetadata,
        WalletConnectSessionKeys, WalletConnectSessionLifecycleState, WalletConnectSessionRecord,
    };

    #[test]
    fn simulation_revert_becomes_distinct_risk_but_unavailable_does_not() {
        let risk = walletconnect_simulation_risk(
            &WalletConnectFeeStatus::WouldRevert,
            Some("execution reverted"),
        );
        assert!(matches!(
            risk,
            Some(WalletConnectRisk::WouldRevert { ref reason }) if reason == "execution reverted"
        ));
        assert!(
            walletconnect_simulation_risk(
                &WalletConnectFeeStatus::UnavailableFailed,
                Some("provider unavailable"),
            )
            .is_none()
        );
    }

    #[test]
    fn approval_uses_spender_only_and_keeps_token_contract_out_of_primary_parties() {
        let account = Address::from([0x11; 20]);
        let token = Address::from([0x22; 20]);
        let spender = Address::from([0x33; 20]);
        let chain = chain(None);
        let registry = registry(token);
        let rates = TokenAnchorRateCache::new();
        let request = request_with(
            WalletConnectParsedRequest::EthSendTransaction {
                transaction: transaction(account, Some(token), U256::ZERO),
            },
            Some(WalletConnectDecodedTransaction {
                target: Some(token),
                native_value: U256::ZERO,
                kind: WalletConnectDecodedCallKind::Erc20Approve {
                    spender,
                    amount: U256::from(1),
                },
            }),
        );
        let intent =
            build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));
        assert_eq!(intent.action, WalletConnectIntentAction::Approve);
        assert_eq!(intent.parties.len(), 1);
        assert_eq!(intent.parties[0].role, WalletConnectPartyRole::Spender);
        assert!(!walletconnect_should_render_token_contract(intent.action));
        let summary =
            super::super::requests::walletconnect_request_authorization_summary(&request, &intent);
        let authorization = summary
            .rows_for_test()
            .into_iter()
            .find(|(label, _)| label == "Intent")
            .expect("intent authorization row")
            .1;
        assert!(authorization.starts_with("Allow spending up to "));
        assert!(authorization.contains("USDC"));
        assert!(
            summary
                .rows_for_test()
                .contains(&("Spender".to_owned(), spender.to_checksum(None),))
        );
        assert!(
            !authorization.contains("Spender can withdraw up to"),
            "approval effect should remain on the review card, not the final summary"
        );
    }

    #[test]
    fn authorization_summary_promotes_walletconnect_risks_to_warnings() {
        let account = Address::from([0x11; 20]);
        let token = Address::from([0x22; 20]);
        let spender = Address::from([0x33; 20]);
        let chain = chain(None);
        let registry = registry(token);
        let rates = TokenAnchorRateCache::new();
        let request = request_with(
            WalletConnectParsedRequest::EthSendTransaction {
                transaction: transaction(account, Some(token), U256::ZERO),
            },
            Some(WalletConnectDecodedTransaction {
                target: Some(token),
                native_value: U256::ZERO,
                kind: WalletConnectDecodedCallKind::Erc20Approve {
                    spender,
                    amount: U256::MAX,
                },
            }),
        );
        let intent =
            build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));
        let summary =
            super::super::requests::walletconnect_request_authorization_summary(&request, &intent);

        assert!(
            summary
                .warnings_for_test()
                .iter()
                .any(|warning| warning.starts_with("Unlimited allowance:"))
        );
    }

    #[test]
    fn approval_effect_distinguishes_known_and_unresolved_allowances() {
        let known = WalletConnectAmount::KnownToken {
            token: Address::ZERO,
            raw: U256::from(5),
            decimals: 6,
            symbol: "USDC".to_owned(),
            display: "0.000005 USDC".to_owned(),
            usd: None,
        };
        assert_eq!(
            walletconnect_approval_effect(WalletConnectIntentAction::Approve, &known),
            Some("Spender can withdraw up to 0.000005 USDC across one or more transactions until the allowance is used or revoked.".to_owned())
        );
        let unlimited = WalletConnectAmount::Unlimited {
            token: Address::ZERO,
            raw: U256::MAX,
            decimals: Some(6),
            symbol: Some("USDC".to_owned()),
            display: "Unlimited USDC".to_owned(),
            unrecognised_contract: false,
        };
        assert_eq!(
            walletconnect_approval_effect(WalletConnectIntentAction::Approve, &unlimited),
            Some("No fixed limit in USDC until the allowance is revoked.".to_owned())
        );
        let unresolved = WalletConnectAmount::RawToken {
            token: Address::ZERO,
            raw: U256::from(123),
            display: "123 raw token units".to_owned(),
            unrecognised_contract: true,
            decimals_unknown: true,
        };
        let unresolved_effect =
            walletconnect_approval_effect(WalletConnectIntentAction::Approve, &unresolved)
                .expect("unresolved finite effect");
        assert!(unresolved_effect.contains("123 raw token units"));
        assert!(unresolved_effect.contains("ERC-20-shaped"));
        assert!(unresolved_effect.contains("unknown contract"));
        assert!(unresolved_effect.contains("not verified"));
        let unresolved_unlimited = WalletConnectAmount::Unlimited {
            token: Address::ZERO,
            raw: U256::MAX,
            decimals: None,
            symbol: None,
            display: "Unlimited raw token units".to_owned(),
            unrecognised_contract: true,
        };
        let unresolved_unlimited_effect = walletconnect_approval_effect(
            WalletConnectIntentAction::Approve,
            &unresolved_unlimited,
        )
        .expect("unresolved unlimited effect");
        assert!(unresolved_unlimited_effect.contains("No fixed limit"));
        assert!(unresolved_unlimited_effect.contains("unknown contract"));
        assert!(
            walletconnect_approval_effect(WalletConnectIntentAction::TokenTransfer, &known,)
                .is_none()
        );
    }

    #[test]
    fn authorization_summary_merges_projected_site_and_dapp_name() {
        let account = Address::from([0x11; 20]);
        let chain = chain(None);
        let registry = EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        let rates = TokenAnchorRateCache::new();
        let request = request_with(
            WalletConnectParsedRequest::EthSignTypedDataV4 {
                account,
                typed_data: serde_json::json!({}),
                domain_chain_id: Some(U256::from(1)),
            },
            None,
        );
        let intent =
            build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));
        let summary =
            super::super::requests::walletconnect_request_authorization_summary(&request, &intent);
        let rows = summary.rows_for_test();
        assert!(rows.contains(&("Requested by".to_owned(), "app.aave.com (Aave)".to_owned())));
        assert!(!rows.iter().any(|(label, _)| label == "Site"));
        assert!(!rows.iter().any(|(label, _)| label == "Dapp"));
        assert!(
            !rows
                .iter()
                .any(|(label, _)| label == "Maximum network cost")
        );
    }

    #[test]
    fn walletconnect_authorization_summary_includes_reviewed_maximum_cost() {
        let account = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);
        let chain = chain(None);
        let registry = EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        let rates = TokenAnchorRateCache::new();
        let request = request_with(
            WalletConnectParsedRequest::EthSendTransaction {
                transaction: transaction(account, Some(target), U256::from(1)),
            },
            Some(WalletConnectDecodedTransaction {
                target: Some(target),
                native_value: U256::from(1),
                kind: WalletConnectDecodedCallKind::NativeTransfer,
            }),
        );
        let intent =
            build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));
        let mut reviewed = super::super::fee::WalletConnectReviewedFeeProjection::unresolved(
            request.key.as_str(),
            request.review_token,
        );
        reviewed.maximum_gas_cost = Some(U256::from(123_456_u64));
        let rows = super::super::requests::walletconnect_request_authorization_summary_with_fee(
            &request,
            &intent,
            Some(&reviewed),
        )
        .rows_for_test();
        assert!(
            rows.iter()
                .any(|(label, value)| label == "Maximum network cost" && !value.is_empty())
        );
        assert!(rows.contains(&("To".to_owned(), target.to_checksum(None))));
    }

    #[test]
    fn critical_party_address_projection_is_single_and_responsive() {
        let address = Address::from_slice(
            &alloy::hex::decode("1234567890abcdef1234567890abcdef12345678").expect("address"),
        );
        let full = address.to_checksum(None);
        let short = walletconnect_checksummed_address_label(&address);
        assert_eq!(
            walletconnect_party_address_label(
                WalletConnectPartyRole::Spender,
                &address,
                WALLETCONNECT_CRITICAL_PARTY_FULL_ADDRESS_MIN_WIDTH,
            ),
            full
        );
        assert_eq!(
            walletconnect_party_address_label(
                WalletConnectPartyRole::Recipient,
                &address,
                px(499.0),
            ),
            short
        );
        assert_eq!(
            walletconnect_party_address_label(WalletConnectPartyRole::Caller, &address, px(620.0),),
            short
        );
    }

    #[test]
    fn peer_provenance_normalizes_host_and_sanitizes_capped_name() {
        let metadata = WalletConnectPeerMetadata {
            name: format!("  Evil\u{202e} dapp\n{}", "x".repeat(100)),
            description: String::new(),
            url: "HTTPS://Example.COM:443/path".to_owned(),
            icons: Vec::new(),
        };
        let projection = walletconnect_peer_provenance(&metadata);
        assert_eq!(projection.site, "example.com");
        let dapp_name = projection.dapp_name.expect("sanitized dapp name");
        assert!(!dapp_name.contains('\u{202e}'));
        assert_eq!(dapp_name.chars().count(), 80);

        let empty = WalletConnectPeerMetadata {
            name: "\u{200f}\u{0000} \t".to_owned(),
            description: String::new(),
            url: "not a url".to_owned(),
            icons: Vec::new(),
        };
        let projection = walletconnect_peer_provenance(&empty);
        assert_eq!(projection.site, "Unknown peer site");
        assert_eq!(projection.dapp_name, None);
    }

    use wallet_ops::{WalletConnectEvmTransaction, WalletConnectPendingRequest};

    fn chain(wrapped_native_token: Option<&str>) -> EffectiveChainConfig {
        EffectiveChainConfig {
            chain_id: 1,
            enabled: true,
            rpc_endpoints: Vec::new(),
            sponsored_bundle_relays: Vec::new(),
            archive_rpc_url: None,
            quick_sync_enabled: false,
            quick_sync_endpoint: None,
            indexed_artifact_source_mode: IndexedArtifactSourceModeSetting::default(),
            indexed_artifact_source: None,
            indexed_wallet_block_range: 0,
            deployment_block: 0,
            v2_start_block: 0,
            legacy_shield_block: 0,
            archive_until_block: 0,
            railgun_contract: String::new(),
            relay_adapt_contract: String::new(),
            relay_adapt_7702_contract: String::new(),
            wrapped_native_token: wrapped_native_token.map(ToOwned::to_owned),
            multicall_contract: String::new(),
            coinbase_payer: None,
            finality_depth: 0,
            block_time: Duration::ZERO,
            block_range: None,
            poll_interval_secs: None,
            gas: EffectiveChainGasSettings {
                gas_limit_buffer: 0,
                gas_price_buffer_numerator: 0,
                gas_price_buffer_denominator: 1,
            },
        }
    }

    fn context<'a>(
        chain: &'a EffectiveChainConfig,
        registry: &'a EffectiveTokenRegistry,
        rates: &'a TokenAnchorRateCache,
        accounts: &'a [PublicAccountMetadata],
        address_book: &'a [PublicAddressBookEntry],
    ) -> WalletConnectIntentContext<'a> {
        WalletConnectIntentContext {
            chain,
            token_registry: registry,
            anchor_rates: rates,
            public_accounts: accounts,
            public_address_book: address_book,
        }
    }

    fn request_with(
        parsed: WalletConnectParsedRequest,
        decoded_transaction: Option<WalletConnectDecodedTransaction>,
    ) -> WalletConnectRequestUi {
        let account = Address::from([0x11; 20]);
        let method = parsed.method();
        WalletConnectRequestUi {
            key: "session-topic:7".to_owned(),
            review_token: 1,
            session: WalletConnectSessionRecord {
                session_uuid: "session-uuid".to_owned(),
                pairing_topic: "pairing-topic".to_owned(),
                session_topic: "session-topic".to_owned(),
                relay_protocol: "irn".to_owned(),
                relay_client_id: "relay-client".to_owned(),
                peer_metadata: WalletConnectPeerMetadata {
                    name: "Aave".to_owned(),
                    description: String::new(),
                    url: "https://app.aave.com".to_owned(),
                    icons: Vec::new(),
                },
                approved_namespaces: BTreeMap::new(),
                selected_public_account_uuid: "public-account".to_owned(),
                selected_public_account_scope: PublicAccountScope::Global,
                owning_private_wallet_uuid: None,
                keys: WalletConnectSessionKeys {
                    sym_key: [1; 32],
                    responder_private_key: [2; 32],
                    responder_public_key: [3; 32],
                },
                expiry_timestamp: 1_700_000_300,
                lifecycle_state: WalletConnectSessionLifecycleState::Active,
            },
            parsed,
            item: WalletConnectPendingRequest {
                id: 7,
                topic: "session-topic".to_owned(),
                dapp_name: "Aave".to_owned(),
                chain_id: "eip155:1".to_owned(),
                method,
                account,
                decoded_transaction,
                raw_details: json!({}),
                expiry_timestamp: Some(1_700_000_300),
            },
            account_source: PublicAccountSource::Imported,
        }
    }

    fn transaction(from: Address, to: Option<Address>, value: U256) -> WalletConnectEvmTransaction {
        WalletConnectEvmTransaction {
            from,
            to,
            value: Some(value),
            data: None,
            access_list: None,
            gas: None,
            gas_price: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            chain_id: Some(1),
            nonce: None,
            transaction_type: None,
            raw: json!({}),
        }
    }

    fn registry(token: Address) -> EffectiveTokenRegistry {
        let info = EffectiveTokenInfo {
            chain_id: 1,
            token_address: token.to_string(),
            symbol: "USDC".to_owned(),
            decimals: 6,
            icon_path: None,
            price_anchor: None,
            built_in: false,
        };
        EffectiveTokenRegistry {
            tokens: BTreeMap::from([((1, token.to_string().to_ascii_lowercase()), info)]),
        }
    }

    fn account(
        address: Address,
        label: Option<&str>,
        status: PublicAccountStatus,
    ) -> PublicAccountMetadata {
        PublicAccountMetadata {
            public_account_uuid: address.to_string(),
            address,
            label: label.map(ToOwned::to_owned),
            source: PublicAccountSource::Imported,
            scope: PublicAccountScope::Global,
            derivation_index: None,
            hardware_descriptor: None,
            status,
            display_order: 0,
        }
    }

    #[test]
    fn token_hit_miss_and_cached_usd_are_explicit() {
        let token = Address::from([0x11; 20]);
        let chain = chain(None);
        let registry = registry(token);
        let rates = TokenAnchorRateCache::new();
        let empty_accounts = Vec::<PublicAccountMetadata>::new();
        let empty_address_book = Vec::<PublicAddressBookEntry>::new();
        let context = context(
            &chain,
            &registry,
            &rates,
            &empty_accounts,
            &empty_address_book,
        );
        let (known, _) =
            resolve_token_amount(context, Some(token), U256::from(1_500_000_u64), false);
        assert!(
            matches!(&known, WalletConnectAmount::KnownToken { decimals: 6, symbol, usd: None, .. } if symbol == "USDC")
        );
        assert_eq!(
            walletconnect_token_contract_recognition(&known),
            "In token list"
        );
        let (unknown, _) = resolve_token_amount(
            context,
            Some(Address::from([0x22; 20])),
            U256::from(123),
            false,
        );
        assert!(
            matches!(&unknown, WalletConnectAmount::RawToken { raw, unrecognised_contract: true, decimals_unknown: true, .. } if *raw == U256::from(123))
        );
        assert_eq!(
            walletconnect_token_contract_recognition(&unknown),
            "Unrecognised token contract; decimals unknown"
        );
    }

    #[test]
    fn native_and_wrapped_trust_use_the_configured_target() {
        let wrapped = Address::from([0x33; 20]);
        let chain = chain(Some(&wrapped.to_string()));
        assert!(is_trusted_wrapped_native(&chain, Some(wrapped)));
        assert!(!is_trusted_wrapped_native(
            &chain,
            Some(Address::from([0x44; 20]))
        ));
        let registry = EffectiveTokenRegistry {
            tokens: BTreeMap::new(),
        };
        let rates = TokenAnchorRateCache::new();
        let empty_accounts = Vec::<PublicAccountMetadata>::new();
        let empty_address_book = Vec::<PublicAddressBookEntry>::new();
        let native = native_amount(
            context(
                &chain,
                &registry,
                &rates,
                &empty_accounts,
                &empty_address_book,
            ),
            U256::from(1),
        );
        assert_eq!(native.symbol, native_token_display_label(1));
        assert!(native.usd.is_none());
    }

    #[test]
    fn unlimited_is_exact_and_large_allowance_stays_finite() {
        let token = Address::from([0x11; 20]);
        let chain = chain(None);
        let registry = registry(token);
        let rates = TokenAnchorRateCache::new();
        let empty_accounts = Vec::<PublicAccountMetadata>::new();
        let empty_address_book = Vec::<PublicAddressBookEntry>::new();
        let context = context(
            &chain,
            &registry,
            &rates,
            &empty_accounts,
            &empty_address_book,
        );
        let (unlimited, _) = resolve_token_amount(context, Some(token), U256::MAX, true);
        let (finite, _) =
            resolve_token_amount(context, Some(token), U256::MAX - U256::from(1), true);
        assert!(matches!(unlimited, WalletConnectAmount::Unlimited { .. }));
        assert!(matches!(finite, WalletConnectAmount::KnownToken { .. }));
    }

    #[test]
    fn party_identity_follows_ownership_then_address_book_precedence() {
        let selected = Address::from([1; 20]);
        let owned = Address::from([2; 20]);
        let contact = Address::from([3; 20]);
        let unknown = Address::from([4; 20]);
        let accounts = vec![
            account(selected, Some("Selected"), PublicAccountStatus::Active),
            account(
                owned,
                Some("Inactive global"),
                PublicAccountStatus::Inactive,
            ),
        ];
        let address_book = vec![
            PublicAddressBookEntry {
                entry_uuid: "owned-book".to_owned(),
                label: "Book owned".to_owned(),
                address: owned,
                display_order: 0,
            },
            PublicAddressBookEntry {
                entry_uuid: "contact".to_owned(),
                label: "Saved contact".to_owned(),
                address: contact,
                display_order: 1,
            },
        ];
        assert!(matches!(
            party_for(
                WalletConnectPartyRole::Recipient,
                selected,
                selected,
                &accounts,
                &address_book
            )
            .badge,
            WalletConnectPartyBadge::ThisWallet
        ));
        assert!(
            matches!(party_for(WalletConnectPartyRole::Recipient, owned, selected, &accounts, &address_book).badge, WalletConnectPartyBadge::YourAccount { label: Some(label) } if label == "Inactive global")
        );
        assert!(
            matches!(party_for(WalletConnectPartyRole::Recipient, contact, selected, &accounts, &address_book).badge, WalletConnectPartyBadge::AddressBook { label } if label == "Saved contact")
        );
        assert!(matches!(
            party_for(
                WalletConnectPartyRole::Recipient,
                unknown,
                selected,
                &accounts,
                &address_book
            )
            .badge,
            WalletConnectPartyBadge::NotInAddressBook
        ));
        assert!(
            walletconnect_party_badge_label(
                WalletConnectPartyRole::Spender,
                &party_for(
                    WalletConnectPartyRole::Spender,
                    unknown,
                    selected,
                    &accounts,
                    &address_book,
                )
                .badge,
            )
            .is_none()
        );
        assert!(
            walletconnect_party_badge_label(
                WalletConnectPartyRole::Contract,
                &party_for(
                    WalletConnectPartyRole::Contract,
                    unknown,
                    selected,
                    &accounts,
                    &address_book,
                )
                .badge,
            )
            .is_none()
        );
        assert!(matches!(
            party_for(
                WalletConnectPartyRole::Spender,
                selected,
                selected,
                &accounts,
                &address_book,
            )
            .badge,
            WalletConnectPartyBadge::ThisWallet
        ));
        assert!(matches!(
            party_for(
                WalletConnectPartyRole::Spender,
                contact,
                selected,
                &accounts,
                &address_book,
            )
            .badge,
            WalletConnectPartyBadge::AddressBook { label } if label == "Saved contact"
        ));
    }

    #[test]
    fn undecoded_attached_native_value_uses_chain_hero_model() {
        let account = Address::from([0x11; 20]);
        let target = Address::from([0x22; 20]);
        let chain = chain(None);
        let registry = registry(target);
        let rates = TokenAnchorRateCache::new();
        let request = request_with(
            WalletConnectParsedRequest::EthSendTransaction {
                transaction: transaction(account, Some(target), U256::from(1)),
            },
            Some(WalletConnectDecodedTransaction {
                target: Some(target),
                native_value: U256::from(1),
                kind: WalletConnectDecodedCallKind::ContractCall {
                    selector: Some([0xaa, 0xbb, 0xcc, 0xdd]),
                },
            }),
        );
        let intent =
            build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));

        assert!(matches!(
            intent.hero.summary,
            WalletConnectHeroSummary::UndecodedCall { .. }
        ));
        assert_eq!(intent.authorization, "Contract call");
        assert!(intent.attached_native.is_some());
        assert!(intent.icon.is_some());
        let summary =
            super::super::requests::walletconnect_request_authorization_summary(&request, &intent);
        let rows = summary.rows_for_test();
        assert!(rows.contains(&("Contract".to_owned(), target.to_checksum(None))));
        assert!(summary.warnings_for_test().iter().any(|warning| {
            warning.contains("Undecoded contract call") && warning.contains("0xaabbccdd")
        }));
    }

    #[test]
    fn personal_previews_are_safe_bounded_or_byte_summaries() {
        assert_eq!(
            personal_message_summary(b"hello").preview.as_deref(),
            Some("hello")
        );
        assert_eq!(
            personal_message_summary(&walletconnect_personal_message_bytes("0x6869"))
                .preview
                .as_deref(),
            Some("hi")
        );
        assert!(personal_message_summary(&[0, 1, 2]).preview.is_none());
        assert!(
            personal_message_summary(b"a\n\n\n\n\n\n\n\n\n\n\n")
                .preview
                .is_none()
        );
        let long = "x".repeat(PERSONAL_SIGN_PREVIEW_MAX_CHARS + 10);
        let preview = personal_message_summary(long.as_bytes())
            .preview
            .expect("preview");
        assert_eq!(preview.chars().count(), PERSONAL_SIGN_PREVIEW_MAX_CHARS);
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn typed_data_domain_fallback_is_neutral() {
        let named =
            typed_data_summary(&json!({"domain": {"name": "Permit"}, "primaryType": "Permit"}));
        assert_eq!(named.domain_name.as_deref(), Some("Permit"));
        assert_eq!(named.primary_type, "Permit");
        let missing = typed_data_summary(&json!({"domain": {}, "primaryType": "Message"}));
        assert_eq!(missing.domain_name, None);
        assert_eq!(missing.primary_type, "Message");
    }

    fn permit_typed_data(token: Address, owner: Address, spender: Address, value: &Value) -> Value {
        json!({
            "types": {
                "EIP712Domain": [
                    {"name": "name", "type": "string"},
                    {"name": "version", "type": "string"},
                    {"name": "chainId", "type": "uint256"},
                    {"name": "verifyingContract", "type": "address"}
                ],
                "Permit": [
                    {"name": "owner", "type": "address"},
                    {"name": "spender", "type": "address"},
                    {"name": "value", "type": "uint256"},
                    {"name": "nonce", "type": "uint256"},
                    {"name": "deadline", "type": "uint256"}
                ]
            },
            "primaryType": "Permit",
            "domain": {
                "name": "USD Coin",
                "version": "2",
                "chainId": 1,
                "verifyingContract": token.to_string()
            },
            "message": {
                "owner": owner.to_string(),
                "spender": spender.to_string(),
                "value": value,
                "nonce": 0,
                "deadline": "0x64"
            }
        })
    }

    #[test]
    fn canonical_eip2612_permit_projects_as_a_typed_approval() {
        let account = Address::from([0x11; 20]);
        let token = Address::from([0x22; 20]);
        let spender = Address::from([0x33; 20]);
        let chain = chain(None);
        let registry = registry(token);
        let rates = TokenAnchorRateCache::new();
        let request = request_with(
            WalletConnectParsedRequest::EthSignTypedDataV4 {
                account,
                typed_data: permit_typed_data(token, account, spender, &json!("1000000")),
                domain_chain_id: Some(U256::from(1)),
            },
            None,
        );
        let intent =
            build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));

        assert_eq!(intent.action, WalletConnectIntentAction::Approve);
        assert!(matches!(
            &intent.amount,
            WalletConnectAmount::KnownToken {
                raw,
                decimals: 6,
                symbol,
                ..
            } if *raw == U256::from(1_000_000_u64) && symbol == "USDC"
        ));
        assert!(matches!(
            &intent.hero.summary,
            WalletConnectHeroSummary::TypedData(summary)
                if summary.domain_name.as_deref() == Some("USD Coin")
                    && summary.primary_type == "Permit"
        ));
        assert!(matches!(
            intent.parties.as_slice(),
            [WalletConnectParty {
                role: WalletConnectPartyRole::Spender,
                address,
                ..
            }] if *address == spender
        ));
        assert!(intent.authorization.contains("1 USDC"));
        assert!(!intent.authorization.contains("Spender can withdraw up to"));
        let summary =
            super::super::requests::walletconnect_request_authorization_summary(&request, &intent);
        assert!(
            summary
                .rows_for_test()
                .contains(&("Typed data".to_owned(), "USD Coin / Permit".to_owned()))
        );
    }

    #[test]
    fn noncanonical_or_foreign_permit_keeps_generic_typed_data_behavior() {
        let account = Address::from([0x11; 20]);
        let token = Address::from([0x22; 20]);
        let spender = Address::from([0x33; 20]);
        let foreign_owner = Address::from([0x44; 20]);
        let chain = chain(None);
        let registry = registry(token);
        let rates = TokenAnchorRateCache::new();
        let request = request_with(
            WalletConnectParsedRequest::EthSignTypedDataV4 {
                account,
                typed_data: permit_typed_data(
                    token,
                    foreign_owner,
                    spender,
                    &json!(U256::MAX.to_string()),
                ),
                domain_chain_id: Some(U256::from(1)),
            },
            None,
        );
        let intent =
            build_walletconnect_intent(&request, context(&chain, &registry, &rates, &[], &[]));

        assert_eq!(intent.action, WalletConnectIntentAction::TypedDataSign);
        assert!(matches!(
            intent.hero.summary,
            WalletConnectHeroSummary::TypedData(_)
        ));
        assert!(matches!(
            intent.parties.as_slice(),
            [WalletConnectParty {
                role: WalletConnectPartyRole::Signer,
                address,
                ..
            }] if *address == account
        ));
        assert!(intent.risks.is_empty());
    }

    #[test]
    fn token_risks_cover_maximum_foreign_and_attached_value_rules() {
        let selected = Address::from([1; 20]);
        let spender = Address::from([2; 20]);
        let foreign = Address::from([3; 20]);
        let native = WalletConnectNativeAmount {
            raw: U256::from(1),
            symbol: "ETH".to_owned(),
            display: "1 ETH".to_owned(),
            usd: None,
        };
        let allowance = token_operation_risks(
            &WalletConnectDecodedCallKind::Erc20Approve {
                spender,
                amount: U256::MAX,
            },
            Some(&native),
            selected,
        );
        assert!(matches!(
            allowance.as_slice(),
            [
                WalletConnectRisk::UnlimitedAllowance { .. },
                WalletConnectRisk::AttachedNativeValue(_)
            ]
        ));
        let equal_source = token_operation_risks(
            &WalletConnectDecodedCallKind::Erc20TransferFrom {
                from: selected,
                to: Address::ZERO,
                amount: U256::from(1),
            },
            None,
            selected,
        );
        assert!(equal_source.is_empty());
        let foreign_source = token_operation_risks(
            &WalletConnectDecodedCallKind::Erc20TransferFrom {
                from: foreign,
                to: Address::ZERO,
                amount: U256::from(1),
            },
            None,
            selected,
        );
        assert!(
            matches!(foreign_source.as_slice(), [WalletConnectRisk::ForeignTransferSource { source }] if *source == foreign)
        );
    }

    #[test]
    fn fee_significance_omits_missing_or_zero_values() {
        for (expected, moving) in [
            (None, Some(U256::from(100))),
            (Some(U256::from(25)), None),
            (Some(U256::from(25)), Some(U256::ZERO)),
        ] {
            assert_eq!(
                classify_walletconnect_fee_significance(expected, moving),
                WalletConnectFeeSignificance::Normal
            );
        }
    }

    #[test]
    fn moving_value_requires_finite_known_amount_and_cached_price() {
        let token = Address::from([0x55; 20]);
        let rates = TokenAnchorRateCache::new();
        rates.store_native_usd_rate(1, U256::from(1_000_000_u64));
        rates.store_rate(1, token, U256::from(1_000_000_000_000_000_000_u64));
        let known_token = WalletConnectAmount::KnownToken {
            token,
            raw: U256::from(1_000_000_000_000_000_000_u128),
            decimals: 18,
            symbol: "TOK".to_owned(),
            display: "1 TOK".to_owned(),
            usd: None,
        };
        assert!(
            walletconnect_moving_usd_micro_value(
                1,
                WalletConnectIntentAction::TokenTransfer,
                &known_token,
                None,
                &rates,
            )
            .is_some()
        );
        let native = WalletConnectAmount::Native {
            raw: U256::from(1_000_000_000_000_000_000_u128),
            symbol: "ETH".to_owned(),
            display: "1 ETH".to_owned(),
            usd: None,
        };
        let attached_native = WalletConnectNativeAmount {
            raw: U256::from(1_000_000_000_000_000_000_u128),
            symbol: "ETH".to_owned(),
            display: "1 ETH".to_owned(),
            usd: None,
        };
        assert_eq!(
            walletconnect_moving_usd_micro_value(
                1,
                WalletConnectIntentAction::NativeTransfer,
                &native,
                Some(&attached_native),
                &rates,
            ),
            Some(U256::from(1_000_000_u64))
        );
        assert_eq!(
            walletconnect_moving_usd_micro_value(
                1,
                WalletConnectIntentAction::Wrap,
                &native,
                Some(&attached_native),
                &rates,
            ),
            Some(U256::from(1_000_000_u64))
        );
        assert_eq!(
            walletconnect_moving_usd_micro_value(
                1,
                WalletConnectIntentAction::TokenTransfer,
                &known_token,
                Some(&attached_native),
                &rates,
            ),
            Some(U256::from(2_000_000_u64))
        );
        let attached_without_cached_value = WalletConnectNativeAmount {
            raw: U256::MAX,
            symbol: "ETH".to_owned(),
            display: "unavailable".to_owned(),
            usd: None,
        };
        assert!(
            walletconnect_moving_usd_micro_value(
                1,
                WalletConnectIntentAction::TokenTransfer,
                &known_token,
                Some(&attached_without_cached_value),
                &rates,
            )
            .is_none()
        );
        assert!(
            walletconnect_moving_usd_micro_value(
                1,
                WalletConnectIntentAction::NativeTransfer,
                &native,
                None,
                &rates,
            )
            .is_some()
        );
        let raw = WalletConnectAmount::RawToken {
            token,
            raw: U256::from(1),
            display: "1 raw token unit".to_owned(),
            unrecognised_contract: true,
            decimals_unknown: true,
        };
        assert!(
            walletconnect_moving_usd_micro_value(
                1,
                WalletConnectIntentAction::TokenTransfer,
                &raw,
                None,
                &rates,
            )
            .is_none()
        );
        assert!(
            walletconnect_moving_usd_micro_value(
                1,
                WalletConnectIntentAction::Approve,
                &known_token,
                None,
                &rates,
            )
            .is_none()
        );
    }
}
