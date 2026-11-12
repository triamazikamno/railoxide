use std::sync::Arc;

use broadcaster_monitor::EventRx;
use gpui::{Context, Focusable, Window};
use gpui_component::{
    WindowExt,
    input::InputEvent,
    select::{SearchableVec, SelectEvent},
    table::TableEvent,
};
use tokio::sync::watch;
use ui::chain_select::Network as SelectedNetwork;
use ui::controls::FullWidthSelectItems;

use super::broadcaster_view::BroadcasterPreferenceListKind;
use super::search_field::SearchFieldEvent;
use super::utxo::should_refresh_utxo_ages;
use super::wallet_header::WalletSelectItem;
use super::{
    DeliveryMode, UNSHIELD_SPINNER_REFRESH_INTERVAL, UTXO_AGE_REFRESH_INTERVAL, WalletRoot,
    WalletTab, ethereum_weth_public_broadcaster_count, logs_search, should_apply_background_focus,
    wallet_header, walletconnect,
};

impl WalletRoot {
    pub(super) fn observe_window_focus(window: &mut Window, cx: &mut Context<'_, Self>) {
        cx.observe_window_activation(window, |root, window, cx| {
            if window.is_window_active() {
                root.enforce_auto_lock(window, cx);
                root.refresh_device_auth_status();
                cx.notify();
            }
            root.sync_platform_attention_for_window(window);
        })
        .detach();
        cx.on_focus_lost(window, |root, window, cx| {
            root.focus_wallet_view_after_focus_loss(window, cx);
        })
        .detach();
    }

    pub(super) fn subscribe_logs(&self, window: &Window, cx: &mut Context<'_, Self>) {
        cx.subscribe(
            &self.logs_search,
            |root, search, _: &SearchFieldEvent, cx| {
                let filter = logs_search::log_filter(search.read(cx).filter());
                root.logs.update(cx, |logs, cx| logs.set_filter(filter, cx));
            },
        )
        .detach();
        cx.subscribe_in(&self.logs, window, Self::logs_pane_event)
            .detach();
        // The open panel's title bar shows the pane's line counts.
        cx.observe(&self.logs, |root, _, cx| {
            if root.logs_open {
                cx.notify();
            }
        })
        .detach();
    }

    pub(super) fn subscribe_utxo_inputs(&self, cx: &mut Context<'_, Self>) {
        cx.subscribe(
            &self.tx_search_input,
            |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().trim().to_ascii_lowercase();
                    this.tx_search_query = Arc::from(query);
                    this.sync_utxo_table(cx);
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe(&self.utxo_table, |_, table, event: &TableEvent, cx| {
            if let TableEvent::ColumnWidthsChanged(widths) = event {
                table.update(cx, |table, cx| {
                    table.delegate_mut().set_column_widths(widths);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub(super) fn subscribe_public_action_inputs(&self, cx: &mut Context<'_, Self>) {
        for input in [
            self.public_form.send_amount_input.clone(),
            self.public_form.shield_amount_input.clone(),
            self.public_form.send_gas_fee.max_fee_input.clone(),
            self.public_form.send_gas_fee.max_priority_fee_input.clone(),
            self.public_form.shield_gas_fee.max_fee_input.clone(),
            self.public_form
                .shield_gas_fee
                .max_priority_fee_input
                .clone(),
            self.public_form.wrap_gas_fee.max_fee_input.clone(),
            self.public_form.wrap_gas_fee.max_priority_fee_input.clone(),
            self.public_form.unwrap_gas_fee.max_fee_input.clone(),
            self.public_form
                .unwrap_gas_fee
                .max_priority_fee_input
                .clone(),
        ] {
            cx.subscribe(&input, |_this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            })
            .detach();
        }
        cx.subscribe(
            &self.public_form.shield_amount_input,
            |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.refresh_native_shield_preparation(cx);
                }
            },
        )
        .detach();
        for input in [
            self.public_form.advanced_send_to_input.clone(),
            self.public_form.advanced_send_value_input.clone(),
            self.public_form.send_gas_fee.max_fee_input.clone(),
            self.public_form.send_gas_fee.max_priority_fee_input.clone(),
        ] {
            cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.invalidate_advanced_public_send_estimate();
                    cx.notify();
                }
            })
            .detach();
        }
        cx.subscribe(
            &self.public_form.advanced_send_data_input,
            |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.invalidate_advanced_public_send_estimate();
                    cx.notify();
                }
            },
        )
        .detach();
    }

    pub(super) fn subscribe_governance_inputs(&self, window: &Window, cx: &mut Context<'_, Self>) {
        cx.subscribe_in(
            &self.governance.participant_picker,
            window,
            |this, _picker, event, window, cx| {
                this.participant_picker_event(event, window, cx);
            },
        )
        .detach();
        cx.subscribe(
            &self.governance.proposal_action_amount_input,
            |_this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            },
        )
        .detach();
        for input in [
            self.governance.reward_gas_fee.max_fee_input.clone(),
            self.governance
                .reward_gas_fee
                .max_priority_fee_input
                .clone(),
        ] {
            cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.governance_gas_fee_changed(cx);
                }
            })
            .detach();
        }
    }

    pub(super) fn subscribe_address_book_inputs(
        &self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.subscribe(
            &self.address_book.search_input,
            |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().trim().to_ascii_lowercase();
                    this.address_book.search_query = Arc::from(query);
                    cx.notify();
                }
            },
        )
        .detach();
        for input in [
            self.address_book.add_label_input.clone(),
            self.address_book.add_address_input.clone(),
            self.address_book.edit_label_input.clone(),
            self.address_book.edit_address_input.clone(),
        ] {
            cx.subscribe(&input, |_this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            })
            .detach();
        }
        for (input, kind) in [
            (
                self.favorite_broadcaster_input.clone(),
                BroadcasterPreferenceListKind::Favorite,
            ),
            (
                self.banned_broadcaster_input.clone(),
                BroadcasterPreferenceListKind::Banned,
            ),
        ] {
            cx.subscribe_in(
                &input,
                window,
                move |this, _input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        this.add_broadcaster_preference_from_input(kind, window, cx);
                    }
                    InputEvent::Change => {
                        this.broadcaster_preference_error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
        }
    }

    pub(super) fn subscribe_header_selects(&self, window: &Window, cx: &mut Context<'_, Self>) {
        cx.subscribe_in(
            &self.chain_select,
            window,
            |this, _select, event: &SelectEvent<wallet_header::NetworkSelectItems>, window, cx| {
                match event {
                    SelectEvent::Confirm(Some(SelectedNetwork::Evm(chain_id))) => {
                        this.select_chain(*chain_id, window, cx);
                    }
                    SelectEvent::Confirm(Some(SelectedNetwork::Tron)) => {
                        this.select_tron(window, cx);
                    }
                    SelectEvent::Confirm(None) => return,
                }
                cx.defer_in(window, |_this, window, cx| {
                    if should_apply_background_focus(window.has_active_dialog(cx)) {
                        window.blur(cx);
                    }
                });
            },
        )
        .detach();
        cx.subscribe_in(
            &self.wallet_select,
            window,
            |this, _select, event: &SelectEvent<SearchableVec<WalletSelectItem>>, window, cx| {
                let SelectEvent::Confirm(Some(value)) = event else {
                    return;
                };
                this.select_wallet(value.as_ref(), window, cx);
                cx.defer_in(window, |_this, window, cx| {
                    if should_apply_background_focus(window.has_active_dialog(cx)) {
                        window.blur(cx);
                    }
                });
            },
        )
        .detach();
    }

    pub(super) fn subscribe_walletconnect_inputs(
        &self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.subscribe_in(
            &self.walletconnect.uri_input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    this.start_walletconnect_pairing_from_input(window, cx);
                }
                InputEvent::Change => {
                    this.walletconnect.error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        cx.subscribe_in(
            &self.walletconnect.account_select,
            window,
            |this,
             _select,
             event: &SelectEvent<
                FullWidthSelectItems<walletconnect::WalletConnectAccountSelectItem>,
            >,
             window,
             cx| {
                let SelectEvent::Confirm(Some(public_account_uuid)) = event else {
                    return;
                };
                this.set_walletconnect_selected_account(public_account_uuid.clone(), cx);
                cx.defer_in(window, |_this, window, cx| {
                    if should_apply_background_focus(window.has_active_dialog(cx)) {
                        window.blur(cx);
                    }
                });
            },
        )
        .detach();
    }

    pub(super) fn subscribe_vault_inputs(&self, window: &Window, cx: &mut Context<'_, Self>) {
        cx.subscribe_in(
            &self.unlock_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.unlock_vault_from_input(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &self.new_password_input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if !matches!(event, InputEvent::PressEnter { .. }) {
                    return;
                }
                let password_entered = !input.read(cx).value().trim().is_empty();
                let confirm_empty = this
                    .confirm_password_input
                    .read(cx)
                    .value()
                    .trim()
                    .is_empty();
                if password_entered && confirm_empty {
                    this.confirm_password_input
                        .read(cx)
                        .focus_handle(cx)
                        .focus(window, cx);
                } else {
                    this.create_vault_from_inputs(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &self.confirm_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.create_vault_from_inputs(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &self.wallet_name_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_wallet_setup_from_input(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &self.add_wallet_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_wallet_setup_from_input(window, cx);
                }
            },
        )
        .detach();
    }

    pub(super) fn subscribe_hardware_wallet_inputs(
        &self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.subscribe_in(
            &self.hardware_wallet_restore_account_index_input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    this.submit_default_hardware_wallet_setup(window, cx);
                }
                InputEvent::Change => {
                    this.hardware_wallet_restore_account_index_set =
                        !input.read(cx).value().trim().is_empty();
                    this.vault_error = None;
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();
        #[cfg(feature = "hardware")]
        {
            cx.subscribe_in(
                &self.hardware_profile_password_input,
                window,
                |this, _input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.unlock_hardware_profile_from_dialog(window, cx);
                    }
                },
            )
            .detach();
            for input in [
                self.hardware_profile_label_input.clone(),
                self.hardware_profile_recovery_start_input.clone(),
                self.hardware_profile_recovery_count_input.clone(),
                self.hardware_profile_exact_index_input.clone(),
            ] {
                cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.hardware_profile_unlock.error = None;
                        cx.notify();
                    }
                })
                .detach();
            }
            cx.subscribe_in(
                &self.trezor_app_passphrase_input,
                window,
                |this, _input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        this.submit_trezor_profile_passphrase_input(window, cx);
                    }
                    InputEvent::Change => {
                        this.hardware_profile_unlock.error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            )
            .detach();
        }
    }

    pub(super) fn subscribe_wallet_maintenance_inputs(
        &self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.subscribe(
            &self.repair_cache_block_input,
            |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.repair_wallet_cache_from_input(cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &self.manage_wallet_label_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.save_wallet_label_edit(window, cx);
                }
            },
        )
        .detach();
    }

    pub(super) fn subscribe_public_account_inputs(
        &self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.subscribe(
            &self.public_form.search_input,
            |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().trim().to_ascii_lowercase();
                    this.public_form.search_query = Arc::from(query);
                    this.public_form
                        .list_scroll
                        .set_offset(gpui::Point::default());
                    this.reconcile_public_account_selection();
                    cx.notify();
                }
            },
        )
        .detach();
        for input in [
            &self.public_form.import_label_input,
            &self.public_form.import_private_key_input,
        ] {
            cx.subscribe_in(
                input,
                window,
                |this, _input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.import_public_account_from_input(window, cx);
                    }
                },
            )
            .detach();
        }
        cx.subscribe_in(
            &self.public_form.add_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.add_public_derived_account_from_input(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &self.public_form.import_password_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.import_public_account_from_input(window, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &self.public_form.edit_label_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.update_selected_public_account_label(window, cx);
                }
            },
        )
        .detach();
    }

    pub(super) fn start_state_watchers(
        mut monitor_event_rx: EventRx,
        mut anchor_refresh_rx: watch::Receiver<u64>,
        cx: &mut Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while monitor_event_rx.changed().await.is_ok() {
                if this
                    .update(cx, |root, cx| {
                        let current_public_broadcaster_count =
                            ethereum_weth_public_broadcaster_count(&root.monitor_fee_rows());
                        let public_broadcaster_count_changed = root
                            .sidebar_public_broadcaster_count
                            != current_public_broadcaster_count;
                        root.sidebar_public_broadcaster_count = current_public_broadcaster_count;
                        if root
                            .send_forms
                            .values()
                            .any(|form| form.delivery_mode == DeliveryMode::PublicBroadcaster)
                            || root
                                .unshield_forms
                                .values()
                                .any(|form| form.delivery_mode == DeliveryMode::PublicBroadcaster)
                            || public_broadcaster_count_changed
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            while anchor_refresh_rx.changed().await.is_ok() {
                if this
                    .update(cx, |root, cx| {
                        root.refresh_walletconnect_fee_usd_values();
                        if let Some(settings) = &root.settings_editor {
                            let editor = settings.read(cx).chain_editor.clone();
                            editor.update(cx, |editor, cx| {
                                editor.set_pricing_status(
                                    root.public_broadcaster_anchor_cache.native_usd_statuses(),
                                    cx,
                                );
                            });
                        }
                        root.publish_gateway_desktop_state();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // Owner notifications include private snapshots, progress, metadata, and device outcomes.
        // Publication does not notify this entity; the gateway coalesces unchanged presentation.
        cx.observe_self(|root, _cx| {
            if root.gateway_has_connected_browser() {
                root.publish_gateway_desktop_state();
            }
        })
        .detach();
    }

    pub(super) fn spawn_utxo_age_refresh(cx: &Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(UTXO_AGE_REFRESH_INTERVAL)
                    .await;
                if this
                    .update(cx, |root, cx| {
                        if root
                            .activity_journal_failures
                            .has_changed()
                            .unwrap_or(false)
                        {
                            root.activity_journal_failures.borrow_and_update();
                            root.pending_activity_journal_notice = true;
                            cx.notify();
                        }
                        if should_refresh_utxo_ages(
                            root.active_activity,
                            root.active_wallet_tab,
                            root.chain_states
                                .get(&root.last_evm_chain)
                                .is_some_and(|state| state.snapshot().is_some()),
                        ) {
                            root.utxo_table.update(cx, |_table, cx| cx.notify());
                        }
                        root.tick_wallet_activity(cx);
                        if (root.private_pending_status_dialog_open
                            || root.gateway_has_connected_browser())
                            && root.private_pending_status_has_shield_timer()
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn spawn_form_and_balance_refresh(&self, cx: &Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(UNSHIELD_SPINNER_REFRESH_INTERVAL)
                    .await;
                if this
                    .update(cx, |root, cx| {
                        if root.send_forms.values().any(|form| {
                            form.generating || form.cost_estimate_pending || form.estimating_cost
                        }) || root.unshield_forms.values().any(|form| {
                            form.generating || form.cost_estimate_pending || form.estimating_cost
                        }) {
                            root.unshield_spinner_tick = root.unshield_spinner_tick.wrapping_add(1);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let public_balance_refresh_interval = self.public_balance_refresh_interval;
        cx.spawn(async move |this, cx| {
            let interval = public_balance_refresh_interval;
            loop {
                cx.background_executor().timer(interval).await;
                if this
                    .update(cx, |root, cx| {
                        if root.active_wallet_tab == WalletTab::Public {
                            root.schedule_automatic_public_balance_refresh(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}
