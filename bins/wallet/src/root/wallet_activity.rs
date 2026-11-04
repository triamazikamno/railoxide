//! The wallet's Activity tab: the selected wallet's journal entries, swaps from its executor
//! records and received rows on the selected network, in-flight ones pinned and the rest newest
//! first, under day headers, with the selected row's details beside the list, a toolbar that
//! searches and filters the rows, the clear-history dialog and the tab's unread marker.
//!
//! Journal reads and writes run off the UI thread. The wallet's entries, markers and executor
//! records are read when the wallet session changes and whenever the journal or the selected
//! chain's stealth accounts change. Rows are rebuilt from them in [`build_activity_rows`] when
//! that data, the selected chain's UTXO snapshot or the selected network changes, never per
//! frame.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use alloy::primitives::{Address, B256, U256};
use chrono::NaiveDate;
use gpui::{
    App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Task, Window, div, prelude::FluentBuilder as _,
    px, rems, rgb,
};
use gpui_component::{
    Disableable as _, IndexPath, Sizable as _, WindowExt as _,
    button::ButtonVariant,
    checkbox::Checkbox,
    description_list::{DescriptionItem, DescriptionList},
    input::InputState,
    list::{List, ListDelegate, ListItem, ListState},
    notification::Notification,
    radio::{Radio, RadioGroup},
    scroll::ScrollableElement as _,
    tag::Tag,
};
use railgun_ui::{format_token_amount, short_address, short_non_evm};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_input, app_muted_text, app_strong_text, app_text};
use ui::dialog::WalletDialogExt as _;
use ui::theme::{self, APP_MONO_FONT_FAMILY};
use wallet_ops::settings::EffectiveTokenRegistry;
use wallet_ops::tron::address::encode_tron_address;
use wallet_ops::vault::{
    ActivityAsset, ActivityAttemptOutcome, ActivityClear, ActivityClearAge, ActivityClearResult,
    ActivityCounterparty, ActivityDirection, ActivityEntry, ActivityEntryId, ActivityJournal,
    ActivityKind, ActivityLeg, ActivityMarkers, ActivityNetwork, ActivityOwner, ActivitySource,
    ActivityStep, ActivityStoreError, DesktopVaultStore, DesktopViewSession, ExecutorRecord,
    ExecutorStore, NonEvmAddressBookEntry, PrivateAddressBookEntry, PublicAccountMetadata,
    PublicAddressBookEntry, SwapUseId, subscribe_activity_changes,
};
use wallet_ops::{
    ActivityStatus, PublicAssetId, ReceivedKind, ReceivedRow, StatusInputs, SwapRow,
    SwapRowRecipient, TokenAnchorRateCache, UtxoOutput, entry_status, has_unread_received,
    is_in_flight, owned_hashes, received_rows, resolve_counterparty, running_activity_entries,
    swap_rows,
};

use super::public_balances::public_asset_icon_path;
use super::ui_helpers::{
    app_status_tag, centered_message, day_label, dialog_max_height, network_name, now_unix,
    styled_dialog_footer, token_icon_slot,
};
use super::{ChainUtxoState, SelectedNetwork, WalletRoot, secondary_dialog_content_width};
use crate::assets::WalletIconSource;

mod search;

use search::ActivitySearchState;
pub(super) use search::row_matches;
#[cfg(test)]
pub(super) use search::{
    ActivityChip, ActivityFacet, ActivityFilter, ActivitySearch, ActivityStatusChoice,
    ActivitySuggestionAction, activity_suggestions,
};

const CLEAR_DIALOG_WIDTH: gpui::Pixels = px(460.0);
const DEFAULT_CLEAR_DAYS: &str = "30";
/// Every row's height, in rems: the list measures one row and gives each the same.
const ACTIVITY_ROW_HEIGHT: f32 = 3.5;
const ACTIVITY_PANE_WIDTH: gpui::Rems = gpui::rems(20.0);
/// Fits a long amount with its value, such as `-23,239.3297 VSN ($835.03)`.
const ACTIVITY_AMOUNTS_WIDTH: gpui::Rems = gpui::rems(16.0);
/// Fits the widest status tag, `Unconfirmed`.
const ACTIVITY_STATUS_WIDTH: gpui::Rems = gpui::rems(6.5);

/// Every journal kind, in the order the clear dialog lists them.
pub(super) const ACTIVITY_KINDS: [ActivityKind; 8] = [
    ActivityKind::PrivateSend,
    ActivityKind::Unshield,
    ActivityKind::Swap,
    ActivityKind::Shield,
    ActivityKind::PublicSend,
    ActivityKind::ContractCall,
    ActivityKind::Governance,
    ActivityKind::Refund,
];

/// What Activity reads from the vault for one wallet session.
#[derive(Clone, Default)]
pub(super) struct ActivityData {
    /// The entries of the wallet and its in-scope accounts on every network, cleared stubs
    /// included.
    pub(super) entries: Vec<ActivityEntry>,
    pub(super) markers: ActivityMarkers,
    /// The wallet's executor records on every Railgun chain.
    pub(super) records: Vec<ExecutorRecord>,
    /// The swap rows those records hold, on every network, hidden ones included.
    pub(super) swaps: Vec<SwapRow>,
}

/// In-memory Activity data from the debug UI fixture or a test. Nothing is read from or
/// written to the vault while it is installed.
#[derive(Clone, Default)]
pub(in crate::root) struct ActivityStandIn {
    pub(in crate::root) data: ActivityData,
    /// The UTXO snapshot received rows and private statuses read, in place of the chain's.
    pub(in crate::root) utxos: Vec<UtxoOutput>,
    /// The entries treated as still submitting, in place of this process's.
    pub(in crate::root) running: BTreeSet<ActivityEntryId>,
    /// The row selected when it is installed.
    pub(in crate::root) selected: Option<ActivityRowId>,
    /// The EVM chain the stand-in was made for.
    pub(in crate::root) chain_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::root) enum ActivityRowId {
    Entry(ActivityEntryId),
    Received(B256),
    Swap(SwapUseId),
}

impl ActivityRowId {
    fn key(self) -> String {
        match self {
            Self::Entry(id) => format!("e-{}", id.opaque_id()),
            Self::Received(hash) => format!("r-{hash:x}"),
            Self::Swap(id) => format!("s-{}", id.opaque_id()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivityRowKind {
    Entry(ActivityKind),
    Received(ReceivedKind),
}

/// One detail line of the selected row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ActivityDetail {
    pub(super) label: &'static str,
    pub(super) value: String,
    /// The full address, hash or block number a copy button beside `value` copies.
    pub(super) copy: Option<String>,
}

impl ActivityDetail {
    /// An address or hash, shown short and copied in full.
    fn copyable(label: &'static str, full: String) -> Self {
        Self {
            label,
            value: shorten(&full),
            copy: Some(full),
        }
    }
}

/// One asset amount of a row, signed by direction.
#[derive(Clone, Debug)]
pub(super) struct ActivityAmount {
    pub(super) text: String,
    pub(super) incoming: bool,
    /// Its value at today's price, where there is one and it doesn't repeat the amount.
    pub(super) usd: Option<String>,
    /// Its value at today's price in micro-USD, shown or not, which `value:` chips compare.
    pub(super) usd_micro: Option<U256>,
    pub(super) icon: Option<WalletIconSource>,
    pub(super) asset: ActivityAsset,
    pub(super) symbol: String,
}

/// One Activity row, derived for display only.
#[derive(Clone, Debug)]
pub(super) struct ActivityRow {
    pub(super) id: ActivityRowId,
    pub(super) kind: ActivityRowKind,
    pub(super) in_flight: bool,
    /// `None` for a received row.
    pub(super) status: Option<ActivityStatus>,
    /// The local time of a journal entry or a swap's start, or the block time of a received row.
    pub(super) time: u64,
    /// The address-book label, else the shortened counterparty.
    pub(super) counterparty: Option<String>,
    /// The entry's counterparty or the swap's recipient address, which `with:` chips match.
    /// `None` for received rows.
    pub(super) with: Option<ActivityWith>,
    /// Every transaction hash its details copy, as [`hash_text`] gives them, which `hash:` chips
    /// match.
    pub(super) hashes: Vec<String>,
    /// The dapp or site the entry came from.
    pub(super) source: Option<String>,
    pub(super) amounts: Vec<ActivityAmount>,
    pub(super) details: Vec<ActivityDetail>,
    pub(super) swap_use: Option<SwapUseId>,
    /// What search matches, lowercased: see [`search_text`].
    pub(super) search: String,
}

/// A row's counterparty, as `with:` chips and suggestions use it.
#[derive(Clone, Debug)]
pub(super) struct ActivityWith {
    /// The full address, lowercased.
    pub(super) address: String,
    /// Its address-book label.
    pub(super) label: Option<String>,
    /// The full address shortened, in its own case.
    pub(super) short: String,
}

/// Address books and accounts that label counterparties and owners.
#[derive(Clone, Copy)]
pub(super) struct ActivityLabels<'a> {
    pub(super) private: &'a [PrivateAddressBookEntry],
    pub(super) public: &'a [PublicAddressBookEntry],
    pub(super) non_evm: &'a [NonEvmAddressBookEntry],
    /// Public and Tron accounts of the wallet, as the Public list shows them.
    pub(super) accounts: &'a [PublicAccountMetadata],
}

/// What [`build_activity_rows`] reads.
pub(super) struct ActivityRowsInput<'a> {
    pub(super) network: ActivityNetwork,
    pub(super) data: &'a ActivityData,
    /// The selected chain's UTXO snapshot, spent notes included; empty where there is none,
    /// as on a public-only chain or Tron.
    pub(super) utxos: &'a [UtxoOutput],
    pub(super) running: &'a BTreeSet<ActivityEntryId>,
    pub(super) labels: ActivityLabels<'a>,
    pub(super) tokens: Option<&'a EffectiveTokenRegistry>,
    /// The network's native symbol and decimals.
    pub(super) native: (&'a str, u8),
    /// Today's prices, which value the amounts.
    pub(super) prices: Option<&'a TokenAnchorRateCache>,
}

#[derive(Default)]
pub(super) struct ActivityRows {
    pub(super) rows: Vec<ActivityRow>,
    /// The newest received row's block, which opening Activity marks as seen.
    pub(super) newest_received_block: Option<u64>,
    pub(super) unread: bool,
}

/// Activity's rows on `input.network`: its journal entries, without cleared stubs, the swaps
/// it paid for, without cleared ones, and the received rows of `input.utxos`. Entries and
/// swaps still in flight come first, then everything newest first.
pub(super) fn build_activity_rows(input: &ActivityRowsInput<'_>) -> ActivityRows {
    let data = input.data;
    let owned = owned_hashes(&data.entries, &data.records);
    let received = if input.utxos.is_empty() {
        Vec::new()
    } else {
        received_rows(input.utxos, &owned, data.markers.received_hidden_before)
    };
    let unread = has_unread_received(
        &received,
        data.markers.last_seen.get(&input.network).copied(),
    );
    let newest_received_block = received.iter().map(|row| row.block_number).max();
    let status_inputs = StatusInputs {
        utxos: input.utxos,
        running: input.running,
        owned: &owned,
    };
    let mut rows: Vec<ActivityRow> = data
        .entries
        .iter()
        .filter(|entry| !entry.cleared && entry.network == input.network)
        .map(|entry| entry_row(entry, &status_inputs, input))
        .chain(
            data.swaps
                .iter()
                .filter(|swap| {
                    swap.network == input.network
                        && !data.markers.hidden_swap_uses.contains(&swap.id)
                })
                .map(|swap| swap_row(swap, input)),
        )
        .chain(received.iter().map(|row| received_row(row, input)))
        .collect();
    rows.sort_by_key(|row| (Reverse(row.in_flight), Reverse(row.time)));
    ActivityRows {
        rows,
        newest_received_block,
        unread,
    }
}

fn entry_row(
    entry: &ActivityEntry,
    status_inputs: &StatusInputs<'_>,
    input: &ActivityRowsInput<'_>,
) -> ActivityRow {
    let status = entry_status(entry, status_inputs);
    // Labels the row or its details show, which search matches.
    let mut labels = Vec::new();
    let mut details = vec![ActivityDetail {
        label: "Status",
        value: status_detail(status).to_owned(),
        copy: None,
    }];
    let into_this_wallet = entry.kind == ActivityKind::Shield
        && input.utxos.iter().any(|note| {
            note.source_tx_hash.parse::<B256>().is_ok_and(|hash| {
                entry
                    .attempts
                    .iter()
                    .any(|attempt| attempt.step == ActivityStep::Main && attempt.hash == hash)
            })
        });
    let with = entry.counterparty.as_ref().map(|counterparty| {
        let full = counterparty_text(counterparty);
        details.push(ActivityDetail::copyable(
            counterparty_detail_label(entry.kind),
            full.clone(),
        ));
        ActivityWith {
            label: resolve_counterparty(
                counterparty,
                input.labels.private,
                input.labels.public,
                input.labels.non_evm,
            )
            .map(str::to_owned),
            short: shorten(&full),
            address: full.to_lowercase(),
        }
    });
    let counterparty = with
        .as_ref()
        .map(|with| with.label.clone().unwrap_or_else(|| with.short.clone()));
    labels.extend(counterparty.clone());
    if into_this_wallet {
        details.push(ActivityDetail {
            label: "To",
            value: "This wallet".to_owned(),
            copy: None,
        });
    }
    if let ActivityOwner::Account(uuid) = &entry.owner {
        let account = input
            .labels
            .accounts
            .iter()
            .find(|account| account.public_account_uuid == *uuid);
        labels.extend(account.and_then(|account| account.label.clone()));
        details.push(ActivityDetail {
            label: "Account",
            value: account.map_or_else(
                || "Account no longer in this wallet".to_owned(),
                |account| account_text(account, input.network),
            ),
            copy: None,
        });
    }
    let source = match &entry.source {
        ActivitySource::Wallet => None,
        ActivitySource::WalletConnect { dapp } => Some(format!("WalletConnect · {dapp}")),
        ActivitySource::Gateway { origin } => Some(format!("Browser · {origin}")),
    };
    if let Some(source) = &source {
        details.push(ActivityDetail {
            label: "Source",
            value: source.clone(),
            copy: None,
        });
    }
    if let Some(fee) = entry.fee {
        let amount = asset_amount(fee.asset, fee.amount, input);
        details.push(ActivityDetail {
            label: "Fee",
            value: match asset_usd(fee.asset, fee.amount, input) {
                Some(usd) => format!("{amount} ({usd})"),
                None => amount,
            },
            copy: None,
        });
    }
    if let Some(payer) = entry.gas_payer {
        let label = input
            .labels
            .public
            .iter()
            .find(|book| book.address == payer)
            .map(|book| book.label.clone())
            .or_else(|| {
                input
                    .labels
                    .accounts
                    .iter()
                    .find(|account| account.address == payer)
                    .and_then(|account| account.label.clone())
            });
        let mut detail = ActivityDetail::copyable("Gas paid by", payer.to_checksum(None));
        if let Some(label) = label {
            detail.value = format!("{label} ({})", detail.value);
            labels.push(label);
        }
        details.push(detail);
    }
    let mut hashes = Vec::new();
    for attempt in &entry.attempts {
        let hash = hash_text(attempt.hash, input.network);
        hashes.push(hash.clone());
        details.push(ActivityDetail::copyable(step_label(attempt.step), hash));
        if let Some(outcome) = attempt.outcome {
            details.push(outcome_detail(outcome));
        }
    }
    if let ActivityStatus::Confirmed(Some(confirmation)) = status
        && !entry
            .attempts
            .iter()
            .any(|attempt| attempt.hash == confirmation.hash)
    {
        // Private sync saw the inputs spent by a transaction the entry didn't record: a relayed
        // send's own, or another one that spent them.
        let hash = hash_text(confirmation.hash, input.network);
        hashes.push(hash.clone());
        details.push(ActivityDetail::copyable(
            if entry.attempts.is_empty() {
                "Transaction"
            } else {
                "Spent in"
            },
            hash,
        ));
    }
    ActivityRow {
        id: ActivityRowId::Entry(entry.id),
        kind: ActivityRowKind::Entry(entry.kind),
        in_flight: is_in_flight(entry, status_inputs),
        status: Some(status),
        time: entry.created_at,
        counterparty: if into_this_wallet {
            Some("Into this wallet".to_owned())
        } else {
            counterparty
        },
        with,
        hashes,
        source,
        amounts: entry
            .legs
            .iter()
            .map(|leg| leg_amount(leg, input))
            .collect(),
        details,
        swap_use: None,
        search: String::new(),
    }
    .searchable(&labels, input.network)
}

impl ActivityRow {
    fn searchable(mut self, labels: &[String], network: ActivityNetwork) -> Self {
        self.search = search_text(&self, labels, network);
        self
    }

    /// The `type:` choice that holds this row.
    const fn clear_kind(&self) -> ActivityClearKind {
        match self.kind {
            ActivityRowKind::Entry(kind) => ActivityClearKind::Entry(kind),
            ActivityRowKind::Received(_) => ActivityClearKind::Received,
        }
    }

    /// Its largest priced amount's value in micro-USD, which `value:` chips compare.
    fn usd_micro(&self) -> Option<U256> {
        self.amounts
            .iter()
            .filter_map(|amount| amount.usd_micro)
            .max()
    }
}

/// A token's contract as the network writes it; `None` for the native coin.
fn contract_text(asset: ActivityAsset, network: ActivityNetwork) -> Option<String> {
    match asset {
        ActivityAsset::Native => None,
        ActivityAsset::Token(token) => Some(match network {
            ActivityNetwork::Tron => encode_tron_address(token),
            ActivityNetwork::Evm(_) => token.to_checksum(None),
        }),
    }
}

/// What search matches in `row`, lowercased: its operation, counterparty and source as shown,
/// `labels`, every full address and hash its details copy, and its tokens' symbols and
/// contracts. Never an amount.
fn search_text(row: &ActivityRow, labels: &[String], network: ActivityNetwork) -> String {
    let tokens = row.amounts.iter().flat_map(|amount| {
        std::iter::once(amount.symbol.clone()).chain(contract_text(amount.asset, network))
    });
    std::iter::once(row_kind_label(row.kind).to_owned())
        .chain(row.counterparty.clone())
        .chain(row.source.clone())
        .chain(labels.iter().cloned())
        .chain(row.details.iter().filter_map(|detail| detail.copy.clone()))
        .chain(tokens)
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
}

fn swap_row(swap: &SwapRow, input: &ActivityRowsInput<'_>) -> ActivityRow {
    let hashes: Vec<String> = swap
        .hashes
        .iter()
        .map(|hash| hash_text(*hash, input.network))
        .collect();
    let mut details = vec![ActivityDetail {
        label: "Status",
        value: status_detail(swap.status).to_owned(),
        copy: None,
    }];
    // The recipient's full address and address-book label, and the network a bridge delivers on.
    let (recipient, destination) = match &swap.recipient {
        None => (None, None),
        Some(SwapRowRecipient::ThisWallet { chain_id }) => (None, Some(network_name(*chain_id))),
        Some(SwapRowRecipient::Evm { address, chain_id }) => (
            Some((
                address.to_checksum(None),
                resolve_counterparty(
                    &ActivityCounterparty::Evm(*address),
                    input.labels.private,
                    input.labels.public,
                    input.labels.non_evm,
                ),
            )),
            (swap.network != ActivityNetwork::Evm(*chain_id)).then(|| network_name(*chain_id)),
        ),
        // Book entries and swap receivers are both in the network's canonical form.
        Some(SwapRowRecipient::NonEvm { network, address }) => (
            Some((
                address.clone(),
                input
                    .labels
                    .non_evm
                    .iter()
                    .find(|entry| entry.network == *network && entry.address == *address)
                    .map(|entry| entry.label.as_str()),
            )),
            Some(network.display_name().to_owned()),
        ),
    };
    let with = recipient.map(|(full, label)| {
        details.push(ActivityDetail::copyable("To", full.clone()));
        ActivityWith {
            label: label.map(str::to_owned),
            short: shorten(&full),
            address: full.to_lowercase(),
        }
    });
    let into_this_wallet = matches!(swap.recipient, Some(SwapRowRecipient::ThisWallet { .. }));
    if into_this_wallet {
        details.push(ActivityDetail {
            label: "To",
            value: "This wallet".to_owned(),
            copy: None,
        });
    }
    if let Some(destination) = &destination {
        details.push(ActivityDetail {
            label: "Destination",
            value: destination.clone(),
            copy: None,
        });
    }
    let counterparty = if into_this_wallet {
        destination
            .as_ref()
            .map(|name| format!("Into this wallet on {name}"))
    } else {
        with.as_ref().map(|with| {
            let shown = with.label.as_deref().unwrap_or(&with.short);
            destination
                .as_ref()
                .map_or_else(|| shown.to_owned(), |name| format!("{shown} · {name}"))
        })
    };
    let labels: Vec<String> = with
        .as_ref()
        .and_then(|with| with.label.clone())
        .into_iter()
        .collect();
    details.extend(
        hashes
            .iter()
            .map(|hash| ActivityDetail::copyable("Transaction", hash.clone())),
    );
    ActivityRow {
        id: ActivityRowId::Swap(swap.id),
        kind: ActivityRowKind::Entry(ActivityKind::Swap),
        in_flight: swap.in_flight,
        status: Some(swap.status),
        time: swap.started_at,
        counterparty,
        with,
        hashes,
        source: None,
        amounts: swap.legs.iter().map(|leg| leg_amount(leg, input)).collect(),
        details,
        swap_use: Some(swap.id),
        search: String::new(),
    }
    .searchable(&labels, input.network)
}

fn received_row(row: &ReceivedRow, input: &ActivityRowsInput<'_>) -> ActivityRow {
    let hash = hash_text(row.hash, input.network);
    let mut details = vec![
        ActivityDetail::copyable("Transaction", hash.clone()),
        ActivityDetail {
            label: "Block",
            value: row.block_number.to_string(),
            copy: Some(row.block_number.to_string()),
        },
    ];
    if row.kind == (ReceivedKind::ShieldReceived { blocked: true }) {
        details.push(ActivityDetail {
            label: "Note",
            value: "A blocked Shield can't be spent privately. The UTXOs tab shows whether it can be refunded.".to_owned(),
            copy: None,
        });
    }
    ActivityRow {
        id: ActivityRowId::Received(row.hash),
        kind: ActivityRowKind::Received(row.kind),
        in_flight: false,
        status: None,
        time: row.block_timestamp,
        counterparty: None,
        with: None,
        hashes: vec![hash],
        source: None,
        amounts: row.legs.iter().map(|leg| leg_amount(leg, input)).collect(),
        details,
        swap_use: None,
        search: String::new(),
    }
    .searchable(&[], input.network)
}

const fn kind_label(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::PrivateSend => "Private send",
        ActivityKind::Unshield => "Unshield",
        ActivityKind::Swap => "Swap",
        ActivityKind::Shield => "Shield",
        ActivityKind::PublicSend => "Public send",
        ActivityKind::ContractCall => "Contract call",
        ActivityKind::Governance => "Governance",
        ActivityKind::Refund => "Refund",
    }
}

const fn row_kind_label(kind: ActivityRowKind) -> &'static str {
    match kind {
        ActivityRowKind::Entry(kind) => kind_label(kind),
        ActivityRowKind::Received(ReceivedKind::Received) => "Received",
        ActivityRowKind::Received(ReceivedKind::ShieldReceived { .. }) => "Shield received",
    }
}

const fn counterparty_detail_label(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::ContractCall | ActivityKind::Governance => "Contract",
        ActivityKind::PrivateSend
        | ActivityKind::Unshield
        | ActivityKind::Swap
        | ActivityKind::Shield
        | ActivityKind::PublicSend
        | ActivityKind::Refund => "To",
    }
}

const fn step_label(step: ActivityStep) -> &'static str {
    match step {
        ActivityStep::Wrap => "Wrap",
        ActivityStep::Approve => "Approval",
        ActivityStep::Undelegate => "Undelegation",
        ActivityStep::Main => "Transaction",
    }
}

/// The status badge's label and color.
const fn status_badge(status: ActivityStatus) -> (&'static str, u32) {
    match status {
        ActivityStatus::Submitted => ("Submitted", theme::TEXT_MUTED),
        ActivityStatus::Unconfirmed => ("Unconfirmed", theme::WARNING),
        ActivityStatus::Confirmed(_) => ("Confirmed", theme::SUCCESS),
        ActivityStatus::Failed { .. } => ("Failed", theme::DANGER),
        ActivityStatus::Incomplete => ("Incomplete", theme::WARNING),
    }
}

const fn status_detail(status: ActivityStatus) -> &'static str {
    match status {
        ActivityStatus::Submitted => {
            "Submitted. The wallet hasn't observed it on chain, which doesn't mean it failed."
        }
        ActivityStatus::Unconfirmed => {
            "The broadcaster didn't answer. Private sync confirms it if its notes are spent."
        }
        ActivityStatus::Confirmed(_) => "Confirmed.",
        ActivityStatus::Failed { fee_spent: true } => "Failed. The network fee was spent.",
        ActivityStatus::Failed { fee_spent: false } => "Failed. No network fee was spent.",
        ActivityStatus::Incomplete => {
            "Incomplete. Only some of its steps ran, or it stopped before finishing."
        }
    }
}

/// An attempt's block, copied without the revert note.
fn outcome_detail(outcome: ActivityAttemptOutcome) -> ActivityDetail {
    let (value, copy) = match outcome {
        ActivityAttemptOutcome::Included { block, success, .. } => (
            if success {
                block.to_string()
            } else {
                format!("{block}, reverted")
            },
            Some(block.to_string()),
        ),
        ActivityAttemptOutcome::Dropped => ("None, it expired".to_owned(), None),
    };
    ActivityDetail {
        label: "Block",
        value,
        copy,
    }
}

fn counterparty_text(counterparty: &ActivityCounterparty) -> String {
    match counterparty {
        ActivityCounterparty::Private(address) => address.clone(),
        ActivityCounterparty::Evm(address) => address.to_checksum(None),
        ActivityCounterparty::Tron(address) => encode_tron_address(*address),
    }
}

fn shorten(full: &str) -> String {
    match full.parse::<Address>() {
        Ok(address) if full.starts_with("0x") => short_address(&address),
        _ => short_non_evm(full),
    }
}

fn account_text(account: &PublicAccountMetadata, network: ActivityNetwork) -> String {
    let address = match network {
        ActivityNetwork::Tron => encode_tron_address(account.address),
        ActivityNetwork::Evm(_) => account.address.to_checksum(None),
    };
    match &account.label {
        Some(label) => format!("{label} ({})", shorten(&address)),
        None => shorten(&address),
    }
}

/// Tron transaction ids are hex without a prefix.
fn hash_text(hash: B256, network: ActivityNetwork) -> String {
    match network {
        ActivityNetwork::Tron => alloy::hex::encode(hash),
        ActivityNetwork::Evm(_) => format!("{hash:#x}"),
    }
}

fn leg_amount(leg: &ActivityLeg, input: &ActivityRowsInput<'_>) -> ActivityAmount {
    let incoming = leg.direction == ActivityDirection::In;
    ActivityAmount {
        text: format!(
            "{}{}",
            if incoming { "+" } else { "-" },
            asset_amount(leg.asset, leg.amount, input)
        ),
        incoming,
        usd: asset_usd(leg.asset, leg.amount, input),
        usd_micro: asset_usd_micro(leg.asset, leg.amount, input),
        icon: asset_icon(leg.asset, input),
        asset: leg.asset,
        symbol: asset_symbol(leg.asset, input),
    }
}

fn asset_symbol(asset: ActivityAsset, input: &ActivityRowsInput<'_>) -> String {
    match asset {
        ActivityAsset::Native => input.native.0.to_owned(),
        ActivityAsset::Token(token) => match input.network {
            ActivityNetwork::Evm(chain_id) => {
                super::token_display_label(chain_id, token, input.tokens)
            }
            ActivityNetwork::Tron => {
                super::token_display_label(railgun_ui::TRON_CHAIN_ID, token, None)
            }
        },
    }
}

/// `amount` of `asset` in micro-USD at today's price.
fn asset_usd_micro(
    asset: ActivityAsset,
    amount: U256,
    input: &ActivityRowsInput<'_>,
) -> Option<U256> {
    let prices = input.prices?;
    let chain_id = asset_chain_id(input.network);
    match asset {
        ActivityAsset::Native => prices.cached_native_usd_micro_value(chain_id, amount),
        ActivityAsset::Token(token) => prices.cached_token_usd_micro_value(chain_id, token, amount),
    }
}

/// `amount` of `asset` in USD at today's price, unless it would repeat a stablecoin's amount.
fn asset_usd(asset: ActivityAsset, amount: U256, input: &ActivityRowsInput<'_>) -> Option<String> {
    let usd = asset_usd_micro(asset, amount, input)?;
    let decimals = match asset {
        ActivityAsset::Native => Some(input.native.1),
        ActivityAsset::Token(token) => {
            super::token_display_metadata(input.tokens, asset_chain_id(input.network), &token)
                .map(|info| info.decimals)
        }
    };
    if decimals.is_some_and(|decimals| {
        railgun_ui::non_redundant_usd_micro_value(amount, decimals, usd).is_none()
    }) {
        return None;
    }
    Some(railgun_ui::format_usd_micro_value(usd))
}

const fn asset_chain_id(network: ActivityNetwork) -> u64 {
    match network {
        ActivityNetwork::Evm(chain_id) => chain_id,
        ActivityNetwork::Tron => railgun_ui::TRON_CHAIN_ID,
    }
}

fn asset_icon(asset: ActivityAsset, input: &ActivityRowsInput<'_>) -> Option<WalletIconSource> {
    if input.network == ActivityNetwork::Tron && asset == ActivityAsset::Native {
        return Some(WalletIconSource::embedded(railgun_ui::TRON_ICON_ASSET_PATH));
    }
    let chain_id = asset_chain_id(input.network);
    let asset = match asset {
        ActivityAsset::Native => PublicAssetId::Native,
        ActivityAsset::Token(token) => PublicAssetId::Erc20(token),
    };
    public_asset_icon_path(chain_id, asset, input.tokens)
}

fn asset_amount(asset: ActivityAsset, amount: U256, input: &ActivityRowsInput<'_>) -> String {
    match asset {
        ActivityAsset::Native => {
            let (symbol, decimals) = input.native;
            format!("{} {symbol}", format_token_amount(amount, decimals))
        }
        ActivityAsset::Token(token) => match input.network {
            ActivityNetwork::Evm(chain_id) => {
                super::format_token_amount_for_display(chain_id, token, amount, input.tokens)
            }
            ActivityNetwork::Tron => super::format_token_amount_for_display(
                railgun_ui::TRON_CHAIN_ID,
                token,
                amount,
                None,
            ),
        },
    }
}

fn relative_age(time: u64) -> String {
    ui::format::format_relative_age(Duration::from_secs(now_unix().saturating_sub(time)))
}

/// A row's tag: its status, or Blocked on a blocked Shield received. Other received rows
/// have none.
fn row_tag(row: &ActivityRow) -> Option<Tag> {
    if row.kind == ActivityRowKind::Received(ReceivedKind::ShieldReceived { blocked: true }) {
        return Some(app_status_tag("Blocked", theme::DANGER));
    }
    row.status.map(|status| {
        let (label, color) = status_badge(status);
        app_status_tag(label, color)
    })
}

fn render_amount_line(amount: &ActivityAmount) -> gpui::Div {
    let text = match &amount.usd {
        Some(usd) => format!("{} ({usd})", amount.text),
        None => amount.text.clone(),
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(token_icon_slot(amount.icon.clone()))
        .child(
            app_text(text)
                .min_w(px(0.0))
                .truncate()
                .when(amount.incoming, |this| this.text_color(rgb(theme::SUCCESS))),
        )
}

/// A list row: status over age, kind over counterparty and source, then up to two amounts.
/// `selected` takes the Public list's selection: the recessed row surface and an accent bar
/// along the left edge.
fn render_activity_row(row: &ActivityRow, selected: bool) -> gpui::Div {
    let subtitle = [row.counterparty.clone(), row.source.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let more = row.amounts.len().saturating_sub(1);
    let shown = if row.amounts.len() > 2 { 1 } else { 2 };
    div()
        .relative()
        .w_full()
        .h(rems(ACTIVITY_ROW_HEIGHT))
        .px(px(12.0))
        .when(selected, |this| {
            this.child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(rgb(theme::SELECTED_ROW_SURFACE)),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(3.0))
                    .bg(rgb(theme::PRIMARY)),
            )
        })
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(ACTIVITY_STATUS_WIDTH)
                .flex_none()
                .flex()
                .flex_col()
                .items_start()
                .gap_1()
                .children(row_tag(row))
                .child(app_muted_text(relative_age(row.time))),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .child(app_strong_text(row_kind_label(row.kind)).truncate())
                .when(!subtitle.is_empty(), |this| {
                    this.child(app_muted_text(subtitle).truncate())
                }),
        )
        // A column of its own, the same width on every row, so icons and amounts line up
        // down the list.
        .child(
            div()
                .w(ACTIVITY_AMOUNTS_WIDTH)
                .flex_none()
                .flex()
                .flex_col()
                .gap_1()
                .children(row.amounts.iter().take(shown).map(render_amount_line))
                .when(row.amounts.len() > 2, |this| {
                    // Indented past the icon, under the amount above.
                    this.child(app_muted_text(format!("+{more} more")).pl(rems(1.75)))
                }),
        )
}

/// A clear-history kind choice: a journal kind, or received rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum ActivityClearKind {
    Entry(ActivityKind),
    Received,
}

const fn clear_kind_label(kind: ActivityClearKind) -> &'static str {
    match kind {
        ActivityClearKind::Entry(kind) => kind_label(kind),
        ActivityClearKind::Received => "Received",
    }
}

/// The clear dialog's choices.
pub(super) struct ActivityClearForm {
    /// "Older than N days" when set, else "All".
    pub(super) older_than: bool,
    pub(super) days: Entity<InputState>,
    pub(super) kinds: Vec<ActivityClearKind>,
}

/// The clear the dialog's choices describe. `None` when nothing is chosen or the day count
/// isn't a whole number.
pub(super) fn activity_clear_selection(
    older_than: bool,
    days: &str,
    kinds: &[ActivityClearKind],
) -> Option<ActivityClear> {
    let age = if older_than {
        ActivityClearAge::OlderThanDays(days.trim().parse().ok()?)
    } else {
        ActivityClearAge::All
    };
    let entry_kinds: Vec<ActivityKind> = ACTIVITY_KINDS
        .into_iter()
        .filter(|kind| kinds.contains(&ActivityClearKind::Entry(*kind)))
        .collect();
    let received = kinds.contains(&ActivityClearKind::Received);
    (!entry_kinds.is_empty() || received).then_some(ActivityClear {
        age,
        kinds: entry_kinds,
        received,
    })
}

fn clear_notice(result: ActivityClearResult, selection: &ActivityClear) -> String {
    let mut notice = if result.cleared == 1 {
        "Cleared 1 entry.".to_owned()
    } else {
        format!("Cleared {} entries.", result.cleared)
    };
    if selection.received {
        notice.push_str(" Older received rows are hidden.");
    }
    if result.kept_in_flight > 0 {
        let _ = write!(notice, " Kept {} still in progress.", result.kept_in_flight);
    }
    notice
}

/// Identifies the session the loaded data belongs to: the wallet's view session and the
/// selected chain's stealth-account owner, whose changes move swap statuses.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ActivityLoadKey {
    wallet_generation: u64,
    view: usize,
    executor_owner: Option<usize>,
}

/// A run of rows under one header.
struct ActivitySection {
    /// Their local day; `None` for the pinned rows still in progress.
    day: Option<NaiveDate>,
    rows: Range<usize>,
}

/// The Activity list's display data: the rows the toolbar's search and filters leave, in
/// order, under their headers. It never reads the root.
#[derive(Default)]
pub(super) struct ActivityListDelegate {
    rows: Vec<ActivityRow>,
    sections: Vec<ActivitySection>,
    selected: Option<ActivityRowId>,
    /// Whether the network has rows before filtering, which picks the empty message.
    has_rows: bool,
}

impl ActivityListDelegate {
    /// Shows `rows`, keeping the selected row selected while it is still among them.
    fn set_rows(&mut self, rows: Vec<ActivityRow>, has_rows: bool) {
        self.sections = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let day = (!row.in_flight).then(|| local_day(row.time));
            match self.sections.last_mut() {
                Some(section) if section.day == day => section.rows.end = index + 1,
                _ => self.sections.push(ActivitySection {
                    day,
                    rows: index..index + 1,
                }),
            }
        }
        if self
            .selected
            .is_some_and(|id| rows.iter().all(|row| row.id != id))
        {
            self.selected = None;
        }
        self.rows = rows;
        self.has_rows = has_rows;
    }

    fn row(&self, ix: IndexPath) -> Option<&ActivityRow> {
        let section = self.sections.get(ix.section)?;
        (ix.row < section.rows.len())
            .then(|| self.rows.get(section.rows.start + ix.row))
            .flatten()
    }

    fn selected_index(&self) -> Option<IndexPath> {
        let index = self
            .rows
            .iter()
            .position(|row| Some(row.id) == self.selected)?;
        self.sections
            .iter()
            .enumerate()
            .find_map(|(section_ix, section)| {
                section
                    .rows
                    .contains(&index)
                    .then(|| IndexPath::new(index - section.rows.start).section(section_ix))
            })
    }

    pub(super) fn selected_row(&self) -> Option<&ActivityRow> {
        let id = self.selected?;
        self.rows.iter().find(|row| row.id == id)
    }

    #[cfg(test)]
    pub(super) fn rows(&self) -> &[ActivityRow] {
        &self.rows
    }
}

impl ListDelegate for ActivityListDelegate {
    type Item = ListItem;

    fn sections_count(&self, _cx: &App) -> usize {
        self.sections.len()
    }

    fn items_count(&self, section: usize, _cx: &App) -> usize {
        self.sections
            .get(section)
            .map_or(0, |section| section.rows.len())
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn render_section_header(
        &mut self,
        section: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<impl IntoElement> {
        let title = self
            .sections
            .get(section)?
            .day
            .map_or_else(|| "In progress".to_owned(), day_label);
        // A label in the Public list's section style over a divider, so days read apart from
        // the rows' own text.
        Some(
            div()
                .px(px(12.0))
                .pt(px(14.0))
                .pb(px(4.0))
                .border_b_1()
                .border_color(rgb(theme::BORDER_SUBTLE))
                .child(
                    app_text(title.to_uppercase())
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD),
                ),
        )
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        let row = self.row(ix)?;
        let selected = self.selected == Some(row.id);
        // The row draws its own padding and selection, since the item's selected background
        // is too faint to find in a long list.
        Some(
            ListItem::new(SharedString::from(format!(
                "wallet-activity-row-{}",
                row.id.key()
            )))
            .h(rems(ACTIVITY_ROW_HEIGHT))
            .p_0()
            .child(render_activity_row(row, selected)),
        )
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> impl IntoElement {
        let (title, hint) = if self.has_rows {
            (
                "No matching activity",
                "Change the search or filters to see more.",
            )
        } else {
            (
                "No activity yet",
                "Sends, shields, swaps and received funds on this network appear here.",
            )
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .child(app_strong_text(title))
            .child(app_muted_text(hint))
    }

    /// A click or the arrow keys select a row, which the pane beside the list shows.
    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected = ix.and_then(|ix| self.row(ix)).map(|row| row.id);
    }
}

fn local_day(time: u64) -> NaiveDate {
    chrono::DateTime::<chrono::Local>::from(UNIX_EPOCH + Duration::from_secs(time)).date_naive()
}

pub(super) struct WalletActivityState {
    key: Option<ActivityLoadKey>,
    data: Option<ActivityData>,
    error: Option<SharedString>,
    stand_in: Option<ActivityStandIn>,
    /// The selected network's rows, before the toolbar's search and filters.
    rows: Vec<ActivityRow>,
    /// The tokens `rows` move, with their symbols and icons, which the token filter lists.
    assets: Vec<(ActivityAsset, String, Option<WalletIconSource>)>,
    /// The network `rows` were built for.
    network: Option<ActivityNetwork>,
    newest_received_block: Option<u64>,
    unread: bool,
    /// The running submissions the rows were built with.
    running: BTreeSet<ActivityEntryId>,
    /// The price revision the rows were valued at.
    rates_revision: u64,
    pub(super) list: Entity<ListState<ActivityListDelegate>>,
    search: ActivitySearchState,
    focus_list_on_render: bool,
    /// The journal read in progress; replacing it drops a superseded one.
    load: Task<()>,
    clear_form: Option<ActivityClearForm>,
    clearing: bool,
    /// Clears confirmed while a stand-in was installed, in place of the vault call.
    #[cfg(test)]
    pub(super) clear_requests: Vec<ActivityClear>,
    /// Polled on the tick rather than awaited: the journal's channel is process-wide.
    journal_changes: Option<tokio::sync::watch::Receiver<u64>>,
    executor_changes: Option<tokio::sync::watch::Receiver<u64>>,
}

impl WalletActivityState {
    /// Activity's list and search field, made with the root's window.
    pub(super) fn new(window: &mut Window, cx: &mut Context<'_, WalletRoot>) -> Self {
        let list = cx.new(|cx| ListState::new(ActivityListDelegate::default(), window, cx));
        let search = ActivitySearchState::new(window, cx);
        Self {
            key: None,
            data: None,
            error: None,
            stand_in: None,
            rows: Vec::new(),
            assets: Vec::new(),
            network: None,
            newest_received_block: None,
            unread: false,
            running: BTreeSet::new(),
            rates_revision: 0,
            list,
            search,
            focus_list_on_render: false,
            load: Task::ready(()),
            clear_form: None,
            clearing: false,
            #[cfg(test)]
            clear_requests: Vec::new(),
            journal_changes: None,
            executor_changes: None,
        }
    }

    pub(super) const fn unread(&self) -> bool {
        self.unread
    }

    #[cfg(test)]
    pub(super) fn last_seen(&self, network: ActivityNetwork) -> Option<u64> {
        self.data.as_ref()?.markers.last_seen.get(&network).copied()
    }

    #[cfg(test)]
    pub(super) fn clear_days_input(&self) -> Option<Entity<InputState>> {
        self.clear_form.as_ref().map(|form| form.days.clone())
    }

    #[cfg(test)]
    pub(super) fn recent_searches(&self) -> &[ActivitySearch] {
        self.search.recent()
    }

    #[cfg(test)]
    pub(super) fn search_input(&self) -> Entity<InputState> {
        self.search.input().clone()
    }

    /// Asks the next frame to focus the list, so the arrow keys move the selection.
    pub(super) const fn request_list_focus(&mut self) {
        self.focus_list_on_render = true;
    }

    fn reset(&mut self, window: gpui::AnyWindowHandle, cx: &mut App) {
        self.key = None;
        self.data = None;
        self.error = None;
        self.rows.clear();
        self.network = None;
        self.newest_received_block = None;
        self.unread = false;
        self.load = Task::ready(());
        self.journal_changes = None;
        self.executor_changes = None;
        self.search.end_session(window, cx);
    }
}

/// Opens `swap_use` in the swap dialog. The view is read out of `root` first: showing a swap
/// reads the root, which an enclosing root update would still hold.
pub(super) fn open_activity_swap(
    root: &Entity<WalletRoot>,
    swap_use: SwapUseId,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(view) = root.read(cx).private_swaps_view() {
        view.update(cx, |view, cx| view.show_swap_use(swap_use, window, cx));
    }
}

/// Reads what Activity shows for `view`'s wallet. Executor records that can't be read only
/// leave swap statuses and their notes unmatched.
fn read_activity(
    store: &DesktopVaultStore,
    view: &Arc<DesktopViewSession>,
    chains: &[u64],
) -> Result<ActivityData, wallet_ops::vault::ActivityStoreError> {
    let journal = ActivityJournal::new(store.db(), Arc::clone(view))?;
    let mut entries = Vec::new();
    for owner in journal.scope_owners()? {
        entries.extend(journal.list(&owner)?);
    }
    let (records, swaps) = wallet_executor_records(store, view, chains);
    Ok(ActivityData {
        entries,
        markers: journal.markers()?,
        records,
        swaps,
    })
}

/// The wallet's executor records on `chains`, and the swap rows each chain's records hold.
fn wallet_executor_records(
    store: &DesktopVaultStore,
    view: &Arc<DesktopViewSession>,
    chains: &[u64],
) -> (Vec<ExecutorRecord>, Vec<SwapRow>) {
    let mut records = Vec::new();
    let mut swaps = Vec::new();
    for chain_id in chains {
        if let Ok(chain_records) = ExecutorStore::new(store.db(), Arc::clone(view), *chain_id)
            .and_then(|store| store.records())
        {
            swaps.extend(swap_rows(*chain_id, &chain_records));
            records.extend(chain_records);
        }
    }
    (records, swaps)
}

impl WalletRoot {
    pub(super) const fn selected_activity_network(&self) -> ActivityNetwork {
        match self.selected_network {
            SelectedNetwork::Evm(chain_id) => ActivityNetwork::Evm(chain_id),
            SelectedNetwork::Tron => ActivityNetwork::Tron,
        }
    }

    fn wallet_activity_load_key(&self) -> Option<ActivityLoadKey> {
        let view = self.view_session.as_ref()?;
        Some(ActivityLoadKey {
            wallet_generation: self.active_wallet_generation,
            view: Arc::as_ptr(view) as usize,
            executor_owner: self
                .stealth_session()
                .and_then(|session| session.executor_owner())
                .map(|owner| Arc::as_ptr(&owner) as usize),
        })
    }

    /// Follows the wallet session and the selected network: reads the journal again when the
    /// session changed, and rebuilds the rows.
    pub(super) fn refresh_wallet_activity(&mut self, cx: &mut Context<'_, Self>) {
        if self.view_session.is_none() {
            self.wallet_activity.reset(self.window_handle, cx);
            self.wallet_activity.stand_in = None;
            return;
        }
        // The debug UI fixture shows its synthesized rows. No release build has any.
        if self
            .wallet_activity
            .stand_in
            .as_ref()
            .is_none_or(|stand_in| stand_in.chain_id != self.last_evm_chain)
            && let Some(stand_in) = super::private_swap::ui_fixture_activity(self.last_evm_chain)
        {
            self.install_wallet_activity_stand_in(stand_in, cx);
            return;
        }
        let key = self.wallet_activity_load_key();
        if self.wallet_activity.stand_in.is_none() && key != self.wallet_activity.key {
            self.wallet_activity.reset(self.window_handle, cx);
            self.wallet_activity.key = key;
            if key.is_some() {
                self.watch_wallet_activity_changes();
                self.load_wallet_activity(cx);
            }
        }
        self.rebuild_wallet_activity(cx);
    }

    /// Shows `stand_in` in place of the vault's Activity until the wallet session ends.
    pub(in crate::root) fn install_wallet_activity_stand_in(
        &mut self,
        stand_in: ActivityStandIn,
        cx: &mut Context<'_, Self>,
    ) {
        self.wallet_activity.reset(self.window_handle, cx);
        self.wallet_activity.key = self.wallet_activity_load_key();
        self.wallet_activity.list.update(cx, |list, _| {
            list.delegate_mut().selected = stand_in.selected;
        });
        self.wallet_activity.data = Some(stand_in.data.clone());
        self.wallet_activity.stand_in = Some(stand_in);
        self.rebuild_wallet_activity(cx);
        cx.notify();
    }

    fn watch_wallet_activity_changes(&mut self) {
        self.wallet_activity.journal_changes = Some(subscribe_activity_changes());
        self.wallet_activity.executor_changes = self
            .stealth_session()
            .and_then(|session| session.executor_owner())
            .map(|owner| owner.subscribe());
    }

    /// Whether the journal or the chain's executor records changed since the last poll.
    fn take_wallet_activity_changes(&mut self) -> bool {
        let state = &mut self.wallet_activity;
        let mut changed = if let Some(journal) = state.journal_changes.as_mut()
            && journal.has_changed().unwrap_or(false)
        {
            journal.borrow_and_update();
            true
        } else {
            false
        };
        if let Some(executor) = state.executor_changes.as_mut() {
            match executor.has_changed() {
                Ok(true) => {
                    executor.borrow_and_update();
                    changed = true;
                }
                Ok(false) => {}
                // The journal is still watched once the chain's owner ends.
                Err(_) => state.executor_changes = None,
            }
        }
        changed
    }

    /// Reads and decrypts on GPUI's background executor; the rows are built back on the UI
    /// thread. A result for a session that has since changed is dropped.
    pub(super) fn load_wallet_activity(&mut self, cx: &Context<'_, Self>) {
        if self.wallet_activity.stand_in.is_some() {
            return;
        }
        let (Some(store), Some(view)) = (self.vault_store.clone(), self.view_session.clone())
        else {
            return;
        };
        let chains: Vec<u64> = self
            .effective_chain_configs
            .railgun_chains()
            .map(|chain| chain.chain_id)
            .collect();
        let key = self.wallet_activity.key;
        let read = cx.background_spawn(async move { read_activity(&store, &view, &chains) });
        self.wallet_activity.load = cx.spawn(async move |root, cx| {
            let result = read.await;
            let _ = root.update(cx, |root, cx| {
                let state = &mut root.wallet_activity;
                if state.key != key || state.stand_in.is_some() {
                    return;
                }
                match result {
                    Ok(data) => {
                        state.data = Some(data);
                        state.error = None;
                    }
                    Err(error) => {
                        tracing::warn!(error = %error.redacted(), "activity journal read failed");
                        state.error = Some("Couldn't read this wallet's activity.".into());
                    }
                }
                root.rebuild_wallet_activity(cx);
                cx.notify();
            });
        });
    }

    fn rebuild_wallet_activity(&mut self, cx: &mut Context<'_, Self>) {
        let network = self.selected_activity_network();
        if self.wallet_activity.network != Some(network) {
            self.wallet_activity.network = Some(network);
            self.wallet_activity.search.change_network();
        }
        if self.wallet_activity.data.is_none() {
            let state = &mut self.wallet_activity;
            state.rows.clear();
            state.assets.clear();
            state.newest_received_block = None;
            state.unread = false;
            state.search.refresh(&state.rows, &state.assets, network);
            self.sync_wallet_activity_list(cx);
            return;
        }
        let state = &self.wallet_activity;
        let Some(data) = state.data.as_ref() else {
            return;
        };
        let snapshot_utxos = match network {
            ActivityNetwork::Evm(chain_id) => self
                .chain_states
                .get(&chain_id)
                .and_then(ChainUtxoState::snapshot)
                .map_or(&[][..], |snapshot| snapshot.utxos.as_slice()),
            ActivityNetwork::Tron => &[][..],
        };
        let running = state
            .stand_in
            .as_ref()
            .map_or_else(running_activity_entries, |stand_in| {
                stand_in.running.clone()
            });
        let native = match network {
            ActivityNetwork::Evm(chain_id) => {
                self.effective_chain_configs
                    .get(chain_id)
                    .map_or(("ETH", 18), |chain| {
                        (
                            chain.native_currency.symbol.as_str(),
                            chain.native_currency.decimals,
                        )
                    })
            }
            ActivityNetwork::Tron => ("TRX", 6),
        };
        let accounts: Vec<PublicAccountMetadata> = self
            .public_accounts
            .iter()
            .chain(&self.tron_public.listed)
            .cloned()
            .collect();
        let built = build_activity_rows(&ActivityRowsInput {
            network,
            data,
            utxos: match &state.stand_in {
                Some(stand_in) if network == ActivityNetwork::Evm(stand_in.chain_id) => {
                    stand_in.utxos.as_slice()
                }
                Some(_) => &[][..],
                None => snapshot_utxos,
            },
            running: &running,
            labels: ActivityLabels {
                private: &self.private_address_book,
                public: &self.public_address_book,
                non_evm: &self.non_evm_address_book,
                accounts: &accounts,
            },
            tokens: Some(&self.effective_token_registry),
            native,
            prices: self
                .public_chain_has_pricing()
                .then_some(&*self.public_broadcaster_anchor_cache),
        });
        let rates_revision = self.activity_rates_revision();
        let state = &mut self.wallet_activity;
        state.rates_revision = rates_revision;
        state.assets.clear();
        for amount in built.rows.iter().flat_map(|row| &row.amounts) {
            if state
                .assets
                .iter()
                .all(|(asset, ..)| *asset != amount.asset)
            {
                state
                    .assets
                    .push((amount.asset, amount.symbol.clone(), amount.icon.clone()));
            }
        }
        state.rows = built.rows;
        state.newest_received_block = built.newest_received_block;
        state.unread = built.unread;
        state.running = running;
        state.search.refresh(&state.rows, &state.assets, network);
        self.sync_wallet_activity_list(cx);
        if self.wallet_activity_visible() {
            self.mark_wallet_activity_seen(cx);
        }
    }

    /// Shows the rows the toolbar's search and filters leave, keeping the selected row
    /// selected while it is still shown.
    fn sync_wallet_activity_list(&self, cx: &mut Context<'_, Self>) {
        let state = &self.wallet_activity;
        let rows: Vec<ActivityRow> = state
            .rows
            .iter()
            .filter(|row| row_matches(row, &state.search.filter))
            .cloned()
            .collect();
        let has_rows = !state.rows.is_empty();
        let list = state.list.clone();
        list.update(cx, |list, cx| {
            list.delegate_mut().set_rows(rows, has_rows);
            cx.notify();
        });
        // The list's own selected index, which it highlights and moves from, needs the window,
        // which a root update doesn't hold.
        let window = self.window_handle;
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                list.update(cx, |list, cx| {
                    let selected = list.delegate().selected_index();
                    list.set_selected_index(selected, window, cx);
                });
            });
        });
    }

    pub(super) fn focus_wallet_activity_list_if_requested(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.wallet_activity.focus_list_on_render || !self.wallet_activity_visible() {
            return;
        }
        self.wallet_activity.focus_list_on_render = false;
        self.wallet_activity
            .list
            .update(cx, |list, cx| list.focus(window, cx));
    }

    fn wallet_activity_visible(&self) -> bool {
        self.active_activity == super::Activity::Wallet
            && self.active_wallet_tab == super::WalletTab::Activity
    }

    /// Called every second: keeps relative times current while Activity is visible, and
    /// unpins an entry once its submission stops running.
    pub(super) fn tick_wallet_activity(&mut self, cx: &mut Context<'_, Self>) {
        if self.take_wallet_activity_changes() {
            self.load_wallet_activity(cx);
        }
        if !self.wallet_activity_visible() {
            return;
        }
        let running_changed = self.wallet_activity.stand_in.is_none()
            && running_activity_entries() != self.wallet_activity.running;
        if running_changed || self.activity_rates_revision() != self.wallet_activity.rates_revision
        {
            self.rebuild_wallet_activity(cx);
        }
        // The list is its own entity: its rows' ages refresh only when it redraws.
        self.wallet_activity.list.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// Changes when prices refresh, which revalues the rows' amounts.
    fn activity_rates_revision(&self) -> u64 {
        *self
            .public_broadcaster_anchor_cache
            .subscribe_refreshes()
            .borrow()
    }

    /// Opening Activity marks the newest shown received row as seen on the selected network.
    pub(super) fn mark_wallet_activity_seen(&mut self, cx: &mut Context<'_, Self>) {
        let network = self.selected_activity_network();
        let state = &mut self.wallet_activity;
        if !state.unread {
            return;
        }
        let Some(block) = state.newest_received_block else {
            return;
        };
        state.unread = false;
        if let Some(data) = state.data.as_mut() {
            let seen = data.markers.last_seen.entry(network).or_insert(block);
            *seen = (*seen).max(block);
        }
        cx.notify();
        if state.stand_in.is_some() {
            return;
        }
        self.write_wallet_activity_last_seen(move |journal| {
            journal.advance_last_seen(network, block)
        });
    }

    /// Sets the chain's last-seen block to the synced tip when the wallet's first catch-up
    /// there completes, so the rows it found aren't unread. A marker already set is kept.
    pub(super) fn init_wallet_activity_last_seen(&self, chain_id: u64) {
        let network = ActivityNetwork::Evm(chain_id);
        if self.wallet_activity.stand_in.is_some()
            || self
                .wallet_activity
                .data
                .as_ref()
                .is_some_and(|data| data.markers.last_seen.contains_key(&network))
        {
            return;
        }
        let Some(tip) = self
            .chain_states
            .get(&chain_id)
            .and_then(ChainUtxoState::sync_tip)
            .and_then(|tip| tip.last_scanned_block)
        else {
            return;
        };
        self.write_wallet_activity_last_seen(move |journal| journal.init_last_seen(network, tip));
    }

    /// Runs `write` on the wallet's journal off the UI thread, logging a failure.
    fn write_wallet_activity_last_seen(
        &self,
        write: impl FnOnce(&ActivityJournal) -> Result<(), ActivityStoreError> + Send + 'static,
    ) {
        let (Some(store), Some(view)) = (self.vault_store.clone(), self.view_session.clone())
        else {
            return;
        };
        self.runtime.spawn_blocking(move || {
            let result = ActivityJournal::new(store.db(), view).and_then(|journal| write(&journal));
            if let Err(error) = result {
                tracing::warn!(error = %error.redacted(), "activity last-seen marker write failed");
            }
        });
    }

    pub(super) fn open_wallet_activity_clear_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let days = cx.new(|cx| InputState::new(window, cx).default_value(DEFAULT_CLEAR_DAYS));
        cx.observe(&days, |_, _, cx| cx.notify()).detach();
        self.wallet_activity.clear_form = Some(ActivityClearForm {
            older_than: true,
            days,
            kinds: ACTIVITY_KINDS
                .into_iter()
                .map(ActivityClearKind::Entry)
                .chain([ActivityClearKind::Received])
                .collect(),
        });
        let root = cx.entity();
        let width = (window.viewport_size().width * 0.92).min(CLEAR_DIALOG_WIDTH);
        let content_width = secondary_dialog_content_width(width);
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            let ok_root = root.clone();
            let close_root = root.clone();
            let (content, ready) = {
                let root_entity = root.clone();
                let root = root.read(cx);
                (
                    root.render_wallet_activity_clear_form(&root_entity, content_width, cx),
                    root.wallet_activity_clear_selection(cx).is_some()
                        && !root.wallet_activity.clearing,
                )
            };
            dialog
                .w(width)
                .max_h(dialog_max_height(window))
                .title(app_strong_text("Clear history"))
                .child(content)
                .footer(styled_dialog_footer(
                    "Clear",
                    true,
                    ButtonVariant::Danger,
                    !ready,
                ))
                .on_ok(move |_, window, cx| {
                    ok_root.update(cx, |root, cx| {
                        root.confirm_wallet_activity_clear(window, cx)
                    })
                })
                .on_close(move |_, _, cx| {
                    close_root.update(cx, |root, cx| {
                        root.wallet_activity.clear_form = None;
                        cx.notify();
                    });
                })
        });
    }

    fn wallet_activity_clear_selection(&self, cx: &App) -> Option<ActivityClear> {
        let form = self.wallet_activity.clear_form.as_ref()?;
        activity_clear_selection(form.older_than, &form.days.read(cx).value(), &form.kinds)
    }

    pub(super) fn set_wallet_activity_clear_older_than(
        &mut self,
        older_than: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(form) = self.wallet_activity.clear_form.as_mut() {
            form.older_than = older_than;
            cx.notify();
        }
    }

    pub(super) fn toggle_wallet_activity_clear_kind(
        &mut self,
        kind: ActivityClearKind,
        checked: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(form) = self.wallet_activity.clear_form.as_mut() {
            form.kinds.retain(|chosen| *chosen != kind);
            if checked {
                form.kinds.push(kind);
            }
            cx.notify();
        }
    }

    /// Starts the clear the dialog describes. Returns whether the dialog may close.
    pub(super) fn confirm_wallet_activity_clear(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(selection) = self.wallet_activity_clear_selection(cx) else {
            return false;
        };
        if self.wallet_activity.clearing {
            return false;
        }
        if self.wallet_activity.stand_in.is_some() {
            #[cfg(test)]
            self.wallet_activity.clear_requests.push(selection.clone());
            let result = self.clear_wallet_activity_stand_in(&selection);
            self.rebuild_wallet_activity(cx);
            window.push_notification(Notification::success(clear_notice(result, &selection)), cx);
            cx.notify();
            return true;
        }
        let (Some(store), Some(view)) = (self.vault_store.clone(), self.view_session.clone())
        else {
            return false;
        };
        let chains: Vec<u64> = self
            .effective_chain_configs
            .railgun_chains()
            .map(|chain| chain.chain_id)
            .collect();
        let running = running_activity_entries();
        let shown_selection = selection.clone();
        self.wallet_activity.clearing = true;
        let join = self.runtime.spawn_blocking(move || {
            let journal = ActivityJournal::new(store.db(), Arc::clone(&view))?;
            let (_, swaps) = wallet_executor_records(&store, &view, &chains);
            let inputs = StatusInputs {
                utxos: &[],
                running: &running,
                owned: &HashSet::new(),
            };
            journal.clear(&selection, now_unix(), &swaps, |entry| {
                is_in_flight(entry, &inputs)
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.wallet_activity.clearing = false;
                let error = match result {
                    Ok(Ok(result)) => {
                        window.push_notification(
                            Notification::success(clear_notice(result, &shown_selection)),
                            cx,
                        );
                        None
                    }
                    Ok(Err(error)) => Some(error.redacted()),
                    Err(_) => Some("clear task ended".to_owned()),
                };
                if let Some(error) = error {
                    tracing::warn!(%error, "activity journal clear failed");
                    window.push_notification(
                        Notification::error("Couldn't clear history. Try again."),
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
        true
    }

    /// The fixture's and a test's clear, in memory, by the same rules: wallet-owned entries go,
    /// account-owned ones become stubs, in-flight ones stay, and swaps and received rows are
    /// hidden.
    fn clear_wallet_activity_stand_in(&mut self, selection: &ActivityClear) -> ActivityClearResult {
        let now = now_unix();
        let cutoff = match selection.age {
            ActivityClearAge::OlderThanDays(days) => {
                Some(now.saturating_sub(u64::from(days) * 86_400))
            }
            ActivityClearAge::All => None,
        };
        let mut result = ActivityClearResult::default();
        let Some(stand_in) = self.wallet_activity.stand_in.as_mut() else {
            return result;
        };
        let inputs = StatusInputs {
            utxos: &[],
            running: &stand_in.running,
            owned: &HashSet::new(),
        };
        let mut kept = Vec::new();
        for entry in std::mem::take(&mut stand_in.data.entries) {
            let matches = !entry.cleared
                && selection.kinds.contains(&entry.kind)
                && cutoff.is_none_or(|cutoff| entry.created_at <= cutoff);
            if matches && !is_in_flight(&entry, &inputs) {
                result.cleared += 1;
                if matches!(entry.owner, ActivityOwner::Account(_)) {
                    kept.push(ActivityEntry {
                        cleared: true,
                        ..entry
                    });
                }
                continue;
            }
            if matches {
                result.kept_in_flight += 1;
            }
            kept.push(entry);
        }
        stand_in.data.entries = kept;
        if selection.kinds.contains(&ActivityKind::Swap) {
            let markers = &mut stand_in.data.markers;
            for swap in &stand_in.data.swaps {
                if markers.hidden_swap_uses.contains(&swap.id)
                    || cutoff.is_some_and(|cutoff| swap.started_at > cutoff)
                {
                    continue;
                }
                if swap.in_flight {
                    result.kept_in_flight += 1;
                } else {
                    markers.hidden_swap_uses.insert(swap.id);
                    result.cleared += 1;
                }
            }
        }
        if selection.received {
            let hidden = &mut stand_in.data.markers.received_hidden_before;
            *hidden = (*hidden).max(cutoff.unwrap_or(now));
        }
        self.wallet_activity.data = Some(stand_in.data.clone());
        result
    }

    fn render_wallet_activity_clear_form(
        &self,
        root: &Entity<Self>,
        width: gpui::Pixels,
        cx: &App,
    ) -> gpui::Div {
        let Some(form) = self.wallet_activity.clear_form.as_ref() else {
            return div();
        };
        let age_root = root.clone();
        let kinds = ACTIVITY_KINDS
            .into_iter()
            .map(|kind| (ActivityClearKind::Entry(kind), kind_label(kind)))
            .chain([(ActivityClearKind::Received, "Received")]);
        let days_invalid =
            form.older_than && form.days.read(cx).value().trim().parse::<u32>().is_err();
        div()
            .w(width)
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(app_muted_text("Age"))
                    .child(
                        RadioGroup::horizontal("wallet-activity-clear-age")
                            .child(Radio::new("wallet-activity-clear-older").label("Older than"))
                            .child(Radio::new("wallet-activity-clear-all").label("All"))
                            .selected_index(Some(usize::from(!form.older_than)))
                            .on_change(move |index, _, cx| {
                                let older_than = *index == 0;
                                age_root.update(cx, |root, cx| {
                                    root.set_wallet_activity_clear_older_than(older_than, cx);
                                });
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div().w(px(72.0)).child(
                                    app_input(&form.days).small().disabled(!form.older_than),
                                ),
                            )
                            .child(app_text("days")),
                    )
                    .when(days_invalid, |this| {
                        this.child(
                            app_muted_text("Enter a whole number of days.")
                                .text_color(rgb(theme::DANGER)),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(app_muted_text("Kinds"))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x_4()
                            .gap_y_2()
                            .children(kinds.map(|(kind, label)| {
                                let kind_root = root.clone();
                                let id = match kind {
                                    ActivityClearKind::Entry(kind) => SharedString::from(format!(
                                        "wallet-activity-clear-kind-{kind:?}"
                                    )),
                                    ActivityClearKind::Received => {
                                        SharedString::from("wallet-activity-clear-kind-received")
                                    }
                                };
                                div().w(px(132.0)).child(
                                    Checkbox::new(id)
                                        .label(label)
                                        .small()
                                        .checked(form.kinds.contains(&kind))
                                        .on_click(move |checked, _, cx| {
                                            let checked = *checked;
                                            kind_root.update(cx, |root, cx| {
                                                root.toggle_wallet_activity_clear_kind(
                                                    kind, checked, cx,
                                                );
                                            });
                                        }),
                                )
                            })),
                    ),
            )
            .child(app_muted_text(
                "This clears every network. Entries of this wallet's Public and Tron accounts are \
                 included, and clearing them removes them from every wallet that shows those \
                 accounts. Entries still in progress are kept. Balances and notes don't change.",
            ))
    }

    pub(super) fn render_wallet_activity_body(
        &self,
        root: &Entity<Self>,
        cx: &App,
    ) -> gpui::AnyElement {
        if self.view_session.is_none() {
            return centered_message("Choose a wallet to continue").into_any_element();
        }
        let state = &self.wallet_activity;
        if let Some(error) = &state.error {
            return centered_message(error.clone()).into_any_element();
        }
        let Some(data) = state.data.as_ref() else {
            return centered_message("Loading activity…").into_any_element();
        };
        let has_history = data.entries.iter().any(|entry| !entry.cleared)
            || data
                .swaps
                .iter()
                .any(|swap| !data.markers.hidden_swap_uses.contains(&swap.id))
            || !state.rows.is_empty();
        let clear_root = root.clone();
        let toolbar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px(px(14.0))
            .pt(px(12.0))
            .pb(px(8.0))
            .child(self.render_wallet_activity_search(root, cx))
            .child(
                app_button("wallet-activity-clear", "Clear history…")
                    .outline()
                    .disabled(!has_history || state.clearing)
                    .on_click(move |_, window, cx| {
                        clear_root.update(cx, |root, cx| {
                            root.open_wallet_activity_clear_dialog(window, cx);
                        });
                    }),
            );
        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .child(List::new(&state.list).size_full().px(px(8.0)).pb(px(8.0))),
            )
            .when(!state.rows.is_empty(), |this| {
                this.child(Self::render_wallet_activity_pane(
                    root,
                    state.list.read(cx).delegate().selected_row(),
                    self.private_swaps_view().is_some(),
                ))
            });
        div()
            .size_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(toolbar)
            .child(body)
            .into_any_element()
    }

    /// The selected row's details, beside the list, or a hint with none selected.
    fn render_wallet_activity_pane(
        root: &Entity<Self>,
        row: Option<&ActivityRow>,
        swaps_available: bool,
    ) -> gpui::AnyElement {
        let content = match row {
            None => div().child(app_muted_text("Select an item to see its details.")),
            Some(row) => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(app_strong_text(row_kind_label(row.kind)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .children(row_tag(row))
                        .child(app_muted_text(relative_age(row.time))),
                )
                .when(!row.amounts.is_empty(), |this| {
                    this.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(row.amounts.iter().map(render_amount_line)),
                    )
                })
                .child(Self::render_wallet_activity_detail(
                    root,
                    row,
                    &row.id.key(),
                    swaps_available,
                )),
        };
        div()
            .debug_selector(|| "wallet-activity-detail".to_owned())
            .w(ACTIVITY_PANE_WIDTH)
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .bg(rgb(theme::SURFACE))
            .overflow_y_scrollbar()
            .child(div().p(px(14.0)).child(content))
            .into_any_element()
    }

    fn render_wallet_activity_detail(
        root: &Entity<Self>,
        row: &ActivityRow,
        key: &str,
        swaps_available: bool,
    ) -> gpui::Div {
        let details = DescriptionList::horizontal()
            .bordered(false)
            .columns(1)
            .label_width(px(96.0))
            .children(row.details.iter().enumerate().map(|(index, detail)| {
                // The list puts no gap after the label, so the value keeps its own.
                let value = match detail.copy.clone() {
                    // A copy button stays beside a short value and visible for a long one.
                    Some(copy) => div()
                        .pl_2()
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(
                            app_text(detail.value.clone())
                                .min_w(px(0.0))
                                .font_family(APP_MONO_FONT_FAMILY)
                                .font_weight(FontWeight::LIGHT),
                        )
                        .child(clipboard_with_toast(
                            SharedString::from(format!("wallet-activity-copy-{key}-{index}")),
                            copy,
                        )),
                    None => app_text(detail.value.clone()).pl_2(),
                };
                DescriptionItem::new(app_muted_text(detail.label).into_any_element())
                    .value(value.into_any_element())
            }));
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(details)
            .when_some(row.swap_use, |this, swap_use| {
                let swap_root = root.clone();
                this.child(
                    div().pt(px(6.0)).flex().child(
                        app_button(
                            SharedString::from(format!("wallet-activity-swap-{key}")),
                            "Open swap…",
                        )
                        .outline()
                        .small()
                        .disabled(!swaps_available)
                        .when(!swaps_available, |button| {
                            button.tooltip("Swaps on this network aren't loaded yet.")
                        })
                        .on_click(move |_, window, cx| {
                            open_activity_swap(&swap_root, swap_use, window, cx);
                        }),
                    ),
                )
            })
    }
}
