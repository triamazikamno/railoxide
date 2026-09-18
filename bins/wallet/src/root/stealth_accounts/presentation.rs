use alloy::primitives::Address;
use gpui::{
    Context, Div, InteractiveElement as _, ParentElement as _, SharedString, Styled as _, div,
};
use gpui_component::{ActiveTheme as _, IconName, Sizable as _, button::ButtonVariants as _};
use ui::controls::{app_button_base, app_text};

use super::{ExecutorAsset, StealthAccountsView};

impl StealthAccountsView {
    pub(super) fn asset_address(
        id: SharedString,
        asset: ExecutorAsset,
        cx: &Context<'_, Self>,
    ) -> Option<Div> {
        match asset {
            ExecutorAsset::Native => None,
            ExecutorAsset::Erc20(address)
            | ExecutorAsset::Erc721 {
                collection: address,
                ..
            } => Some(Self::address(id, address, cx)),
        }
    }

    pub(super) fn address(id: SharedString, address: Address, cx: &Context<'_, Self>) -> Div {
        Self::copyable_identifier(
            id,
            address.to_checksum(None),
            railgun_ui::short_address(&address),
            "Copy address",
            cx,
        )
    }

    pub(super) fn copyable_identifier(
        id: SharedString,
        value: String,
        display: String,
        tooltip: &'static str,
        cx: &Context<'_, Self>,
    ) -> Div {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .text_color(gpui::rgb(ui::theme::TEXT_MUTED))
            .child(
                app_text(display)
                    .text_xs()
                    .font_family(ui::theme::APP_MONO_FONT_FAMILY),
            )
            .child(Self::copy_identifier_button(id, value, tooltip, cx))
    }

    pub(super) fn copy_identifier_button(
        id: SharedString,
        value: String,
        tooltip: &'static str,
        cx: &Context<'_, Self>,
    ) -> gpui_component::button::Button {
        let view = cx.weak_entity();
        let selector = id.to_string();
        app_button_base(id)
            .debug_selector(move || selector)
            .ghost()
            .xsmall()
            .compact()
            .icon(IconName::Copy)
            .accessibility_label(tooltip)
            .tooltip(tooltip)
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                if view
                    .upgrade()
                    .is_some_and(|view| view.read(cx).session_is_current(cx))
                {
                    ui::clipboard::copy_to_clipboard_with_toast(value.clone(), window, cx);
                }
            })
    }

    pub(super) fn purpose(id: SharedString, summary: &str, cx: &Context<'_, Self>) -> Div {
        // Existing encrypted summaries store the recipient after this separator.
        // Decode that value with Alloy while leaving the stored intent untouched.
        let recipient = summary.split_once(" → ").and_then(|(intent, recipient)| {
            let (address, suffix) = recipient.split_once(' ').unwrap_or((recipient, ""));
            address
                .parse::<Address>()
                .ok()
                .map(|address| (intent, address, suffix))
        });
        let content = div().flex().flex_wrap().items_center().gap_x_1();
        if let Some((intent, address, suffix)) = recipient {
            content
                .child(app_text(format!("{intent} →")))
                .child(Self::address(id, address, cx).text_color(cx.theme().foreground))
                .child(app_text(suffix.to_owned()).text_xs())
        } else {
            content.child(app_text(summary.to_owned()).whitespace_normal())
        }
    }
}
