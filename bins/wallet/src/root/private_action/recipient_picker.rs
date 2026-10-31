use crate::root::ui_helpers::dialog_footer;

use super::*;
use gpui::{ElementId, FontWeight};
use ui::recipient_picker::{RecipientPicker, RecipientPickerEvent, RecipientSuggestion};
pub(in crate::root) use ui::recipient_picker::{
    RecipientSuggestionDirection,
    suggestion_index_after_move as recipient_suggestion_index_after_move,
};
use wallet_ops::{bridge::NonEvmNetwork, vault::NonEvmAddressBookEntry};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum RecipientOptionSource {
    PrivateWallet,
    PrivateAddressBook,
    PublicAccount,
    PublicAddressBook,
    NonEvmAddressBook,
}

/// The address rules a recipient picker follows: a delivery form's, or for a swap receiver on
/// a non-EVM network, that network's own format, with its non-EVM address-book entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum RecipientRules {
    Form(DeliveryFormKind),
    NonEvm(NonEvmNetwork),
}

impl From<DeliveryFormKind> for RecipientRules {
    fn from(kind: DeliveryFormKind) -> Self {
        Self::Form(kind)
    }
}

/// The non-EVM address-book entries of `network`, as a swap receiver's suggestions there.
pub(in crate::root) fn non_evm_recipient_options(
    address_book: &[NonEvmAddressBookEntry],
    network: NonEvmNetwork,
) -> Vec<RecipientOption> {
    address_book
        .iter()
        .filter(|entry| entry.network == network)
        .map(|entry| RecipientOption {
            label: Arc::from(entry.label.as_str()),
            address: Arc::from(entry.address.as_str()),
            source: RecipientOptionSource::NonEvmAddressBook,
        })
        .collect()
}

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
    chain_id: u64,
) -> Vec<RecipientOption> {
    accounts
        .iter()
        .filter(|account| account.status == PublicAccountStatus::Active)
        .filter(|account| account.is_available_on_chain(chain_id))
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
    ui::recipient_picker::matches_search(&option.label, &option.address, query)
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
    kind: impl Into<RecipientRules>,
    recipient: &str,
) -> String {
    if recipient_query_is_valid(kind, recipient) {
        String::new()
    } else {
        recipient.to_owned()
    }
}

pub(in crate::root) fn recipient_query_is_valid(
    kind: impl Into<RecipientRules>,
    recipient: &str,
) -> bool {
    let recipient = recipient.trim();
    !recipient.is_empty()
        && match kind.into() {
            RecipientRules::Form(DeliveryFormKind::Send) => {
                parse_railgun_recipient(recipient).is_ok()
            }
            RecipientRules::Form(DeliveryFormKind::Unshield) => parse_address(recipient).is_some(),
            RecipientRules::NonEvm(network) => network.parse_receiver(recipient).is_ok(),
        }
}

fn first_recipient_suggestion_index(len: usize) -> Option<usize> {
    (len > 0).then_some(0)
}

// Suggestion list transitions: whether the list is open and which row is highlighted. They
// follow `kind`'s address rules, so every form that owns a picker behaves the same.

/// The list after the user opens or closes it. An open list highlights its first row.
pub(in crate::root) fn recipient_suggestions_toggled(
    kind: impl Into<RecipientRules>,
    options: &[RecipientOption],
    query: &str,
    open: bool,
) -> (bool, Option<usize>) {
    let open = open && !options.is_empty();
    let filter_query = recipient_suggestion_filter_query(kind, query);
    let selected_index = if open {
        first_recipient_suggestion_index(filtered_recipient_options(options, &filter_query).len())
    } else {
        None
    };
    (open, selected_index)
}

/// The list after the input changed: open while a partial entry searches the options.
pub(in crate::root) fn recipient_suggestions_for_input(
    kind: impl Into<RecipientRules>,
    options: &[RecipientOption],
    query: &str,
) -> (bool, Option<usize>) {
    let open =
        !query.trim().is_empty() && !recipient_query_is_valid(kind, query) && !options.is_empty();
    let selected_index = if open {
        first_recipient_suggestion_index(filtered_recipient_options(options, query).len())
    } else {
        None
    };
    (open, selected_index)
}

/// The open list after an arrow key moved its highlight, or `None` when the key doesn't move
/// it: there is nothing to suggest, or the input holds a new recipient to save.
pub(in crate::root) fn recipient_suggestion_moved(
    kind: impl Into<RecipientRules>,
    options: &[RecipientOption],
    query: &str,
    current: Option<usize>,
    direction: RecipientSuggestionDirection,
) -> Option<(bool, Option<usize>)> {
    let kind = kind.into();
    if options.is_empty() || can_save_recipient(kind, query, options) {
        return None;
    }
    let filter_query = recipient_suggestion_filter_query(kind, query);
    let filtered_len = filtered_recipient_options(options, &filter_query).len();
    Some((
        true,
        recipient_suggestion_index_after_move(current, filtered_len, direction),
    ))
}

/// The address Enter selects: the highlighted row, or the only one.
pub(in crate::root) fn recipient_suggestion_to_confirm(
    kind: impl Into<RecipientRules>,
    options: &[RecipientOption],
    query: &str,
    current: Option<usize>,
) -> Option<String> {
    let filter_query = recipient_suggestion_filter_query(kind, query);
    let filtered = filtered_recipient_options(options, &filter_query);
    let selected_index = current.or_else(|| (filtered.len() == 1).then_some(0));
    selected_index
        .and_then(|index| filtered.get(index))
        .map(|option| selected_recipient_address(option).to_owned())
}

/// Whether the input holds a valid recipient that no option already names.
pub(in crate::root) fn can_save_recipient(
    kind: impl Into<RecipientRules>,
    recipient: &str,
    options: &[RecipientOption],
) -> bool {
    match kind.into() {
        RecipientRules::Form(DeliveryFormKind::Send) => {
            can_save_private_recipient(recipient, options)
        }
        RecipientRules::Form(DeliveryFormKind::Unshield) => {
            can_save_public_recipient(recipient, options)
        }
        // Options hold canonical receivers, which the entry is compared in.
        RecipientRules::NonEvm(network) => network
            .parse_receiver(recipient)
            .is_ok_and(|receiver| !options.iter().any(|option| *option.address == *receiver)),
    }
}

pub(in crate::root) fn selected_recipient_address(option: &RecipientOption) -> &str {
    &option.address
}

pub(in crate::root) fn recipient_option_display_address(option: &RecipientOption) -> String {
    match option.source {
        RecipientOptionSource::PrivateWallet | RecipientOptionSource::PrivateAddressBook => {
            short_hash(&option.address)
        }
        RecipientOptionSource::PublicAccount
        | RecipientOptionSource::PublicAddressBook
        | RecipientOptionSource::NonEvmAddressBook => option.address.to_string(),
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
    let save_root = root.clone();
    form_recipient_picker(
        delivery_element_id(key, kind, "recipient-picker"),
        delivery_element_id(key, kind, "save-recipient"),
        kind,
        input,
        current_value,
        suggestions_open,
        selected_index,
        suggestions_scroll,
        options,
        generating,
        move |event, window, cx| {
            root.update(cx, |root, cx| match event {
                RecipientPickerEvent::Toggle => root.toggle_recipient_suggestions(kind, key, cx),
                RecipientPickerEvent::Dismiss => root.dismiss_recipient_suggestions(kind, key, cx),
                RecipientPickerEvent::Move(direction) => {
                    root.move_recipient_suggestion_selection(kind, key, *direction, cx);
                }
                RecipientPickerEvent::Select(address) => {
                    root.select_recipient_suggestion(kind, key, address, window, cx);
                }
            });
        },
        move |recipient, window, cx| {
            save_root.update(cx, |root, cx| {
                root.open_save_recipient_dialog(kind, recipient, window, cx);
            });
        },
    )
}

/// The recipient picker of a form that owns its suggestion state, such as a Private Unshield
/// or a swap's receiver. `kind` decides the address rules and the options, and Save offers a
/// valid recipient that no option names. The form handles `on_event`; `on_save` gets the
/// recipient to add to `kind`'s address book.
pub(in crate::root) fn form_recipient_picker(
    id: impl Into<ElementId>,
    save_id: SharedString,
    kind: impl Into<RecipientRules>,
    input: &Entity<InputState>,
    current_value: &str,
    suggestions_open: bool,
    selected_index: Option<usize>,
    suggestions_scroll: &ScrollHandle,
    options: &[RecipientOption],
    disabled: bool,
    on_event: impl Fn(&RecipientPickerEvent, &mut Window, &mut App) + 'static,
    on_save: impl Fn(String, &mut Window, &mut App) + 'static,
) -> RecipientPicker {
    let kind = kind.into();
    let save_visible = can_save_recipient(kind, current_value, options);
    let options = options
        .iter()
        .map(|option| {
            RecipientSuggestion::new(
                option.address.to_string(),
                option.label.to_string(),
                option.address.to_string(),
            )
            .display_address(recipient_option_display_address(option))
            .account(matches!(
                option.source,
                RecipientOptionSource::PrivateWallet | RecipientOptionSource::PublicAccount
            ))
        })
        .collect();
    RecipientPicker::new(id, input, options, on_event)
        .query(recipient_suggestion_filter_query(kind, current_value))
        .suggestions(suggestions_open, selected_index, suggestions_scroll)
        .disabled(disabled)
        .when(save_visible, |picker| {
            let recipient = current_value.trim().to_owned();
            let selector = save_id.clone();
            picker.action(
                app_button_base(save_id)
                    .debug_selector(move || selector.to_string())
                    .icon(Icon::new(RailgunActionIcon::Save))
                    .outline()
                    .small()
                    .compact()
                    .accessibility_label("Save recipient")
                    .tooltip("Save recipient")
                    .disabled(disabled)
                    .on_click(move |_, window, cx| on_save(recipient.clone(), window, cx)),
            )
        })
}

impl WalletRoot {
    /// Forget the loaded private, public and non-EVM address books.
    pub(in crate::root) fn clear_address_books(&mut self) {
        self.private_address_book.clear();
        self.public_address_book.clear();
        self.non_evm_address_book.clear();
    }

    pub(in crate::root) fn reload_address_books(&mut self, cx: &mut Context<'_, Self>) {
        let Some(store) = self.vault_store.as_ref() else {
            self.clear_address_books();
            return;
        };
        let Some(view_session) = self.view_session.as_ref() else {
            self.clear_address_books();
            return;
        };
        match (
            store.list_private_address_book_entries_for_session(view_session.as_ref()),
            store.list_public_address_book_entries_for_session(view_session.as_ref()),
            store.list_non_evm_address_book_entries_for_session(view_session.as_ref()),
        ) {
            (Ok(private_entries), Ok(public_entries), Ok(non_evm_entries)) => {
                self.private_address_book = private_entries;
                self.public_address_book = public_entries;
                self.non_evm_address_book = non_evm_entries;
            }
            (private_result, public_result, non_evm_result) => {
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
                if let Err(error) = non_evm_result {
                    tracing::warn!(
                        error_kind = vault_error_kind(&error),
                        "load non-EVM address book failed"
                    );
                }
                self.clear_address_books();
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
        private_unshield_recipient_options(
            &self.public_accounts,
            &self.public_address_book,
            self.selected_chain,
        )
    }

    fn private_wallet_recipient_sources(&self) -> Vec<PrivateWalletRecipientSource> {
        let Some(store) = self.vault_store.as_ref() else {
            return Vec::new();
        };
        let Some(view_session) = self.view_session.as_ref() else {
            return Vec::new();
        };
        crate::root::vault::visible_wallet_metadata(
            &self.wallet_metadata,
            self.revealed_passphrase_context_id.as_deref(),
        )
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
        let (open, selected_index) = recipient_suggestions_toggled(kind, &options, &query, open);
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
        let (open, selected_index) = recipient_suggestions_for_input(kind, &options, &query);
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
        let current = self.recipient_suggestion_index(kind, key);
        let Some((open, selected_index)) =
            recipient_suggestion_moved(kind, &options, &query, current, direction)
        else {
            return;
        };
        let changed = self.set_recipient_suggestions_state(kind, key, open, selected_index);
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
        let Some(recipient) = recipient_suggestion_to_confirm(
            kind,
            &self.recipient_options_for_kind(kind),
            &query,
            self.recipient_suggestion_index(kind, key),
        ) else {
            return;
        };
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
        kind: impl Into<RecipientRules>,
        recipient: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let kind = kind.into();
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
                        root.save_recipient_to_address_book(kind, &label, &recipient, window, cx)
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
                            .font_weight(FontWeight::LIGHT)
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
        kind: RecipientRules,
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
            RecipientRules::Form(DeliveryFormKind::Send) => store
                .add_private_address_book_entry_for_session(
                    view_session.as_ref(),
                    &label,
                    recipient,
                )
                .map(|_| ()),
            RecipientRules::Form(DeliveryFormKind::Unshield) => store
                .add_public_address_book_entry_for_session(view_session.as_ref(), &label, recipient)
                .map(|_| ()),
            RecipientRules::NonEvm(network) => store
                .add_non_evm_address_book_entry_for_session(
                    view_session.as_ref(),
                    &label,
                    network,
                    recipient,
                )
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
