//! The swap form: tokens, amount, the gas share and price tolerance, the setup's broadcaster
//! route, the quote with its price check, the single review, and placing the approved order.
//!
//! A new swap is quoted before anything is paid, with a stand-in executor that has no code, as
//! a fresh stealth account has none. One review approves the setup and the private minimum.
//! The wallet then sends the setup and, once it's confirmed, quotes again for the delegated
//! stealth account. If the shield fee, the price check and the minimum still hold, a
//! confirm-only step places the order; otherwise the review opens again. Retries and changed
//! terms use the form without a setup.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use gpui::{
    Anchor, App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, FontWeight,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, px, relative, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Colorize as _, Disableable as _, Icon, IconName, Selectable as _,
    Sizable as _, WindowExt as _,
    alert::Alert,
    button::{Button, ButtonGroup, ButtonVariants as _},
    checkbox::Checkbox,
    collapsible::Collapsible,
    input::{Enter as InputEnter, InputEvent, InputState},
    popover::Popover,
    select::{
        SearchableVec, Select, SelectDelegate, SelectEvent, SelectGroup, SelectItem, SelectState,
    },
    slider::{Slider, SliderEvent, SliderState},
    spinner::Spinner,
    tooltip::Tooltip,
};
use ui::clipboard::{clipboard_with_toast, copy_to_clipboard_with_toast};
use ui::controls::{
    amount_max_button, app_amount_input, app_amount_shimmer_text, app_amount_text, app_button,
    app_button_base, app_button_label, app_input, app_muted_shimmer_text, app_muted_text,
    app_segment_button, app_shimmer_text, app_strong_text, app_text,
};
use ui::hint::hint_card;
use ui::recipient_picker::RecipientPickerEvent;
use ui::theme;
use wallet_ops::{
    BridgeLegPrice, DesktopPrivateSpendAuthorization, ExecutorOwner, ExecutorRecoveryFeeEstimate,
    OperationHttpClient, OperationNetworkIsolation, PublicBroadcasterCandidate,
    PublicBroadcasterResultKind, PublicBroadcasterSelection, PublicBroadcasterWaku,
    QuoteDeviationError, SwapAccountCandidate, SwapAccountReplacement, SwapAccountReplacements,
    SwapAmountPlan, SwapAmountRequest, SwapBridgeClients, SwapBridgeDestination, SwapBridgeQuote,
    SwapBridgeRoute, SwapDestinationContext, SwapExecutor, SwapOrderOutcome, SwapOrderRequest,
    SwapPairPreparation, SwapPairSide, SwapPrice, SwapReview, SwapReviewChange, SwapReviewRequest,
    SwapSetupRequest, TokenAnchorRateCache, TransactionGenerationStage, WalletSession,
    bridge::{
        BridgeApi, BridgeApiError, BridgeDestination, NonEvmBridgeDestination, NonEvmNetwork,
        NonEvmReceiverError, PublicBridgePath, across_destination_tokens, near_destination_tokens,
        near_non_evm_destination_tokens,
    },
    cow::{
        CowOrderbookClient, GAS_SHARE_BALANCED_BPS, GAS_SHARE_LOOSE_BPS, GAS_SHARE_TIGHT_BPS,
        OrderLimitError, OrderUid,
    },
    default_public_broadcaster_fee_limit, delivery_shortfall_allowance, prepare_swap_pair,
    settings::{
        BridgeDestinationProfile, BridgeProfile, BridgeReceiverRejection, EffectiveChainConfig,
        EffectiveTokenRegistry, SwapReceiverRejection, SwapTokenEligibility, SwapTokenRole,
        resolve_effective_chain_rpc_route, swap_destination_tokens,
    },
    swap_speed_up_relaxes,
    vault::{
        BridgeDelivery, BridgePrivateDelivery, BridgeProvider, BridgeShieldFailure, BridgeSurplus,
        ExecutorOperationId, ExecutorRecord, NonEvmDelivery, PublicAccountMetadata,
        PublicAccountSource, SwapAccountChoice, SwapAccountRole, SwapAccountUse,
        SwapAdmissionEvidence, SwapApproval, SwapApprovedAccount, SwapApprovedAccounts,
        SwapApprovedBounds, SwapDelivery, SwapSetupFeeLimit, SwapUseId, SwapUseRecord, SwapUseSide,
        swap_account_refusal,
    },
};

mod assets_networks;
mod bridge_routes;
mod delivery;
mod form_actions;
mod inputs;
mod labels;
mod opening;
mod order_settings;
mod orders;
pub(super) mod public_source;
mod quotes;
mod render_accounts;
mod render_details;
mod render_form;
mod render_gas;
mod render_review;
mod setup_retry;
mod setup_routes;
mod setup_submit;
mod sources;
mod summary;

/// Keeps the tracing target of the moved debug UI-fixture events at this module's path.
#[cfg(debug_assertions)]
const LOG_TARGET: &str = module_path!();

use self::buy_picker::{
    BuyNetwork, BuyNetworkKey, BuyPicker, NetworkAvailability, NetworkSync, NetworkUnavailable,
    PrivateNetworkFacts, SetupFunds, ensure_buy_picker_bindings, private_network_availability,
    public_network_availability,
};
use self::public_source::{PublicSwapForm, public_strip};
use self::quote_failure::{QuoteFailure, map_quote_failure};
use super::dialog::{SwapDialogView, powered_by_cow, provider_mark_path, provider_mention};
use super::model::{
    SwapFormMode as FormMode, SwapSetupProgress, SwapStage, SwapStepAccount, format_bps_percent,
    private_delivery_credit, provider_name, quote_anchor_delta_bps, swap_cost_bps, swap_form_mode,
    swap_total_cost,
};
use super::setup_progress::SetupProgress;
use super::{
    PrivateSwapsView, SWAP_BROADCASTER_REPUBLISH_INTERVAL, SWAP_BROADCASTER_RESPONSE_TIMEOUT,
    SwapAction, SwapJobKind, SwapTracking, non_evm_amount, non_evm_token_icon, swap_delivery,
    swap_private_delivery, swap_sell_amount, swap_tokens,
};
use crate::assets::{RailgunActionIcon, WalletIconSource};
use crate::root::broadcaster_picker::{
    BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL, BroadcasterChoice,
    BroadcasterPickerFeeEstimateContext, BroadcasterPickerTarget, broadcaster_candidate_label,
    selected_broadcaster_label,
};
use crate::root::chain_load::ChainUtxoState;
use crate::root::private_action::{
    PrivateActionAssetSelectItem, RecipientOption, RecipientPickerState, RecipientRules,
    UnshieldAsset, fee_token_selector, form_recipient_picker, non_evm_recipient_options,
    private_action_asset_select_items, recipient_suggestion_to_confirm,
    recipient_suggestions_for_input,
};
use crate::root::public_account::public_account_display_label;
use crate::root::public_balances::{public_asset_icon_path, public_balance_amount_label};
use crate::root::public_broadcaster::{
    PublicBroadcasterFeeTokenOption, highest_value_public_broadcaster_fee_token,
    public_broadcaster_fee_token_options_from_snapshot,
    resolve_selected_public_broadcaster_fee_token,
};
use crate::root::spend_authorization::{
    SpendAuthorizationCard, SpendAuthorizationHint, SpendAuthorizationLabel,
    SpendAuthorizationSummary, SpendAuthorizationSummaryRow, spend_authorization_recipient_display,
};
use crate::root::stealth_accounts::{RecoveryPickerContext, same_offer};
use crate::root::ui_helpers::{badged_token_icon, now_unix};
use crate::root::{
    COST_ESTIMATE_DEBOUNCE, DeliveryFormKind, WalletRoot, format_token_amount_ceiling_for_display,
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
/// How a review names a stealth account the swap hasn't reserved yet.
const NEW_ACCOUNT: &str = "New account";
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
// A temporary notice for a known Private POI list operator issue. Remove these and the notice's
// checkbox once the issue is fixed.
const BLOCKED_SHIELD_HEADLINE: &str = "Cross-chain Shields are often blocked right now.";
const BLOCKED_SHIELD_DETAILS: &str = "A Private POI list is currently blocking many Shields that arrive from another network. The issue is temporary and is being fixed. A blocked Shield can't be spent privately. You can refund it to the stealth account that made it, which is public and needs gas for the refund.";
/// What a private payer can do instead, so the notice doesn't read as a risk to every swap.
const BLOCKED_SHIELD_PRIVATE_ALTERNATIVES: &str =
    "Same-network swaps and delivery to a Public address aren't affected.";
/// What a Public payer can do instead: its form only shields on another network.
const BLOCKED_SHIELD_PUBLIC_ALTERNATIVES: &str = "Shielding on this network isn't affected.";
const BLOCKED_SHIELD_LABEL: &str = "I accept that this Shield may be blocked";
const BLOCKED_SHIELD_PROBLEM: &str = "Accept the blocked Shield risk before you review the swap.";
const BLOCKED_SHIELD_REVIEW_WARNING: &str = "Cross-chain Shields are often blocked right now. A blocked Shield can't be spent privately. You can refund it to the stealth account that made it, which is public and needs gas for the refund.";
/// The steps of a new swap, as its reviews' stepper names them.
/// The Earlier approval row's hint on a speed-up's review.
const EARLIER_APPROVAL_HINT: &str = "An earlier order of this swap approved this minimum. Whichever order trades, another order's approved bridge deposit can run in the same settlement. If it is this one, this is what the swap delivers.";

const SWAP_STEPS: [&str; 2] = ["Set up stealth account", "Place order"];
/// The steps of a private Bridge swap, which sets up a stealth account on each network.
const PRIVATE_BRIDGE_SWAP_STEPS: [&str; 2] = ["Set up stealth accounts", "Place order"];
const SAME_TOKEN_BRIDGE: &str =
    "Same-token bridging isn't supported. Choose another token to receive, or sell something else.";
const DESTINATION_AUTHORIZATION_MISSING: &str =
    "The approval didn't cover the destination network. Review the swap again. Nothing was sent.";
/// Under a speed-up's Minimum controls, and why its Review… is off, while the new order
/// wouldn't ask for less than the open one.
const SPEED_UP_NOT_LOWER: &str =
    "This doesn't lower the open order's minimum, so it wouldn't speed the swap up.";
/// Review…'s tooltip while the form has no quote; the line under the Buy amount says why.
const NO_QUOTE_YET: &str = "Get a quote first. The line under the Buy amount says what it needs.";
/// A speed-up review's note on the order it leaves in place.
const SPEED_UP_STAYS_LIVE: &str =
    "The open order stays live until the new order invalidates it, or until it fills or expires.";

/// Why Pay from lists a Public account that is shared between Private wallets as unavailable,
/// and why the Public tab can't bridge from it.
pub(in crate::root) const SHARED_ACCOUNT_REASON: &str =
    "Accounts shared between Private wallets can't pay for a swap";

/// One source of Pay from: Private balance, or a Public account with its address.
#[derive(Clone)]
struct SwapSourceSelectItem {
    account: Option<String>,
    label: SharedString,
    /// A Public account's address, shown shortened after its label and as its avatar.
    address: Option<Address>,
    balance: SharedString,
    /// The source can't be chosen.
    unavailable: bool,
    /// Why, in place of the row's balance, when the row has a reason of its own.
    reason: Option<SharedString>,
    /// The row is the heading of the Public accounts under it, not a source.
    heading: bool,
}

impl SwapSourceSelectItem {
    /// The row that heads the Public accounts. The kit's list sizes every section header from
    /// the first section's, which Private balance leaves empty, and gives every row one height,
    /// so the heading is a row that can't be chosen.
    fn heading(label: impl Into<SharedString>) -> Self {
        Self {
            // No source has this value, so the heading is never the selected row.
            account: Some(String::new()),
            label: label.into(),
            address: None,
            balance: SharedString::default(),
            unavailable: true,
            reason: None,
            heading: true,
        }
    }

    /// The avatar, the label and the shortened address, as the row and the closed select
    /// show them.
    fn identity(&self) -> gpui::Div {
        div()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(source_avatar(self.address))
            .child(app_text(self.label.clone()).flex_none())
            .children(self.address.map(|address| {
                app_muted_text(railgun_ui::short_address(&address))
                    .text_xs()
                    .min_w_0()
                    .truncate()
                    .font_family(theme::APP_MONO_FONT_FAMILY)
            }))
    }
}

impl SelectItem for SwapSourceSelectItem {
    type Value = Option<String>;

    fn title(&self) -> SharedString {
        self.address.map_or_else(
            || self.label.clone(),
            |address| format!("{} {}", self.label, railgun_ui::short_address(&address)).into(),
        )
    }

    fn value(&self) -> &Self::Value {
        &self.account
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(self.identity().into_any_element())
    }

    fn disabled(&self) -> bool {
        self.unavailable
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let selector = self.account.as_ref().map_or_else(
            || "swap-source-private".to_owned(),
            |uuid| format!("swap-source-public-{uuid}"),
        );
        if self.heading {
            return div()
                .w_full()
                .min_w_0()
                .child(app_muted_text(self.label.clone()).text_xs().truncate());
        }
        // Every row has one height, so a row's reason takes the place of its balance.
        let trailing = self.reason.clone().map_or_else(
            || balance_text(self.balance.clone()).flex_none(),
            |reason| app_muted_text(reason).text_xs().min_w_0().truncate(),
        );
        div()
            .debug_selector(move || selector)
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(self.identity().flex_1())
            .child(trailing)
    }
}

struct SwapSourceSelect {
    items: SearchableVec<SelectGroup<SwapSourceSelectItem>>,
}

impl SwapSourceSelect {
    fn new(items: Vec<SelectGroup<SwapSourceSelectItem>>) -> Self {
        Self {
            items: SearchableVec::new(items),
        }
    }
}

impl SelectDelegate for SwapSourceSelect {
    type Item = SwapSourceSelectItem;

    fn sections_count(&self, cx: &App) -> usize {
        self.items.sections_count(cx)
    }

    fn items_count(&self, section: usize) -> usize {
        self.items.items_count(section)
    }

    fn item(&self, index: gpui_component::IndexPath) -> Option<&Self::Item> {
        self.items.item(index)
    }

    fn position<V>(&self, value: &V) -> Option<gpui_component::IndexPath>
    where
        Self::Item: SelectItem<Value = V>,
        V: PartialEq,
    {
        self.items.position(value)
    }

    fn perform_search(&mut self, query: &str, window: &mut Window, cx: &mut App) -> Task<()> {
        self.items.perform_search(query, window, cx)
    }

    /// The one section has no header: the Public accounts' heading is a row.
    fn render_section_header(
        &self,
        _: usize,
        _: &mut Window,
        _: &mut App,
    ) -> Option<gpui::AnyElement> {
        Some(div().into_any_element())
    }
}

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
    /// The stealth accounts are already reserved; set up again whichever still needs it.
    resume: bool,
    sell: Address,
    buy: Address,
    /// The setup of the swap's own stealth account. `None` for an existing account, which
    /// takes no setup and pays no setup fee.
    origin: Option<OriginSetup>,
    /// The wallet's broadcaster network client, which delivers to broadcasters of every network.
    waku: PublicBroadcasterWaku,
    /// The destination action this attempt authorizes. `None` also covers a fresh account
    /// whose setup is already pending or confirmed and isn't sent again.
    destination: Option<DestinationPlan>,
    /// The use that reserves the chosen accounts or resumes their saved preparation. `None`
    /// while a new setup uses its source operation's first use.
    swap_use: Option<SwapUseId>,
    /// Persisted with the setup: the approved amount, minimum, fee and price check. It binds
    /// the fee token and limit of each setup the swap needs.
    approval: SwapApproval,
    orderbook: Option<CowOrderbookClient>,
}

/// The setup of the swap's own stealth account, through a broadcaster on the swap's network.
#[derive(Clone)]
struct OriginSetup {
    candidate: PublicBroadcasterCandidate,
    maximum_private_fee: U256,
}

impl SetupApproval {
    /// The destination network and the stealth account there that this approval also signs
    /// for: one it sets up, or an existing one.
    pub(super) fn destination_account(&self) -> Option<(u64, ExecutorOperationId)> {
        self.destination.as_ref().map(DestinationPlan::identity)
    }

    /// What the review's Pay now row shows: [`setup_fees`] of this approval's setups.
    fn fees(&self, chain_id: u64) -> Vec<SetupFee> {
        setup_fees(
            chain_id,
            self.origin.as_ref(),
            self.destination.as_ref().and_then(DestinationPlan::setup),
        )
    }
}

/// The fees of the setups a review approves, one for each account the swap sets up: the
/// `origin` setup's on `chain_id`, the swap's own network, then the `destination` setup's on
/// its network. An existing account has no setup, so no fee.
fn setup_fees(
    chain_id: u64,
    origin: Option<&OriginSetup>,
    destination: Option<&DestinationSetup>,
) -> Vec<SetupFee> {
    let fee = |chain_id, candidate: &PublicBroadcasterCandidate, maximum| SetupFee {
        chain_id,
        token: candidate.token,
        maximum,
        broadcaster: broadcaster_candidate_label(candidate),
    };
    origin
        .map(|origin| fee(chain_id, &origin.candidate, origin.maximum_private_fee))
        .into_iter()
        .chain(destination.map(|destination| {
            fee(
                destination.chain_id,
                &destination.candidate,
                destination.maximum_private_fee,
            )
        }))
        .collect()
}

/// What a setup review covers: the setup of each account the swap sets up, an existing
/// destination account that takes none, and the approval saved with them, which binds the
/// accounts and the fee limit of each setup.
struct SetupParts {
    origin: Option<OriginSetup>,
    destination: Option<DestinationPlan>,
    approval: SwapApproval,
}

/// The destination action of one setup attempt. A private delivery can have no action when
/// its fresh destination account's setup is already pending or confirmed.
#[derive(Clone)]
enum DestinationPlan {
    Setup(Box<DestinationSetup>),
    Existing(DestinationAccount),
}

impl DestinationPlan {
    fn identity(&self) -> (u64, ExecutorOperationId) {
        match self {
            Self::Setup(setup) => (setup.chain_id, setup.operation),
            Self::Existing(account) => (account.chain_id, account.operation),
        }
    }

    fn setup(&self) -> Option<&DestinationSetup> {
        match self {
            Self::Setup(setup) => Some(setup),
            Self::Existing(_) => None,
        }
    }
}

/// An existing stealth account chosen as a private Bridge swap's destination. The form's
/// destination choice without one is a new account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DestinationAccount {
    /// The network the account was chosen on. The choice doesn't outlive that network.
    pub(super) chain_id: u64,
    pub(super) operation: ExecutorOperationId,
    index: u32,
    address: Address,
}

/// The setup of a private Bridge swap's destination stealth account, through a broadcaster on
/// its own network and paid from the private balance there.
#[derive(Clone)]
pub(super) struct DestinationSetup {
    pub(super) chain_id: u64,
    /// A new operation, or the account's saved one when its setup is sent again.
    pub(super) operation: ExecutorOperationId,
    candidate: PublicBroadcasterCandidate,
    maximum_private_fee: U256,
}

/// One stealth account's setup the user approved to send again by itself: the swap's own
/// account's, or a private Bridge swap's destination account's. The swap's approved terms stay.
#[derive(Clone)]
pub(super) struct SetupRetry {
    /// The swap, on its own network.
    pub(super) operation: ExecutorOperationId,
    pub(super) side: SetupSide,
    /// The use whose approval was reviewed, retained through asynchronous preparation.
    swap_use: SwapUseId,
    /// The network of the account that is set up again.
    pub(super) chain_id: u64,
    /// That account: the swap's own, or its destination account on `chain_id`.
    pub(super) account: ExecutorOperationId,
    candidate: PublicBroadcasterCandidate,
    maximum_private_fee: U256,
    waku: PublicBroadcasterWaku,
    /// The setup fee limit approved with the swap for this account, in its fee token.
    approved_fee: Option<SwapSetupFeeLimit>,
    /// The swap's approval with this review's fee token and limit, when the limit is above
    /// the approved one, the token is another one, or none was approved. It is saved before
    /// the setup is sent.
    approval: Option<SwapApproval>,
    /// The account is retired, so the retry's job first replaces it with a newly reserved one
    /// and sets that one up. A hardware wallet's replacement is reserved before its review,
    /// whose retry then names the new account and replaces nothing.
    replaces: bool,
}

/// How a reviewed setup fee limit differs from the one approved with the swap for the same
/// account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupFeeLimitChange {
    /// The approved token, and not above the approved limit. The approved limit stays.
    Within,
    /// The approved token, above the approved limit.
    Higher,
    /// Another fee token, whose amounts don't compare with the approved one's.
    AnotherToken,
}

/// How `reviewed` differs from the `approved` limit. `None` without an approved limit, which
/// has nothing to compare with: the reviewed limit is saved and no change is shown.
fn setup_fee_limit_change(
    approved: Option<SwapSetupFeeLimit>,
    reviewed: SwapSetupFeeLimit,
) -> Option<SetupFeeLimitChange> {
    let approved = approved?;
    Some(if approved.token != reviewed.token {
        SetupFeeLimitChange::AnotherToken
    } else if reviewed.amount > approved.amount {
        SetupFeeLimitChange::Higher
    } else {
        SetupFeeLimitChange::Within
    })
}

/// The open retry of one stealth account's setup: the dialog over the swap's detail, and the
/// setup route its broadcaster controls drive. It is dropped when the dialog closes or the
/// swap dialog shows something else.
pub(super) struct SetupRetryForm {
    /// The swap, on its own network.
    pub(super) operation: ExecutorOperationId,
    pub(super) side: SetupSide,
    /// The network of the account whose setup is sent again.
    chain_id: u64,
    /// That account: the swap's own, or its destination account on `chain_id`.
    account: ExecutorOperationId,
    /// The account's number and address, as the dialog names it.
    shown: Option<SwapStepAccount>,
    route: SetupRoute,
    /// The retry dialog's own focus, which the broadcaster picker returns to.
    focus: Option<gpui::WeakFocusHandle>,
    /// Why the last Continue opened no review.
    error: Option<String>,
}

/// One setup fee of a review: at most `maximum` of `token`, paid to `broadcaster` on `chain_id`.
struct SetupFee {
    chain_id: u64,
    token: Address,
    maximum: U256,
    broadcaster: String,
}

/// The share of the swap authorized for costs, followed by its cost breakdown.
struct AuthorizedCostWarning {
    headline: String,
    details: String,
}

impl AuthorizedCostWarning {
    fn message(&self) -> String {
        format!("{} {}", self.headline, self.details)
    }
}

/// One stealth account of a private Bridge swap, as its reviews name it.
struct ReviewAccount {
    /// "Source" for the swap's own account, "Destination" for the one it delivers to.
    role: &'static str,
    chain_id: u64,
    /// The account's address, with its number once its record is read. `None` for a new
    /// account, which has no address before the swap reserves it.
    account: Option<(Option<u32>, Address)>,
    /// The swap reuses the account, which links it to the account's earlier activity.
    reused: bool,
}

impl ReviewAccount {
    /// "#35 on Ethereum", or the short address of an account whose number isn't known.
    fn name(&self) -> Option<String> {
        let (index, address) = self.account?;
        Some(format!(
            "{} on {}",
            index.map_or_else(
                || railgun_ui::short_address(&address),
                |index| format!("#{index}")
            ),
            network_name(self.chain_id)
        ))
    }

    /// The review's row for the account: its role and network, then "New account" or its
    /// number and short address with a control that copies the address.
    fn row(&self) -> SpendAuthorizationSummaryRow {
        let label =
            SpendAuthorizationLabel::on_network(format!("{} ·", self.role), self.chain_id, "");
        match self.account {
            Some((index, address)) => {
                SpendAuthorizationSummaryRow::naming_network(label, address.to_checksum(None))
                    .with_copyable_account(
                        index.map_or_else(String::new, |index| format!("#{index} · ")),
                        "stealth account address",
                    )
            }
            None => SpendAuthorizationSummaryRow::naming_network(label, NEW_ACCOUNT),
        }
    }

    /// The review's warning for a reused account, naming it and its network.
    fn reuse_warning(&self) -> Option<String> {
        let name = self.name().filter(|_| self.reused)?;
        Some(format!(
            "Reusing {name} links this swap to that account's earlier public activity there. A new stealth account offers more privacy."
        ))
    }
}

/// Whose setup a route pays for: the swap's own stealth account, or a private Bridge swap's
/// destination stealth account on its network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SetupSide {
    Origin,
    Destination,
}

/// The open order a speed-up form replaces: its ID, and the bounds it was signed with, which
/// its record never changes.
#[derive(Clone)]
struct SpeedUpOrder {
    uid: OrderUid,
    bounds: SwapApprovedBounds,
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
    /// The lower destination minimum an earlier order of the swap can still deliver, which the
    /// user accepted with this review. `None` when the review disclosed none.
    continuing_minimum: Option<U256>,
    /// Approved in a full review rather than the confirm-only step. Only a full review replaces
    /// the approval saved with the setup, which binds the swap's first order.
    full_review: bool,
    /// The swap use a reused account is claimed for, made once with its draft so that
    /// submitting again resumes the same claim. `None` for a swap's own order or retry, which
    /// belongs to the use that claims its account.
    pub(super) swap_use: Option<SwapUseId>,
    /// The existing destination stealth account chosen for a private Bridge swap on a reused
    /// source account. The order claims both accounts for `swap_use` before it signs. `None`
    /// for any other order, and once the swap's record names its destination account.
    pub(super) pair_destination: Option<DestinationAccount>,
    /// The open order this approval speeds up, as its form named it. `None` for any other
    /// order. Signing takes the order from the review's plan, which must name the same one.
    speed_up: Option<OrderUid>,
}

impl OrderApproval {
    /// The order's delivery, when it shields on another network.
    pub(super) fn private_delivery(&self) -> Option<BridgeDelivery> {
        self.review.plan().delivery().private_bridge()
    }

    /// The approval signs a second order beside an open one, by its form or by its plan.
    pub(super) fn speeds_up(&self) -> bool {
        self.speed_up.is_some() || self.review.plan().speed_up().is_some()
    }
}

/// The route a Bridge review was quoted with, kept for signing its order.
#[derive(Clone)]
struct QuotedBridge {
    clients: SwapBridgeClients,
    destination: QuotedDestination,
}

/// Where a quoted Bridge route delivers: a provider's destination on a chain with that chain's
/// configuration, or a NEAR Intents destination on a non-EVM network, which has none.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum QuotedDestination {
    Chain {
        destination: BridgeDestination,
        chain: EffectiveChainConfig,
    },
    NonEvm(NonEvmBridgeDestination),
}

impl QuotedBridge {
    const fn route(&self) -> SwapBridgeRoute<'_> {
        SwapBridgeRoute {
            clients: &self.clients,
            destination: match &self.destination {
                QuotedDestination::Chain { destination, chain } => {
                    SwapBridgeDestination::Chain { destination, chain }
                }
                QuotedDestination::NonEvm(destination) => {
                    SwapBridgeDestination::NonEvm(destination)
                }
            },
        }
    }
}

/// What a speed-up's review says about an earlier order of the same swap whose approved bridge
/// deposit can still run: the amounts are of the token received on the destination network.
struct ContinuingDelivery {
    /// The earlier order's approved minimum.
    lower: String,
    /// The reviewed order's minimum.
    new: String,
    /// The destination network's name.
    network: String,
    surplus: BridgeSurplus,
}

impl ContinuingDelivery {
    /// The earlier minimum with its network, such as "90 USDC on Arbitrum".
    fn lower_there(&self) -> String {
        format!("{} on {}", self.lower, self.network)
    }

    fn headline(&self) -> String {
        format!(
            "An earlier order of this swap can still deliver {}.",
            self.lower_there()
        )
    }

    /// What follows the headline. `source` names the swap's own network, where the rest of
    /// what the order buys is handled as the delivery's surplus choice says.
    fn details(&self, source: &str) -> String {
        let rest = match self.surplus {
            BridgeSurplus::Reshield => "is returned privately",
            BridgeSurplus::KeepInAccount => "stays in the stealth account",
            BridgeSurplus::BridgedByProvider => "is returned separately",
        };
        format!(
            "Whichever order trades, that earlier approved deposit may run in its place, and you receive {} there instead of at least {}. Anything left on {source} {rest} and doesn't add to it.",
            self.lower, self.new
        )
    }

    /// The review's warning. Two orders can trade, so it also says that the order that trades
    /// needn't be the one whose deposit runs.
    fn warning(&self) -> String {
        format!(
            "This swap may deliver {} instead of {}. Whichever order trades, another order's approved deposit can run in its settlement and decide what is delivered.",
            self.lower_there(),
            self.new
        )
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
    /// The user picked the fee token. Until then it follows the default, which changes as
    /// broadcaster offers and prices arrive.
    fee_token_chosen: bool,
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

    /// Estimate again for an offer that changed, with the last estimate left on screen until
    /// the new one answers.
    fn renew_estimate(&mut self) {
        self.estimate_candidate = None;
        self.estimate_task = None;
        self.next_estimate = Some(Instant::now());
        self.estimate_revision = self.estimate_revision.wrapping_add(1);
    }

    /// The estimate, while its broadcaster still makes the offer it was estimated for. A
    /// review approves only such an estimate.
    fn offered_estimate(&self) -> Option<&ExecutorRecoveryFeeEstimate> {
        self.estimate.as_ref().filter(|estimate| {
            self.candidates
                .iter()
                .any(|current| same_offer(current, estimate.broadcaster()))
        })
    }

    /// An estimate is under way, or the one on screen is for an offer its broadcaster no longer
    /// makes, so a review can't approve it yet.
    fn estimate_pending(&self) -> bool {
        self.estimate_error.is_none()
            && (self.estimate_task.is_some()
                || (self.estimate.is_some() && self.offered_estimate().is_none()))
    }

    /// The route holds a fee token, its options or an estimate, as one that is in use does.
    const fn is_used(&self) -> bool {
        self.fee_token.is_some()
            || !self.fee_options.is_empty()
            || self.estimate.is_some()
            || self.estimate_error.is_some()
            || self.estimate_task.is_some()
    }

    /// Forget the route of an account that takes no setup. The form's live-update task, which
    /// the swap's own route holds, stays. Whether anything was forgotten.
    fn release(&mut self) -> bool {
        if !self.is_used() {
            return false;
        }
        let refresh_task = self.refresh_task.take();
        *self = Self {
            refresh_task,
            ..Self::default()
        };
        true
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

/// A set-up stealth account as an account select lists it: its operation, index and address,
/// the sell and buy tokens of its last swap when the select names them, and what it was set
/// up for.
type AccountRow = (
    ExecutorOperationId,
    u32,
    Address,
    Option<(Address, Address)>,
    Option<String>,
);

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
    /// A non-EVM network's token: its mint or contract as 1Click lists it, which tells it
    /// apart, `None` for the native asset and on a chain. Its `asset.token` is unused.
    contract: Option<String>,
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

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(provider_mention(self.provider, app_text).into_any_element())
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
            .child(provider_mention(self.provider, app_text).flex_none())
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
    /// The form's network, kept when Receive to changed, can't take Private balance; the line
    /// under Receive to says why. While it is `syncing`, the wallet's sync or setup-fee check
    /// there may still resolve it.
    Network {
        problem: SharedString,
        syncing: bool,
    },
    /// The receiver as entered can't be used; the line under the Receiver field says why.
    Receiver(SharedString),
    /// No provider delivers the Buy token on the chosen network yet; the Buy panel and the
    /// Provider row say why.
    Bridge,
    /// The existing stealth account chosen as a private Bridge swap's destination can't take
    /// the swap; the line under its select says why. The choice stays.
    Account(SharedString),
}

/// What a Bridge swap's providers deliver on one network for one sell token.
struct BridgeRoutes {
    across: Vec<BridgeDestination>,
    near: Vec<BridgeDestination>,
    /// Tokens that are the sell token's own asset. The Buy list offers them, so the form can
    /// say why that pair isn't bridged.
    same_asset: Vec<BridgeDestination>,
    /// The network's configured wrapped native token, which Across delivers there as the
    /// native asset. `None` on a network without that setting.
    across_native: Option<Address>,
    /// The provider that couldn't be asked, and why. Its list is empty, and Retry asks again.
    unavailable: Option<(BridgeProvider, eyre::Report)>,
    /// Whether a provider serves this network from the swap's: Across has a route to it, for
    /// any token, or the wallet ships its 1Click name. A provider that couldn't be asked
    /// counts as serving it.
    served: bool,
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
    /// What a private delivery does when its shield on the destination network can't run.
    shield_failure: BridgeShieldFailure,
    /// Keyed by sell token and destination network. A missing entry is loading.
    routes: HashMap<(Address, u64), eyre::Result<BridgeRoutes>>,
    /// The routes being fetched, by the same key: the form's network's, and those of a network
    /// the Buy picker shows.
    routes_tasks: HashMap<(Address, u64), Task<()>>,
    /// What NEAR Intents delivers on each non-EVM network, from 1Click's token list, keyed by
    /// sell token and network. A missing entry is loading.
    non_evm_routes: HashMap<(Address, NonEvmNetwork), eyre::Result<Vec<NonEvmBridgeDestination>>>,
    non_evm_tasks: HashMap<(Address, NonEvmNetwork), Task<()>>,
    /// The unreachable provider notice's popover. Reloaded routes close it.
    notice_open: bool,
    /// The Provider row's hint popover. Reloaded routes close it.
    provider_hint_open: bool,
    /// The "If the shield fails" row's hint popover.
    failure_hint_open: bool,
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
    /// NEAR Intents delivers the Buy token on a non-EVM network.
    NonEvm(&'a NonEvmBridgeDestination),
}

/// The destination terms of a Bridge quote, apart from the receiver.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BridgeTerms {
    network: u64,
    token: Address,
    provider: BridgeProvider,
    surplus: BridgeSurplus,
    /// A private delivery's failure choice.
    private: Option<BridgePrivateDelivery>,
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
    public: Option<PublicSwapForm>,
    pay_from_select: Entity<SelectState<SwapSourceSelect>>,
    /// The swap's executor operation once a setup was approved, or the swap being retried.
    operation: Option<ExecutorOperationId>,
    reuse_account: bool,
    /// The swap use this draft claims its accounts for while either is an existing one, made
    /// when an account is chosen. A swap of two new accounts takes its source's first use.
    reuse_use: Option<SwapUseId>,
    /// A new swap's choice of stealth account. A started swap keeps its own and has none.
    account_select: Option<Entity<SelectState<SearchableVec<SwapAccountSelectItem>>>>,
    /// A private Bridge swap's destination: an existing stealth account on the form's network,
    /// or `None` for a new one. Never chosen for the user, and dropped with the form.
    destination_account: Option<DestinationAccount>,
    /// A new swap's choice of destination stealth account, shown for Private balance on
    /// another network.
    destination_select: Option<Entity<SelectState<SearchableVec<SwapAccountSelectItem>>>>,
    /// The set-up accounts each loaded network offers as a destination, read from local
    /// records when the form's delivery or the Buy picker needs them.
    destination_candidates: HashMap<u64, Vec<SwapAccountCandidate>>,
    sell: Address,
    /// The selected Buy asset: an ERC-20 on the swap's network, see [`SwapForm::order_buy`],
    /// or on another network, the token delivered there, `Address::ZERO` for its native asset,
    /// which only a Public address receives.
    buy: Option<Address>,
    /// A Public address receives the wrapped native Buy asset as the native asset. Reset to
    /// wrapped when the Buy asset changes or delivery goes back to Private.
    native_output: bool,
    sell_select: Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>,
    /// The Buy token button's picker, which sets the delivery kind, the network and the Buy
    /// asset together.
    picker: BuyPicker,
    /// Delivery on another network makes the swap a Bridge swap. `None` is the swap's own
    /// network, or the non-EVM network of `non_evm`. Receive to keeps it, so it may be one the
    /// delivery kind can't take.
    network: Option<u64>,
    /// A built-in non-EVM destination, which NEAR Intents alone delivers to. `network`
    /// is `None` while it is set, and `buy` too: the Buy asset is `non_evm_token`.
    non_evm: Option<NonEvmNetwork>,
    /// The non-EVM Buy asset by its 1Click mint or contract, `Some(None)` for the native asset.
    /// `None` until one is picked.
    #[allow(clippy::option_option)]
    non_evm_token: Option<Option<String>>,
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
    receiver_picker: RecipientPickerState,
    /// The delivery the choices and the receiver give, or why there is none. Checked again
    /// when any of them, or the stealth account, changes. See [`Self::quote_delivery`] for the
    /// delivery a quote is requested for.
    delivery: Result<SwapDelivery, DeliveryProblem>,
    route: SetupRoute,
    /// The setup route of a private Bridge swap's destination stealth account, on its network.
    /// Unused for any other delivery, and once that account is set up.
    destination_route: SetupRoute,
    /// The route the broadcaster picker was opened for.
    broadcaster_side: SetupSide,
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
    /// Acceptance of the lower delivery an earlier order of the swap can still make. Applies
    /// only to the current form quote, like `high_costs_acknowledged`.
    continuing_acknowledged: bool,
    /// Acceptance that a private Bridge delivery's Shield may be blocked. Applies to the
    /// delivery kind, so a new quote keeps it; leaving a private Bridge delivery clears it.
    blocked_shield_acknowledged: bool,
    error: Option<String>,
    /// The order detail this form continues or retries, which Back returns to.
    back_to_detail: Option<ExecutorOperationId>,
    /// The form speeds this open order up: its order is planned from that order's record, its
    /// terms are fixed, and only the Minimum, through the gas share, can change.
    speed_up: Option<SpeedUpOrder>,
    details_open: bool,
    /// The setup broadcaster popover. Presses elsewhere in the form close it, so a fee token
    /// list opened inside it stays usable.
    settings_open: bool,
    /// The gas strip's "Why pay less than the full gas?" popover. A new quote closes it.
    gas_help_open: bool,
    /// The Details of a failed quote's alert. Clearing the quote or asking for a new one
    /// closes them.
    quote_details_open: bool,
    _subscriptions: Vec<Subscription>,
}

impl SwapForm {
    pub(super) const fn operation(&self) -> Option<ExecutorOperationId> {
        self.operation
    }

    /// The swap delivers on another network than its own: a chain, or a non-EVM network.
    const fn bridged(&self) -> bool {
        self.network.is_some() || self.non_evm.is_some()
    }

    /// The form's destination network as the Buy picker keys it, `own` for the swap's own.
    fn network_key(&self, own: u64) -> BuyNetworkKey {
        self.non_evm.map_or_else(
            || BuyNetworkKey::Chain(self.network.unwrap_or(own)),
            BuyNetworkKey::NonEvm,
        )
    }

    /// A Buy asset is chosen, on a chain or a non-EVM network.
    const fn has_buy(&self) -> bool {
        self.buy.is_some() || self.non_evm_token.is_some()
    }

    /// The Buy list of a non-EVM `network`: 1Click's tokens there, the native asset first, then
    /// by symbol. Empty until they load. A native asset's icon is its network's.
    fn non_evm_buy_items(&self, network: NonEvmNetwork) -> Vec<SwapBuyItem> {
        let Some(Ok(destinations)) = self.bridge.non_evm_routes.get(&(self.sell, network)) else {
            return Vec::new();
        };
        let mut items = destinations
            .iter()
            .map(|destination| SwapBuyItem {
                asset: PrivateActionAssetSelectItem {
                    token: Address::ZERO,
                    label: Arc::from(destination.symbol.as_str()),
                    icon_path: non_evm_token_icon(
                        network,
                        destination.contract.as_deref(),
                        &destination.symbol,
                    ),
                },
                near_only: false,
                contract: destination.contract.clone(),
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            (left.contract.is_some(), left.asset.label.to_lowercase())
                .cmp(&(right.contract.is_some(), right.asset.label.to_lowercase()))
        });
        items
    }

    /// The form delivers to the private balance on another network, which makes the swap a
    /// private Bridge swap with a stealth account on each network.
    const fn private_bridge(&self) -> bool {
        matches!(self.receive_to, ReceiveTo::PrivateBalance) && self.network.is_some()
    }

    /// The form's delivery makes a cross-chain Shield whose blocked-Shield risk isn't accepted
    /// yet. A speed-up's open order already carries that delivery.
    const fn blocked_shield_unaccepted(&self) -> bool {
        self.private_bridge() && self.speed_up.is_none() && !self.blocked_shield_acknowledged
    }

    /// Receive to or the network changed: once the delivery is no longer a private Bridge
    /// delivery, coming back to one asks for the blocked-Shield acceptance again.
    const fn blocked_shield_delivery_changed(&mut self) {
        if !self.private_bridge() {
            self.blocked_shield_acknowledged = false;
        }
    }

    /// The existing account chosen as the private Bridge delivery's destination. A choice made
    /// for another network, or for another delivery, doesn't count.
    fn destination_choice(&self) -> Option<DestinationAccount> {
        self.destination_account
            .filter(|account| self.private_bridge() && self.network == Some(account.chain_id))
    }

    /// The form still chooses its accounts: a new swap, or one that reuses a set-up account.
    const fn chooses_accounts(&self) -> bool {
        self.operation.is_none() || self.reuse_account
    }

    /// Pay from can change: no stealth account is chosen or reserved, and a Public payer's
    /// swap hasn't started.
    fn can_change_source(&self) -> bool {
        self.operation.is_none()
            && self
                .public
                .as_ref()
                .is_none_or(|public| public.operation.is_none())
    }

    /// An account choice changed: the draft claims its accounts under a new swap use while
    /// either side reuses one, and under none while both are new.
    fn renew_reuse_use(&mut self) {
        self.reuse_use = if self.reuse_account || self.destination_choice().is_some() {
            SwapUseId::random().ok()
        } else {
            None
        };
    }

    /// The destination network changed: an account chosen on the old one doesn't follow, and
    /// the destination is a new account again. The swap's own account stays.
    pub(super) fn clear_destination(&mut self) {
        if self.destination_account.take().is_some() {
            self.destination_route.invalidate_estimate();
            self.renew_reuse_use();
        }
    }

    /// A private delivery's terms as the form has them.
    const fn private_delivery(&self) -> BridgePrivateDelivery {
        BridgePrivateDelivery {
            on_shield_failure: self.bridge.shield_failure,
        }
    }

    const fn setup_route(&self, side: SetupSide) -> &SetupRoute {
        match side {
            SetupSide::Origin => &self.route,
            SetupSide::Destination => &self.destination_route,
        }
    }

    const fn setup_route_mut(&mut self, side: SetupSide) -> &mut SetupRoute {
        match side {
            SetupSide::Origin => &mut self.route,
            SetupSide::Destination => &mut self.destination_route,
        }
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

    /// The bridge leg is being quoted again at the pending share, so the amounts it sets shimmer.
    fn bridge_refreshing(&self, review: &Arc<SwapReview>) -> bool {
        self.gas_share_pending(review) && self.bridge_quote_error.is_none()
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
    /// then `Address::ZERO`, a placeholder that signing refuses. A private Bridge delivery
    /// carries the placeholder until its destination stealth account is reserved.
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
    /// a Bridge swap has no provider for the Buy token, or its network can't take the delivery
    /// kind.
    fn quote_delivery(&self) -> Option<SwapDelivery> {
        match &self.delivery {
            Ok(delivery) => Some(delivery.clone()),
            Err(
                DeliveryProblem::Bridge
                | DeliveryProblem::Network { .. }
                | DeliveryProblem::Account(_),
            ) => None,
            Err(DeliveryProblem::Receiver(_)) => {
                // A non-EVM quote sends 1Click a placeholder receiver of its own.
                if let (Some(network), BridgeState::NonEvm(destination)) =
                    (self.non_evm, self.bridge_state())
                {
                    return Some(SwapDelivery::NonEvmBridge(non_evm_delivery(
                        network,
                        String::new(),
                        destination,
                    )));
                }
                // Until 1Click lists the Buy token there, there is nothing to quote.
                if self.non_evm.is_some() {
                    return None;
                }
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
                    private: None,
                }))
            }
        }
    }

    /// Whether the order `review` would sign asks for less than the open order a speed-up
    /// replaces. `None` for a form that speeds nothing up.
    fn speed_up_relaxes(&self, review: &SwapReview) -> Option<bool> {
        let open = self.speed_up.as_ref()?;
        Some(
            review
                .buy_amount_for(review.suggested_private_minimum())
                .is_ok_and(|buy_amount| swap_speed_up_relaxes(&open.bounds, buy_amount)),
        )
    }

    /// Why the form's delivery can't be quoted or reviewed.
    fn delivery_problem(&self) -> Option<SharedString> {
        match &self.delivery {
            Err(
                DeliveryProblem::Network { problem, .. }
                | DeliveryProblem::Receiver(problem)
                | DeliveryProblem::Account(problem),
            ) => Some(problem.clone()),
            Err(DeliveryProblem::Bridge) => Some("Choose a token the bridge delivers.".into()),
            Ok(_) => None,
        }
    }

    fn review_problem(&self, review: &Arc<SwapReview>) -> Option<SharedString> {
        match &self.delivery {
            Err(_) => self.delivery_problem(),
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
            Ok(_) if self.speed_up_relaxes(review) == Some(false) => {
                Some(SPEED_UP_NOT_LOWER.into())
            }
            Ok(_) if !review.price_verified() && !self.price_acknowledged => {
                Some("Accept the unverified price before you review the swap.".into())
            }
            Ok(_) if authorized_high_cost(review).is_some() && !self.high_costs_acknowledged => {
                Some("Confirm Swap anyway to accept the high swap costs.".into())
            }
            Ok(_) if review.continuing_minimum().is_some() && !self.continuing_acknowledged => {
                Some("Accept the earlier order's delivery before you review the swap.".into())
            }
            Ok(_) if self.blocked_shield_unaccepted() => Some(BLOCKED_SHIELD_PROBLEM.into()),
            Ok(_) => None,
        }
    }

    /// The token the order buys on the swap's network: for a Bridge swap, the one handed to
    /// its provider, once the provider is known.
    fn quote_buy(&self) -> Option<Address> {
        match self.bridge_state() {
            BridgeState::SameChain => self.order_buy(),
            BridgeState::Ready { destination, .. } => Some(destination.intermediate),
            BridgeState::NonEvm(destination) => Some(destination.intermediate),
            _ => None,
        }
    }

    fn bridge_state(&self) -> BridgeState<'_> {
        // A non-EVM network's token is listed by 1Click, which alone delivers there.
        if let Some(network) = self.non_evm {
            let destinations = match self.bridge.non_evm_routes.get(&(self.sell, network)) {
                None => return BridgeState::Loading,
                Some(Err(error)) => return BridgeState::Failed(error),
                Some(Ok(destinations)) => destinations,
            };
            let Some(token) = &self.non_evm_token else {
                return BridgeState::NoToken;
            };
            return destinations
                .iter()
                .find(|destination| &destination.contract == token)
                .map_or(BridgeState::Unavailable, BridgeState::NonEvm);
        }
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
        // Only Across can shield on delivery, and it shields the wrapped native token as
        // itself.
        let private = self.receive_to == ReceiveTo::PrivateBalance;
        let (across, near) = if private {
            (destination_for(&routes.across, token), None)
        } else {
            (
                routes.across_for(token),
                destination_for(&routes.near, token),
            )
        };
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
                    Some((provider, _)) if !private || *provider == BridgeProvider::Across => {
                        BridgeState::Unreachable(*provider)
                    }
                    _ => BridgeState::Unavailable,
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
            private: self.private_bridge().then(|| self.private_delivery()),
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
    /// The account and the notes come from the record of the open order being sped up.
    SpeedUp {
        operation: ExecutorOperationId,
        order: OrderUid,
    },
}

struct QuoteRequest {
    /// Opens the swap's orderbook route when the request carries none.
    http: wallet_ops::HttpContext,
    executor: QuoteExecutor,
    /// This session's last read at the latest block that showed the swap's own setup included,
    /// for an order or a speed-up on that setup while its record leaves it unresolved.
    inclusion: Option<wallet_ops::SwapSetupInclusion>,
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
    destination: QuoteBridgeDestination,
    origin: BridgeProfile,
}

/// Where a Bridge delivery to quote goes. Each holds the Buy list's entry, or `None` to find
/// the approved delivery's again.
#[allow(clippy::large_enum_variant)]
enum QuoteBridgeDestination {
    Chain {
        destination: Option<BridgeDestination>,
        chain: EffectiveChainConfig,
    },
    NonEvm(Option<NonEvmBridgeDestination>),
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
    /// The broadcaster's answer for the swap's own setup, and for a private Bridge swap's
    /// destination setup, each when it was sent. Each stands by itself. An existing account
    /// has no setup, so no answer.
    Sent {
        origin: Option<eyre::Result<PublicBroadcasterResultKind>>,
        destination: Option<eyre::Result<PublicBroadcasterResultKind>>,
    },
    /// The approved amount no longer fits one order; nothing was paid.
    TooLarge,
}

/// A setup job's work with its error as a value, so the job's completion can end the setup's
/// progress with it. `start_job` reports an error of the work itself and skips the completion.
async fn setup_job_result<T>(
    work: impl Future<Output = eyre::Result<T>>,
) -> eyre::Result<eyre::Result<T>> {
    Ok(work.await)
}

/// A private Bridge swap's destination stealth account with what reserves it and sends its
/// setup: its network's session and owner, and the authorization for the account there.
struct DestinationSubmission {
    plan: DestinationPlan,
    session: Arc<WalletSession>,
    owner: Arc<ExecutorOwner>,
    authorization: DesktopPrivateSpendAuthorization,
}

/// One stealth account a swap's setup job replaced after finding it used: the swap's own
/// account's operation before, and the swap after.
#[derive(Clone, Copy)]
pub(super) struct ReplacedSwapAccount {
    pub(super) from: ExecutorOperationId,
    pub(super) swap: SwapAccountReplacement,
}

/// The view's following of the replacements a setup job reports while it runs.
struct FollowedReplacements {
    followed: Rc<Cell<usize>>,
    _following: Task<()>,
}

impl FollowedReplacements {
    /// Stop following, and tell how many of the job's replacements the view followed.
    fn finish(self) -> usize {
        self.followed.get()
    }
}

/// One account of a reserved pair, prepared again: a new account on `side` for its setup's
/// retry, replaced when it is found used, and an existing one as it is.
async fn resume_pair_side(
    accounts: &mut SwapAccountReplacements<'_>,
    side: SwapUseSide,
    owner: &ExecutorOwner,
    operation: ExecutorOperationId,
    candidate: Option<&PublicBroadcasterCandidate>,
    authorization: &DesktopPrivateSpendAuthorization,
    client: &OperationHttpClient,
) -> eyre::Result<SwapPairSide> {
    match candidate {
        Some(candidate) => Box::pin(accounts.resume_setup(side, candidate, authorization, client))
            .await
            .map(SwapPairSide::Setup),
        None => existing_pair_side(owner, operation),
    }
}

/// An existing stealth account as one side of a reserved swap pair.
fn existing_pair_side(
    owner: &ExecutorOwner,
    operation: ExecutorOperationId,
) -> eyre::Result<SwapPairSide> {
    let executor = owner
        .records()?
        .into_iter()
        .find(|record| record.operation() == operation)
        .and_then(|record| record.address())
        .ok_or_else(|| eyre::eyre!("The stealth account is unavailable."))?;
    Ok(SwapPairSide::Existing {
        operation,
        executor,
    })
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
    // A speed-up has no executor to preview: its plan names the open order's.
    let executor = match request.executor {
        QuoteExecutor::Preview => Ok(owner.swap_preview_executor()?),
        QuoteExecutor::Setup(operation) => Ok(owner.swap_setup_preview(operation)?),
        QuoteExecutor::Order { operation, reuse } => {
            Ok(owner.swap_order_preview(operation, reuse, request.inclusion)?)
        }
        QuoteExecutor::SpeedUp { operation, order } => Err((operation, order)),
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
        let client = owner.swap_orderbook_client(&request.http.operation_http_client().await?)?;
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
        let origin = request_bridge.origin;
        let destination = match &request_bridge.destination {
            QuoteBridgeDestination::Chain { destination, chain } => QuotedDestination::Chain {
                destination: match destination {
                    Some(destination) => destination.clone(),
                    None => approved_bridge_destination(&clients, origin, chain, request).await?,
                },
                chain: chain.clone(),
            },
            QuoteBridgeDestination::NonEvm(destination) => {
                QuotedDestination::NonEvm(match destination {
                    Some(destination) => destination.clone(),
                    None => approved_non_evm_destination(&clients, &origin, request).await?,
                })
            }
        };
        *bridge = Some(QuotedBridge {
            clients,
            destination,
        });
    }
    let amount_request = SwapAmountRequest {
        sell_token: request.sell,
        buy_token: request.buy,
        amount: request.amount,
        delivery: request.delivery.clone(),
        byte_budget: request.byte_budget,
    };
    let planning_owner = Arc::clone(owner);
    let planning_session = Arc::clone(session);
    let inclusion = request.inclusion;
    // Note selection can take a moment on fragmented balances.
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "input_planning", "started");
    let plan = tokio::task::spawn_blocking(move || match executor {
        Ok(executor) => {
            planning_owner.plan_swap_amount(&executor, &planning_session, &amount_request)
        }
        // The open order's own notes, tokens, spend and delivery, whatever the request names.
        Err((operation, order)) => {
            planning_owner.plan_swap_speed_up(&planning_session, operation, order, inclusion)
        }
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
/// error. NEAR Intents isn't offered on a chain without a 1Click name and isn't asked: its
/// list is empty, and an Across failure is then the error.
async fn fetch_bridge_routes(
    clients: &SwapBridgeClients,
    origin: BridgeProfile,
    destination: BridgeDestinationProfile,
    sell: Address,
    tokens: &EffectiveTokenRegistry,
    across_native: Option<Address>,
) -> eyre::Result<BridgeRoutes> {
    let chain = destination.chain_id();
    let near_offered = destination.one_click_blockchain().is_some();
    let (routes, listed, unavailable) = match tokio::join!(
        clients.across.available_routes(origin.chain_id(), chain),
        async {
            if near_offered {
                clients.near.tokens().await
            } else {
                Ok(Vec::new())
            }
        },
    ) {
        (Ok(routes), Ok(listed)) => (routes, listed, None),
        (Err(error), Ok(_)) if !near_offered => return Err(error.into()),
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
    // Across failing here means NEAR Intents is offered, so the network isn't unserved.
    let served = near_offered || !routes.is_empty();
    let across = across_destination_tokens(&routes, sell, tokens, origin.chain_id(), chain);
    let near = near_destination_tokens(&listed, &origin, &destination, sell, tokens);
    // Without a sell token to leave out, the lists also hold the sell token's own asset.
    let mut same_asset: Vec<BridgeDestination> = Vec::new();
    for candidate in
        across_destination_tokens(&routes, Address::ZERO, tokens, origin.chain_id(), chain)
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
        served,
    })
}

/// The approved Bridge delivery's destination in its provider's list today. It must still
/// bridge the approved bought token.
async fn approved_bridge_destination(
    clients: &SwapBridgeClients,
    origin: BridgeProfile,
    chain: &EffectiveChainConfig,
    request: &QuoteRequest,
) -> eyre::Result<BridgeDestination> {
    let SwapDelivery::Bridge(delivery) = request.delivery else {
        return Err(eyre::eyre!("Only a Bridge swap has a bridge route."));
    };
    let destination = chain
        .bridge_destination()
        .ok_or_else(|| eyre::eyre!("Bridging to this network isn't supported."))?;
    let routes = fetch_bridge_routes(
        clients,
        origin,
        destination,
        request.sell,
        &request.tokens,
        across_unwrapped_token(chain),
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

/// The approved non-EVM delivery's destination in 1Click's list today, bought through the
/// approved intermediate.
async fn approved_non_evm_destination(
    clients: &SwapBridgeClients,
    origin: &BridgeProfile,
    request: &QuoteRequest,
) -> eyre::Result<NonEvmBridgeDestination> {
    let SwapDelivery::NonEvmBridge(delivery) = &request.delivery else {
        return Err(eyre::eyre!("Only a Bridge swap has a bridge route."));
    };
    fetch_non_evm_routes(
        clients,
        origin,
        delivery.network,
        request.sell,
        &request.tokens,
    )
    .await?
    .into_iter()
    .find(|offered| {
        offered.near.destination_asset == delivery.asset_id
            && offered.contract == delivery.contract
            && offered.intermediate == request.buy
    })
    .ok_or_else(|| {
        eyre::eyre!(
            "NEAR Intents no longer offers the approved route. Open the swap to review it again."
        )
    })
}

/// What NEAR Intents delivers on a non-EVM `network` for `sell`, from 1Click's token list on
/// `clients`' route.
async fn fetch_non_evm_routes(
    clients: &SwapBridgeClients,
    origin: &BridgeProfile,
    network: NonEvmNetwork,
    sell: Address,
    tokens: &EffectiveTokenRegistry,
) -> eyre::Result<Vec<NonEvmBridgeDestination>> {
    let listed = clients.near.tokens().await?;
    Ok(near_non_evm_destination_tokens(
        &listed, origin, network, sell, tokens,
    ))
}

struct BridgeRoutesResult<R = BridgeRoutes> {
    orderbook: Option<CowOrderbookClient>,
    bridge_clients: Option<SwapBridgeClients>,
    routes: eyre::Result<R>,
}

/// The bridge clients on the swap's orderbook route, creating that route and the clients if
/// the form has none yet. What is created is left in `orderbook` and `bridge_clients`.
async fn swap_route_bridge_clients(
    owner: &ExecutorOwner,
    http: &wallet_ops::HttpContext,
    orderbook: &mut Option<CowOrderbookClient>,
    bridge_clients: &mut Option<SwapBridgeClients>,
) -> eyre::Result<SwapBridgeClients> {
    let client = if let Some(client) = orderbook.clone() {
        client
    } else {
        let client = owner.swap_orderbook_client(&http.operation_http_client().await?)?;
        *orderbook = Some(client.clone());
        client
    };
    Ok(if let Some(clients) = bridge_clients.clone() {
        clients
    } else {
        let clients = owner.swap_bridge_clients(&client)?;
        *bridge_clients = Some(clients.clone());
        clients
    })
}

/// Fetch the Buy list of a Bridge swap on the swap's orderbook route, creating that route if
/// the form has none yet.
async fn request_bridge_routes(
    owner: Arc<ExecutorOwner>,
    http: wallet_ops::HttpContext,
    orderbook: Option<CowOrderbookClient>,
    bridge_clients: Option<SwapBridgeClients>,
    origin: BridgeProfile,
    destination: BridgeDestinationProfile,
    sell: Address,
    tokens: EffectiveTokenRegistry,
    across_native: Option<Address>,
) -> BridgeRoutesResult {
    let mut bridge_clients = bridge_clients.filter(|_| orderbook.is_some());
    let mut orderbook = orderbook;
    let routes = async {
        let clients =
            swap_route_bridge_clients(&owner, &http, &mut orderbook, &mut bridge_clients).await?;
        fetch_bridge_routes(&clients, origin, destination, sell, &tokens, across_native).await
    }
    .await;
    BridgeRoutesResult {
        orderbook,
        bridge_clients,
        routes,
    }
}

/// Fetch the Buy list of a non-EVM `network` as [`request_bridge_routes`] does a chain's.
async fn request_non_evm_routes(
    owner: Arc<ExecutorOwner>,
    http: wallet_ops::HttpContext,
    orderbook: Option<CowOrderbookClient>,
    bridge_clients: Option<SwapBridgeClients>,
    origin: BridgeProfile,
    network: NonEvmNetwork,
    sell: Address,
    tokens: EffectiveTokenRegistry,
) -> BridgeRoutesResult<Vec<NonEvmBridgeDestination>> {
    let mut bridge_clients = bridge_clients.filter(|_| orderbook.is_some());
    let mut orderbook = orderbook;
    let routes = async {
        let clients =
            swap_route_bridge_clients(&owner, &http, &mut orderbook, &mut bridge_clients).await?;
        fetch_non_evm_routes(&clients, &origin, network, sell, &tokens).await
    }
    .await;
    BridgeRoutesResult {
        orderbook,
        bridge_clients,
        routes,
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
            contract: None,
        })
        .collect()
}

/// Where `token` sorts in an asset list: the native asset, then the chain's `wrapped` native
/// token, then every other token.
fn native_rank(token: Address, wrapped: Option<Address>) -> u8 {
    if token == Address::ZERO {
        0
    } else if Some(token) == wrapped {
        1
    } else {
        2
    }
}

fn destination_for(list: &[BridgeDestination], token: Address) -> Option<&BridgeDestination> {
    list.iter()
        .find(|destination| destination.destination_token == token)
}

/// The wrapped native token Across delivers on `chain` as the native asset: the `SpokePool`
/// unwraps the chain's configured wrapped native token for a receiver without code. A chain
/// without that setting has none, and its tokens are shown as themselves.
const fn across_unwrapped_token(chain: &EffectiveChainConfig) -> Option<Address> {
    chain.wrapped_native_token
}

/// The surplus choice a provider takes: Across's `chosen` one; NEAR Intents converts it all.
const fn bridge_surplus(provider: BridgeProvider, chosen: BridgeSurplus) -> BridgeSurplus {
    match provider {
        BridgeProvider::Across => chosen,
        BridgeProvider::NearIntents => BridgeSurplus::BridgedByProvider,
    }
}

/// A Public account as the swap form and its reviews name it: its label.
pub(super) fn public_source_label(account: &PublicAccountMetadata) -> String {
    public_account_display_label(account).unwrap_or_else(|| "Public account".into())
}

/// A Pay from source's avatar: a Public account's identicon, and for Private balance the
/// same space left empty, so the labels line up.
fn source_avatar(address: Option<Address>) -> gpui::Div {
    let avatar = div().size_4().flex_none();
    let Some(address) = address else {
        return avatar;
    };
    let color = ui::public_address::public_account_identicon_color(address.as_ref());
    let pattern = ui::public_address::public_account_identicon_pattern(address.as_ref());
    avatar.flex().flex_col().children(
        pattern
            .as_chunks::<{ ui::public_address::PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE }>()
            .0
            .iter()
            .map(|row| {
                div().flex_1().flex().children(row.iter().map(|active| {
                    // The identicon's colour is address-derived data, as on the Public tab.
                    let cell = div().flex_1();
                    if *active { cell.bg(rgb(color)) } else { cell }
                }))
            }),
    )
}

pub(super) use crate::root::ui_helpers::network_name;

/// Whether `receive_to` can deliver on `chain`, another chain the picker lists. A Public
/// address needs only its RPC endpoints. Private balance also needs an accepted swap
/// profile, the wallet's private sync ready in this session, and either private funds for the
/// destination stealth account's setup or, with `reusable`, a set-up account there to select.
/// While it isn't known whether the funds held there can pay for the setup, the setup fee
/// reads as being checked.
fn destination_availability(
    root: &WalletRoot,
    receive_to: ReceiveTo,
    chain: &EffectiveChainConfig,
    reusable: bool,
) -> NetworkAvailability {
    let rpc = resolve_effective_chain_rpc_route(chain.chain_id, chain).is_ok();
    match receive_to {
        ReceiveTo::PublicAddress => public_network_availability(rpc),
        ReceiveTo::PrivateBalance => private_network_availability(PrivateNetworkFacts {
            rpc,
            swap_profile: chain.swap_profile().is_some(),
            sync: network_sync(root.chain_states.get(&chain.chain_id)),
            // Debug UI fixture: a wallet without private funds there picks the network too.
            funds: if super::ui_fixture::active() {
                SetupFunds::Funded
            } else {
                destination_setup_funds(root, chain)
            },
            reusable,
        }),
    }
}

/// Where the wallet's private sync of a chain stands. A chain without a session yet reads as
/// loading: opening the Buy picker starts it. A synced chain taking in newer blocks stays
/// ready, so its quote isn't dropped for each catch-up.
const fn network_sync(state: Option<&ChainUtxoState>) -> NetworkSync {
    match state {
        Some(
            ChainUtxoState::Ready { .. }
            | ChainUtxoState::Syncing {
                catching_up: true, ..
            },
        ) => NetworkSync::Ready,
        Some(ChainUtxoState::Error { .. }) => NetworkSync::Failed,
        Some(state) => NetworkSync::Loading(state.sync_percent()),
        None => NetworkSync::Loading(None),
    }
}

/// How long after the fee subscription goes live a missing offer may still arrive. Offers
/// arrived up to about 30 s after it in testing; the margin covers slower broadcasters.
const FEE_OFFERS_SETTLE: Duration = Duration::from_mins(1);

/// Whether the wallet could pay a broadcaster for a stealth account's setup on `chain`: some
/// POI-verified, spendable private balance there is in a token that a broadcaster compatible
/// with the chain's executor profile accepts, under the fee policy and trust filter a new
/// setup route starts with. The picker then checks that balance against the setup estimate.
/// Without such a balance, notes held there leave it unknown while their POI status is being
/// refreshed, or while no broadcaster offer on the chain has arrived.
fn destination_setup_funds(root: &WalletRoot, chain: &EffectiveChainConfig) -> SetupFunds {
    let chain_id = chain.chain_id;
    let Some(profile) = chain.accepted_executor_profile() else {
        return SetupFunds::Unfunded;
    };
    let Some(state) = root.chain_states.get(&chain_id) else {
        return SetupFunds::Unfunded;
    };
    let Some(snapshot) = state.snapshot() else {
        return SetupFunds::Unfunded;
    };
    let rows = root.monitor_fee_rows();
    let funded = public_broadcaster_fee_token_options_from_snapshot(
        snapshot,
        &rows,
        Some(profile),
        root.public_broadcaster_fee_policy(false),
        &root.public_broadcaster_trust_filter(false),
        Some(&root.effective_token_registry),
        |token| {
            root.public_broadcaster_anchor_cache
                .cached_rate(chain_id, token)
        },
    )
    .iter()
    .any(|option| option.eligible_broadcaster_count > 0);
    // A POI refresh or a first offer on the chain can still make the held notes pay for it, and
    // so can an offer from a broadcaster that hasn't announced since the fee subscription went
    // live, as after a new Tor session.
    let settling = root
        .monitor_state
        .read()
        .fee_subscription_started()
        .is_some_and(|started| started.elapsed() < FEE_OFFERS_SETTLE);
    let pending =
        state.poi_refreshing() || settling || !rows.iter().any(|row| row.chain_id == chain_id);
    if funded {
        SetupFunds::Funded
    } else if snapshot.unspent_count > 0 && pending {
        SetupFunds::Unknown
    } else {
        SetupFunds::Unfunded
    }
}

/// The non-EVM delivery of a Bridge `review` whose bridge leg is quoted, whose strip amounts are
/// in the delivery's token and decimals.
fn non_evm_bridge_review(review: &SwapReview) -> Option<NonEvmDelivery> {
    match (review.plan().delivery(), review.bridge()) {
        (SwapDelivery::NonEvmBridge(delivery), Some(_)) => Some(delivery),
        _ => None,
    }
}

/// 1Click's USD value of `amount` of a non-EVM delivery's token, at the rate its `quote` values
/// the expected output, labelled as 1Click's: "≈ $12.50 · 1Click estimate". For display only.
/// `None` when the quote states no USD value.
fn one_click_usd_estimate(quote: &SwapBridgeQuote, amount: U256) -> Option<String> {
    let usd = amount
        .checked_mul(quote.expected_output_usd?)?
        .checked_div(quote.expected_output)?;
    Some(format!(
        "≈ {} · 1Click estimate",
        railgun_ui::format_usd_micro_value(usd)
    ))
}

/// What a NEAR Intents delivery to `network` makes public, for What becomes public.
fn near_intents_disclosure(network: &str) -> String {
    format!(
        "The order names a 1Click deposit address, and the settlement pays it in the same transaction that unshields from Railgun. 1Click learns the receiver when the order is signed, so the receiver's funds on {network} can be traced to this swap."
    )
}

/// The review's warning for an unverified price. A non-EVM bridge leg without an anchor says
/// that no independent price is known for the token it delivers.
fn unverified_price_warning(review: &SwapReview) -> String {
    match review.plan().delivery() {
        SwapDelivery::NonEvmBridge(delivery)
            if review
                .bridge()
                .is_some_and(|bridge| bridge.leg == BridgeLegPrice::Unverified) =>
        {
            format!(
                "{UNVERIFIED_PRICE_WARNING} No independent price is known for {} on {}.",
                delivery.symbol,
                delivery.network.display_name()
            )
        }
        _ => UNVERIFIED_PRICE_WARNING.to_owned(),
    }
}

/// A review's row naming a non-EVM token's mint or contract, which it copies. The native asset
/// has none.
fn non_evm_contract_row(delivery: &NonEvmDelivery) -> Option<SpendAuthorizationSummaryRow> {
    let contract = delivery.contract.as_deref()?;
    Some(
        SpendAuthorizationSummaryRow::new(contract_label(delivery.network), contract)
            .with_short_copyable(railgun_ui::short_non_evm(contract)),
    )
}

/// What a non-EVM network calls a token's address: a mint on Solana, a contract elsewhere.
const fn contract_label(network: NonEvmNetwork) -> &'static str {
    match network {
        NonEvmNetwork::Solana => "Mint",
        NonEvmNetwork::Bitcoin | NonEvmNetwork::Tron | NonEvmNetwork::Zcash => "Contract",
    }
}

/// Under the Buy panel, the chosen non-EVM token's mint or contract, shortened, with a control
/// that copies it in full. Look-alike symbols are told apart by it. The native asset has none.
fn render_non_evm_contract(form: &SwapForm) -> Option<gpui::Div> {
    let network = form.non_evm?;
    let contract = form.non_evm_token.clone()??;
    let copy_id = SharedString::from(format!("swap-buy-contract-{contract}-copy"));
    Some(
        div()
            .debug_selector(|| "swap-buy-contract".into())
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .child(
                app_muted_text(contract_label(network))
                    .text_xs()
                    .flex_none(),
            )
            .child(
                app_muted_text(railgun_ui::short_non_evm(&contract))
                    .text_xs()
                    .min_w_0()
                    .truncate()
                    .font_family(theme::APP_MONO_FONT_FAMILY),
            )
            .child(
                clipboard_with_toast(copy_id, contract)
                    .tooltip(format!("Copy {}", contract_label(network).to_lowercase())),
            ),
    )
}

/// The Receiver field's placeholder, which names the destination network's address format.
const fn receiver_placeholder(network: Option<NonEvmNetwork>) -> &'static str {
    match network {
        None => "0x address",
        Some(NonEvmNetwork::Bitcoin) => "Bitcoin address: bc1…, 1… or 3…",
        Some(NonEvmNetwork::Solana) => "Solana address",
        Some(NonEvmNetwork::Tron) => "Tron address: T…",
        Some(NonEvmNetwork::Zcash) => "Zcash address: u1…, t1… or tex1…",
    }
}

/// Why a receiver can't be used on a non-EVM `network`, naming its format.
fn non_evm_receiver_message(network: NonEvmNetwork, error: NonEvmReceiverError) -> String {
    let name = network.display_name();
    match error {
        NonEvmReceiverError::Testnet => {
            "Only mainnet addresses can receive the swap. Enter a mainnet address.".to_owned()
        }
        NonEvmReceiverError::OtherNetwork(_) => {
            format!("The receiver must be a {name} address.")
        }
        NonEvmReceiverError::Invalid => match network {
            NonEvmNetwork::Bitcoin => {
                "This isn't a valid Bitcoin address. Check it, including its checksum.".to_owned()
            }
            NonEvmNetwork::Solana => "This isn't a valid Solana address.".to_owned(),
            NonEvmNetwork::Tron => {
                "This isn't a valid Tron address. Check it, including its checksum.".to_owned()
            }
            NonEvmNetwork::Zcash => {
                "This isn't a valid Zcash address. Check it, including its checksum.".to_owned()
            }
        },
        NonEvmReceiverError::Sapling => {
            "The bridge can't pay a Sapling-only address. Enter a t1… address, or a unified \
             address (u1…) with an Orchard or transparent receiver."
                .to_owned()
        }
    }
}

/// The delivery of `destination`, a token NEAR Intents delivers on `network`, to `receiver`.
fn non_evm_delivery(
    network: NonEvmNetwork,
    receiver: String,
    destination: &NonEvmBridgeDestination,
) -> NonEvmDelivery {
    NonEvmDelivery {
        network,
        receiver,
        asset_id: destination.near.destination_asset.clone(),
        contract: destination.contract.clone(),
        symbol: destination.symbol.clone(),
        decimals: destination.near.destination_decimals,
    }
}

/// The token a Bridge review delivers on its network. Only a Bridge review has one.
fn review_destination_token(review: &SwapReview) -> Address {
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
        // 1Click also sends these for pairs it lists, during outages that clear in minutes.
        BridgeApiError::Rejected {
            api: api @ BridgeApi::NearIntents,
            message,
            ..
        } if message == ONE_CLICK_NO_QUOTE => Some(format!(
            "{api} can't quote this pair right now. This is usually temporary. Retry in a few minutes."
        )),
        BridgeApiError::Rejected {
            api: api @ BridgeApi::NearIntents,
            message,
            ..
        } if message == ONE_CLICK_NO_LIQUIDITY => Some(format!(
            "{api} has no liquidity for this pair right now. This is usually temporary. Retry in a few minutes."
        )),
        BridgeApiError::Rejected { api, message, .. } => Some(format!("{api}: {message}")),
        _ => None,
    }
}

/// NEAR Intents' incident status page.
const NEAR_INTENTS_STATUS_URL: &str = "https://partners.near-intents.org/shield/status";
/// 1Click's refusals during an outage, as [`bridge_rejection`] and [`near_intents_status`]
/// match them.
const ONE_CLICK_NO_QUOTE: &str = "Quoting for this pair is not available";
const ONE_CLICK_NO_LIQUIDITY: &str = "No liquidity available";

/// NEAR Intents' status page, for a failure that a NEAR Intents outage can cause.
fn near_intents_status(error: &eyre::Report) -> Option<&'static str> {
    match error.downcast_ref::<BridgeApiError>()? {
        BridgeApiError::Unavailable {
            api: BridgeApi::NearIntents,
            ..
        } => Some(NEAR_INTENTS_STATUS_URL),
        BridgeApiError::Rejected {
            api: BridgeApi::NearIntents,
            message,
            ..
        } if message == ONE_CLICK_NO_QUOTE || message == ONE_CLICK_NO_LIQUIDITY => {
            Some(NEAR_INTENTS_STATUS_URL)
        }
        _ => None,
    }
}

/// A provider that couldn't be reached on the swap's route. Retry asks on a fresh one.
fn bridge_unreachable(error: &eyre::Report) -> Option<String> {
    match error.downcast_ref::<BridgeApiError>()? {
        BridgeApiError::RateLimited { api } => Some(format!(
            "{api} is limiting requests right now. Retry in a minute."
        )),
        // A 5xx, such as 1Click's "Internal server error": the provider was reached.
        BridgeApiError::Unavailable { api, .. } => Some(format!(
            "{api} is having a temporary problem. Retry in a few minutes."
        )),
        BridgeApiError::Timeout { api, .. } | BridgeApiError::Transport { api, .. } => Some(
            format!("Couldn't reach {api} through the swap's network route."),
        ),
        _ => None,
    }
}

/// What the setup broadcaster controls show of one route, captured when their holder renders.
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

impl SetupRouteSettings {
    fn of(route: &SetupRoute, busy: bool) -> Self {
        Self {
            fee_options: route.fee_options.clone(),
            fee_token: route.fee_token.unwrap_or_default(),
            allow_out_of_range: route.allow_out_of_range,
            favorites_only: route.favorites_only,
            random_selected: route.selected.is_none(),
            specific_label: selected_broadcaster_label(&route.choice(), &route.candidates),
            candidate_count: route.candidates.len(),
            busy,
        }
    }
}

/// The shared broadcaster settings with the fee token selector, driving `side`'s setup route
/// of the open form or retry. Specific broadcaster opens the broadcaster picker on top of the
/// swap dialog.
fn setup_route_settings(
    view: Entity<PrivateSwapsView>,
    side: SetupSide,
    route: &SetupRouteSettings,
) -> gpui::Stateful<gpui::Div> {
    use ui::private_action::BroadcasterSettingsEvent as Event;
    let (token_id, settings_id) = match side {
        SetupSide::Origin => ("swap-setup-fee-token", "swap-setup-broadcaster-settings"),
        SetupSide::Destination => (
            "swap-destination-setup-fee-token",
            "swap-destination-setup-broadcaster-settings",
        ),
    };
    let fee_view = view.clone();
    let fee_token = fee_token_selector(
        token_id.into(),
        &route.fee_options,
        route.fee_token,
        route.busy,
        move |token, _, cx| {
            fee_view.update(cx, |view, cx| {
                if let Some(route) = view.active_setup_route_mut(side) {
                    route.fee_token = Some(token);
                    route.fee_token_chosen = true;
                    route.invalidate_estimate();
                }
                view.refresh_setup_route(cx);
            });
        },
    );
    ui::private_action::broadcaster_settings_fields(
        settings_id,
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
                    view.choose_specific_setup_broadcaster(side, window, cx);
                    return;
                }
                let Some(route) = view.active_setup_route_mut(side) else {
                    return;
                };
                match event {
                    Event::Random => route.selected = None,
                    Event::AllowOutOfRange(value) => route.allow_out_of_range = value,
                    Event::FavoritesOnly(value) => route.favorites_only = value,
                    Event::ChooseSpecific => {}
                }
                route.invalidate_estimate();
                view.refresh_setup_route(cx);
            });
        },
    )
}

/// What reusing the stealth account `index` at `address` on `chain_id` reveals, under its
/// select, with the address to copy. The text's tooltip holds the full privacy warning the
/// review repeats.
fn account_reuse_line(id: &'static str, index: u32, address: Address, chain_id: u64) -> gpui::Div {
    reuse_line(
        id,
        format!(
            "Reuses #{index} · {} on {} and links this swap to its earlier activity there. No setup fee.",
            railgun_ui::short_address(&address),
            network_name(chain_id)
        ),
        address,
    )
}

/// A reuse warning's `text` under an account select, with the account's `address` to copy.
fn reuse_line(id: &'static str, text: String, address: Address) -> gpui::Div {
    div()
        .debug_selector(move || id.into())
        .w_full()
        .flex()
        .items_start()
        .gap_1()
        .child(
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .flex()
                .items_start()
                .gap_1()
                .child(
                    Icon::new(IconName::TriangleAlert)
                        .xsmall()
                        .flex_none()
                        .text_color(rgb(theme::WARNING)),
                )
                .child(
                    app_text(text)
                        .text_xs()
                        .min_w_0()
                        .text_color(rgb(theme::WARNING))
                        .whitespace_normal(),
                )
                .tooltip(|window, cx| Tooltip::new(ACCOUNT_REUSE_NOTE).build(window, cx)),
        )
        .child(
            clipboard_with_toast(
                SharedString::from(format!("{id}-copy")),
                address.to_checksum(None),
            )
            .xsmall()
            .tooltip("Copy address"),
        )
}

/// A new stealth account, the first entry of an account select and its default.
fn new_account_item() -> SwapAccountSelectItem {
    SwapAccountSelectItem {
        operation: None,
        address: None,
        label: "New account (recommended)".into(),
    }
}

/// Under Receive to: why the form's network can't take Private balance. A sync or setup-fee
/// check under way resolves by itself and shows its progress in the price line instead.
fn receive_to_problem(form: &SwapForm) -> Option<gpui::Div> {
    let Err(DeliveryProblem::Network {
        problem,
        syncing: false,
    }) = &form.delivery
    else {
        return None;
    };
    // Under the control, past the label and the row's gap.
    Some(
        error_alert("swap-receive-to-problem", problem.clone()).pl(rems(ACCOUNT_LABEL_WIDTH + 0.5)),
    )
}

/// The line under the stealth account select while a setup is quoted. A `problem` the user has
/// to solve is a danger Alert, like the one under Receive to.
/// A `pending` line shimmers: a fee being estimated, or a setup the order waits for.
fn setup_fee_line(line: String, problem: bool, pending: bool) -> gpui::Div {
    if problem {
        return error_alert("swap-setup-problem", line);
    }
    if pending {
        return app_muted_shimmer_text(line).text_xs().whitespace_normal();
    }
    app_muted_text(line).text_xs().whitespace_normal()
}

/// The failure choice's hint card: paragraphs under the title, the two options' led by their
/// labels.
fn shield_failure_card(text: &[(Option<String>, String)], window: &Window) -> gpui::Div {
    hint_card("If the shield fails", theme::INFO, window).children(text.iter().map(
        |(label, paragraph)| {
            div()
                .flex()
                .flex_col()
                .children(
                    label
                        .clone()
                        .map(|label| div().font_weight(FontWeight::MEDIUM).child(label)),
                )
                .child(div().whitespace_normal().child(paragraph.clone()))
        },
    ))
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

/// A price below its anchor, in parentheses after the amounts it belongs to. Its tooltip
/// says what it was checked against.
fn price_delta_note(delta: PriceDelta, cx: &App) -> gpui::Stateful<gpui::Div> {
    div()
        .id("swap-price-delta")
        .flex()
        .items_baseline()
        .child(app_muted_text("("))
        .child(delta.value(cx))
        .child(app_muted_text(format!(" vs {})", delta.anchor)))
        .when_some(delta.checked, |delta, checked| {
            delta.tooltip(move |window, cx| Tooltip::new(checked.clone()).build(window, cx))
        })
}

fn price_delta(review: &SwapReview) -> Option<PriceDelta> {
    let quote = review.quote();
    checked_price_delta(review.price(), Some((quote.sell_amount, quote.buy_amount)))
}

/// A checked `price` against its anchor, for the `quoted` sell and buy amounts. Without them
/// the delta has no figure and reads as within range, which a verified price is.
fn checked_price_delta(price: &SwapPrice, quoted: Option<(U256, U256)>) -> Option<PriceDelta> {
    let SwapPrice::Verified { rate, observations } = price else {
        return None;
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
        bps: quoted.and_then(|(sold, bought)| {
            let expected = if rate.sell_rate.is_zero() {
                U256::ZERO
            } else {
                sold.saturating_mul(rate.buy_rate) / rate.sell_rate
            };
            quote_anchor_delta_bps(bought, expected)
        }),
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

/// What one whole sold token buys, in the bought token's base units, when `sell_amount` of a
/// token with `sell_decimals` is exchanged for `buy_amount`. `None` for no sold amount.
fn unit_rate(sell_amount: U256, sell_decimals: u8, buy_amount: U256) -> Option<U256> {
    if sell_amount.is_zero() {
        return None;
    }
    Some(buy_amount.saturating_mul(U256::from(10u8).pow(U256::from(sell_decimals))) / sell_amount)
}

/// The Bridge row's value for Across: the provider, its `fee`, and a private delivery's
/// `allowance` for the shield's gas, without its symbol.
fn across_row_value(provider: &str, fee: Option<&str>, allowance: Option<&str>) -> String {
    match (fee, allowance) {
        (Some(fee), Some(allowance)) => format!("{provider} · {fee} + ≈ {allowance} delivery"),
        (Some(fee), None) => format!("{provider} · fee {fee}"),
        (None, _) => provider.to_owned(),
    }
}

/// What a private delivery through Across costs, for the Bridge row's hint: its `fee` and the
/// `allowance` for the shield's gas on `network`.
fn across_delivery_note(fee: &str, network: u64, allowance: &str) -> String {
    format!(
        "Across charges {fee}. Shielding on delivery costs the relayer extra gas on {}, estimated at {allowance}.",
        network_name(network)
    )
}

/// A new swap's steps, as its reviews' stepper names them. A private Bridge swap sets up
/// `two_accounts`.
pub(super) const fn swap_steps(two_accounts: bool) -> [&'static str; 2] {
    if two_accounts {
        PRIVATE_BRIDGE_SWAP_STEPS
    } else {
        SWAP_STEPS
    }
}

/// The stepper's explanation of a new swap's two steps.
pub(super) fn swap_steps_hint(two_accounts: bool) -> SpendAuthorizationHint {
    let setup = if two_accounts {
        "1. Set up stealth accounts. A broadcaster on each network creates a one-time account for this swap, and you pay both setup fees."
    } else {
        "1. Set up stealth account. A broadcaster creates a one-time account for this swap, and you pay the setup fee."
    };
    SpendAuthorizationHint::new(
        "Two steps",
        [
            setup,
            "2. Place order. Once setup confirms, the wallet checks the quote again and asks you to confirm. If the terms changed, you review them first.",
            "With \"Just this spend\", you enter the password again for step 2.",
        ],
    )
}

/// A failure choice as its control and the review name it, after the network the funds end up
/// on: the swap's `origin`, or the `destination`.
fn shield_failure_label(choice: BridgeShieldFailure, origin: &str, destination: &str) -> String {
    match choice {
        BridgeShieldFailure::RefundOnOrigin => format!("Refund on {origin}"),
        BridgeShieldFailure::KeepOnDestination => format!("Keep on {destination}"),
    }
}

/// [`shield_failure_label`] as a review row's value, which carries the network's mark.
fn shield_failure_mention(
    choice: BridgeShieldFailure,
    origin: u64,
    destination: u64,
) -> SpendAuthorizationLabel {
    match choice {
        BridgeShieldFailure::RefundOnOrigin => {
            SpendAuthorizationLabel::on_network("Refund on", origin, "")
        }
        BridgeShieldFailure::KeepOnDestination => {
            SpendAuthorizationLabel::on_network("Keep on", destination, "")
        }
    }
}

/// The info icon beside the estimated return, whose hover says where the return goes.
fn source_return_help(review: &SwapReview) -> gpui::Stateful<gpui::Div> {
    let note = source_return_note(review);
    div()
        .id("swap-source-return-help")
        .flex_none()
        .tooltip(move |window, cx| {
            Tooltip::element(move |window, _| {
                hint_card("Estimated return", theme::INFO, window)
                    .w(rems(20.))
                    .child(div().whitespace_normal().child(note))
            })
            .build(window, cx)
        })
        .child(
            app_button_base("swap-source-return-help-trigger")
                .ghost()
                .xsmall()
                .icon(IconName::Info)
                .accessibility_label(note)
                .debug_selector(|| "swap-source-return-help".into()),
        )
}

fn source_return_note(review: &SwapReview) -> &'static str {
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

/// A review's collapsed Costs line: the `total` as the form's costs line has it, then
/// `pay_now`, the amounts paid up front and not refunded.
fn review_costs_summary(total: Option<String>, pay_now: Option<&str>) -> String {
    total
        .into_iter()
        .chain(pay_now.map(|amounts| format!("up to {amounts} now, not refunded")))
        .collect::<Vec<_>>()
        .join(" · ")
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
/// swap's minimum is its destination credit, valued as the deposit less all delivery costs.
fn authorized_high_cost(review: &SwapReview) -> Option<u64> {
    let minimum = review
        .suggested_private_minimum()
        .saturating_sub(bridge_cost(review));
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
        Self::new(
            review.best_case(),
            review.slippage_bps(),
            review.gas_estimate(),
        )
    }

    /// The bar of an order with this best case, price tolerance and gas estimate.
    fn new(best_case: U256, slippage_bps: u32, estimate: U256) -> Self {
        let tolerance = U256::from(10_000_u32.saturating_sub(slippage_bps));
        let tolerated = best_case.saturating_mul(tolerance) / U256::from(10_000_u32);
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
/// a Bridge swap, its destination token, at the quote's ratio of the minimum received there to
/// the deposit. A private delivery receives its destination minimum less the shield fee there.
/// That ratio holds until the bridge quote a share change asks for.
#[derive(Clone, Copy)]
struct StripScale {
    shown: U256,
    ordered: U256,
}

impl StripScale {
    fn of(review: &SwapReview) -> Self {
        match review.bridge() {
            Some(bridge) if !review.suggested_private_minimum().is_zero() => Self {
                shown: bridge.received_minimum(),
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

/// A provider's hint card: its `notes`, then its `disclaimer`. NEAR Intents' title is a warning.
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
        .child(div().whitespace_normal().child(disclaimer.clone()))
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
        .saturating_sub(bridge_cost(review))
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

/// Provider fees plus private destination gas and shielding, in buy-token base units.
fn bridge_cost(review: &SwapReview) -> U256 {
    bridge_fee(review).saturating_add(private_delivery_cost(review))
}

/// Value the private delivery's deductions at Across's same-asset quote ratio. The source
/// deposit less the provider fee corresponds to `quoted_output` before destination costs;
/// scaling the net credit avoids assuming equal token decimals on the two chains.
fn private_delivery_cost(review: &SwapReview) -> U256 {
    let Some((bridge, private)) = review
        .bridge()
        .and_then(|bridge| bridge.private.map(|private| (bridge, private)))
    else {
        return U256::ZERO;
    };
    let before_delivery = review
        .suggested_private_minimum()
        .saturating_sub(bridge_fee(review));
    let retained = before_delivery
        .saturating_mul(bridge.received_minimum())
        .checked_div(private.quoted_output)
        .unwrap_or_default();
    before_delivery.saturating_sub(retained)
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

/// A wrapping danger Alert at `selector`, for a message with no action.
fn error_alert(selector: &'static str, message: impl Into<SharedString>) -> gpui::Div {
    div()
        .debug_selector(move || selector.into())
        .w_full()
        .min_w_0()
        .child(Alert::error(selector, message.into()).small().min_w_0())
}

/// A wrapping danger Alert at `selector` with its `action` inside, such as Retry. Alert accepts
/// text only, so one frame contains its message and the button, as the high-cost warning's does.
fn action_alert(selector: &'static str, message: String, action: Button, cx: &App) -> gpui::Div {
    alert_frame(selector, message, None, Some(action), None, cx)
}

/// The Details section of an [`alert_frame`]: the technical `text` behind the alert's message,
/// shown while `open`. Its toggle sets the form's state through `view`.
struct AlertDetails {
    text: String,
    open: bool,
    view: Entity<PrivateSwapsView>,
}

/// A wrapping danger Alert at `selector`. Under its message it has the provider's `status`
/// page to copy, then the collapsed `details`, when the failure has any. Its `action` comes
/// last, so it stays in view with the details open.
fn alert_frame(
    selector: &'static str,
    message: String,
    details: Option<AlertDetails>,
    action: Option<Button>,
    status: Option<&'static str>,
    cx: &App,
) -> gpui::Div {
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
        .children(status.map(|url| {
            div()
                .debug_selector(move || format!("{selector}-status"))
                // Lines up with the message: the small Alert's icon is its text size, then 6px.
                .pl(rems(0.875))
                .ml(px(6.))
                .min_w_0()
                .flex()
                .flex_col()
                .child(app_muted_text("NEAR Intents incident status:").text_xs())
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            app_muted_text(url.trim_start_matches("https://"))
                                .text_xs()
                                .min_w_0()
                                .truncate(),
                        )
                        .child(
                            clipboard_with_toast(
                                SharedString::from(format!("{selector}-status-copy")),
                                url,
                            )
                            .tooltip("Copy status page address"),
                        ),
                )
        }))
        .children(details.map(|details| alert_details(selector, details)))
        .children(action.map(|action| div().flex().child(action)))
}

/// The "Details" toggle of the alert at `selector` and, while open, the text with its copy
/// button. The selectors carry the alert's, as a form can show two alerts.
fn alert_details(selector: &'static str, details: AlertDetails) -> Collapsible {
    let AlertDetails { text, open, view } = details;
    let toggle = if open { "Hide details" } else { "Show details" };
    let copied = SharedString::from(text.clone());
    Collapsible::new()
        .open(open)
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(
            app_button_base(SharedString::from(format!("{selector}-details-toggle")))
                .debug_selector(move || format!("{selector}-details-toggle"))
                .ghost()
                .small()
                .flex_none()
                .accessibility_label(toggle)
                .tooltip(toggle)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(app_button_label("Details"))
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
                .on_click(move |_, _, cx| {
                    view.update(cx, |view, cx| view.set_quote_details_open(!open, cx));
                }),
        )
        .content(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .items_start()
                .gap_1()
                .child(
                    div()
                        .debug_selector(move || format!("{selector}-details"))
                        .w_full()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        // One element a line keeps the text's line breaks.
                        .children(text.lines().map(|line| {
                            app_muted_text(line.to_owned())
                                .text_xs()
                                .w_full()
                                .min_w_0()
                                .whitespace_normal()
                        })),
                )
                .child(
                    app_button(
                        SharedString::from(format!("{selector}-details-copy")),
                        "Copy",
                    )
                    .debug_selector(move || format!("{selector}-details-copy"))
                    .ghost()
                    .xsmall()
                    .flex_none()
                    .icon(IconName::Copy)
                    .on_click(move |_, window, cx| {
                        copy_to_clipboard_with_toast(copied.clone(), window, cx);
                    }),
                ),
        )
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
        SwapReviewChange::DestinationShieldFee { .. } => {
            "the Railgun shield fee on the destination network changed"
        }
        SwapReviewChange::UnshieldFee { .. } => "the Railgun unshield fee changed",
        SwapReviewChange::QuoteDeviates => "the quote moved away from the anchor price",
        SwapReviewChange::PriceVerification => "the price check changed",
        SwapReviewChange::PriceUnavailable => "the independent price became unavailable",
        SwapReviewChange::Minimum { .. } => "the quote fell below the minimum you approved then",
        SwapReviewChange::DestinationMinimum { .. } => {
            "the bridge quote fell below the minimum you approved then"
        }
        SwapReviewChange::DeliveryAllowance { .. } => {
            "shielding on the destination network got more expensive, which left less than the minimum you approved then"
        }
        SwapReviewChange::Accounts => "the stealth accounts or their setup changed",
        SwapReviewChange::ApprovalPlan => "the token approval the swap needs changed",
        SwapReviewChange::ContinuingApproval { .. } => {
            "an earlier order of this swap can still deliver less on the destination network"
        }
        SwapReviewChange::SpeedUpBuyAmount { .. } => {
            "this doesn't lower the open order's minimum, so it wouldn't speed the swap up"
        }
    }
}

/// What a setup submission leaves for the user to know, from each setup's own problem. A
/// private Bridge swap's two setups are named by their `networks`: the swap's own, then the
/// destination's.
fn setup_problems(
    origin: Option<String>,
    destination: Option<String>,
    networks: Option<&(String, String)>,
) -> Option<String> {
    let Some((origin_network, destination_network)) = networks else {
        return origin;
    };
    let problems = [(origin_network, origin), (destination_network, destination)]
        .into_iter()
        .filter_map(|(network, problem)| Some(format!("{network}: {}", problem?)))
        .collect::<Vec<_>>();
    (!problems.is_empty()).then(|| problems.join(" "))
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

#[path = "buy_picker.rs"]
mod buy_picker;

#[path = "quote_failure.rs"]
mod quote_failure;

#[cfg(test)]
#[path = "ui_tests.rs"]
mod ui_tests;
