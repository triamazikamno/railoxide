//! The swap form: tokens, amount, the gas share and price tolerance, the setup's broadcaster
//! route, the quote with its price check, the single review, and placing the approved order.
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
    Anchor, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, ParentElement as _,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    Window, div, prelude::FluentBuilder as _, relative, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Colorize as _, Disableable as _, Icon, IconName, Selectable as _,
    Sizable as _, WindowExt as _,
    alert::Alert,
    button::{Button, ButtonGroup, ButtonVariants as _},
    checkbox::Checkbox,
    collapsible::Collapsible,
    combobox::{Combobox, ComboboxEvent, ComboboxState},
    input::{Enter as InputEnter, InputEvent, InputState},
    popover::Popover,
    select::{Caret, SearchableVec, Select, SelectEvent, SelectItem, SelectState},
    slider::{Slider, SliderEvent, SliderState},
    spinner::Spinner,
    tag::Tag,
    tooltip::Tooltip,
};
use ui::controls::{
    app_amount_input, app_amount_text, app_button, app_button_base, app_button_label, app_input,
    app_muted_text, app_segment_button, app_strong_text, app_text,
};
use ui::hint::hint_card;
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
    cow::{
        CowOrderbookClient, GAS_SHARE_BALANCED_BPS, GAS_SHARE_LOOSE_BPS, GAS_SHARE_TIGHT_BPS,
        OrderLimitError,
    },
    default_public_broadcaster_fee_limit,
    settings::{
        BridgeProfile, BridgeReceiverRejection, EffectiveChainConfig, EffectiveTokenRegistry,
        SwapReceiverRejection, SwapTokenEligibility, SwapTokenRole,
        resolve_effective_chain_rpc_route, swap_destination_tokens,
    },
    vault::{
        BridgeDelivery, BridgeProvider, BridgeSurplus, ExecutorOperationId, ExecutorRecord,
        SwapApproval, SwapApprovedBounds, SwapDelivery,
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
use crate::assets::RailgunActionIcon;
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
    SpendAuthorizationCard, SpendAuthorizationHint, SpendAuthorizationSummary,
    SpendAuthorizationSummaryRow, spend_authorization_recipient_display,
};
use crate::root::stealth_accounts::{RecoveryPickerContext, same_offer};
use crate::root::{
    COST_ESTIMATE_DEBOUNCE, DeliveryFormKind, format_token_amount_ceiling_for_display,
    format_unshield_amount_input, format_value_with_usd_label, native_wrapped_output_labels,
    new_text_input, parse_address,
};

/// The price tolerance on the best case. The persisted field keeps its old name, slippage.
const SLIPPAGE_CHOICES: [(u32, &str); 4] = [(10, "0.1%"), (50, "0.5%"), (100, "1%"), (300, "3%")];
const DEFAULT_SLIPPAGE_BPS: u32 = 50;
/// Order validity choices for Private and Public address delivery, in minutes.
const VALIDITY_MINUTES: [u64; 3] = [10, 30, 60];
/// Authorized costs from this share of the swap need Swap anyway.
const AUTHORIZED_COST_WARNING_BPS: u64 = 2_000;
/// The gas bar's keyboard step, in percent of its length.
const GAS_BAR_STEP: u16 = 5;
/// The gas bar's key context, for its arrow, Home and End bindings.
const GAS_BAR_KEY_CONTEXT: &str = "SwapGasBar";
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
const GAS_HELP_TITLE: &str = "Why pay less than the full gas?";
/// What settling a swap tends to cost, in basis points of the gas estimate. The estimate prices
/// upper-bound hook gas at a cushioned gas price; observed settlements cost 16% to 27% of it.
const REALISTIC_GAS_BPS: u64 = 2_000;
/// A Public address receiver follows Private Unshield's recipient rules and suggestions, and
/// Save adds it to the public address book.
const RECEIVER_RULES: DeliveryFormKind = DeliveryFormKind::Unshield;
const ENTER_RECEIVER: &str = "Enter an address.";
/// Private Unshield's message for an entry that doesn't parse as an address.
const INVALID_RECEIVER: &str = "Enter a valid public EVM recipient address";
const EXTERNAL_DELIVERY_DISCLOSURE: &str = "The order names the receiver, and the settlement pays it in the same transaction that unshields from Railgun, so the receiver and amount are linked to that spend.";
const NEAR_INTENTS_DISCLAIMER: &str = "A bridge operator holds the funds between the deposit and delivery. Refunds depend on the 1Click service and aren't guaranteed.";
/// The steps of a new swap, as its reviews' stepper names them.
const SWAP_STEPS: [&str; 2] = ["Set up stealth account", "Place order"];
const SAME_TOKEN_BRIDGE: &str =
    "Same-token bridging isn't supported. Choose another token to receive, or sell something else.";

/// Pay more of the gas: move the gas bar's knob one step toward "you pay all gas".
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
struct GasBarLeft;

/// Pay less of the gas: move the knob one step toward "solvers pay all gas".
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
struct GasBarRight;

/// Move the knob to the bar's start, the most gas the swap can pay.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
struct GasBarStart;

/// Move the knob to the bar's end, where solvers pay all gas.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui::Action)]
#[action(no_json)]
struct GasBarEnd;

/// Aborts a spawned Tokio task when dropped, as the handle alone detaches it.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Marks that the gas bar's key bindings are installed in this app.
struct GasBarBindings;

impl gpui::Global for GasBarBindings {}

/// Bind the gas bar's keys once per app. `gpui-component`'s Slider has no keyboard handling of
/// its own, so the strip's focus handle takes the arrows, Home and End.
fn ensure_gas_bar_bindings(cx: &mut App) {
    if cx.has_global::<GasBarBindings>() {
        return;
    }
    cx.bind_keys([
        KeyBinding::new("left", GasBarLeft, Some(GAS_BAR_KEY_CONTEXT)),
        KeyBinding::new("right", GasBarRight, Some(GAS_BAR_KEY_CONTEXT)),
        KeyBinding::new("home", GasBarStart, Some(GAS_BAR_KEY_CONTEXT)),
        KeyBinding::new("end", GasBarEnd, Some(GAS_BAR_KEY_CONTEXT)),
    ]);
    cx.set_global(GasBarBindings);
}

/// A gas share preset. Any other share is Custom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GasPreset {
    Tight,
    Balanced,
    Loose,
}

impl GasPreset {
    /// In the bar's order, from "fills most easily" to "most you could get".
    const ALL: [Self; 3] = [Self::Loose, Self::Balanced, Self::Tight];

    /// The preset's share of the gas estimate.
    const fn share_bps(self) -> u16 {
        match self {
            Self::Tight => GAS_SHARE_TIGHT_BPS,
            Self::Balanced => GAS_SHARE_BALANCED_BPS,
            Self::Loose => GAS_SHARE_LOOSE_BPS,
        }
    }

    /// The preset a share names, if it is one of the presets.
    const fn of_share(share_bps: u16) -> Option<Self> {
        match share_bps {
            GAS_SHARE_TIGHT_BPS => Some(Self::Tight),
            GAS_SHARE_BALANCED_BPS => Some(Self::Balanced),
            GAS_SHARE_LOOSE_BPS => Some(Self::Loose),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Tight => "Higher",
            Self::Balanced => "Optimal",
            Self::Loose => "Lower",
        }
    }

    /// The preset's name away from the strip, where "Your guaranteed minimum" doesn't title it.
    const fn summary(self) -> &'static str {
        match self {
            Self::Tight => "Higher minimum",
            Self::Balanced => "Optimal",
            Self::Loose => "Lower minimum",
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Tight => "swap-gas-tight",
            Self::Balanced => "swap-gas-balanced",
            Self::Loose => "swap-gas-loose",
        }
    }
}

/// Order terms a reopened form restores. A term without a record takes the default.
#[derive(Clone, Copy, Default)]
struct SavedTerms {
    slippage_bps: Option<u32>,
    gas_share_bps: Option<u16>,
    valid_for: Option<Duration>,
}

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
                        .flex_none()
                        .text_xs()
                        .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                        .child("NEAR"),
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

/// Why the form has no delivery to review.
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
    /// The provider that couldn't be asked, and why. Its list is empty, and Retry asks again.
    unavailable: Option<(BridgeProvider, eyre::Report)>,
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
    /// The unreachable provider notice's popover. Reloaded routes close it.
    notice_open: bool,
    /// The Provider row's hint popover. Reloaded routes close it.
    provider_hint_open: bool,
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
    /// The provider that answered doesn't deliver the Buy token, and this one couldn't be asked.
    Unreachable(BridgeProvider),
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
    /// The price tolerance on the best case.
    slippage_bps: u32,
    /// The share of the gas estimate the user chose, in basis points. A quote that can't
    /// support it is priced at Tight, and this share is kept, so the next quote tries it again.
    gas_share_bps: u16,
    /// Custom is selected: the knob or the Minimum field set the share, and no preset is.
    gas_custom: bool,
    /// The bar over the gas share: its value is the knob's position, 0 at "you pay all gas".
    gas_slider: Entity<SliderState>,
    /// The Minimum field, synced with the knob.
    gas_minimum_input: Entity<InputState>,
    /// The edit button opened the bar and the Minimum field, and neither it nor a preset has
    /// closed them since.
    gas_minimum_editing: bool,
    /// The bar's focus, which takes the arrow, Home and End keys.
    gas_bar_focus: FocusHandle,
    /// How long the order is valid after signing. Bridge delivery uses the profile's window.
    valid_for: Duration,
    receive_to: ReceiveTo,
    /// The Public address receiver as entered; the input's value is authoritative.
    receiver_input: Entity<InputState>,
    receiver_value: String,
    receiver_suggestions_open: bool,
    receiver_suggestion_index: Option<usize>,
    receiver_suggestions_scroll: ScrollHandle,
    /// The delivery the choices and the receiver give, or why there is none. Checked again
    /// when any of them, or the stealth account, changes. See [`Self::quote_delivery`] for the
    /// delivery a quote is requested for.
    delivery: Result<SwapDelivery, DeliveryProblem>,
    route: SetupRoute,
    assets: FormAssets,
    quote: QuoteState,
    quote_task: Option<Task<()>>,
    quote_revision: u64,
    quote_terms: Option<QuoteTerms>,
    /// A Bridge swap's bridge leg couldn't be quoted again at the strip's gas share. The ready
    /// quote stays, and can't be reviewed until another share or a retry is quoted.
    bridge_quote_error: Option<SharedString>,
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
    /// The gas strip's "Why pay less than the full gas?" popover. A new quote closes it.
    gas_help_open: bool,
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

    /// The strip shows a gas share `review` wasn't quoted at: a Bridge swap's share, priced
    /// locally until its bridge leg is quoted again. A share the quote can't support isn't
    /// pending, as the strip then shows the quote as it was priced.
    fn gas_share_pending(&self, review: &Arc<SwapReview>) -> bool {
        strip_review(self, review).gas_share_bps() != review.gas_share_bps()
    }

    /// `review` can't support the form's share and was priced at Tight instead. The user
    /// didn't choose that share, so the card selects no preset and shows no minimum, and
    /// [`Self::review_problem`] refuses the review until a share the quote supports is chosen.
    fn gas_share_fallback(&self, review: &Arc<SwapReview>) -> bool {
        strip_review(self, review).gas_share_bps() != self.gas_share_bps
    }

    /// The preset the strip selects: the shown share's, or the form's until a quote is ready.
    /// None for a custom share, and none while the quote fell back from the form's share.
    fn selected_gas_preset(&self, review: Option<&Arc<SwapReview>>) -> Option<GasPreset> {
        if self.gas_custom || review.is_some_and(|review| self.gas_share_fallback(review)) {
            return None;
        }
        GasPreset::of_share(review.map_or(self.gas_share_bps, |review| {
            strip_review(self, review).gas_share_bps()
        }))
    }

    /// The bar and the Minimum field show under the strip's row: the edit button opened them,
    /// or the share is a custom one.
    const fn gas_bar_open(&self) -> bool {
        self.gas_minimum_editing || self.gas_custom
    }

    /// The gas share the strip shows for the ready quote, if there is one.
    fn shown_gas_share(&self) -> Option<u16> {
        match &self.quote {
            QuoteState::Ready(review) => Some(strip_review(self, review).gas_share_bps()),
            _ => None,
        }
    }

    /// The receiver the form's quote carries: the entered one once it can be used, and until
    /// then `Address::ZERO`, a placeholder that signing refuses.
    const fn quote_receiver(&self) -> Address {
        match &self.delivery {
            Ok(
                SwapDelivery::External { receiver }
                | SwapDelivery::Bridge(BridgeDelivery { receiver, .. }),
            ) => *receiver,
            _ => Address::ZERO,
        }
    }

    /// The delivery to quote. No quote names the receiver, so a receiver that can't be used
    /// yet doesn't hold the quote back: the delivery then goes to the placeholder of
    /// [`Self::quote_receiver`], and [`Self::review_problem`] refuses the review. `None` while
    /// a Bridge swap has no provider for the Buy token.
    fn quote_delivery(&self) -> Option<SwapDelivery> {
        match &self.delivery {
            Ok(delivery) => Some(*delivery),
            Err(DeliveryProblem::Bridge) => None,
            Err(DeliveryProblem::Receiver(_)) => {
                let receiver = Address::ZERO;
                let Some(network) = self.network else {
                    return Some(SwapDelivery::External { receiver });
                };
                let terms = self.bridge_terms()?;
                Some(SwapDelivery::Bridge(BridgeDelivery {
                    provider: terms.provider,
                    destination_chain: network,
                    receiver,
                    destination_token: terms.token,
                    surplus: terms.surplus,
                }))
            }
        }
    }

    fn review_problem(&self, review: &Arc<SwapReview>) -> Option<SharedString> {
        match &self.delivery {
            Err(DeliveryProblem::Receiver(problem)) => Some(problem.clone()),
            Err(DeliveryProblem::Bridge) => Some("Choose a token the bridge delivers.".into()),
            Ok(delivery) if *delivery != review.plan().delivery() => {
                Some("Wait for the quote.".into())
            }
            Ok(_) if self.gas_share_pending(review) => Some(
                self.bridge_quote_error
                    .clone()
                    .unwrap_or_else(|| "Updating the bridge quote…".into()),
            ),
            Ok(_) if self.gas_share_fallback(review) => {
                Some("Gas is too high for this swap right now.".into())
            }
            Ok(_) if !review.price_verified() && !self.price_acknowledged => {
                Some("Accept the unverified price before you review the swap.".into())
            }
            Ok(_) if authorized_high_cost(review).is_some() && !self.high_costs_acknowledged => {
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
            (_, None, None) => {
                return match &routes.unavailable {
                    Some((provider, _)) => BridgeState::Unreachable(*provider),
                    None => BridgeState::Unavailable,
                };
            }
        };
        BridgeState::Ready {
            destination,
            provider,
            across: across.is_some(),
            near: near.is_some(),
            switched: across.is_none() && self.bridge.chosen != Some(BridgeProvider::NearIntents),
        }
    }

    /// The provider the chosen network's routes couldn't ask.
    fn bridge_unavailable(&self) -> Option<BridgeProvider> {
        let routes = self.bridge.routes.get(&(self.sell, self.network?))?;
        let (provider, _) = routes.as_ref().ok()?.unavailable.as_ref()?;
        Some(*provider)
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
    gas_share_bps: u16,
    valid_for: Duration,
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
        gas_share_bps: request.gas_share_bps,
        valid_for: request.valid_for,
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
/// One provider failing leaves its list empty and is named in the routes; both failing is the
/// error.
async fn fetch_bridge_routes(
    clients: &SwapBridgeClients,
    origin: BridgeProfile,
    destination: BridgeProfile,
    sell: Address,
    tokens: &EffectiveTokenRegistry,
    across_native: Option<Address>,
) -> eyre::Result<BridgeRoutes> {
    let chain = destination.chain_id();
    let (routes, listed, unavailable) = match tokio::join!(
        clients.across.available_routes(origin.chain_id(), chain),
        clients.near.tokens(),
    ) {
        (Ok(routes), Ok(listed)) => (routes, listed, None),
        (Ok(routes), Err(error)) => (
            routes,
            Vec::new(),
            Some((BridgeProvider::NearIntents, eyre::Report::from(error))),
        ),
        (Err(error), Ok(listed)) => (
            Vec::new(),
            listed,
            Some((BridgeProvider::Across, eyre::Report::from(error))),
        ),
        (Err(across), Err(near)) => {
            let (across, near) = (eyre::Report::from(across), eyre::Report::from(near));
            // The form shows one error: the first it can describe.
            return Err(
                if bridge_unreachable(&across).is_none() && bridge_unreachable(&near).is_some() {
                    near
                } else {
                    across
                },
            );
        }
    };
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
        unavailable,
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
    // The approved provider couldn't be asked, which says nothing about its route.
    if let Some((provider, error)) = routes.unavailable
        && provider == delivery.provider
    {
        return Err(error);
    }
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
                Some(SavedTerms {
                    slippage_bps: Some(pending.slippage_bps),
                    gas_share_bps: Some(pending.gas_share_bps),
                    valid_for: Some(pending.valid_for),
                }),
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
        // The last order's terms, or the approved ones. An order or approval saved before gas
        // shares restores its tolerance, and the share and validity take their defaults.
        let bounds = record
            .swap()
            .and_then(|swap| swap.orders().last())
            .map(wallet_ops::vault::SwapOrderRecord::bounds)
            .or_else(|| record.swap_approval().map(|approval| &approval.bounds));
        let terms = match bounds {
            Some(bounds) => SavedTerms {
                slippage_bps: Some(bounds.slippage_bps),
                gas_share_bps: bounds.gas_share_bps,
                valid_for: bounds
                    .valid_for_secs
                    .map(|secs| Duration::from_secs(secs.into())),
            },
            None => SavedTerms {
                slippage_bps: tracking.and_then(|tracking| tracking.slippage_bps),
                gas_share_bps: tracking.and_then(|tracking| tracking.gas_share_bps),
                valid_for: tracking.and_then(|tracking| tracking.valid_for),
            },
        };
        let delivery = swap_delivery(record);
        let existing = (self.stage(record) != SwapStage::SetupRetired).then_some(operation);
        self.open_form(
            existing,
            sell,
            Some(buy),
            amount,
            Some(terms),
            delivery,
            window,
            cx,
        );
        if let Some(form) = self.form.as_mut() {
            form.back_to_detail = Some(operation);
        }
    }

    /// Open the form with its fields filled in, and the order `terms` a reopened swap restores.
    /// A native `buy`, which only a Public address `delivery` receives, fills in as the wrapped
    /// native Buy asset with native output, before the Buy options are built or a quote is
    /// scheduled. A Bridge `delivery` fills in its
    /// network, provider, surplus choice and destination token, which replaces `buy`, the token
    /// handed to the provider.
    #[allow(clippy::too_many_arguments)]
    fn open_form(
        &mut self,
        operation: Option<ExecutorOperationId>,
        sell: Address,
        buy: Option<Address>,
        amount: Option<U256>,
        terms: Option<SavedTerms>,
        delivery: SwapDelivery,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        ensure_gas_bar_bindings(cx);
        let terms = terms.unwrap_or_default();
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
            notice_open: false,
            provider_hint_open: false,
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
        let gas_share_bps = terms
            .gas_share_bps
            .unwrap_or(GAS_SHARE_BALANCED_BPS)
            .min(GAS_SHARE_LOOSE_BPS);
        // The knob moves to the quote's position once one is ready.
        let gas_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(f32::from(GAS_BAR_STEP))
                .default_value(f32::from(100 - gas_share_bps / 100))
        });
        let gas_minimum_input = cx.new(|cx| InputState::new(window, cx).placeholder("0.0"));
        let gas_bar_focus = cx.focus_handle().tab_stop(true);
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
                &gas_slider,
                window,
                |this, slider, event: &SliderEvent, window, cx| {
                    if this
                        .form
                        .as_ref()
                        .is_none_or(|form| form.gas_slider != *slider)
                    {
                        return;
                    }
                    match event {
                        SliderEvent::Change(value) | SliderEvent::Release(value) => {
                            this.move_gas_bar(value.end(), window, cx);
                        }
                    }
                },
            ),
            cx.subscribe_in(
                &gas_minimum_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if this
                        .form
                        .as_ref()
                        .is_none_or(|form| form.gas_minimum_input != *input)
                    {
                        return;
                    }
                    match event {
                        InputEvent::Change => this.gas_minimum_edited(window, cx),
                        InputEvent::Blur => {
                            // An amount beyond the bar's ends shows the end it was clamped to.
                            this.sync_gas_controls(window, cx);
                        }
                        InputEvent::PressEnter { .. } | InputEvent::Focus => {}
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
            slippage_bps: terms.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS),
            gas_share_bps,
            gas_custom: GasPreset::of_share(gas_share_bps).is_none(),
            gas_slider,
            gas_minimum_input,
            gas_minimum_editing: false,
            gas_bar_focus,
            valid_for: terms
                .valid_for
                .unwrap_or_else(|| self.default_valid_for(cx)),
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
            bridge_quote_error: None,
            orderbook: None,
            bridge_clients: None,
            quoted_bridge: None,
            price_acknowledged: false,
            high_costs_acknowledged: false,
            error: None,
            back_to_detail: None,
            details_open: false,
            settings_open: false,
            gas_help_open: false,
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
        form.bridge.notice_open = false;
        form.bridge.provider_hint_open = false;
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
        form.bridge.notice_open = false;
        form.bridge.provider_hint_open = false;
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
                let unreachable = form.bridge_unavailable();
                let across_reason = (!across).then(|| {
                    let reason = if unreachable == Some(BridgeProvider::Across) {
                        "Unreachable now".to_owned()
                    } else if destination.destination_token == Address::ZERO {
                        format!("Doesn't deliver {}", destination.symbol)
                    } else {
                        format!("No route to {network}")
                    };
                    reason.into()
                });
                let near_reason = (!near).then(|| {
                    if unreachable == Some(BridgeProvider::NearIntents) {
                        "Unreachable now".into()
                    } else {
                        format!("Not listed on {network}").into()
                    }
                });
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
        self.receiver_changed(window, cx);
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
        self.receiver_changed(window, cx);
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

    /// The delivery choice changed: check it, then quote again, which clears the
    /// acknowledgements.
    fn delivery_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        self.refresh_form_delivery(cx);
        if let Some(form) = self.form.as_mut() {
            form.error = None;
        }
        self.schedule_quote(window, cx);
        cx.notify();
    }

    /// The receiver changed: check it, and clear the acknowledgements. No quote names the
    /// receiver, so a quote of the form's terms stays. A ready one takes the receiver here,
    /// without a request, and one in flight takes it when it lands. Without such a quote, the
    /// swap is quoted.
    fn receiver_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        self.refresh_form_delivery(cx);
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let quoted = form.quote_terms.is_some()
            && form.quote_terms == self.form_quote_terms(form, cx)
            && !matches!(form.quote, QuoteState::Idle | QuoteState::Failed(_));
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.error = None;
        if !quoted {
            self.schedule_quote(window, cx);
            cx.notify();
            return;
        }
        form.price_acknowledged = false;
        form.high_costs_acknowledged = false;
        let moved = match &form.quote {
            QuoteState::Ready(review) => {
                let moved = review.with_receiver(form.quote_receiver());
                (moved.plan().delivery() != review.plan().delivery()).then(|| Arc::new(moved))
            }
            _ => None,
        };
        if let Some(review) = moved {
            // A bridge refresh that is due or under way was asked for the replaced review, and
            // its reply would be dropped. Ask again for this one.
            let refresh = review.bridge().is_some()
                && form.gas_share_pending(&review)
                && form.bridge_quote_error.is_none();
            form.quote = QuoteState::Ready(review);
            if refresh {
                self.schedule_bridge_quote(window, cx);
            }
        }
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

    /// Choose a gas share preset. It closes the bar and the Minimum field.
    fn set_gas_preset(
        &mut self,
        preset: GasPreset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.close_gas_minimum(window, cx);
        self.set_gas_share(preset.share_bps(), false, window, cx);
    }

    /// Stop showing the bar and the Minimum field for the edit button. Focus on either moves
    /// to the amount before they go, so keyboard input and Escape still reach the dialog.
    fn close_gas_minimum(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.gas_minimum_editing = false;
        if form.gas_bar_focus.is_focused(window)
            || form
                .gas_minimum_input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        {
            self.focus_form_amount(window, cx);
        }
    }

    /// The edit button: open the bar and the Minimum field, with the field focused, or close
    /// them. A custom share keeps them open, and the button then only focuses the field. The
    /// share and its preset stay until the knob or the field moves it.
    fn edit_gas_minimum(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.gas_minimum_editing && !form.gas_custom {
            self.close_gas_minimum(window, cx);
            cx.notify();
            return;
        }
        form.gas_minimum_editing = true;
        let input = form.gas_minimum_input.clone();
        // The field takes the shown minimum before it is focused, as a focused one keeps its text.
        self.sync_gas_controls(window, cx);
        input.update(cx, |input, cx| input.focus(window, cx));
        // Once the field is drawn, typing replaces the shown minimum instead of appending to it.
        window.on_next_frame(|window, cx| {
            window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
        });
        cx.notify();
    }

    /// Choose the share of the gas estimate the minimum deducts. A same-chain quote is priced
    /// again locally, without a request. A Bridge swap's bridge leg was quoted for the old
    /// order amount, so a change of the share the strip shows quotes that leg again; until
    /// then the strip shows the local price, and the swap can't be reviewed. A change clears
    /// consent to high costs.
    fn set_gas_share(
        &mut self,
        share_bps: u16,
        custom: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let share_bps = share_bps.min(GAS_SHARE_LOOSE_BPS);
        let shown = form.shown_gas_share();
        if (form.gas_share_bps, form.gas_custom) != (share_bps, custom) {
            form.gas_share_bps = share_bps;
            form.gas_custom = custom;
            form.high_costs_acknowledged = false;
            form.error = None;
        }
        if let Some(tracking) = form
            .operation
            .and_then(|operation| self.tracking.get_mut(&operation))
        {
            tracking.gas_share_bps = Some(share_bps);
        }
        if form.network.is_none() {
            let repriced = match &form.quote {
                QuoteState::Ready(review) if review.gas_share_bps() != share_bps => {
                    review.with_gas_share(share_bps).ok()
                }
                _ => None,
            };
            if let Some(repriced) = repriced {
                form.quote = QuoteState::Ready(Arc::new(repriced));
            }
        }
        // A release at the dragged share, or a share the quote can't support, shows the same
        // share and starts no refresh.
        let refresh = form.network.is_some() && form.shown_gas_share() != shown;
        self.sync_gas_controls(window, cx);
        if refresh {
            self.schedule_bridge_quote(window, cx);
        }
        cx.notify();
    }

    /// The gas bar moved to `value`, its position in percent from "you pay all gas". Moving
    /// the bar selects Custom.
    fn move_gas_bar(&mut self, value: f32, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(QuoteState::Ready(review)) = self.form.as_ref().map(|form| &form.quote) else {
            return;
        };
        let share_bps = GasBar::of(review).share_at(slider_percent(value) * 100);
        self.set_gas_share(share_bps, true, window, cx);
    }

    /// Move the gas bar's knob from its current step, in percent of the bar, by the keyboard.
    fn step_gas_bar(
        &mut self,
        step: impl FnOnce(u16) -> u16,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let shown = strip_review(form, review);
        let bar = GasBar::of(&shown);
        let current = nearest_step(bar.position_bps(shown.gas_share_bps()));
        let share_bps = bar.share_at(step(current).min(100) * 100);
        self.set_gas_share(share_bps, true, window, cx);
    }

    /// The Minimum field changed: the share whose minimum it is, clamped to the bar. A later
    /// quote keeps the share, not the typed amount.
    fn gas_minimum_edited(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let Ok(shown) = wallet_ops::parse_unshield_amount(
            &form.gas_minimum_input.read(cx).value(),
            self.strip_decimals(form, review, cx),
        ) else {
            return;
        };
        let minimum = StripScale::of(review).order(shown);
        // For Private delivery, the amount before the shield fee.
        let pre_fee = review.buy_amount_for(minimum).unwrap_or(minimum);
        let share_bps = GasBar::of(review).share_for_pre_fee(pre_fee);
        self.set_gas_share(share_bps, true, window, cx);
    }

    /// Put the knob at the shown share's nearest step, and the Minimum field at the shown
    /// minimum unless the user is typing in it.
    fn sync_gas_controls(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let shown = strip_review(form, review);
        let value = f32::from(nearest_step(
            GasBar::of(&shown).position_bps(shown.gas_share_bps()),
        ));
        let input = form.gas_minimum_input.clone();
        // At the display precision of the card's amounts; the share, not this text, sets the order.
        let text = (!input.read(cx).focus_handle(cx).is_focused(window)).then(|| {
            let minimum = StripScale::of(review).show(shown.suggested_private_minimum());
            match self.strip_decimals(form, review, cx) {
                Some(decimals) => railgun_ui::format_token_amount(minimum, decimals),
                None => format_unshield_amount_input(minimum, None),
            }
        });
        form.gas_slider.update(cx, |slider, cx| {
            slider.set_value(value, window, cx);
        });
        if let Some(text) = text {
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
    }

    /// Choose how long the order is valid after signing. Bridge delivery keeps the profile's
    /// window. `validTo` is set from the quote, so the swap is quoted again.
    fn set_valid_for(
        &mut self,
        valid_for: Duration,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.valid_for == valid_for || form.network.is_some() {
            return;
        }
        form.valid_for = valid_for;
        self.schedule_quote(window, cx);
        // The choice lives in the quote details, which hide until a new quote is ready, as
        // Price tolerance does.
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
            .swap_summary(&review, Some(&approval), None, None, cx)
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
        tracking.gas_share_bps = bounds.gas_share_bps;
        tracking.valid_for = bounds
            .valid_for_secs
            .map(|secs| Duration::from_secs(secs.into()));
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

    /// What a quote of the form as it stands is requested for, once it has a Buy token and an
    /// amount.
    fn form_quote_terms(&self, form: &SwapForm, cx: &App) -> Option<QuoteTerms> {
        // Before setup the quote uses a stand-in. Set-up accounts use local observations;
        // execution preparation refreshes the account before proving and signing.
        let executor = match (self.form_mode(form), form.operation) {
            (FormMode::Setup { resume: true }, Some(operation)) => QuoteExecutor::Setup(operation),
            (FormMode::Order, Some(operation)) => QuoteExecutor::Order {
                operation,
                reuse: form.reuse_account,
            },
            _ => QuoteExecutor::Preview,
        };
        let (buy, amount) = form.quote_buy().zip(self.form_amount(form, cx).ok())?;
        Some(QuoteTerms {
            executor,
            sell: form.sell,
            buy,
            amount,
            slippage_bps: form.slippage_bps,
            receive_to: form.receive_to,
            bridge: form.bridge_terms(),
        })
    }

    fn schedule_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut() {
            form.price_acknowledged = false;
            form.high_costs_acknowledged = false;
            // A ready quote's task is its bridge refresh, which a full quote supersedes.
            if matches!(form.quote, QuoteState::Ready(_)) {
                form.quote_task = None;
                form.quote_revision = form.quote_revision.wrapping_add(1);
            }
            form.bridge_quote_error = None;
        }
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let operation = form.operation;
        let mode = self.form_mode(form);
        if !matches!(mode, FormMode::Setup { .. } | FormMode::Order) {
            return;
        }
        let (gas_share_bps, valid_for) = (form.gas_share_bps, form.valid_for);
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
        let (Some(terms), Some((anchor_cache, tokens)), Some(delivery)) = (
            self.form_quote_terms(form, cx),
            registries,
            form.quote_delivery(),
        ) else {
            if let Some(form) = self.form.as_mut() {
                form.quote = QuoteState::Idle;
                form.quote_task = None;
                form.quote_terms = None;
                form.gas_help_open = false;
            }
            cx.notify();
            return;
        };
        let tracking = operation.map(|operation| self.tracking.entry(operation).or_default());
        let byte_budget = tracking.as_ref().and_then(|tracking| tracking.byte_budget);
        let tracked_orderbook = tracking.and_then(|tracking| {
            tracking.amount = Some(terms.amount);
            tracking.slippage_bps = Some(terms.slippage_bps);
            tracking.gas_share_bps = Some(gas_share_bps);
            tracking.valid_for = Some(valid_for);
            tracking.orderbook.clone()
        });
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let request = QuoteRequest {
            executor: terms.executor,
            sell: terms.sell,
            buy: terms.buy,
            amount: terms.amount,
            delivery,
            slippage_bps: terms.slippage_bps,
            gas_share_bps,
            valid_for,
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
        form.gas_help_open = false;
        form.quote_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(QUOTE_DEBOUNCE).await;
            let result = runtime.spawn(quote_swap(owner, session, request)).await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_quote(operation, revision, result.ok(), window, cx);
            });
        }));
        cx.notify();
    }

    /// Quote a Bridge swap's bridge leg again at the share its strip previews, after the
    /// debounce. The ready quote stays in view, and the orderbook isn't asked. A share back at
    /// the quoted one has nothing to refresh, which also ends a refresh still under way.
    fn schedule_bridge_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let QuoteState::Ready(review) = &form.quote else {
            return;
        };
        let review = Arc::clone(review);
        let operation = form.operation;
        let gas_share_bps = strip_review(form, &review).gas_share_bps();
        let pending = gas_share_bps != review.gas_share_bps()
            && matches!(
                self.form_mode(form),
                FormMode::Setup { .. } | FormMode::Order
            );
        let bridge = form.quoted_bridge.clone();
        // As a full quote, a refresh after a signing-time price failure skips the anchor cache.
        let price_unavailable = operation.is_some_and(|operation| {
            self.reapproval == Some((operation, SwapReviewChange::PriceUnavailable))
        });
        let registries = self.root.upgrade().map(|root| {
            let root = root.read(cx);
            (
                (!price_unavailable).then(|| Arc::clone(&root.public_broadcaster_anchor_cache)),
                root.effective_token_registry.clone(),
            )
        });
        let owner = Arc::clone(&self.owner);
        let runtime = self.runtime.clone();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // A ready quote's task is an earlier refresh. Dropping it stops its request.
        form.quote_task = None;
        form.quote_revision = form.quote_revision.wrapping_add(1);
        form.bridge_quote_error = None;
        if !pending {
            cx.notify();
            return;
        }
        form.price_acknowledged = false;
        form.high_costs_acknowledged = false;
        let (Some(bridge), Some((anchor_cache, tokens))) = (bridge, registries) else {
            form.bridge_quote_error =
                Some("The swap's bridge route isn't ready. Retry the quote.".into());
            cx.notify();
            return;
        };
        let revision = form.quote_revision;
        form.quote_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(QUOTE_DEBOUNCE).await;
            let quoted = Arc::clone(&review);
            let work = runtime.spawn(async move {
                Box::pin(owner.requote_swap_bridge(
                    &quoted,
                    gas_share_bps,
                    bridge.route(),
                    anchor_cache.as_deref(),
                    &tokens,
                ))
                .await
            });
            // Dropping a Tokio handle detaches its task, so a superseded refresh aborts it.
            let _abort = AbortOnDrop(work.abort_handle());
            let result = work.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_bridge_quote(
                    operation,
                    revision,
                    &review,
                    gas_share_bps,
                    result.ok(),
                    window,
                    cx,
                );
            });
        }));
        cx.notify();
    }

    /// Install a bridge refresh's reply, if the form still shows the quote and the share it
    /// was asked for. A failure keeps the quote and the share, and says why.
    fn apply_bridge_quote(
        &mut self,
        operation: Option<ExecutorOperationId>,
        revision: u64,
        quoted: &Arc<SwapReview>,
        gas_share_bps: u16,
        result: Option<eyre::Result<SwapReview>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        if form.operation != operation || form.quote_revision != revision {
            return;
        }
        let current = matches!(&form.quote, QuoteState::Ready(review)
            if Arc::ptr_eq(review, quoted)
                && strip_review(form, review).gas_share_bps() == gas_share_bps
                && form.quote_delivery() == Some(review.plan().delivery()))
            && matches!(
                self.form_mode(form),
                FormMode::Setup { .. } | FormMode::Order
            );
        let outcome = match result {
            Some(Ok(review)) => Ok(review),
            Some(Err(error))
                if matches!(
                    error.downcast_ref::<QuoteDeviationError>(),
                    Some(QuoteDeviationError::ExceedsThreshold)
                ) =>
            {
                let threshold = self
                    .swap_profile(cx)
                    .map_or(300, |profile| profile.anchor_deviation_bps());
                Err(format!(
                    "The bridge's rate is more than {} below the anchor price.",
                    format_bps_percent(u64::from(threshold))
                ))
            }
            Some(Err(error)) => {
                Err(bridge_unreachable(&error).unwrap_or_else(|| format!("{error:#}")))
            }
            None => Err("Updating the bridge quote stopped unexpectedly. Try again.".to_owned()),
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        form.quote_task = None;
        if !current {
            cx.notify();
            return;
        }
        match outcome {
            Ok(review) => {
                // The destination terms changed, so consent to the old ones doesn't carry over.
                form.price_acknowledged = false;
                form.high_costs_acknowledged = false;
                form.quote = QuoteState::Ready(Arc::new(review));
            }
            Err(error) => form.bridge_quote_error = Some(error.into()),
        }
        self.quote_installed(operation, window, cx);
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
                // A share changed while this quote was in flight prices it locally. A share
                // the quote can't support keeps the Tight price the quote fell back to.
                let review = match review.with_gas_share(form.gas_share_bps) {
                    Ok(repriced)
                        if form.network.is_none()
                            && review.gas_share_bps() != form.gas_share_bps =>
                    {
                        Box::new(repriced)
                    }
                    _ => review,
                };
                // The quote names no receiver, so it takes the form's as it is now.
                QuoteState::Ready(Arc::new(review.with_receiver(form.quote_receiver())))
            }
            Ok(QuoteOutcome::PriceBlocked(block)) => QuoteState::PriceBlocked(block),
            Err(error) => QuoteState::Failed(error),
        };
        self.quote_installed(operation, window, cx);
    }

    /// Continue from the form's quote after a full quote or a bridge refresh ended.
    fn quote_installed(
        &mut self,
        operation: Option<ExecutorOperationId>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.form.as_ref() else {
            return;
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
        // Setup can confirm while its retry quote or bridge refresh is in flight. Replace that
        // setup preview with an order quote before offering approval.
        if self.form_mode(form) == FormMode::Order
            && matches!(&form.quote, QuoteState::Ready(review)
                if review.plan().swap_executor().requires_setup())
        {
            self.schedule_quote(window, cx);
            return;
        }
        // A Bridge share changed while the quote was in flight: quote its bridge leg again.
        if form.bridge_quote_error.is_none()
            && matches!(&form.quote, QuoteState::Ready(review)
                if review.bridge().is_some() && form.gas_share_pending(review))
        {
            self.schedule_bridge_quote(window, cx);
        }
        // The knob and the Minimum field follow the new quote's gas; the share stays.
        self.sync_gas_controls(window, cx);
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
        // A review that names a change marks the amounts that differ from the saved approval.
        let approved = change.and_then(|_| self.approved_bounds(operation, review));
        let summary = self
            .swap_summary(&approval.review, None, change, approved, cx)
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

    /// The bounds of the approval saved with the swap's setup, while that approval still binds
    /// the first order and names `review`'s token pair and delivery.
    fn approved_bounds(
        &self,
        operation: ExecutorOperationId,
        review: &SwapReview,
    ) -> Option<&SwapApprovedBounds> {
        let plan = review.plan();
        let record = self
            .record(operation)
            .filter(|record| record.swap().is_none())?;
        let approval = record.swap_approval()?;
        (approval.delivery == plan.delivery()
            && record.swap_approval_tokens() == Some((plan.sell_token(), plan.buy_token())))
        .then_some(&approval.bounds)
    }

    /// The single review of a swap. With `setup`, a new swap's setup and its order, placed once
    /// the setup is confirmed; without, an order for a stealth account that is set up. With
    /// `approved`, the amounts that differ from those bounds show their difference.
    fn swap_summary(
        &self,
        review: &SwapReview,
        setup: Option<&SetupApproval>,
        change: Option<SwapReviewChange>,
        approved: Option<&SwapApprovedBounds>,
        cx: &App,
    ) -> SpendAuthorizationSummary {
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let valid_for = review.valid_for().as_secs() / 60;
        let slippage = format_bps_percent(u64::from(review.slippage_bps()));
        let chain = self.chain_label();
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
                    format!("up to {maximum_fee} · not refunded"),
                )
                .with_hint(SpendAuthorizationHint::new(
                    "Setup fee",
                    [format!(
                        "Paid to broadcaster {} from your private balance to create the stealth account. Not refunded if the order doesn't fill.",
                        broadcaster_candidate_label(&setup.candidate)
                    )],
                )),
            );
        }
        // A Bridge order buys exactly this and deposits it with its provider.
        let minimum = self.token_amount(buy, review.suggested_private_minimum(), cx);
        let bridge = match plan.delivery() {
            SwapDelivery::Bridge(delivery) => Some(delivery),
            _ => None,
        };
        let receive = match bridge.zip(review.bridge()) {
            Some((delivery, quote)) => self
                .bridge_receive_card(
                    delivery,
                    Some(quote.expected_output),
                    quote.destination_minimum,
                    cx,
                )
                .with_amount_change(
                    approved.and_then(|approved| approved.destination_minimum),
                    quote.destination_minimum,
                    false,
                    |amount| {
                        self.network_token_amount(
                            delivery.destination_chain,
                            self.bridge_received_token(delivery, cx),
                            amount,
                            cx,
                        )
                    },
                ),
            None => self
                .receive_card(review, review.suggested_private_minimum(), cx)
                .with_amount_change(
                    approved.map(|approved| approved.private_minimum),
                    review.suggested_private_minimum(),
                    false,
                    |amount| self.token_amount(buy, amount, cx),
                ),
        };
        let preset = gas_share_name(review.gas_share_bps());
        let (allowed, estimate) = (
            self.gas_money(buy, review.gas_allowance(), cx),
            self.gas_money(buy, review.gas_estimate(), cx),
        );
        rows.push(
            SpendAuthorizationSummaryRow::new(
                "Gas",
                format!("{preset} · up to {allowed} of ≈ {estimate}"),
            )
            .with_hint(SpendAuthorizationHint::new(
                "Gas you pay",
                [format!(
                    "This swap costs about {estimate} in network fees. You pay at most {allowed} ({preset}). If no solver covers the rest within {valid_for} minutes, the order expires and nothing is swapped."
                )],
            ))
            .with_amount_change(
                approved.and_then(|approved| approved.gas_allowance),
                review.gas_allowance(),
                true,
                |amount| self.gas_money(buy, amount, cx),
            ),
        );
        let surplus = review.estimated_source_surplus();
        if let Some(surplus) = surplus {
            let hint = SpendAuthorizationHint::new(
                format!("Returned on {chain}"),
                [source_return_hint(review, &minimum, &chain)],
            );
            rows.push(
                SpendAuthorizationSummaryRow::new(
                    "Returned",
                    self.with_usd(
                        format!("≈ {}", self.token_amount(buy, surplus, cx)),
                        buy,
                        surplus,
                        cx,
                    ),
                )
                .with_hint(match self.bridge_total_usd_value(review, cx) {
                    Some(total) => hint.with_fact(
                        "Estimated total received",
                        format!("≈ {}", railgun_ui::format_usd_micro_value(total)),
                    ),
                    None => hint,
                }),
            );
        }
        if let Some(delivery) = bridge {
            rows.push(self.bridge_row(delivery, buy, Some(review), surplus.is_some(), cx));
        }
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
        let mut details = Vec::new();
        // A Bridge order signs its deposit, which the Deposit row shows.
        if bridge.is_none()
            && let Ok(signed) = review.buy_amount_for(review.suggested_private_minimum())
        {
            let signed = self.token_amount(buy, signed, cx);
            details.push((
                "Signed minimum",
                if plan.delivery() == SwapDelivery::Reshield {
                    format!("{signed}, before the shield fee")
                } else {
                    signed
                },
            ));
        }
        details.push((
            "Railgun unshield",
            self.token_amount(sell, plan.amount().saturating_sub(review.sell_amount()), cx),
        ));
        if let Some(fee) = review.cow_fee() {
            details.push(("CoW fee", self.token_amount(buy, fee, cx)));
        }
        details.push((
            "Gas estimate",
            format!(
                "≈ {estimate} at {} gwei",
                format_gwei(review.gas_price_wei())
            ),
        ));
        details.push(("Price tolerance", slippage.clone()));
        if let Some(delivery) = bridge {
            details.push((
                match delivery.provider {
                    BridgeProvider::Across => "Deposit to Across",
                    BridgeProvider::NearIntents => "Deposit to 1Click",
                },
                format!("{minimum} on {chain}"),
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
        // What becomes public: the line's summary, and the paragraphs of its card.
        let mut public = vec![
            "Placing the order publishes its tokens, amounts, price limit, and hook data, including the notes it spends, even if it never fills. A later spend of those notes can be linked to this swap.".to_owned(),
        ];
        let receiver = match plan.delivery() {
            SwapDelivery::Reshield => None,
            SwapDelivery::External { receiver } => {
                public.push(EXTERNAL_DELIVERY_DISCLOSURE.to_owned());
                Some((receiver, self.token_symbol(buy, cx)))
            }
            SwapDelivery::Bridge(delivery) => {
                public.push(self.bridge_disclosure(delivery));
                Some((
                    delivery.receiver,
                    self.network_token_symbol(
                        delivery.destination_chain,
                        self.bridge_received_token(delivery, cx),
                        cx,
                    ),
                ))
            }
        };
        if !isolated {
            public.push(
                "This network mode can't give the swap its own network route, so its orderbook requests aren't isolated from your other wallet traffic.".to_owned(),
            );
        }
        let own_account_warning = receiver
            .as_ref()
            .and_then(|(receiver, received)| self.own_account_warning(*receiver, received, cx));
        let public_summary = match (&receiver, &own_account_warning) {
            (None, _) => "the order and the notes it spends".to_owned(),
            (Some((receiver, _)), Some(_)) => format!(
                "this swap, and {} as its receiver",
                spend_authorization_recipient_display(&receiver.to_checksum(None))
            ),
            (Some(_), None) => "the order, the receiver and the amount".to_owned(),
        };
        let public = SpendAuthorizationHint::new("What becomes public", public);
        let (title, confirm_label) = if setup.is_some() {
            ("Set up stealth account and swap", "Create stealth account")
        } else {
            ("Private swap", "Swap")
        };
        let summary = SpendAuthorizationSummary::new(title, "", rows)
            .with_title_chip(chain)
            .with_cards(
                self.sell_card(sell, plan.amount(), cx).with_amount_change(
                    approved.map(SwapApprovedBounds::spend_amount),
                    plan.amount(),
                    true,
                    |amount| self.token_amount(sell, amount, cx),
                ),
                receive,
            )
            .with_compact_rows()
            .with_details(
                "Order terms",
                format!("{slippage} price · {valid_for} min"),
                details,
                Some(
                    "If someone triggers the swap's unshield and the order doesn't fill, recovering the tokens costs the unshield and shield fees.",
                ),
            )
            .with_disclosure(
                public_summary,
                match own_account_warning {
                    Some(warning) => public.with_warning(warning),
                    None => public,
                },
            )
            .with_confirm_label(confirm_label);
        let summary = if setup.is_some() {
            summary.with_steps(1, SWAP_STEPS, swap_steps_hint())
        } else {
            summary
        };
        let mut warnings = Vec::new();
        if !review.price_verified() {
            warnings.push(Arc::from(UNVERIFIED_PRICE_WARNING));
        }
        if let Some(bps) = authorized_high_cost(review) {
            warnings.push(Arc::from(self.authorized_cost_message(review, bps, cx)));
        }
        if bridge.is_some_and(|delivery| delivery.provider == BridgeProvider::NearIntents) {
            warnings.push(Arc::from(NEAR_INTENTS_DISCLAIMER));
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

    /// The warning for External or Bridge delivery to a `receiver` that is one of the wallet's
    /// own Public accounts, which the swap links to a Railgun spend of the `received` token.
    fn own_account_warning(&self, receiver: Address, received: &str, cx: &App) -> Option<String> {
        self.receiver_label(receiver, cx)
            .filter(|(_, own)| *own)
            .map(|(label, _)| {
                format!(
                    "{label} becomes publicly linked to this swap: anyone can see that it received {received} from a Railgun spend."
                )
            })
    }

    /// The review's Sell card.
    fn sell_card(&self, sell: Address, amount: U256, cx: &App) -> SpendAuthorizationCard {
        SpendAuthorizationCard::new(
            "Sell",
            self.token_amount(sell, amount, cx),
            self.token_icon(sell, cx),
        )
        .with_usd(
            self.usd_label(sell, amount, cx)
                .filter(|usd| !usd_repeats_amount(usd, &self.bare_amount(sell, amount, cx))),
        )
    }

    /// `card` with the line naming `receiver`. Hovering it shows the wallet's name for the
    /// address, with one of its own Public accounts marked as such.
    fn with_receiver_line(
        &self,
        card: SpendAuthorizationCard,
        receiver: Address,
        cx: &App,
    ) -> SpendAuthorizationCard {
        let label = self.receiver_label(receiver, cx).map(|(label, own)| {
            if own {
                format!("{label} · your Public account")
            } else {
                label
            }
        });
        card.with_receiver(receiver.to_checksum(None), label)
    }

    /// The Receive card of a swap that pays out on its own chain: at least `minimum`, the best
    /// case, and where it goes.
    fn receive_card(&self, review: &SwapReview, minimum: U256, cx: &App) -> SpendAuthorizationCard {
        let buy = review.plan().buy_token();
        let card = SpendAuthorizationCard::new(
            "Receive at least",
            self.token_amount(buy, minimum, cx),
            self.token_icon(buy, cx),
        )
        .with_usd(
            self.usd_label(buy, minimum, cx)
                .filter(|usd| !usd_repeats_amount(usd, &self.bare_amount(buy, minimum, cx))),
        )
        .with_emphasis(
            "up to",
            self.bare_amount(buy, best_after_fees(review), cx),
            "if solvers pay all gas",
        );
        match review.plan().delivery() {
            SwapDelivery::Reshield => card.with_line("to your private balance"),
            SwapDelivery::External { receiver } => self.with_receiver_line(card, receiver, cx),
            SwapDelivery::Bridge(delivery) => self.with_receiver_line(card, delivery.receiver, cx),
        }
    }

    /// A Bridge swap's Receive card, in its network's token. Across delivers exactly the
    /// `minimum`; 1Click at least the `minimum`, and about `expected` when it is known for this
    /// order.
    fn bridge_receive_card(
        &self,
        delivery: BridgeDelivery,
        expected: Option<U256>,
        minimum: U256,
        cx: &App,
    ) -> SpendAuthorizationCard {
        let (network, token) = (
            delivery.destination_chain,
            self.bridge_received_token(delivery, cx),
        );
        let card = |bound: &str| {
            SpendAuthorizationCard::new(
                format!("Receive on {}, {bound}", network_name(network)),
                self.network_token_amount(network, token, minimum, cx),
                self.chain_token_metadata(network, token, cx)
                    .and_then(|metadata| metadata.icon_path),
            )
            .with_usd(
                self.network_usd_micro_value(network, delivery.destination_token, minimum, cx)
                    .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)))
                    .filter(|usd| {
                        !usd_repeats_amount(
                            usd,
                            &self.network_bare_amount(network, token, minimum, cx),
                        )
                    }),
            )
        };
        let card = match delivery.provider {
            BridgeProvider::Across => card("exactly"),
            BridgeProvider::NearIntents => {
                let card = card("at least");
                match expected {
                    Some(expected) => card.with_emphasis(
                        "about",
                        self.network_bare_amount(network, token, expected, cx),
                        "expected",
                    ),
                    None => card,
                }
            }
        };
        self.with_receiver_line(card, delivery.receiver, cx)
    }

    /// A Bridge swap's Bridge row: its provider and, in the full `review`, its fee, with the
    /// provider's terms behind the info button. The confirm-only step, without a `review`,
    /// shows no fee, since the minimum it leaves was approved. Without a Returned row, the
    /// hint says where Across leaves the surplus.
    fn bridge_row(
        &self,
        delivery: BridgeDelivery,
        bought: Address,
        review: Option<&SwapReview>,
        returned: bool,
        cx: &App,
    ) -> SpendAuthorizationSummaryRow {
        let chain = self.chain_label();
        let provider = provider_name(delivery.provider);
        let fee = review
            .and_then(SwapReview::bridge)
            .and_then(|bridge| bridge.fee);
        let (value, mut paragraphs) = match delivery.provider {
            BridgeProvider::Across => {
                let fee = fee.map(|fee| self.token_amount(bought, fee, cx));
                let mut paragraphs = vec!["Relayers usually fill within a few minutes.".to_owned()];
                if let Some(fee) = &fee {
                    paragraphs.push(format!(
                        "The {fee} fee covers relayer and LP fees and is already out of the amount you receive."
                    ));
                }
                if !returned {
                    paragraphs.push(match delivery.surplus {
                        BridgeSurplus::KeepInAccount => {
                            format!("CoW's surplus stays in the stealth account on {chain}.")
                        }
                        _ => format!("CoW's surplus is reshielded on {chain}."),
                    });
                }
                (
                    fee.map_or_else(
                        || provider.to_owned(),
                        |fee| format!("{provider} · fee {fee}"),
                    ),
                    paragraphs,
                )
            }
            BridgeProvider::NearIntents => {
                let via = format!(
                    "Via {} on {chain}. The whole payout is converted, surplus included.",
                    self.token_symbol(bought, cx)
                );
                match (review, fee) {
                    (None, _) => (provider.to_owned(), vec![via]),
                    // The fee's share of the deposit it was taken from.
                    (Some(review), Some(fee)) => (
                        format!("{provider} · fee ≈ {}", self.token_amount(bought, fee, cx)),
                        vec![
                            via,
                            format!(
                                "The 1Click fee, about {}, is already in the minimum.",
                                format_bps_percent(swap_cost_bps(
                                    fee,
                                    review.suggested_private_minimum().saturating_sub(fee)
                                ))
                            ),
                        ],
                    ),
                    // A leg between different assets without anchors has no value for its fee.
                    (Some(_), None) => (
                        format!("{provider} · fee included"),
                        vec![
                            via,
                            "The 1Click fee is already in the minimum. There's no independent price to value it.".to_owned(),
                        ],
                    ),
                }
            }
        };
        // A Bridge order buys exactly the deposit, the quote's minimum.
        if let Some(review) = review
            && delivery.provider == BridgeProvider::Across
        {
            paragraphs.push(format!(
                "If the deposit isn't filled before it expires, Across refunds the {} to the stealth account on {chain}, usually within a few hours. Recovering it to your private balance costs a shield fee and a broadcaster fee.",
                self.token_amount(bought, review.suggested_private_minimum(), cx)
            ));
        }
        SpendAuthorizationSummaryRow::new("Bridge", value)
            .with_hint(SpendAuthorizationHint::new(provider, paragraphs))
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
        let valid_for = self.default_valid_for(cx);
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
            // A legacy approval without a share needs a full review whatever the requote uses.
            gas_share_bps: approval
                .bounds
                .gas_share_bps
                .unwrap_or(GAS_SHARE_BALANCED_BPS),
            valid_for: approval
                .bounds
                .valid_for_secs
                .map_or(valid_for, |secs| Duration::from_secs(secs.into())),
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
                // The next attempt gets a new isolated route instead of the failed circuit.
                self.tracking.entry(operation).or_default().orderbook = None;
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
            QuoteOutcome::Review(review) => match review.approved_order_minimum(approval) {
                // The approved minimum, or a Bridge deposit raised to keep the approved
                // destination minimum.
                Ok(private_minimum) => {
                    let Some(orderbook) = result.orderbook else {
                        return;
                    };
                    let approval = OrderApproval {
                        operation,
                        review: Arc::from(review),
                        private_minimum,
                        price_acknowledged: approval.price_acknowledged,
                        orderbook,
                        bridge: result.bridge,
                        destination_minimum: approval.bounds.destination_minimum,
                        full_review: false,
                    };
                    let summary = match self.place_summary(&approval, cx) {
                        Ok(summary) => summary,
                        Err(error) => {
                            self.fail(operation, format!("{error:#}"));
                            return;
                        }
                    };
                    self.request_authorization(
                        SwapAction::Order(Box::new(approval)),
                        summary,
                        window,
                        cx,
                    );
                    return;
                }
                Err(change) => Some(change),
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

    /// The confirm-only step for an order approved with its setup, whose terms still hold. Its
    /// Receive card names the approved receiver of a Public address or Bridge swap once more,
    /// and a Bridge swap's destination terms the approval binds. The gas shown is what the
    /// signed minimum leaves room for.
    fn place_summary(
        &self,
        approval: &OrderApproval,
        cx: &App,
    ) -> eyre::Result<SpendAuthorizationSummary> {
        let review = &approval.review;
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let valid_for = review.valid_for().as_secs() / 60;
        let tolerance = format_bps_percent(u64::from(review.slippage_bps()));
        let mut details = vec![(
            "Gas you pay",
            format!(
                "Up to {} of ≈ {}",
                self.gas_money(buy, review.gas_allowance_for(approval.private_minimum)?, cx),
                self.gas_money(buy, review.gas_estimate(), cx)
            ),
        )];
        if let Some(fee) = review.cow_fee() {
            details.push(("CoW fee", self.token_amount(buy, fee, cx)));
        }
        details.push(("Price tolerance", tolerance.clone()));
        details.push(("Order valid for", format!("{valid_for} minutes from now")));
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
                // The estimate was quoted for the review's own deposit.
                let expected = Some(quote.expected_output).filter(|expected| {
                    approval.private_minimum == review.suggested_private_minimum()
                        && *expected >= minimum
                });
                self.bridge_receive_card(delivery, expected, minimum, cx)
            }
            _ => self.receive_card(&approval.review, approval.private_minimum, cx),
        };
        let rows = bridge
            .map(|delivery| self.bridge_row(delivery, buy, None, false, cx))
            .into_iter()
            .collect();
        let checked = bridge.map_or_else(
            || {
                "The order keeps the minimum you approved. Costs were checked again and are shown below."
                    .to_owned()
            },
            |delivery| {
                format!(
                    "The order keeps the minimum you approved. Costs and the {} quote were checked again.",
                    provider_name(delivery.provider)
                )
            },
        );
        Ok(
            SpendAuthorizationSummary::new("Place swap order", checked, rows)
                .with_title_chip(self.chain_label())
                .with_steps(2, SWAP_STEPS, swap_steps_hint())
                .with_cards(self.sell_card(sell, plan.amount(), cx), receive)
                .with_compact_rows()
                .with_details(
                    "Order terms",
                    format!(
                        "{} · {tolerance} price · {valid_for} min from now",
                        gas_share_name(review.gas_share_bps())
                    ),
                    details,
                    None,
                )
                .with_confirm_label("Place order")
                .with_warnings(if approval.review.price_verified() {
                    Vec::new()
                } else {
                    vec![Arc::from(UNVERIFIED_PRICE_WARNING)]
                }),
        )
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
            gas_share_bps: approval.review.gas_share_bps(),
            valid_for: approval.review.valid_for(),
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

    /// The order validity a quote requests: the swap profile's window.
    fn default_valid_for(&self, cx: &App) -> Duration {
        self.swap_profile(cx)
            .map_or(Duration::from_mins(10), |profile| profile.valid_to_window())
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

    fn set_gas_help_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.gas_help_open != open
        {
            form.gas_help_open = open;
            cx.notify();
        }
    }

    fn set_bridge_notice_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.bridge.notice_open != open
        {
            form.bridge.notice_open = open;
            cx.notify();
        }
    }

    fn set_provider_hint_open(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        if let Some(form) = self.form.as_mut()
            && form.bridge.provider_hint_open != open
        {
            form.bridge.provider_hint_open = open;
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

    /// A gas amount of `token` in the wallet's currency, "$1.48", or as a bare amount without a
    /// cached rate.
    fn gas_money(&self, token: Address, amount: U256, cx: &App) -> String {
        self.usd_micro_value(token, amount, cx).map_or_else(
            || self.bare_amount(token, amount, cx),
            railgun_ui::format_usd_micro_value,
        )
    }

    /// An amount the gas strip shows, without its symbol: for a Bridge `review`, in its
    /// destination token.
    fn strip_amount(&self, form: &SwapForm, review: &SwapReview, amount: U256, cx: &App) -> String {
        match (form.network, review.bridge()) {
            (Some(network), Some(_)) => {
                self.network_bare_amount(network, review_destination_token(review), amount, cx)
            }
            _ => self.bare_amount(review.plan().buy_token(), amount, cx),
        }
    }

    /// The symbol of the gas strip's amounts, for Custom's Minimum field.
    fn strip_symbol(&self, form: &SwapForm, review: &SwapReview, cx: &App) -> String {
        match (form.network, review.bridge()) {
            (Some(network), Some(_)) => {
                self.network_token_symbol(network, review_destination_token(review), cx)
            }
            _ => self.token_symbol(review.plan().buy_token(), cx),
        }
    }

    /// The decimals of the gas strip's amounts, for Custom's Minimum field.
    fn strip_decimals(&self, form: &SwapForm, review: &SwapReview, cx: &App) -> Option<u8> {
        match (form.network, review.bridge()) {
            (Some(network), Some(_)) => self
                .chain_token_metadata(network, review_destination_token(review), cx)
                .map(|metadata| metadata.decimals),
            _ => self.token_decimals(review.plan().buy_token(), cx),
        }
    }

    /// The USD value beside Custom's Minimum field, for `amount` of the order's bought token:
    /// for a Bridge `review`, the destination amount it scales to at the destination token's
    /// rate, and none without that rate.
    fn strip_usd_label(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        amount: U256,
        cx: &App,
    ) -> Option<String> {
        match (form.network, review.bridge()) {
            (Some(network), Some(_)) => {
                let usd = self.network_usd_micro_value(
                    network,
                    review_destination_token(review),
                    StripScale::of(review).show(amount),
                    cx,
                )?;
                Some(format!("≈ {}", railgun_ui::format_usd_micro_value(usd)))
            }
            _ => self.usd_label(review.plan().buy_token(), amount, cx),
        }
    }

    /// The warning on authorized costs of `bps` of the swap: the gas the user allows, any
    /// bridge fee, and the share of the swap they and the other costs take.
    fn authorized_cost_message(&self, review: &SwapReview, bps: u64, cx: &App) -> String {
        let buy = review.plan().buy_token();
        let gas = self.gas_money(buy, review.gas_allowance(), cx);
        // In whole percent, rounded down, as the 20% threshold reads.
        let share = format_bps_percent(bps / 100 * 100);
        let fee = bridge_fee(review);
        let mut message = if fee.is_zero() {
            format!("You allow paying up to {gas} of gas, {share} of this swap.")
        } else {
            format!(
                "You allow paying up to {gas} of gas and a {} bridge fee, {share} of this swap.",
                self.gas_money(buy, fee, cx)
            )
        };
        if GasBar::of(review).gas_exceeds() && review.gas_share_bps() > GAS_SHARE_TIGHT_BPS {
            message.push_str(" A higher minimum, or a larger amount, loses less.");
        }
        message
    }

    /// The warning on a quote that fell back from the form's gas share, whose gas estimate is
    /// then at least the swap's best case: the estimate, and what the user can do about it.
    fn gas_too_high_message(&self, review: &SwapReview, cx: &App) -> String {
        format!(
            "Gas for this swap costs about {} right now, more than the swap is worth. Try a larger amount, or wait for gas to drop.",
            self.gas_money(review.plan().buy_token(), review.gas_estimate(), cx)
        )
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
            // The fee token's spendable balance can't cover the fee: say so in its units.
            match (
                crate::root::private_action::form_error_max_immediately_spendable(error),
                &form.route.estimate_candidate,
            ) {
                (Some(spendable), Some(candidate)) => format!(
                    "the fee is more than the {} you can spend right now",
                    self.token_amount(candidate.token, spendable, cx)
                ),
                _ => error.clone(),
            }
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
            .and_then(|amount| self.usd_label(form.sell, amount, cx))
            .filter(|usd| !usd_repeats_amount(usd, &form.amount_input.read(cx).value()));
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

    /// The Buy panel: the guaranteed minimum, its token, the price check and the best case, and
    /// the gas strip. The flip button sits on the seam above it.
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
        // A quote that fell back from the form's share has no minimum the user chose.
        let fallback = review.is_some_and(|review| form.gas_share_fallback(review));
        // The minimum the order guarantees. A Bridge swap shows what arrives on its network, in
        // that network's token.
        let amount = review.filter(|_| !fallback).map(|review| {
            let minimum = strip_review(form, review).suggested_private_minimum();
            self.strip_amount(form, review, StripScale::of(review).show(minimum), cx)
        });
        // A quote's amount is a minimum, which the label says, so the amount starts at the
        // panel's edge like the Sell amount.
        let (title, balance) = match (form.network, amount.is_some()) {
            (Some(network), true) => (format!("Buy on {}, at least", network_name(network)), None),
            (Some(network), false) => (format!("Buy on {}", network_name(network)), None),
            (None, quoted) => (
                if quoted { "Buy at least" } else { "Buy" }.to_owned(),
                form.buy
                    .and_then(|buy| self.private_balance_label(form, buy, cx)),
            ),
        };
        let same_token = matches!(form.bridge_state(), BridgeState::SameToken);
        // The strip's row is drawn before the quote is ready too, so the panel keeps its height.
        let strip = review.is_some()
            || matches!(
                self.form_mode(form),
                FormMode::Setup { .. } | FormMode::Order
            );
        amount_panel(same_token, cx)
            .debug_selector(|| "swap-buy-panel".into())
            .child(app_muted_text(title))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div().flex_1().min_w_0().child(match amount {
                            Some(amount) => app_amount_text(amount).min_w_0().truncate(),
                            None => app_amount_text(if fallback { "—" } else { "0" })
                                .text_color(rgb(theme::TEXT_SUBTLE)),
                        }),
                    )
                    .children(self.render_bridge_notice(form, cx))
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
            .children(strip.then(|| self.render_gas_strip(form, review, editable, cx)))
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
            .gap_x_2();
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
                return line.child(retry_alert(
                    "swap-price-error",
                    self.quote_error_message(error, cx),
                    self.bridge_routes_retry(cx),
                    cx,
                ));
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
            BridgeState::Unreachable(provider) => {
                return line.child(retry_alert(
                    "swap-bridge-unreachable",
                    format!(
                        "{} is unreachable, and no other bridge delivers this token to {network}.",
                        provider_name(provider)
                    ),
                    self.bridge_routes_retry(cx),
                    cx,
                ));
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
                } else {
                    "Enter an amount that fits to get a quote"
                })
                .whitespace_normal(),
            ),
            QuoteState::Loading => line
                .child(Spinner::new().small())
                .child(app_muted_text("Getting a quote and checking the price…")),
            QuoteState::Failed(error) => line.child(retry_alert(
                "swap-price-error",
                self.quote_error_message(error, cx),
                retry(),
                cx,
            )),
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
                line.child(retry_alert("swap-price-error", text, retry(), cx))
            }
            QuoteState::Ready(review) => {
                // A quote that fell back from the form's share shows no amounts, as the user
                // chose none of them. The gas strip says why.
                if form.gas_share_fallback(review) {
                    return line.when(!review.price_verified(), |line| {
                        line.child(
                            app_text(UNVERIFIED_PRICE_WARNING)
                                .text_color(cx.theme().warning)
                                .whitespace_normal(),
                        )
                    });
                }
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
                // The minimum's value, and the best case, which solvers paying all gas leaves.
                let buy = review.plan().buy_token();
                let shown = strip_review(form, review);
                let up_to = self.bare_amount(buy, best_after_fees(&shown), cx);
                let minimum = shown.suggested_private_minimum();
                let amount =
                    self.strip_amount(form, review, StripScale::of(review).show(minimum), cx);
                let usd = self
                    .usd_label(buy, minimum, cx)
                    .filter(|usd| !usd_repeats_amount(usd, &amount));
                let line = line.child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_wrap()
                        .items_baseline()
                        .gap_x_1()
                        .children(usd.map(|usd| app_muted_text(format!("{usd} ·"))))
                        .child(app_muted_text("up to"))
                        .child(app_strong_text(up_to)),
                );
                match review.price() {
                    SwapPrice::Unverified => line.child(
                        app_text(UNVERIFIED_PRICE_WARNING)
                            .text_color(cx.theme().warning)
                            .whitespace_normal(),
                    ),
                    // A quote at or above the anchor is in the details.
                    SwapPrice::Verified { .. } => {
                        line.children(price_delta(review).filter(PriceDelta::below).map(|delta| {
                            div()
                                .id("swap-price-delta")
                                .flex()
                                .items_baseline()
                                .child(app_muted_text("("))
                                .child(delta.value(cx))
                                .child(app_muted_text(format!(" vs {})", delta.anchor)))
                                .when_some(delta.checked, |delta, checked| {
                                    delta.tooltip(move |window, cx| {
                                        Tooltip::new(checked.clone()).build(window, cx)
                                    })
                                })
                        }))
                    }
                }
            }
        }
    }

    /// A Bridge swap's warning button, left of the Buy selector, while one provider couldn't be
    /// asked: a tooltip on hover, and a popover with Retry on click or from the keyboard. The
    /// price line says so instead while only that provider could deliver the Buy token.
    fn render_bridge_notice(
        &self,
        form: &SwapForm,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let provider = form.bridge_unavailable()?;
        if matches!(form.bridge_state(), BridgeState::Unreachable(_)) {
            return None;
        }
        let provider = provider_name(provider);
        let view = cx.entity();
        let open_view = view.clone();
        let busy = self.busy();
        let open = form.bridge.notice_open;
        Some(
            div()
                .id("swap-bridge-partial")
                .debug_selector(|| "swap-bridge-partial".into())
                .flex_none()
                .when(!open, |this| {
                    this.tooltip(move |window, cx| {
                        Tooltip::element(move |window, _| bridge_notice_card(provider, window))
                            .build(window, cx)
                    })
                })
                .child(
                    Popover::new("swap-bridge-notice")
                        .anchor(Anchor::TopRight)
                        .open(open)
                        .on_open_change(move |open, _, cx| {
                            open_view.update(cx, |view, cx| {
                                view.set_bridge_notice_open(*open, cx);
                            });
                        })
                        .trigger(
                            app_button_base("swap-bridge-notice-trigger")
                                .ghost()
                                .xsmall()
                                .icon(
                                    Icon::new(IconName::TriangleAlert)
                                        .text_color(rgb(theme::WARNING)),
                                )
                                .accessibility_label(format!("{provider} is unreachable"))
                                .debug_selector(|| "swap-bridge-notice-trigger".into()),
                        )
                        .content(move |_, window, _| {
                            bridge_notice_card(provider, window).child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .child(bridge_routes_retry_button(view.clone(), busy)),
                            )
                        }),
                ),
        )
    }

    /// Retry for a Bridge swap's routes.
    fn bridge_routes_retry(&self, cx: &Context<'_, Self>) -> Button {
        bridge_routes_retry_button(cx.entity(), self.busy())
    }

    fn quote_error_message(&self, error: &eyre::Report, cx: &App) -> String {
        match error.downcast_ref::<OrderLimitError>() {
            Some(OrderLimitError::HookCostExceedsOutput {
                buy_token,
                gas_estimate,
                ..
            }) => format!(
                "Sell amount is too small. Estimated cost: {}",
                self.token_amount(*buy_token, *gas_estimate, cx)
            ),
            _ => bridge_unreachable(error).unwrap_or_else(|| format!("{error:#}")),
        }
    }

    /// The gas strip across the bottom of the Buy panel: a row with the share presets and the
    /// edit button, and under it what a ready quote adds. Until a quote is ready, the row shows
    /// the form's share, and a preset chosen there prices the next quote.
    fn render_gas_strip(
        &self,
        form: &SwapForm,
        review: Option<&Arc<SwapReview>>,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let shown = review.map(|review| strip_review(form, review));
        let selected = form.selected_gas_preset(review);
        let estimate_money = shown
            .as_ref()
            .map(|shown| self.gas_money(shown.plan().buy_token(), shown.gas_estimate(), cx));
        // The bar and the Minimum field show the quote's amounts.
        let open = review.is_some() && form.gas_bar_open();
        let presets = ButtonGroup::new("swap-gas-presets")
            .outline()
            .compact()
            .disabled(!editable)
            .children(GasPreset::ALL.into_iter().map(|preset| {
                // A preset that would leave no positive minimum can't be chosen.
                let priced = review.map(|review| review.with_gas_share(preset.share_bps()));
                let unavailable = priced.as_ref().is_some_and(Result::is_err);
                // A preset whose minimum solvers are unlikely to accept carries a warning.
                let warning = priced
                    .and_then(Result::ok)
                    .and_then(|priced| self.fill_warning(&priced, cx));
                let button = app_segment_button(
                    preset.id(),
                    preset.name(),
                    Some(preset) == selected,
                    !editable || unavailable,
                    warning.is_some().then(|| {
                        Icon::new(IconName::TriangleAlert)
                            .xsmall()
                            .text_color(rgb(theme::WARNING))
                            .into_any_element()
                    }),
                )
                .debug_selector(|| preset.id().into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_gas_preset(preset, window, cx);
                }));
                match (&estimate_money, warning) {
                    (Some(estimate_money), _) if unavailable => button.tooltip(format!(
                        "Gas (≈ {estimate_money}) is more than this swap returns"
                    )),
                    (_, Some(warning)) => button.tooltip(warning),
                    _ => button,
                }
            }));
        let edit = app_button_base("swap-gas-edit-minimum")
            .ghost()
            .xsmall()
            .icon(Icon::new(RailgunActionIcon::Pencil))
            .accessibility_label("Set a custom minimum")
            .tooltip("Set a custom minimum")
            .selected(open)
            .disabled(!editable || review.is_none())
            .debug_selector(|| "swap-gas-edit-minimum".into())
            .on_click(cx.listener(|this, _, window, cx| {
                this.edit_gas_minimum(window, cx);
            }));
        let strip = div()
            .debug_selector(|| "swap-gas-strip".into())
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
                    .flex_wrap()
                    .items_center()
                    .gap_x_2()
                    .gap_y_1()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .child(app_muted_text("Minimum"))
                            .child(self.render_gas_help(form, shown.as_deref(), cx)),
                    )
                    // The presets keep their labels' width. A row too narrow for them wraps
                    // them under the label.
                    .child(
                        div()
                            .ml_auto()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .child(presets)
                            .child(edit),
                    ),
            );
        let (Some(review), Some(shown)) = (review, &shown) else {
            return strip;
        };
        self.render_gas_quote(strip, form, review, shown, editable, cx)
    }

    /// What a ready quote adds under the gas strip's row: the bar and the Minimum field while
    /// they are open, a Bridge swap's pending refresh, and the warning on high authorized
    /// costs. Amounts are numbers only, as the Buy card names its token; a Bridge swap's are
    /// its destination amounts.
    fn render_gas_quote(
        &self,
        strip: gpui::Div,
        form: &SwapForm,
        review: &Arc<SwapReview>,
        shown: &SwapReview,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let open = form.gas_bar_open();
        let minimum_field = open.then(|| {
            let usd = self
                .strip_usd_label(form, review, shown.suggested_private_minimum(), cx)
                .filter(|usd| !usd_repeats_amount(usd, &form.gas_minimum_input.read(cx).value()));
            div()
                .debug_selector(|| "swap-gas-minimum-field".into())
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(app_muted_text("Minimum").flex_none())
                .child(
                    div().w(rems(10.)).flex_none().child(
                        app_input(&form.gas_minimum_input)
                            .small()
                            .suffix(app_muted_text(self.strip_symbol(form, review, cx)))
                            .disabled(!editable),
                    ),
                )
                .children(usd.map(app_muted_text))
        });
        let quoting = matches!(
            self.form_mode(form),
            FormMode::Setup { .. } | FormMode::Order
        );
        // A share the bridge leg wasn't quoted at: the strip's amounts are estimates until the
        // refresh ends, and a failed refresh says why and offers a full quote.
        let refresh = form
            .gas_share_pending(review)
            .then(|| match &form.bridge_quote_error {
                None => div().w_full().child(
                    app_muted_text(
                        "The destination amount is an estimate while the bridge quote updates.",
                    )
                    .debug_selector(|| "swap-gas-pending".into())
                    .whitespace_normal(),
                ),
                Some(error) => retry_alert(
                    "swap-bridge-quote-error",
                    error.to_string(),
                    app_button("swap-bridge-quote-retry", "Retry")
                        .debug_selector(|| "swap-bridge-quote-retry".into())
                        .outline()
                        .small()
                        .flex_none()
                        .disabled(!editable || self.busy())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.retry_quote(window, cx);
                        })),
                    cx,
                ),
            });
        // A quote that fell back from the form's share says so instead of asking consent to
        // costs the user didn't choose.
        let warning = if form.gas_share_fallback(review) {
            quoting.then(|| {
                div()
                    .w_full()
                    .min_w_0()
                    .debug_selector(|| "swap-gas-too-high".into())
                    .child(
                        Alert::warning("swap-gas-too-high", self.gas_too_high_message(review, cx))
                            .small()
                            .min_w_0(),
                    )
            })
        } else {
            authorized_high_cost(review)
                .filter(|_| quoting)
                .and_then(|bps| {
                    Self::render_cost_acknowledgement(
                        form,
                        Some(self.authorized_cost_message(review, bps, cx)),
                        editable,
                        cx,
                    )
                })
        };
        strip
            .children(open.then(|| self.render_gas_bar(form, review, shown, editable, cx)))
            .children(minimum_field)
            .children(refresh)
            .children(warning)
            .children(self.render_source_return(review, cx))
    }

    /// The bar over the gas share, a `Slider` whose value is the knob's position in percent
    /// from "you pay all gas". A drawn track sits under the slider's transparent one: dotted
    /// from the start to the knob, the loss the minimum refuses, and solid from the knob to the
    /// end, where the swap can end up, with ticks at the other presets. Its focus takes the
    /// arrows in 5% steps, Home and End.
    fn render_gas_bar(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        shown: &SwapReview,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let bar = GasBar::of(shown);
        let position = f32::from(bar.position_bps(shown.gas_share_bps())) / 10_000.;
        let (start, start_label, end) = self.gas_bar_ends(form, review, shown, cx);
        let ticks = GasPreset::ALL
            .into_iter()
            .map(GasPreset::share_bps)
            .filter(|bps| *bps != shown.gas_share_bps() && *bps <= bar.max_share_bps())
            .map(|bps| f32::from(bar.position_bps(bps)) / 10_000.)
            .collect::<Vec<_>>();
        let (solid, muted, ring) = (
            cx.theme().primary,
            cx.theme().muted_foreground,
            cx.theme().ring,
        );
        let track = div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .right_0()
            .flex()
            .items_center()
            .child(
                div()
                    .relative()
                    .w_full()
                    .h_1p5()
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .w(relative(position))
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .w_full()
                                    .h_0()
                                    .border_t_2()
                                    .border_dashed()
                                    .border_color(muted),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(relative(position))
                            .right_0()
                            .rounded_full()
                            .bg(solid),
                    )
                    .children(ticks.into_iter().map(|tick| {
                        div()
                            .absolute()
                            .left(relative(tick))
                            .top(-rems(0.1875))
                            .w_0p5()
                            .h_3()
                            .bg(muted)
                    }))
                    // A disabled slider draws no thumb, so the knob is drawn here.
                    .when(!editable, |track| {
                        track.child(
                            div()
                                .absolute()
                                .left(relative(position))
                                .ml(-rems(0.5))
                                .top(-rems(0.3125))
                                .size_4()
                                .rounded_full()
                                .bg(solid),
                        )
                    }),
            );
        let end_label = |amount: String, label: &'static str| {
            div()
                .flex()
                .flex_col()
                .child(app_strong_text(amount).text_xs())
                .child(app_muted_text(label).text_xs())
        };
        div()
            .id("swap-gas-bar")
            .debug_selector(|| "swap-gas-bar".into())
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .px_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(gpui::transparent_black())
            .when(editable, |element| {
                element
                    .track_focus(&form.gas_bar_focus)
                    .key_context(GAS_BAR_KEY_CONTEXT)
                    .focus_visible(move |style| style.border_color(ring))
                    .on_action(cx.listener(|this, _: &GasBarLeft, window, cx| {
                        this.step_gas_bar(|step| step.saturating_sub(GAS_BAR_STEP), window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &GasBarRight, window, cx| {
                        this.step_gas_bar(|step| step + GAS_BAR_STEP, window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &GasBarStart, window, cx| {
                        this.step_gas_bar(|_| 0, window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &GasBarEnd, window, cx| {
                        this.step_gas_bar(|_| 100, window, cx);
                    }))
            })
            .child(
                div().relative().w_full().h_6().child(track).child(
                    Slider::new(&form.gas_slider)
                        .reverse()
                        .bg(gpui::transparent_black())
                        .disabled(!editable),
                ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .justify_between()
                    .gap_2()
                    .child(end_label(start, start_label))
                    .child(end_label(end, "most you could get").items_end()),
            )
    }

    /// The gas bar's ends: its start and what it means, and the best case at its end. The start
    /// is the minimum if the user pays all gas, or zero when the gas exceeds the swap.
    fn gas_bar_ends(
        &self,
        form: &SwapForm,
        review: &SwapReview,
        shown: &SwapReview,
        cx: &App,
    ) -> (String, &'static str, String) {
        let scale = StripScale::of(review);
        let loose = shown
            .with_gas_share(GAS_SHARE_LOOSE_BPS)
            .ok()
            .filter(|_| !GasBar::of(shown).gas_exceeds());
        let (start, label) = loose.map_or((U256::ZERO, "gas exceeds the swap"), |loose| {
            (loose.suggested_private_minimum(), "fills most easily")
        });
        (
            self.strip_amount(form, review, scale.show(start), cx),
            label,
            self.strip_amount(form, review, scale.show(best_after_fees(shown)), cx),
        )
    }

    /// The strip's info button with its "Why pay less than the full gas?" explanation, the
    /// current quote's figures only: a tooltip on hover, and a popover on click or from the
    /// keyboard. The bar's legend is in it while the bar is open. Without a ready quote the
    /// explanation has no figures.
    fn render_gas_help(
        &self,
        form: &SwapForm,
        shown: Option<&SwapReview>,
        cx: &Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let trigger = app_button_base("swap-gas-help-trigger")
            .ghost()
            .xsmall()
            .icon(IconName::Info)
            .accessibility_label(GAS_HELP_TITLE)
            .debug_selector(|| "swap-gas-help-trigger".into());
        let view = cx.entity();
        let text = self.gas_help_text(shown, cx);
        let legend = form
            .gas_bar_open()
            .then(|| (cx.theme().primary, cx.theme().muted_foreground));
        let tooltip_text = text.clone();
        div()
            .id("swap-gas-help-hover")
            .flex_none()
            .when(!form.gas_help_open, |this| {
                this.tooltip(move |window, cx| {
                    let text = tooltip_text.clone();
                    Tooltip::element(move |window, _| gas_help_card(&text, legend, window))
                        .build(window, cx)
                })
            })
            .child(
                Popover::new("swap-gas-help")
                    .anchor(Anchor::TopLeft)
                    .open(form.gas_help_open)
                    .on_open_change(move |open, _, cx| {
                        view.update(cx, |view, cx| view.set_gas_help_open(*open, cx));
                    })
                    .trigger(trigger)
                    .content(move |_, window, _| {
                        gas_help_card(&text, legend, window)
                            .debug_selector(|| "swap-gas-help-content".into())
                    }),
            )
    }

    /// Why `review`'s minimum is unlikely to fill: the gas it allows, with the price tolerance a
    /// solver can also draw on, is below what settling tends to cost. `None` otherwise.
    fn fill_warning(&self, review: &SwapReview, cx: &App) -> Option<String> {
        let bps = U256::from(10_000_u64);
        let realistic = review.gas_estimate() * U256::from(REALISTIC_GAS_BPS) / bps;
        let tolerance = review.best_case() * U256::from(review.slippage_bps()) / bps;
        let allowed = review.gas_allowance();
        (allowed.saturating_add(tolerance) < realistic).then(|| {
            let buy = review.plan().buy_token();
            format!(
                "Unlikely to fill: settling this swap usually costs about {}, and this minimum leaves solvers {} for gas",
                self.gas_money(buy, realistic, cx),
                self.gas_money(buy, allowed, cx)
            )
        })
    }

    /// The info popover's paragraphs: the live gas estimate and what a lower minimum does;
    /// what adds gas; that solvers don't have to cover it; that the minimum is the only
    /// guarantee; and the fees that apply either way.
    fn gas_help_text(&self, review: Option<&SwapReview>, cx: &App) -> [String; 5] {
        let solvers = "Solvers compete to fill orders, and on small orders often pay all of the gas. They don't have to.".to_owned();
        let minimum = "Your minimum is the only amount the swap guarantees. Paying less of the gas raises it. If no solver accepts before the order expires, nothing is swapped and your funds stay private.".to_owned();
        // Without a quote there are no figures to show.
        let Some(review) = review else {
            return [
                "A private swap costs more in network fees than a plain trade. Solvers usually cover some of it. The lower your minimum, the easier your order fills.".to_owned(),
                "The fees pay for the trade and for unshielding before it, and for shielding or the bridge hand-off after it.".to_owned(),
                solvers,
                minimum,
                "Railgun's fees and CoW's fee apply either way.".to_owned(),
            ];
        };
        let chain = self.chain_label();
        let steps = match review.plan().delivery() {
            SwapDelivery::Reshield => {
                format!("two steps on {chain}: unshielding before the trade and shielding after it")
            }
            SwapDelivery::External { .. } => {
                format!("a step on {chain}: unshielding before the trade")
            }
            SwapDelivery::Bridge(_) => format!(
                "two steps on {chain}: unshielding before the trade and handing the tokens to the bridge after it"
            ),
        };
        let percent = |bps: U256| format_bps_percent(u64::try_from(bps).unwrap_or(u64::MAX));
        let (unshield, shield) = (
            percent(review.unshield_fee_bps()),
            percent(review.shield_fee_bps()),
        );
        let railgun = match review.plan().delivery() {
            SwapDelivery::Reshield if unshield == shield => {
                format!("Railgun's two {unshield} fees")
            }
            SwapDelivery::Reshield => {
                format!("Railgun's {unshield} unshield and {shield} shield fees")
            }
            _ => format!("Railgun's {unshield} unshield fee"),
        };
        [
            // An ordering, not a probability: a solver that fills a higher minimum fills a
            // lower one.
            format!(
                "This swap costs about {} in network fees. Solvers usually cover some of it. The lower your minimum, the easier your order fills.",
                self.gas_money(review.plan().buy_token(), review.gas_estimate(), cx)
            ),
            format!("The fees pay for the trade and for {steps}."),
            solvers,
            minimum,
            format!("{railgun} and CoW's fee apply either way."),
        ]
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
                // A share still being quoted shows estimated terms, which can't be accepted.
                .disabled(self.busy() || form.gas_share_pending(review))
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    if let Some(form) = this.form.as_mut() {
                        form.price_acknowledged = *checked;
                        form.error = None;
                    }
                    cx.notify();
                }))
        })
    }

    /// The wrapping warning on high authorized costs, with Swap anyway inside it.
    fn render_cost_acknowledgement(
        form: &SwapForm,
        message: Option<String>,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let message = message?;
        // A share still being quoted shows estimated terms, which can't be accepted.
        let provisional = matches!(&form.quote, QuoteState::Ready(review)
            if form.gas_share_pending(review));
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
                            Alert::warning("swap-high-costs", message)
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
                                .disabled(!editable || provisional)
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
    /// receiver: why it can't be used, and for native output, that a contract wallet may not
    /// accept it. A retry keeps its attempt's delivery, and a set-up native pair keeps its
    /// Public address.
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
                        .children(problem)
                        .children(native),
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

    /// A Bridge swap's Provider row: the select, its hint button, and the provider's refusal of
    /// the amount beneath, then Across's Surplus row.
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
        // A token only one provider delivers leaves nothing to choose.
        let alternative = matches!(
            state,
            BridgeState::Ready {
                across: true,
                near: true,
                ..
            }
        );
        let select = Select::new(&form.provider_select)
            .w_full()
            .placeholder(placeholder)
            .disabled(!editable || !alternative);
        let mut rejection = None;
        let mut rows = Vec::new();
        if let BridgeState::Ready { provider, .. } = state {
            if let QuoteState::Failed(error) = &form.quote
                && let Some(message) = bridge_rejection(error)
            {
                rejection = Some(
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
                            app_text(message)
                                .text_xs()
                                .min_w_0()
                                .text_color(cx.theme().danger)
                                .whitespace_normal(),
                        ),
                );
            }
            if provider == BridgeProvider::Across {
                rows.push(self.render_surplus_row(form, editable, cx));
            }
        }
        let provider = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(labeled_row(
                "Provider",
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1().min_w_0().child(select))
                    .children(self.render_provider_hint(form, cx)),
            ))
            .when_some(rejection, |row, rejection| {
                row.child(
                    // Under the select, past the label and the row's gap.
                    div().pl(rems(ACCOUNT_LABEL_WIDTH + 0.5)).child(rejection),
                )
            });
        rows.insert(0, provider);
        rows
    }

    /// The chosen provider's hint button, right of its select: a tooltip on hover, and a
    /// popover on click or from the keyboard. The card says why the provider was switched, how
    /// it pays out, and its disclaimer. NEAR Intents' is a warning.
    fn render_provider_hint(
        &self,
        form: &SwapForm,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let BridgeState::Ready {
            destination,
            provider,
            switched,
            near,
            ..
        } = form.bridge_state()
        else {
            return None;
        };
        let mut notes = Vec::<SharedString>::new();
        // The only provider's select is disabled, so the card says why there is no choice.
        if provider == BridgeProvider::Across && !near {
            notes.push(
                if form.bridge_unavailable() == Some(BridgeProvider::NearIntents) {
                    "NEAR Intents is unreachable, so this swap uses Across.".into()
                } else {
                    format!(
                        "NEAR Intents doesn't deliver {}, so this swap uses Across.",
                        destination.symbol
                    )
                    .into()
                },
            );
        }
        if switched {
            notes.push(
                if form.bridge_unavailable() == Some(BridgeProvider::Across) {
                    "Across is unreachable, so this swap uses NEAR Intents.".into()
                } else {
                    format!(
                        "Across doesn't deliver {}, so this swap uses NEAR Intents.",
                        destination.symbol
                    )
                    .into()
                },
            );
        }
        let disclaimer: SharedString = match provider {
            BridgeProvider::NearIntents => {
                notes.push(
                    format!(
                        "Via {} on {}. The whole payout is converted, surplus included.",
                        self.token_symbol(destination.intermediate, cx),
                        self.chain_label()
                    )
                    .into(),
                );
                NEAR_INTENTS_DISCLAIMER.into()
            }
            BridgeProvider::Across => {
                // The SpokePool unwraps WETH only for receivers without code.
                if self.across_delivers_native(form, destination.destination_token, cx) {
                    notes.push(
                        "Across delivers ETH to wallets. Contract receivers get WETH.".into(),
                    );
                }
                notes.push(self.surplus_note(form, destination.intermediate, cx).into());
                format!(
                    "If the deposit isn't filled before it expires, Across refunds it to the stealth account on {}, usually within a few hours. Recovering it costs a shield fee and a broadcaster fee.",
                    self.chain_label()
                )
                .into()
            }
        };
        let icon = match provider {
            BridgeProvider::NearIntents => {
                Icon::new(IconName::TriangleAlert).text_color(rgb(theme::WARNING))
            }
            BridgeProvider::Across => Icon::new(IconName::Info),
        };
        let view = cx.entity();
        let open = form.bridge.provider_hint_open;
        let (tooltip_notes, tooltip_disclaimer) = (notes.clone(), disclaimer.clone());
        Some(
            div()
                .id("swap-provider-hint")
                .debug_selector(|| "swap-provider-hint".into())
                .flex_none()
                .when(!open, |this| {
                    this.tooltip(move |window, cx| {
                        let (notes, disclaimer) =
                            (tooltip_notes.clone(), tooltip_disclaimer.clone());
                        Tooltip::element(move |window, _| {
                            provider_hint_card(provider, &notes, &disclaimer, window)
                        })
                        .build(window, cx)
                    })
                })
                .child(
                    Popover::new("swap-provider-hint-popover")
                        .anchor(Anchor::TopRight)
                        .open(open)
                        .on_open_change(move |open, _, cx| {
                            view.update(cx, |view, cx| view.set_provider_hint_open(*open, cx));
                        })
                        .trigger(
                            app_button_base("swap-provider-hint-trigger")
                                .ghost()
                                .xsmall()
                                .icon(icon)
                                .accessibility_label(format!("About {}", provider_name(provider)))
                                .debug_selector(|| "swap-provider-hint-trigger".into()),
                        )
                        .content(move |_, window, _| {
                            provider_hint_card(provider, &notes, &disclaimer, window)
                                .debug_selector(|| "swap-provider-hint-content".into())
                        }),
                ),
        )
    }

    /// Across's choice for what `CoW` pays above the deposit: reshield it, or leave it in the
    /// stealth account. The provider's hint explains it.
    fn render_surplus_row(
        &self,
        form: &SwapForm,
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
        labeled_row(
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
        )
    }

    /// What the Surplus row's choice is about, with the deposit of `bought` once quoted.
    fn surplus_note(&self, form: &SwapForm, bought: Address, cx: &App) -> String {
        // Bridge orders buy exactly the deposit, the quote's minimum.
        let deposit = match &form.quote {
            QuoteState::Ready(review) if review.bridge().is_some() => format!(
                "the {} deposit",
                self.token_amount(bought, review.suggested_private_minimum(), cx)
            ),
            _ => "the deposit".to_owned(),
        };
        format!(
            "Surplus is anything CoW pays above {deposit}. Reshielding it costs the shield fee."
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

    /// The rate and total costs; expanded, Railgun's and `CoW`'s fees, the gas estimate and the
    /// gas the user pays, the price tolerance, and the order's validity.
    fn render_details(
        &self,
        form: &SwapForm,
        review: &Arc<SwapReview>,
        with_setup: bool,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Collapsible {
        let plan = review.plan();
        let (sell, buy) = (plan.sell_token(), plan.buy_token());
        let costs = total_cost(review);
        // In money, or as the token amount without a cached rate.
        let costs_label = self.usd_micro_value(buy, costs, cx).map_or_else(
            || {
                self.with_usd(
                    format!("≈ {} in costs", self.token_amount(buy, costs, cx)),
                    buy,
                    costs,
                    cx,
                )
            },
            |usd| format!("≈ {} in costs", railgun_ui::format_usd_micro_value(usd)),
        );
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
                            .child(app_muted_text(costs_label).flex_none())
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
        let shown = strip_review(form, review);
        let unshield_fee = plan.amount().saturating_sub(review.sell_amount());
        let percent = |bps: U256| format_bps_percent(u64::try_from(bps).unwrap_or(u64::MAX));
        let with_rate = |value: String, rate: String| {
            div()
                .flex()
                .items_baseline()
                .gap_1()
                .child(app_text(value))
                .child(app_muted_text(rate))
        };
        let content = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(detail_row(
                "Railgun unshield",
                with_rate(
                    self.token_amount(sell, unshield_fee, cx),
                    percent(review.unshield_fee_bps()),
                ),
                true,
                Some("Railgun's fee on the amount sold, taken before the order.".into()),
            ))
            .children(review.cow_fee().map(|fee| {
                detail_row(
                    "CoW fee",
                    app_text(self.token_amount(buy, fee, cx)),
                    true,
                    Some("CoW Protocol's fee, already out of the quote.".into()),
                )
            }))
            .child(detail_row(
                format!("Gas at {} gwei", format_gwei(review.gas_price_wei())),
                app_text(format!(
                    "≈ {}",
                    self.gas_money(buy, review.gas_estimate(), cx)
                )),
                true,
                Some("Gas for the trade and the private steps, at the current network gas price with a 25% cushion. Solvers may charge less.".into()),
            ))
            .child(detail_row(
                "Gas you pay",
                with_rate(
                    format!("up to {}", self.gas_money(buy, shown.gas_allowance(), cx)),
                    format_bps_percent(u64::from(shown.gas_share_bps())),
                ),
                true,
                None,
            ))
            .when(plan.delivery() == SwapDelivery::Reshield, |content| {
                content.child(detail_row(
                    "Railgun shield",
                    with_rate(
                        percent(review.shield_fee_bps()),
                        "of what you receive".to_owned(),
                    ),
                    true,
                    Some("Railgun's fee on shielding the bought tokens to your private balance.".into()),
                ))
            })
            .children(self.bridge_detail_rows(review, cx))
            // Below the anchor, the Buy card shows it instead.
            .children(
                price_delta(review)
                    .filter(|delta| review.bridge().is_none() && !delta.below())
                    .map(|delta| {
                        detail_row(
                            if delta.anchor == "Chainlink" {
                                "Price vs Chainlink"
                            } else {
                                "Price vs anchor"
                            },
                            delta.value(cx),
                            false,
                            delta.checked,
                        )
                    }),
            )
            .child(detail_row(
                "Price tolerance",
                Self::render_slippage(form, editable, cx),
                false,
                None,
            ))
            .child(Self::render_validity(form, review, with_setup, editable, cx));
        details.content(content)
    }

    /// Order valid for: 10, 30 or 60 minutes, in a popover. Choosing another quotes the swap
    /// again. Bridge delivery keeps the profile's window, which its bridge quote needs, so it
    /// shows that window and why.
    fn render_validity(
        form: &SwapForm,
        review: &SwapReview,
        with_setup: bool,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let bridge = form.network.is_some();
        let minutes = if bridge {
            review.valid_for()
        } else {
            form.valid_for
        }
        .as_secs()
            / 60;
        let label = if with_setup {
            format!("{minutes} minutes after setup")
        } else {
            format!("{minutes} minutes")
        };
        let value = if bridge {
            app_text(label).into_any_element()
        } else {
            let view = cx.entity();
            Popover::new("swap-validity")
                .anchor(Anchor::TopRight)
                .trigger(
                    app_button_base("swap-validity-trigger")
                        .ghost()
                        .xsmall()
                        .dropdown_caret(true)
                        .accessibility_label("Order valid for")
                        .child(app_button_label(label)),
                )
                .content(move |_, _, _| {
                    div()
                        .w(rems(16.))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(app_strong_text("Order valid for"))
                        .child(
                            ButtonGroup::new("swap-validity-choices")
                                .outline()
                                .compact()
                                .children(VALIDITY_MINUTES.into_iter().map(|choice| {
                                    let view = view.clone();
                                    app_segment_button(
                                        SharedString::from(format!("swap-validity-{choice}")),
                                        format!("{choice} min"),
                                        minutes == choice,
                                        !editable,
                                        None,
                                    )
                                    .on_click(move |_, window, cx| {
                                        view.update(cx, |view, cx| {
                                            view.set_valid_for(
                                                Duration::from_mins(choice),
                                                window,
                                                cx,
                                            );
                                        });
                                    })
                                })),
                        )
                        .child(
                            app_muted_text(
                                "A longer window gives solvers more time to cover the gas, and keeps the published unshield runnable for longer.",
                            )
                            .whitespace_normal(),
                        )
                })
                .into_any_element()
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(detail_row("Order valid for", value, false, None))
            .when(bridge, |row| {
                row.child(
                    app_muted_text(format!(
                        "Bridge swaps keep the {minutes}-minute window their bridge quote needs."
                    ))
                    .debug_selector(|| "swap-validity-locked".into())
                    .text_xs()
                    .whitespace_normal(),
                )
            })
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

    /// The price tolerance presets in a popover. Choosing another preset quotes the swap again.
    /// The quote details, this popover among them, hide until a new quote is ready.
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
                    .accessibility_label("Price tolerance")
                    .child(app_button_label(format_bps_percent(u64::from(selected)))),
            )
            .content(move |_, _, _| {
                div()
                    .w(rems(16.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(app_strong_text("Price tolerance"))
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
                            "A lower tolerance fills less often. An unfilled order costs only the setup.",
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

/// The quote against the anchor price, and the reading behind it.
struct PriceDelta {
    /// In basis points. None when the amounts give no ratio.
    bps: Option<i64>,
    /// "Chainlink" only when every anchor reading came from a Chainlink round.
    anchor: &'static str,
    checked: Option<String>,
}

impl PriceDelta {
    /// The quote is below the anchor, as shown: `format_bps_percent` rounds nothing away.
    fn below(&self) -> bool {
        self.bps.is_some_and(|bps| bps < 0)
    }

    /// "−0.4%" in the color of its sign, or that the quote is within range.
    fn value(&self, cx: &App) -> gpui::Div {
        let Some(bps) = self.bps else {
            return app_muted_text("within range");
        };
        let color = match bps.cmp(&0) {
            std::cmp::Ordering::Less => cx.theme().danger,
            std::cmp::Ordering::Equal => cx.theme().muted_foreground,
            std::cmp::Ordering::Greater => cx.theme().success,
        };
        app_text(format!(
            "{}{}",
            if bps < 0 { "−" } else { "+" },
            format_bps_percent(bps.unsigned_abs())
        ))
        .text_color(color)
    }
}

fn price_delta(review: &SwapReview) -> Option<PriceDelta> {
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
    let checked = observations
        .iter()
        .map(|observation| observation.block.number)
        .max()
        .map(|block| format!("Checked against {anchor} at block {block}"));
    Some(PriceDelta {
        bps: quote_anchor_delta_bps(review.quote().buy_amount, expected),
        anchor,
        checked,
    })
}

/// Whether a USD value reads as the amount beside it, as "≈ $10.00" does beside "10": the same
/// number once "≈", "$", thousands separators and trailing zeros are dropped.
fn usd_repeats_amount(usd: &str, amount: &str) -> bool {
    let number = |text: &str| {
        let text = text
            .chars()
            .filter(|symbol| !matches!(*symbol, '≈' | '$' | ',') && !symbol.is_whitespace())
            .collect::<String>();
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text
        }
    };
    number(usd) == number(amount)
}

/// The Returned row's hint: where the surplus above a Bridge order's `deposit` goes on `chain`,
/// what comes off it, and that there may be none.
fn source_return_hint(review: &SwapReview, deposit: &str, chain: &str) -> String {
    match review.plan().delivery() {
        SwapDelivery::Bridge(BridgeDelivery {
            surplus: BridgeSurplus::Reshield,
            ..
        }) => format!(
            "Anything CoW pays above the {deposit} deposit is reshielded to your private balance, after estimated gas and the shield fee. It may be zero."
        ),
        _ => format!(
            "Anything CoW pays above the {deposit} deposit is kept in your stealth account on {chain}, after estimated gas. It may be zero."
        ),
    }
}

/// The stepper's explanation of a new swap's two steps.
fn swap_steps_hint() -> SpendAuthorizationHint {
    SpendAuthorizationHint::new(
        "Two steps",
        [
            "1. Set up stealth account. A broadcaster creates a one-time account for this swap, and you pay the setup fee.",
            "2. Place order. Once setup confirms, the wallet checks the quote again and asks you to confirm. If the terms changed, you review them first.",
            "With \"Just this spend\", you enter the password again for step 2.",
        ],
    )
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

/// The unshield fee, converted at the best-case rate, and every deduction from the best case
/// down to `received`. The best case already counts `CoW`'s network fee as output.
fn swap_fees_to(review: &SwapReview, received: U256) -> U256 {
    let unshield_fee = review.plan().amount().saturating_sub(review.sell_amount());
    swap_total_cost(
        unshield_fee,
        review
            .quote()
            .sell_amount
            .saturating_add(review.quote().fee_amount),
        review.best_case(),
        received,
    )
}

/// Costs the user authorizes, in basis points of the swap: the swap amount less the
/// `minimum`, both in the Buy token at the quoted trading rate. The swap amount is the `best`
/// case of the order's `sold` amount, scaled to the `spent` amount before the unshield fee.
fn authorized_cost_bps(best: U256, spent: U256, sold: U256, minimum: U256) -> u64 {
    if sold.is_zero() {
        return 0;
    }
    let swap = best.saturating_mul(spent) / sold;
    if swap.is_zero() {
        return 0;
    }
    (swap
        .saturating_sub(minimum)
        .saturating_mul(U256::from(10_000_u32))
        / swap)
        .saturating_to::<u64>()
}

/// A review's authorized costs, when they are high enough to need Swap anyway. A Bridge
/// swap's minimum is its destination minimum, valued as the deposit less the bridge fee.
fn authorized_high_cost(review: &SwapReview) -> Option<u64> {
    let minimum = review
        .suggested_private_minimum()
        .saturating_sub(bridge_fee(review));
    let bps = authorized_cost_bps(
        review.best_case(),
        review.plan().amount(),
        review.sell_amount(),
        minimum,
    );
    (bps >= AUTHORIZED_COST_WARNING_BPS).then_some(bps)
}

/// The review the gas strip shows: the quote at the form's share. A Bridge swap prices a new
/// share locally until its bridge leg is quoted again. A share the quote can't support shows
/// the quote as it was priced, at Tight.
fn strip_review(form: &SwapForm, review: &Arc<SwapReview>) -> Arc<SwapReview> {
    if review.gas_share_bps() == form.gas_share_bps {
        return Arc::clone(review);
    }
    review
        .with_gas_share(form.gas_share_bps)
        .map_or_else(|_| Arc::clone(review), Arc::new)
}

/// The best case less Railgun's fee on the output: the shield fee, for Private delivery.
fn best_after_fees(review: &SwapReview) -> U256 {
    let best = review.best_case();
    match review.plan().delivery() {
        SwapDelivery::Reshield => best.saturating_sub(review.shield_fee_on_output(best)),
        _ => best,
    }
}

/// The gas bar of a review. Positions are linear in the minimum: the bar ends at the best case
/// after the price tolerance, where solvers pay all gas, and starts where the user pays all of
/// it, or at zero when the gas estimate exceeds the swap.
#[derive(Clone, Copy)]
struct GasBar {
    /// The best case after the price tolerance, before the shield fee.
    tolerated: U256,
    estimate: U256,
    /// The gas the bar spans: the estimate, or the tolerated best case when gas exceeds it.
    span: U256,
}

impl GasBar {
    fn of(review: &SwapReview) -> Self {
        let tolerance = U256::from(10_000_u32.saturating_sub(review.slippage_bps()));
        let tolerated = review.best_case().saturating_mul(tolerance) / U256::from(10_000_u32);
        let estimate = review.gas_estimate();
        Self {
            tolerated,
            estimate,
            span: estimate.min(tolerated),
        }
    }

    /// The gas estimate leaves nothing of the swap, so the bar starts at zero.
    fn gas_exceeds(&self) -> bool {
        self.estimate >= self.tolerated
    }

    /// The largest share that leaves a positive minimum.
    fn max_share_bps(&self) -> u16 {
        if !self.gas_exceeds() || self.estimate.is_zero() {
            return GAS_SHARE_LOOSE_BPS;
        }
        // The allowance, `ceil(estimate * share / 10000)`, must stay below the tolerated best.
        (self.tolerated.saturating_sub(U256::ONE) * U256::from(10_000_u32) / self.estimate)
            .saturating_to::<u16>()
            .min(GAS_SHARE_LOOSE_BPS)
    }

    /// The knob's position at a share, in basis points of the bar from its start.
    fn position_bps(&self, share_bps: u16) -> u16 {
        if self.span.is_zero() {
            return 10_000;
        }
        let allowance =
            self.estimate.saturating_mul(U256::from(share_bps)) / U256::from(10_000_u32);
        let from_end = (allowance.saturating_mul(U256::from(10_000_u32)) / self.span)
            .min(U256::from(10_000_u32))
            .saturating_to::<u16>();
        10_000 - from_end
    }

    /// The share at a position in basis points of the bar from its start, to the nearest
    /// basis point, and at most the largest share with a positive minimum.
    fn share_at(&self, position_bps: u16) -> u16 {
        if self.estimate.is_zero() {
            return 0;
        }
        let from_end = U256::from(10_000_u16.saturating_sub(position_bps));
        // The allowance, `span * from_end / 10000`, as basis points of the estimate.
        let share =
            (self.span.saturating_mul(from_end) + self.estimate / U256::from(2)) / self.estimate;
        share.saturating_to::<u16>().min(self.max_share_bps())
    }

    /// The share whose minimum before the shield fee is `pre_fee`, clamped to the bar.
    fn share_for_pre_fee(&self, pre_fee: U256) -> u16 {
        if self.estimate.is_zero() {
            return 0;
        }
        let allowance = self.tolerated.saturating_sub(pre_fee);
        let share = (allowance.saturating_mul(U256::from(10_000_u32))
            + self.estimate / U256::from(2))
            / self.estimate;
        share.saturating_to::<u16>().min(self.max_share_bps())
    }
}

/// Converts the order's amounts, in the bought token, to the amounts the gas strip shows: for
/// a Bridge swap, its destination token, at the quote's ratio of destination minimum to
/// deposit. That ratio holds until the bridge quote a share change asks for.
#[derive(Clone, Copy)]
struct StripScale {
    shown: U256,
    ordered: U256,
}

impl StripScale {
    fn of(review: &SwapReview) -> Self {
        match review.bridge() {
            Some(bridge) if !review.suggested_private_minimum().is_zero() => Self {
                shown: bridge.destination_minimum,
                ordered: review.suggested_private_minimum(),
            },
            _ => Self {
                shown: U256::ONE,
                ordered: U256::ONE,
            },
        }
    }

    fn show(self, ordered: U256) -> U256 {
        ordered.saturating_mul(self.shown) / self.ordered
    }

    fn order(self, shown: U256) -> U256 {
        if self.shown.is_zero() {
            return U256::ZERO;
        }
        shown.saturating_mul(self.ordered) / self.shown
    }
}

/// Retry for a Bridge swap's routes, which asks `view`'s providers again.
fn bridge_routes_retry_button(view: Entity<PrivateSwapsView>, disabled: bool) -> Button {
    app_button("swap-bridge-routes-retry", "Retry")
        .debug_selector(|| "swap-bridge-routes-retry".into())
        .outline()
        .small()
        .flex_none()
        .disabled(disabled)
        .on_click(move |_, window, cx| {
            view.update(cx, |view, cx| view.retry_bridge_routes(window, cx));
        })
}

/// The unreachable provider notice as a hint card.
fn bridge_notice_card(provider: &'static str, window: &Window) -> gpui::Div {
    hint_card("Some tokens may be missing", theme::WARNING, window).child(
        div().whitespace_normal().child(format!(
            "{provider} is unreachable, so tokens only it delivers aren't listed."
        )),
    )
}

/// A provider's hint card: its `notes`, then its `disclaimer`. NEAR Intents' is a warning.
fn provider_hint_card(
    provider: BridgeProvider,
    notes: &[SharedString],
    disclaimer: &SharedString,
    window: &Window,
) -> gpui::Div {
    let warning = provider == BridgeProvider::NearIntents;
    let title_color = if warning { theme::WARNING } else { theme::INFO };
    hint_card(provider_name(provider), title_color, window)
        .children(
            notes
                .iter()
                .map(|note| div().whitespace_normal().child(note.clone())),
        )
        .child(
            div()
                .whitespace_normal()
                .when(warning, |disclaimer| {
                    disclaimer.text_color(rgb(theme::WARNING))
                })
                .child(disclaimer.clone()),
        )
}

/// The gas explanation as a hint card: `text`'s paragraphs under the title, with a legend of
/// the bar's graphics in the `(solid, muted)` colours while the bar shows.
fn gas_help_card(
    text: &[String; 5],
    legend: Option<(gpui::Hsla, gpui::Hsla)>,
    window: &Window,
) -> gpui::Div {
    let [costs, gas, solvers, minimum, fees] = text;
    let paragraph = |text: &String| div().whitespace_normal().child(text.clone());
    let entry = |marker: gpui::Div, label: &'static str| {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w_4()
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(marker),
            )
            .child(div().text_color(rgb(theme::TEXT_MUTED)).child(label))
    };
    hint_card(GAS_HELP_TITLE, theme::INFO, window)
        .w(rems(20.))
        .children([costs, gas, solvers, minimum].map(paragraph))
        .children(legend.map(|(solid, muted)| {
            div()
                .debug_selector(|| "swap-gas-help-legend".into())
                .flex()
                .flex_col()
                .gap_1()
                .child(entry(
                    div().size_3().rounded_full().bg(solid),
                    "Your minimum",
                ))
                .child(entry(
                    div().w_4().h_1().rounded_full().bg(solid),
                    "Where the swap can end up",
                ))
                .child(entry(
                    div()
                        .w_4()
                        .h_0()
                        .border_t_2()
                        .border_dashed()
                        .border_color(muted),
                    "Losses you've refused",
                ))
        }))
        .child(paragraph(fees))
}

/// The slider value's nearest whole percent.
fn slider_percent(value: f32) -> u16 {
    (0..=100_u16)
        .min_by(|left, right| {
            (f32::from(*left) - value)
                .abs()
                .total_cmp(&(f32::from(*right) - value).abs())
        })
        .unwrap_or(0)
}

/// A position in basis points of the gas bar at the bar's nearest step, in percent.
const fn nearest_step(position_bps: u16) -> u16 {
    let step = GAS_BAR_STEP * 100;
    (position_bps + step / 2) / step * GAS_BAR_STEP
}

/// An RPC gas price in gwei, "1.18": two decimals from 1 gwei and three below it, without
/// trailing zeros.
pub(super) fn format_gwei(wei: u128) -> String {
    let (unit, divisor, width) = if wei >= 1_000_000_000 {
        (10_000_000_u128, 100_u128, 2_usize)
    } else {
        (1_000_000, 1_000, 3)
    };
    let scaled = wei.saturating_add(unit / 2) / unit;
    if scaled == 0 {
        return if wei == 0 { "0" } else { "< 0.001" }.to_owned();
    }
    let text = format!("{}.{:0width$}", scaled / divisor, scaled % divisor);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The name of a gas share: its preset, or Custom with the share.
pub(super) fn gas_share_name(share_bps: u16) -> String {
    GasPreset::of_share(share_bps).map_or_else(
        || format!("Custom · {}", format_bps_percent(u64::from(share_bps))),
        |preset| preset.summary().to_owned(),
    )
}

/// Expected value retained in buy-token units after the allowed gas, shield and bridge fees.
/// Across includes both the destination payment and source-chain return.
fn expected_output(review: &SwapReview) -> U256 {
    let quoted = expected_payout(review);
    quoted
        .saturating_sub(shield_fee_on(review, quoted))
        .saturating_sub(bridge_fee(review))
}

/// The `CoW` payout if solvers charge the allowed gas, before delivery.
const fn expected_payout(review: &SwapReview) -> U256 {
    review.best_case().saturating_sub(review.gas_allowance())
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

/// A wrapping danger Alert at `selector` with its Retry inside. Alert accepts text only, so
/// one frame contains its message and the button, as the high-cost warning's does.
fn retry_alert(selector: &'static str, message: String, retry: Button, cx: &App) -> gpui::Div {
    let danger = cx.theme().danger;
    div()
        .debug_selector(move || selector.into())
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(danger.mix_oklab(gpui::transparent_white(), 0.3))
        .bg(danger.mix_oklab(gpui::transparent_white(), 0.04))
        .child(
            div().min_w_0().child(
                Alert::error(selector, message)
                    .small()
                    .p_0()
                    .border_0()
                    .bg(gpui::transparent_black()),
            ),
        )
        .child(div().flex().child(retry))
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
        SwapReviewChange::GasShare => "this approval predates gas shares and needs a full review",
        SwapReviewChange::GasAllowance { .. } => "the gas you allow increased",
        SwapReviewChange::Validity { .. } => "the order validity changed",
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
