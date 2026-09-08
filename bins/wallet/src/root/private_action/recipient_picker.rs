use crate::root::ui_helpers::dialog_footer;

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum RecipientOptionSource {
    PrivateWallet,
    PrivateAddressBook,
    PublicAccount,
    PublicAddressBook,
}

impl RecipientOptionSource {
    const fn icon(self) -> RailgunActionIcon {
        match self {
            Self::PrivateWallet | Self::PublicAccount => RailgunActionIcon::Wallet,
            Self::PrivateAddressBook | Self::PublicAddressBook => RailgunActionIcon::BookUser,
        }
    }
}

const RECIPIENT_PICKER_INPUT_HEIGHT: Pixels = px(32.0);
const RECIPIENT_SUGGESTIONS_MAX_HEIGHT: Pixels = px(220.0);
const RECIPIENT_OPTION_ADDRESS_TEXT_SIZE: Pixels = px(11.0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct RecipientOption {
    pub(in crate::root) label: Arc<str>,
    pub(in crate::root) address: Arc<str>,
    pub(in crate::root) source: RecipientOptionSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct PrivateWalletRecipientSource {
    pub(in crate::root) label: Arc<str>,
    pub(in crate::root) address: Arc<str>,
    pub(in crate::root) active: bool,
}

pub(in crate::root) fn private_send_recipient_options(
    wallets: &[PrivateWalletRecipientSource],
    address_book: &[PrivateAddressBookEntry],
) -> Vec<RecipientOption> {
    wallets
        .iter()
        .filter(|wallet| wallet.active)
        .map(|wallet| RecipientOption {
            label: Arc::clone(&wallet.label),
            address: Arc::clone(&wallet.address),
            source: RecipientOptionSource::PrivateWallet,
        })
        .chain(address_book.iter().map(|entry| RecipientOption {
            label: Arc::from(entry.label.as_str()),
            address: Arc::from(entry.address.as_str()),
            source: RecipientOptionSource::PrivateAddressBook,
        }))
        .collect()
}

pub(in crate::root) fn hardware_wallet_recipient_source_from_metadata(
    metadata: &WalletMetadataBundle,
) -> Option<PrivateWalletRecipientSource> {
    let address = metadata
        .hardware_account
        .as_ref()
        .and_then(|account| account.receive_address.as_ref())?;
    Some(PrivateWalletRecipientSource {
        label: Arc::from(metadata.label.as_str()),
        address: Arc::from(address.as_str()),
        active: metadata.status == WalletStatus::Active,
    })
}

pub(in crate::root) fn private_unshield_recipient_options(
    accounts: &[PublicAccountMetadata],
    address_book: &[PublicAddressBookEntry],
) -> Vec<RecipientOption> {
    accounts
        .iter()
        .filter(|account| account.status == PublicAccountStatus::Active)
        .map(|account| RecipientOption {
            label: Arc::from(
                public_account_display_label(account)
                    .unwrap_or_else(|| short_address(&account.address)),
            ),
            address: Arc::from(account.address.to_checksum(None)),
            source: RecipientOptionSource::PublicAccount,
        })
        .chain(address_book.iter().map(|entry| RecipientOption {
            label: Arc::from(entry.label.as_str()),
            address: Arc::from(entry.address.to_checksum(None)),
            source: RecipientOptionSource::PublicAddressBook,
        }))
        .collect()
}

pub(in crate::root) fn recipient_option_matches_search(
    option: &RecipientOption,
    query: &str,
) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    option.label.to_ascii_lowercase().contains(&query)
        || option.address.to_ascii_lowercase().contains(&query)
}

pub(in crate::root) fn filtered_recipient_options(
    options: &[RecipientOption],
    query: &str,
) -> Vec<RecipientOption> {
    options
        .iter()
        .filter(|option| recipient_option_matches_search(option, query))
        .cloned()
        .collect()
}

pub(in crate::root) fn recipient_suggestion_filter_query(
    kind: DeliveryFormKind,
    recipient: &str,
) -> String {
    if recipient_query_is_valid(kind, recipient) {
        String::new()
    } else {
        recipient.to_owned()
    }
}

pub(in crate::root) fn recipient_query_is_valid(kind: DeliveryFormKind, recipient: &str) -> bool {
    let recipient = recipient.trim();
    !recipient.is_empty()
        && match kind {
            DeliveryFormKind::Send => parse_railgun_recipient(recipient).is_ok(),
            DeliveryFormKind::Unshield => parse_address(recipient).is_some(),
        }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum RecipientSuggestionDirection {
    Previous,
    Next,
}

pub(in crate::root) const fn recipient_suggestion_index_after_move(
    current: Option<usize>,
    len: usize,
    direction: RecipientSuggestionDirection,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, direction) {
        (Some(index), RecipientSuggestionDirection::Next) => (index + 1) % len,
        (Some(0) | None, RecipientSuggestionDirection::Previous) => len - 1,
        (Some(index), RecipientSuggestionDirection::Previous) => index.saturating_sub(1),
        (None, RecipientSuggestionDirection::Next) => 0,
    })
}

fn first_recipient_suggestion_index(len: usize) -> Option<usize> {
    (len > 0).then_some(0)
}

pub(in crate::root) fn selected_recipient_address(option: &RecipientOption) -> &str {
    &option.address
}

pub(in crate::root) fn recipient_option_display_address(option: &RecipientOption) -> String {
    match option.source {
        RecipientOptionSource::PrivateWallet | RecipientOptionSource::PrivateAddressBook => {
            short_hash(&option.address)
        }
        RecipientOptionSource::PublicAccount | RecipientOptionSource::PublicAddressBook => {
            option.address.to_string()
        }
    }
}

pub(in crate::root) fn normalized_address_book_save_label(label: &str) -> Option<String> {
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_owned())
}

pub(in crate::root) fn can_save_private_recipient(
    recipient: &str,
    options: &[RecipientOption],
) -> bool {
    let recipient = recipient.trim();
    !recipient.is_empty()
        && parse_railgun_recipient(recipient).is_ok()
        && !private_recipient_matches_existing_option(recipient, options)
}

pub(in crate::root) fn can_save_public_recipient(
    recipient: &str,
    options: &[RecipientOption],
) -> bool {
    let Some(recipient) = parse_address(recipient.trim()) else {
        return false;
    };
    !options.iter().any(|option| {
        parse_address(option.address.trim()).is_some_and(|address| address == recipient)
    })
}

fn private_recipient_matches_existing_option(recipient: &str, options: &[RecipientOption]) -> bool {
    let Ok(recipient_data) = parse_railgun_recipient(recipient) else {
        return false;
    };
    options.iter().any(|option| {
        parse_railgun_recipient(&option.address).is_ok_and(|option_data| {
            option_data.master_public_key == recipient_data.master_public_key
                && option_data.viewing_public_key == recipient_data.viewing_public_key
        })
    })
}

#[derive(Default)]
struct RecipientPickerLayoutState {
    bounds: Option<Bounds<Pixels>>,
}

#[derive(IntoElement)]
struct RecipientPicker {
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    input: Entity<InputState>,
    current_value: Arc<str>,
    suggestions_open: bool,
    selected_index: Option<usize>,
    suggestions_scroll: ScrollHandle,
    options: Vec<RecipientOption>,
    generating: bool,
}

pub(in crate::root) fn render_recipient_picker(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    input: &Entity<InputState>,
    current_value: &str,
    suggestions_open: bool,
    selected_index: Option<usize>,
    suggestions_scroll: &ScrollHandle,
    options: &[RecipientOption],
    generating: bool,
) -> impl IntoElement {
    RecipientPicker {
        root,
        key,
        kind,
        input: input.clone(),
        current_value: Arc::from(current_value),
        suggestions_open,
        selected_index,
        suggestions_scroll: suggestions_scroll.clone(),
        options: options.to_vec(),
        generating,
    }
}

impl RenderOnce for RecipientPicker {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            root,
            key,
            kind,
            input,
            current_value,
            suggestions_open,
            selected_index,
            suggestions_scroll,
            options,
            generating,
        } = self;
        let layout_state = window.use_keyed_state(
            delivery_element_id(key, kind, "recipient-picker-layout"),
            cx,
            |_, _| RecipientPickerLayoutState::default(),
        );
        let picker_bounds = layout_state.read(cx).bounds;
        let keyboard_root = root.clone();
        let escape_root = root.clone();
        let outside_root = root.clone();
        let toggle_root = root.clone();
        let dropdown_root = root.clone();
        let save_root = root;
        let focus_input = input.clone();
        let save_recipient = current_value.trim().to_owned();
        let save_visible = match kind {
            DeliveryFormKind::Send => can_save_private_recipient(&current_value, &options),
            DeliveryFormKind::Unshield => can_save_public_recipient(&current_value, &options),
        };
        let filter_query = recipient_suggestion_filter_query(kind, &current_value);
        let filtered = filtered_recipient_options(&options, &filter_query);
        let selected_index =
            selected_index.and_then(|index| (index < filtered.len()).then_some(index));
        let show_suggestions =
            suggestions_open && !save_visible && !generating && !options.is_empty();
        let suggestions_bounds = if show_suggestions {
            picker_bounds
        } else {
            None
        };

        div()
            .relative()
            .w_full()
            .h(RECIPIENT_PICKER_INPUT_HEIGHT)
            .on_mouse_down_out(move |_event, _window, cx| {
                outside_root.update(cx, |root, cx| {
                    root.dismiss_recipient_suggestions(kind, key, cx);
                });
            })
            .on_action(move |_: &InputEscape, _window, cx| {
                if suggestions_open {
                    escape_root.update(cx, |root, cx| {
                        root.dismiss_recipient_suggestions(kind, key, cx);
                    });
                } else {
                    cx.propagate();
                }
            })
            .on_key_down(move |event: &KeyDownEvent, _window, cx| {
                let direction = match event.keystroke.key.as_str() {
                    "down" => Some(RecipientSuggestionDirection::Next),
                    "up" => Some(RecipientSuggestionDirection::Previous),
                    "escape" if suggestions_open => {
                        keyboard_root.update(cx, |root, cx| {
                            root.dismiss_recipient_suggestions(kind, key, cx);
                        });
                        cx.stop_propagation();
                        return;
                    }
                    "escape" => return,
                    _ => None,
                };
                if let Some(direction) = direction {
                    keyboard_root.update(cx, |root, cx| {
                        root.move_recipient_suggestion_selection(kind, key, direction, cx);
                    });
                    cx.stop_propagation();
                }
            })
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(RECIPIENT_PICKER_INPUT_HEIGHT)
                    .child(
                        private_action_input(&input)
                            .w_full()
                            .pr(px(44.0))
                            .disabled(generating),
                    )
                    .child(
                        canvas(
                            {
                                let layout_state = layout_state;
                                move |bounds, _window, cx| {
                                    layout_state.update(cx, |state, _cx| {
                                        state.bounds = Some(bounds);
                                    });
                                }
                            },
                            |_, (), _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children((!save_visible).then(|| {
                        div().absolute().right(px(6.0)).top(px(5.0)).child(
                            app_button_base(delivery_element_id(
                                key,
                                kind,
                                "recipient-suggestions-trigger",
                            ))
                            .icon(Icon::new(RailgunActionIcon::BookUser))
                            .outline()
                            .small()
                            .compact()
                            .accessibility_label("Select recipient")
                            .tooltip("Select recipient")
                            .disabled(generating || options.is_empty())
                            .on_click(move |_event, window, cx| {
                                cx.stop_propagation();
                                focus_input.read(cx).focus_handle(cx).focus(window, cx);
                                toggle_root.update(cx, |root, cx| {
                                    root.toggle_recipient_suggestions(kind, key, cx);
                                });
                            }),
                        )
                    }))
                    .children(save_visible.then(|| {
                        div().absolute().right(px(6.0)).top(px(5.0)).child(
                            app_button_base(delivery_element_id(key, kind, "save-recipient"))
                                .icon(Icon::new(RailgunActionIcon::Save))
                                .outline()
                                .small()
                                .compact()
                                .accessibility_label("Save recipient")
                                .tooltip("Save recipient")
                                .disabled(generating)
                                .on_click(move |_event, window, cx| {
                                    let recipient = save_recipient.clone();
                                    save_root.update(cx, |root, cx| {
                                        root.open_save_recipient_dialog(
                                            kind, key, recipient, window, cx,
                                        );
                                    });
                                }),
                        )
                    })),
            )
            .children(suggestions_bounds.map(|bounds| {
                deferred(render_recipient_suggestions_menu(
                    &dropdown_root,
                    key,
                    kind,
                    filtered,
                    selected_index,
                    &suggestions_scroll,
                    bounds,
                ))
                .with_priority(gpui_kit::base::POPUP_PRIORITY)
            }))
    }
}

fn render_recipient_suggestions_menu(
    root: &Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    options: Vec<RecipientOption>,
    selected_index: Option<usize>,
    scroll_handle: &ScrollHandle,
    picker_bounds: Bounds<Pixels>,
) -> gpui::AnyElement {
    let menu = render_recipient_suggestions_menu_content(
        root,
        key,
        kind,
        options,
        selected_index,
        scroll_handle,
    );
    anchored()
        .snap_to_window_with_margin(px(8.0))
        .child(div().w(picker_bounds.size.width).child(menu))
        .into_any_element()
}

fn render_recipient_suggestions_menu_content(
    root: &Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    options: Vec<RecipientOption>,
    selected_index: Option<usize>,
    scroll_handle: &ScrollHandle,
) -> gpui::Div {
    let menu = div()
        .w_full()
        .p(px(8.0))
        .flex()
        .flex_col()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::POPOVER_BG))
        .overflow_hidden()
        .occlude();
    if options.is_empty() {
        return menu.child(app_muted_text("No matching recipients"));
    }
    let mut list = div()
        .id(delivery_element_id(key, kind, "recipient-suggestions-list"))
        .max_h(RECIPIENT_SUGGESTIONS_MAX_HEIGHT)
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .flex()
        .flex_col()
        .gap_1();
    for (index, option) in options.into_iter().enumerate() {
        let select_root = root.clone();
        let address = selected_recipient_address(&option).to_owned();
        let selected = selected_index == Some(index);
        list = list.child(
            div()
                .id(SharedString::from(format!(
                    "{}-recipient-option-{index}",
                    delivery_element_id(key, kind, "recipient")
                )))
                .w_full()
                .p(px(8.0))
                .rounded_sm()
                .cursor_pointer()
                .when(selected, |this| this.bg(rgb(theme::SURFACE_HOVER)))
                .hover(|this| this.bg(rgb(theme::SURFACE_HOVER)))
                .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                    cx.stop_propagation();
                    let address = address.clone();
                    select_root.update(cx, |root, cx| {
                        root.select_recipient_suggestion(kind, key, &address, window, cx);
                    });
                })
                .child(recipient_option_row(&option)),
        );
    }
    menu.child(list).vertical_scrollbar(scroll_handle)
}

fn recipient_option_row(option: &RecipientOption) -> gpui::Div {
    let display_address = recipient_option_display_address(option);

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(app_strong_text(option.label.to_string()))
                .child(
                    Icon::new(option.source.icon())
                        .small()
                        .text_color(rgb(theme::TEXT_MUTED)),
                ),
        )
        .child(
            div()
                .text_size(RECIPIENT_OPTION_ADDRESS_TEXT_SIZE)
                .font_family(APP_FONT_FAMILY)
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(display_address)),
        )
}

impl WalletRoot {
    pub(in crate::root) fn reload_address_books(&mut self, cx: &mut Context<'_, Self>) {
        let Some(store) = self.vault_store.as_ref() else {
            self.private_address_book.clear();
            self.public_address_book.clear();
            return;
        };
        let Some(view_session) = self.view_session.as_ref() else {
            self.private_address_book.clear();
            self.public_address_book.clear();
            return;
        };
        match (
            store.list_private_address_book_entries_for_session(view_session.as_ref()),
            store.list_public_address_book_entries_for_session(view_session.as_ref()),
        ) {
            (Ok(private_entries), Ok(public_entries)) => {
                self.private_address_book = private_entries;
                self.public_address_book = public_entries;
            }
            (private_result, public_result) => {
                if let Err(error) = private_result {
                    tracing::warn!(
                        error_kind = vault_error_kind(&error),
                        "load private address book failed"
                    );
                }
                if let Err(error) = public_result {
                    tracing::warn!(
                        error_kind = vault_error_kind(&error),
                        "load public address book failed"
                    );
                }
                self.private_address_book.clear();
                self.public_address_book.clear();
            }
        }
        cx.notify();
    }

    pub(in crate::root) fn private_send_recipient_options(&self) -> Vec<RecipientOption> {
        private_send_recipient_options(
            &self.private_wallet_recipient_sources(),
            &self.private_address_book,
        )
    }

    pub(in crate::root) fn private_unshield_recipient_options(&self) -> Vec<RecipientOption> {
        private_unshield_recipient_options(&self.public_accounts, &self.public_address_book)
    }

    fn private_wallet_recipient_sources(&self) -> Vec<PrivateWalletRecipientSource> {
        let Some(store) = self.vault_store.as_ref() else {
            return Vec::new();
        };
        let Some(view_session) = self.view_session.as_ref() else {
            return Vec::new();
        };
        self.wallet_metadata
            .iter()
            .filter_map(|metadata| {
                let address = if metadata.wallet_uuid == view_session.wallet_id() {
                    view_session.receive_address().ok().or_else(|| {
                        metadata
                            .hardware_account
                            .as_ref()
                            .and_then(|account| account.receive_address.clone())
                    })
                } else if metadata.source.is_hardware_derived() {
                    return hardware_wallet_recipient_source_from_metadata(metadata);
                } else {
                    store
                        .load_view_session_with_view_session(
                            view_session.as_ref(),
                            &metadata.wallet_uuid,
                        )
                        .ok()
                        .and_then(|session| session.receive_address().ok())
                }?;
                Some(PrivateWalletRecipientSource {
                    label: Arc::from(metadata.label.as_str()),
                    address: Arc::from(address),
                    active: metadata.status == WalletStatus::Active,
                })
            })
            .collect()
    }

    pub(in crate::root) fn set_private_action_recipient(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        recipient: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let recipient_value = Arc::from(recipient);
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.recipient_value.as_ref() == recipient {
                    return false;
                }
                form.recipient_value = Arc::clone(&recipient_value);
                form.error = None;
                form.result = None;
                form.cost_estimate = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.recipient_input.update(cx, |input, cx| {
                    input.set_value(recipient.to_owned(), window, cx);
                });
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.recipient_value.as_ref() == recipient {
                    return false;
                }
                form.recipient_value = Arc::clone(&recipient_value);
                form.error = None;
                form.result = None;
                form.cost_estimate = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.recipient_input.update(cx, |input, cx| {
                    input.set_value(recipient.to_owned(), window, cx);
                });
                true
            }),
        };
        if changed {
            if kind == DeliveryFormKind::Unshield {
                self.refresh_unshield_native_top_up_state(key, cx);
            }
            self.debounce_public_broadcaster_cost_estimate(kind, key, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn set_recipient_suggestions_open(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(query) = self.recipient_query(kind, key) else {
            return;
        };
        let options = self.recipient_options_for_kind(kind);
        let open = open && !options.is_empty();
        let filter_query = recipient_suggestion_filter_query(kind, &query);
        let selected_index = if open {
            first_recipient_suggestion_index(
                filtered_recipient_options(&options, &filter_query).len(),
            )
        } else {
            None
        };
        let changed = self.set_recipient_suggestions_state(kind, key, open, selected_index);
        if changed {
            cx.notify();
        }
    }

    pub(in crate::root) fn toggle_recipient_suggestions(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let currently_open = self.recipient_suggestions_open(kind, key).unwrap_or(false);
        self.set_recipient_suggestions_open(kind, key, !currently_open, cx);
    }

    pub(in crate::root) fn dismiss_recipient_suggestions(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = self.set_recipient_suggestions_state(kind, key, false, None);
        if changed {
            cx.notify();
        }
    }

    pub(in crate::root) fn update_recipient_suggestions_for_input_change(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(query) = self.recipient_query(kind, key) else {
            return;
        };
        let options = self.recipient_options_for_kind(kind);
        let open = !query.trim().is_empty()
            && !recipient_query_is_valid(kind, &query)
            && !options.is_empty();
        let selected_index = if open {
            first_recipient_suggestion_index(filtered_recipient_options(&options, &query).len())
        } else {
            None
        };
        let changed = self.set_recipient_suggestions_state(kind, key, open, selected_index);
        if changed {
            cx.notify();
        }
    }

    pub(in crate::root) fn move_recipient_suggestion_selection(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        direction: RecipientSuggestionDirection,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(query) = self.recipient_query(kind, key) else {
            return;
        };
        let options = self.recipient_options_for_kind(kind);
        let save_visible = match kind {
            DeliveryFormKind::Send => can_save_private_recipient(&query, &options),
            DeliveryFormKind::Unshield => can_save_public_recipient(&query, &options),
        };
        if options.is_empty() || save_visible {
            return;
        }
        let filter_query = recipient_suggestion_filter_query(kind, &query);
        let filtered_len = filtered_recipient_options(&options, &filter_query).len();
        let current = self.recipient_suggestion_index(kind, key);
        let selected_index =
            recipient_suggestion_index_after_move(current, filtered_len, direction);
        let changed = self.set_recipient_suggestions_state(kind, key, true, selected_index);
        if changed {
            cx.notify();
        }
    }

    pub(in crate::root) fn confirm_selected_recipient_suggestion(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.recipient_suggestions_open(kind, key).unwrap_or(false) {
            return;
        }
        let query = self.recipient_query(kind, key);
        let Some(query) = query else {
            return;
        };
        let filter_query = recipient_suggestion_filter_query(kind, &query);
        let filtered =
            filtered_recipient_options(&self.recipient_options_for_kind(kind), &filter_query);
        let selected_index = self
            .recipient_suggestion_index(kind, key)
            .or_else(|| (filtered.len() == 1).then_some(0));
        let Some(option) = selected_index.and_then(|index| filtered.get(index)) else {
            return;
        };
        let recipient = selected_recipient_address(option).to_owned();
        self.select_recipient_suggestion(kind, key, &recipient, window, cx);
    }

    pub(in crate::root) fn select_recipient_suggestion(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        recipient: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let suggestions_changed = self.set_recipient_suggestions_state(kind, key, false, None);
        self.set_private_action_recipient(kind, key, recipient, window, cx);
        if suggestions_changed {
            cx.notify();
        }
    }

    pub(in crate::root) fn recipient_combobox_search_active(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> bool {
        let Some(open) = self.recipient_suggestions_open(kind, key) else {
            return false;
        };
        if !open {
            return false;
        }
        let Some(query) = self.recipient_query(kind, key) else {
            return false;
        };
        !query.trim().is_empty() && !recipient_query_is_valid(kind, &query)
    }

    fn recipient_options_for_kind(&self, kind: DeliveryFormKind) -> Vec<RecipientOption> {
        match kind {
            DeliveryFormKind::Send => self.private_send_recipient_options(),
            DeliveryFormKind::Unshield => self.private_unshield_recipient_options(),
        }
    }

    fn recipient_query(&self, kind: DeliveryFormKind, key: UnshieldAssetKey) -> Option<String> {
        match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .filter(|form| !form.generating)
                .map(|form| form.recipient_value.to_string()),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .filter(|form| !form.generating)
                .map(|form| form.recipient_value.to_string()),
        }
    }

    fn recipient_suggestions_open(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> Option<bool> {
        match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .map(|form| form.recipient_suggestions_open),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .map(|form| form.recipient_suggestions_open),
        }
    }

    fn recipient_suggestion_index(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> Option<usize> {
        match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .and_then(|form| form.recipient_suggestion_index),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .and_then(|form| form.recipient_suggestion_index),
        }
    }

    fn set_recipient_suggestions_state(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        open: bool,
        selected_index: Option<usize>,
    ) -> bool {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if let Some(index) = selected_index {
                    form.recipient_suggestions_scroll.scroll_to_item(index);
                }
                if form.recipient_suggestions_open == open
                    && form.recipient_suggestion_index == selected_index
                {
                    return false;
                }
                form.recipient_suggestions_open = open;
                form.recipient_suggestion_index = selected_index;
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if let Some(index) = selected_index {
                    form.recipient_suggestions_scroll.scroll_to_item(index);
                }
                if form.recipient_suggestions_open == open
                    && form.recipient_suggestion_index == selected_index
                {
                    return false;
                }
                form.recipient_suggestions_open = open;
                form.recipient_suggestion_index = selected_index;
                true
            }),
        }
    }

    pub(in crate::root) fn open_save_recipient_dialog(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        recipient: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.address_book_save_error = None;
        self.address_book_label_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let root = cx.entity();
        let label_input = self.address_book_label_input.clone();
        let dialog_label_input = label_input.clone();
        let save_label_input = label_input.clone();
        let save_recipient = recipient.clone();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(460.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            let save_root = root.clone();
            let save_recipient = save_recipient.clone();
            let save_label_input = save_label_input.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Save recipient"))
                .footer(dialog_footer("Save", true))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.address_book_save_error = None;
                        root.address_book_label_input
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    });
                })
                .on_ok(move |_event, window, cx| {
                    let label = save_label_input.read(cx).value().to_string();
                    let recipient = save_recipient.clone();
                    save_root.update(cx, |root, cx| {
                        root.save_recipient_to_address_book(
                            kind, key, &label, &recipient, window, cx,
                        )
                    })
                })
                .child(content_root.read(cx).render_save_recipient_dialog_content(
                    &dialog_label_input,
                    &recipient,
                    content_width,
                ))
        });
        cx.defer_in(window, move |_root, window, cx| {
            label_input.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    fn render_save_recipient_dialog_content(
        &self,
        label_input: &Entity<InputState>,
        recipient: &str,
        content_width: Pixels,
    ) -> gpui::Div {
        div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .child(app_input(label_input))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(app_muted_text("Address"))
                    .child(
                        div()
                            .w_full()
                            .p(px(8.0))
                            .rounded_sm()
                            .bg(rgb(theme::SURFACE))
                            .font_family(APP_FONT_FAMILY)
                            .text_size(APP_TEXT_SIZE)
                            .child(SharedString::from(recipient.to_owned())),
                    ),
            )
            .children(self.address_book_save_error.as_ref().map(|error| {
                Alert::error("wallet-address-book-save-error", error.to_string()).small()
            }))
    }

    fn save_recipient_to_address_book(
        &mut self,
        kind: DeliveryFormKind,
        _key: UnshieldAssetKey,
        label: &str,
        recipient: &str,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(label) = normalized_address_book_save_label(label) else {
            self.address_book_save_error = Some(Arc::from("Enter a label to save this recipient"));
            cx.notify();
            return false;
        };
        let Some(store) = self.vault_store.as_ref() else {
            self.address_book_save_error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return false;
        };
        let Some(view_session) = self.view_session.as_ref() else {
            self.address_book_save_error = Some(Arc::from("Unlock the wallet vault first"));
            cx.notify();
            return false;
        };
        let result = match kind {
            DeliveryFormKind::Send => store
                .add_private_address_book_entry_for_session(
                    view_session.as_ref(),
                    &label,
                    recipient,
                )
                .map(|_| ()),
            DeliveryFormKind::Unshield => store
                .add_public_address_book_entry_for_session(view_session.as_ref(), &label, recipient)
                .map(|_| ()),
        };
        match result {
            Ok(()) => {
                self.address_book_save_error = None;
                self.reload_address_books(cx);
                true
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    "save address book recipient failed"
                );
                self.address_book_save_error = Some(Arc::from(error.to_string()));
                cx.notify();
                false
            }
        }
    }
}
