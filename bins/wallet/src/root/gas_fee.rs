use std::sync::Arc;

use gpui::{
    App, AppContext, Context, Entity, ParentElement, Styled, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable,
    input::{InputEvent, InputState},
};
use ui::controls::{app_input, app_muted_text};
use ui::theme;
use wallet_ops::{SelfBroadcastGasFeeQuote, SelfBroadcastGasFeeSelection};

use super::{
    DeliveryFormKind, UnshieldAssetKey, WalletRoot, labeled_field, public_action::PublicActionMode,
};

const GWEI_WEI: u128 = 1_000_000_000;

pub(super) use ui::gas_fee::{
    GasFeeEditTarget as Eip1559GasFeeEditTarget, GasFeeMode as Eip1559GasFeeMode,
};
use ui::gas_fee::{GasFeeEditor, GasFeeEditorEvent};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Eip1559GasFeeTarget {
    Private {
        key: UnshieldAssetKey,
        kind: DeliveryFormKind,
    },
    Public {
        mode: PublicActionMode,
    },
    WalletConnect {
        request_key: Arc<str>,
    },
}

pub(super) struct Eip1559GasFeeEditorState {
    pub(super) mode: Eip1559GasFeeMode,
    pub(super) max_fee_input: Entity<InputState>,
    pub(super) max_priority_fee_input: Entity<InputState>,
    pub(super) pending_programmatic_max_fee_input: Option<Arc<str>>,
    pub(super) pending_programmatic_max_priority_fee_input: Option<Arc<str>>,
    pub(super) quote: Option<SelfBroadcastGasFeeQuote>,
    pub(super) refreshing: bool,
    pub(super) refresh_id: u64,
    pub(super) error: Option<Arc<str>>,
    pub(super) quote_error: Option<Arc<str>>,
}

#[derive(Clone)]
pub(super) struct GasRetryInputs {
    pub(super) max_fee_input: Entity<InputState>,
    pub(super) max_tip_input: Entity<InputState>,
}

impl GasRetryInputs {
    pub(super) fn new<T>(
        initial_max_fee_per_gas: u128,
        initial_max_priority_fee_per_gas: u128,
        window: &mut Window,
        cx: &mut Context<'_, T>,
    ) -> Self {
        let max_fee_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("max fee gwei");
            input.set_value(format_gwei(initial_max_fee_per_gas), window, cx);
            input
        });
        let max_tip_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("max tip gwei");
            input.set_value(format_gwei(initial_max_priority_fee_per_gas), window, cx);
            input
        });
        Self {
            max_fee_input,
            max_tip_input,
        }
    }

    pub(super) fn subscribe_clear_error<T: 'static>(
        &self,
        cx: &mut Context<'_, T>,
        clear: impl Fn(&mut T, &mut Context<'_, T>) + Clone + 'static,
    ) {
        let clear_max_fee = clear.clone();
        cx.subscribe(
            &self.max_fee_input,
            move |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    clear_max_fee(this, cx);
                }
            },
        )
        .detach();

        cx.subscribe(
            &self.max_tip_input,
            move |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    clear(this, cx);
                }
            },
        )
        .detach();
    }

    pub(super) fn render_fields(&self) -> gpui::Div {
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .gap_3()
            .child(
                labeled_field("Max fee (gwei)", app_input(&self.max_fee_input).w_full())
                    .flex_1()
                    .min_w(px(150.0)),
            )
            .child(
                labeled_field("Max tip (gwei)", app_input(&self.max_tip_input).w_full())
                    .flex_1()
                    .min_w(px(150.0)),
            )
    }

    pub(super) fn parse(&self, cx: &App) -> Result<(u128, u128), String> {
        let max_fee = parse_gwei_to_wei(self.max_fee_input.read(cx).value().as_ref())?;
        let max_tip = parse_gwei_to_wei(self.max_tip_input.read(cx).value().as_ref())?;
        validate_custom_gas_fee(max_fee, max_tip)?;
        Ok((max_fee, max_tip))
    }
}

impl Eip1559GasFeeEditorState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<'_, WalletRoot>) -> Self {
        Self {
            mode: Eip1559GasFeeMode::Auto,
            max_fee_input: cx.new(|cx| InputState::new(window, cx).placeholder("max fee gwei")),
            max_priority_fee_input: cx
                .new(|cx| InputState::new(window, cx).placeholder("max tip gwei")),
            pending_programmatic_max_fee_input: None,
            pending_programmatic_max_priority_fee_input: None,
            quote: None,
            refreshing: false,
            refresh_id: 0,
            error: None,
            quote_error: None,
        }
    }

    pub(super) fn selection(&self, cx: &App) -> Result<SelfBroadcastGasFeeSelection, String> {
        match self.mode {
            Eip1559GasFeeMode::Auto => Ok(SelfBroadcastGasFeeSelection::Auto),
            Eip1559GasFeeMode::Custom => {
                let max_fee_per_gas =
                    parse_gwei_to_wei(self.max_fee_input.read(cx).value().as_ref())?;
                let max_priority_fee_per_gas =
                    parse_gwei_to_wei(self.max_priority_fee_input.read(cx).value().as_ref())?;
                validate_custom_gas_fee(max_fee_per_gas, max_priority_fee_per_gas)?;
                Ok(SelfBroadcastGasFeeSelection::Custom {
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                })
            }
        }
    }

    pub(super) fn reset_for_request<T>(&mut self, window: &mut Window, cx: &mut Context<'_, T>) {
        self.mode = Eip1559GasFeeMode::Auto;
        self.quote = None;
        self.refreshing = false;
        self.refresh_id = self.refresh_id.wrapping_add(1);
        self.error = None;
        self.quote_error = None;
        self.pending_programmatic_max_fee_input = Some(Arc::from(""));
        self.pending_programmatic_max_priority_fee_input = Some(Arc::from(""));
        self.max_fee_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.max_priority_fee_input
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    pub(super) fn seed_custom_from_auto_if_empty(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, WalletRoot>,
    ) {
        let Some(quote) = self.quote else {
            return;
        };
        let max_fee_empty = self.max_fee_input.read(cx).value().trim().is_empty();
        let max_priority_fee_empty = self
            .max_priority_fee_input
            .read(cx)
            .value()
            .trim()
            .is_empty();
        if !max_fee_empty || !max_priority_fee_empty {
            return;
        }

        let max_fee = format_gwei(quote.suggested_max_fee_per_gas);
        let max_priority_fee = format_gwei(quote.suggested_max_priority_fee_per_gas);
        self.pending_programmatic_max_fee_input = Some(Arc::from(max_fee.as_str()));
        self.pending_programmatic_max_priority_fee_input =
            Some(Arc::from(max_priority_fee.as_str()));
        self.max_fee_input
            .update(cx, |input, cx| input.set_value(max_fee, window, cx));
        self.max_priority_fee_input.update(cx, |input, cx| {
            input.set_value(max_priority_fee, window, cx);
        });
    }

    pub(super) fn overwrite_custom_from_auto(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, WalletRoot>,
    ) -> bool {
        let Some(quote) = self.quote else {
            return false;
        };
        let max_fee = format_gwei(quote.suggested_max_fee_per_gas);
        let max_priority_fee = format_gwei(quote.suggested_max_priority_fee_per_gas);
        self.pending_programmatic_max_fee_input = Some(Arc::from(max_fee.as_str()));
        self.pending_programmatic_max_priority_fee_input =
            Some(Arc::from(max_priority_fee.as_str()));
        self.max_fee_input
            .update(cx, |input, cx| input.set_value(max_fee, window, cx));
        self.max_priority_fee_input.update(cx, |input, cx| {
            input.set_value(max_priority_fee, window, cx);
        });
        true
    }

    pub(super) fn consume_programmatic_input_change(
        &mut self,
        target: Eip1559GasFeeEditTarget,
        current: &str,
    ) -> bool {
        let pending = match target {
            Eip1559GasFeeEditTarget::MaxFee => &mut self.pending_programmatic_max_fee_input,
            Eip1559GasFeeEditTarget::MaxTip => {
                &mut self.pending_programmatic_max_priority_fee_input
            }
        };
        consume_pending_programmatic_input_change(pending, current)
    }
}

pub(super) fn consume_pending_programmatic_input_change(
    pending: &mut Option<Arc<str>>,
    current: &str,
) -> bool {
    let Some(expected) = pending.take() else {
        return false;
    };
    expected.as_ref() == current
}

impl WalletRoot {
    pub(super) fn set_eip1559_gas_fee_mode(
        &mut self,
        target: Eip1559GasFeeTarget,
        mode: Eip1559GasFeeMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match target {
            Eip1559GasFeeTarget::Private { key, kind } => {
                self.set_self_broadcast_gas_fee_mode(kind, key, mode, window, cx);
            }
            Eip1559GasFeeTarget::Public { mode: action_mode } => {
                self.set_public_action_gas_fee_mode(action_mode, mode, window, cx);
            }
            Eip1559GasFeeTarget::WalletConnect { request_key } => {
                self.set_walletconnect_gas_fee_mode(&request_key, mode, window, cx);
            }
        }
    }

    pub(super) fn refresh_eip1559_gas_fee_quote(
        &mut self,
        target: Eip1559GasFeeTarget,
        cx: &mut Context<'_, Self>,
    ) {
        match target {
            Eip1559GasFeeTarget::Private { key, kind } => {
                self.refresh_self_broadcast_gas_fee_quote(kind, key, cx);
            }
            Eip1559GasFeeTarget::Public { mode } => {
                self.refresh_public_action_gas_fee_quote(mode, cx);
            }
            Eip1559GasFeeTarget::WalletConnect { request_key } => {
                self.refresh_walletconnect_gas_fee_quote(request_key, cx);
            }
        }
    }

    pub(super) fn customize_eip1559_gas_fee_from_auto(
        &mut self,
        target: Eip1559GasFeeTarget,
        edit_target: Eip1559GasFeeEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match target {
            Eip1559GasFeeTarget::Private { key, kind } => {
                self.customize_self_broadcast_gas_fee_from_auto(kind, key, edit_target, window, cx);
            }
            Eip1559GasFeeTarget::Public { mode } => {
                self.customize_public_action_gas_fee_from_auto(mode, edit_target, window, cx);
            }
            Eip1559GasFeeTarget::WalletConnect { request_key } => {
                self.customize_walletconnect_gas_fee_from_auto(
                    &request_key,
                    edit_target,
                    window,
                    cx,
                );
            }
        }
    }
}

pub(super) fn render_eip1559_gas_fee_editor(
    root: Entity<WalletRoot>,
    target: &Eip1559GasFeeTarget,
    state: &Eip1559GasFeeEditorState,
    disabled: bool,
) -> gpui::Div {
    let target_id = gas_fee_target_id(target);
    let target = target.clone();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            GasFeeEditor::new(
                format!("wallet-eip1559-gas-{target_id}"),
                &state.max_fee_input,
                &state.max_priority_fee_input,
                move |event, window, cx| {
                    root.update(cx, |root, cx| match *event {
                        GasFeeEditorEvent::Mode(mode) => {
                            root.set_eip1559_gas_fee_mode(target.clone(), mode, window, cx);
                        }
                        GasFeeEditorEvent::Refresh => {
                            root.refresh_eip1559_gas_fee_quote(target.clone(), cx);
                        }
                        GasFeeEditorEvent::Edit(field) => {
                            root.customize_eip1559_gas_fee_from_auto(
                                target.clone(),
                                field,
                                window,
                                cx,
                            );
                        }
                    });
                },
            )
            .mode(state.mode)
            .quote(state.quote.map(|quote| {
                (
                    format_gwei(quote.suggested_max_fee_per_gas),
                    format_gwei(quote.suggested_max_priority_fee_per_gas),
                )
            }))
            .refreshing(state.refreshing)
            .disabled(disabled),
        )
        .when_some(state.error.as_ref(), |this, error| {
            this.child(app_muted_text(error.to_string()).text_color(rgb(theme::DANGER)))
        })
        .when_some(state.quote_error.as_ref(), |this, error| {
            let color = if state.quote.is_some() {
                theme::WARNING
            } else {
                theme::DANGER
            };
            this.child(app_muted_text(error.to_string()).text_color(rgb(color)))
        })
}

fn gas_fee_target_id(target: &Eip1559GasFeeTarget) -> String {
    match target {
        Eip1559GasFeeTarget::Private { key, kind } => {
            format!(
                "private-{}-{}-{}",
                key.chain_id,
                key.token,
                gas_fee_kind_id(*kind)
            )
        }
        Eip1559GasFeeTarget::Public { mode } => {
            format!("public-{}", public_action_mode_id(*mode))
        }
        Eip1559GasFeeTarget::WalletConnect { request_key } => {
            format!("walletconnect-{request_key}")
        }
    }
}

const fn gas_fee_kind_id(kind: DeliveryFormKind) -> &'static str {
    match kind {
        DeliveryFormKind::Send => "send",
        DeliveryFormKind::Unshield => "unshield",
    }
}

const fn public_action_mode_id(mode: PublicActionMode) -> &'static str {
    match mode {
        PublicActionMode::Shield => "shield",
        PublicActionMode::Send => "send",
    }
}

pub(super) fn parse_gwei_to_wei(input: &str) -> Result<u128, String> {
    let value = input.trim();
    if value.is_empty() {
        return Err("Enter a gas fee in gwei".to_string());
    }
    if value.starts_with('-') {
        return Err("Gas fee cannot be negative".to_string());
    }
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fractional = parts.next();
    if parts.next().is_some() || whole.is_empty() && fractional.is_none() {
        return Err("Enter a valid decimal gwei amount".to_string());
    }
    let whole_wei = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u128>()
            .map_err(|_| "Enter a valid decimal gwei amount".to_string())?
            .checked_mul(GWEI_WEI)
            .ok_or_else(|| "Gas fee is too large".to_string())?
    };
    let fractional_wei = if let Some(fractional) = fractional {
        if fractional.len() > 9 || !fractional.chars().all(|ch| ch.is_ascii_digit()) {
            return Err("Enter no more than 9 decimal places for gwei".to_string());
        }
        let mut padded = fractional.to_string();
        while padded.len() < 9 {
            padded.push('0');
        }
        padded
            .parse::<u128>()
            .map_err(|_| "Enter a valid decimal gwei amount".to_string())?
    } else {
        0
    };
    whole_wei
        .checked_add(fractional_wei)
        .ok_or_else(|| "Gas fee is too large".to_string())
}

pub(super) fn validate_custom_gas_fee(
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Result<(), String> {
    if max_fee_per_gas == 0 {
        return Err("Max fee must be greater than 0 gwei".to_string());
    }
    if max_priority_fee_per_gas > max_fee_per_gas {
        return Err("Max tip cannot exceed max fee".to_string());
    }
    Ok(())
}

pub(super) fn format_gwei(wei: u128) -> String {
    let whole = wei / GWEI_WEI;
    let fractional = wei % GWEI_WEI;
    if fractional == 0 {
        return whole.to_string();
    }
    let mut fractional = format!("{fractional:09}");
    while fractional.ends_with('0') {
        fractional.pop();
    }
    format!("{whole}.{fractional}")
}
