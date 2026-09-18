use alloy::primitives::Address;
use gpui::{
    App, AppContext as _, Context, Entity, Focusable, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Window, div,
};
use gpui_component::{
    Sizable as _,
    input::{InputEvent, InputState},
    select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState},
};
use ui::controls::app_input;
use wallet_ops::settings::EffectiveTokenRegistry;

use super::labeled_field;
use crate::{assets::WalletIconSource, root::tokens::token_display_metadata};

#[derive(Clone)]
struct TokenItem {
    address: Option<Address>,
    label: SharedString,
    icon: Option<WalletIconSource>,
}

impl SelectItem for TokenItem {
    type Value = Option<Address>;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(self.row().into_any_element())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.row()
    }

    fn value(&self) -> &Self::Value {
        &self.address
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_ascii_lowercase();
        self.address.is_none()
            || self.label.to_ascii_lowercase().contains(&query)
            || self.address.is_some_and(|address| {
                address
                    .to_checksum(None)
                    .to_ascii_lowercase()
                    .contains(&query)
            })
    }
}

impl TokenItem {
    fn row(&self) -> gpui::Div {
        ui::private_action::asset_row(self.label.clone(), self.icon.clone().map(Into::into))
    }
}

fn token_items(registry: &EffectiveTokenRegistry, chain_id: u64) -> Vec<TokenItem> {
    let mut items = registry
        .tokens
        .values()
        .filter(|token| token.chain_id == chain_id)
        .map(|token| {
            let address = token
                .token_address
                .parse()
                .expect("validated token address");
            TokenItem {
                address: Some(address),
                label: format!("{} · {}", token.symbol, railgun_ui::short_address(&address)).into(),
                icon: token_display_metadata(Some(registry), chain_id, &address)
                    .and_then(|metadata| metadata.icon_path),
            }
        })
        .collect::<Vec<_>>();
    items.sort_by_key(|item| item.label.to_ascii_lowercase());
    items.push(TokenItem {
        address: None,
        label: "Custom address…".into(),
        icon: None,
    });
    items
}

pub(super) struct TokenPicker {
    select: Entity<SelectState<SearchableVec<TokenItem>>>,
    address: Entity<InputState>,
}

impl TokenPicker {
    pub(super) fn new(
        registry: &EffectiveTokenRegistry,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(token_items(registry, chain_id)),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        let address = cx.new(|cx| InputState::new(window, cx).placeholder("0x token address"));
        cx.subscribe_in(&select, window, |_, _, event, window, cx| {
            if matches!(event, SelectEvent::Confirm(Some(None))) {
                cx.defer_in(window, |this, window, cx| {
                    this.address.read(cx).focus_handle(cx).focus(window, cx);
                });
            }
            cx.notify();
        })
        .detach();
        cx.subscribe(&address, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        Self { select, address }
    }

    pub(super) fn token(&self, cx: &App) -> Result<Address, String> {
        match self.select.read(cx).selected_value() {
            Some(Some(address)) => Ok(*address),
            Some(None) => self
                .address
                .read(cx)
                .value()
                .trim()
                .parse()
                .map_err(|_| "Enter a valid token address.".to_owned()),
            None => Err("Choose a token or select Custom address.".to_owned()),
        }
    }
}

impl Focusable for TokenPicker {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.select.read(cx).focus_handle(cx)
    }
}

impl Render for TokenPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let mut fields = div().w_full().min_w_0().flex().flex_col().gap_2().child(
            labeled_field(
                "Token",
                Select::new(&self.select)
                    .small()
                    .w_full()
                    .placeholder("Choose a token")
                    .accessibility_label("Token")
                    .search_placeholder("Search symbol or address"),
            )
            // Selecting a token must not also submit the containing dialog.
            .on_action(|_: &gpui_component::dialog::Confirm, _, cx| cx.stop_propagation()),
        );
        if self.select.read(cx).selected_value() == Some(&None) {
            fields = fields.child(labeled_field(
                "Token address",
                app_input(&self.address).small().aria_label("Token address"),
            ));
        }
        fields
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wallet_ops::settings::{
        CustomTokenSettings, WalletSettings, build_effective_token_registry,
    };

    #[test]
    fn known_tokens_follow_the_chain_and_include_settings_tokens() {
        let custom = Address::repeat_byte(0x42);
        let mut settings = WalletSettings::default();
        settings.tokens.custom_tokens.push(CustomTokenSettings {
            chain_id: 1,
            token_address: custom.to_string(),
            symbol: "CUSTOM".into(),
            ..Default::default()
        });
        let registry = build_effective_token_registry(&settings).unwrap();
        for chain_id in [1, 137] {
            let items = token_items(&registry, chain_id);
            let addresses = items
                .iter()
                .filter_map(|item| item.address)
                .collect::<std::collections::BTreeSet<_>>();
            let expected = registry
                .tokens
                .values()
                .filter(|token| token.chain_id == chain_id)
                .map(|token| token.token_address.parse::<Address>().unwrap())
                .collect();
            assert_eq!(addresses, expected);
            assert_eq!(addresses.contains(&custom), chain_id == 1);
            let custom_choice = items.iter().find(|item| item.address.is_none()).unwrap();
            assert!(custom_choice.matches("unknown token"));
            let known = items.iter().find(|item| item.address.is_some()).unwrap();
            assert!(known.matches(&known.address.unwrap().to_checksum(None)));
        }
    }
}
