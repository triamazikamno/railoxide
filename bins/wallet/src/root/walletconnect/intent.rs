use alloy::dyn_abi::{DynSolValue, TypedData};
use alloy::primitives::{Address, U256};
use gpui::{Pixels, px};
use railgun_ui::{format_usd_micro_value, native_currency_icon_asset_path};
use serde_json::Value;
use wallet_ops::{
    TokenAnchorRateCache, WalletConnectDecodedCallKind, WalletConnectDecodedTransaction,
    WalletConnectEvmTransaction, WalletConnectParsedRequest,
    settings::{EffectiveChainConfig, EffectiveTokenRegistry},
    vault::{PublicAccountMetadata, PublicAddressBookEntry},
};

use crate::assets::WalletIconSource;
use crate::root::{format_token_amount_for_display, token_display_metadata};

use super::{
    WalletConnectRequestUi, fee::WalletConnectFeeStatus,
    helpers::walletconnect_transaction_selector, requests::walletconnect_personal_message_bytes,
};

const PERSONAL_SIGN_PREVIEW_MAX_CHARS: usize = 160;
pub(super) const WALLETCONNECT_CRITICAL_PARTY_FULL_ADDRESS_MIN_WIDTH: Pixels = px(500.0);

#[derive(Clone, Copy)]
pub(super) struct WalletConnectIntentContext<'a> {
    pub(super) chain: &'a EffectiveChainConfig,
    pub(super) selected_chain_id: u64,
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
    ChainAdd,
    WatchAsset,
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
            Self::ChainAdd => "Confirm configured chain",
            Self::WatchAsset => "Watch token",
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
    Policy(String),
    ChainSwitch {
        from_chain_id: u64,
        to_chain_id: u64,
    },
    Amount,
    PersonalMessage(WalletConnectPersonalMessageSummary),
    TypedData(WalletConnectTypedDataSummary),
    UndecodedCall {
        selector: Option<[u8; 4]>,
    },
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
struct WalletConnectTokenPermit {
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
        | WalletConnectIntentAction::ChainSwitch
        | WalletConnectIntentAction::ChainAdd
        | WalletConnectIntentAction::WatchAsset => None,
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
    pub(super) chain_id: u64,
    pub(super) native_currency: railgun_ui::NativeCurrency,
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
    let selected_account = request.item.account.expect("account-bound intent");
    let mut chain_id = context.chain.chain_id;
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
            if let Some(permit) = parse_token_permit(typed_data, selected_account) {
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
        WalletConnectParsedRequest::WalletAddEthereumChain {
            chain_id,
            definition,
            ..
        } => {
            action = WalletConnectIntentAction::ChainAdd;
            hero_summary = WalletConnectHeroSummary::Policy(definition.as_ref().map_or_else(
                || format!("Use saved configuration for chain {chain_id}"),
                |chain| format!("Add {} ({chain_id})", chain.name),
            ));
        }
        WalletConnectParsedRequest::WalletWatchAsset { address, .. } => {
            action = WalletConnectIntentAction::WatchAsset;
            hero_summary = WalletConnectHeroSummary::Policy(format!(
                "Token {address} on chain {}",
                context.chain.chain_id
            ));
        }
        WalletConnectParsedRequest::WalletSwitchEthereumChain {
            chain_id: target_chain_id,
        } => {
            action = WalletConnectIntentAction::ChainSwitch;
            chain_id = *target_chain_id;
            hero_summary = WalletConnectHeroSummary::ChainSwitch {
                from_chain_id: if context.selected_chain_id == chain_id {
                    context.chain.chain_id
                } else {
                    context.selected_chain_id
                },
                to_chain_id: chain_id,
            };
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
        native_currency: context.chain.native_currency.clone(),
        action,
        chain_id,
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
        provenance: walletconnect_peer_provenance(
            &request.binding.peer_name,
            &request.binding.peer_url,
        ),
    }
}

fn walletconnect_peer_provenance(name: &str, url: &str) -> WalletConnectPeerProvenance {
    let site = reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "Unknown peer site".to_owned());
    WalletConnectPeerProvenance {
        site,
        dapp_name: sanitize_walletconnect_dapp_name(name),
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
    let selected_account = request.item.account.expect("account-bound intent");

    let Some(kind) = decoded.map(|decoded| &decoded.kind) else {
        if resolved.attached_native.is_some() {
            resolved.icon = native_currency_icon_asset_path(context.chain.chain_id)
                .map(WalletIconSource::embedded);
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
                symbol: context.chain.native_currency.symbol.clone(),
                display: context.chain.native_currency.format_amount(native_value),
                usd: context
                    .anchor_rates
                    .cached_native_usd_micro_value(context.chain.chain_id, native_value)
                    .map(format_usd_micro_value),
            };
            resolved.icon = native_currency_icon_asset_path(context.chain.chain_id)
                .map(WalletIconSource::embedded);
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
                resolved.icon = native_currency_icon_asset_path(context.chain.chain_id)
                    .map(WalletIconSource::embedded);
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
            native_currency_icon_asset_path(context.chain.chain_id).map(WalletIconSource::embedded);
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
        symbol: context.chain.native_currency.symbol.clone(),
        display: context.chain.native_currency.format_amount(raw),
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

/// Reads an EIP-2612 or DAI-style permit from the values Alloy signs, so JSON the signer ignores
/// cannot hide it.
fn parse_token_permit(
    value: &Value,
    selected_account: Address,
) -> Option<WalletConnectTokenPermit> {
    let typed_data: TypedData = serde_json::from_value(value.clone()).ok()?;
    let encode_type = typed_data.encode_type().ok()?;
    let token = typed_data.domain.verifying_contract?;
    let message = typed_data.coerce().ok()?;
    let (_, _, fields) = message.as_custom_struct()?;
    let (owner, spender, value) = match (encode_type.as_str(), fields) {
        (
            "Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)",
            [
                DynSolValue::Address(owner),
                DynSolValue::Address(spender),
                DynSolValue::Uint(value, 256),
                ..,
            ],
        ) => (*owner, *spender, *value),
        // DAI sets the allowance to `uint(-1)` when `allowed` is true and to zero otherwise.
        (
            "Permit(address holder,address spender,uint256 nonce,uint256 expiry,bool allowed)",
            [
                DynSolValue::Address(holder),
                DynSolValue::Address(spender),
                _,
                _,
                DynSolValue::Bool(allowed),
            ],
        ) => (
            *holder,
            *spender,
            if *allowed { U256::MAX } else { U256::ZERO },
        ),
        _ => return None,
    };
    (owner == selected_account).then_some(WalletConnectTokenPermit {
        token,
        spender,
        value,
    })
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
        WalletConnectHeroSummary::Policy(_)
        | WalletConnectHeroSummary::ChainSwitch { .. }
        | WalletConnectHeroSummary::TypedData(_)
        | WalletConnectHeroSummary::Amount
        | WalletConnectHeroSummary::UndecodedCall { .. }
        | WalletConnectHeroSummary::None => {}
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests;
