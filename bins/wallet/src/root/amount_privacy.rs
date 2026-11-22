//! Amount privacy hints on the Shield and Unshield forms and in their review: the inputs each
//! form reads from local data, and the component that shows wallet-ops' evaluation. wallet-ops
//! owns every threshold and this module owns every word. Nothing here sends a request, waits,
//! or changes whether an action can proceed.

use std::cell::Cell;
use std::rc::Rc;
use std::time::SystemTime;

use alloy::primitives::{Address, U256};
use gpui::{
    AnyElement, App, Context, FontWeight, HighlightStyle, Hsla, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, StyledText, Window,
    accesskit::Role, div, prelude::FluentBuilder as _, px, relative, rgb,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, button::ButtonVariants as _, popover::Popover,
};
use gpui_kit::TestSupportExt as _;
use ui::controls::{app_button_base, app_muted_text};
use ui::hint::hint_card;
use ui::theme::{self, APP_TEXT_LINE_HEIGHT, APP_TEXT_SIZE};
use wallet_ops::{
    AmountStatisticsStatus, FeeHandlingMode, NetworkRoute, OwnDeposit, PrivacyLevel, PublicAssetId,
    PublicBroadcasterCostEstimate, RAILGUN_PROTOCOL_FEE_BPS, RpcTransport, ShieldHintInput,
    UnshieldHintInput, UnshieldSender, WalletNetworkMode, amount_statistics_status_for_chain,
    evaluate_shield_hints, evaluate_unshield_hints, parse_send_amount, recipient_deposited,
    settings::EffectiveChainConfig,
};

use crate::assets::RailgunActionIcon;

use super::private_action::{SelfBroadcastFundingMode, effective_delivery_funding_mode};
use super::ui_helpers::now_unix;
use super::{
    ChainUtxoState, DeliveryMode, PublicActionMode, UnshieldAsset, UnshieldAssetKey,
    UnshieldFormState, WalletRoot, format_send_amount_input, format_unshield_amount_input,
    public_asset_decimals, public_asset_label, unshield_form_max_entered_amount,
};

mod common_amounts;
mod presentation;

use common_amounts::{CommonAmountsTrigger, render_common_amounts};
pub(in crate::root) use presentation::AmountPrivacyPresentation;
use presentation::{
    AMOUNT_CROWD_KEY, PrivacyFactorLine, TREES_INFO_LABEL, TREES_INFO_TEXT, TREES_INFO_TITLE,
    shield_presentation, unshield_presentation,
};

/// Called with an amount to type, before the shield fee, when a choosable common amount is picked.
pub(in crate::root) type UseAmount = Rc<dyn Fn(U256, &mut Window, &mut App)>;

/// Right padding that keeps the card's first row clear of the corner icon.
const ICON_CLEARANCE: f32 = 28.0;

/// The hints under an amount, or in a review, in a dashed card with the stealth icon in its
/// corner; nothing to show leaves an empty element and no card. `id` names this instance; each
/// factor row's id adds its key. The common amounts show only with `on_use_amount`: for the
/// amount-crowd factor, behind an icon on it when it is good, otherwise behind a button under it;
/// without that factor, behind a button at the end of the card.
pub(in crate::root) fn render_amount_privacy_hints(
    id: &str,
    presentation: &AmountPrivacyPresentation,
    on_use_amount: Option<UseAmount>,
    cx: &App,
) -> gpui::Div {
    let (common_icon, common_row, common_end) =
        common_amounts_trigger(id, presentation, on_use_amount);
    let empty = presentation.lines.is_empty()
        && presentation.notice.is_none()
        && presentation.factors.is_empty()
        && common_end.is_none();
    // Only the first row runs beside the corner icon, so only it keeps clear of it: the icon
    // reaches 14px into the content area, plus a 14px gap. When the factors come first, that is
    // their first row, not the whole list.
    let factor_rows = render_factor_rows(id, presentation, common_icon, common_row, cx);
    let no_factors = factor_rows.is_empty();
    let first = Cell::new(true);
    let clear_icon =
        |row: gpui::Div| row.when(first.replace(false), |row| row.pr(px(ICON_CLEARANCE)));
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .when(!empty, privacy_card)
        .children(
            presentation
                .lines
                .iter()
                .map(|line| clear_icon(app_muted_text(line.clone()).whitespace_normal())),
        )
        .when_some(presentation.notice, |this, notice| {
            this.child(clear_icon(app_muted_text(notice).whitespace_normal()))
        })
        .when(!no_factors, |this| {
            this.child(
                div()
                    .id(SharedString::from(format!("{id}-factors")))
                    .role(Role::List)
                    .aria_label("Privacy factors")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(factor_rows),
            )
        })
        // Keeps clear of the corner icon only as the card's first row.
        .children(common_end.map(|row| if no_factors { clear_icon(row) } else { row }))
}

/// One row per factor, the first clear of the corner icon when nothing precedes the factors,
/// with the common amounts' icon on the amount-crowd factor or their row under it.
fn render_factor_rows(
    id: &str,
    presentation: &AmountPrivacyPresentation,
    mut common_icon: Option<AnyElement>,
    mut common_row: Option<AnyElement>,
    cx: &App,
) -> Vec<AnyElement> {
    let factors_first = presentation.lines.is_empty() && presentation.notice.is_none();
    let mut rows = Vec::new();
    for (index, factor) in presentation.factors.iter().enumerate() {
        let clear = factors_first && index == 0;
        let amount_crowd = factor.key == AMOUNT_CROWD_KEY;
        let icon = common_icon.take_if(|_| amount_crowd);
        rows.push(render_privacy_factor(id, factor, clear, icon, cx).into_any_element());
        if amount_crowd {
            rows.extend(common_row.take());
        }
    }
    rows
}

/// The common amounts as the icon on the amount-crowd factor, the row under it, or the row at
/// the end of the card; at most one is set, and none without `on_use` or common amounts. A good
/// factor gets the icon, any other the row under it with the labelled button, and without the
/// factor the labelled button goes at the end, its entered amount in the primary colour.
fn common_amounts_trigger(
    id: &str,
    presentation: &AmountPrivacyPresentation,
    on_use: Option<UseAmount>,
) -> (Option<AnyElement>, Option<AnyElement>, Option<gpui::Div>) {
    let (Some(on_use), Some(common)) = (on_use, presentation.common_amounts.as_ref()) else {
        return (None, None, None);
    };
    let level = presentation
        .factors
        .iter()
        .find(|factor| factor.key == AMOUNT_CROWD_KEY)
        .map(|factor| factor.level);
    let Some(level) = level else {
        let button = render_common_amounts(
            id,
            common.clone(),
            None,
            on_use,
            CommonAmountsTrigger::Button,
        );
        return (None, None, Some(div().flex().child(button)));
    };
    let trigger = if level == PrivacyLevel::Good {
        CommonAmountsTrigger::Icon
    } else {
        CommonAmountsTrigger::Button
    };
    let element =
        render_common_amounts(id, common.clone(), Some(level), on_use, trigger).into_any_element();
    match trigger {
        CommonAmountsTrigger::Icon => (Some(element), None, None),
        CommonAmountsTrigger::Button => {
            // Under the factor's text: past its 14px level icon, the icon's 4px margin and the
            // row's 4px gap.
            let row = div().flex().pl(px(22.0)).child(element);
            (None, Some(row.into_any_element()), None)
        }
    }
}

/// The card around the hints: a dashed border on the surrounding background, with the stealth
/// icon in its top-right corner.
fn privacy_card(hints: gpui::Div) -> gpui::Div {
    hints
        .px_3()
        .py(px(10.0))
        .rounded_lg()
        .border_1()
        .border_dashed()
        .border_color(rgb(theme::BORDER_STRONG))
        // Decorative: an icon without an id or role has no accessibility node.
        .child(
            Icon::new(RailgunActionIcon::HatGlasses)
                .absolute()
                // Centred on the first 21px text line below the 10px top padding.
                .top(px(12.5))
                .right(px(10.0))
                .size_4()
                .text_color(rgb(theme::TEXT_MUTED)),
        )
}

/// The level's icon in its theme colour, then the line with its emphasised figures in bold, then
/// the explanation of trees when the line names one, or the common amounts' icon when given. The
/// icon's shape and the accessibility label name the level, so neither depends on colour.
fn render_privacy_factor(
    id: &str,
    factor: &PrivacyFactorLine,
    clear_icon: bool,
    common_amounts: Option<AnyElement>,
    cx: &App,
) -> impl IntoElement {
    let (icon, color) = level_style(factor.level, cx);
    let bold = HighlightStyle {
        font_weight: Some(FontWeight::SEMIBOLD),
        ..HighlightStyle::default()
    };
    let row_id = format!("{id}-factor-{}", factor.key);
    div()
        .id(SharedString::from(row_id.clone()))
        .role(Role::ListItem)
        .aria_label(factor.accessibility_label())
        .flex()
        .items_start()
        .gap_1()
        .when(clear_icon, |row| row.pr(px(ICON_CLEARANCE)))
        // Twice as far from the text as the info button, which sits as close as other info icons.
        .child(
            first_line_box()
                .mr_1()
                .child(Icon::new(icon).small().text_color(color)),
        )
        .child(
            // Not growing, so the info button follows the text.
            div()
                .flex_initial()
                .min_w_0()
                .whitespace_normal()
                .text_size(APP_TEXT_SIZE)
                .line_height(relative(APP_TEXT_LINE_HEIGHT))
                .child(
                    StyledText::new(factor.text.clone())
                        .with_highlights(factor.emphasis.iter().map(|range| (range.clone(), bold))),
                ),
        )
        .when(factor.explains_trees, |this| {
            this.child(render_trees_info(&row_id))
        })
        .children(common_amounts)
        .test_support()
}

/// The level's icon and its theme colour.
fn level_style(level: PrivacyLevel, cx: &App) -> (IconName, Hsla) {
    let theme = cx.theme();
    match level {
        PrivacyLevel::Bad => (IconName::CircleX, theme.danger),
        PrivacyLevel::Caution => (IconName::TriangleAlert, theme.warning),
        PrivacyLevel::Good => (IconName::CircleCheck, theme.success),
    }
}

/// One text line high, so what it holds centres on the first line of a wrapped one.
fn first_line_box() -> gpui::Div {
    div()
        .flex_none()
        .h(APP_TEXT_SIZE * APP_TEXT_LINE_HEIGHT)
        .flex()
        .items_center()
}

/// An info button whose popover explains trees.
fn render_trees_info(row_id: &str) -> impl IntoElement {
    first_line_box()
        .id(SharedString::from(format!("{row_id}-trees")))
        .child(
            Popover::new(SharedString::from(format!("{row_id}-trees-popover")))
                .trigger(
                    app_button_base(SharedString::from(format!("{row_id}-trees-button")))
                        .ghost()
                        .small()
                        // The other info icons' 14px icon in an 18px box, in their colour.
                        .size(px(18.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .icon(IconName::Info)
                        .accessibility_label(TREES_INFO_LABEL),
                )
                .content(|_, window, _| {
                    hint_card(TREES_INFO_TITLE, theme::INFO, window)
                        .child(div().whitespace_normal().child(TREES_INFO_TEXT))
                }),
        )
        .test_support()
}

impl WalletRoot {
    /// Fills the Shield amount as typing it would, so the hints follow it.
    pub(in crate::root) fn use_shield_amount(
        &mut self,
        amount: U256,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let decimals = self.public_form.selected_asset.and_then(|asset| {
            public_asset_decimals(
                self.effective_chain_configs.get(self.last_evm_chain),
                asset,
                Some(&self.effective_token_registry),
            )
        });
        let value = format_send_amount_input(amount, decimals);
        self.public_form
            .shield_amount_input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        self.set_public_action_error(PublicActionMode::Shield, None);
        self.refresh_native_shield_preparation(cx);
        cx.notify();
    }

    /// Fills the Unshield amount as typing it would: unlike `set_value`, `replace_all` emits
    /// `InputEvent::Change`, so the amount input's subscription clears the edit state and
    /// re-estimates. Common exit amounts can be chosen only when fees are deducted from the
    /// amount, where the field holds the exit's value.
    pub(in crate::root) fn use_unshield_amount(
        &self,
        key: UnshieldAssetKey,
        amount: U256,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self
            .unshield_forms
            .get(&key)
            .filter(|form| !form.generating)
        else {
            return;
        };
        let value = format_unshield_amount_input(amount, form.asset.decimals);
        form.amount_input
            .update(cx, |input, cx| input.replace_all(value, window, cx));
    }

    /// The Shield form's hints for the entered amount, `None` until one is entered. A base-token
    /// shield reads the statistics of its wrapped token.
    pub(in crate::root) fn shield_privacy_presentation(
        &self,
        cx: &App,
    ) -> Option<AmountPrivacyPresentation> {
        let asset = self.public_form.selected_asset?;
        let chain_id = self.last_evm_chain;
        let chain = self.effective_chain_configs.get(chain_id);
        let token = shield_statistics_token(chain, asset)?;
        let decimals = public_asset_decimals(chain, asset, Some(&self.effective_token_registry));
        let amount = parse_send_amount(
            self.public_form
                .shield_amount_input
                .read(cx)
                .value()
                .as_ref(),
            decimals,
        )
        .ok()
        .filter(|amount| !amount.is_zero())?;
        let balance = self
            .selected_public_balance_entry()
            .and_then(|entry| entry.amount.amount())
            .unwrap_or(U256::ZERO);
        // The forest's tip is only the current tree once the wallet has synced.
        let current_tree = match self.chain_states.get(&chain_id) {
            Some(ChainUtxoState::Ready { session, .. }) => session.merkle_forest_tip_tree(),
            _ => None,
        };
        #[cfg(debug_assertions)]
        let (balance, current_tree) =
            super::private_swap::ui_fixture_shield_hint_stand_ins(decimals)
                .unwrap_or((balance, current_tree));
        let statistics = self.amount_statistics(chain_id);
        let hints = evaluate_shield_hints(&ShieldHintInput {
            statistics: &statistics,
            token,
            amount,
            shield_fee_bps: RAILGUN_PROTOCOL_FEE_BPS,
            balance,
            current_tree,
            route: self.amount_privacy_route(chain_id),
        });
        let symbol = public_asset_label(chain, asset, Some(&self.effective_token_registry));
        Some(shield_presentation(&hints, decimals, &symbol))
    }

    /// The Unshield form's hints. The plan comes from the broadcaster estimate in broadcaster
    /// delivery and from the form's own selection otherwise; without one the hints that need
    /// it are left out.
    pub(in crate::root) fn unshield_privacy_presentation(
        &self,
        key: UnshieldAssetKey,
    ) -> Option<AmountPrivacyPresentation> {
        let form = self.unshield_forms.get(&key)?;
        let chain_id = form.asset.chain_id;
        // A private balance of the base token is already its wrapped token.
        let token = form.asset.token;
        let estimate = hint_estimate(form);
        // The estimate's own amount and fee choice, which its plan follows.
        let entered_amount_with_deducted_fee = estimate
            .filter(|estimate| {
                estimate.fee_mode == FeeHandlingMode::DeductFromAmount
                    && estimate.fee_token == estimate.action_token
            })
            .map(|estimate| estimate.entered_amount);
        let planned = if form.delivery_mode == DeliveryMode::PublicBroadcaster {
            estimate.and_then(|estimate| estimate.planned_unshields.as_deref())
        } else {
            // A native top-up or a private sponsorship selects its notes another way.
            let funding = effective_delivery_funding_mode(
                form.delivery_mode,
                self.effective_chain_configs.get(chain_id),
                form.self_broadcast_funding,
            );
            form.self_broadcast_plan.as_deref().filter(|_| {
                !form.native_top_up_enabled && funding == SelfBroadcastFundingMode::PublicBalance
            })
        };
        let own_deposits = self.own_shield_deposits(&form.asset);
        let sender = match form.delivery_mode {
            DeliveryMode::SelfBroadcast => UnshieldSender::PublicAccount,
            DeliveryMode::PublicBroadcaster => UnshieldSender::Broadcaster,
            DeliveryMode::ManualCalldata => UnshieldSender::CalldataOnly,
        };
        let recipient_deposited =
            form.recipient_value
                .trim()
                .parse::<Address>()
                .is_ok_and(|recipient| {
                    recipient_deposited(
                        recipient,
                        chain_id,
                        &self.public_accounts,
                        self.wallet_activity_entries(),
                    )
                });
        let statistics = self.amount_statistics(chain_id);
        let hints = evaluate_unshield_hints(&UnshieldHintInput {
            statistics: &statistics,
            token,
            planned,
            own_deposits: &own_deposits,
            sender,
            recipient_deposited,
            route: self.amount_privacy_route(chain_id),
            now: now_unix(),
            entered_amount_with_deducted_fee,
            max_amount: unshield_form_max_entered_amount(form, form.delivery_mode, form.fee_mode),
        });
        Some(unshield_presentation(
            &hints,
            form.asset.decimals,
            &form.asset.label,
            // With fees added on top, the amount field holds what the recipient gets, not the exit.
            form.fee_mode == FeeHandlingMode::DeductFromAmount,
        ))
    }

    /// The wallet's own deposits of `asset`'s token, from its chain's session.
    fn own_shield_deposits(&self, asset: &UnshieldAsset) -> Vec<OwnDeposit> {
        #[cfg(debug_assertions)]
        if let Some(deposits) =
            super::private_swap::ui_fixture_own_deposit_stand_ins(asset.decimals)
        {
            return deposits;
        }
        match self.chain_states.get(&asset.chain_id) {
            Some(
                ChainUtxoState::Ready { session, .. } | ChainUtxoState::Syncing { session, .. },
            ) => session.own_shield_deposits(asset.token),
            _ => Vec::new(),
        }
    }

    /// The chain's amount statistics, read from its session's memory.
    fn amount_statistics(&self, chain_id: u64) -> AmountStatisticsStatus {
        #[cfg(debug_assertions)]
        if let Some(status) = super::private_swap::ui_fixture_amount_statistics(
            chain_id,
            self.effective_chain_configs
                .get(chain_id)
                .and_then(|chain| chain.wrapped_native_token),
        ) {
            return status;
        }
        match self.chain_states.get(&chain_id) {
            Some(
                ChainUtxoState::Ready { session, .. } | ChainUtxoState::Syncing { session, .. },
            ) => session.amount_statistics_status(&self.effective_chain_configs, SystemTime::now()),
            _ => amount_statistics_status_for_chain(
                self.effective_chain_configs.get(chain_id),
                || None,
            ),
        }
    }

    fn amount_privacy_route(&self, chain_id: u64) -> NetworkRoute {
        network_route(
            self.effective_chain_configs.get(chain_id),
            self.http.network_mode(),
        )
    }
}

/// The broadcaster estimate whose plan the Unshield hints follow in broadcaster delivery: the
/// current one, or while a refresh is computed, the one it replaced, so the card keeps its lines.
fn hint_estimate(form: &UnshieldFormState) -> Option<&PublicBroadcasterCostEstimate> {
    form.cost_estimate
        .as_ref()
        .or_else(|| {
            form.replaced_estimate
                .as_ref()
                .filter(|_| form.estimating_cost)
        })
        .filter(|_| form.delivery_mode == DeliveryMode::PublicBroadcaster)
}

/// The token whose statistics a Shield of `asset` reads: the pool holds a base-token shield as
/// its wrapped token.
fn shield_statistics_token(
    chain: Option<&EffectiveChainConfig>,
    asset: PublicAssetId,
) -> Option<Address> {
    match asset {
        PublicAssetId::Erc20(token) => Some(token),
        PublicAssetId::Native => chain?.wrapped_native_token,
    }
}

/// Direct in direct network mode or when `chain` has an RPC endpoint marked direct, which may
/// carry its traffic.
fn network_route(
    chain: Option<&EffectiveChainConfig>,
    network_mode: WalletNetworkMode,
) -> NetworkRoute {
    let direct_endpoint = chain.is_some_and(|chain| {
        chain
            .rpc_route
            .endpoint_urls()
            .iter()
            .any(|url| chain.rpc_route.endpoint_transport(url) == RpcTransport::Direct)
    });
    if direct_endpoint || network_mode == WalletNetworkMode::Direct {
        NetworkRoute::Direct
    } else {
        NetworkRoute::Private
    }
}

#[cfg(test)]
mod tests;
