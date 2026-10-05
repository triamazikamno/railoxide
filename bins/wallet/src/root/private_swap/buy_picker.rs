//! The Buy picker behind the swap form's Buy token button: a switch for the delivery kind, the
//! tokens of one network with a search, and the networks with theirs. Picking a token closes
//! it and sets the delivery kind, network and token together.
//!
//! The form owns the picker's state. The modal dialog gives its two lists their rows from
//! [`BuyPickerContent`] whenever it renders.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use gpui::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    KeyBinding, NoAction, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, relative, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Sizable as _, WindowExt as _,
    button::ButtonGroup,
    list::{List, ListDelegate, ListItem, ListState},
    select::SelectItem as _,
    spinner::Spinner,
    tag::Tag,
    tooltip::Tooltip,
};
use ui::controls::{app_muted_text, app_segment_button, app_strong_text, app_text};
use ui::theme;
use wallet_ops::{PublicBroadcasterCandidate, WalletSession};

use super::{
    AbortOnDrop, BridgeProvider, BridgeRoutes, PrivateSwapsView, ReceiveTo, SwapBuyItem, SwapForm,
    bridge_routes_retry_button, network_name, same_offer,
};
use crate::assets::WalletIconSource;
use crate::root::{PRIVATE_ASSET_LIST_WIDTH, dialog_max_height};

/// Let Enter and Space activate the focused picker button instead of the dialog's `Confirm`.
const BUTTONS_KEY_CONTEXT: &str = "SwapBuyPickerButtons";

/// A list lays its rows out at one height, in rems. A token row is one line, a network row
/// its name over a reason.
const TOKEN_ROW_HEIGHT: f32 = 2.;
const NETWORK_ROW_HEIGHT: f32 = 3.;

/// Marks that the picker's key bindings are installed in this app.
struct BuyPickerBindings;

impl gpui::Global for BuyPickerBindings {}

/// Bind the picker's keys once per app.
pub(super) fn ensure_buy_picker_bindings(cx: &mut App) {
    if cx.has_global::<BuyPickerBindings>() {
        return;
    }
    cx.bind_keys([
        KeyBinding::new("enter", NoAction, Some(BUTTONS_KEY_CONTEXT)),
        KeyBinding::new("space", NoAction, Some(BUTTONS_KEY_CONTEXT)),
    ]);
    cx.set_global(BuyPickerBindings);
}

/// Why a delivery kind can't deliver on a network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NetworkUnavailable {
    /// The chain isn't enabled with RPC endpoints.
    Rpc,
    /// The chain has no accepted swap profile, which a stealth account there needs.
    PublicOnly,
    /// The wallet's private sync of the chain failed in this session.
    SyncFailed,
    /// No spendable private balance there pays a setup broadcaster.
    Unfunded,
    /// The setup fee could not be checked through the destination session.
    SetupFee,
    /// Neither provider serves the chain from the swap's, as its fetched routes show.
    NoBridge,
}

/// Whether a network can be picked for the selected delivery kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NetworkAvailability {
    Available,
    /// Setup isn't available, but the network can be picked with an existing stealth account,
    /// which the user selects in the form.
    ReuseOnly,
    /// The wallet's private sync there isn't ready yet, with its progress in percent once
    /// known.
    Syncing(Option<u8>),
    CheckingFee,
    Unavailable(NetworkUnavailable),
}

impl NetworkAvailability {
    /// The network can be picked: with a new stealth account there, or only with an existing
    /// one.
    pub(super) const fn is_available(self) -> bool {
        matches!(self, Self::Available | Self::ReuseOnly)
    }

    /// Everything but the setup's funding holds, which is all a delivery to an existing
    /// stealth account needs of the network.
    pub(super) const fn admits_existing_account(self) -> bool {
        matches!(
            self,
            Self::Available
                | Self::ReuseOnly
                | Self::CheckingFee
                | Self::Unavailable(NetworkUnavailable::Unfunded | NetworkUnavailable::SetupFee)
        )
    }

    /// The line under `network` in the picker's list while it can't be picked, or can be only
    /// with an existing stealth account.
    fn list_reason(self, network: &str) -> Option<String> {
        match self {
            Self::Available => None,
            Self::ReuseOnly => Some("Existing account only".to_owned()),
            Self::Syncing(None) => Some("Syncing…".to_owned()),
            Self::Syncing(Some(percent)) => Some(format!("Syncing… {percent}%")),
            Self::CheckingFee => Some("Checking setup fee…".to_owned()),
            Self::Unavailable(NetworkUnavailable::Rpc) => {
                Some(format!("Enable {network} with RPC endpoints first"))
            }
            Self::Unavailable(NetworkUnavailable::PublicOnly) => {
                Some("Public address only".to_owned())
            }
            Self::Unavailable(NetworkUnavailable::SyncFailed) => {
                Some("Private sync failed there".to_owned())
            }
            Self::Unavailable(NetworkUnavailable::Unfunded) => {
                Some("Needs private funds there for the setup fee".to_owned())
            }
            Self::Unavailable(NetworkUnavailable::SetupFee) => {
                Some("Couldn't check the setup fee. Retrying…".to_owned())
            }
            Self::Unavailable(NetworkUnavailable::NoBridge) => {
                Some("No bridge serves it from this network".to_owned())
            }
        }
    }

    /// Why Private balance can't deliver on `network`, the form's kept one, and what to do
    /// about it. The line shows under Receive to.
    pub(super) fn private_problem(self, network: &str) -> Option<String> {
        let elsewhere = "Pick a token on another network or use Public address.";
        Some(match self {
            Self::Available => return None,
            Self::ReuseOnly => format!(
                "New stealth account setup is unavailable on {network}. Select an existing stealth account there, or use Public address."
            ),
            Self::CheckingFee => {
                format!("Checking whether your private funds on {network} cover the setup fee.")
            }
            Self::Syncing(None) => format!(
                "Private balance needs {network} synced first. Wait for it, or use Public address."
            ),
            Self::Syncing(Some(percent)) => format!(
                "Private balance needs {network} synced first, now at {percent}%. Wait for it, or use Public address."
            ),
            Self::Unavailable(NetworkUnavailable::Rpc) => {
                format!("Private balance needs {network} enabled with RPC endpoints. {elsewhere}")
            }
            Self::Unavailable(NetworkUnavailable::PublicOnly) => {
                format!("Private balance isn't available on {network}. {elsewhere}")
            }
            Self::Unavailable(NetworkUnavailable::SyncFailed) => {
                format!("Private balance needs {network} synced, and its sync failed. {elsewhere}")
            }
            Self::Unavailable(NetworkUnavailable::Unfunded) => format!(
                "Private balance needs private funds on {network} for the setup fee. {elsewhere}"
            ),
            Self::Unavailable(NetworkUnavailable::SetupFee) => format!(
                "Couldn't check the setup fee on {network}. Retrying, or use Public address."
            ),
            Self::Unavailable(NetworkUnavailable::NoBridge) => format!(
                "No bridge serves {network} from this network. Pick a token on another network."
            ),
        })
    }
}

/// Where the wallet's private sync of a network stands in this session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NetworkSync {
    /// Not loaded yet, or still syncing, with the progress in percent once known.
    Loading(Option<u8>),
    Ready,
    Failed,
}

/// What decides whether Private balance can deliver on another network a bridge reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PrivateNetworkFacts {
    /// The chain is enabled with RPC endpoints.
    pub(super) rpc: bool,
    /// The chain has an accepted swap profile.
    pub(super) swap_profile: bool,
    pub(super) sync: NetworkSync,
    /// A spendable private balance there is in a token a compatible broadcaster accepts.
    pub(super) funded: bool,
    /// A set-up stealth account there is locally eligible as a destination.
    pub(super) reusable: bool,
}

/// Whether Private balance can deliver on a network with these `facts`. The first condition
/// that fails is the reason.
pub(super) const fn private_network_availability(
    facts: PrivateNetworkFacts,
) -> NetworkAvailability {
    if !facts.rpc {
        return NetworkAvailability::Unavailable(NetworkUnavailable::Rpc);
    }
    if !facts.swap_profile {
        return NetworkAvailability::Unavailable(NetworkUnavailable::PublicOnly);
    }
    match facts.sync {
        NetworkSync::Loading(percent) => NetworkAvailability::Syncing(percent),
        NetworkSync::Failed => NetworkAvailability::Unavailable(NetworkUnavailable::SyncFailed),
        NetworkSync::Ready if !facts.funded && facts.reusable => NetworkAvailability::ReuseOnly,
        NetworkSync::Ready if !facts.funded => {
            NetworkAvailability::Unavailable(NetworkUnavailable::Unfunded)
        }
        NetworkSync::Ready => NetworkAvailability::Available,
    }
}

/// Whether a Public address can receive on a network: it needs only RPC endpoints. A network
/// whose fetched routes show that no provider serves it is marked by the form afterwards.
pub(super) const fn public_network_availability(rpc: bool) -> NetworkAvailability {
    if rpc {
        NetworkAvailability::Available
    } else {
        NetworkAvailability::Unavailable(NetworkUnavailable::Rpc)
    }
}

/// A network in the picker's list: the swap's own first, then the other chains a bridge could
/// deliver on. One that can't take the delivery kind is listed with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BuyNetwork {
    pub(super) chain_id: u64,
    pub(super) label: SharedString,
    pub(super) this_network: bool,
    pub(super) availability: NetworkAvailability,
}

/// The Buy picker's state.
pub(super) struct BuyPicker {
    pub(super) open: bool,
    /// The network whose tokens are listed. Picking a token makes it the form's.
    pub(super) network: u64,
    /// The shown network's tokens under their search. Enter or a click picks the selected one.
    pub(super) tokens: Entity<ListState<BuyTokensDelegate>>,
    /// The networks under their search, which doesn't change the shown one. Enter or a click
    /// shows the selected one's tokens.
    pub(super) networks: Entity<ListState<BuyNetworksDelegate>>,
    funding: HashMap<u64, NetworkFunding>,
}

/// The picker's two lists, with empty searches and nothing selected.
fn buy_picker_lists(
    network: u64,
    window: &mut Window,
    cx: &mut Context<'_, PrivateSwapsView>,
) -> (
    Entity<ListState<BuyTokensDelegate>>,
    Entity<ListState<BuyNetworksDelegate>>,
) {
    let view = cx.weak_entity();
    let tokens = cx.new(|cx| {
        ListState::new(
            BuyTokensDelegate {
                view: view.clone(),
                network,
                tokens: BuyPickerTokens::Listed(Vec::new()),
                rows: Vec::new(),
                query: String::new(),
                busy: false,
                selected: None,
            },
            window,
            cx,
        )
        .searchable(true)
    });
    let networks = cx.new(|cx| {
        ListState::new(
            BuyNetworksDelegate {
                view,
                shown: network,
                networks: Vec::new(),
                rows: Vec::new(),
                query: String::new(),
                selected: None,
            },
            window,
            cx,
        )
        .searchable(true)
    });
    (tokens, networks)
}

impl BuyPicker {
    #[cfg(test)]
    pub(super) fn funding_checked_for_tests(&self, network: u64) -> bool {
        self.funding
            .get(&network)
            .is_some_and(|funding| funding.task.is_none())
    }

    #[cfg(test)]
    pub(super) fn expire_funding_for_tests(&mut self, network: u64) {
        let funding = self.funding.get_mut(&network).unwrap();
        assert!(funding.task.is_none());
        funding.checked_at = Instant::now().checked_sub(Duration::from_secs(30)).unwrap();
    }

    pub(super) fn new(
        network: u64,
        window: &mut Window,
        cx: &mut Context<'_, PrivateSwapsView>,
    ) -> Self {
        let (tokens, networks) = buy_picker_lists(network, window, cx);
        Self {
            open: false,
            network,
            tokens,
            networks,
            funding: HashMap::new(),
        }
    }
}

/// A setup estimate belongs to the session, balances and broadcaster offers it checked.
struct FundingInputs {
    session: Arc<WalletSession>,
    balances: Vec<(Address, U256)>,
    candidates: Vec<PublicBroadcasterCandidate>,
}

impl FundingInputs {
    fn same_funds(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.session, &other.session) && self.balances == other.balances
    }

    fn matches(&self, other: &Self) -> bool {
        self.same_funds(other)
            && self.candidates.len() == other.candidates.len()
            && self
                .candidates
                .iter()
                .zip(&other.candidates)
                .all(|(a, b)| same_offer(a, b))
    }
}

struct NetworkFunding {
    inputs: FundingInputs,
    checked_at: Instant,
    availability: NetworkAvailability,
    task: Option<Task<()>>,
}

/// Use the setup planner itself, including its POI notes and reservations, to check funding.
/// Trying the cheapest eligible offer for each token keeps a small balance in one token from
/// hiding a usable balance in another. No account is reserved and nothing is signed.
pub(super) async fn estimate_network_funding(
    session: &WalletSession,
    candidates: Vec<PublicBroadcasterCandidate>,
) -> NetworkAvailability {
    let Some(owner) = session.executor_owner() else {
        return NetworkAvailability::Unavailable(NetworkUnavailable::SetupFee);
    };
    let mut failed = false;
    for candidate in candidates {
        match tokio::time::timeout(
            Duration::from_secs(15),
            owner.can_fund_swap_setup(session, candidate),
        )
        .await
        {
            Ok(Ok(true)) => return NetworkAvailability::Available,
            Ok(Ok(false)) => {}
            _ => failed = true,
        }
    }
    NetworkAvailability::Unavailable(if failed {
        NetworkUnavailable::SetupFee
    } else {
        NetworkUnavailable::Unfunded
    })
}

impl PrivateSwapsView {
    fn network_funding_inputs(&self, network: u64, cx: &App) -> Option<FundingInputs> {
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        let super::ChainUtxoState::Ready { session, .. } = root.chain_states.get(&network)? else {
            return None;
        };
        let (options, _, _) = self.setup_fee_route(network, Address::ZERO, None, false, false, cx);
        let mut balances = Vec::new();
        let mut candidates = Vec::new();
        for option in options {
            // Spendable funds don't change when an offer arrives or expires. Keep tokens
            // without a current broadcaster in the balance key as well.
            balances.push((option.token, option.max_spendable));
            if let Some(candidate) = self
                .broadcaster_candidates(network, option.token, false, false, cx)
                .into_iter()
                .min_by_key(|candidate| candidate.fee)
            {
                candidates.push(candidate);
            }
        }
        Some(FundingInputs {
            session: Arc::clone(session),
            balances,
            candidates,
        })
    }

    pub(super) fn network_setup_availability(&self, network: u64, cx: &App) -> NetworkAvailability {
        let Some(inputs) = self.network_funding_inputs(network, cx) else {
            return NetworkAvailability::CheckingFee;
        };
        self.form
            .as_ref()
            .and_then(|form| form.picker.funding.get(&network))
            .filter(|funding| funding.inputs.same_funds(&inputs))
            .map_or(NetworkAvailability::CheckingFee, |funding| {
                funding.availability
            })
    }

    pub(super) fn refresh_network_funding(&mut self, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref().filter(|form| {
            form.receive_to == ReceiveTo::PrivateBalance
                && (form.picker.open || form.network.is_some())
        }) else {
            return;
        };
        let networks = self
            .network_items(ReceiveTo::PublicAddress, cx)
            .into_iter()
            .filter(|network| {
                !network.this_network
                    && network.availability.is_available()
                    && (form.picker.open || form.network == Some(network.chain_id))
            })
            .map(|network| network.chain_id)
            .collect::<Vec<_>>();
        for network in networks {
            let Some(inputs) = self.network_funding_inputs(network, cx) else {
                continue;
            };
            let previous = self
                .form
                .as_ref()
                .and_then(|form| form.picker.funding.get(&network));
            if previous.is_some_and(|funding| {
                funding.inputs.matches(&inputs)
                    && (funding.task.is_some()
                        || funding.checked_at.elapsed() < Duration::from_secs(30))
            }) {
                continue;
            }
            // Funding is an eligibility hint; setup review estimates the current offer again.
            // Keep the last result during periodic checks and offer renewals so a background
            // refresh doesn't invalidate the swap quote. Changed funds or sessions still wait.
            let availability = previous
                .filter(|funding| funding.inputs.same_funds(&inputs))
                .map_or(NetworkAvailability::CheckingFee, |funding| {
                    funding.availability
                });
            let session = Arc::clone(&inputs.session);
            let candidates = inputs.candidates.clone();
            let runtime = self.runtime.clone();
            let task = cx.spawn(async move |view, cx| {
                let work = runtime
                    .spawn(async move { estimate_network_funding(&session, candidates).await });
                let _abort = AbortOnDrop(work.abort_handle());
                let availability = work.await.unwrap_or(NetworkAvailability::Unavailable(
                    NetworkUnavailable::SetupFee,
                ));
                let _ = view.update(cx, |view, cx| {
                    if let Some(funding) = view
                        .form
                        .as_mut()
                        .and_then(|form| form.picker.funding.get_mut(&network))
                    {
                        funding.availability = availability;
                        funding.checked_at = Instant::now();
                        funding.task = None;
                        cx.notify();
                    }
                });
            });
            if let Some(form) = self.form.as_mut() {
                form.picker.funding.insert(
                    network,
                    NetworkFunding {
                        inputs,
                        checked_at: Instant::now(),
                        availability,
                        task: Some(task),
                    },
                );
                cx.notify();
            }
        }
    }
}

/// A row of the picker's token list.
pub(super) struct BuyPickerToken {
    pub(super) item: SwapBuyItem,
    /// The wallet's private balance of the token on the shown network, for Private balance.
    pub(super) balance: Option<String>,
}

/// The token pane: the shown network's list, or where its routes stand.
pub(super) enum BuyPickerTokens {
    Loading,
    Failed(String),
    Listed(Vec<BuyPickerToken>),
}

/// What the Buy picker shows, captured when its dialog renders.
pub(super) struct BuyPickerContent {
    pub(super) receive_to: ReceiveTo,
    /// Receive to can't change, as in the form's row.
    pub(super) receive_to_locked: bool,
    pub(super) token_list: Entity<ListState<BuyTokensDelegate>>,
    pub(super) network_list: Entity<ListState<BuyNetworksDelegate>>,
    pub(super) network: u64,
    /// The shown network is the swap's own.
    pub(super) own_network: bool,
    /// Every token of the shown network. The token list's search filters them.
    pub(super) tokens: BuyPickerTokens,
    /// Every network. The network list's search filters them.
    pub(super) networks: Vec<BuyNetwork>,
    pub(super) busy: bool,
}

impl BuyPickerContent {
    /// Give the two lists their rows.
    fn sync_lists(self, cx: &mut App) {
        let Self {
            token_list,
            network_list,
            network,
            tokens,
            networks,
            busy,
            ..
        } = self;
        token_list.update(cx, |list, _| {
            list.delegate_mut().set_content(network, tokens, busy);
        });
        network_list.update(cx, |list, _| {
            list.delegate_mut().set_content(network, networks);
        });
    }
}

/// Select `row` of `list` and scroll to it.
fn select_row<D: ListDelegate>(
    list: &Entity<ListState<D>>,
    row: usize,
    window: &mut Window,
    cx: &mut App,
) {
    list.update(cx, |list, cx| {
        list.set_selected_index(Some(IndexPath::new(row)), window, cx);
        list.scroll_to_selected_item(window, cx);
    });
}

/// The token list's rows: the shown network's tokens the search matches. While there are
/// none, the list shows where the network's routes stand.
pub(super) struct BuyTokensDelegate {
    view: WeakEntity<PrivateSwapsView>,
    network: u64,
    tokens: BuyPickerTokens,
    /// The listed tokens the search matches, as indexes into them.
    rows: Vec<usize>,
    query: String,
    busy: bool,
    selected: Option<IndexPath>,
}

impl BuyTokensDelegate {
    fn set_content(&mut self, network: u64, tokens: BuyPickerTokens, busy: bool) {
        self.network = network;
        self.tokens = tokens;
        self.busy = busy;
        self.filter();
    }

    fn filter(&mut self) {
        self.rows = match &self.tokens {
            BuyPickerTokens::Listed(tokens) => tokens
                .iter()
                .enumerate()
                .filter(|(_, token)| token.item.asset.matches(&self.query))
                .map(|(index, _)| index)
                .collect(),
            BuyPickerTokens::Loading | BuyPickerTokens::Failed(_) => Vec::new(),
        };
    }

    fn row(&self, row: usize) -> Option<&BuyPickerToken> {
        let BuyPickerTokens::Listed(tokens) = &self.tokens else {
            return None;
        };
        tokens.get(*self.rows.get(row)?)
    }

    /// The tokens the search matches, in the list's order.
    pub(super) fn listed(&self) -> impl Iterator<Item = Address> {
        (0..self.rows.len())
            .filter_map(|row| self.row(row))
            .map(|row| row.item.asset.token)
    }
}

impl ListDelegate for BuyTokensDelegate {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> Task<()> {
        query.trim().clone_into(&mut self.query);
        self.filter();
        Task::ready(())
    }

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.rows.len()
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        let row = self.row(ix.row)?;
        Some(
            ListItem::new(SharedString::from(format!(
                "swap-buy-picker-token-{:#x}",
                row.item.asset.token
            )))
            .h(rems(TOKEN_ROW_HEIGHT))
            .px_2()
            .py_0()
            .rounded(cx.theme().radius)
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(network_token_icon(
                        row.item.asset.icon_path.clone(),
                        self.network,
                    ))
                    .child(
                        app_text(row.item.asset.label.to_string())
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                    )
                    .children(
                        row.balance
                            .clone()
                            .map(|balance| app_muted_text(balance).text_xs().flex_none()),
                    )
                    .when(row.item.near_only, |row| row.child(near_tag())),
            ),
        )
    }

    /// Where the list stands while it has no rows. Its Retry is a button like the header's.
    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> impl IntoElement {
        let network = network_name(self.network);
        let status = match &self.tokens {
            BuyPickerTokens::Loading => div()
                .flex()
                .items_center()
                .gap_2()
                .child(Spinner::new().small())
                .child(
                    app_muted_text(format!("Getting the tokens {network} can receive…"))
                        .min_w_0()
                        .whitespace_normal(),
                ),
            BuyPickerTokens::Failed(error) => div()
                .flex()
                .flex_col()
                .items_start()
                .gap_2()
                .child(
                    app_text(error.clone())
                        .text_color(cx.theme().danger)
                        .whitespace_normal(),
                )
                .children(
                    self.view
                        .upgrade()
                        .map(|view| bridge_routes_retry_button(view, self.busy)),
                ),
            BuyPickerTokens::Listed(_) => app_muted_text(if self.query.is_empty() {
                format!("No tokens to buy on {network}")
            } else {
                "No matching tokens".to_owned()
            })
            .whitespace_normal(),
        };
        div()
            .key_context(BUTTONS_KEY_CONTEXT)
            .px_2()
            .py_1()
            .child(status)
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected = ix;
    }

    /// Enter or a click picks the token.
    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) {
        let Some(token) = self
            .selected
            .and_then(|ix| self.row(ix.row))
            .map(|row| row.item.asset.token)
        else {
            return;
        };
        let _ = self.view.update(cx, |view, cx| {
            view.pick_buy_token(token, window, cx);
        });
    }
}

/// The network list's rows: the networks the search matches, by name or by the start of the
/// chain id. One the delivery kind can't deliver on is disabled with the reason.
pub(super) struct BuyNetworksDelegate {
    view: WeakEntity<PrivateSwapsView>,
    /// The network whose tokens the picker shows, which its row marks.
    shown: u64,
    networks: Vec<BuyNetwork>,
    /// The networks the search matches, as indexes into them.
    rows: Vec<usize>,
    query: String,
    selected: Option<IndexPath>,
}

impl BuyNetworksDelegate {
    fn set_content(&mut self, shown: u64, networks: Vec<BuyNetwork>) {
        self.shown = shown;
        self.networks = networks;
        self.filter();
    }

    fn filter(&mut self) {
        self.rows = self
            .networks
            .iter()
            .enumerate()
            .filter(|(_, network)| {
                network.label.to_lowercase().contains(&self.query)
                    || (!self.query.is_empty()
                        && network.chain_id.to_string().starts_with(&self.query))
            })
            .map(|(index, _)| index)
            .collect();
    }

    fn row(&self, row: usize) -> Option<&BuyNetwork> {
        self.networks.get(*self.rows.get(row)?)
    }

    /// The networks the search matches, in the list's order.
    pub(super) fn listed(&self) -> impl Iterator<Item = u64> {
        (0..self.rows.len())
            .filter_map(|row| self.row(row))
            .map(|row| row.chain_id)
    }
}

impl ListDelegate for BuyNetworksDelegate {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> Task<()> {
        self.query = query.trim().to_lowercase();
        self.filter();
        Task::ready(())
    }

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.rows.len()
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        let network = self.row(ix.row)?;
        let chain_id = network.chain_id;
        let available = network.availability.is_available();
        let shown = self.shown == chain_id;
        let note = if network.this_network {
            Some("Current network".to_owned())
        } else {
            network.availability.list_reason(&network.label)
        };
        // The reason is cut to the row's one line, so the pointer shows it whole.
        let tooltip = note.clone().filter(|_| !network.this_network);
        Some(
            ListItem::new(SharedString::from(format!(
                "swap-buy-picker-network-{chain_id}"
            )))
            .debug_selector(move || format!("swap-buy-picker-network-{chain_id}"))
            .h(rems(NETWORK_ROW_HEIGHT))
            .px_2()
            .py_0()
            .rounded(cx.theme().radius)
            .disabled(!available)
            // The shown network keeps the selected segment's fill and color, and no hover.
            .confirmed(shown)
            .when(shown, |row| row.bg(rgb(theme::SURFACE_HOVER)))
            .when_some(tooltip, |row, tooltip| {
                row.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            })
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .size_4()
                                    .flex_none()
                                    .when(!available, |icon| icon.opacity(0.45))
                                    .children(
                                        railgun_ui::chain_icon_asset_path(chain_id)
                                            .map(|path| gpui::img(path).size_full()),
                                    ),
                            )
                            .child(
                                app_text(network.label.clone())
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .when(shown, |label| label.text_color(rgb(theme::PRIMARY))),
                            )
                            .when(shown, |row| {
                                row.child(
                                    Icon::new(IconName::Check)
                                        .small()
                                        .flex_none()
                                        .text_color(rgb(theme::PRIMARY)),
                                )
                            }),
                    )
                    // Under the label, past the icon and the row's gap.
                    .children(note.map(|note| app_muted_text(note).text_xs().pl_6().truncate())),
            ),
        )
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .child(app_muted_text("No matching networks").whitespace_normal())
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected = ix;
    }

    /// Enter or a click shows the network's tokens, when the delivery kind can deliver there.
    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) {
        let Some(chain_id) = self
            .selected
            .and_then(|ix| self.row(ix.row))
            .map(|row| row.chain_id)
        else {
            return;
        };
        let _ = self.view.update(cx, |view, cx| {
            view.show_buy_picker_network(chain_id, window, cx);
        });
    }
}

impl PrivateSwapsView {
    /// Open the picker on the form's network and Buy asset, with empty searches.
    pub(super) fn open_buy_picker(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let own = self.session.chain_id;
        let busy = self.busy();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if busy || form.picker.open || (form.operation.is_some() && !form.reuse_account) {
            return;
        }
        let buy = form.buy;
        let picker = &mut form.picker;
        picker.open = true;
        picker.network = form.network.unwrap_or(own);
        (picker.tokens, picker.networks) = buy_picker_lists(picker.network, window, cx);
        let tokens = picker.tokens.clone();
        self.settle_buy_picker(window, cx);
        // The form's Buy asset is the selected token.
        let row = buy.and_then(|buy| {
            tokens
                .read(cx)
                .delegate()
                .listed()
                .position(|token| token == buy)
        });
        if let Some(row) = row {
            select_row(&tokens, row, window, cx);
        }
        let view = cx.weak_entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            let close_view = view.clone();
            let content = view.upgrade().and_then(|view| {
                let content = view
                    .read(cx)
                    .form
                    .as_ref()
                    .filter(|form| form.picker.open)
                    .map(|form| view.read(cx).buy_picker_content(form, cx))?;
                Some(render_buy_picker(&view, content, window, cx))
            });
            dialog
                .title(app_strong_text("Choose buy asset"))
                .w((window.viewport_size().width * 0.92).min(PRIVATE_ASSET_LIST_WIDTH))
                .max_h(dialog_max_height(window))
                .on_ok(|_, _, _| false)
                .on_close(move |_, _, cx| {
                    let _ = close_view.update(cx, Self::close_buy_picker);
                })
                .children(content)
        });
        cx.defer_in(window, move |view, window, cx| {
            if view.form.as_ref().is_some_and(|form| form.picker.open) {
                tokens.read(cx).focus_handle(cx).focus(window, cx);
            }
        });
    }

    pub(super) fn close_buy_picker(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.picker.open
        {
            form.picker.open = false;
            cx.notify();
        }
    }

    /// The picker opened, or Receive to changed while it is open: its lists follow the delivery
    /// kind, with the shown network and the first token selected. A shown network the kind
    /// can't take gives way to the swap's own, which only changes what the picker lists. For
    /// Private balance, the networks that aren't loaded start loading.
    pub(super) fn settle_buy_picker(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let own = self.session.chain_id;
        // A network without setup funds can be picked for an existing account there.
        self.refresh_destination_accounts(window, cx);
        let Some(form) = self.form.as_ref().filter(|form| form.picker.open) else {
            return;
        };
        let receive_to = form.receive_to;
        let shown = form.picker.network;
        let selectable = self
            .network_items(receive_to, cx)
            .iter()
            .any(|network| network.chain_id == shown && network.availability.is_available());
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if !selectable {
            form.picker.network = own;
        }
        if receive_to == ReceiveTo::PrivateBalance {
            self.load_private_networks(None, cx);
        }
        self.load_bridge_routes(window, cx);
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let content = self.buy_picker_content(form, cx);
        let (tokens, networks) = (content.token_list.clone(), content.network_list.clone());
        let shown = content.network;
        content.sync_lists(cx);
        select_row(&tokens, 0, window, cx);
        let row = networks
            .read(cx)
            .delegate()
            .listed()
            .position(|chain_id| chain_id == shown);
        if let Some(row) = row {
            select_row(&networks, row, window, cx);
        }
        cx.notify();
    }

    /// List `chain_id`'s tokens, when the delivery kind can deliver there.
    pub(super) fn show_buy_picker_network(
        &mut self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref().filter(|form| form.picker.open) else {
            return;
        };
        if !self
            .network_items(form.receive_to, cx)
            .iter()
            .any(|network| network.chain_id == chain_id && network.availability.is_available())
        {
            return;
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.picker.network = chain_id;
        let tokens = form.picker.tokens.clone();
        select_row(&tokens, 0, window, cx);
        self.load_bridge_routes(window, cx);
        cx.notify();
    }

    /// Pick `token` on the shown network: the picker closes, and the form takes the network
    /// and the token together and quotes them. The delivery kind is already the form's.
    pub(super) fn pick_buy_token(
        &mut self,
        token: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let own = self.session.chain_id;
        let Some(form) = self.form.as_mut().filter(|form| form.picker.open) else {
            return;
        };
        form.picker.open = false;
        window.close_dialog(cx);
        let network = (form.picker.network != own).then_some(form.picker.network);
        if form.network == network && form.buy == Some(token) {
            cx.notify();
            return;
        }
        let network_changed = form.network != network;
        if network_changed {
            form.network = network;
            // The provider follows the new network's token, unless the user picks one again.
            form.bridge.chosen = None;
        }
        form.buy = Some(token);
        form.native_output = false;
        form.error = None;
        if network_changed {
            // An account chosen on the old network doesn't follow to the new one.
            form.clear_destination();
        }
        self.load_bridge_routes(window, cx);
        self.bridge_choices_changed(window, cx);
    }

    /// The tokens the picker lists: what the delivery kind can deliver on the shown network.
    pub(super) fn buy_picker_items(&self, form: &SwapForm, cx: &App) -> Vec<SwapBuyItem> {
        let shown = form.picker.network;
        self.buy_items_on(form, (shown != self.session.chain_id).then_some(shown), cx)
    }

    /// What the open picker shows for `form`.
    pub(super) fn buy_picker_content(&self, form: &SwapForm, cx: &App) -> BuyPickerContent {
        let own = self.session.chain_id;
        let picker = &form.picker;
        let shown = picker.network;
        let private = form.receive_to == ReceiveTo::PrivateBalance;
        let routes = (shown != own).then(|| form.bridge.routes.get(&(form.sell, shown)));
        let tokens = match routes {
            Some(None) => BuyPickerTokens::Loading,
            Some(Some(Err(error))) => BuyPickerTokens::Failed(self.quote_error_message(error, cx)),
            // NEAR's successful response cannot supply tokens for private delivery.
            Some(Some(Ok(BridgeRoutes {
                unavailable: Some((BridgeProvider::Across, error)),
                ..
            }))) if private => BuyPickerTokens::Failed(self.quote_error_message(error, cx)),
            _ => {
                let totals = private.then(|| self.private_totals(form, shown, cx));
                BuyPickerTokens::Listed(
                    self.buy_picker_items(form, cx)
                        .into_iter()
                        .map(|item| {
                            let token = item.asset.token;
                            let balance = totals.as_ref().map(|totals| {
                                let total = totals
                                    .iter()
                                    .find(|(asset, _)| *asset == token)
                                    .map_or(U256::ZERO, |(_, total)| *total);
                                self.network_bare_amount(shown, token, total, cx)
                            });
                            BuyPickerToken { item, balance }
                        })
                        .collect(),
                )
            }
        };
        BuyPickerContent {
            receive_to: form.receive_to,
            receive_to_locked: self.receive_to_locked(form),
            token_list: picker.tokens.clone(),
            network_list: picker.networks.clone(),
            network: shown,
            own_network: shown == own,
            tokens,
            networks: self.network_items(form.receive_to, cx),
            busy: self.busy(),
        }
    }
}

/// A token's icon with its network's icon as a badge at its corner.
pub(super) fn network_token_icon(icon: Option<WalletIconSource>, chain_id: u64) -> gpui::Div {
    div()
        .relative()
        .w(rems(1.25))
        .h(rems(1.25))
        .flex_none()
        .children(icon.map(|icon| {
            gpui::img(gpui::ImageSource::from(icon))
                .size_full()
                .rounded_full()
        }))
        .children(railgun_ui::chain_icon_asset_path(chain_id).map(|path| {
            gpui::img(path)
                .absolute()
                .right(-rems(0.1875))
                .bottom(-rems(0.1875))
                .w(rems(0.625))
                .h(rems(0.625))
                .rounded_full()
        }))
}

/// The tag of a token only NEAR Intents delivers.
fn near_tag() -> Tag {
    Tag::secondary()
        .outline()
        .small()
        .rounded_full()
        .flex_none()
        .text_xs()
        .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
        .child("NEAR")
}

/// The open picker: the Receive to switch over the token pane and the network pane, whose
/// lists take their rows from `content`.
fn render_buy_picker(
    view: &Entity<PrivateSwapsView>,
    content: BuyPickerContent,
    window: &Window,
    cx: &mut App,
) -> gpui::Div {
    let switch = |id: &'static str, label: &'static str, receive_to: ReceiveTo| {
        let view = view.clone();
        app_segment_button(
            id,
            label,
            content.receive_to == receive_to,
            content.receive_to_locked,
            None,
        )
        .debug_selector(move || id.into())
        .on_click(move |_, window, cx| {
            view.update(cx, |view, cx| view.set_receive_to(receive_to, window, cx));
        })
    };
    let header = div()
        .key_context(BUTTONS_KEY_CONTEXT)
        .flex()
        .flex_none()
        .flex_wrap()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(app_muted_text("Receive to").text_xs())
        .child(
            ButtonGroup::new("swap-buy-picker-receive-to")
                .outline()
                .compact()
                .disabled(content.receive_to_locked)
                .child(switch(
                    "swap-buy-picker-private",
                    "Private balance",
                    ReceiveTo::PrivateBalance,
                ))
                .child(switch(
                    "swap-buy-picker-public",
                    "Public address",
                    ReceiveTo::PublicAddress,
                )),
        );
    let panes = div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_stretch()
        .child(token_pane(&content))
        .child(network_pane(&content));
    content.sync_lists(cx);
    div()
        .debug_selector(|| "swap-buy-picker".into())
        .w_full()
        .h(window.viewport_size().height * 0.6)
        .rounded(cx.theme().radius)
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(header)
        .child(panes)
}

/// The shown network's tokens under their search. Down and Up move the selection, and Enter
/// picks it.
fn token_pane(content: &BuyPickerContent) -> gpui::Div {
    let network = network_name(content.network);
    // Only Across shields on delivery, so it alone lists another network's tokens.
    let heading = if content.receive_to == ReceiveTo::PrivateBalance && !content.own_network {
        format!("Tokens Across delivers on {network}")
    } else {
        format!("Tokens on {network}")
    };
    div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .child(app_muted_text(heading).text_xs().px_2())
        .child(
            div()
                .debug_selector(|| "swap-buy-picker-tokens".into())
                .flex_1()
                .min_h_0()
                .child(
                    List::new(&content.token_list)
                        .small()
                        .search_placeholder("Search name or paste address"),
                ),
        )
}

/// The networks under their search. Down and Up move the selection, and Enter shows its
/// tokens.
fn network_pane(content: &BuyPickerContent) -> gpui::Div {
    div()
        .w(rems(11.))
        .flex_none()
        .min_h_0()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .border_l_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(app_muted_text("Network").text_xs().px_2())
        .child(
            div()
                .debug_selector(|| "swap-buy-picker-networks".into())
                .flex_1()
                .min_h_0()
                .child(
                    List::new(&content.network_list)
                        .small()
                        .search_placeholder("Search networks"),
                ),
        )
}
