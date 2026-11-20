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

mod alerts_and_rows;
mod assets_networks;
mod bridge_routes;
mod delivery;
mod form_actions;
mod form_helpers;
mod gas_bar;
mod inputs;
mod labels;
mod opening;
mod order_settings;
mod orders;
mod price_and_costs;
pub(super) mod public_source;
mod quote_jobs;
mod quotes;
mod render_accounts;
mod render_details;
mod render_form;
mod render_gas;
mod render_review;
mod route_requests;
mod setup_retry;
mod setup_route_settings;
mod setup_routes;
mod setup_submit;
mod sources;
mod summary;
mod swap_form_state;

pub(super) use alerts_and_rows::*;
pub(super) use form_helpers::*;
pub(super) use gas_bar::*;
pub(super) use price_and_costs::*;
use quote_jobs::{existing_pair_side, quote_swap, resume_pair_side, setup_job_result};
use route_requests::{
    approved_bridge_destination, approved_non_evm_destination, request_bridge_routes,
    request_non_evm_routes,
};
use setup_route_settings::{
    SetupRouteSettings, account_reuse_line, new_account_item, receive_to_problem, reuse_line,
    setup_fee_line, setup_route_settings, shield_failure_card,
};

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

struct BridgeRoutesResult<R = BridgeRoutes> {
    orderbook: Option<CowOrderbookClient>,
    bridge_clients: Option<SwapBridgeClients>,
    routes: eyre::Result<R>,
}

pub(super) use crate::root::ui_helpers::network_name;

#[path = "buy_picker.rs"]
mod buy_picker;

#[path = "quote_failure.rs"]
mod quote_failure;

#[cfg(test)]
#[path = "ui_tests.rs"]
mod ui_tests;
