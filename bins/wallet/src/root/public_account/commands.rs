use std::sync::Arc;

use gpui::{App, Context, Entity, IntoElement, ParentElement, Styled, Window, div, px, rgb};
use gpui_component::{
    ActiveTheme, Disableable, Icon, Sizable,
    button::ButtonVariants,
    menu::{PopupMenu, PopupMenuItem},
};
use ui::controls::{app_button_base, app_button_label, app_text};
use wallet_ops::vault::{PublicAccountMetadata, PublicAccountSource, PublicAccountStatus};

use super::{public_account_display_label, public_address_qr_payload};
use crate::root::WalletRoot;

#[derive(Clone, Copy)]
pub(super) enum AccountCommand {
    Copy,
    Qr,
    Connect,
    Sessions(usize),
    Stealth,
    Rename,
    Activate,
    Deactivate,
    Delete,
}

impl AccountCommand {
    pub(super) fn label(self) -> String {
        match self {
            Self::Copy => "Copy address".into(),
            Self::Qr => "Show QR code".into(),
            Self::Connect => "Connect dapp".into(),
            Self::Sessions(count) => format!("WalletConnect sessions · {count}"),
            Self::Stealth => "View stealth account".into(),
            Self::Rename => "Rename".into(),
            Self::Activate => "Activate".into(),
            Self::Deactivate => "Deactivate".into(),
            Self::Delete => "Delete".into(),
        }
    }

    const fn id(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Qr => "qr",
            Self::Connect | Self::Sessions(_) => "walletconnect",
            Self::Stealth => "stealth",
            Self::Rename => "rename",
            Self::Activate => "activate",
            Self::Deactivate => "deactivate",
            Self::Delete => "delete",
        }
    }

    const fn disabled(self, account: &PublicAccountMetadata) -> bool {
        matches!(self, Self::Connect) && matches!(account.status, PublicAccountStatus::Inactive)
    }
}

impl WalletRoot {
    pub(super) fn public_account_commands(
        &self,
        account: &PublicAccountMetadata,
    ) -> Vec<AccountCommand> {
        let sessions = self.walletconnect_account_session_count(&account.public_account_uuid);
        let mut commands = vec![
            AccountCommand::Copy,
            AccountCommand::Qr,
            if sessions == 0 {
                AccountCommand::Connect
            } else {
                AccountCommand::Sessions(sessions)
            },
        ];
        if matches!(account.source, PublicAccountSource::ExecutorDerived(_)) {
            commands.push(AccountCommand::Stealth);
        }
        commands.push(AccountCommand::Rename);
        commands.push(if account.source == PublicAccountSource::Imported {
            AccountCommand::Delete
        } else if account.status == PublicAccountStatus::Inactive {
            AccountCommand::Activate
        } else {
            AccountCommand::Deactivate
        });
        commands
    }

    pub(super) fn run_public_account_command(
        &mut self,
        uuid: &str,
        command: AccountCommand,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self
            .public_account_for_uuid(Some(uuid))
            .cloned()
            .filter(|_| self.view_session.is_some())
        else {
            return;
        };
        if command.disabled(&account) {
            return;
        }
        match command {
            AccountCommand::Copy => ui::clipboard::copy_to_clipboard_with_toast(
                public_address_qr_payload(account.address),
                window,
                cx,
            ),
            AccountCommand::Qr => self.open_public_address_qr_dialog(
                uuid,
                public_account_display_label(&account),
                account.address,
                window,
                cx,
            ),
            AccountCommand::Connect => {
                self.open_walletconnect_connection_dialog(Arc::from(uuid), window, cx);
            }
            AccountCommand::Sessions(_) => {
                self.open_walletconnect_account_sessions_dialog(Arc::from(uuid), window, cx);
            }
            AccountCommand::Rename => {
                self.open_public_account_edit_dialog(Arc::from(uuid), window, cx);
            }
            AccountCommand::Activate => self.activate_public_account(uuid, window, cx),
            AccountCommand::Deactivate => self.deactivate_public_account(uuid, window, cx),
            AccountCommand::Delete => self.delete_public_account(uuid, window, cx),
            AccountCommand::Stealth => {
                if let PublicAccountSource::ExecutorDerived(source) = account.source {
                    let session = match self.chain_states.get(&source.chain_id()) {
                        Some(
                            crate::root::ChainUtxoState::Ready { session, .. }
                            | crate::root::ChainUtxoState::Syncing { session, .. },
                        ) => Some(session.clone()),
                        _ => None,
                    };
                    if let Some(session) = session {
                        let target = crate::root::stealth_accounts::StealthAccountTarget::new(
                            &session,
                            source.operation(),
                        );
                        self.open_stealth_account(&target, window, cx);
                    }
                }
            }
        }
    }

    pub(super) fn public_account_menu(
        &self,
        root: &Entity<Self>,
        account: &PublicAccountMetadata,
        mut menu: PopupMenu,
    ) -> PopupMenu {
        menu = menu.action_context(self.public_form.list_focus.clone());
        for command in self.public_account_commands(account) {
            let label = command.label();
            let item = if matches!(command, AccountCommand::Delete) {
                menu = menu.separator();
                PopupMenuItem::element(move |_, cx| {
                    app_text(label.clone()).text_color(cx.theme().danger)
                })
            } else {
                PopupMenuItem::new(label)
            };
            let root = root.clone();
            let uuid = account.public_account_uuid.clone();
            menu = menu.item(item.disabled(command.disabled(account)).on_click(
                move |_, window, cx| {
                    root.update(cx, |root, cx| {
                        root.run_public_account_command(&uuid, command, window, cx);
                    });
                },
            ));
        }
        menu
    }

    /// Account-level commands for the selected row's action bar. Copy, QR and
    /// stealth navigation stay in the address line and the row menu.
    const fn action_bar_command(command: AccountCommand) -> bool {
        !matches!(
            command,
            AccountCommand::Copy | AccountCommand::Qr | AccountCommand::Stealth
        )
    }

    pub(super) fn render_public_account_action_bar(
        &self,
        root: &Entity<Self>,
        account: &PublicAccountMetadata,
        cx: &App,
    ) -> impl IntoElement {
        let mut bar = div().flex().flex_wrap().items_center().gap_1();
        for command in self
            .public_account_commands(account)
            .into_iter()
            .filter(|command| Self::action_bar_command(*command))
        {
            let action_root = root.clone();
            let uuid = account.public_account_uuid.clone();
            let id = gpui::SharedString::from(format!("public-{}-{uuid}", command.id()));
            let label = command.label();
            let danger = matches!(command, AccountCommand::Deactivate | AccountCommand::Delete);
            let icon_color = if danger {
                cx.theme().danger
            } else {
                rgb(ui::theme::TEXT_MUTED).into()
            };
            let icon = match command {
                AccountCommand::Connect | AccountCommand::Sessions(_) => {
                    // The shared WalletConnect mark, with its session dot, as in the toolbar.
                    crate::root::walletconnect::walletconnect_logo_with_presence(
                        px(16.0),
                        matches!(command, AccountCommand::Sessions(_)),
                    )
                }
                AccountCommand::Rename => Icon::new(crate::assets::RailgunActionIcon::Pencil)
                    .small()
                    .text_color(icon_color)
                    .into_any_element(),
                AccountCommand::Activate => Icon::empty()
                    .path(ui::icons::eye_icon_path())
                    .small()
                    .text_color(icon_color)
                    .into_any_element(),
                AccountCommand::Deactivate => Icon::empty()
                    .path(ui::icons::ban_icon_path())
                    .small()
                    .text_color(icon_color)
                    .into_any_element(),
                AccountCommand::Delete => Icon::new(crate::assets::RailgunActionIcon::Trash2)
                    .small()
                    .text_color(icon_color)
                    .into_any_element(),
                AccountCommand::Copy | AccountCommand::Qr | AccountCommand::Stealth => {
                    gpui::Empty.into_any_element()
                }
            };
            let mut button = app_button_base(id)
                .accessibility_label(label.clone())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(icon)
                        .child(app_button_label(label)),
                )
                .ghost()
                .xsmall()
                .disabled(command.disabled(account));
            if danger {
                button = button.text_color(cx.theme().danger);
            }
            bar = bar.child(button.on_click(move |_, window, cx| {
                cx.stop_propagation();
                action_root.update(cx, |root, cx| {
                    root.run_public_account_command(&uuid, command, window, cx);
                });
            }));
        }
        bar
    }
}
