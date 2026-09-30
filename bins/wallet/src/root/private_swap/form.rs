//! The swap form: tokens, amount and slippage, the setup's broadcaster route, the quote with
//! its price check, the single review, and placing the approved order.
//!
//! A new swap is quoted before anything is paid, with a stand-in executor that has no code, as
//! a fresh stealth account has none. One review approves the setup and the private minimum.
//! The wallet then sends the setup and, once it's confirmed, quotes again for the delegated
//! stealth account. If the shield fee, the price check and the minimum still hold, a
//! confirm-only step places the order; otherwise the review opens again. Retries and changed
//! terms use the form without a setup.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use gpui::{
    Anchor, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, relative, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Colorize as _, Disableable as _, Icon, IconName, Sizable as _,
    WindowExt as _,
    alert::Alert,
    button::{ButtonGroup, ButtonVariants as _},
    checkbox::Checkbox,
    collapsible::Collapsible,
    combobox::{Combobox, ComboboxEvent, ComboboxState},
    input::{Enter as InputEnter, InputEvent, InputState},
    popover::Popover,
    select::{Caret, SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    spinner::Spinner,
    tag::Tag,
    tooltip::Tooltip,
};
use ui::controls::{
    app_amount_input, app_amount_text, app_button, app_button_base, app_button_label,
    app_muted_text, app_segment_button, app_strong_text, app_text,
};
use ui::recipient_picker::RecipientPickerEvent;
use ui::theme;
use wallet_ops::{
    DesktopPrivateSpendAuthorization, ExecutorOwner, ExecutorRecoveryFeeEstimate,
    OperationNetworkIsolation, PublicBroadcasterCandidate, PublicBroadcasterResultKind,
    PublicBroadcasterSelection, QuoteDeviationError, SwapAmountPlan, SwapAmountRequest,
    SwapBridgeClients, SwapBridgeRoute, SwapExecutor, SwapOrderOutcome, SwapOrderRequest,
    SwapPrice, SwapReview, SwapReviewChange, SwapReviewRequest, SwapSetupRequest,
    TokenAnchorRateCache, TransactionGenerationStage, WakuDeliveryClient, WalletSession,
    bridge::{
        BridgeApiError, BridgeDestination, across_destination_tokens, near_destination_tokens,
    },
    cow::{CowOrderbookClient, OrderLimitError},
    default_public_broadcaster_fee_limit,
    settings::{
        BridgeProfile, BridgeReceiverRejection, EffectiveChainConfig, EffectiveTokenRegistry,
        SwapReceiverRejection, SwapTokenEligibility, SwapTokenRole,
        resolve_effective_chain_rpc_route, swap_destination_tokens,
    },
    vault::{
        BridgeDelivery, BridgeProvider, BridgeSurplus, ExecutorOperationId, ExecutorRecord,
        SwapApproval, SwapDelivery,
    },
};

use super::dialog::{SwapDialogView, powered_by_cow};
use super::model::{
    SwapFormMode as FormMode, SwapStage, format_bps_percent, provider_name, quote_anchor_delta_bps,
    swap_cost_bps, swap_form_mode, swap_total_cost,
};
use super::{
    PrivateSwapsView, SWAP_BROADCASTER_REPUBLISH_INTERVAL, SWAP_BROADCASTER_RESPONSE_TIMEOUT,
    SwapAction, SwapJobKind, swap_delivery, swap_sell_amount, swap_tokens,
};
use crate::root::broadcaster_picker::{
    BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL, BroadcasterChoice,
    BroadcasterPickerFeeEstimateContext, BroadcasterPickerTarget, broadcaster_candidate_label,
    selected_broadcaster_label,
};
use crate::root::private_action::{
    PrivateActionAssetSelectItem, RecipientOption, UnshieldAsset, fee_token_selector,
    form_recipient_picker, private_action_asset_select_items, recipient_suggestion_moved,
    recipient_suggestion_to_confirm, recipient_suggestions_for_input,
    recipient_suggestions_toggled,
};
use crate::root::public_account::public_account_display_label;
use crate::root::public_broadcaster::{
    PublicBroadcasterFeeTokenOption, public_broadcaster_fee_token_options_from_snapshot,
    resolve_selected_public_broadcaster_fee_token,
};
use crate::root::spend_authorization::{
    SpendAuthorizationAsset, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
};
use crate::root::stealth_accounts::{RecoveryPickerContext, same_offer};
use crate::root::{
    COST_ESTIMATE_DEBOUNCE, DeliveryFormKind, format_token_amount_ceiling_for_display,
    format_unshield_amount_input, format_value_with_usd_label, native_wrapped_output_labels,
    new_text_input, parse_address,
};

const SLIPPAGE_CHOICES: [(u32, &str); 4] = [(10, "0.1%"), (50, "0.5%"), (100, "1%"), (300, "3%")];
const DEFAULT_SLIPPAGE_BPS: u32 = 50;
const QUOTE_DEBOUNCE: Duration = Duration::from_millis(600);
// Correlates overlapping quote attempts without logging a wallet or operation identifier.
static NEXT_QUOTE_TRACE_ID: AtomicU64 = AtomicU64::new(1);
/// The stealth account row's label column, in rems; the line under the select starts past it.
const ACCOUNT_LABEL_WIDTH: f32 = 7.5;
/// The narrowest a labeled row's control gets beside its label. A narrower dialog puts the
/// control under the label instead.
const ROW_CONTROL_MIN_WIDTH: f32 = 15.;
const ACCOUNT_REUSE_NOTE: &str = "Reusing this public address can link this swap to its previous activity and reduce your privacy. A new stealth account offers more privacy.";
const UNVERIFIED_PRICE_WARNING: &str = "Price couldn't be independently verified.";
/// A Public address receiver follows Private Unshield's recipient rules and suggestions, and
/// Save adds it to the public address book.
const RECEIVER_RULES: DeliveryFormKind = DeliveryFormKind::Unshield;
const ENTER_RECEIVER: &str = "Enter an address.";
/// Private Unshield's message for an entry that doesn't parse as an address.
const INVALID_RECEIVER: &str = "Enter a valid public EVM recipient address";
const EXTERNAL_DELIVERY_DISCLOSURE: &str = "The order names the receiver, and the settlement pays it in the same transaction that unshields from Railgun, so the receiver and amount are linked to that spend.";
const NEAR_INTENTS_DISCLAIMER: &str = "A bridge operator holds the funds between the deposit and delivery. Refunds depend on the 1Click service and aren't guaranteed.";
const SAME_TOKEN_BRIDGE: &str =
    "Same-token bridging isn't supported. Choose another token to receive, or sell something else.";

/// A swap the user approved in one review: its setup through a broadcaster's private fee, and
/// the order's terms, placed once the setup is confirmed.
#[derive(Clone)]
pub(super) struct SetupApproval {
    pub(super) operation: ExecutorOperationId,
    /// The stealth account is already reserved; set it up again with the same account.
    resume: bool,
    sell: Address,
    buy: Address,
    candidate: PublicBroadcasterCandidate,
    maximum_private_fee: U256,
    waku: Arc<WakuDeliveryClient>,
    /// Persisted with the setup: the approved amount, minimum, fee and price check.
    approval: SwapApproval,
    orderbook: Option<CowOrderbookClient>,
}

/// The order terms the user approved, for a stealth account that is set up.
#[derive(Clone)]
pub(super) struct OrderApproval {
    pub(super) operation: ExecutorOperationId,
    review: Arc<SwapReview>,
    /// The reviewed suggestion, or the minimum approved with the setup.
    private_minimum: U256,
    price_acknowledged: bool,
    orderbook: CowOrderbookClient,
    /// A Bridge delivery's route, as the review was quoted.
    bridge: Option<QuotedBridge>,
    /// The approved minimum on a Bridge delivery's destination network: the review's, or the
    /// one approved with the setup.
    destination_minimum: Option<U256>,
    /// Approved in a full review rather than the confirm-only step. Only a full review replaces
    /// the approval saved with the setup, which binds the swap's first order.
    full_review: bool,
}

/// The route a Bridge review was quoted with, kept for signing its order.
#[derive(Clone)]
struct QuotedBridge {
    clients: SwapBridgeClients,
    destination: BridgeDestination,
    destination_chain: EffectiveChainConfig,
}

impl QuotedBridge {
    const fn route(&self) -> SwapBridgeRoute<'_> {
        SwapBridgeRoute {
            clients: &self.clients,
            destination: &self.destination,
            destination_chain: &self.destination_chain,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PriceBlock {
    /// The quote is worse than the anchor price by more than the profile allows.
    Deviates,
}

enum QuoteState {
    Idle,
    Loading,
    /// The amount doesn't fit one swap; `largest` does.
    TooLarge {
        largest: U256,
    },
    Ready(Arc<SwapReview>),
    PriceBlocked(PriceBlock),
    Failed(eyre::Report),
}

#[derive(Default)]
struct SetupRoute {
    fee_token: Option<Address>,
    fee_options: Vec<PublicBroadcasterFeeTokenOption>,
    candidates: Vec<PublicBroadcasterCandidate>,
    selected: Option<String>,
    allow_out_of_range: bool,
    favorites_only: bool,
    estimate: Option<ExecutorRecoveryFeeEstimate>,
    estimate_candidate: Option<PublicBroadcasterCandidate>,
    estimate_error: Option<String>,
    estimate_task: Option<Task<()>>,
    estimate_revision: u64,
    next_estimate: Option<Instant>,
    refresh_task: Option<Task<()>>,
}

impl SetupRoute {
    fn invalidate_estimate(&mut self) {
        self.estimate = None;
        self.estimate_candidate = None;
        self.estimate_error = None;
        self.estimate_task = None;
        self.next_estimate = None;
        self.estimate_revision = self.estimate_revision.wrapping_add(1);
    }

    fn choice(&self) -> BroadcasterChoice {
        self.selected
            .clone()
            .map_or(BroadcasterChoice::Random, |railgun_address| {
                BroadcasterChoice::Specific { railgun_address }
            })
    }
}

/// What the form shows of the wallet's private notes. Reading them decrypts the executor
/// records, so the form keeps them and reads them again only after an observation, a record
/// change, or a change of its operation or Sell token, not on every frame.
#[derive(Default)]
struct FormAssets {
    /// [`PrivateSwapsView::sell_assets`] for the form's operation.
    sell_assets: Vec<UnshieldAsset>,
    /// Each private token's total, for the Buy panel's balance.
    totals: Vec<(Address, U256)>,
    /// The Sell token's locked notes.
    locked: U256,
    /// Whether the Sell token is a Buy option when another token is sold, which flipping
    /// needs.
    sell_receivable: bool,
}

/// A new swap's stealth account: a new one, or a set-up account to use again.
#[derive(Clone)]
struct SwapAccountSelectItem {
    /// `None` for a new stealth account.
    operation: Option<ExecutorOperationId>,
    address: Option<Address>,
    label: SharedString,
}

impl SelectItem for SwapAccountSelectItem {
    type Value = Option<ExecutorOperationId>;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.operation
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_ascii_lowercase();
        self.label.to_ascii_lowercase().contains(&query)
            || self.address.is_some_and(|address| {
                address
                    .to_checksum(None)
                    .to_ascii_lowercase()
                    .contains(&query)
            })
    }
}

/// A Buy asset. On another network, a token only NEAR Intents delivers carries its tag.
#[derive(Clone)]
struct SwapBuyItem {
    asset: PrivateActionAssetSelectItem,
    near_only: bool,
}

impl SelectItem for SwapBuyItem {
    type Value = Address;

    fn title(&self) -> SharedString {
        self.asset.title()
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        self.asset.display_title()
    }

    fn render(&self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(self.asset.render(window, cx))
            .when(self.near_only, |row| {
                row.child(
                    Tag::secondary()
                        .outline()
                        .small()
                        .rounded_full()
                        .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                        .child("NEAR Intents"),
                )
            })
    }

    fn value(&self) -> &Self::Value {
        self.asset.value()
    }

    fn matches(&self, query: &str) -> bool {
        self.asset.matches(query)
    }
}

/// A delivery network: the swap's own, or another built-in chain a bridge reaches. A chain
/// that isn't enabled with RPC endpoints is listed with the reason and can't be chosen.
#[derive(Clone)]
struct NetworkSelectItem {
    chain_id: u64,
    label: SharedString,
    this_network: bool,
    unavailable: Option<SharedString>,
}

impl SelectItem for NetworkSelectItem {
    type Value = u64;

    fn title(&self) -> SharedString {
        if self.this_network {
            format!("{} (this network)", self.label).into()
        } else {
            self.label.clone()
        }
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(
            ui::wallet_identity::chain_label_row(
                self.title(),
                railgun_ui::chain_icon_asset_path(self.chain_id),
            )
            .into_any_element(),
        )
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(ui::wallet_identity::chain_label_row(
                        self.label.clone(),
                        railgun_ui::chain_icon_asset_path(self.chain_id),
                    ))
                    .when(self.this_network, |row| {
                        row.child(app_muted_text("This network").text_xs())
                    }),
            )
            .children(
                self.unavailable
                    .clone()
                    .map(|reason| app_muted_text(reason).text_xs().whitespace_normal()),
            )
    }

    fn value(&self) -> &Self::Value {
        &self.chain_id
    }

    fn disabled(&self) -> bool {
        self.unavailable.is_some()
    }
}

/// A bridge provider. One without a route for the Buy token is listed with the reason and
/// can't be chosen.
#[derive(Clone)]
struct ProviderSelectItem {
    provider: BridgeProvider,
    unavailable: Option<SharedString>,
}

impl SelectItem for ProviderSelectItem {
    type Value = BridgeProvider;

    fn title(&self) -> SharedString {
        provider_name(self.provider).into()
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        // The list gives every row the height of one measured row, so the reason shares the
        // provider's line instead of wrapping below it.
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(app_text(provider_name(self.provider)).flex_none())
            .children(
                self.unavailable
                    .clone()
                    .map(|reason| app_muted_text(reason).min_w_0().truncate()),
            )
    }

    fn value(&self) -> &Self::Value {
        &self.provider
    }

    fn disabled(&self) -> bool {
        self.unavailable.is_some()
    }
}

/// Where the bought token goes: back to the private balance, or to a public address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiveTo {
    PrivateBalance,
    PublicAddress,
}

/// Why the form has no delivery to quote.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DeliveryProblem {
    /// The receiver as entered can't be used; the line under the Receiver field says why.
    Receiver(SharedString),
    /// No provider delivers the Buy token on the chosen network yet; the Buy panel and the
    /// Provider row say why.
    Bridge,
}

/// What a Bridge swap's providers deliver on one network for one sell token.
struct BridgeRoutes {
    across: Vec<BridgeDestination>,
    near: Vec<BridgeDestination>,
    /// Tokens that are the sell token's own asset. The Buy list offers them, so the form can
    /// say why that pair isn't bridged.
    same_asset: Vec<BridgeDestination>,
    /// The wrapped native token Across delivers on this network as the native asset.
    across_native: Option<Address>,
}

impl BridgeRoutes {
    /// The Buy token Across's `destination` is listed as: the native asset for the wrapped
    /// native token it unwraps.
    fn across_buy_token(&self, destination: &BridgeDestination) -> Address {
        if self.across_native == Some(destination.destination_token) {
            Address::ZERO
        } else {
            destination.destination_token
        }
    }

    /// Across's destination for the Buy `token`.
    fn across_for(&self, token: Address) -> Option<&BridgeDestination> {
        self.across
            .iter()
            .find(|destination| self.across_buy_token(destination) == token)
    }
}

/// A Bridge swap's choices besides the network, and the routes fetched for them.
struct BridgeChoices {
    /// The provider the user picked. `None` lets the Buy token decide: Across where it
    /// delivers, otherwise NEAR Intents.
    chosen: Option<BridgeProvider>,
    /// Across's surplus choice; NEAR Intents always bridges the surplus.
    surplus: BridgeSurplus,
    /// Keyed by sell token and destination network. A missing entry is loading.
    routes: HashMap<(Address, u64), eyre::Result<BridgeRoutes>>,
    routes_task: Option<((Address, u64), Task<()>)>,
}

/// Where a Bridge swap stands with the chosen network's routes and Buy token.
enum BridgeState<'a> {
    /// Delivery on the swap's own network.
    SameChain,
    Loading,
    Failed(&'a eyre::Report),
    NoToken,
    /// The Buy token is the sell token's own asset.
    SameToken,
    /// Neither provider delivers the Buy token any more.
    Unavailable,
    Ready {
        destination: &'a BridgeDestination,
        provider: BridgeProvider,
        across: bool,
        near: bool,
        /// Picking a token Across doesn't deliver moved the provider to NEAR Intents.
        switched: bool,
    },
}

/// The destination terms of a Bridge quote, apart from the receiver.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BridgeTerms {
    network: u64,
    token: Address,
    provider: BridgeProvider,
    surplus: BridgeSurplus,
}

/// What the form's quote was requested for, apart from a Public address receiver. The quote
/// names the executor, never the receiver, so it stays accurate while the receiver is edited.
#[derive(Clone, Copy, PartialEq, Eq)]
struct QuoteTerms {
    executor: QuoteExecutor,
    sell: Address,
    buy: Address,
    amount: U256,
    slippage_bps: u32,
    receive_to: ReceiveTo,
    bridge: Option<BridgeTerms>,
}

pub(super) struct SwapForm {
    /// The swap's executor operation once a setup was approved, or the swap being retried.
    operation: Option<ExecutorOperationId>,
    reuse_account: bool,
    /// A new swap's choice of stealth account. A started swap keeps its own and has none.
    account_select: Option<Entity<SelectState<SearchableVec<SwapAccountSelectItem>>>>,
    sell: Address,
    /// The selected Buy asset: an ERC-20 on the swap's network, see [`SwapForm::order_buy`],
    /// or on another network, the token delivered there, `Address::ZERO` for its native asset.
    buy: Option<Address>,
    /// A Public address receives the wrapped native Buy asset as the native asset. Reset to
    /// wrapped when the Buy asset changes or delivery goes back to Private.
    native_output: bool,
    sell_select: Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>,
    buy_select: Entity<ComboboxState<SearchableVec<SwapBuyItem>>>,
    /// A Public address on another network makes the swap a Bridge swap. `None` is the swap's
    /// own network, and Private delivery always has it.
    network: Option<u64>,
    network_select: Entity<SelectState<SearchableVec<NetworkSelectItem>>>,
    provider_select: Entity<SelectState<SearchableVec<ProviderSelectItem>>>,
    bridge: BridgeChoices,
    amount_input: Entity<InputState>,
    slippage_bps: u32,
    receive_to: ReceiveTo,
    /// The Public address receiver as entered; the input's value is authoritative.
    receiver_input: Entity<InputState>,
    receiver_value: String,
    receiver_suggestions_open: bool,
    receiver_suggestion_index: Option<usize>,
    receiver_suggestions_scroll: ScrollHandle,
    /// The delivery the choices and the receiver give, or why there is none. Checked again
    /// when any of them, or the stealth account, changes.
    delivery: Result<SwapDelivery, DeliveryProblem>,
    route: SetupRoute,
    assets: FormAssets,
    quote: QuoteState,
    quote_task: Option<Task<()>>,
    quote_revision: u64,
    quote_terms: Option<QuoteTerms>,
    /// The route of this form's quotes, kept for the swap's order once it has an operation.
    orderbook: Option<CowOrderbookClient>,
    /// Bridge clients on `orderbook`'s route. Set and cleared with it.
    bridge_clients: Option<SwapBridgeClients>,
    /// The bridge route the ready quote was priced with.
    quoted_bridge: Option<QuotedBridge>,
    price_acknowledged: bool,
    /// Applies only to the current form quote; editing or retrying clears it.
    high_costs_acknowledged: bool,
    error: Option<String>,
    /// The order detail this form continues or retries, which Back returns to.
    back_to_detail: Option<ExecutorOperationId>,
    details_open: bool,
    /// The setup broadcaster popover. Presses elsewhere in the form close it, so a fee token
    /// list opened inside it stays usable.
    settings_open: bool,
    /// The "You receive" hint pinned open as a popover. A new quote closes it.
    receive_help_open: bool,
    _subscriptions: Vec<Subscription>,
}

impl SwapForm {
    pub(super) const fn operation(&self) -> Option<ExecutorOperationId> {
        self.operation
    }

    /// The token the order buys: the native marker, `Address::ZERO`, for native output, and
    /// otherwise the selected Buy asset.
    fn order_buy(&self) -> Option<Address> {
        self.buy.map(|buy| {
            if self.native_output {
                Address::ZERO
            } else {
                buy
            }
        })
    }

    fn review_problem(&self, review: &SwapReview) -> Option<SharedString> {
        match &self.delivery {
            Err(DeliveryProblem::Receiver(problem)) => Some(problem.clone()),
            Err(DeliveryProblem::Bridge) => Some("Choose a token the bridge delivers.".into()),
            Ok(delivery) if *delivery != review.plan().delivery() => {
                Some("Wait for the quote.".into())
            }
            Ok(_) if !review.price_verified() && !self.price_acknowledged => {
                Some("Accept the unverified price before you review the swap.".into())
            }
            Ok(_) if high_cost_bps(review).is_some() && !self.high_costs_acknowledged => {
                Some("Confirm Swap anyway to accept the high swap costs.".into())
            }
            Ok(_) => None,
        }
    }

    /// The token the order buys on the swap's network: for a Bridge swap, the one handed to
    /// its provider, once the provider is known.
    fn quote_buy(&self) -> Option<Address> {
        match self.bridge_state() {
            BridgeState::SameChain => self.order_buy(),
            BridgeState::Ready { destination, .. } => Some(destination.intermediate),
            _ => None,
        }
    }

    fn bridge_state(&self) -> BridgeState<'_> {
        let Some(network) = self.network else {
            return BridgeState::SameChain;
        };
        let routes = match self.bridge.routes.get(&(self.sell, network)) {
            None => return BridgeState::Loading,
            Some(Err(error)) => return BridgeState::Failed(error),
            Some(Ok(routes)) => routes,
        };
        let Some(token) = self.buy else {
            return BridgeState::NoToken;
        };
        let (across, near) = (
            routes.across_for(token),
            destination_for(&routes.near, token),
        );
        let (destination, provider) = match (self.bridge.chosen, across, near) {
            (Some(BridgeProvider::NearIntents), _, Some(near)) | (_, None, Some(near)) => {
                (near, BridgeProvider::NearIntents)
            }
            (_, Some(across), _) => (across, BridgeProvider::Across),
            (_, None, None) if destination_for(&routes.same_asset, token).is_some() => {
                return BridgeState::SameToken;
            }
            (_, None, None) => return BridgeState::Unavailable,
        };
        BridgeState::Ready {
            destination,
            provider,
            across: across.is_some(),
            near: near.is_some(),
            switched: across.is_none() && self.bridge.chosen != Some(BridgeProvider::NearIntents),
        }
    }

    /// The Bridge terms a quote covers, once a provider delivers the Buy token.
    fn bridge_terms(&self) -> Option<BridgeTerms> {
        let BridgeState::Ready {
            destination,
            provider,
            ..
        } = self.bridge_state()
        else {
            return None;
        };
        Some(BridgeTerms {
            network: self.network?,
            token: destination.destination_token,
            provider,
            surplus: bridge_surplus(provider, self.bridge.surplus),
        })
    }

    /// Use `orderbook`, or none, for this form's requests, with the bridge clients on its route.
    fn set_orderbook(
        &mut self,
        orderbook: Option<CowOrderbookClient>,
        bridge_clients: Option<SwapBridgeClients>,
    ) {
        self.orderbook = orderbook;
        self.bridge_clients = bridge_clients.filter(|_| self.orderbook.is_some());
    }

    pub(super) fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    /// Open or close the receiver suggestions, keeping the highlighted row in view.
    fn set_receiver_suggestions(&mut self, (open, index): (bool, Option<usize>)) {
        if let Some(index) = index {
            self.receiver_suggestions_scroll.scroll_to_item(index);
        }
        self.receiver_suggestions_open = open;
        self.receiver_suggestion_index = index;
    }
}

/// The executor a quote is planned for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QuoteExecutor {
    /// Before the setup: a stand-in with no code, like a fresh stealth account.
    Preview,
    /// A setup retry may reuse this operation's reserved fee inputs.
    Setup(ExecutorOperationId),
    /// Account state recorded by observation; refreshed before execution preparation.
    Order {
        operation: ExecutorOperationId,
        reuse: bool,
    },
}

struct QuoteRequest {
    executor: QuoteExecutor,
    sell: Address,
    buy: Address,
    amount: U256,
    delivery: SwapDelivery,
    slippage_bps: u32,
    byte_budget: Option<usize>,
    orderbook: Option<CowOrderbookClient>,
    /// Bridge clients on `orderbook`'s route, when the form has them.
    bridge_clients: Option<SwapBridgeClients>,
    bridge: Option<QuoteBridgeRequest>,
    anchor_cache: Option<Arc<TokenAnchorRateCache>>,
    tokens: EffectiveTokenRegistry,
}

/// A Bridge delivery's route to quote.
struct QuoteBridgeRequest {
    /// The Buy list's entry, or `None` to find the approved delivery's again.
    destination: Option<BridgeDestination>,
    destination_chain: EffectiveChainConfig,
    origin: BridgeProfile,
}

enum QuoteOutcome {
    TooLarge(U256),
    Review(Box<SwapReview>),
    PriceBlocked(PriceBlock),
}

struct QuoteResult {
    orderbook: Option<CowOrderbookClient>,
    bridge_clients: Option<SwapBridgeClients>,
    /// The route a Bridge quote was priced with.
    bridge: Option<QuotedBridge>,
    outcome: eyre::Result<QuoteOutcome>,
}

enum SetupOutcome {
    /// The broadcaster's answer for the setup.
    Sent(PublicBroadcasterResultKind),
    /// The approved amount no longer fits one order; nothing was paid.
    TooLarge,
}

/// Plan the notes without proving, for a stand-in executor before setup or from recorded
/// account state, then quote without hooks and price the order
/// limit. Nothing is signed. The swap's orderbook route is returned for reuse even when
/// quoting fails.
#[tracing::instrument(
    name = "swap_quote",
    target = "swap_quote",
    level = "debug",
    skip_all,
    fields(quote_id = NEXT_QUOTE_TRACE_ID.fetch_add(1, Ordering::Relaxed))
)]
async fn quote_swap(
    owner: Arc<ExecutorOwner>,
    session: Arc<WalletSession>,
    request: QuoteRequest,
) -> QuoteResult {
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "total", "started");
    let mut orderbook = request.orderbook.clone();
    let mut bridge_clients = request
        .bridge_clients
        .clone()
        .filter(|_| orderbook.is_some());
    let mut bridge = None;
    let outcome = quote_swap_terms(
        &owner,
        &session,
        &request,
        &mut orderbook,
        &mut bridge_clients,
        &mut bridge,
    )
    .await;
    let status = match &outcome {
        Ok(QuoteOutcome::Review(_)) => "ready",
        Ok(QuoteOutcome::TooLarge(_)) => "too_large",
        Ok(QuoteOutcome::PriceBlocked(_)) => "price_blocked",
        Err(_) => "error",
    };
    tracing::debug!(
        target: "swap_quote",
        step = "total",
        elapsed_ms = started.elapsed().as_millis(),
        status,
        "finished"
    );
    QuoteResult {
        orderbook,
        bridge_clients,
        bridge,
        outcome,
    }
}

async fn quote_swap_terms(
    owner: &Arc<ExecutorOwner>,
    session: &Arc<WalletSession>,
    request: &QuoteRequest,
    orderbook: &mut Option<CowOrderbookClient>,
    bridge_clients: &mut Option<SwapBridgeClients>,
    bridge: &mut Option<QuotedBridge>,
) -> eyre::Result<QuoteOutcome> {
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "executor", "started");
    let executor = match request.executor {
        QuoteExecutor::Preview => owner.swap_preview_executor()?,
        QuoteExecutor::Setup(operation) => owner.swap_setup_preview(operation)?,
        QuoteExecutor::Order { operation, reuse } => owner.swap_order_preview(operation, reuse)?,
    };
    tracing::debug!(
        target: "swap_quote",
        step = "executor",
        elapsed_ms = started.elapsed().as_millis(),
        "finished"
    );
    let started = Instant::now();
    tracing::debug!(
        target: "swap_quote",
        step = "orderbook_client",
        reused = orderbook.is_some(),
        "started"
    );
    let client = if let Some(client) = orderbook.clone() {
        client
    } else {
        let client = Box::pin(owner.swap_orderbook_client()).await?;
        *orderbook = Some(client.clone());
        client
    };
    tracing::debug!(
        target: "swap_quote",
        step = "orderbook_client",
        elapsed_ms = started.elapsed().as_millis(),
        "finished"
    );
    if let Some(request_bridge) = &request.bridge {
        let clients = if let Some(clients) = bridge_clients.clone() {
            clients
        } else {
            let clients = owner.swap_bridge_clients(&client)?;
            *bridge_clients = Some(clients.clone());
            clients
        };
        let destination = if let Some(destination) = &request_bridge.destination {
            destination.clone()
        } else {
            approved_bridge_destination(&clients, request_bridge, request).await?
        };
        *bridge = Some(QuotedBridge {
            clients,
            destination,
            destination_chain: request_bridge.destination_chain.clone(),
        });
    }
    let amount_request = SwapAmountRequest {
        sell_token: request.sell,
        buy_token: request.buy,
        amount: request.amount,
        delivery: request.delivery,
        byte_budget: request.byte_budget,
    };
    let planning_owner = Arc::clone(owner);
    let planning_session = Arc::clone(session);
    // Note selection can take a moment on fragmented balances.
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "input_planning", "started");
    let plan = tokio::task::spawn_blocking(move || {
        planning_owner.plan_swap_amount(&executor, &planning_session, &amount_request)
    })
    .await;
    tracing::debug!(
        target: "swap_quote",
        step = "input_planning",
        elapsed_ms = started.elapsed().as_millis(),
        success = matches!(&plan, Ok(Ok(_))),
        "finished"
    );
    let plan = plan.map_err(|_| eyre::eyre!("Planning the swap stopped unexpectedly."))??;
    let plan = match plan {
        SwapAmountPlan::TooLarge { largest } => {
            return Ok(QuoteOutcome::TooLarge(largest.amount()));
        }
        SwapAmountPlan::Fits(plan) => plan,
    };
    match Box::pin(owner.review_swap(SwapReviewRequest {
        plan,
        slippage_bps: request.slippage_bps,
        orderbook: &client,
        anchor_cache: request.anchor_cache.as_deref(),
        token_registry: &request.tokens,
        bridge: bridge.as_ref().map(QuotedBridge::route),
    }))
    .await
    {
        Ok(review) => Ok(QuoteOutcome::Review(Box::new(review))),
        Err(error)
            if matches!(
                error.downcast_ref::<QuoteDeviationError>(),
                Some(QuoteDeviationError::ExceedsThreshold)
            ) =>
        {
            Ok(QuoteOutcome::PriceBlocked(PriceBlock::Deviates))
        }
        Err(error) => Err(error),
    }
}

/// What each provider delivers on `destination`'s chain for `sell`, asked on `clients`' route.
async fn fetch_bridge_routes(
    clients: &SwapBridgeClients,
    origin: BridgeProfile,
    destination: BridgeProfile,
    sell: Address,
    tokens: &EffectiveTokenRegistry,
    across_native: Option<Address>,
) -> eyre::Result<BridgeRoutes> {
    let chain = destination.chain_id();
    let (routes, listed) = tokio::try_join!(
        clients.across.available_routes(origin.chain_id(), chain),
        clients.near.tokens(),
    )?;
    let across = across_destination_tokens(&routes, sell, tokens, chain);
    let near = near_destination_tokens(&listed, &origin, &destination, sell, tokens);
    // Without a sell token to leave out, the lists also hold the sell token's own asset.
    let mut same_asset: Vec<BridgeDestination> = Vec::new();
    for candidate in across_destination_tokens(&routes, Address::ZERO, tokens, chain)
        .into_iter()
        .chain(near_destination_tokens(
            &listed,
            &origin,
            &destination,
            Address::ZERO,
            tokens,
        ))
    {
        let token = candidate.destination_token;
        if destination_for(&across, token).is_none()
            && destination_for(&near, token).is_none()
            && destination_for(&same_asset, token).is_none()
        {
            same_asset.push(candidate);
        }
    }
    Ok(BridgeRoutes {
        across,
        near,
        same_asset,
        across_native,
    })
}

/// The approved Bridge delivery's destination in its provider's list today. It must still
/// bridge the approved bought token.
async fn approved_bridge_destination(
    clients: &SwapBridgeClients,
    bridge: &QuoteBridgeRequest,
    request: &QuoteRequest,
) -> eyre::Result<BridgeDestination> {
    let SwapDelivery::Bridge(delivery) = request.delivery else {
        return Err(eyre::eyre!("Only a Bridge swap has a bridge route."));
    };
    let destination = bridge
        .destination_chain
        .bridge_profile()
        .ok_or_else(|| eyre::eyre!("Bridging to this network isn't supported."))?;
    let routes = fetch_bridge_routes(
        clients,
        bridge.origin,
        destination,
        request.sell,
        &request.tokens,
        across_unwrapped_token(&bridge.destination_chain),
    )
    .await?;
    let offered = match delivery.provider {
        BridgeProvider::Across => routes.across,
        BridgeProvider::NearIntents => routes.near,
    };
    offered
        .into_iter()
        .find(|offered| {
            (offered.destination_token, offered.intermediate)
                == (delivery.destination_token, request.buy)
        })
        .ok_or_else(|| {
            eyre::eyre!(
                "{} no longer offers the approved route. Open the swap to review it again.",
                provider_name(delivery.provider)
            )
        })
}

struct BridgeRoutesResult {
    orderbook: Option<CowOrderbookClient>,
    bridge_clients: Option<SwapBridgeClients>,
    routes: eyre::Result<BridgeRoutes>,
}

/// Fetch the Buy list of a Bridge swap on the swap's orderbook route, creating that route if
/// the form has none yet.
async fn request_bridge_routes(
    owner: Arc<ExecutorOwner>,
    orderbook: Option<CowOrderbookClient>,
    bridge_clients: Option<SwapBridgeClients>,
    origin: BridgeProfile,
    destination: BridgeProfile,
    sell: Address,
    tokens: EffectiveTokenRegistry,
    across_native: Option<Address>,
) -> BridgeRoutesResult {
    let mut bridge_clients = bridge_clients.filter(|_| orderbook.is_some());
    let mut orderbook = orderbook;
    let routes = async {
        let client = if let Some(client) = orderbook.clone() {
            client
        } else {
            let client = Box::pin(owner.swap_orderbook_client()).await?;
            orderbook = Some(client.clone());
            client
        };
        let clients = if let Some(clients) = bridge_clients.clone() {
            clients
        } else {
            let clients = owner.swap_bridge_clients(&client)?;
            bridge_clients = Some(clients.clone());
            clients
        };
        fetch_bridge_routes(&clients, origin, destination, sell, &tokens, across_native).await
    }
    .await;
    BridgeRoutesResult {
        orderbook,
        bridge_clients,
        routes,
    }
}

impl PrivateSwapsView {
    pub(super) fn open_new_form(
        &mut self,
        sell: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        self.open_form(
            None,
            sell,
            None,
            None,
            None,
            SwapDelivery::Reshield,
            window,
            cx,
        );
    }

    /// Explicit account selection keeps normal swaps on their fresh-account path.
    pub(in crate::root) fn open_account_form(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        let record = self.owner.records().ok().and_then(|records| {
            records
                .into_iter()
                .find(|record| record.operation() == operation)
        });
        let Some(record) = record.filter(|record| record.address().is_some()) else {
            return;
        };
        let sell = swap_tokens(&record).map(|(sell, _)| sell).or_else(|| {
            self.sell_assets(Some(operation), cx)
                .first()
                .map(|asset| asset.token)
        });
        let Some(sell) = sell else {
            return;
        };
        self.open_form(
            None,
            sell,
            None,
            None,
            None,
            SwapDelivery::Reshield,
            window,
            cx,
        );
        // Offer this account even when it can't swap now; its check explains why.
        let items = self.swap_account_items(Some(&record), cx);
        if let Some(select) = self
            .form
            .as_ref()
            .and_then(|form| form.account_select.clone())
        {
            select.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(items), window, cx);
            });
        }
        self.select_form_account(Some(operation), window, cx);
    }

    /// The accounts a new swap can use: a new one first, then set-up accounts, newest first.
    /// `chosen` is listed even when the local rules leave it out.
    fn swap_account_items(
        &self,
        chosen: Option<&ExecutorRecord>,
        cx: &App,
    ) -> Vec<SwapAccountSelectItem> {
        let mut accounts = self
            .owner
            .swap_account_candidates()
            .unwrap_or_default()
            .into_iter()
            .map(|candidate| {
                (
                    candidate.operation(),
                    candidate.index(),
                    candidate.address(),
                    candidate.is_hidden(),
                    candidate.last_pair(),
                )
            })
            .collect::<Vec<_>>();
        if let Some(record) = chosen
            && let Some(address) = record.address()
            && !accounts
                .iter()
                .any(|(operation, ..)| *operation == record.operation())
        {
            accounts.push((
                record.operation(),
                record.index(),
                address,
                record.is_hidden(),
                record
                    .swap()
                    .map(|swap| (swap.terms().sell_token(), swap.terms().buy_token())),
            ));
        }
        accounts.sort_by_key(|(_, index, ..)| std::cmp::Reverse(*index));
        let mut items = vec![SwapAccountSelectItem {
            operation: None,
            address: None,
            label: "New account (recommended)".into(),
        }];
        items.extend(
            accounts
                .into_iter()
                .map(|(operation, index, address, hidden, last_pair)| {
                    let mut label = vec![format!("#{index}"), railgun_ui::short_address(&address)];
                    if hidden {
                        label.push("Hidden".into());
                    }
                    if let Some((sell, buy)) = last_pair {
                        label.push(format!(
                            "Last used {} → {}",
                            self.token_symbol(sell, cx),
                            self.token_symbol(buy, cx)
                        ));
                    }
                    SwapAccountSelectItem {
                        operation: Some(operation),
                        address: Some(address),
                        label: label.join(" · ").into(),
                    }
                }),
        );
        items
    }

    /// Use a set-up stealth account for this new swap, or go back to a new account. The
    /// tokens, amount and slippage stay, and the swap is quoted again using recorded account
    /// state. Execution preparation checks the chosen account before proving.
    fn select_form_account(
        &mut self,
        operation: Option<ExecutorOperationId>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let busy = self.busy();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(select) = form.account_select.clone() else {
            return;
        };
        if !busy && form.operation != operation {
            if let Some(operation) = operation {
                self.tracking.entry(operation).or_default().auto_place = false;
            }
            form.operation = operation;
            form.reuse_account = operation.is_some();
            form.error = None;
            // Each account quotes on its own orderbook route, which keeps the accounts
            // unlinked there.
            form.set_orderbook(None, None);
            form.route.invalidate_estimate();
        }
        let current = form.operation;
        if select.read(cx).selected_value() != Some(&current) {
            select.update(cx, |select, cx| {
                select.set_selected_value(&current, window, cx);
            });
        }
        // Load the chosen account's record, which may not belong to a swap, and drop a
        // previous one.
        self.reload_records();
        // The account decides which notes the swap can spend.
        self.refresh_form_assets(cx);
        // A receiver can't be the swap's own stealth account.
        self.refresh_form_delivery(cx);
        self.refresh_setup_route(cx);
        self.schedule_quote(window, cx);
        cx.notify();
    }

    /// Continue or retry a swap. A retired setup starts a new review with a fresh account. The
    /// swap's delivery comes back with its tokens: approved, or the retried attempt's, which a
    /// retry keeps. A new swap, from a retired setup or a new order, can change it.
    pub(super) fn open_existing_form(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.session_is_current(cx) || self.busy() {
            return;
        }
        let Some(record) = self.record(operation) else {
            return;
        };
        if self.setup_retry_problem(record).is_some() {
            return;
        }
        if let Some(pending) = self.pending_order(record) {
            self.open_form(
                Some(operation),
                pending.sell,
                Some(pending.buy),
                Some(pending.amount),
                Some(pending.slippage_bps),
                pending.delivery,
                window,
                cx,
            );
            if let Some(form) = self.form.as_mut() {
                form.back_to_detail = Some(operation);
                form.reuse_account = pending.reuse_account;
            }
            self.schedule_quote(window, cx);
            return;
        }
        let Some((sell, buy)) = swap_tokens(record) else {
            return;
        };
        let tracking = self.tracking.get(&operation);
        let amount =
            swap_sell_amount(record).or_else(|| tracking.and_then(|tracking| tracking.amount));
        let slippage = record
            .swap()
            .and_then(|swap| swap.orders().last())
            .map(|order| order.bounds().slippage_bps)
            .or_else(|| {
                record
                    .swap_approval()
                    .map(|approval| approval.bounds.slippage_bps)
            })
            .or_else(|| tracking.and_then(|tracking| tracking.slippage_bps));
        let delivery = swap_delivery(record);
        let existing = (self.stage(record) != SwapStage::SetupRetired).then_some(operation);
        self.open_form(
            existing,
            sell,
            Some(buy),
            amount,
            slippage,
            delivery,
            window,
            cx,
        );
        if let Some(form) = self.form.as_mut() {
            form.back_to_detail = Some(operation);
        }
    }

    /// Open the form with its fields filled in. A native `buy`, which only a Public address
    /// `delivery` receives, fills in as the wrapped native Buy asset with native output, before
    /// the Buy options are built or a quote is scheduled. A Bridge `delivery` fills in its
    /// network, provider, surplus choice and destination token, which replaces `buy`, the token
    /// handed to the provider.
    #[allow(clippy::too_many_arguments)]
    fn open_form(
        &mut self,
        operation: Option<ExecutorOperationId>,
        sell: Address,
        buy: Option<Address>,
        amount: Option<U256>,
        slippage_bps: Option<u32>,
        delivery: SwapDelivery,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (receive_to, receiver) = match delivery {
            SwapDelivery::Reshield => (ReceiveTo::PrivateBalance, String::new()),
            SwapDelivery::External { receiver }
            | SwapDelivery::Bridge(BridgeDelivery { receiver, .. }) => {
                (ReceiveTo::PublicAddress, receiver.to_checksum(None))
            }
        };
        let mut bridge = BridgeChoices {
            chosen: None,
            surplus: BridgeSurplus::Reshield,
            routes: HashMap::new(),
            routes_task: None,
        };
        let (network, buy, native_output) = match (delivery, buy) {
            (SwapDelivery::Bridge(delivery), _) => {
                // The approved provider stays, even where the other one also delivers.
                bridge.chosen = Some(delivery.provider);
                if delivery.surplus == BridgeSurplus::KeepInAccount {
                    bridge.surplus = BridgeSurplus::KeepInAccount;
                }
                (
                    Some(delivery.destination_chain),
                    Some(self.bridge_received_token(delivery, cx)),
                    false,
                )
            }
            (_, Some(Address::ZERO)) => {
                let wrapped = self.wrapped_native_token(cx);
                (None, wrapped, wrapped.is_some())
            }
            (_, buy) => (None, buy, false),
        };
        let assets = self.form_assets(operation, sell, cx);
        let sell_items = private_action_asset_select_items(&assets.sell_assets);
        let sell_index = select_index(&sell_items, sell);
        // A Bridge swap's Buy list fills in once its network's routes load.
        let buy_items = if network.is_some() {
            Vec::new()
        } else {
            same_chain_buy_items(self.buy_items(sell, cx))
        };
        let buy_selected = buy.and_then(|buy| buy_index(&buy_items, buy));
        let network_items = self.network_items(cx);
        let network_index = network_items
            .iter()
            .position(|item| item.chain_id == network.unwrap_or(self.session.chain_id))
            .map(|index| gpui_component::IndexPath::default().row(index));
        let decimals = self.token_decimals(sell, cx);
        let sell_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(sell_items), sell_index, window, cx)
                .searchable(true)
        });
        let buy_select = cx.new(|cx| {
            ComboboxState::new(
                SearchableVec::new(buy_items),
                buy_selected.into_iter().collect(),
                window,
                cx,
            )
            .searchable(true)
        });
        let network_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(network_items), network_index, window, cx)
        });
        // Filled in once the network's routes and the Buy token give a provider.
        let provider_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(Vec::<ProviderSelectItem>::new()),
                None,
                window,
                cx,
            )
        });
        let amount_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("0.0");
            if let Some(amount) = amount {
                input.set_value(format_unshield_amount_input(amount, decimals), window, cx);
            }
            input
        });
        let receiver_input = new_text_input(window, cx, "0x address");
        if !receiver.is_empty() {
            receiver_input.update(cx, |input, cx| {
                input.set_value(receiver.clone(), window, cx);
            });
        }
        let account_select = operation.is_none().then(|| {
            let items = self.swap_account_items(None, cx);
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(items),
                    Some(gpui_component::IndexPath::default()),
                    window,
                    cx,
                )
                .searchable(true)
            })
        });
        let mut subscriptions = vec![
            cx.subscribe_in(
                &amount_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change)
                        && this
                            .form
                            .as_ref()
                            .is_some_and(|form| form.amount_input == *input)
                    {
                        this.form_inputs_changed(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &sell_select,
                window,
                |this,
                 _,
                 event: &SelectEvent<SearchableVec<PrivateActionAssetSelectItem>>,
                 window,
                 cx| {
                    if let SelectEvent::Confirm(Some(token)) = event {
                        this.set_form_sell(*token, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &buy_select,
                window,
                |this, _, event: &ComboboxEvent<SearchableVec<SwapBuyItem>>, window, cx| {
                    if let ComboboxEvent::Confirm(tokens) = event
                        && let Some(token) = tokens.first()
                    {
                        this.set_form_buy(*token, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &network_select,
                window,
                |this, _, event: &SelectEvent<SearchableVec<NetworkSelectItem>>, window, cx| {
                    if let SelectEvent::Confirm(Some(chain_id)) = event {
                        this.set_form_network(*chain_id, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &provider_select,
                window,
                |this, _, event: &SelectEvent<SearchableVec<ProviderSelectItem>>, window, cx| {
                    if let SelectEvent::Confirm(Some(provider)) = event {
                        this.set_form_provider(*provider, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &receiver_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if this
                        .form
                        .as_ref()
                        .is_none_or(|form| form.receiver_input != *input)
                    {
                        return;
                    }
                    match event {
                        InputEvent::Change => this.receiver_edited(window, cx),
                        InputEvent::PressEnter { .. } => {
                            this.confirm_receiver_suggestion(window, cx);
                        }
                        _ => {}
                    }
                },
            ),
        ];
        if let Some(select) = &account_select {
            subscriptions.push(cx.subscribe_in(
                select,
                window,
                |this, _, event: &SelectEvent<SearchableVec<SwapAccountSelectItem>>, window, cx| {
                    if let SelectEvent::Confirm(Some(operation)) = event {
                        this.select_form_account(*operation, window, cx);
                    }
                },
            ));
        }
        let mut form = SwapForm {
            operation,
            reuse_account: false,
            account_select,
            sell,
            buy,
            native_output,
            sell_select,
            buy_select,
            network,
            network_select,
            provider_select,
            bridge,
            amount_input,
            slippage_bps: slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS),
            receive_to,
            receiver_input,
            receiver_value: receiver,
            receiver_suggestions_open: false,
            receiver_suggestion_index: None,
            receiver_suggestions_scroll: ScrollHandle::new(),
            delivery: Ok(SwapDelivery::Reshield),
            route: SetupRoute::default(),
            assets,
            quote: QuoteState::Idle,
            quote_task: None,
            quote_revision: 0,
            quote_terms: None,
            orderbook: None,
            bridge_clients: None,
            quoted_bridge: None,
            price_acknowledged: false,
            high_costs_acknowledged: false,
            error: None,
            back_to_detail: None,
            details_open: false,
            settings_open: false,
            receive_help_open: false,
            _subscriptions: subscriptions,
        };
        // A prefilled receiver is checked again, against this form's stealth account.
        form.delivery = self.form_delivery(&form, cx);
        self.form = Some(form);
        self.start_setup_route_updates(cx);
        self.load_bridge_routes(window, cx);
        self.schedule_quote(window, cx);
        self.show_view(SwapDialogView::Form, window, cx);
    }

    fn form_mode(&self, form: &SwapForm) -> FormMode {
        if form.reuse_account {
            return FormMode::Order;
        }
        swap_form_mode(
            form.operation
                .and_then(|operation| self.record(operation))
                .map(|record| self.stage(record)),
        )
    }

    pub(super) fn sell_assets(
        &self,
        operation: Option<ExecutorOperationId>,
        cx: &App,
    ) -> Vec<UnshieldAsset> {
        let Some(root) = self.root.upgrade() else {
            return Vec::new();
        };
        let root = root.read(cx);
        let Some(profile) = root
            .effective_chain_configs
            .get(self.session.chain_id)
            .and_then(EffectiveChainConfig::swap_profile)
        else {
            return Vec::new();
        };
        root.private_action_asset_options(DeliveryFormKind::Unshield, self.session.chain_id)
            .into_iter()
            .filter(|asset| {
                profile.token_eligibility(asset.token, SwapTokenRole::Sell)
                    == SwapTokenEligibility::Eligible
            })
            .map(|mut asset| {
                asset.max_batched = self
                    .owner
                    .max_swap_amount(&self.session, operation, asset.token)
                    .unwrap_or_default();
                asset
            })
            .collect()
    }

    /// Read the form's [`FormAssets`] again.
    pub(super) fn refresh_form_assets(&mut self, cx: &App) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let assets = self.form_assets(form.operation, form.sell, cx);
        if let Some(form) = self.form.as_mut() {
            form.assets = assets;
        }
    }

    fn form_assets(
        &self,
        operation: Option<ExecutorOperationId>,
        sell: Address,
        cx: &App,
    ) -> FormAssets {
        let totals = self.root.upgrade().map_or_else(Vec::new, |root| {
            root.read(cx)
                .private_action_asset_options(DeliveryFormKind::Unshield, self.session.chain_id)
                .into_iter()
                .map(|asset| (asset.token, asset.total))
                .collect()
        });
        FormAssets {
            sell_assets: self.sell_assets(operation, cx),
            totals,
            locked: self.session.locked_note_value(sell),
            // The Buy list leaves out the token sold, and never lists the native asset.
            sell_receivable: self
                .buy_items(Address::ZERO, cx)
                .iter()
                .any(|item| item.token == sell),
        }
    }

    /// The destination list: configured tokens the swap profile accepts. Native output is a
    /// switch on the wrapped native token, not an entry.
    fn buy_items(&self, sell: Address, cx: &App) -> Vec<PrivateActionAssetSelectItem> {
        let Some(root) = self.root.upgrade() else {
            return Vec::new();
        };
        let root = root.read(cx);
        let Some(profile) = root
            .effective_chain_configs
            .get(self.session.chain_id)
            .and_then(EffectiveChainConfig::swap_profile)
        else {
            return Vec::new();
        };
        let mut items = swap_destination_tokens(&root.effective_token_registry, &profile)
            .into_iter()
            .filter_map(|info| {
                let token = info.token_address.parse::<Address>().ok()?;
                (token != sell).then(|| PrivateActionAssetSelectItem {
                    token,
                    label: Arc::from(info.symbol.as_str()),
                    icon_path: self.token_icon(token, cx),
                })
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| left.label.cmp(&right.label));
        items
    }

    /// A Bridge swap's destination list: what either provider delivers, native first. Tokens
    /// only NEAR Intents delivers carry its tag. The wrapped native token Across unwraps is
    /// listed as the native asset. The sell token's own asset stays listed, so picking it
    /// explains why the pair isn't bridged.
    fn bridge_buy_items(&self, network: u64, routes: &BridgeRoutes, cx: &App) -> Vec<SwapBuyItem> {
        let mut items: Vec<SwapBuyItem> = Vec::new();
        for (token, destination, near_only) in routes
            .across
            .iter()
            .map(|destination| (routes.across_buy_token(destination), destination, false))
            .chain(
                routes
                    .near
                    .iter()
                    .map(|destination| (destination.destination_token, destination, true)),
            )
            .chain(
                routes
                    .same_asset
                    .iter()
                    .map(|destination| (destination.destination_token, destination, false)),
            )
        {
            if items.iter().any(|item| item.asset.token == token) {
                continue;
            }
            let metadata = self.chain_token_metadata(network, token, cx);
            // Across's unwrapped token takes the native asset's name.
            let label = match &metadata {
                Some(metadata) if token != destination.destination_token => &metadata.symbol,
                _ => &destination.symbol,
            };
            items.push(SwapBuyItem {
                asset: PrivateActionAssetSelectItem {
                    token,
                    label: Arc::from(label.as_str()),
                    icon_path: metadata.and_then(|metadata| metadata.icon_path),
                },
                near_only,
            });
        }
        items.sort_by(|left, right| {
            (left.asset.token != Address::ZERO, &left.asset.label)
                .cmp(&(right.asset.token != Address::ZERO, &right.asset.label))
        });
        items
    }

    /// The Buy list for the form's network and sell token. A Bridge swap's is empty until its
    /// routes load.
    fn buy_select_items(&self, form: &SwapForm, cx: &App) -> Vec<SwapBuyItem> {
        match form.network {
            None => same_chain_buy_items(self.buy_items(form.sell, cx)),
            Some(network) => match form.bridge.routes.get(&(form.sell, network)) {
                Some(Ok(routes)) => self.bridge_buy_items(network, routes, cx),
                _ => Vec::new(),
            },
        }
    }

    /// Rebuild the Buy list and select the form's Buy asset in it.
    fn refresh_buy_items(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let items = self.buy_select_items(form, cx);
        let selected = form.buy.and_then(|buy| buy_index(&items, buy));
        form.buy_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            select.set_selected_indices(selected, window, cx);
        });
    }

    /// The networks a Public address can receive on: this one first, then the other built-in
    /// chains a bridge reaches. One that isn't enabled with RPC endpoints can't be chosen.
    fn network_items(&self, cx: &App) -> Vec<NetworkSelectItem> {
        let own = self.session.chain_id;
        let mut items = vec![NetworkSelectItem {
            chain_id: own,
            label: network_name(own).into(),
            this_network: true,
            unavailable: None,
        }];
        let Some(root) = self.root.upgrade() else {
            return items;
        };
        let root = root.read(cx);
        if root
            .effective_chain_configs
            .get(own)
            .and_then(EffectiveChainConfig::bridge_profile)
            .is_none()
        {
            return items;
        }
        items.extend(
            root.effective_chain_configs
                .values()
                .filter(|chain| chain.chain_id != own && chain.bridge_profile().is_some())
                .map(|chain| {
                    let label = network_name(chain.chain_id);
                    let unavailable = resolve_effective_chain_rpc_route(chain.chain_id, chain)
                        .is_err()
                        .then(|| format!("Enable {label} with RPC endpoints first").into());
                    NetworkSelectItem {
                        chain_id: chain.chain_id,
                        label: label.into(),
                        this_network: false,
                        unavailable,
                    }
                }),
        );
        items
    }

    /// The swap network's bridge parameters.
    fn origin_bridge_profile(&self, cx: &App) -> Option<BridgeProfile> {
        self.root
            .upgrade()?
            .read(cx)
            .effective_chain_configs
            .get(self.session.chain_id)?
            .bridge_profile()
    }

    /// The effective configuration of the form's destination network.
    fn destination_chain(&self, form: &SwapForm, cx: &App) -> Option<EffectiveChainConfig> {
        let network = form.network?;
        self.root
            .upgrade()?
            .read(cx)
            .effective_chain_configs
            .get(network)
            .cloned()
    }

    /// Fetch the routes of the form's network for its sell token, unless they are known or
    /// already loading. A missing entry reads as loading.
    fn load_bridge_routes(&mut self, window: &Window, cx: &Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(network) = form.network else {
            return;
        };
        let key = (form.sell, network);
        if form.bridge.routes.contains_key(&key)
            || form
                .bridge
                .routes_task
                .as_ref()
                .is_some_and(|(loading, _)| *loading == key)
        {
            return;
        }
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (origin, destination, tokens, across_native) = {
            let root = root.read(cx);
            let profile = |chain_id| {
                root.effective_chain_configs
                    .get(chain_id)
                    .and_then(EffectiveChainConfig::bridge_profile)
            };
            (
                profile(self.session.chain_id),
                profile(network),
                root.effective_token_registry.clone(),
                root.effective_chain_configs
                    .get(network)
                    .and_then(across_unwrapped_token),
            )
        };
        let (Some(origin), Some(destination)) = (origin, destination) else {
            return;
        };
        let tracked = form
            .operation
            .and_then(|operation| self.tracking.get(&operation))
            .and_then(|tracking| tracking.orderbook.clone());
        let orderbook = form.orderbook.clone().or(tracked);
        let bridge_clients = form
            .orderbook
            .as_ref()
            .and_then(|_| form.bridge_clients.clone());
        let operation = form.operation;
        let owner = Arc::clone(&self.owner);
        let runtime = self.runtime.clone();
        let task = cx.spawn_in(window, async move |view, cx| {
            let result = runtime
                .spawn(request_bridge_routes(
                    owner,
                    orderbook,
                    bridge_clients,
                    origin,
                    destination,
                    key.0,
                    tokens,
                    across_native,
                ))
                .await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_bridge_routes(operation, key, result.ok(), window, cx);
            });
        });
        if let Some(form) = self.form.as_mut() {
            form.bridge.routes_task = Some((key, task));
        }
    }

    fn apply_bridge_routes(
        &mut self,
        operation: Option<ExecutorOperationId>,
        key: (Address, u64),
        result: Option<BridgeRoutesResult>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.operation != operation
            || form
                .bridge
                .routes_task
                .as_ref()
                .is_none_or(|(loading, _)| *loading != key)
        {
            return;
        }
        form.bridge.routes_task = None;
        let routes = match result {
            Some(result) => {
                // The form keeps its own route when it got one meanwhile.
                if form.orderbook.is_none()
                    && let Some(orderbook) = result.orderbook
                {
                    form.set_orderbook(Some(orderbook.clone()), result.bridge_clients);
                    if let Some(operation) = operation {
                        self.tracking.entry(operation).or_default().orderbook = Some(orderbook);
                    }
                }
                result.routes
            }
            None => Err(eyre::eyre!(
                "Loading the bridge routes stopped unexpectedly. Try again."
            )),
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.bridge.routes.insert(key, routes);
        self.bridge_choices_changed(window, cx);
    }

    /// Ask the providers again on a fresh route, as quote retries do.
    fn retry_bridge_routes(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(network) = form.network else {
            return;
        };
        form.bridge.routes.remove(&(form.sell, network));
        form.bridge.routes_task = None;
        form.set_orderbook(None, None);
        if let Some(tracking) = form
            .operation
            .and_then(|operation| self.tracking.get_mut(&operation))
        {
            tracking.orderbook = None;
        }
        self.load_bridge_routes(window, cx);
        cx.notify();
    }

    /// The network, Buy token, provider, routes or sell token changed: rebuild the Buy and
    /// Provider lists, check the delivery again and quote it, which clears acknowledgements.
    fn bridge_choices_changed(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.refresh_buy_items(window, cx);
        self.refresh_provider_select(window, cx);
        self.delivery_changed(window, cx);
    }

    /// List both providers, each unavailable one with its reason, and select the one the Buy
    /// token uses. Without one, the select shows why.
    fn refresh_provider_select(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let (items, selected) = match form.bridge_state() {
            BridgeState::Ready {
                destination,
                provider,
                across,
                near,
                ..
            } => {
                let network = network_name(form.network.unwrap_or(self.session.chain_id));
                let across_reason = (!across).then(|| {
                    let reason = if destination.destination_token == Address::ZERO {
                        format!("Doesn't deliver {}", destination.symbol)
                    } else {
                        format!("No route to {network}")
                    };
                    reason.into()
                });
                let near_reason = (!near).then(|| format!("Not listed on {network}").into());
                (
                    vec![
                        ProviderSelectItem {
                            provider: BridgeProvider::Across,
                            unavailable: across_reason,
                        },
                        ProviderSelectItem {
                            provider: BridgeProvider::NearIntents,
                            unavailable: near_reason,
                        },
                    ],
                    Some(provider),
                )
            }
            _ => (Vec::new(), None),
        };
        form.provider_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            if let Some(provider) = selected {
                select.set_selected_value(&provider, window, cx);
            } else {
                select.set_selected_index(None, window, cx);
            }
        });
    }

    fn token_decimals(&self, token: Address, cx: &App) -> Option<u8> {
        self.token_metadata(token, cx)
            .map(|metadata| metadata.decimals)
    }

    /// The chain's wrapped native token, which a Public address can receive as the native asset.
    fn wrapped_native_token(&self, cx: &App) -> Option<Address> {
        let root = self.root.upgrade()?;
        root.read(cx)
            .effective_chain_configs
            .get(self.session.chain_id)?
            .wrapped_native_token
    }

    /// Whether the Buy panel offers native output: a Public address receives the chain's
    /// wrapped native token.
    fn offers_native_output(&self, form: &SwapForm, cx: &App) -> bool {
        form.receive_to == ReceiveTo::PublicAddress
            && form.network.is_none()
            && form.buy.is_some()
            && form.buy == self.wrapped_native_token(cx)
    }

    fn form_amount(&self, form: &SwapForm, cx: &App) -> Result<U256, String> {
        let amount = wallet_ops::parse_unshield_amount(
            &form.amount_input.read(cx).value(),
            self.token_decimals(form.sell, cx),
        )
        .map_err(|error| format!("{error:#}"))?;
        if amount.is_zero() {
            return Err("Enter an amount to swap.".into());
        }
        Ok(amount)
    }

    fn form_inputs_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.error = None;
        }
        self.schedule_quote(window, cx);
        cx.notify();
    }

    fn set_form_sell(&mut self, token: Address, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.form.as_ref().is_none_or(|form| {
            (form.operation.is_some() && !form.reuse_account) || form.sell == token
        }) {
            return;
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.sell = token;
        form.error = None;
        if form.network.is_none() && form.buy == Some(token) {
            form.buy = None;
            form.native_output = false;
        }
        form.route.invalidate_estimate();
        self.refresh_form_assets(cx);
        // A Bridge swap's routes depend on the sell token.
        self.load_bridge_routes(window, cx);
        self.refresh_buy_items(window, cx);
        self.refresh_provider_select(window, cx);
        self.refresh_setup_route(cx);
        self.refresh_form_delivery(cx);
        self.schedule_quote(window, cx);
        cx.notify();
    }

    fn set_form_buy(&mut self, token: Address, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if (form.operation.is_some() && !form.reuse_account) || form.buy == Some(token) {
            return;
        }
        form.buy = Some(token);
        form.native_output = false;
        form.error = None;
        if form.network.is_some() {
            // The token decides the provider, unless the user picked one.
            self.refresh_provider_select(window, cx);
            self.refresh_form_delivery(cx);
        }
        self.schedule_quote(window, cx);
        cx.notify();
    }

    /// Deliver on `chain_id`: another network makes the swap a Bridge swap. Its Buy tokens are
    /// that network's, so the Buy asset and any provider choice reset.
    fn set_form_network(&mut self, chain_id: u64, window: &mut Window, cx: &mut Context<'_, Self>) {
        let network = (chain_id != self.session.chain_id).then_some(chain_id);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.network == network
            || form.receive_to != ReceiveTo::PublicAddress
            || (form.operation.is_some() && !form.reuse_account)
        {
            return;
        }
        form.network = network;
        form.buy = None;
        form.native_output = false;
        form.bridge.chosen = None;
        form.error = None;
        self.load_bridge_routes(window, cx);
        self.bridge_choices_changed(window, cx);
    }

    fn set_form_provider(
        &mut self,
        provider: BridgeProvider,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.network.is_none()
            || (form.operation.is_some() && !form.reuse_account)
            || form.bridge.chosen == Some(provider)
        {
            return;
        }
        let before = form.bridge_terms();
        form.bridge.chosen = Some(provider);
        form.error = None;
        if form.bridge_terms() == before {
            // Picking the provider already in use changes nothing to quote.
            self.refresh_provider_select(window, cx);
            cx.notify();
            return;
        }
        self.bridge_choices_changed(window, cx);
    }

    fn set_form_surplus(
        &mut self,
        surplus: BridgeSurplus,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.bridge.surplus == surplus || self.is_retry(form) {
            return;
        }
        if let Some(form) = self.form.as_mut() {
            form.bridge.surplus = surplus;
        }
        self.delivery_changed(window, cx);
    }

    /// Pay the wrapped native Buy asset to the Public address as the native asset, or as the
    /// wrapped token. The order's buy token changes, so the swap is quoted again.
    fn set_native_output(&mut self, native: bool, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if (form.operation.is_some() && !form.reuse_account)
            || form.native_output == native
            || (native && !self.offers_native_output(form, cx))
        {
            return;
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.native_output = native;
        form.error = None;
        self.schedule_quote(window, cx);
        cx.notify();
    }

    /// Choose where the bought token goes. Only a Public address can receive the native asset,
    /// so any change returns the output to the wrapped token.
    fn set_receive_to(
        &mut self,
        receive_to: ReceiveTo,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if self.receive_to_locked(form) || form.receive_to == receive_to {
            return;
        }
        let own = self.session.chain_id;
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.receive_to = receive_to;
        form.native_output = false;
        form.set_receiver_suggestions((false, None));
        // Private delivery stays on the swap's network, whose tokens the Buy list holds.
        if receive_to == ReceiveTo::PrivateBalance && form.network.take().is_some() {
            form.buy = None;
            form.bridge.chosen = None;
            form.network_select.update(cx, |select, cx| {
                select.set_selected_value(&own, window, cx);
            });
            self.bridge_choices_changed(window, cx);
            return;
        }
        self.delivery_changed(window, cx);
    }

    /// The receiver input changed: search the suggestions, then check and quote the entry.
    fn receiver_edited(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let options = self.receiver_options(cx);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let value = form.receiver_input.read(cx).value().to_string();
        if form.receiver_value == value {
            return;
        }
        form.receiver_value = value;
        let suggestions =
            recipient_suggestions_for_input(RECEIVER_RULES, &options, &form.receiver_value);
        form.set_receiver_suggestions(suggestions);
        self.delivery_changed(window, cx);
    }

    /// Use `receiver`, as a picked suggestion does.
    fn set_form_receiver(
        &mut self,
        receiver: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.set_receiver_suggestions((false, None));
        if form.receiver_value == receiver {
            cx.notify();
            return;
        }
        receiver.clone_into(&mut form.receiver_value);
        form.receiver_input.update(cx, |input, cx| {
            input.set_value(receiver.to_owned(), window, cx);
        });
        // Programmatic input changes don't emit InputEvent::Change.
        self.delivery_changed(window, cx);
    }

    fn receiver_picker_event(
        &mut self,
        event: &RecipientPickerEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let options = self.receiver_options(cx);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let suggestions = match event {
            RecipientPickerEvent::Toggle => recipient_suggestions_toggled(
                RECEIVER_RULES,
                &options,
                &form.receiver_value,
                !form.receiver_suggestions_open,
            ),
            // Presses anywhere outside the picker dismiss it.
            RecipientPickerEvent::Dismiss if !form.receiver_suggestions_open => return,
            RecipientPickerEvent::Dismiss => (false, None),
            RecipientPickerEvent::Move(direction) => {
                let Some(suggestions) = recipient_suggestion_moved(
                    RECEIVER_RULES,
                    &options,
                    &form.receiver_value,
                    form.receiver_suggestion_index,
                    *direction,
                ) else {
                    return;
                };
                suggestions
            }
            RecipientPickerEvent::Select(receiver) => {
                self.set_form_receiver(receiver, window, cx);
                return;
            }
        };
        form.set_receiver_suggestions(suggestions);
        cx.notify();
    }

    /// Enter in the receiver input picks the highlighted suggestion.
    fn confirm_receiver_suggestion(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self
            .form
            .as_ref()
            .filter(|form| form.receiver_suggestions_open)
        else {
            return;
        };
        let Some(receiver) = recipient_suggestion_to_confirm(
            RECEIVER_RULES,
            &self.receiver_options(cx),
            &form.receiver_value,
            form.receiver_suggestion_index,
        ) else {
            return;
        };
        self.set_form_receiver(&receiver, window, cx);
    }

    /// The delivery choice or the receiver changed: check it, then quote again, which clears
    /// the acknowledgements.
    fn delivery_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        self.refresh_form_delivery(cx);
        if let Some(form) = self.form.as_mut() {
            form.error = None;
        }
        self.schedule_quote(window, cx);
        cx.notify();
    }

    fn refresh_form_delivery(&mut self, cx: &App) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let delivery = self.form_delivery(form, cx);
        if let Some(form) = self.form.as_mut() {
            form.delivery = delivery;
        }
    }

    /// The delivery the form's choices and receiver give. The receiver parses as Private
    /// Unshield's recipients do, and mustn't be one of the swap's own addresses. A new swap has
    /// no stealth account to compare until it's reserved; signing checks it then. On another
    /// network, the receiver is checked against that network's contracts, and the Buy token
    /// needs a provider.
    fn form_delivery(&self, form: &SwapForm, cx: &App) -> Result<SwapDelivery, DeliveryProblem> {
        if form.receive_to == ReceiveTo::PrivateBalance {
            return Ok(SwapDelivery::Reshield);
        }
        let receiver_problem = |problem: &'static str| DeliveryProblem::Receiver(problem.into());
        let entered = form.receiver_value.trim();
        if entered.is_empty() {
            return Err(receiver_problem(ENTER_RECEIVER));
        }
        let receiver = parse_address(entered).ok_or_else(|| receiver_problem(INVALID_RECEIVER))?;
        if let Some(network) = form.network {
            return self.bridge_delivery(form, network, receiver, cx);
        }
        let unavailable =
            || receiver_problem("Public address delivery isn't available on this chain.");
        let root = self.root.upgrade().ok_or_else(unavailable)?;
        let chain = root
            .read(cx)
            .effective_chain_configs
            .get(self.session.chain_id)
            .ok_or_else(unavailable)?;
        let profile = chain.swap_profile().ok_or_else(unavailable)?;
        let railgun = chain
            .require_railgun()
            .map_err(|_| unavailable())?
            .deployment
            .contract;
        // The zero address is rejected first, so it stands in for an unknown executor.
        let executor = form
            .operation
            .and_then(|operation| self.record(operation))
            .and_then(ExecutorRecord::address)
            .unwrap_or(Address::ZERO);
        profile
            .check_receiver(railgun, executor, receiver)
            .map_err(|rejection| receiver_problem(receiver_rejection_message(rejection)))?;
        Ok(SwapDelivery::External { receiver })
    }

    /// A Bridge delivery to `receiver` on `network`, which mustn't be one of that network's
    /// protocol contracts, with the provider the Buy token uses.
    fn bridge_delivery(
        &self,
        form: &SwapForm,
        network: u64,
        receiver: Address,
        cx: &App,
    ) -> Result<SwapDelivery, DeliveryProblem> {
        let name = network_name(network);
        let unavailable =
            || DeliveryProblem::Receiver(format!("Bridging to {name} isn't available.").into());
        let root = self.root.upgrade().ok_or_else(unavailable)?;
        let chain = root
            .read(cx)
            .effective_chain_configs
            .get(network)
            .ok_or_else(unavailable)?;
        // A chain without a Railgun deployment has no Railgun contract to reject.
        let railgun = chain
            .railgun
            .as_ref()
            .map_or(Address::ZERO, |railgun| railgun.deployment.contract);
        chain
            .bridge_profile()
            .ok_or_else(unavailable)?
            .check_receiver(railgun, receiver)
            .map_err(|rejection| {
                DeliveryProblem::Receiver(bridge_receiver_message(rejection, &name).into())
            })?;
        let BridgeState::Ready {
            destination,
            provider,
            ..
        } = form.bridge_state()
        else {
            return Err(DeliveryProblem::Bridge);
        };
        Ok(SwapDelivery::Bridge(BridgeDelivery {
            provider,
            destination_chain: network,
            receiver,
            destination_token: destination.destination_token,
            surplus: bridge_surplus(provider, form.bridge.surplus),
        }))
    }

    /// The receiver suggestions, the same as Private Unshield's: the selected wallet's active
    /// public accounts, then the public address book.
    fn receiver_options(&self, cx: &App) -> Vec<RecipientOption> {
        self.root.upgrade().map_or_else(Vec::new, |root| {
            root.read(cx).private_unshield_recipient_options()
        })
    }

    /// The wallet's name for `receiver`: one of its Public accounts, marked `true`, or a public
    /// address-book entry.
    pub(super) fn receiver_label(&self, receiver: Address, cx: &App) -> Option<(String, bool)> {
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        if let Some(account) = root
            .public_accounts
            .iter()
            .find(|account| account.address == receiver)
        {
            let label = public_account_display_label(account)
                .unwrap_or_else(|| railgun_ui::short_address(&account.address));
            return Some((label, true));
        }
        root.public_address_book
            .iter()
            .find(|entry| entry.address == receiver)
            .map(|entry| (entry.label.clone(), false))
    }

    fn set_slippage(&mut self, bps: u32, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.slippage_bps == bps {
            return;
        }
        form.slippage_bps = bps;
        self.schedule_quote(window, cx);
        // The popover lives in the quote details, which only a ready quote shows. Move focus
        // to the amount before they go, so keyboard input and Escape still reach the dialog.
        if self
            .form
            .as_ref()
            .is_some_and(|form| !matches!(form.quote, QuoteState::Ready(_)))
        {
            self.focus_form_amount(window, cx);
        }
        cx.notify();
    }

    fn use_amount(&mut self, amount: U256, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let value = format_unshield_amount_input(amount, self.token_decimals(form.sell, cx));
        let input = form.amount_input.clone();
        input.update(cx, |input, cx| input.set_value(value, window, cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        // Programmatic input changes don't emit InputEvent::Change.
        self.form_inputs_changed(window, cx);
    }

    fn form_primary(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.close_setup_settings(cx);
        let Some(form) = self.form.as_ref() else {
            return;
        };
        match self.form_mode(form) {
            FormMode::Setup { resume } => self.request_setup(resume, window, cx),
            FormMode::Order => {
                // A change still waiting for review is named when the user opens it.
                let change = self
                    .reapproval
                    .filter(|(pending, _)| form.operation == Some(*pending))
                    .map(|(_, change)| change);
                self.request_order_review(change, window, cx);
            }
            FormMode::SettingUp | FormMode::Placed => {}
        }
    }

    /// Resume a quote that waited for the setup's confirmation.
    pub(super) fn continue_form_after_observation(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if self.form_mode(form) == FormMode::Order
            && (matches!(form.quote, QuoteState::Idle | QuoteState::Failed(_))
                || matches!(&form.quote, QuoteState::Ready(review)
                    if review.plan().swap_executor().requires_setup()))
            && form.quote_task.is_none()
        {
            self.schedule_quote(window, cx);
        }
    }

    // Setup route

    fn start_setup_route_updates(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(root) = self.root.upgrade() {
            root.update(cx, |root, _| root.public_broadcaster_anchor_refresh.wake());
        }
        self.refresh_setup_route(cx);
        let task = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL)
                    .await;
                let active = view
                    .update(cx, |view, cx| {
                        if view.form.is_none() || !view.session_is_current(cx) {
                            return false;
                        }
                        // Most ticks change nothing; redrawing the form for them costs a frame.
                        if view.update_setup_route(cx) == Some(true) {
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        });
        if let Some(form) = self.form.as_mut() {
            form.route.refresh_task = Some(task);
        }
    }

    pub(super) fn setup_fee_route(
        &self,
        preferred: Address,
        current: Option<Address>,
        allow_out_of_range: bool,
        favorites_only: bool,
        cx: &App,
    ) -> (
        Vec<PublicBroadcasterFeeTokenOption>,
        Option<Address>,
        Vec<PublicBroadcasterCandidate>,
    ) {
        let Some(root_entity) = self.root.upgrade() else {
            return (Vec::new(), None, Vec::new());
        };
        let root = root_entity.read(cx);
        let chain_id = self.session.chain_id;
        let Some(profile) = root
            .effective_chain_configs
            .get(chain_id)
            .and_then(EffectiveChainConfig::accepted_executor_profile)
        else {
            return (Vec::new(), None, Vec::new());
        };
        let policy = root.public_broadcaster_fee_policy(allow_out_of_range);
        let trust = root.public_broadcaster_trust_filter(favorites_only);
        let rows = root.monitor_fee_rows();
        let options = root
            .chain_states
            .get(&chain_id)
            .and_then(|state| state.snapshot())
            .map(|snapshot| {
                public_broadcaster_fee_token_options_from_snapshot(
                    snapshot,
                    &rows,
                    None,
                    Some(profile),
                    policy,
                    &trust,
                    Some(&root.effective_token_registry),
                    |token| {
                        root.public_broadcaster_anchor_cache
                            .cached_rate(chain_id, token)
                    },
                )
            })
            .unwrap_or_default();
        let token = (!options.is_empty()).then(|| {
            resolve_selected_public_broadcaster_fee_token(
                current.unwrap_or(preferred),
                preferred,
                &options,
            )
        });
        let candidates = token
            .map(|token| self.broadcaster_candidates(token, allow_out_of_range, favorites_only, cx))
            .unwrap_or_default();
        (options, token, candidates)
    }

    fn refresh_setup_route(&mut self, cx: &mut Context<'_, Self>) {
        if self.update_setup_route(cx).is_some() {
            cx.notify();
        }
    }

    /// Read the setup route again. `None` when the form has no setup route to update;
    /// otherwise whether anything the form or the broadcaster picker shows changed.
    fn update_setup_route(&mut self, cx: &mut Context<'_, Self>) -> Option<bool> {
        let form = self.form.as_ref()?;
        if !matches!(self.form_mode(form), FormMode::Setup { .. }) || self.busy() {
            return None;
        }
        let (options, token, candidates) = self.setup_fee_route(
            form.sell,
            form.route.fee_token,
            form.route.allow_out_of_range,
            form.route.favorites_only,
            cx,
        );
        let form = self.form.as_mut()?;
        let route = &mut form.route;
        let quote_changed = route.estimate_candidate.as_ref().is_some_and(|quoted| {
            !candidates
                .iter()
                .any(|candidate| same_offer(quoted, candidate))
        });
        let token_changed = route.fee_token != token;
        let choice_changed = route.selected.as_ref().is_some_and(|address| {
            !candidates
                .iter()
                .any(|candidate| &candidate.railgun_address == address)
        });
        let shown_changed =
            route.fee_options != options || !same_shown_candidates(&route.candidates, &candidates);
        if choice_changed {
            route.selected = None;
        }
        route.fee_options = options;
        route.fee_token = token;
        route.candidates = candidates;
        if quote_changed || token_changed || choice_changed {
            route.invalidate_estimate();
        }
        let due = route.estimate_task.is_none()
            && (route.estimate.is_none() && route.estimate_error.is_none()
                || route
                    .next_estimate
                    .is_some_and(|next| Instant::now() >= next));
        if due {
            self.schedule_setup_estimate(cx);
        }
        Some(quote_changed || token_changed || choice_changed || shown_changed || due)
    }

    fn schedule_setup_estimate(&mut self, cx: &mut Context<'_, Self>) {
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.route.candidates.is_empty() {
            return;
        }
        let selection =
            form.route
                .selected
                .as_ref()
                .map_or(PublicBroadcasterSelection::Random, |address| {
                    PublicBroadcasterSelection::Specific {
                        railgun_address: address.clone(),
                    }
                });
        let candidate = {
            let root = root.read(cx);
            wallet_ops::select_public_broadcaster_with_policy_and_trust(
                &form.route.candidates,
                &selection,
                root.public_broadcaster_fee_policy(form.route.allow_out_of_range),
                &root.public_broadcaster_trust_filter(form.route.favorites_only),
            )
        };
        let Ok(candidate) = candidate else {
            return;
        };
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        let runtime = self.runtime.clone();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let operation = form.operation;
        let route = &mut form.route;
        route.estimate_revision = route.estimate_revision.wrapping_add(1);
        let revision = route.estimate_revision;
        route.estimate_candidate = Some(candidate.clone());
        route.estimate = None;
        route.estimate_error = None;
        route.estimate_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(COST_ESTIMATE_DEBOUNCE).await;
            let result = runtime
                .spawn(async move {
                    Box::pin(owner.estimate_swap_setup_fee(&session, operation, candidate)).await
                })
                .await;
            let result = result
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
            let _ = view.update(cx, |view, cx| {
                let Some(form) = view.form.as_mut() else {
                    return;
                };
                let route = &mut form.route;
                if route.estimate_revision != revision {
                    return;
                }
                route.estimate_task = None;
                route.next_estimate = Some(
                    Instant::now()
                        + if result.is_ok() {
                            Duration::from_secs(30)
                        } else {
                            Duration::from_secs(5)
                        },
                );
                match result {
                    Ok(estimate) => route.estimate = Some(estimate),
                    Err(error) => route.estimate_error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The broadcaster picker's view of the setup route.
    pub(in crate::root) fn setup_picker_context(&self) -> Option<RecoveryPickerContext> {
        let form = self.form.as_ref()?;
        Some(RecoveryPickerContext {
            chain_id: self.session.chain_id,
            token: form.route.fee_token?,
            choice: form.route.choice(),
            candidates: form.route.candidates.clone(),
            allow_out_of_range: form.route.allow_out_of_range,
            favorites_only: form.route.favorites_only,
            busy: self.busy(),
            estimating: form.route.estimate_task.is_some(),
            fee_context: form
                .route
                .estimate
                .as_ref()
                .map(BroadcasterPickerFeeEstimateContext::from),
        })
    }

    pub(in crate::root) fn set_setup_allow_out_of_range(
        &mut self,
        checked: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.route.allow_out_of_range = checked;
        form.route.invalidate_estimate();
        self.refresh_setup_route(cx);
    }

    pub(in crate::root) fn choose_setup_broadcaster(
        &mut self,
        address: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() || !self.session_is_current(cx) {
            return;
        }
        self.refresh_setup_route(cx);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if !form
            .route
            .candidates
            .iter()
            .any(|candidate| candidate.railgun_address == address)
        {
            return;
        }
        form.route.selected = Some(address);
        form.route.invalidate_estimate();
        let input = form.amount_input.clone();
        self.schedule_setup_estimate(cx);
        input.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    // Review, setup and submission

    fn request_setup(&mut self, resume: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.busy() {
            return;
        }
        let waku = self.broadcaster_network(cx);
        let (approval, review) = match self.setup_approval(resume, waku) {
            Ok(approved) => approved,
            Err(error) => {
                if let Some(form) = self.form.as_mut() {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        let summary = self
            .swap_summary(&review, Some(&approval), None, cx)
            .requiring_explicit_review();
        self.request_authorization(SwapAction::Setup(Box::new(approval)), summary, window, cx);
    }

    /// The swap as reviewed: the quoted terms with the suggested minimum, and the setup's
    /// broadcaster route.
    fn setup_approval(
        &self,
        resume: bool,
        waku: Option<Arc<WakuDeliveryClient>>,
    ) -> Result<(SetupApproval, Arc<SwapReview>), String> {
        let form = self.form.as_ref().ok_or("The swap form closed.")?;
        let buy = form.quote_buy().ok_or("Choose the token to receive.")?;
        let review = match &form.quote {
            QuoteState::Ready(review)
                if review.plan().sell_token() == form.sell && review.plan().buy_token() == buy =>
            {
                Arc::clone(review)
            }
            _ => return Err("Wait for the quote.".into()),
        };
        if let Some(problem) = form.review_problem(&review) {
            return Err(problem.to_string());
        }
        let estimate = form
            .route
            .estimate
            .as_ref()
            .ok_or("Wait for the setup fee estimate.")?;
        let candidate = estimate.broadcaster();
        if !form
            .route
            .candidates
            .iter()
            .any(|current| same_offer(current, candidate))
        {
            return Err("The broadcaster quote changed. Wait for a new estimate.".into());
        }
        let waku = waku.ok_or("Wait for the broadcaster network connection, then try again.")?;
        let approval = review
            .approval(review.suggested_private_minimum(), form.price_acknowledged)
            .map_err(|error| format!("{error:#}"))?;
        let operation = match form.operation {
            Some(operation) => operation,
            None => ExecutorOperationId::random().map_err(|error| error.to_string())?,
        };
        Ok((
            SetupApproval {
                operation,
                resume,
                sell: form.sell,
                buy,
                candidate: candidate.clone(),
                maximum_private_fee: default_public_broadcaster_fee_limit(estimate.fee_amount()),
                waku,
                approval,
                orderbook: form.orderbook.clone(),
            },
            review,
        ))
    }

    /// Reserve the stealth account, check that the approved amount still fits one order,
    /// persist the approval, and hand the setup to the broadcaster. Nothing is paid when the
    /// amount no longer fits. The order follows once the setup is confirmed.
    pub(super) fn submit_setup(
        &mut self,
        approval: SetupApproval,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let operation = approval.operation;
        let (progress, receiver) =
            tokio::sync::watch::channel(TransactionGenerationStage::SelectingPrivateNotes);
        let mut changes = receiver.clone();
        let watch = cx.spawn(async move |view, cx| {
            while changes.changed().await.is_ok() {
                if view.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let bounds = &approval.approval.bounds;
        let tracking = self.tracking.entry(operation).or_default();
        tracking.amount = Some(bounds.spend_amount());
        tracking.slippage_bps = Some(bounds.slippage_bps);
        if approval.orderbook.is_some() {
            tracking.orderbook.clone_from(&approval.orderbook);
        }
        tracking.setup = None;
        tracking.cursor = None;
        tracking.setup_read_at = None;
        tracking.located_at = None;
        tracking.error = None;
        tracking.setup_stage = Some(receiver);
        tracking.setup_watch = Some(watch);
        // The order was approved with the setup: place it as soon as the setup is confirmed.
        tracking.auto_place = true;
        let byte_budget = tracking.byte_budget;
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        self.start_job(
            operation,
            SwapJobKind::Setup,
            async move {
                let prepared = if approval.resume {
                    Box::pin(owner.resume_swap_setup(operation, approval.candidate, &authorization))
                        .await?
                } else {
                    // The new record holds the approval from its first write.
                    Box::pin(owner.prepare_swap_setup(
                        operation,
                        approval.candidate,
                        approval.approval.clone(),
                        &authorization,
                    ))
                    .await?
                };
                let reserved = SwapExecutor::reserved(&prepared)?;
                let request = SwapAmountRequest {
                    sell_token: approval.sell,
                    buy_token: approval.buy,
                    amount: approval.approval.bounds.spend_amount(),
                    delivery: approval.approval.delivery,
                    byte_budget,
                };
                let planning_owner = Arc::clone(&owner);
                let planning_session = Arc::clone(&session);
                let plan = tokio::task::spawn_blocking(move || {
                    planning_owner.plan_swap_amount(&reserved, &planning_session, &request)
                })
                .await
                .map_err(|_| eyre::eyre!("Planning the swap stopped unexpectedly."))??;
                if matches!(plan, SwapAmountPlan::TooLarge { .. }) {
                    return Ok(SetupOutcome::TooLarge);
                }
                // A resumed setup's record keeps its earlier approval until the user authorized
                // this review.
                if approval.resume {
                    owner.record_swap_approval(operation, approval.approval)?;
                }
                let outcome = Box::pin(owner.submit_swap_setup(
                    &prepared,
                    SwapSetupRequest {
                        maximum_private_fee: approval.maximum_private_fee,
                        session,
                        authorization,
                        waku: approval.waku,
                        verify_proof: true,
                        progress_tx: Some(progress),
                        response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                        republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                    },
                ))
                .await?;
                Ok(SetupOutcome::Sent(outcome.result))
            },
            move |this, outcome, window, cx| {
                let tracking = this.tracking.entry(operation).or_default();
                tracking.setup_stage = None;
                tracking.setup_watch = None;
                match outcome {
                    SetupOutcome::Sent(result) => {
                        tracking.error = broadcaster_result_problem(&result, "setup");
                    }
                    SetupOutcome::TooLarge => {
                        tracking.auto_place = false;
                        // The form offers the largest amount that fits, for a new review.
                        if !window.has_active_dialog(cx)
                            || this.detail_is_active(operation, window, cx)
                        {
                            this.open_existing_form(operation, window, cx);
                        }
                        this.fail(operation, "Your notes changed after the quote, and the approved amount no longer fits one swap. Nothing was paid.".into());
                    }
                }
                cx.notify();
            },
            window,
            cx,
        );
        // The detail follows the setup and then the order it places.
        self.show_detail(operation, window, cx);
    }

    // Quote and order

    fn retry_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // An explicit retry gets a new isolated route instead of reusing a failed circuit.
        form.set_orderbook(None, None);
        if let Some(tracking) = form
            .operation
            .and_then(|operation| self.tracking.get_mut(&operation))
        {
            tracking.orderbook = None;
        }
        self.schedule_quote(window, cx);
    }

    fn schedule_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.price_acknowledged = false;
            form.high_costs_acknowledged = false;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let operation = form.operation;
        let mode = self.form_mode(form);
        if !matches!(mode, FormMode::Setup { .. } | FormMode::Order) {
            return;
        }
        let (sell, buy, slippage_bps) = (form.sell, form.quote_buy(), form.slippage_bps);
        let amount = self.form_amount(form, cx).ok();
        let registries = self.root.upgrade().map(|root| {
            let root = root.read(cx);
            (
                Arc::clone(&root.public_broadcaster_anchor_cache),
                root.effective_token_registry.clone(),
            )
        });
        let bridge = match form.bridge_state() {
            BridgeState::Ready { destination, .. } => self
                .destination_chain(form, cx)
                .zip(self.origin_bridge_profile(cx))
                .map(|(destination_chain, origin)| QuoteBridgeRequest {
                    destination: Some(destination.clone()),
                    destination_chain,
                    origin,
                }),
            _ => None,
        };
        // Before setup the quote uses a stand-in. Set-up accounts use local observations;
        // execution preparation refreshes the account before proving and signing.
        let executor = match (mode, operation) {
            (FormMode::Setup { resume: true }, Some(operation)) => QuoteExecutor::Setup(operation),
            (FormMode::Order, Some(operation)) => QuoteExecutor::Order {
                operation,
                reuse: form.reuse_account,
            },
            _ => QuoteExecutor::Preview,
        };
        let terms = buy.zip(amount).map(|(buy, amount)| QuoteTerms {
            executor,
            sell,
            buy,
            amount,
            slippage_bps,
            receive_to: form.receive_to,
            bridge: form.bridge_terms(),
        });
        let (Some(terms), Some((anchor_cache, tokens)), Ok(delivery)) =
            (terms, registries, form.delivery.clone())
        else {
            // Only a receiver that can be used is quoted. Until then, a quote of the same terms
            // for another receiver stays in view, as its numbers don't depend on the receiver.
            let keep = terms.is_some()
                && form.quote_terms == terms
                && !matches!(form.quote, QuoteState::Idle | QuoteState::Failed(_));
            if let Some(form) = self.form.as_mut()
                && !keep
            {
                form.quote = QuoteState::Idle;
                form.quote_task = None;
                form.quote_terms = None;
                form.receive_help_open = false;
            }
            cx.notify();
            return;
        };
        let tracking = operation.map(|operation| self.tracking.entry(operation).or_default());
        let byte_budget = tracking.as_ref().and_then(|tracking| tracking.byte_budget);
        let tracked_orderbook = tracking.and_then(|tracking| {
            tracking.amount = Some(terms.amount);
            tracking.slippage_bps = Some(slippage_bps);
            tracking.orderbook.clone()
        });
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let request = QuoteRequest {
            executor,
            sell,
            buy: terms.buy,
            amount: terms.amount,
            delivery,
            slippage_bps,
            byte_budget,
            orderbook: form.orderbook.clone().or(tracked_orderbook),
            bridge_clients: form
                .orderbook
                .as_ref()
                .and_then(|_| form.bridge_clients.clone()),
            bridge,
            // A fresh signing-time failure must reach the acknowledgement even if the
            // background cache still contains an older rate.
            anchor_cache: if operation.is_some_and(|operation| {
                self.reapproval == Some((operation, SwapReviewChange::PriceUnavailable))
            }) {
                None
            } else {
                Some(anchor_cache)
            },
            tokens,
        };
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        let runtime = self.runtime.clone();
        form.quote_revision = form.quote_revision.wrapping_add(1);
        let revision = form.quote_revision;
        form.quote_terms = Some(terms);
        form.quote = QuoteState::Loading;
        form.receive_help_open = false;
        form.quote_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(QUOTE_DEBOUNCE).await;
            let result = runtime.spawn(quote_swap(owner, session, request)).await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_quote(operation, revision, result.ok(), window, cx);
            });
        }));
        cx.notify();
    }

    fn apply_quote(
        &mut self,
        operation: Option<ExecutorOperationId>,
        revision: u64,
        result: Option<QuoteResult>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.operation != operation || form.quote_revision != revision {
            return;
        }
        form.quote_task = None;
        form.high_costs_acknowledged = false;
        let Some(result) = result else {
            form.quote =
                QuoteState::Failed(eyre::eyre!("Quoting stopped unexpectedly. Try again."));
            cx.notify();
            return;
        };
        if let Some(orderbook) = result.orderbook {
            form.set_orderbook(Some(orderbook.clone()), result.bridge_clients);
            if let Some(operation) = operation {
                self.tracking.entry(operation).or_default().orderbook = Some(orderbook);
            }
        }
        form.quoted_bridge = result.bridge;
        form.quote = match result.outcome {
            Ok(QuoteOutcome::TooLarge(largest)) => QuoteState::TooLarge { largest },
            Ok(QuoteOutcome::Review(review)) => {
                if review.price_verified() {
                    form.price_acknowledged = false;
                }
                QuoteState::Ready(Arc::from(review))
            }
            Ok(QuoteOutcome::PriceBlocked(block)) => QuoteState::PriceBlocked(block),
            Err(error) => QuoteState::Failed(error),
        };
        // Only the swap's own form, in front, reopens the review. Otherwise the change stays
        // pending, and the form's Review… names it.
        let reopen = matches!(&form.quote, QuoteState::Ready(review)
            if form.review_problem(review).is_none())
            && operation.is_some_and(|operation| {
                self.reapproval
                    .is_some_and(|(pending, _)| pending == operation)
                    && self.swap_dialog_shows(operation, window, cx)
            });
        // Setup can confirm while its retry quote is in flight. Replace that setup preview
        // with an order quote before offering approval.
        if self.form.as_ref().is_some_and(|form| {
            self.form_mode(form) == FormMode::Order
                && matches!(&form.quote, QuoteState::Ready(review)
                    if review.plan().swap_executor().requires_setup())
        }) {
            self.schedule_quote(window, cx);
            return;
        }
        if reopen && let Some((_, change)) = self.reapproval {
            self.request_order_review(Some(change), window, cx);
        }
        cx.notify();
    }

    fn request_order_review(
        &mut self,
        change: Option<SwapReviewChange>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(operation) = form.operation else {
            return;
        };
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let orderbook = form.orderbook.clone().or_else(|| {
            self.tracking
                .get(&operation)
                .and_then(|tracking| tracking.orderbook.clone())
        });
        let problem = form.review_problem(review).or_else(|| {
            orderbook
                .is_none()
                .then_some("The swap's orderbook route isn't ready. Refresh the quote.".into())
        });
        let (Some(orderbook), None) = (orderbook, &problem) else {
            if let Some(form) = self.form.as_mut() {
                form.error = problem.map(|problem| problem.to_string());
            }
            cx.notify();
            return;
        };
        // A first order is bound to the approval saved with its setup, so the review names what
        // differs from it, such as another delivery, rather than the change that reopened the
        // form.
        let change = self.first_order_change(operation, review).or(change);
        let approval = OrderApproval {
            operation,
            review: Arc::clone(review),
            private_minimum: review.suggested_private_minimum(),
            price_acknowledged: form.price_acknowledged,
            orderbook,
            bridge: form.quoted_bridge.clone(),
            destination_minimum: review.bridge().map(|bridge| bridge.destination_minimum),
            full_review: true,
        };
        let summary = self
            .swap_summary(&approval.review, None, change, cx)
            .requiring_explicit_review();
        // The review about to open names the pending change.
        if self
            .reapproval
            .is_some_and(|(pending, _)| pending == operation)
        {
            self.reapproval = None;
        }
        self.request_authorization(SwapAction::Order(Box::new(approval)), summary, window, cx);
    }

    /// How `review` differs from the approval saved with the swap's setup, while that approval
    /// still binds the first order.
    fn first_order_change(
        &self,
        operation: ExecutorOperationId,
        review: &SwapReview,
    ) -> Option<SwapReviewChange> {
        self.record(operation)
            .filter(|record| record.swap().is_none())
            .and_then(ExecutorRecord::swap_approval)
            .and_then(|approval| review.approval_change(approval))
    }

    /// The single review of a swap. With `setup`, a new swap's setup and its order, placed once
    /// the setup is confirmed; without, an order for a stealth account that is set up.
    fn swap_summary(
        &self,
        review: &SwapReview,
        setup: Option<&SetupApproval>,
        change: Option<SwapReviewChange>,
        cx: &App,
    ) -> SpendAuthorizationSummary {
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let sell_amount = self.token_amount(sell, plan.amount(), cx);
        let valid_for = self
            .swap_profile(cx)
            .map_or(10, |profile| profile.valid_to_window().as_secs() / 60);
        let slippage = format_bps_percent(u64::from(review.slippage_bps()));
        let mut rows = Vec::new();
        if let Some(setup) = setup {
            let root = self.root.upgrade();
            let maximum_fee = format_token_amount_ceiling_for_display(
                self.session.chain_id,
                setup.candidate.token,
                setup.maximum_private_fee,
                root.as_ref()
                    .map(|root| &root.read(cx).effective_token_registry),
            );
            rows.push(
                SpendAuthorizationSummaryRow::new(
                    "Pay now",
                    format!("Up to {maximum_fee} setup fee"),
                )
                .with_icon(self.token_icon(setup.candidate.token, cx))
                .with_note(format!(
                    "Via broadcaster {}. Not refunded if the order doesn't fill.",
                    broadcaster_candidate_label(&setup.candidate)
                )),
            );
        }
        // A Bridge order buys exactly this and deposits it with its provider.
        let minimum = self.token_amount(buy, review.suggested_private_minimum(), cx);
        let bridge = match plan.delivery() {
            SwapDelivery::Bridge(delivery) => Some(delivery),
            _ => None,
        };
        rows.push(match bridge.zip(review.bridge()) {
            Some((delivery, quote)) => self.bridge_receive_row(
                delivery,
                quote.expected_output,
                quote.destination_minimum,
                cx,
            ),
            None => SpendAuthorizationSummaryRow::new(
                "You receive",
                format!("≈ {}", self.token_amount(buy, expected_output(review), cx)),
            )
            .with_icon(self.token_icon(buy, cx))
            .with_note(format!("At least {minimum}")),
        });
        if let Some(surplus) = review.estimated_source_surplus() {
            rows.push(
                SpendAuthorizationSummaryRow::new(
                    format!("Estimated return on {}", self.chain_label()),
                    self.with_usd(
                        format!("≈ {}", self.token_amount(buy, surplus, cx)),
                        buy,
                        surplus,
                        cx,
                    ),
                )
                .with_note(source_return_note(review)),
            );
            if let Some(total) = self.bridge_total_usd_value(review, cx) {
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Estimated total received",
                    format!("≈ {}", railgun_ui::format_usd_micro_value(total)),
                ));
            }
        }
        let (delivery_rows, own_account_warning) =
            self.delivery_rows(plan.delivery(), buy, Some(review), cx);
        rows.extend(delivery_rows);
        if plan.swap_executor().is_reused() {
            rows.push(
                SpendAuthorizationSummaryRow::new(
                    "Stealth account",
                    plan.executor().to_checksum(None),
                )
                .with_shortened_copyable(),
            );
        }
        let isolated = review.isolation() == OperationNetworkIsolation::Dedicated;
        if let OperationNetworkIsolation::Unavailable(mode) = review.isolation() {
            rows.push(SpendAuthorizationSummaryRow::new(
                "Network route",
                format!("Not isolated in {mode} mode"),
            ));
        }
        let shields = matches!(
            plan.delivery(),
            SwapDelivery::Reshield
                | SwapDelivery::Bridge(BridgeDelivery {
                    surplus: BridgeSurplus::Reshield,
                    ..
                })
        );
        let mut details = vec![
            ("Slippage", slippage.clone()),
            (
                "CoW network fee",
                format!(
                    "{}, in the quote",
                    self.token_amount(buy, cow_fee(review), cx)
                ),
            ),
            (
                "Hook gas",
                format!(
                    "≈ {}; allowance {}",
                    self.token_amount(buy, review.estimated_hook_cost(), cx),
                    self.token_amount(buy, review.hook_cost(), cx)
                ),
            ),
            (
                if shields {
                    "Railgun fees"
                } else {
                    "Railgun fee"
                },
                railgun_fees_label(review),
            ),
        ];
        if let Some(delivery) = bridge {
            details.push((
                match delivery.provider {
                    BridgeProvider::Across => "Deposit to Across",
                    BridgeProvider::NearIntents => "Deposit to 1Click",
                },
                format!("{minimum} on {}", self.chain_label()),
            ));
        }
        details.push((
            "Order valid for",
            if setup.is_some() {
                format!("{valid_for} minutes after setup")
            } else {
                format!("{valid_for} minutes")
            },
        ));
        let mut context = "Placing the order publishes its tokens, amounts, price limit, and hook data, including the notes it spends, even if it never fills. A later spend of those notes can be linked to this swap.".to_owned();
        match plan.delivery() {
            SwapDelivery::Reshield => {}
            SwapDelivery::External { .. } => {
                context.push('\n');
                context.push_str(EXTERNAL_DELIVERY_DISCLOSURE);
            }
            SwapDelivery::Bridge(delivery) => {
                context.push('\n');
                context.push_str(&self.bridge_disclosure(delivery));
            }
        }
        if !isolated {
            context.push_str(
                "\nThis network mode can't give the swap its own network route, so its orderbook requests aren't isolated from your other wallet traffic.",
            );
        }
        let (title, confirm_label) = if setup.is_some() {
            ("Set up stealth account and swap", "Create stealth account")
        } else {
            ("Private swap", "Swap")
        };
        let summary = SpendAuthorizationSummary::new(title, "", rows)
            .with_title_chip(self.chain_label())
            .with_asset_pair(
                SpendAuthorizationAsset::new(sell_amount, self.token_icon(sell, cx)),
                self.review_buy_asset(plan.delivery(), buy, cx),
            )
            .with_details(
                "Order terms",
                format!("{slippage} slippage · {valid_for} min"),
                details,
                Some(
                    "If someone triggers the swap's unshield and the order doesn't fill, recovering the tokens costs the unshield and shield fees.",
                ),
            )
            .with_info_context("What becomes public", context)
            .with_confirm_label(confirm_label);
        let summary = if setup.is_some() {
            summary
                .with_progress(
                    1,
                    2,
                    "Step 1 of 2 · you confirm the order once setup completes",
                )
                .with_once_lifetime_note("You'll enter the password again to place the order.")
        } else {
            summary
        };
        let mut warnings = Vec::new();
        if !review.price_verified() {
            warnings.push(Arc::from(UNVERIFIED_PRICE_WARNING));
        }
        if let Some(bps) = high_cost_bps(review) {
            warnings.push(Arc::from(high_cost_message(bps)));
        }
        if let Some(warning) = own_account_warning {
            warnings.push(Arc::from(warning));
        }
        if let Some(delivery) = bridge {
            warnings.push(Arc::from(match delivery.provider {
                BridgeProvider::Across => format!(
                    "If the deposit isn't filled before it expires, Across refunds the {minimum} to the stealth account on {}, usually within a few hours. Recovering it to your private balance costs a shield fee and a broadcaster fee.",
                    self.chain_label()
                ),
                BridgeProvider::NearIntents => NEAR_INTENTS_DISCLAIMER.to_owned(),
            }));
        }
        if plan.swap_executor().is_reused() {
            warnings.push(Arc::from(ACCOUNT_REUSE_NOTE));
        }
        if let Some(change) = change {
            warnings.push(Arc::from(format!(
                "The terms changed since your last review: {}. Check them before you approve.",
                review_change_label(change)
            )));
        }
        summary.with_warnings(warnings)
    }

    /// The review's Receiver row for External or Bridge delivery to `receiver`, with its full
    /// address to check, and a warning when it's one of the wallet's own Public accounts, which
    /// the swap links to a Railgun spend of the `received` token.
    fn external_receiver_review(
        &self,
        receiver: Address,
        received: &str,
        cx: &App,
    ) -> (SpendAuthorizationSummaryRow, Option<String>) {
        let row = SpendAuthorizationSummaryRow::new("Receiver", receiver.to_checksum(None));
        match self.receiver_label(receiver, cx) {
            Some((label, true)) => (
                row.with_full_address(Some(format!("{label} · your Public account"))),
                Some(format!(
                    "{label} becomes publicly linked to this swap: anyone can see that it received {received} from a Railgun spend."
                )),
            ),
            Some((label, false)) => (row.with_full_address(Some(label)), None),
            None => (
                row.with_full_address(None).with_note("Not a saved address"),
                None,
            ),
        }
    }

    /// The rows after You receive that say where `delivery` pays out: an External swap's
    /// Receiver, or a Bridge swap's Destination, Receiver and provider terms, with the full
    /// review's notes and Bridge fee when given its `review`. Also the Receiver's warning.
    fn delivery_rows(
        &self,
        delivery: SwapDelivery,
        buy: Address,
        review: Option<&SwapReview>,
        cx: &App,
    ) -> (Vec<SpendAuthorizationSummaryRow>, Option<String>) {
        match delivery {
            SwapDelivery::Reshield => (Vec::new(), None),
            SwapDelivery::External { receiver } => {
                let (row, warning) =
                    self.external_receiver_review(receiver, &self.token_symbol(buy, cx), cx);
                (vec![row], warning)
            }
            SwapDelivery::Bridge(delivery) => {
                let network = delivery.destination_chain;
                let received = self.network_token_symbol(
                    network,
                    self.bridge_received_token(delivery, cx),
                    cx,
                );
                let (receiver, warning) =
                    self.external_receiver_review(delivery.receiver, &received, cx);
                let mut rows = vec![
                    SpendAuthorizationSummaryRow::new("Destination", network_name(network))
                        .with_icon(
                            railgun_ui::chain_icon_asset_path(network)
                                .map(crate::assets::WalletIconSource::embedded),
                        ),
                    receiver,
                ];
                rows.extend(self.bridge_term_rows(delivery, review, cx));
                (rows, warning)
            }
        }
    }

    /// A Bridge swap's You receive row, in its network's token. Across delivers exactly the
    /// `minimum`; 1Click about `expected`, and at least the `minimum`.
    fn bridge_receive_row(
        &self,
        delivery: BridgeDelivery,
        expected: U256,
        minimum: U256,
        cx: &App,
    ) -> SpendAuthorizationSummaryRow {
        let (network, token) = (
            delivery.destination_chain,
            self.bridge_received_token(delivery, cx),
        );
        let on_network = |amount| {
            format!(
                "{} on {}",
                self.network_token_amount(network, token, amount, cx),
                network_name(network)
            )
        };
        let row = match delivery.provider {
            BridgeProvider::Across => SpendAuthorizationSummaryRow::new(
                "You receive",
                on_network(minimum),
            )
            .with_note(match delivery.surplus {
                BridgeSurplus::KeepInAccount => format!(
                    "Exactly this amount. CoW's surplus stays in the stealth account on {}.",
                    self.chain_label()
                ),
                _ => format!(
                    "Exactly this amount. CoW's surplus is reshielded on {}.",
                    self.chain_label()
                ),
            }),
            BridgeProvider::NearIntents => SpendAuthorizationSummaryRow::new(
                "You receive",
                format!("≈ {}", on_network(expected)),
            )
            .with_note(format!(
                "At least {}",
                self.network_token_amount(network, token, minimum, cx)
            )),
        };
        row.with_icon(
            self.chain_token_metadata(network, token, cx)
                .and_then(|metadata| metadata.icon_path),
        )
    }

    /// A Bridge swap's Provider row, its Bridge fee in the full `review`, and Across's Surplus
    /// choice. The confirm-only step shows no fee, since the minimum it leaves was approved.
    fn bridge_term_rows(
        &self,
        delivery: BridgeDelivery,
        review: Option<&SwapReview>,
        cx: &App,
    ) -> Vec<SpendAuthorizationSummaryRow> {
        let chain = self.chain_label();
        let provider =
            SpendAuthorizationSummaryRow::new("Provider", provider_name(delivery.provider));
        let (provider, fee_row, deposit) = match review {
            None => (provider, None, None),
            Some(review) => {
                let bought = review.plan().buy_token();
                // A Bridge order buys exactly the deposit, the quote's minimum.
                let deposit = review.suggested_private_minimum();
                let fee = review.bridge().and_then(|bridge| bridge.fee);
                let (note, fee_row) = match delivery.provider {
                    BridgeProvider::Across => (
                        "Relayers usually fill within a few minutes.".to_owned(),
                        fee.map(|fee| {
                            SpendAuthorizationSummaryRow::new(
                                "Bridge fee",
                                self.token_amount(bought, fee, cx),
                            )
                            .with_note(
                                "Across relayer and LP fees, already out of the amount above",
                            )
                        }),
                    ),
                    BridgeProvider::NearIntents => (
                        format!(
                            "Via {} on {chain}. The whole payout is converted, surplus included.",
                            self.token_symbol(bought, cx)
                        ),
                        Some(match fee {
                            // The fee's share of the deposit it was taken from.
                            Some(fee) => SpendAuthorizationSummaryRow::new(
                                "Bridge fee",
                                format!("≈ {}", self.token_amount(bought, fee, cx)),
                            )
                            .with_note(format!(
                                "1Click fee, about {}, already in the minimum",
                                format_bps_percent(swap_cost_bps(fee, deposit.saturating_sub(fee)))
                            )),
                            // A leg between different assets without anchors has no value for
                            // its fee.
                            None => SpendAuthorizationSummaryRow::new(
                                "Bridge fee",
                                "Included in the minimum",
                            )
                            .with_note("1Click fee. There's no independent price to value it."),
                        }),
                    ),
                };
                (
                    provider.with_note(note),
                    fee_row,
                    Some(self.token_amount(bought, deposit, cx)),
                )
            }
        };
        let mut rows = vec![provider];
        rows.extend(fee_row);
        let surplus = match delivery.surplus {
            BridgeSurplus::Reshield => {
                Some((format!("Reshield on {chain}"), ", less the shield fee"))
            }
            BridgeSurplus::KeepInAccount => Some(("Keep in stealth account".to_owned(), "")),
            BridgeSurplus::BridgedByProvider => None,
        };
        if let Some((choice, cost)) = surplus {
            let row = SpendAuthorizationSummaryRow::new("Surplus", choice);
            rows.push(match deposit {
                Some(deposit) => {
                    row.with_note(format!("Anything above the {deposit} deposit{cost}"))
                }
                None => row,
            });
        }
        rows
    }

    /// What a Bridge swap makes public across chains, for What becomes public.
    fn bridge_disclosure(&self, delivery: BridgeDelivery) -> String {
        let network = network_name(delivery.destination_chain);
        match delivery.provider {
            BridgeProvider::Across => format!(
                "The settlement on {} unshields from Railgun and makes the Across deposit in one transaction. The deposit names the receiver, {network} and the amounts, so the receiver's funds on {network} can be traced to this swap.",
                self.chain_label()
            ),
            BridgeProvider::NearIntents => format!(
                "The order names a 1Click deposit address, and the settlement pays it in the same transaction that unshields from Railgun. 1Click learns the receiver when the order is signed, so the receiver's funds on {network} can be traced to this swap."
            ),
        }
    }

    /// The review's bought asset: for a Bridge swap, the token its network receives.
    fn review_buy_asset(
        &self,
        delivery: SwapDelivery,
        buy: Address,
        cx: &App,
    ) -> SpendAuthorizationAsset {
        match delivery {
            SwapDelivery::Bridge(delivery) => {
                let (network, token) = (
                    delivery.destination_chain,
                    self.bridge_received_token(delivery, cx),
                );
                SpendAuthorizationAsset::new(
                    format!(
                        "{} on {}",
                        self.network_token_symbol(network, token, cx),
                        network_name(network)
                    ),
                    self.chain_token_metadata(network, token, cx)
                        .and_then(|metadata| metadata.icon_path),
                )
            }
            _ => SpendAuthorizationAsset::new(self.token_symbol(buy, cx), self.token_icon(buy, cx)),
        }
    }

    /// Place the order approved with the setup once the setup is confirmed. The approved amount
    /// is planned and quoted again first; unchanged terms need only a confirm-only step, which a
    /// remembered spend authorization satisfies without a prompt.
    pub(super) fn place_approved_order(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() {
            return;
        }
        let Some(record) = self.record(operation) else {
            return;
        };
        if self.stage(record) != SwapStage::Approved {
            return;
        }
        let (Some((sell, buy)), Some(approval)) = (swap_tokens(record), record.swap_approval())
        else {
            return;
        };
        let approval = approval.clone();
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (anchor_cache, tokens, destination_chain) = {
            let root = root.read(cx);
            let destination_chain = match approval.delivery {
                SwapDelivery::Bridge(delivery) => root
                    .effective_chain_configs
                    .get(delivery.destination_chain)
                    .cloned(),
                _ => None,
            };
            (
                Arc::clone(&root.public_broadcaster_anchor_cache),
                root.effective_token_registry.clone(),
                destination_chain,
            )
        };
        // The approved destination is looked up again in its provider's list.
        let bridge = destination_chain.zip(self.origin_bridge_profile(cx)).map(
            |(destination_chain, origin)| QuoteBridgeRequest {
                destination: None,
                destination_chain,
                origin,
            },
        );
        let tracking = self.tracking.entry(operation).or_default();
        let request = QuoteRequest {
            executor: QuoteExecutor::Order {
                operation,
                reuse: false,
            },
            sell,
            buy,
            amount: approval.bounds.spend_amount(),
            delivery: approval.delivery,
            slippage_bps: approval.bounds.slippage_bps,
            byte_budget: tracking.byte_budget,
            orderbook: tracking.orderbook.clone(),
            bridge_clients: None,
            bridge,
            anchor_cache: Some(anchor_cache),
            tokens,
        };
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        self.start_job(
            operation,
            SwapJobKind::Requote,
            async move { Ok::<_, eyre::Report>(quote_swap(owner, session, request).await) },
            move |this, result, window, cx| {
                this.apply_approved_quote(operation, &approval, result, window, cx);
            },
            window,
            cx,
        );
    }

    fn apply_approved_quote(
        &mut self,
        operation: ExecutorOperationId,
        approval: &SwapApproval,
        result: QuoteResult,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let tracking = self.tracking.entry(operation).or_default();
        if let Some(orderbook) = &result.orderbook {
            tracking.orderbook = Some(orderbook.clone());
        }
        // Whatever the answer, the next attempt waits for the user.
        tracking.auto_place = false;
        let outcome = match result.outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                self.fail(operation, format!("{error:#}"));
                return;
            }
        };
        // Keep the detail visible during the request. Only another dialog defers
        // authorization or review; failures remain available without an automatic retry.
        if window.has_active_dialog(cx) && !self.detail_is_active(operation, window, cx) {
            self.tracking.entry(operation).or_default().auto_place = true;
            return;
        }
        let change = match outcome {
            QuoteOutcome::Review(review) => match review.approval_change(approval) {
                None => {
                    let Some(orderbook) = result.orderbook else {
                        return;
                    };
                    let approval = OrderApproval {
                        operation,
                        review: Arc::from(review),
                        private_minimum: approval.bounds.private_minimum,
                        price_acknowledged: approval.price_acknowledged,
                        orderbook,
                        bridge: result.bridge,
                        destination_minimum: approval.bounds.destination_minimum,
                        full_review: false,
                    };
                    let summary = self.place_summary(&approval, cx);
                    self.request_authorization(
                        SwapAction::Order(Box::new(approval)),
                        summary,
                        window,
                        cx,
                    );
                    return;
                }
                Some(change) => Some(change),
            },
            QuoteOutcome::PriceBlocked(PriceBlock::Deviates) => {
                Some(SwapReviewChange::QuoteDeviates)
            }
            // The form shows the largest amount that fits.
            QuoteOutcome::TooLarge(_) => None,
        };
        // Nothing was signed. The form quotes the current terms and, once they are ready,
        // opens the review with the change named.
        self.reapproval = change.map(|change| (operation, change));
        self.open_existing_form(operation, window, cx);
    }

    /// The confirm-only step for an order approved with its setup, whose terms still hold. A
    /// Public address swap shows the approved receiver in full once more, and a Bridge swap
    /// every destination term the approval binds.
    fn place_summary(&self, approval: &OrderApproval, cx: &App) -> SpendAuthorizationSummary {
        let plan = approval.review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let valid_for = self
            .swap_profile(cx)
            .map_or(10, |profile| profile.valid_to_window().as_secs() / 60);
        let bridge = match plan.delivery() {
            SwapDelivery::Bridge(delivery) => Some(delivery),
            _ => None,
        };
        let receive = match (
            bridge,
            approval.review.bridge(),
            approval.destination_minimum,
        ) {
            (Some(delivery), Some(quote), Some(minimum)) => {
                self.bridge_receive_row(delivery, quote.expected_output, minimum, cx)
            }
            _ => SpendAuthorizationSummaryRow::new(
                "You receive",
                format!(
                    "≈ {}",
                    self.token_amount(buy, expected_output(&approval.review), cx)
                ),
            )
            .with_icon(self.token_icon(buy, cx))
            .with_note(format!(
                "At least {}",
                self.token_amount(buy, approval.private_minimum, cx)
            )),
        };
        let mut rows = vec![receive];
        rows.extend(self.delivery_rows(plan.delivery(), buy, None, cx).0);
        let checked = bridge.map_or_else(
            || "Costs and minimum were checked again and still match what you approved.".to_owned(),
            |delivery| {
                format!(
                    "Costs, the {} quote and the minimum were checked again and still match what you approved.",
                    provider_name(delivery.provider)
                )
            },
        );
        SpendAuthorizationSummary::new("Place swap order", checked, rows)
            .with_title_chip(self.chain_label())
            .with_progress(2, 2, "Step 2 of 2 · stealth account is set up")
            .with_asset_pair(
                SpendAuthorizationAsset::new(
                    self.token_amount(sell, plan.amount(), cx),
                    self.token_icon(sell, cx),
                ),
                self.review_buy_asset(plan.delivery(), buy, cx),
            )
            .with_details(
                "Order terms",
                format!("{valid_for} min from now"),
                vec![
                    (
                        "CoW network fee",
                        format!(
                            "{}, in the quote",
                            self.token_amount(buy, cow_fee(&approval.review), cx)
                        ),
                    ),
                    (
                        "Hook gas",
                        format!(
                            "Up to {}, covered by the minimum",
                            self.token_amount(buy, approval.review.hook_cost(), cx)
                        ),
                    ),
                    ("Order valid for", format!("{valid_for} minutes from now")),
                ],
                None,
            )
            .with_confirm_label("Place order")
            .with_warnings(if approval.review.price_verified() {
                Vec::new()
            } else {
                vec![Arc::from(UNVERIFIED_PRICE_WARNING)]
            })
    }

    pub(super) fn submit_order(
        &mut self,
        approval: OrderApproval,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() || !self.session_is_current(cx) {
            return;
        }
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (anchors, tokens) = {
            let root = root.read(cx);
            (
                Arc::clone(&root.public_broadcaster_anchor_cache),
                root.effective_token_registry.clone(),
            )
        };
        let operation = approval.operation;
        let plan = approval.review.plan();
        // A first order must match the approval saved with its setup. After a full review the
        // user authorized, the reviewed terms replace it; a confirm-only step never does.
        let replacement = (approval.full_review
            && self
                .record(operation)
                .is_some_and(|record| record.swap().is_none() && record.swap_approval().is_some()))
        .then(|| {
            approval
                .review
                .approval(approval.private_minimum, approval.price_acknowledged)
        });
        let pending = super::PendingSwapOrder {
            previous_order: self
                .record(operation)
                .and_then(|record| record.swap())
                .and_then(|swap| swap.orders().last())
                .map(wallet_ops::vault::SwapOrderRecord::uid),
            sell: plan.sell_token(),
            buy: plan.buy_token(),
            delivery: plan.delivery(),
            amount: plan.amount(),
            private_minimum: approval.private_minimum,
            slippage_bps: approval.review.slippage_bps(),
            reuse_account: plan.swap_executor().is_reused(),
            started_at: super::now_unix(),
        };
        self.tracking.entry(operation).or_default().pending_order = Some(pending);
        let owner = Arc::clone(&self.owner);
        let session = Arc::clone(&self.session);
        self.reapproval = None;
        self.start_job(
            operation,
            SwapJobKind::Order,
            async move {
                if let Some(replacement) = replacement {
                    owner.record_swap_approval(operation, replacement?)?;
                }
                Box::pin(owner.submit_swap_order(SwapOrderRequest {
                    review: approval.review.as_ref(),
                    private_minimum: approval.private_minimum,
                    price_acknowledged: approval.price_acknowledged,
                    session,
                    authorization,
                    orderbook: &approval.orderbook,
                    anchor_cache: &anchors,
                    token_registry: &tokens,
                    bridge: approval.bridge.as_ref().map(QuotedBridge::route),
                    destination_minimum: approval.destination_minimum,
                    verify_proof: true,
                }))
                .await
            },
            move |this, outcome, window, cx| this.finish_order(operation, outcome, window, cx),
            window,
            cx,
        );
    }

    pub(super) fn finish_order(
        &mut self,
        operation: ExecutorOperationId,
        outcome: SwapOrderOutcome,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let shown = self.swap_dialog_shows(operation, window, cx);
        if let SwapOrderOutcome::Submitted { .. } = outcome {
            let tracking = self.tracking.entry(operation).or_default();
            tracking.cursor = None;
            tracking.error = None;
            tracking.auto_place = false;
            // The swap's own form or detail moves on to the detail; another dialog stays.
            if !window.has_active_dialog(cx) || shown {
                self.show_detail(operation, window, cx);
            }
            cx.notify();
            return;
        }
        if window.has_active_dialog(cx)
            && !shown
            && self
                .form
                .as_ref()
                .is_none_or(|form| form.operation != Some(operation))
        {
            match outcome {
                SwapOrderOutcome::ReviewRequired(change) => {
                    self.reapproval = Some((operation, change));
                    self.fail(
                        operation,
                        format!("Review the swap again: {}.", review_change_label(change)),
                    );
                }
                SwapOrderOutcome::Replan { byte_budget, .. } => {
                    self.tracking.entry(operation).or_default().byte_budget = Some(byte_budget);
                    self.fail(operation, "The order is too large. Retry with a smaller amount after this attempt ends.".into());
                }
                SwapOrderOutcome::Submitted { .. } => {}
            }
            cx.notify();
            return;
        }
        // An order placed from the detail continues in the form.
        if self
            .form
            .as_ref()
            .is_none_or(|form| form.operation != Some(operation))
        {
            self.open_existing_form(operation, window, cx);
        }
        match outcome {
            SwapOrderOutcome::Submitted { .. } => {}
            SwapOrderOutcome::ReviewRequired(change) => {
                // Nothing was signed. Quote the current terms, then review them again.
                self.reapproval = Some((operation, change));
                self.schedule_quote(window, cx);
            }
            SwapOrderOutcome::Replan {
                byte_budget,
                attempt_recorded,
            } => {
                self.tracking.entry(operation).or_default().byte_budget = Some(byte_budget);
                if let Some(form) = self.form.as_mut() {
                    form.quote = QuoteState::Idle;
                    form.error = Some(if attempt_recorded {
                        "The orderbook rejected this order's size. The order stays recorded until it expires. Then retry, and the swap offers the largest amount that fits."
                    } else {
                        "The order is too large for the orderbook. Enter a smaller amount."
                    }
                    .into());
                }
                if !attempt_recorded {
                    self.schedule_quote(window, cx);
                }
            }
        }
        cx.notify();
    }

    // Presentation helpers

    pub(super) fn swap_profile(&self, cx: &App) -> Option<wallet_ops::settings::SwapProfile> {
        self.root
            .upgrade()?
            .read(cx)
            .effective_chain_configs
            .get(self.session.chain_id)?
            .swap_profile()
    }

    fn chain_label(&self) -> String {
        network_name(self.session.chain_id)
    }

    pub(super) fn token_icon(
        &self,
        token: Address,
        cx: &App,
    ) -> Option<crate::assets::WalletIconSource> {
        self.token_metadata(token, cx)
            .and_then(|metadata| metadata.icon_path)
    }

    /// "1 WETH = 2,601.30 USDC" at the quoted trading rate, excluding explicit fees.
    fn rate_label(&self, review: &SwapReview, cx: &App) -> String {
        let plan = review.plan();
        self.pair_rate_label(
            plan.sell_token(),
            plan.buy_token(),
            review.quote().sell_amount,
            review.quote().buy_amount,
            cx,
        )
        .unwrap_or_else(|| "Unavailable".into())
    }

    /// "1 WETH = 2,601.30 USDC" for `sell_amount` of `sell` exchanged for `buy_amount` of `buy`.
    pub(super) fn pair_rate_label(
        &self,
        sell: Address,
        buy: Address,
        sell_amount: U256,
        buy_amount: U256,
        cx: &App,
    ) -> Option<String> {
        let sell_decimals = self.token_decimals(sell, cx)?;
        let buy_decimals = self.token_decimals(buy, cx)?;
        if sell_amount.is_zero() {
            return None;
        }
        let rate = buy_amount.saturating_mul(U256::from(10u8).pow(U256::from(sell_decimals)))
            / sell_amount;
        Some(format!(
            "1 {} = {} {}",
            self.token_symbol(sell, cx),
            railgun_ui::format_token_amount(rate, buy_decimals),
            self.token_symbol(buy, cx)
        ))
    }

    // Rendering

    /// "Retry swap" once an attempt of this swap ended, otherwise "Swap".
    pub(super) fn form_title(&self) -> &'static str {
        if self.form.as_ref().is_some_and(|form| self.is_retry(form)) {
            "Retry swap"
        } else {
            "Swap"
        }
    }

    /// The form retries an attempt of its swap that ended. A retry keeps the attempt's tokens,
    /// stealth account and delivery; a new swap on a reused account chooses its own.
    fn is_retry(&self, form: &SwapForm) -> bool {
        !form.reuse_account
            && form
                .operation
                .and_then(|operation| self.record(operation))
                .and_then(ExecutorRecord::swap)
                .is_some_and(|swap| !swap.orders().is_empty())
    }

    /// Receive to can't change: a retry keeps its attempt's delivery, and a set-up swap whose
    /// fixed pair buys the native asset, or bridges, can only pay a Public address, though it
    /// may name another receiver.
    fn receive_to_locked(&self, form: &SwapForm) -> bool {
        self.is_retry(form)
            || ((form.native_output || form.network.is_some())
                && form.operation.is_some()
                && !form.reuse_account)
    }

    /// The order detail the form returns to, when it continues or retries a swap.
    pub(super) fn form_back_target(&self) -> Option<ExecutorOperationId> {
        self.form.as_ref()?.back_to_detail
    }

    pub(super) fn focus_form_amount(&self, window: &mut Window, cx: &mut App) {
        if let Some(form) = self.form.as_ref() {
            form.amount_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        }
    }

    pub(super) fn close_setup_settings(&mut self, cx: &mut Context<'_, Self>) {
        self.set_setup_settings_open(false, cx);
    }

    fn set_setup_settings_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.settings_open != open
        {
            form.settings_open = open;
            cx.notify();
        }
    }

    fn set_receive_help_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.receive_help_open != open
        {
            form.receive_help_open = open;
            cx.notify();
        }
    }

    fn toggle_details(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.details_open = !form.details_open;
            cx.notify();
        }
    }

    /// Swap the sell and buy tokens. The amount belonged to the old sell token, so it clears.
    fn flip_tokens(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let (sell, Some(buy)) = (form.sell, form.buy) else {
            return;
        };
        let (input, sell_select, buy_select) = (
            form.amount_input.clone(),
            form.sell_select.clone(),
            form.buy_select.clone(),
        );
        input.update(cx, |input, cx| input.set_value("", window, cx));
        sell_select.update(cx, |select, cx| {
            select.set_selected_value(&buy, window, cx);
        });
        self.set_form_sell(buy, window, cx);
        self.set_form_buy(sell, window, cx);
        buy_select.update(cx, |select, cx| {
            select.set_selected_values(&[sell], window, cx);
        });
    }

    /// Open the broadcaster picker on top of the swap dialog. The popover draws above dialogs,
    /// so it closes first, and focus moves to the amount, where the picker returns it.
    fn choose_specific_setup_broadcaster(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(token) = form.route.fee_token else {
            return;
        };
        form.settings_open = false;
        let input = form.amount_input.clone();
        input.read(cx).focus_handle(cx).focus(window, cx);
        let target = BroadcasterPickerTarget::Swap(cx.weak_entity());
        let chain_id = self.session.chain_id;
        let _ = self.root.update(cx, |root, cx| {
            root.open_broadcaster_picker_for_target(
                target,
                "Swap setup",
                chain_id,
                token,
                window,
                cx,
            );
        });
        cx.notify();
    }

    /// The wallet's cached USD value of `amount` in micro-dollars, for display only. The price
    /// check never uses it.
    pub(super) fn usd_micro_value(&self, token: Address, amount: U256, cx: &App) -> Option<U256> {
        self.network_usd_micro_value(self.session.chain_id, token, amount, cx)
    }

    fn network_usd_micro_value(
        &self,
        network: u64,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> Option<U256> {
        let root = self.root.upgrade()?;
        let cache = &root.read(cx).public_broadcaster_anchor_cache;
        if token == Address::ZERO {
            return cache.cached_native_usd_micro_value(network, amount);
        }
        cache.cached_token_usd_micro_value(network, token, amount)
    }

    fn usd_label(&self, token: Address, amount: U256, cx: &App) -> Option<String> {
        let usd = self.usd_micro_value(token, amount, cx)?;
        Some(format!("≈ {}", railgun_ui::format_usd_micro_value(usd)))
    }

    fn bridge_usd_value(&self, review: &SwapReview, cx: &App) -> Option<U256> {
        let SwapDelivery::Bridge(delivery) = review.plan().delivery() else {
            return None;
        };
        let bridge = review.bridge()?;
        self.network_usd_micro_value(
            delivery.destination_chain,
            delivery.destination_token,
            bridge.expected_output,
            cx,
        )
        .or_else(|| {
            // Across delivers the same asset. Its fixed deposit less the relay fee is
            // already in source-token units, so no destination decimals or $1 peg are assumed.
            if bridge.provider != BridgeProvider::Across {
                return None;
            }
            let received = review
                .suggested_private_minimum()
                .checked_sub(bridge.fee?)?;
            self.usd_micro_value(review.plan().buy_token(), received, cx)
        })
    }

    fn bridge_total_usd_value(&self, review: &SwapReview, cx: &App) -> Option<U256> {
        let surplus = review.estimated_source_surplus()?;
        let source = if surplus.is_zero() {
            U256::ZERO
        } else {
            self.usd_micro_value(review.plan().buy_token(), surplus, cx)?
        };
        Some(self.bridge_usd_value(review, cx)?.saturating_add(source))
    }

    /// `label`, which shows `amount` of `token`, followed by the amount's USD value as the
    /// wallet shows it elsewhere: "0.0025 WETH · $6.76", nothing for a dollar stablecoin, or
    /// "· USD unavailable" without a cached rate.
    pub(super) fn with_usd(&self, label: String, token: Address, amount: U256, cx: &App) -> String {
        format_value_with_usd_label(
            label,
            amount,
            self.token_decimals(token, cx),
            self.usd_micro_value(token, amount, cx),
            false,
        )
    }

    /// The USD value [`Self::with_usd`] would show for `amount` of `token`, "$6.76": none for a
    /// dollar stablecoin or without a cached rate.
    pub(super) fn usd_value(&self, token: Address, amount: U256, cx: &App) -> Option<String> {
        let usd = self.usd_micro_value(token, amount, cx)?;
        self.token_decimals(token, cx)
            .is_none_or(|decimals| {
                railgun_ui::non_redundant_usd_micro_value(amount, decimals, usd).is_some()
            })
            .then(|| railgun_ui::format_usd_micro_value(usd))
    }

    /// The private balance of `token`, when the wallet holds any.
    fn private_balance_label(&self, form: &SwapForm, token: Address, cx: &App) -> Option<String> {
        let (_, total) = form
            .assets
            .totals
            .iter()
            .find(|(asset, _)| *asset == token)?;
        Some(self.token_amount(token, *total, cx))
    }

    /// [`Self::bare_amount`] of a token on `network`, such as a Bridge swap's destination.
    fn network_bare_amount(&self, network: u64, token: Address, amount: U256, cx: &App) -> String {
        self.chain_token_metadata(network, token, cx).map_or_else(
            || amount.to_string(),
            |metadata| railgun_ui::format_token_amount(amount, metadata.decimals),
        )
    }

    /// [`Self::token_amount`] of a token on `network`, such as a Bridge swap's destination.
    pub(super) fn network_token_amount(
        &self,
        network: u64,
        token: Address,
        amount: U256,
        cx: &App,
    ) -> String {
        format!(
            "{} {}",
            self.network_bare_amount(network, token, amount, cx),
            self.network_token_symbol(network, token, cx)
        )
    }

    pub(super) fn network_token_symbol(&self, network: u64, token: Address, cx: &App) -> String {
        self.chain_token_metadata(network, token, cx).map_or_else(
            || railgun_ui::short_address(&token),
            |metadata| metadata.symbol,
        )
    }

    /// An amount without its symbol, for a panel whose token select names the token.
    pub(super) fn bare_amount(&self, token: Address, amount: U256, cx: &App) -> String {
        self.token_decimals(token, cx).map_or_else(
            || amount.to_string(),
            |decimals| railgun_ui::format_token_amount(amount, decimals),
        )
    }

    /// "#181 · 0x7d20…aa10" for the swap's own stealth account.
    fn account_label(&self, operation: ExecutorOperationId) -> Option<String> {
        let record = self.record(operation)?;
        Some(record.address().map_or_else(
            || format!("#{}", record.index()),
            |address| {
                format!(
                    "#{} · {}",
                    record.index(),
                    railgun_ui::short_address(&address)
                )
            },
        ))
    }

    /// "Setup ≈ 0.0001 WETH · $0.28 · random broadcaster", or where the estimate stands.
    fn setup_line(&self, form: &SwapForm, cx: &App) -> String {
        if let Some(estimate) = &form.route.estimate {
            let broadcaster = if form.route.selected.is_some() {
                selected_broadcaster_label(&form.route.choice(), &form.route.candidates)
            } else {
                "random broadcaster".to_owned()
            };
            let (token, fee) = (estimate.broadcaster().token, estimate.fee_amount());
            return format!(
                "Setup ≈ {} · {broadcaster}",
                self.with_usd(self.token_amount(token, fee, cx), token, fee, cx)
            );
        }
        let status = if let Some(error) = &form.route.estimate_error {
            error.clone()
        } else if form.route.estimate_task.is_some() {
            "Estimating…".into()
        } else if form.route.fee_options.is_empty() {
            "No spendable private fee token".into()
        } else {
            "Waiting for a compatible broadcaster".into()
        };
        format!("Setup · {status}")
    }

    /// The form and its footer: the credit, Cancel, and the form's primary action.
    pub(super) fn render_form(&self, cx: &Context<'_, Self>) -> (gpui::Div, Option<gpui::Div>) {
        let Some(form) = self.form.as_ref() else {
            return (app_muted_text("Wallet session ended."), None);
        };
        let mode = self.form_mode(form);
        let quoting = matches!(mode, FormMode::Setup { .. } | FormMode::Order);
        let editable = quoting && !self.busy();
        let locked = form.operation.is_some() && !form.reuse_account;
        let sell_assets = &form.assets.sell_assets;
        let review = match &form.quote {
            QuoteState::Ready(review) if quoting => Some(review),
            _ => None,
        };
        let close_settings = cx.entity();
        let footer_close_settings = close_settings.clone();
        let body = div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_4()
            // A press elsewhere in the form closes the setup broadcaster popover. The popover
            // and the fee token list it opens draw above the form, so presses there don't.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                close_settings.update(cx, Self::close_setup_settings);
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.render_sell_panel(form, sell_assets, editable, locked, cx))
                    .child(
                        div()
                            .relative()
                            .child(self.render_buy_panel(form, editable, locked, cx))
                            .child(self.render_flip(form, sell_assets, editable, locked, cx)),
                    ),
            )
            .children(self.render_price_acknowledgement(form, cx))
            .child(self.render_delivery(form, editable, cx))
            .child(self.render_account_row(form, mode, editable, cx))
            .children(review.map(|review| {
                self.render_details(
                    form,
                    review,
                    matches!(mode, FormMode::Setup { .. }),
                    editable,
                    cx,
                )
            }))
            .children(form.error.as_ref().map(|error| {
                Alert::error("swap-form-error", error.clone())
                    .small()
                    .min_w_0()
            }));
        let footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            // The footer sits outside the form, so it closes the popover the same way.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                footer_close_settings.update(cx, Self::close_setup_settings);
            })
            .child(powered_by_cow(cx))
            // The buttons wrap as one group, so a narrow dialog keeps them together on the
            // right.
            .child(
                div()
                    .ml_auto()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button("swap-form-cancel", "Cancel")
                            .flex_none()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_swap_dialog(window, cx);
                            })),
                    )
                    .child(self.render_form_primary(form, mode, cx)),
            );
        (body, Some(footer))
    }

    fn render_form_primary(
        &self,
        form: &SwapForm,
        mode: FormMode,
        cx: &Context<'_, Self>,
    ) -> gpui::AnyElement {
        let busy = self.busy();
        let quoted = match &form.quote {
            QuoteState::Ready(review) => form.review_problem(review).is_none(),
            _ => false,
        };
        match mode {
            FormMode::Setup { .. } | FormMode::SettingUp | FormMode::Order => {
                // A new swap's review also approves its setup, so it needs the setup's fee.
                let ready = match mode {
                    FormMode::Setup { .. } => quoted && form.route.estimate.is_some(),
                    FormMode::Order => quoted,
                    FormMode::SettingUp | FormMode::Placed => false,
                };
                app_button("swap-form-review", "Review…")
                    .primary()
                    .flex_none()
                    .loading(busy)
                    .disabled(busy || !ready)
                    .on_click(cx.listener(|this, _, window, cx| this.form_primary(window, cx)))
                    .into_any_element()
            }
            FormMode::Placed => {
                let operation = form.operation;
                app_button("swap-form-progress", "View progress…")
                    .flex_none()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(operation) = operation {
                            this.navigate(SwapDialogView::Detail(operation), window, cx);
                        }
                    }))
                    .into_any_element()
            }
        }
    }

    /// The Sell panel: the amount, its token, what's available, and the amount-reduction
    /// prompt when the amount doesn't fit one swap.
    fn render_sell_panel(
        &self,
        form: &SwapForm,
        sell_assets: &[UnshieldAsset],
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let asset = sell_assets.iter().find(|asset| asset.token == form.sell);
        let available = asset.map_or(U256::ZERO, |asset| match &form.quote {
            QuoteState::TooLarge { largest } => (*largest).min(asset.max_batched),
            _ => asset.max_batched,
        });
        let available_label = self.token_amount(form.sell, available, cx);
        let locked_value = asset.map_or(U256::ZERO, |_| form.assets.locked);
        let usd = self
            .form_amount(form, cx)
            .ok()
            .and_then(|amount| self.usd_label(form.sell, amount, cx));
        let too_large = self.render_too_large(form, editable, cx);
        let submit_view = cx.entity();
        let submit_enabled = editable && matches!(form.quote, QuoteState::Ready(_));
        amount_panel(too_large.is_some(), cx)
            .child(app_muted_text("Sell"))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .on_action(move |_: &InputEnter, window, cx| {
                                if submit_enabled {
                                    submit_view
                                        .update(cx, |view, cx| view.form_primary(window, cx));
                                }
                            })
                            .child(app_amount_input(&form.amount_input).disabled(!editable)),
                    )
                    .child(token_pill(ui::private_action::asset_select(
                        &form.sell_select,
                        locked || !editable,
                    ))),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_2()
                    .child(div().flex_1().min_w_0().children(usd.map(app_muted_text)))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .children(asset.map(|_| {
                                balance_text(if locked_value.is_zero() {
                                    available_label.clone()
                                } else {
                                    format!(
                                        "{available_label} · {} locked",
                                        self.token_amount(form.sell, locked_value, cx)
                                    )
                                })
                            }))
                            .when(!locked_value.is_zero(), |row| {
                                row.child(
                                    app_button_base("swap-review-locked-notes")
                                        .label("Review")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                                        .tooltip("Show the locked notes and what holds them")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_locked_notes(window, cx);
                                        })),
                                )
                            })
                            .when(!available.is_zero(), |row| {
                                row.child(
                                    app_button_base("swap-amount-max")
                                        .debug_selector(|| "swap-amount-max".into())
                                        .label("Max")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                                        .disabled(!editable)
                                        .tooltip(format!(
                                            "Use {available_label}, the most one swap can spend"
                                        ))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.use_amount(available, window, cx);
                                        })),
                                )
                            }),
                    ),
            )
            .children(too_large)
    }

    /// The Buy panel: the raw quote, its token, the price check, and the expected amount after
    /// every cost. The flip button sits on the seam above it.
    fn render_buy_panel(
        &self,
        form: &SwapForm,
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let review = match &form.quote {
            QuoteState::Ready(review) => Some(review),
            _ => None,
        };
        // A Bridge swap shows what arrives on its network, in that network's token.
        let amount = review.map(|review| match (form.network, review.bridge()) {
            (Some(network), Some(bridge)) => self.network_bare_amount(
                network,
                review_destination_token(review),
                bridge.expected_output,
                cx,
            ),
            _ => self.bare_amount(review.plan().buy_token(), review.quote().buy_amount, cx),
        });
        let (title, balance) = match form.network {
            Some(network) => (format!("Buy on {}", network_name(network)), None),
            None => (
                "Buy".to_owned(),
                form.buy
                    .and_then(|buy| self.private_balance_label(form, buy, cx)),
            ),
        };
        let same_token = matches!(form.bridge_state(), BridgeState::SameToken);
        amount_panel(same_token, cx)
            .child(app_muted_text(title))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(match amount {
                        Some(amount) => app_amount_text(amount).truncate(),
                        None => app_amount_text("0").text_color(rgb(theme::TEXT_SUBTLE)),
                    }))
                    .child(
                        token_pill(
                            Combobox::new(&form.buy_select)
                                .w_full()
                                .placeholder("Select asset")
                                .disabled(locked || !editable)
                                .render_trigger(|trigger, _, cx| {
                                    let title = trigger.selection().first().map_or_else(
                                        || {
                                            div()
                                                .child(
                                                    trigger
                                                        .placeholder()
                                                        .cloned()
                                                        .unwrap_or_default(),
                                                )
                                                .into_any_element()
                                        },
                                        |(_, item)| {
                                            item.display_title()
                                                .unwrap_or_else(|| item.title().into_any_element())
                                        },
                                    );
                                    div()
                                        .w_full()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .text_color(if trigger.is_disabled() {
                                            cx.theme().muted_foreground
                                        } else {
                                            cx.theme().foreground
                                        })
                                        .child(div().flex_1().min_w_0().child(title))
                                        .child(
                                            Caret::new(trigger.size())
                                                .text_color(cx.theme().muted_foreground),
                                        )
                                })
                                .when(form.buy.is_none() && editable && !locked, |select| {
                                    select
                                        .border_color(cx.theme().primary.opacity(0.65))
                                        .bg(cx.theme().primary.opacity(0.12))
                                }),
                        )
                        .debug_selector(|| "swap-buy-selector".into()),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(
                        same_token
                            || matches!(
                                form.quote,
                                QuoteState::Failed(_) | QuoteState::PriceBlocked(_)
                            ),
                        gpui::Styled::items_start,
                    )
                    .child(self.render_price_line(form, cx).flex_1().min_w_0())
                    .children(balance.map(|balance| balance_text(balance).flex_none())),
            )
            .children(self.render_native_output(form, editable, locked, cx))
            .children(review.and_then(|review| {
                if !matches!(
                    self.form_mode(form),
                    FormMode::Setup { .. } | FormMode::Order
                ) {
                    return None;
                }
                Self::render_cost_acknowledgement(form, high_cost_bps(review), editable, cx)
                    .map(gpui::Styled::mt_2)
            }))
            .children(review.map(|review| self.render_receive_strip(form, review, cx)))
    }

    /// Private Unshield's native/wrapped output switch, offered while a Public address receives
    /// the chain's wrapped native token.
    fn render_native_output(
        &self,
        form: &SwapForm,
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        if !self.offers_native_output(form, cx) {
            return None;
        }
        let (native_label, wrapped_label) = native_wrapped_output_labels(self.session.chain_id)?;
        let view = cx.entity();
        Some(
            ui::private_action::unshield_output_toggle(
                "swap-native-output",
                native_label,
                wrapped_label,
                form.native_output,
                locked || !editable,
                move |native, window, cx| {
                    view.update(cx, |view, cx| view.set_native_output(native, window, cx));
                },
            )
            .debug_selector(|| "swap-native-output".into()),
        )
    }

    fn render_flip(
        &self,
        form: &SwapForm,
        sell_assets: &[UnshieldAsset],
        editable: bool,
        locked: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let reason = match form.buy {
            _ if locked => Some("This swap's tokens are fixed".to_owned()),
            _ if form.network.is_some() => Some("The tokens are on different networks".to_owned()),
            None => Some("Choose a token to receive first".to_owned()),
            Some(buy)
                if !sell_assets
                    .iter()
                    .any(|asset| asset.token == buy && !asset.max_batched.is_zero()) =>
            {
                Some(format!(
                    "No spendable private {} to sell",
                    self.token_symbol(buy, cx)
                ))
            }
            // The Buy list when `buy` is sold leaves `buy` out.
            Some(buy) if buy == form.sell || !form.assets.sell_receivable => Some(format!(
                "{} can't be received in a private swap",
                self.token_symbol(form.sell, cx)
            )),
            Some(_) => None,
        };
        let disabled = !editable || reason.is_some();
        let tooltip = reason.unwrap_or_else(|| "Switch the Sell and Buy tokens".to_owned());
        // Paint after the Buy panel as its sibling: GPUI paints a parent's border after its
        // children. Center the small button over the half-gap above the panel.
        div()
            .absolute()
            .left_0()
            .right_0()
            .top(-rems(0.75 + 0.125))
            .flex()
            .justify_center()
            .child(
                app_button_base("swap-flip")
                    .small()
                    .border_1()
                    .border_color(cx.theme().border)
                    .icon(IconName::ArrowDown)
                    .accessibility_label("Switch tokens")
                    .tooltip(tooltip)
                    .disabled(disabled)
                    .debug_selector(|| "swap-flip".into())
                    .on_click(cx.listener(|this, _, window, cx| this.flip_tokens(window, cx))),
            )
    }

    /// The price check under the Buy amount. A failed or missing check replaces the anchor
    /// delta on the same line.
    fn render_price_line(&self, form: &SwapForm, cx: &Context<'_, Self>) -> gpui::Div {
        let retry = || {
            app_button("swap-price-retry", "Retry")
                .debug_selector(|| "swap-price-retry".into())
                .outline()
                .small()
                .flex_none()
                .disabled(self.busy() || form.quote_task.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.retry_quote(window, cx)))
        };
        let line = div()
            .debug_selector(|| "swap-price-status".into())
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x_2()
            .when(
                matches!(
                    form.quote,
                    QuoteState::Failed(_) | QuoteState::PriceBlocked(_)
                ),
                |line| line.flex_col().items_start().gap_y_1(),
            );
        // Until a provider delivers the Buy token, the line says why a Bridge swap can't be
        // quoted.
        let network = network_name(form.network.unwrap_or(self.session.chain_id));
        match form.bridge_state() {
            BridgeState::Loading => {
                return line
                    .child(Spinner::new().small())
                    .child(app_muted_text(format!(
                        "Getting the tokens {network} can receive…"
                    )));
            }
            BridgeState::Failed(error) => {
                return line
                    .flex_col()
                    .items_start()
                    .gap_y_1()
                    .child(self.render_quote_error(error, cx))
                    .child(
                        app_button("swap-bridge-routes-retry", "Retry")
                            .outline()
                            .small()
                            .flex_none()
                            .disabled(self.busy())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.retry_bridge_routes(window, cx);
                            })),
                    );
            }
            BridgeState::SameToken => {
                return line.child(
                    div()
                        .debug_selector(|| "swap-bridge-same-token".into())
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(
                            Icon::new(IconName::CircleX)
                                .xsmall()
                                .flex_none()
                                .text_color(cx.theme().danger),
                        )
                        .child(
                            app_text(SAME_TOKEN_BRIDGE)
                                .min_w_0()
                                .text_color(cx.theme().danger)
                                .whitespace_normal(),
                        ),
                );
            }
            BridgeState::Unavailable => {
                return line.child(
                    app_text(format!(
                        "No bridge delivers this token to {network} now. Choose another token."
                    ))
                    .text_color(cx.theme().danger)
                    .whitespace_normal(),
                );
            }
            BridgeState::SameChain | BridgeState::NoToken | BridgeState::Ready { .. } => {}
        }
        // The Provider row shows a provider's refusal of the amount.
        if let QuoteState::Failed(error) = &form.quote
            && bridge_rejection(error).is_some()
        {
            return line;
        }
        match &form.quote {
            QuoteState::Idle | QuoteState::TooLarge { .. } => line.child(
                app_muted_text(if form.buy.is_none() {
                    "Choose a token to receive"
                } else if matches!(form.quote, QuoteState::Idle)
                    && matches!(form.delivery, Err(DeliveryProblem::Receiver(_)))
                    && self.form_amount(form, cx).is_ok()
                {
                    "Enter a receiver to get a quote"
                } else {
                    "Enter an amount that fits to get a quote"
                })
                .whitespace_normal(),
            ),
            QuoteState::Loading => line
                .child(Spinner::new().small())
                .child(app_muted_text("Getting a quote and checking the price…")),
            QuoteState::Failed(error) => line
                .child(self.render_quote_error(error, cx))
                .child(retry()),
            QuoteState::PriceBlocked(block) => {
                let text = match block {
                    PriceBlock::Deviates => {
                        let threshold = self
                            .swap_profile(cx)
                            .map_or(300, |profile| profile.anchor_deviation_bps());
                        format!(
                            "The exchange rate before fees is more than {} below the anchor price",
                            format_bps_percent(u64::from(threshold))
                        )
                    }
                };
                line.child(
                    app_text(text)
                        .w_full()
                        .min_w_0()
                        .text_color(rgb(theme::WARNING))
                        .whitespace_normal(),
                )
                .child(retry())
            }
            QuoteState::Ready(review) => {
                if review.bridge().is_some() {
                    return line
                        .debug_selector(|| "swap-destination-usd".into())
                        .child(app_muted_text(
                            self.bridge_usd_value(review, cx).map_or_else(
                                || "USD value unavailable".to_owned(),
                                |usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)),
                            ),
                        ))
                        .when(!review.price_verified(), |line| {
                            line.child(
                                app_text(UNVERIFIED_PRICE_WARNING)
                                    .text_color(cx.theme().warning)
                                    .whitespace_normal(),
                            )
                        });
                }
                let buy = review.plan().buy_token();
                match review.price() {
                    SwapPrice::Unverified => line.child(
                        app_text(UNVERIFIED_PRICE_WARNING)
                            .text_color(cx.theme().warning)
                            .whitespace_normal(),
                    ),
                    SwapPrice::Verified { .. } => line
                        .children(
                            self.usd_label(buy, review.quote().buy_amount, cx)
                                .map(app_muted_text),
                        )
                        .children(price_delta(review, cx).map(|(delta, checked)| {
                            div().id("swap-price-delta").child(delta).when_some(
                                checked,
                                |delta, checked| {
                                    delta.tooltip(move |window, cx| {
                                        Tooltip::new(checked.clone()).build(window, cx)
                                    })
                                },
                            )
                        })),
                }
            }
        }
    }

    fn render_quote_error(&self, error: &eyre::Report, cx: &App) -> gpui::Div {
        let message = match error.downcast_ref::<OrderLimitError>() {
            Some(OrderLimitError::HookCostExceedsOutput {
                buy_token,
                hook_cost,
                ..
            }) => format!(
                "Sell amount is too small. Estimated cost: {}",
                self.token_amount(*buy_token, *hook_cost, cx)
            ),
            _ => bridge_unreachable(error).unwrap_or_else(|| format!("{error:#}")),
        };
        app_text(message)
            .debug_selector(|| "swap-price-error".into())
            .w_full()
            .min_w_0()
            .text_color(cx.theme().danger)
            .whitespace_normal()
    }

    /// The expected private amount at the quoted price, with the signed minimum beneath it, on a
    /// strip across the bottom of the Buy panel.
    fn render_receive_strip(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        // A Bridge swap receives on its network, in that network's token.
        let (label, help, expected, minimum) = if let (Some(network), Some(bridge)) =
            (form.network, review.bridge())
        {
            let token = review_destination_token(review);
            (
                format!("Receive on {}", network_name(network)),
                match bridge.provider {
                    BridgeProvider::Across => {
                        "The bridge pays this fixed amount after its fee. Any remaining CoW payout stays on the source chain. Dollar values use the source asset's price when the destination price is unavailable."
                    }
                    BridgeProvider::NearIntents => {
                        "The bridge's quote includes its fee. It converts the full CoW payout, including any surplus."
                    }
                },
                self.network_bare_amount(network, token, bridge.expected_output, cx),
                self.network_bare_amount(network, token, bridge.destination_minimum, cx),
            )
        } else {
            let token = review.plan().buy_token();
            (
                "You receive".to_owned(),
                if matches!(review.plan().delivery(), SwapDelivery::External { .. }) {
                    "The estimate includes CoW's fee and estimated hook gas. The minimum also allows for higher gas use and slippage."
                } else {
                    "The estimate includes CoW's fee, estimated hook gas and Railgun's shield fee. The minimum also allows for higher gas use and slippage."
                },
                self.bare_amount(token, expected_output(review), cx),
                self.bare_amount(token, review.suggested_private_minimum(), cx),
            )
        };
        let fixed = review
            .bridge()
            .is_some_and(|bridge| bridge.provider == BridgeProvider::Across);
        let open = form.receive_help_open;
        let view = cx.entity();
        div()
            // Out to the panel's border, past its `px_3` and `py_2p5` padding.
            .mx(rems(-0.75))
            .mb(rems(-0.625))
            .mt_1()
            .px_3()
            .py_2p5()
            .rounded_b_lg()
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        Popover::new("swap-receive-help-popover")
                            .open(open)
                            .on_open_change(move |open, _, cx| {
                                view.update(cx, |view, cx| view.set_receive_help_open(*open, cx));
                            })
                            .trigger(
                                app_button_base("swap-receive-help-trigger")
                                    .text()
                                    .xsmall()
                                    .compact()
                                    .accessibility_label("About the receive amount")
                                    .child(
                                        div()
                                            .id("swap-receive-help")
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .child(app_muted_text(label))
                                            .child(
                                                Icon::new(IconName::Info)
                                                    .xsmall()
                                                    .text_color(rgb(theme::TEXT_MUTED)),
                                            )
                                            .when(!open, |this| {
                                                this.tooltip(move |window, cx| {
                                                    Tooltip::element(move |window, _| {
                                                        receive_help_card(help, window)
                                                    })
                                                    .build(window, cx)
                                                })
                                            }),
                                    ),
                            )
                            .content(move |_, window, _| receive_help_card(help, window)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .items_end()
                            .child(
                                app_strong_text(if fixed {
                                    expected
                                } else {
                                    format!("≈ {expected}")
                                })
                                .text_size(theme::BALANCE_TEXT_SIZE)
                                .font_weight(gpui::FontWeight::SEMIBOLD),
                            )
                            .child(
                                app_muted_text(if fixed {
                                    "Fixed bridge payout".to_owned()
                                } else {
                                    format!("at least {minimum}")
                                })
                                .text_right()
                                .whitespace_normal(),
                            ),
                    ),
            )
            .children(self.render_source_return(review, cx))
    }

    fn render_source_return(&self, review: &SwapReview, cx: &App) -> Option<gpui::Div> {
        let surplus = review.estimated_source_surplus()?;
        let token = review.plan().buy_token();
        Some(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .pt_2()
                .border_t_1()
                .border_color(cx.theme().border)
                .debug_selector(|| "swap-source-return".into())
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_start()
                        .justify_between()
                        .gap_2()
                        .child(
                            app_muted_text(format!("Estimated return on {}", self.chain_label()))
                                .min_w_0()
                                .whitespace_normal(),
                        )
                        .child(
                            app_text(self.with_usd(
                                format!("≈ {}", self.token_amount(token, surplus, cx)),
                                token,
                                surplus,
                                cx,
                            ))
                            .flex_none(),
                        ),
                )
                .child(
                    app_muted_text(source_return_note(review))
                        .text_xs()
                        .whitespace_normal(),
                )
                .when_some(self.bridge_total_usd_value(review, cx), |row, total| {
                    row.child(
                        div()
                            .w_full()
                            .flex()
                            .justify_between()
                            .gap_2()
                            .debug_selector(|| "swap-total-received".into())
                            .child(app_text("Estimated total received"))
                            .child(app_strong_text(format!(
                                "≈ {}",
                                railgun_ui::format_usd_micro_value(total)
                            ))),
                    )
                }),
        )
    }

    fn render_price_acknowledgement(
        &self,
        form: &SwapForm,
        cx: &Context<'_, Self>,
    ) -> Option<Checkbox> {
        let QuoteState::Ready(review) = &form.quote else {
            return None;
        };
        (!review.price_verified()).then(|| {
            Checkbox::new("swap-price-acknowledged")
                .label("I accept this price without an independent check")
                .checked(form.price_acknowledged)
                .small()
                .disabled(self.busy())
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    if let Some(form) = this.form.as_mut() {
                        form.price_acknowledged = *checked;
                        form.error = None;
                    }
                    cx.notify();
                }))
        })
    }

    fn render_cost_acknowledgement(
        form: &SwapForm,
        cost_bps: Option<u64>,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let bps = cost_bps?;
        let warning = cx.theme().warning;
        // Alert accepts text only. One frame contains its message and the standard checkbox.
        Some(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(warning.mix_oklab(gpui::transparent_white(), 0.3))
                .bg(warning.mix_oklab(gpui::transparent_white(), 0.04))
                .debug_selector(|| "swap-high-costs".into())
                .child(
                    div()
                        .min_w_0()
                        .debug_selector(|| "swap-high-cost-message".into())
                        .child(
                            Alert::warning("swap-high-costs", high_cost_message(bps))
                                .small()
                                .p_0()
                                .border_0()
                                .bg(gpui::transparent_black()),
                        ),
                )
                .child(
                    div()
                        .debug_selector(|| "swap-costs-acknowledged".into())
                        .child(
                            Checkbox::new("swap-costs-acknowledged")
                                .label("Swap anyway")
                                .checked(form.high_costs_acknowledged)
                                .small()
                                .disabled(!editable)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    if let Some(form) = this.form.as_mut() {
                                        form.high_costs_acknowledged = *checked;
                                        form.error = None;
                                    }
                                    cx.notify();
                                })),
                        ),
                ),
        )
    }

    /// Receive to, and for a Public address the Receiver with its suggestions. Under the
    /// receiver: the saved entry it matches, why it can't be used, and for native output, that
    /// a contract wallet may not accept it. A retry keeps its attempt's delivery, and a set-up
    /// native pair keeps its Public address.
    fn render_delivery(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let disabled = !editable || self.receive_to_locked(form);
        let choice = |id: &'static str, label: &'static str, receive_to: ReceiveTo| {
            app_segment_button(id, label, form.receive_to == receive_to, disabled, None).on_click(
                cx.listener(move |this, _, window, cx| {
                    this.set_receive_to(receive_to, window, cx);
                }),
            )
        };
        let rows = div().w_full().flex().flex_col().gap_4().child(labeled_row(
            "Receive to",
            div().flex().child(
                ButtonGroup::new("swap-receive-to")
                    .outline()
                    .compact()
                    .disabled(disabled)
                    .child(choice(
                        "swap-receive-private",
                        "Private balance",
                        ReceiveTo::PrivateBalance,
                    ))
                    .child(choice(
                        "swap-receive-public",
                        "Public address",
                        ReceiveTo::PublicAddress,
                    )),
            ),
        ));
        if form.receive_to == ReceiveTo::PrivateBalance {
            return rows;
        }
        // Another network changes the Buy tokens, so a set-up swap's fixed pair keeps its own.
        let locked = form.operation.is_some() && !form.reuse_account;
        let rows = rows.child(labeled_row(
            "Network",
            Select::new(&form.network_select)
                .w_full()
                .disabled(!editable || locked),
        ));
        let view = cx.entity();
        let save_root = self.root.clone();
        let picker = form_recipient_picker(
            "swap-receiver-picker",
            "swap-save-receiver".into(),
            RECEIVER_RULES,
            &form.receiver_input,
            &form.receiver_value,
            form.receiver_suggestions_open,
            form.receiver_suggestion_index,
            &form.receiver_suggestions_scroll,
            &self.receiver_options(cx),
            !editable || self.is_retry(form),
            move |event, window, cx| {
                view.update(cx, |view, cx| view.receiver_picker_event(event, window, cx));
            },
            move |receiver, window, cx| {
                let _ = save_root.update(cx, |root, cx| {
                    root.open_save_recipient_dialog(RECEIVER_RULES, receiver, window, cx);
                });
            },
        );
        let matched = match &form.delivery {
            Err(DeliveryProblem::Receiver(_)) => None,
            _ => parse_address(form.receiver_value.trim())
                .and_then(|receiver| self.receiver_label(receiver, cx)),
        };
        // An empty field only asks for an address; an entry that can't be used is an error.
        let problem = match &form.delivery {
            Err(DeliveryProblem::Receiver(problem)) => Some(
                app_muted_text(problem.clone())
                    .debug_selector(|| "swap-receiver-problem".into())
                    .text_xs()
                    .whitespace_normal()
                    .when(!form.receiver_value.trim().is_empty(), |line| {
                        line.text_color(cx.theme().danger)
                    }),
            ),
            _ => None,
        };
        // The SpokePool unwraps WETH only for receivers without code.
        let unwrapped = match form.bridge_state() {
            BridgeState::Ready {
                destination,
                provider: BridgeProvider::Across,
                ..
            } if self.across_delivers_native(form, destination.destination_token, cx) => {
                Some(form_note(
                    IconName::Info,
                    "Across delivers ETH to wallets. Contract receivers get WETH.",
                ))
            }
            _ => None,
        };
        let native = form.native_output.then(|| {
            div()
                .debug_selector(|| "swap-native-payout-note".into())
                .flex()
                .items_center()
                .gap_1()
                .child(
                    Icon::new(IconName::Info)
                        .xsmall()
                        .flex_none()
                        .text_color(rgb(theme::TEXT_MUTED)),
                )
                .child(
                    app_muted_text(self.native_payout_note(cx))
                        .text_xs()
                        .whitespace_normal(),
                )
        });
        rows.child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(labeled_row("Receiver", picker))
                .child(
                    // Under the field, past the label and the row's gap.
                    div()
                        .pl(rems(ACCOUNT_LABEL_WIDTH + 0.5))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(matched.map(|(label, own)| {
                            app_muted_text(if own {
                                format!("Public account · {label}")
                            } else {
                                format!("Address book · {label}")
                            })
                            .text_xs()
                            .truncate()
                        }))
                        .children(problem)
                        .children(native)
                        .children(unwrapped),
                ),
        )
        .when(form.network.is_some(), |rows| {
            rows.children(self.render_bridge_rows(form, editable && !locked, cx))
        })
    }

    /// Whether Across pays `token` on the form's network as ETH: WETH on Ethereum or Arbitrum
    /// One reaches a receiver without code unwrapped.
    fn across_delivers_native(&self, form: &SwapForm, token: Address, cx: &App) -> bool {
        self.destination_chain(form, cx)
            .and_then(|chain| across_unwrapped_token(&chain))
            == Some(token)
    }

    /// The token a Bridge `delivery` pays out, as swaps name it: the native asset for the
    /// wrapped native token Across unwraps.
    pub(super) fn bridge_received_token(&self, delivery: BridgeDelivery, cx: &App) -> Address {
        let unwrapped = delivery.provider == BridgeProvider::Across
            && self.root.upgrade().and_then(|root| {
                root.read(cx)
                    .effective_chain_configs
                    .get(delivery.destination_chain)
                    .and_then(across_unwrapped_token)
            }) == Some(delivery.destination_token);
        if unwrapped {
            Address::ZERO
        } else {
            delivery.destination_token
        }
    }

    /// A Bridge swap's Provider row, with the provider's refusal of the amount, why it was
    /// switched, and its disclaimer beneath, then Across's Surplus row.
    fn render_bridge_rows(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Vec<gpui::Div> {
        let state = form.bridge_state();
        let placeholder = match state {
            BridgeState::SameToken => "No provider for this pair",
            BridgeState::Loading => "Loading routes…",
            _ => "Choose a token to receive",
        };
        let select = Select::new(&form.provider_select)
            .w_full()
            .placeholder(placeholder)
            .disabled(!editable || !matches!(state, BridgeState::Ready { .. }));
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        if let BridgeState::Ready {
            destination,
            provider,
            switched,
            ..
        } = state
        {
            if let QuoteState::Failed(error) = &form.quote
                && let Some(rejection) = bridge_rejection(error)
            {
                lines.push(
                    div()
                        .debug_selector(|| "swap-provider-error".into())
                        .flex()
                        .items_start()
                        .gap_1()
                        .child(
                            Icon::new(IconName::CircleX)
                                .xsmall()
                                .flex_none()
                                .text_color(cx.theme().danger),
                        )
                        .child(
                            app_text(rejection)
                                .text_xs()
                                .min_w_0()
                                .text_color(cx.theme().danger)
                                .whitespace_normal(),
                        ),
                );
            }
            if switched {
                lines.push(
                    app_muted_text(format!(
                        "Switched to NEAR Intents: Across doesn't deliver {}.",
                        destination.symbol
                    ))
                    .debug_selector(|| "swap-provider-switched".into())
                    .text_xs()
                    .whitespace_normal(),
                );
            }
            let disclaimer = match provider {
                BridgeProvider::Across => format!(
                    "If the deposit isn't filled before it expires, Across refunds it to the stealth account on {}, usually within a few hours. Recovering it costs a shield fee and a broadcaster fee.",
                    self.chain_label()
                ),
                BridgeProvider::NearIntents => NEAR_INTENTS_DISCLAIMER.to_owned(),
            };
            lines.push(
                form_note(IconName::Info, disclaimer)
                    .debug_selector(|| "swap-provider-disclaimer".into()),
            );
            if provider == BridgeProvider::Across {
                rows.push(self.render_surplus_row(form, destination.intermediate, editable, cx));
            } else {
                lines.push(
                    app_muted_text(format!(
                        "Via {} on {}. The whole payout is converted, surplus included.",
                        self.token_symbol(destination.intermediate, cx),
                        self.chain_label()
                    ))
                    .text_xs()
                    .whitespace_normal(),
                );
            }
        }
        let provider = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(labeled_row("Provider", select))
            .when(!lines.is_empty(), |row| {
                row.child(
                    // Under the select, past the label and the row's gap.
                    div()
                        .pl(rems(ACCOUNT_LABEL_WIDTH + 0.5))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(lines),
                )
            });
        rows.insert(0, provider);
        rows
    }

    /// Across's choice for what `CoW` pays above the deposit: reshield it, or leave it in the
    /// stealth account.
    fn render_surplus_row(
        &self,
        form: &SwapForm,
        bought: Address,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let disabled = !editable || self.is_retry(form);
        let choice = |id: &'static str, label: String, surplus: BridgeSurplus| {
            app_segment_button(id, label, form.bridge.surplus == surplus, disabled, None).on_click(
                cx.listener(move |this, _, window, cx| {
                    this.set_form_surplus(surplus, window, cx);
                }),
            )
        };
        // Bridge orders buy exactly the deposit, the quote's minimum.
        let deposit = match &form.quote {
            QuoteState::Ready(review) if review.bridge().is_some() => format!(
                "the {} deposit",
                self.token_amount(bought, review.suggested_private_minimum(), cx)
            ),
            _ => "the deposit".to_owned(),
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(labeled_row(
                "Surplus",
                div().flex().child(
                    ButtonGroup::new("swap-bridge-surplus")
                        .outline()
                        .compact()
                        .disabled(disabled)
                        .child(choice(
                            "swap-surplus-reshield",
                            format!("Reshield on {}", self.chain_label()),
                            BridgeSurplus::Reshield,
                        ))
                        .child(choice(
                            "swap-surplus-keep",
                            "Keep in stealth account".to_owned(),
                            BridgeSurplus::KeepInAccount,
                        )),
                ),
            ))
            .child(
                div().pl(rems(ACCOUNT_LABEL_WIDTH + 0.5)).child(
                    app_muted_text(format!(
                        "Anything CoW pays above {deposit}. Reshielding costs the shield fee."
                    ))
                    .text_xs()
                    .whitespace_normal(),
                ),
            )
    }

    /// `GPv2` pays a native buy with a fixed-stipend transfer, which a contract wallet whose
    /// `receive` needs more gas can't accept. The wrapped token has no such limit.
    fn native_payout_note(&self, cx: &App) -> String {
        let native = self.token_symbol(Address::ZERO, cx);
        let wrapped = self.wrapped_native_token(cx).map_or_else(
            || "its wrapped token".to_owned(),
            |token| self.token_symbol(token, cx),
        );
        format!(
            "Smart-contract wallets may not accept {native} from CoW. Choose {wrapped} for those."
        )
    }

    /// The stealth account select, the setup broadcaster settings for a new account, and one
    /// line on what the account choice costs or reveals.
    fn render_account_row(
        &self,
        form: &SwapForm,
        mode: FormMode,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let control = match (&form.account_select, form.operation) {
            (Some(select), _) => Select::new(select)
                .w_full()
                .disabled(!editable)
                .into_any_element(),
            (None, Some(operation)) => {
                fixed_account(self.account_label(operation).unwrap_or_default()).into_any_element()
            }
            (None, None) => div().into_any_element(),
        };
        // The line under the select is secondary to it, so it's smaller.
        let line = match mode {
            FormMode::SettingUp => self.render_setting_up_section(form),
            FormMode::Placed => app_muted_text(
                "This swap has an order. Follow it on the Private tab until it ends.",
            )
            .text_xs()
            .whitespace_normal(),
            FormMode::Setup { .. } => app_muted_text(self.setup_line(form, cx))
                .text_xs()
                .whitespace_normal(),
            FormMode::Order if form.reuse_account => self.render_reuse_warning(form),
            FormMode::Order => {
                app_muted_text("Already set up for this swap. No setup fee.").text_xs()
            }
        };
        let select = div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().min_w_0().child(control))
            .when(matches!(mode, FormMode::Setup { .. }), |row| {
                row.child(self.render_setup_settings(form, cx))
            });
        // The setting-up section spans the form; any other line sits under the select.
        let (column, section) = if mode == FormMode::SettingUp {
            (select, Some(line))
        } else {
            (
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(select)
                    .child(line),
                None,
            )
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                // Like `labeled_row`, but the label lines up with the select, not the line.
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_start()
                    .gap_2()
                    .child(
                        app_muted_text("Stealth account")
                            .w(rems(ACCOUNT_LABEL_WIDTH))
                            .h_8()
                            .flex_none()
                            .flex()
                            .items_center(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(rems(ROW_CONTROL_MIN_WIDTH))
                            .child(column),
                    ),
            )
            .children(section)
    }

    /// The reuse warning; its tooltip holds the full privacy warning the review repeats.
    fn render_reuse_warning(&self, form: &SwapForm) -> gpui::Div {
        let account = form
            .operation
            .and_then(|operation| self.record(operation))
            .map_or_else(
                || "this account".to_owned(),
                |record| format!("#{}", record.index()),
            );
        div().child(
            div()
                .id("swap-account-reuse")
                .child(warning_line(
                    format!("Links this swap to {account}'s earlier activity. No setup fee."),
                    true,
                ))
                .tooltip(|window, cx| Tooltip::new(ACCOUNT_REUSE_NOTE).build(window, cx)),
        )
    }

    /// The gear that opens the setup broadcaster settings: fee token, Random or Specific
    /// broadcaster, favorites only, and out-of-range fees.
    fn render_setup_settings(&self, form: &SwapForm, cx: &Context<'_, Self>) -> Popover {
        let view = cx.entity();
        let open_view = view.clone();
        let choice = form.route.choice();
        let route = SetupRouteSettings {
            fee_options: form.route.fee_options.clone(),
            fee_token: form.route.fee_token.unwrap_or_default(),
            allow_out_of_range: form.route.allow_out_of_range,
            favorites_only: form.route.favorites_only,
            random_selected: form.route.selected.is_none(),
            specific_label: selected_broadcaster_label(&choice, &form.route.candidates),
            candidate_count: form.route.candidates.len(),
            busy: self.busy(),
        };
        Popover::new("swap-setup-settings")
            .anchor(Anchor::TopRight)
            // Presses elsewhere in the form close it; see `render_form`.
            .overlay_closable(false)
            .open(form.settings_open)
            .on_open_change(move |open, _, cx| {
                open_view.update(cx, |view, cx| view.set_setup_settings_open(*open, cx));
            })
            .trigger(
                app_button_base("swap-setup-settings-trigger")
                    .ghost()
                    .icon(IconName::Settings)
                    .accessibility_label("Setup broadcaster")
                    .tooltip("Setup broadcaster"),
            )
            .content(move |_, _, _| setup_route_settings(view.clone(), &route).w(rems(24.)))
    }

    /// The rate and total costs; expanded, the costs taken from the output, the minimum the
    /// review approves, slippage, and the order's validity.
    fn render_details(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        with_setup: bool,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Collapsible {
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let costs = total_cost(review);
        let open = form.details_open;
        let toggle = if open { "Hide details" } else { "Show details" };
        let details = Collapsible::new()
            .open(open)
            .w_full()
            .min_w_0()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .child(
                app_button_base("swap-details-toggle")
                    .ghost()
                    .w_full()
                    .min_w_0()
                    .h_auto()
                    .min_h_8()
                    .px_0()
                    .py_1()
                    .accessibility_label(toggle)
                    .tooltip(toggle)
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                app_text(self.rate_label(review, cx))
                                    .flex_1()
                                    .min_w_0()
                                    .truncate(),
                            )
                            .child(
                                app_muted_text(self.with_usd(
                                    format!("≈ {} in costs", self.token_amount(buy, costs, cx)),
                                    buy,
                                    costs,
                                    cx,
                                ))
                                .flex_none(),
                            )
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronUp
                                } else {
                                    IconName::ChevronDown
                                })
                                .xsmall()
                                .flex_none(),
                            ),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_details(cx))),
            );
        if !open {
            return details;
        }
        let hook_cost = review.estimated_hook_cost();
        let cow_fee = cow_fee(review);
        let unshield_fee = plan.amount().saturating_sub(review.sell_amount());
        let minutes = self
            .swap_profile(cx)
            .map_or(10, |profile| profile.valid_to_window().as_secs() / 60);
        let minimum = review.suggested_private_minimum();
        let railgun_fees = match plan.delivery() {
            // No shield: only the unshield fee applies.
            SwapDelivery::External { .. }
            | SwapDelivery::Bridge(BridgeDelivery {
                surplus: BridgeSurplus::KeepInAccount | BridgeSurplus::BridgedByProvider,
                ..
            }) => detail_row(
                "Railgun fee",
                app_text(self.with_usd(
                    format!("{} unshield", self.token_amount(sell, unshield_fee, cx)),
                    sell,
                    unshield_fee,
                    cx,
                )),
                true,
                Some(railgun_fees_label(review)),
            ),
            SwapDelivery::Reshield | SwapDelivery::Bridge(_) => {
                let shield_fee = shield_fee_on(review, review.estimated_buy_amount());
                // Two tokens, so one USD total, or none unless both have a rate.
                let railgun_fees_usd = self
                    .usd_micro_value(sell, unshield_fee, cx)
                    .zip(self.usd_micro_value(buy, shield_fee, cx))
                    .map(|(unshield, shield)| unshield.saturating_add(shield));
                detail_row(
                    "Railgun fees",
                    app_text(format_value_with_usd_label(
                        format!(
                            "{} + {}",
                            self.token_amount(sell, unshield_fee, cx),
                            self.token_amount(buy, shield_fee, cx)
                        ),
                        U256::ZERO,
                        // Without decimals the helper skips its stablecoin check, which doesn't
                        // apply to a total across two tokens.
                        None,
                        railgun_fees_usd,
                        false,
                    )),
                    true,
                    Some(railgun_fees_label(review)),
                )
            }
        };
        let content = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(detail_row(
                "CoW network fee",
                app_text(self.with_usd(
                    format!("≈ {}", self.token_amount(buy, cow_fee, cx)),
                    buy,
                    cow_fee,
                    cx,
                )),
                true,
                Some("CoW's quoted fee, already included in the quote.".into()),
            ))
            .child(detail_row(
                "Hook gas",
                app_text(self.with_usd(
                    format!("≈ {}", self.token_amount(buy, hook_cost, cx)),
                    buy,
                    hook_cost,
                    cx,
                )),
                true,
                Some(format!(
                    "Estimated at the current network gas price. The minimum allows for {} using conservative gas usage and a 25% gas-price cushion. Actual fees may differ.",
                    self.token_amount(buy, review.hook_cost(), cx)
                )),
            ))
            .child(railgun_fees)
            .children(self.bridge_detail_rows(review, cx))
            .when(
                !matches!(plan.delivery(), SwapDelivery::Bridge(_)),
                |content| {
                    content.child(detail_row(
                        "Receive at least",
                        app_text(self.with_usd(
                            self.token_amount(buy, minimum, cx),
                            buy,
                            minimum,
                            cx,
                        )),
                        false,
                        Some("The minimum you approve in the review, after every fee.".into()),
                    ))
                },
            )
            .child(detail_row(
                "Slippage",
                Self::render_slippage(form, editable, cx),
                false,
                None,
            ))
            .child(detail_row(
                "Order valid for",
                app_text(if with_setup {
                    format!("{minutes} minutes after setup")
                } else {
                    format!("{minutes} minutes")
                }),
                false,
                None,
            ));
        details.content(content)
    }

    /// A Bridge quote's Bridge fee, in the bought token, and what the receiver gets on the
    /// destination network in place of Receive at least: Across's exact output, or NEAR
    /// Intents' minimum. Other quotes have neither.
    fn bridge_detail_rows(&self, review: &SwapReview, cx: &App) -> Vec<gpui::Div> {
        let (SwapDelivery::Bridge(delivery), Some(bridge)) =
            (review.plan().delivery(), review.bridge())
        else {
            return Vec::new();
        };
        let buy = review.plan().buy_token();
        let provider = provider_name(delivery.provider);
        let receive = self.network_token_amount(
            delivery.destination_chain,
            self.bridge_received_token(delivery, cx),
            bridge.destination_minimum,
            cx,
        );
        let mut rows = Vec::new();
        rows.extend(bridge.fee.map(|fee| {
            detail_row(
                "Bridge fee",
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(app_text(self.with_usd(
                        self.token_amount(buy, fee, cx),
                        buy,
                        fee,
                        cx,
                    )))
                    .child(app_muted_text(provider)),
                true,
                Some(format!(
                    "{provider}'s fee, already out of the amount below."
                )),
            )
        }));
        rows.push(detail_row(
            format!("Receive on {}", network_name(delivery.destination_chain)),
            match delivery.provider {
                BridgeProvider::Across => div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(app_text(receive))
                    .child(app_muted_text("exactly")),
                BridgeProvider::NearIntents => app_text(format!("at least {receive}")),
            },
            false,
            Some("The minimum you approve in the review, after every fee.".into()),
        ));
        rows
    }

    /// Today's slippage presets in a popover. Choosing another preset quotes the swap again. The
    /// quote details, this popover among them, hide until a new quote is ready.
    fn render_slippage(form: &SwapForm, editable: bool, cx: &Context<'_, Self>) -> Popover {
        let view = cx.entity();
        let selected = form.slippage_bps;
        Popover::new("swap-slippage")
            .anchor(Anchor::TopRight)
            .trigger(
                app_button_base("swap-slippage-trigger")
                    .ghost()
                    .xsmall()
                    .dropdown_caret(true)
                    .accessibility_label("Slippage")
                    .child(app_button_label(format_bps_percent(u64::from(selected)))),
            )
            .content(move |_, _, _| {
                div()
                    .w(rems(16.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(app_strong_text("Slippage"))
                    .child(
                        ButtonGroup::new("swap-slippage-presets")
                            .outline()
                            .compact()
                            .children(SLIPPAGE_CHOICES.into_iter().map(|(bps, label)| {
                                let view = view.clone();
                                app_segment_button(
                                    SharedString::from(format!("swap-slippage-{bps}")),
                                    label,
                                    selected == bps,
                                    !editable,
                                    None,
                                )
                                .on_click(move |_, window, cx| {
                                    view.update(cx, |view, cx| view.set_slippage(bps, window, cx));
                                })
                            })),
                    )
                    .child(
                        app_muted_text(
                            "Lower slippage fills less often. An unfilled order costs only the setup.",
                        )
                        .whitespace_normal(),
                    )
            })
    }

    /// Show what keeps notes out of this swap, above the form.
    fn open_locked_notes(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        crate::root::locked_notes::open_locked_notes_dialog(
            self.root.clone(),
            Arc::clone(&self.session),
            self.runtime.clone(),
            window,
            cx,
        );
    }

    /// The amount-reduction prompt next to the amount it describes.
    fn render_too_large(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let QuoteState::TooLarge { largest } = form.quote else {
            return None;
        };
        let entered = self.form_amount(form, cx).map_or_else(
            |_| "This amount".into(),
            |amount| self.token_amount(form.sell, amount, cx),
        );
        let largest_label = self.token_amount(form.sell, largest, cx);
        Some(
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    app_text(format!(
                        "{entered} is spread across more notes than one swap can spend. Up to {largest_label} fits."
                    ))
                    .flex_1()
                    .min_w_0()
                    .text_color(rgb(theme::DANGER))
                    .whitespace_normal(),
                )
                .when(!largest.is_zero(), |row| {
                    row.child(
                        app_button("swap-use-largest", format!("Use {largest_label}"))
                            .small()
                            .flex_none()
                            .disabled(!editable)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.use_amount(largest, window, cx);
                            })),
                    )
                }),
        )
    }

    fn render_setting_up_section(&self, form: &SwapForm) -> gpui::Div {
        let tracking = form
            .operation
            .and_then(|operation| self.tracking.get(&operation));
        let stage = tracking
            .and_then(|tracking| tracking.setup_stage.as_ref())
            .map(|receiver| *receiver.borrow());
        let status = stage.map_or(
            "Waiting for the setup to be confirmed",
            TransactionGenerationStage::label,
        );
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(app_text(format!("Setting up the swap's stealth account · {status}"))),
            )
            .children(
                tracking
                    .and_then(|tracking| tracking.error.clone())
                    .map(|error| {
                        app_muted_text(error)
                            .text_color(rgb(theme::DANGER))
                            .whitespace_normal()
                    }),
            )
            .child(
                app_muted_text(
                    "Confirmation takes a few minutes. You can close this; the swap stays on the Private tab, and its order is placed once the setup is confirmed.",
                )
                .whitespace_normal(),
            )
    }
}

fn select_index(
    items: &[PrivateActionAssetSelectItem],
    token: Address,
) -> Option<gpui_component::IndexPath> {
    items
        .iter()
        .position(|item| item.token == token)
        .map(|index| gpui_component::IndexPath::default().row(index))
}

fn buy_index(items: &[SwapBuyItem], token: Address) -> Option<gpui_component::IndexPath> {
    items
        .iter()
        .position(|item| item.asset.token == token)
        .map(|index| gpui_component::IndexPath::default().row(index))
}

/// Whether two setup route candidate lists show the same: the same offers, in order, with the
/// labels and fee checks the form and the broadcaster picker show.
fn same_shown_candidates(
    left: &[PublicBroadcasterCandidate],
    right: &[PublicBroadcasterCandidate],
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            same_offer(left, right)
                && left.identifier == right.identifier
                && left.version == right.version
                && left.fee_policy_status == right.fee_policy_status
        })
}

fn same_chain_buy_items(items: Vec<PrivateActionAssetSelectItem>) -> Vec<SwapBuyItem> {
    items
        .into_iter()
        .map(|asset| SwapBuyItem {
            asset,
            near_only: false,
        })
        .collect()
}

fn destination_for(list: &[BridgeDestination], token: Address) -> Option<&BridgeDestination> {
    list.iter()
        .find(|destination| destination.destination_token == token)
}

/// The wrapped native token Across delivers on `chain` as the native asset: the `SpokePool`
/// unwraps WETH on Ethereum and Arbitrum One for a receiver without code.
fn across_unwrapped_token(chain: &EffectiveChainConfig) -> Option<Address> {
    chain
        .wrapped_native_token
        .filter(|_| matches!(chain.chain_id, 1 | 42161))
}

/// The surplus choice a provider takes: Across's `chosen` one; NEAR Intents converts it all.
const fn bridge_surplus(provider: BridgeProvider, chosen: BridgeSurplus) -> BridgeSurplus {
    match provider {
        BridgeProvider::Across => chosen,
        BridgeProvider::NearIntents => BridgeSurplus::BridgedByProvider,
    }
}

pub(super) fn network_name(chain_id: u64) -> String {
    railgun_ui::chain_name(chain_id).map_or_else(|| chain_id.to_string(), str::to_owned)
}

/// The token a Bridge review delivers on its network. Only a Bridge review has one.
const fn review_destination_token(review: &SwapReview) -> Address {
    match review.plan().delivery() {
        SwapDelivery::Bridge(delivery) => delivery.destination_token,
        _ => review.plan().buy_token(),
    }
}

/// Why a Bridge receiver can't be used on `network`.
fn bridge_receiver_message(rejection: BridgeReceiverRejection, network: &str) -> String {
    match rejection {
        BridgeReceiverRejection::ZeroAddress => {
            "Tokens sent to the zero address are lost. Enter the receiver's address.".to_owned()
        }
        BridgeReceiverRejection::Railgun => format!(
            "This is the Railgun contract on {network}. Tokens sent to it directly can't be recovered."
        ),
        BridgeReceiverRejection::SpokePool => format!(
            "This is the Across SpokePool on {network}. Tokens sent to it directly can't be recovered."
        ),
    }
}

/// A provider's refusal to quote the amount, for the Provider row. 1Click states its route
/// minimum in its message, which is shown as sent.
fn bridge_rejection(error: &eyre::Report) -> Option<String> {
    match error.downcast_ref::<BridgeApiError>()? {
        BridgeApiError::AmountTooLow => {
            Some("The amount is below Across's minimum for this route.".to_owned())
        }
        BridgeApiError::Rejected { api, message, .. } if message.is_empty() => {
            Some(format!("{api} rejected this amount."))
        }
        BridgeApiError::Rejected { api, message, .. } => Some(format!("{api}: {message}")),
        _ => None,
    }
}

/// A provider that couldn't be reached on the swap's route. Retry asks on a fresh one.
fn bridge_unreachable(error: &eyre::Report) -> Option<String> {
    match error.downcast_ref::<BridgeApiError>()? {
        BridgeApiError::RateLimited { api } => Some(format!(
            "{api} is limiting requests right now. Retry in a minute."
        )),
        BridgeApiError::Unavailable { api, .. }
        | BridgeApiError::Timeout { api, .. }
        | BridgeApiError::Transport { api, .. } => Some(format!(
            "Couldn't reach {api} through the swap's network route."
        )),
        _ => None,
    }
}

/// What the setup broadcaster popover shows, captured when the form renders.
struct SetupRouteSettings {
    fee_options: Vec<PublicBroadcasterFeeTokenOption>,
    fee_token: Address,
    allow_out_of_range: bool,
    favorites_only: bool,
    random_selected: bool,
    specific_label: String,
    candidate_count: usize,
    busy: bool,
}

/// The shared broadcaster settings with the fee token selector, driving the swap's setup route.
/// Specific broadcaster opens the broadcaster picker on top of the swap dialog.
fn setup_route_settings(
    view: Entity<PrivateSwapsView>,
    route: &SetupRouteSettings,
) -> gpui::Stateful<gpui::Div> {
    use ui::private_action::BroadcasterSettingsEvent as Event;
    let fee_view = view.clone();
    let fee_token = fee_token_selector(
        "swap-setup-fee-token".into(),
        &route.fee_options,
        route.fee_token,
        route.busy,
        move |token, _, cx| {
            fee_view.update(cx, |view, cx| {
                if let Some(form) = view.form.as_mut() {
                    form.route.fee_token = Some(token);
                    form.route.invalidate_estimate();
                }
                view.refresh_setup_route(cx);
            });
        },
    );
    ui::private_action::broadcaster_settings_fields(
        "swap-setup-broadcaster-settings",
        ui::private_action::BroadcasterSettings {
            allow_out_of_range: route.allow_out_of_range,
            favorites_only: route.favorites_only,
            random_selected: route.random_selected,
            specific_label: route.specific_label.clone(),
            candidate_count: route.candidate_count,
            disabled: route.busy,
        },
        fee_token,
        None,
        move |event, window, cx| {
            view.update(cx, |view, cx| {
                if matches!(event, Event::ChooseSpecific) {
                    view.choose_specific_setup_broadcaster(window, cx);
                    return;
                }
                let Some(form) = view.form.as_mut() else {
                    return;
                };
                match event {
                    Event::Random => form.route.selected = None,
                    Event::AllowOutOfRange(value) => form.route.allow_out_of_range = value,
                    Event::FavoritesOnly(value) => form.route.favorites_only = value,
                    Event::ChooseSpecific => {}
                }
                form.route.invalidate_estimate();
                view.refresh_setup_route(cx);
            });
        },
    )
}

/// The quote against the anchor price, such as "(−0.4% vs Chainlink)", and the reading behind
/// it. Chainlink is named only when every anchor reading came from a Chainlink round.
fn price_delta(review: &SwapReview, cx: &App) -> Option<(gpui::Div, Option<String>)> {
    let SwapPrice::Verified { rate, observations } = review.price() else {
        return None;
    };
    let expected = if rate.sell_rate.is_zero() {
        U256::ZERO
    } else {
        review.quote().sell_amount.saturating_mul(rate.buy_rate) / rate.sell_rate
    };
    let anchor = if !observations.is_empty()
        && observations
            .iter()
            .all(|observation| observation.updated_at.is_some())
    {
        "Chainlink"
    } else {
        "the anchor price"
    };
    let delta = quote_anchor_delta_bps(review.quote().buy_amount, expected).map_or_else(
        || app_muted_text(format!("(within range of {anchor})")),
        |delta| {
            let color = match delta.cmp(&0) {
                std::cmp::Ordering::Less => cx.theme().danger,
                std::cmp::Ordering::Equal => cx.theme().muted_foreground,
                std::cmp::Ordering::Greater => cx.theme().success,
            };
            div()
                .flex()
                .items_baseline()
                .child(app_muted_text("("))
                .child(
                    app_text(format!(
                        "{}{}",
                        if delta < 0 { "−" } else { "+" },
                        format_bps_percent(delta.unsigned_abs())
                    ))
                    .text_color(color),
                )
                .child(app_muted_text(format!(" vs {anchor})")))
        },
    );
    let checked = observations
        .iter()
        .map(|observation| observation.block.number)
        .max()
        .map(|block| format!("Checked against {anchor} at block {block}"));
    Some((delta, checked))
}

const fn source_return_note(review: &SwapReview) -> &'static str {
    match review.plan().delivery() {
        SwapDelivery::Bridge(BridgeDelivery {
            surplus: BridgeSurplus::Reshield,
            ..
        }) => {
            "To your private balance, after estimated gas and the shield fee. Surplus may be zero."
        }
        _ => "Kept in your stealth account, after estimated gas. Surplus may be zero.",
    }
}

/// Estimated swap fees, including hook gas, at the quoted trading rate. Setup is separate.
fn total_cost(review: &SwapReview) -> U256 {
    swap_fees_to(review, expected_output(review))
}

/// The known fees plus the hook gas cap, at the quoted trading rate.
fn worst_case_cost(review: &SwapReview) -> U256 {
    swap_fees_to(review, worst_case_output(review))
}

/// The sell-token fees and every deduction from the quote down to `received`.
fn swap_fees_to(review: &SwapReview, received: U256) -> U256 {
    let unshield_fee = review.plan().amount().saturating_sub(review.sell_amount());
    swap_total_cost(
        unshield_fee.saturating_add(review.quote().fee_amount),
        review.quote().sell_amount,
        review.quote().buy_amount,
        received,
    )
}

/// `CoW`'s quoted fee in buy-token base units. The quote already includes it.
fn cow_fee(review: &SwapReview) -> U256 {
    swap_total_cost(
        review.quote().fee_amount,
        review.quote().sell_amount,
        review.quote().buy_amount,
        review.quote().buy_amount,
    )
}

/// Warns on the worst case, with the hook gas cap counted as a cost.
fn high_cost_bps(review: &SwapReview) -> Option<u64> {
    let bps = swap_cost_bps(worst_case_cost(review), worst_case_output(review));
    (bps >= 1_000).then_some(bps)
}

fn high_cost_message(bps: u64) -> String {
    format!(
        "Swap costs could reach {} of the swap amount, counting the hook gas allowance and gas-price cushion.",
        format_bps_percent(bps)
    )
}

/// Expected value retained in buy-token units after gas, shield and bridge fees.
/// Across includes both the destination payment and source-chain return.
fn expected_output(review: &SwapReview) -> U256 {
    let quoted = review.estimated_buy_amount();
    quoted
        .saturating_sub(shield_fee_on(review, quoted))
        .saturating_sub(bridge_fee(review))
}

/// The private amount at the quoted price if hook gas reaches its estimate: the quote less the
/// hook cost, Railgun's shield fee and any bridge fee.
fn worst_case_output(review: &SwapReview) -> U256 {
    let after_hooks = review.quote().buy_amount.saturating_sub(review.hook_cost());
    after_hooks
        .saturating_sub(shield_fee_on(review, after_hooks))
        .saturating_sub(bridge_fee(review))
}

/// The bridge fee in buy-token base units, zero without a bridge or a priced leg.
fn bridge_fee(review: &SwapReview) -> U256 {
    review
        .bridge()
        .and_then(|bridge| bridge.fee)
        .unwrap_or_default()
}

/// Railgun's shield fee on a buy-token amount.
fn shield_fee_on(review: &SwapReview, amount: U256) -> U256 {
    review.shield_fee_on_output(amount)
}

/// A rounded Sell or Buy panel, raised above the dialog. A danger border marks an
/// amount that doesn't fit one swap.
fn amount_panel(danger: bool, cx: &App) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2p5()
        .rounded_lg()
        .border_1()
        .border_color(if danger {
            cx.theme().danger
        } else {
            cx.theme().border
        })
        .bg(cx.theme().group_box)
}

/// A form row with its label in the stealth account row's label column. In a narrow dialog
/// the control wraps under the label.
fn labeled_row(label: &'static str, control: impl IntoElement) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            app_muted_text(label)
                .w(rems(ACCOUNT_LABEL_WIDTH))
                .flex_none(),
        )
        .child(
            div()
                .flex_1()
                .min_w(rems(ROW_CONTROL_MIN_WIDTH))
                .child(control),
        )
}

/// A panel's token select, at its trailing edge.
fn token_pill(select: impl IntoElement) -> gpui::Div {
    div().w(rems(9.)).flex_none().child(select)
}

/// A started swap's own stealth account, which the form can't change.
fn fixed_account(label: String) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .h_8()
        .px_3()
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .child(
            app_muted_text(label)
                .min_w_0()
                .truncate()
                .font_family(theme::APP_MONO_FONT_FAMILY),
        )
}

/// A warning in the warning color. A `small` one is a secondary line, such as the one under the
/// stealth account select.
fn warning_line(text: impl Into<SharedString>, small: bool) -> gpui::Div {
    let icon = Icon::new(IconName::TriangleAlert).text_color(rgb(theme::WARNING));
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(if small { icon.xsmall() } else { icon.small() })
        .child(
            app_text(text)
                .when(small, gpui::Styled::text_xs)
                .text_color(rgb(theme::WARNING))
                .whitespace_normal(),
        )
}

/// A panel's balance: secondary to the amount beside it, so muted and smaller.
fn balance_text(text: impl Into<SharedString>) -> gpui::Div {
    app_muted_text(text).text_xs()
}

/// A secondary line under a form row, led by `icon`, such as a provider's disclaimer.
fn form_note(icon: IconName, text: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap_1()
        .child(
            Icon::new(icon)
                .xsmall()
                .flex_none()
                .text_color(rgb(theme::TEXT_MUTED)),
        )
        .child(app_muted_text(text).text_xs().min_w_0().whitespace_normal())
}

/// What the receive strip's amount already accounts for, for its tooltip and pinned popover.
fn receive_help_card(help: &'static str, window: &Window) -> gpui::Div {
    ui::hint::hint_card("You receive", theme::INFO, window).child(div().child(help))
}

/// One details row. Indented rows are the costs taken from the output; `help` explains the
/// value in a tooltip.
fn detail_row(
    label: impl Into<SharedString>,
    value: impl IntoElement,
    indented: bool,
    help: Option<String>,
) -> gpui::Div {
    let label = label.into();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .when(indented, gpui::Styled::pl_4)
        .child(
            div()
                .id(label.clone())
                .flex()
                .items_center()
                .gap_1()
                .child(app_muted_text(label))
                .when_some(help, |label, help| {
                    label
                        .child(
                            Icon::new(IconName::Info)
                                .xsmall()
                                .text_color(rgb(theme::TEXT_MUTED)),
                        )
                        .tooltip(move |window, cx| Tooltip::new(help.clone()).build(window, cx))
                }),
        )
        .child(div().flex_none().child(value))
}

fn railgun_fees_label(review: &SwapReview) -> String {
    let unshield = format_bps_percent(u64::try_from(review.unshield_fee_bps()).unwrap_or(u64::MAX));
    match review.plan().delivery() {
        // Across can reshield what CoW pays above the deposit.
        SwapDelivery::Bridge(BridgeDelivery {
            surplus: BridgeSurplus::Reshield,
            ..
        }) => format!("{unshield} unshield, shield on surplus"),
        // Other External and Bridge orders carry no shield of the bought amount.
        SwapDelivery::External { .. } | SwapDelivery::Bridge(_) => format!("{unshield} unshield"),
        SwapDelivery::Reshield => format!(
            "{unshield} unshield, {} shield",
            format_bps_percent(u64::try_from(review.shield_fee_bps()).unwrap_or(u64::MAX))
        ),
    }
}

/// Why the form can't use a receiver, under the Receiver field.
const fn receiver_rejection_message(rejection: SwapReceiverRejection) -> &'static str {
    match rejection {
        // CoW reads a zero receiver as the order's owner, the stealth account.
        SwapReceiverRejection::ZeroAddress => {
            "The zero address would pay the stealth account making the swap. Enter the receiver's address."
        }
        SwapReceiverRejection::Executor => {
            "This is the stealth account making the swap. Choose Private balance to keep the tokens."
        }
        SwapReceiverRejection::Railgun => {
            "This is the Railgun contract. Tokens sent to it directly can't be recovered."
        }
        SwapReceiverRejection::Settlement
        | SwapReceiverRejection::VaultRelayer
        | SwapReceiverRejection::HooksTrampoline => {
            "This is a CoW Protocol contract. Tokens sent to it directly can't be recovered."
        }
    }
}

const fn review_change_label(change: SwapReviewChange) -> &'static str {
    match change {
        SwapReviewChange::Delivery => "the delivery or receiver changed",
        SwapReviewChange::HookCost => "the network and hook limit needs review",
        SwapReviewChange::ShieldFee { .. } => "the Railgun shield fee changed",
        SwapReviewChange::UnshieldFee { .. } => "the Railgun unshield fee changed",
        SwapReviewChange::QuoteDeviates => "the quote now deviates from the anchor price",
        SwapReviewChange::PriceVerification => "the price check changed",
        SwapReviewChange::PriceUnavailable => "the independent price is unavailable",
        SwapReviewChange::Minimum { .. } => {
            "the current quote no longer supports the minimum you approved"
        }
        SwapReviewChange::DestinationMinimum { .. } => "the bridge quote is below your minimum",
    }
}

/// What a broadcaster's answer leaves for the user to know. `None` when it accepted.
pub(super) fn broadcaster_result_problem(
    result: &PublicBroadcasterResultKind,
    what: &str,
) -> Option<String> {
    match result {
        PublicBroadcasterResultKind::Submitted { .. } => None,
        PublicBroadcasterResultKind::Failed { error } => Some(format!(
            "The broadcaster reported a problem with the {what}: {error}"
        )),
        PublicBroadcasterResultKind::TimedOut => Some(format!(
            "No broadcaster response yet. The {what} stays tracked, and the swap updates when it lands."
        )),
    }
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod ui_tests;
