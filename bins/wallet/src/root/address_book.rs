use std::sync::Arc;

use alloy::primitives::Address;
use gpui::{
    App, Context, ElementId, Entity, Focusable, FontWeight, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, SharedString, Styled, Window, div, px, rgb,
};
use gpui_component::{
    Icon, Sizable, WindowExt,
    alert::Alert,
    button::{Button, ButtonVariants},
    input::InputState,
    scroll::ScrollableElement,
};
use railgun_ui::short_address;
use ui::controls::{app_button, app_input, app_muted_text, app_strong_text};
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{
    parse_railgun_recipient,
    vault::{PrivateAddressBookEntry, PublicAddressBookEntry, VaultError},
};

use crate::assets::RailgunActionIcon;
use crate::root::ui_helpers::dialog_footer;

use super::utxo::short_hash;
use super::{
    ConfirmationDialogProps, WalletRoot, app_status_tag, confirmation_dialog, dialog_max_height,
    secondary_dialog_content_width, vault_error_kind,
};

const ADDRESS_BOOK_CONTENT_WIDTH: Pixels = px(980.0);
const ADDRESS_BOOK_DIALOG_WIDTH: Pixels = px(500.0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum AddressBookEntryKind {
    Private,
    Public,
}

impl AddressBookEntryKind {
    const fn section_label(self) -> &'static str {
        match self {
            Self::Private => "Private recipients",
            Self::Public => "Public EVM recipients",
        }
    }

    const fn type_validation_message(self) -> &'static str {
        match self {
            Self::Private => "Private address-book entries must use a 0zk recipient",
            Self::Public => "Public address-book entries must use a 0x EVM address",
        }
    }

    const fn invalid_address_message(self) -> &'static str {
        match self {
            Self::Private => "Enter a valid private 0zk recipient",
            Self::Public => "Enter a valid public EVM address",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum AddressBookDetectedType {
    Private,
    Public,
    Unknown,
}

impl AddressBookDetectedType {
    const fn label(self) -> &'static str {
        match self {
            Self::Private => "Private recipient",
            Self::Public => "Public EVM recipient",
            Self::Unknown => "Unknown type",
        }
    }
}

impl From<AddressBookEntryKind> for AddressBookDetectedType {
    fn from(value: AddressBookEntryKind) -> Self {
        match value {
            AddressBookEntryKind::Private => Self::Private,
            AddressBookEntryKind::Public => Self::Public,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AddressBookEntryTarget {
    pub(super) kind: AddressBookEntryKind,
    pub(super) entry_uuid: Arc<str>,
}

impl AddressBookEntryTarget {
    fn new(kind: AddressBookEntryKind, entry_uuid: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            entry_uuid: entry_uuid.into(),
        }
    }
}

pub(super) struct AddressBookState {
    pub(super) search_input: Entity<InputState>,
    pub(super) add_label_input: Entity<InputState>,
    pub(super) add_address_input: Entity<InputState>,
    pub(super) edit_label_input: Entity<InputState>,
    pub(super) edit_address_input: Entity<InputState>,
    pub(super) search_query: Arc<str>,
    pub(super) editing_entry: Option<AddressBookEntryTarget>,
    pub(super) error: Option<Arc<str>>,
}

impl AddressBookState {
    pub(super) fn clear_dialog_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, WalletRoot>,
    ) {
        self.editing_entry = None;
        self.error = None;
        self.add_label_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.add_address_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.edit_label_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.edit_address_input
            .update(cx, |input, cx| input.set_value("", window, cx));
    }
}

#[derive(Clone, Copy)]
enum AddressBookDialogMode {
    Add,
    Edit(AddressBookEntryKind),
}

impl AddressBookDialogMode {
    const fn title(self) -> &'static str {
        match self {
            Self::Add => "Add address-book entry",
            Self::Edit(AddressBookEntryKind::Private) => "Edit private recipient",
            Self::Edit(AddressBookEntryKind::Public) => "Edit public recipient",
        }
    }

    const fn action_label(self) -> &'static str {
        match self {
            Self::Add => "Add",
            Self::Edit(_) => "Save",
        }
    }
}

pub(in crate::root) fn address_book_detected_type(address: &str) -> AddressBookDetectedType {
    let address = address.trim().to_ascii_lowercase();
    if address.starts_with("0zk") {
        AddressBookDetectedType::Private
    } else if address.starts_with("0x") {
        AddressBookDetectedType::Public
    } else {
        AddressBookDetectedType::Unknown
    }
}

pub(in crate::root) fn address_book_entry_matches_search(
    label: &str,
    address: &str,
    query: &str,
) -> bool {
    let query = query.trim().to_ascii_lowercase();
    query.is_empty()
        || label.to_ascii_lowercase().contains(&query)
        || address.to_ascii_lowercase().contains(&query)
}

pub(in crate::root) fn address_book_entry_validation_message(
    expected_kind: Option<AddressBookEntryKind>,
    label: &str,
    address: &str,
) -> Option<&'static str> {
    if label.trim().is_empty() {
        return Some("Enter a label");
    }
    let address = address.trim();
    if address.is_empty() {
        return Some("Enter an address");
    }

    match expected_kind {
        Some(kind) => validate_address_for_kind(kind, address),
        None => match address_book_detected_type(address) {
            AddressBookDetectedType::Private => {
                validate_address_for_kind(AddressBookEntryKind::Private, address)
            }
            AddressBookDetectedType::Public => {
                validate_address_for_kind(AddressBookEntryKind::Public, address)
            }
            AddressBookDetectedType::Unknown => {
                Some("Enter a 0zk private recipient or 0x public EVM address")
            }
        },
    }
}

fn validate_address_for_kind(kind: AddressBookEntryKind, address: &str) -> Option<&'static str> {
    let detected = address_book_detected_type(address);
    if detected != AddressBookDetectedType::from(kind) {
        return Some(kind.type_validation_message());
    }
    let valid = match kind {
        AddressBookEntryKind::Private => parse_railgun_recipient(address).is_ok(),
        AddressBookEntryKind::Public => address.parse::<Address>().is_ok(),
    };
    (!valid).then_some(kind.invalid_address_message())
}

fn filtered_private_address_book_entries<'a>(
    entries: &'a [PrivateAddressBookEntry],
    query: &str,
) -> Vec<&'a PrivateAddressBookEntry> {
    entries
        .iter()
        .filter(|entry| address_book_entry_matches_search(&entry.label, &entry.address, query))
        .collect()
}

fn filtered_public_address_book_entries<'a>(
    entries: &'a [PublicAddressBookEntry],
    query: &str,
) -> Vec<&'a PublicAddressBookEntry> {
    entries
        .iter()
        .filter(|entry| {
            address_book_entry_matches_search(&entry.label, &entry.address.to_checksum(None), query)
        })
        .collect()
}

impl WalletRoot {
    pub(super) fn render_address_book_view(&self, root: &Entity<Self>) -> gpui::AnyElement {
        let add_root = root.clone();
        let private_entries = filtered_private_address_book_entries(
            &self.private_address_book,
            &self.address_book.search_query,
        );
        let public_entries = filtered_public_address_book_entries(
            &self.public_address_book,
            &self.address_book.search_query,
        );

        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_y_scrollbar()
            .bg(rgb(theme::SURFACE_ELEVATED))
            .child(
                div()
                    .w(ADDRESS_BOOK_CONTENT_WIDTH)
                    .max_w_full()
                    .mx_auto()
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_3()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                Icon::new(RailgunActionIcon::BookUser)
                                                    .size_5()
                                                    .text_color(rgb(theme::PRIMARY)),
                                            )
                                            .child(
                                                app_strong_text("Address book")
                                                    .text_size(px(20.0))
                                                    .font_weight(FontWeight::SEMIBOLD),
                                            ),
                                    ),
                            )
                            .child(
                                app_button("wallet-address-book-add-entry", "Add entry")
                                    .primary()
                                    .small()
                                    .on_click(move |_event, window, cx| {
                                        add_root.update(cx, |root, cx| {
                                            root.open_address_book_add_dialog(window, cx);
                                        });
                                    }),
                            ),
                    )
                    .children(self.address_book.error.as_ref().map(|message| {
                        Alert::error("wallet-address-book-error", message.to_string()).small()
                    }))
                    .child(app_input(&self.address_book.search_input))
                    .child(self.render_private_address_book_section(root, &private_entries))
                    .child(self.render_public_address_book_section(root, &public_entries)),
            )
            .into_any_element()
    }

    fn render_private_address_book_section(
        &self,
        root: &Entity<Self>,
        entries: &[&PrivateAddressBookEntry],
    ) -> gpui::Div {
        let mut section = address_book_section_shell(
            AddressBookEntryKind::Private,
            entries.len(),
            self.private_address_book.len(),
            &self.address_book.search_query,
        );
        if entries.is_empty() {
            return section.child(address_book_empty_state(&self.address_book.search_query));
        }
        for entry in entries {
            section = section.child(Self::render_private_address_book_row(root, entry));
        }
        section
    }

    fn render_public_address_book_section(
        &self,
        root: &Entity<Self>,
        entries: &[&PublicAddressBookEntry],
    ) -> gpui::Div {
        let mut section = address_book_section_shell(
            AddressBookEntryKind::Public,
            entries.len(),
            self.public_address_book.len(),
            &self.address_book.search_query,
        );
        if entries.is_empty() {
            return section.child(address_book_empty_state(&self.address_book.search_query));
        }
        for entry in entries {
            section = section.child(Self::render_public_address_book_row(root, entry));
        }
        section
    }

    fn render_private_address_book_row(
        root: &Entity<Self>,
        entry: &PrivateAddressBookEntry,
    ) -> gpui::AnyElement {
        let display_address = short_hash(&entry.address);
        Self::render_address_book_row(
            root,
            AddressBookEntryKind::Private,
            &entry.entry_uuid,
            &entry.label,
            &display_address,
        )
    }

    fn render_public_address_book_row(
        root: &Entity<Self>,
        entry: &PublicAddressBookEntry,
    ) -> gpui::AnyElement {
        let display_address = short_address(&entry.address);
        Self::render_address_book_row(
            root,
            AddressBookEntryKind::Public,
            &entry.entry_uuid,
            &entry.label,
            &display_address,
        )
    }

    fn render_address_book_row(
        root: &Entity<Self>,
        kind: AddressBookEntryKind,
        entry_uuid: &str,
        label: &str,
        display_address: &str,
    ) -> gpui::AnyElement {
        let row_group = SharedString::from(format!("address-book-row-group-{entry_uuid}"));

        div()
            .id(SharedString::from(format!(
                "wallet-address-book-row-{entry_uuid}"
            )))
            .group(row_group)
            .w_full()
            .flex()
            .items_center()
            .gap_3()
            .px(px(12.0))
            .py(px(10.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::SURFACE))
            .hover(|row| row.border_color(rgb(theme::PRIMARY)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(app_strong_text(label.to_owned()))
                    .child(
                        app_muted_text(display_address.to_owned())
                            .font_family(APP_MONO_FONT_FAMILY)
                            .text_size(APP_TEXT_SIZE),
                    ),
            )
            .child(Self::render_address_book_row_actions(
                root, kind, entry_uuid, label,
            ))
            .into_any_element()
    }

    fn render_address_book_row_actions(
        root: &Entity<Self>,
        kind: AddressBookEntryKind,
        entry_uuid: &str,
        label: &str,
    ) -> gpui::Div {
        let edit_root = root.clone();
        let delete_root = root.clone();
        let edit_target = AddressBookEntryTarget::new(kind, Arc::<str>::from(entry_uuid));
        let delete_target = edit_target.clone();
        let label = label.to_owned();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                address_book_icon_button(
                    SharedString::from(format!("wallet-address-book-edit-{entry_uuid}")),
                    Icon::new(RailgunActionIcon::Pencil),
                    "Edit entry",
                )
                .on_click(move |_event, window, cx| {
                    edit_root.update(cx, |root, cx| {
                        root.open_address_book_edit_dialog(&edit_target, window, cx);
                    });
                }),
            )
            .child(
                address_book_icon_button(
                    SharedString::from(format!("wallet-address-book-delete-{entry_uuid}")),
                    Icon::new(RailgunActionIcon::Trash2),
                    "Delete entry",
                )
                .danger()
                .on_click(move |_event, window, cx| {
                    let root = delete_root.clone();
                    let target = delete_target.clone();
                    let label = label.clone();
                    let dialog_width =
                        (window.viewport_size().width * 0.92).min(ADDRESS_BOOK_DIALOG_WIDTH);
                    let dialog_max_height = dialog_max_height(window);
                    window.open_alert_dialog(cx, move |dialog, _window, _cx| {
                        let confirm_root = root.clone();
                        let target = target.clone();
                        confirmation_dialog(
                            dialog,
                            ConfirmationDialogProps::danger(
                                "Delete address-book entry?",
                                "This removes the saved recipient from your address book.",
                                None,
                                "Delete entry",
                            ),
                            dialog_width,
                            dialog_max_height,
                        )
                        .child(app_strong_text(label.clone()).whitespace_normal())
                        .on_ok(move |_event, window, cx| {
                            confirm_root.update(cx, |root, cx| {
                                root.delete_address_book_entry(&target, window, cx);
                            });
                            true
                        })
                    });
                }),
            )
    }

    fn open_address_book_add_dialog(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.close_all_dialogs(cx);
        self.address_book.clear_dialog_state(window, cx);
        let root = cx.entity();
        let add_label_input = self.address_book.add_label_input.clone();
        let add_address_input = self.address_book.add_address_input.clone();
        let focus_label_input = add_label_input.clone();
        let dialog_width = (window.viewport_size().width * 0.92).min(ADDRESS_BOOK_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            let save_root = root.clone();
            let error = content_root.read(cx).address_book.error.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(AddressBookDialogMode::Add.title()))
                .footer(dialog_footer(
                    AddressBookDialogMode::Add.action_label(),
                    true,
                ))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.address_book.clear_dialog_state(window, cx);
                    });
                })
                .on_ok(move |_event, window, cx| {
                    save_root.update(cx, |root, cx| {
                        root.add_address_book_entry_from_dialog(window, cx)
                    })
                })
                .child(render_address_book_dialog_content(
                    AddressBookDialogMode::Add,
                    content_width,
                    &add_label_input,
                    &add_address_input,
                    error.as_ref(),
                    cx,
                ))
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_label_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        });
    }

    fn open_address_book_edit_dialog(
        &mut self,
        target: &AddressBookEntryTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((label, address)) = self.address_book_entry_values(target) else {
            self.address_book.error = Some(Arc::from("Address-book entry not found"));
            cx.notify();
            return;
        };
        window.close_all_dialogs(cx);
        self.address_book.clear_dialog_state(window, cx);
        self.address_book.editing_entry = Some(target.clone());
        self.address_book.edit_label_input.update(cx, |input, cx| {
            input.set_value(label, window, cx);
        });
        self.address_book
            .edit_address_input
            .update(cx, |input, cx| {
                input.set_value(address, window, cx);
            });
        let root = cx.entity();
        let edit_label_input = self.address_book.edit_label_input.clone();
        let edit_address_input = self.address_book.edit_address_input.clone();
        let focus_label_input = edit_label_input.clone();
        let dialog_width = (window.viewport_size().width * 0.92).min(ADDRESS_BOOK_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let mode = AddressBookDialogMode::Edit(target.kind);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            let save_root = root.clone();
            let error = content_root.read(cx).address_book.error.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(mode.title()))
                .footer(dialog_footer(mode.action_label(), true))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.address_book.clear_dialog_state(window, cx);
                    });
                })
                .on_ok(move |_event, window, cx| {
                    save_root.update(cx, |root, cx| {
                        root.update_address_book_entry_from_dialog(window, cx)
                    })
                })
                .child(render_address_book_dialog_content(
                    mode,
                    content_width,
                    &edit_label_input,
                    &edit_address_input,
                    error.as_ref(),
                    cx,
                ))
        });
        cx.defer_in(window, move |_root, window, cx| {
            focus_label_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        });
    }

    fn address_book_entry_values(
        &self,
        target: &AddressBookEntryTarget,
    ) -> Option<(String, String)> {
        match target.kind {
            AddressBookEntryKind::Private => self
                .private_address_book
                .iter()
                .find(|entry| entry.entry_uuid == target.entry_uuid.as_ref())
                .map(|entry| (entry.label.clone(), entry.address.clone())),
            AddressBookEntryKind::Public => self
                .public_address_book
                .iter()
                .find(|entry| entry.entry_uuid == target.entry_uuid.as_ref())
                .map(|entry| (entry.label.clone(), entry.address.to_checksum(None))),
        }
    }

    fn add_address_book_entry_from_dialog(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let label = self
            .address_book
            .add_label_input
            .read(cx)
            .value()
            .to_string();
        let address = self
            .address_book
            .add_address_input
            .read(cx)
            .value()
            .to_string();
        if let Some(message) = address_book_entry_validation_message(None, &label, &address) {
            self.address_book.error = Some(Arc::from(message));
            cx.notify();
            return false;
        }
        let Some(store) = self.vault_store.as_ref() else {
            self.address_book.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return false;
        };
        let Some(view_session) = self.view_session.as_ref() else {
            self.address_book.error = Some(Arc::from("Unlock the wallet vault first"));
            cx.notify();
            return false;
        };
        let result = match address_book_detected_type(&address) {
            AddressBookDetectedType::Private => store
                .add_private_address_book_entry_for_session(view_session.as_ref(), &label, &address)
                .map(|_| ()),
            AddressBookDetectedType::Public => store
                .add_public_address_book_entry_for_session(view_session.as_ref(), &label, &address)
                .map(|_| ()),
            AddressBookDetectedType::Unknown => unreachable!("validated add entry type"),
        };
        self.handle_address_book_mutation_result(result, "add address book entry", cx)
    }

    fn update_address_book_entry_from_dialog(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(target) = self.address_book.editing_entry.clone() else {
            self.address_book.error = Some(Arc::from("Address-book entry not found"));
            cx.notify();
            return false;
        };
        let label = self
            .address_book
            .edit_label_input
            .read(cx)
            .value()
            .to_string();
        let address = self
            .address_book
            .edit_address_input
            .read(cx)
            .value()
            .to_string();
        if let Some(message) =
            address_book_entry_validation_message(Some(target.kind), &label, &address)
        {
            self.address_book.error = Some(Arc::from(message));
            cx.notify();
            return false;
        }
        let Some(store) = self.vault_store.as_ref() else {
            self.address_book.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return false;
        };
        let Some(view_session) = self.view_session.as_ref() else {
            self.address_book.error = Some(Arc::from("Unlock the wallet vault first"));
            cx.notify();
            return false;
        };
        let result = match target.kind {
            AddressBookEntryKind::Private => store
                .update_private_address_book_entry_for_session(
                    view_session.as_ref(),
                    target.entry_uuid.as_ref(),
                    &label,
                    &address,
                )
                .map(|_| ()),
            AddressBookEntryKind::Public => store
                .update_public_address_book_entry_for_session(
                    view_session.as_ref(),
                    target.entry_uuid.as_ref(),
                    &label,
                    &address,
                )
                .map(|_| ()),
        };
        self.handle_address_book_mutation_result(result, "update address book entry", cx)
    }

    fn delete_address_book_entry(
        &mut self,
        target: &AddressBookEntryTarget,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.as_ref() else {
            self.address_book.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(view_session) = self.view_session.as_ref() else {
            self.address_book.error = Some(Arc::from("Unlock the wallet vault first"));
            cx.notify();
            return;
        };
        let result = match target.kind {
            AddressBookEntryKind::Private => store
                .delete_private_address_book_entry_for_session(
                    view_session.as_ref(),
                    target.entry_uuid.as_ref(),
                )
                .map(|_| ()),
            AddressBookEntryKind::Public => store
                .delete_public_address_book_entry_for_session(
                    view_session.as_ref(),
                    target.entry_uuid.as_ref(),
                )
                .map(|_| ()),
        };
        let _saved =
            self.handle_address_book_mutation_result(result, "delete address book entry", cx);
    }

    fn handle_address_book_mutation_result(
        &mut self,
        result: Result<(), VaultError>,
        operation: &'static str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match result {
            Ok(()) => {
                self.address_book.error = None;
                self.address_book.editing_entry = None;
                self.reload_address_books(cx);
                true
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    operation,
                    "address book operation failed"
                );
                self.address_book.error = Some(Arc::from(address_book_error_message(&error)));
                cx.notify();
                false
            }
        }
    }
}

fn address_book_section_shell(
    kind: AddressBookEntryKind,
    visible_count: usize,
    total_count: usize,
    query: &str,
) -> gpui::Div {
    let count_label = if query.trim().is_empty() {
        total_count.to_string()
    } else {
        format!("{visible_count}/{total_count}")
    };
    div().w_full().flex().flex_col().gap_2().child(
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                app_strong_text(kind.section_label())
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(app_status_tag(count_label, theme::PRIMARY)),
    )
}

fn address_book_empty_state(query: &str) -> gpui::Div {
    let message = if query.trim().is_empty() {
        "No saved recipients yet."
    } else {
        "No saved recipients match this search."
    };
    div()
        .w_full()
        .px(px(12.0))
        .py(px(14.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE))
        .child(app_muted_text(message))
}

fn render_address_book_dialog_content(
    mode: AddressBookDialogMode,
    content_width: Pixels,
    label_input: &Entity<InputState>,
    address_input: &Entity<InputState>,
    error: Option<&Arc<str>>,
    cx: &App,
) -> gpui::Div {
    let expected_kind = match mode {
        AddressBookDialogMode::Add => None,
        AddressBookDialogMode::Edit(kind) => Some(kind),
    };
    let label = label_input.read(cx).value().to_string();
    let address = address_input.read(cx).value().to_string();
    let validation = address_book_entry_validation_message(expected_kind, &label, &address);
    div()
        .w(content_width)
        .flex()
        .flex_col()
        .gap_3()
        .child(app_input(label_input))
        .child(app_input(address_input))
        .child(render_address_book_type_status(
            expected_kind,
            &address,
            validation,
        ))
        .children(error.as_ref().map(|message| {
            Alert::error("wallet-address-book-dialog-error", message.to_string()).small()
        }))
}

fn render_address_book_type_status(
    expected_kind: Option<AddressBookEntryKind>,
    address: &str,
    validation: Option<&'static str>,
) -> gpui::Div {
    let detected = expected_kind.map_or_else(|| address_book_detected_type(address), Into::into);
    let status_color = validation.map_or(theme::SUCCESS, |_| theme::WARNING);
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(app_muted_text("Type"))
                .child(app_status_tag(detected.label(), status_color)),
        )
        .children(validation.map(|status| app_muted_text(status).text_color(rgb(status_color))))
}

fn address_book_icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> Button {
    let tooltip: SharedString = tooltip.into();
    Button::new(id)
        .icon(icon)
        .ghost()
        .small()
        .accessibility_label(tooltip.clone())
        .tooltip(tooltip)
        .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
            cx.stop_propagation();
        })
}

fn address_book_error_message(error: &VaultError) -> String {
    match error {
        VaultError::InvalidAddressBookLabel => "Enter a label".to_owned(),
        VaultError::InvalidPrivateAddressBookAddress => AddressBookEntryKind::Private
            .invalid_address_message()
            .to_owned(),
        VaultError::InvalidPublicAddressBookAddress => AddressBookEntryKind::Public
            .invalid_address_message()
            .to_owned(),
        VaultError::DuplicatePrivateAddressBookAddress => {
            "This private recipient is already saved or belongs to an active Private wallet"
                .to_owned()
        }
        VaultError::DuplicatePublicAddressBookAddress => {
            "This public recipient is already saved or belongs to an active Public account"
                .to_owned()
        }
        VaultError::PrivateAddressBookEntryNotFound
        | VaultError::PublicAddressBookEntryNotFound => "Address-book entry not found".to_owned(),
        _ => error.to_string(),
    }
}
