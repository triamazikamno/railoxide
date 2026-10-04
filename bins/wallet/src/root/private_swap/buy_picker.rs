//! The Buy picker behind the swap form's Buy token button: a switch for the delivery kind, the
//! tokens of one network with a search, and the networks. Picking a token closes it and sets
//! the delivery kind, network and token together.
//!
//! The form owns the picker's state. The modal dialog reads its live lists into
//! [`BuyPickerContent`] whenever it renders.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement as _, KeyBinding, KeyDownEvent, NoAction, ParentElement as _, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Task, Window, div,
    prelude::FluentBuilder as _, relative, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::ButtonGroup,
    input::{Enter as InputEnter, Escape as InputEscape, InputState},
    list::ListItem,
    select::SelectItem as _,
    spinner::Spinner,
    tag::Tag,
};
use gpui_kit::base::actions::{Confirm, SelectDown, SelectLeft, SelectRight, SelectUp};
use ui::controls::{app_input, app_muted_text, app_segment_button, app_strong_text, app_text};
use ui::recipient_picker::{
    RecipientSuggestionDirection as Direction, suggestion_index_after_move,
};
use ui::theme;
use wallet_ops::{PublicBroadcasterCandidate, WalletSession};

use super::{
    AbortOnDrop, BridgeProvider, BridgeRoutes, PrivateSwapsView, ReceiveTo, SwapBuyItem, SwapForm,
    bridge_routes_retry_button, network_name, same_offer,
};
use crate::assets::WalletIconSource;
use crate::root::{PRIVATE_ASSET_LIST_WIDTH, dialog_max_height};

/// The key contexts of the picker's two lists, for their arrow keys.
const TOKENS_KEY_CONTEXT: &str = "SwapBuyPickerTokens";
const NETWORKS_KEY_CONTEXT: &str = "SwapBuyPickerNetworks";
/// Let Enter and Space activate the focused picker button instead of the dialog's `Confirm`.
const BUTTONS_KEY_CONTEXT: &str = "SwapBuyPickerButtons";

/// Marks that the picker's key bindings are installed in this app.
struct BuyPickerBindings;

impl gpui::Global for BuyPickerBindings {}

/// Bind the picker's keys once per app. Enter and Space reach a focused list as the dialog's
/// `Confirm`.
pub(super) fn ensure_buy_picker_bindings(cx: &mut App) {
    if cx.has_global::<BuyPickerBindings>() {
        return;
    }
    cx.bind_keys([
        KeyBinding::new("up", SelectUp, Some(TOKENS_KEY_CONTEXT)),
        KeyBinding::new("down", SelectDown, Some(TOKENS_KEY_CONTEXT)),
        KeyBinding::new("right", SelectRight, Some(TOKENS_KEY_CONTEXT)),
        KeyBinding::new("up", SelectUp, Some(NETWORKS_KEY_CONTEXT)),
        KeyBinding::new("down", SelectDown, Some(NETWORKS_KEY_CONTEXT)),
        KeyBinding::new("left", SelectLeft, Some(NETWORKS_KEY_CONTEXT)),
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

/// Whether a Public address can receive on a network: it needs only RPC endpoints.
pub(super) const fn public_network_availability(rpc: bool) -> NetworkAvailability {
    if rpc {
        NetworkAvailability::Available
    } else {
        NetworkAvailability::Unavailable(NetworkUnavailable::Rpc)
    }
}

/// A network in the picker's list: the swap's own first, then the other built-in chains a
/// bridge reaches. One that can't take the delivery kind is listed with the reason.
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
    pub(super) search: Entity<InputState>,
    /// The highlighted token, which Enter picks. `None` highlights the first row.
    pub(super) token: Option<Address>,
    /// The highlighted network, which Enter shows the tokens of.
    pub(super) highlighted_network: u64,
    /// The lists' focus, which takes their arrow keys.
    pub(super) tokens_focus: FocusHandle,
    pub(super) networks_focus: FocusHandle,
    pub(super) tokens_scroll: ScrollHandle,
    funding: HashMap<u64, NetworkFunding>,
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
        Self {
            open: false,
            network,
            search: cx
                .new(|cx| InputState::new(window, cx).placeholder("Search name or paste address")),
            token: None,
            highlighted_network: network,
            tokens_focus: cx.focus_handle().tab_stop(true),
            networks_focus: cx.focus_handle().tab_stop(true),
            tokens_scroll: ScrollHandle::new(),
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
    pub(super) search: Entity<InputState>,
    pub(super) searched: bool,
    pub(super) network: u64,
    pub(super) tokens: BuyPickerTokens,
    pub(super) highlighted_token: Option<Address>,
    pub(super) networks: Vec<BuyNetwork>,
    pub(super) highlighted_network: u64,
    pub(super) tokens_focus: FocusHandle,
    pub(super) networks_focus: FocusHandle,
    pub(super) tokens_scroll: ScrollHandle,
    pub(super) busy: bool,
}

/// The highlighted token: the picker's while the list holds it, otherwise the first row.
fn highlighted_token(picker: &BuyPicker, items: &[SwapBuyItem]) -> Option<Address> {
    picker
        .token
        .filter(|token| items.iter().any(|item| item.asset.token == *token))
        .or_else(|| items.first().map(|item| item.asset.token))
}

/// The highlighted network: the picker's while it can be picked, otherwise the shown one.
fn highlighted_network(picker: &BuyPicker, networks: &[BuyNetwork]) -> u64 {
    if networks.iter().any(|network| {
        network.chain_id == picker.highlighted_network && network.availability.is_available()
    }) {
        picker.highlighted_network
    } else {
        picker.network
    }
}

impl PrivateSwapsView {
    /// Open the picker on the form's network and Buy asset, with an empty search.
    pub(super) fn open_buy_picker(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let own = self.session.chain_id;
        let busy = self.busy();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if busy || form.picker.open || (form.operation.is_some() && !form.reuse_account) {
            return;
        }
        let picker = &mut form.picker;
        picker.open = true;
        picker.network = form.network.unwrap_or(own);
        picker.token = form.buy;
        let search = picker.search.clone();
        search.update(cx, |search, cx| search.set_value("", window, cx));
        self.settle_buy_picker(window, cx);
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
                Some(render_buy_picker(&view, &content, window, cx))
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
                search.read(cx).focus_handle(cx).focus(window, cx);
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

    /// The search changed: the first match is highlighted.
    pub(super) fn buy_picker_searched(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.picker.token = None;
            form.picker.tokens_scroll.scroll_to_item(0);
            cx.notify();
        }
    }

    /// The picker opened, or Receive to changed while it is open: its lists follow the delivery
    /// kind. A shown network the kind can't take gives way to the swap's own, which only
    /// changes what the picker lists. For Private balance, the networks that aren't loaded
    /// start loading.
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
        let picker = &mut form.picker;
        if !selectable {
            picker.network = own;
        }
        picker.highlighted_network = picker.network;
        picker.tokens_scroll.scroll_to_item(0);
        if receive_to == ReceiveTo::PrivateBalance {
            self.load_private_networks(None, cx);
        }
        self.load_bridge_routes(window, cx);
        cx.notify();
    }

    /// List `chain_id`'s tokens, when the delivery kind can deliver there.
    pub(super) fn show_buy_picker_network(
        &mut self,
        chain_id: u64,
        window: &Window,
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
        let picker = &mut form.picker;
        picker.network = chain_id;
        picker.highlighted_network = chain_id;
        picker.token = None;
        picker.tokens_scroll.scroll_to_item(0);
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

    /// [`Self::buy_picker_items`] the search matches.
    fn buy_picker_matches(&self, form: &SwapForm, cx: &App) -> Vec<SwapBuyItem> {
        let query = form.picker.search.read(cx).value();
        self.buy_picker_items(form, cx)
            .into_iter()
            .filter(|item| item.asset.matches(&query))
            .collect()
    }

    fn move_buy_picker_token(&mut self, direction: Direction, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let items = self.buy_picker_matches(form, cx);
        let current = highlighted_token(&form.picker, &items)
            .and_then(|token| items.iter().position(|item| item.asset.token == token));
        let Some(index) = suggestion_index_after_move(current, items.len(), direction) else {
            return;
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.picker.token = Some(items[index].asset.token);
        form.picker.tokens_scroll.scroll_to_item(index);
        cx.notify();
    }

    /// Pick the highlighted token, as Enter does in the search and in the token list.
    fn confirm_buy_picker_token(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let items = self.buy_picker_matches(form, cx);
        if let Some(token) = highlighted_token(&form.picker, &items) {
            self.pick_buy_token(token, window, cx);
        }
    }

    /// Move the highlight among the networks that can be picked.
    fn move_buy_picker_network(&mut self, direction: Direction, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let networks = self.network_items(form.receive_to, cx);
        let highlighted = highlighted_network(&form.picker, &networks);
        let selectable = networks
            .iter()
            .filter(|network| network.availability.is_available())
            .map(|network| network.chain_id)
            .collect::<Vec<_>>();
        let current = selectable
            .iter()
            .position(|chain_id| *chain_id == highlighted);
        let Some(index) = suggestion_index_after_move(current, selectable.len(), direction) else {
            return;
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.picker.highlighted_network = selectable[index];
        cx.notify();
    }

    /// Show the highlighted network's tokens, as Enter does in the network list.
    fn confirm_buy_picker_network(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let networks = self.network_items(form.receive_to, cx);
        let highlighted = highlighted_network(&form.picker, &networks);
        self.show_buy_picker_network(highlighted, window, cx);
    }

    /// Move the keyboard to the token list, or to the search while the list has no rows.
    fn focus_buy_picker_tokens(&self, window: &mut Window, cx: &mut App) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let focus = if self.buy_picker_matches(form, cx).is_empty() {
            form.picker.search.read(cx).focus_handle(cx)
        } else {
            form.picker.tokens_focus.clone()
        };
        focus.focus(window, cx);
    }

    fn focus_buy_picker_networks(&self, window: &mut Window, cx: &mut App) {
        if let Some(form) = self.form.as_ref() {
            form.picker.networks_focus.focus(window, cx);
        }
    }

    /// What the open picker shows for `form`.
    pub(super) fn buy_picker_content(&self, form: &SwapForm, cx: &App) -> BuyPickerContent {
        let own = self.session.chain_id;
        let picker = &form.picker;
        let shown = picker.network;
        let private = form.receive_to == ReceiveTo::PrivateBalance;
        let routes = (shown != own).then(|| form.bridge.routes.get(&(form.sell, shown)));
        let items = self.buy_picker_matches(form, cx);
        let highlighted = highlighted_token(picker, &items);
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
                    items
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
        let networks = self.network_items(form.receive_to, cx);
        BuyPickerContent {
            receive_to: form.receive_to,
            receive_to_locked: self.receive_to_locked(form),
            search: picker.search.clone(),
            searched: !picker.search.read(cx).value().trim().is_empty(),
            network: shown,
            tokens,
            highlighted_token: highlighted,
            highlighted_network: highlighted_network(picker, &networks),
            networks,
            tokens_focus: picker.tokens_focus.clone(),
            networks_focus: picker.networks_focus.clone(),
            tokens_scroll: picker.tokens_scroll.clone(),
            busy: self.busy(),
        }
    }
}

/// A listener that runs `handler` on `view`, whatever its event.
fn on_view<E: 'static>(
    view: &Entity<PrivateSwapsView>,
    handler: fn(&mut PrivateSwapsView, &mut Window, &mut Context<'_, PrivateSwapsView>),
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let view = view.clone();
    move |_, window, cx| view.update(cx, |view, cx| handler(view, window, cx))
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

/// The open picker: the Receive to switch over the token pane and the network pane.
fn render_buy_picker(
    view: &Entity<PrivateSwapsView>,
    content: &BuyPickerContent,
    window: &Window,
    cx: &App,
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
    div()
        .debug_selector(|| "swap-buy-picker".into())
        .w_full()
        .h(window.viewport_size().height * 0.6)
        .rounded(cx.theme().radius)
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
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
                ),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .items_stretch()
                .child(token_pane(view, content, cx))
                .child(network_pane(view, content, cx)),
        )
}

/// The search and the shown network's tokens. Down and Up in the search move the highlight,
/// and Enter picks it.
fn token_pane(view: &Entity<PrivateSwapsView>, content: &BuyPickerContent, cx: &App) -> gpui::Div {
    let network = network_name(content.network);
    let own = content
        .networks
        .first()
        .is_some_and(|own| own.chain_id == content.network);
    // Only Across shields on delivery, so it alone lists another network's tokens.
    let heading = if content.receive_to == ReceiveTo::PrivateBalance && !own {
        format!("Tokens Across delivers on {network}")
    } else {
        format!("Tokens on {network}")
    };
    let move_view = view.clone();
    let ring = cx.theme().ring;
    // Where the list stands while it has no rows. Its Retry is a button like the header's.
    let status = |status: gpui::Div| {
        div()
            .key_context(BUTTONS_KEY_CONTEXT)
            .px_2()
            .py_1()
            .child(status)
            .into_any_element()
    };
    let list = match &content.tokens {
        BuyPickerTokens::Loading => status(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(Spinner::new().small())
                .child(
                    app_muted_text(format!("Getting the tokens {network} can receive…"))
                        .min_w_0()
                        .whitespace_normal(),
                ),
        ),
        BuyPickerTokens::Failed(error) => status(
            div()
                .flex()
                .flex_col()
                .items_start()
                .gap_2()
                .child(
                    app_text(error.clone())
                        .text_color(cx.theme().danger)
                        .whitespace_normal(),
                )
                .child(bridge_routes_retry_button(view.clone(), content.busy)),
        ),
        BuyPickerTokens::Listed(rows) if rows.is_empty() => status(
            app_muted_text(if content.searched {
                "No matching tokens".to_owned()
            } else {
                format!("No tokens to buy on {network}")
            })
            .whitespace_normal(),
        ),
        BuyPickerTokens::Listed(rows) => div()
            .id("swap-buy-picker-tokens")
            .debug_selector(|| "swap-buy-picker-tokens".into())
            .track_focus(&content.tokens_focus)
            .key_context(TOKENS_KEY_CONTEXT)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&content.tokens_scroll)
            .flex()
            .flex_col()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(gpui::transparent_black())
            .focus_visible(move |style| style.border_color(ring))
            .on_action(on_view::<SelectUp>(view, |view, _, cx| {
                view.move_buy_picker_token(Direction::Previous, cx);
            }))
            .on_action(on_view::<SelectDown>(view, |view, _, cx| {
                view.move_buy_picker_token(Direction::Next, cx);
            }))
            .on_action(on_view::<SelectRight>(view, |view, window, cx| {
                view.focus_buy_picker_networks(window, cx);
            }))
            .on_action(on_view::<Confirm>(view, |view, window, cx| {
                view.confirm_buy_picker_token(window, cx);
            }))
            .children(rows.iter().map(|row| token_row(view, content, row, cx)))
            .into_any_element(),
    };
    div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .child(
            div()
                .on_key_down(move |event: &KeyDownEvent, _, cx| {
                    let direction = match event.keystroke.key.as_str() {
                        "down" => Direction::Next,
                        "up" => Direction::Previous,
                        _ => return,
                    };
                    move_view.update(cx, |view, cx| view.move_buy_picker_token(direction, cx));
                    cx.stop_propagation();
                })
                .on_action(on_view::<InputEnter>(view, |view, window, cx| {
                    view.confirm_buy_picker_token(window, cx);
                }))
                .on_action(on_view::<InputEscape>(view, |view, window, cx| {
                    view.close_buy_picker(cx);
                    window.close_dialog(cx);
                }))
                .child(
                    app_input(&content.search)
                        .small()
                        .aria_label("Search tokens"),
                ),
        )
        .child(app_muted_text(heading).text_xs().px_2())
        .child(list)
}

fn token_row(
    view: &Entity<PrivateSwapsView>,
    content: &BuyPickerContent,
    row: &BuyPickerToken,
    cx: &App,
) -> ListItem {
    let token = row.item.asset.token;
    let id = format!("swap-buy-picker-token-{token:#x}");
    let selector = id.clone();
    let view = view.clone();
    ListItem::new(SharedString::from(id))
        .debug_selector(move || selector)
        .px_2()
        .rounded(cx.theme().radius)
        .selected(content.highlighted_token == Some(token))
        .on_click(move |_, window, cx| {
            view.update(cx, |view, cx| view.pick_buy_token(token, window, cx));
        })
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .child(network_token_icon(
                    row.item.asset.icon_path.clone(),
                    content.network,
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
        )
}

/// The networks. One the delivery kind can't deliver on is disabled with the reason.
fn network_pane(
    view: &Entity<PrivateSwapsView>,
    content: &BuyPickerContent,
    cx: &App,
) -> gpui::Div {
    let ring = cx.theme().ring;
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
        .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
        .child(app_muted_text("Network").text_xs().px_2())
        .child(
            div()
                .id("swap-buy-picker-networks")
                .debug_selector(|| "swap-buy-picker-networks".into())
                .track_focus(&content.networks_focus)
                .key_context(NETWORKS_KEY_CONTEXT)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(gpui::transparent_black())
                .focus_visible(move |style| style.border_color(ring))
                .on_action(on_view::<SelectUp>(view, |view, _, cx| {
                    view.move_buy_picker_network(Direction::Previous, cx);
                }))
                .on_action(on_view::<SelectDown>(view, |view, _, cx| {
                    view.move_buy_picker_network(Direction::Next, cx);
                }))
                .on_action(on_view::<SelectLeft>(view, |view, window, cx| {
                    view.focus_buy_picker_tokens(window, cx);
                }))
                .on_action(on_view::<Confirm>(view, |view, window, cx| {
                    view.confirm_buy_picker_network(window, cx);
                }))
                .children(
                    content
                        .networks
                        .iter()
                        .map(|network| network_row(view, content, network, cx)),
                ),
        )
}

fn network_row(
    view: &Entity<PrivateSwapsView>,
    content: &BuyPickerContent,
    network: &BuyNetwork,
    cx: &App,
) -> ListItem {
    let chain_id = network.chain_id;
    let available = network.availability.is_available();
    let note = if network.this_network {
        Some("This network".to_owned())
    } else {
        network.availability.list_reason(&network.label)
    };
    let view = view.clone();
    ListItem::new(SharedString::from(format!(
        "swap-buy-picker-network-{chain_id}"
    )))
    .debug_selector(move || format!("swap-buy-picker-network-{chain_id}"))
    .px_2()
    .rounded(cx.theme().radius)
    .disabled(!available)
    .selected(available && content.highlighted_network == chain_id)
    .confirmed(content.network == chain_id)
    .check_icon(IconName::Check)
    .on_click(move |_, window, cx| {
        view.update(cx, |view, cx| {
            view.show_buy_picker_network(chain_id, window, cx);
        });
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
                    .child(app_text(network.label.clone()).min_w_0().truncate()),
            )
            // Under the label, past the icon and the row's gap.
            .children(note.map(|note| app_muted_text(note).text_xs().pl_6().whitespace_normal())),
    )
}
