use std::sync::Arc;

use gpui::Context;
use wallet_ops::{
    parse_railgun_recipient,
    vault::{BroadcasterPreferences, VaultError},
};

use super::broadcaster_picker::{BroadcasterChoice, broadcaster_choice_supported_by_candidates};
use super::{DeliveryFormKind, DeliveryMode, UnshieldAssetKey, WalletRoot, vault_error_kind};

impl WalletRoot {
    pub(super) fn reload_broadcaster_preferences(&mut self, cx: &mut Context<'_, Self>) {
        let Some(store) = self.vault_store.as_ref() else {
            self.set_broadcaster_preferences(BroadcasterPreferences::default(), cx);
            return;
        };
        let Some(view_session) = self.view_session.as_ref() else {
            self.set_broadcaster_preferences(BroadcasterPreferences::default(), cx);
            return;
        };
        match store.list_broadcaster_preferences_for_session(view_session.as_ref()) {
            Ok(preferences) => {
                self.set_broadcaster_preferences(preferences, cx);
                self.broadcaster_preference_error = None;
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    "load broadcaster preferences failed"
                );
                self.set_broadcaster_preferences(BroadcasterPreferences::default(), cx);
                self.broadcaster_preference_error = Some(Arc::from(error.to_string()));
            }
        }
        cx.notify();
    }

    pub(super) fn add_favorite_broadcaster(
        &mut self,
        address: &str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let result = self.with_broadcaster_preference_store(|store, view_session| {
            store
                .add_favorite_broadcaster_for_session(view_session.as_ref(), address)
                .map(|_| ())
        });
        self.handle_broadcaster_preference_mutation_result(result, "add favorite broadcaster", cx)
    }

    pub(super) fn remove_favorite_broadcaster(
        &mut self,
        address: &str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let result = self.with_broadcaster_preference_store(|store, view_session| {
            store
                .remove_favorite_broadcaster_for_session(view_session.as_ref(), address)
                .map(|_| ())
        });
        self.handle_broadcaster_preference_mutation_result(
            result,
            "remove favorite broadcaster",
            cx,
        )
    }

    pub(super) fn add_banned_broadcaster(
        &mut self,
        address: &str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let result = self.with_broadcaster_preference_store(|store, view_session| {
            store
                .add_banned_broadcaster_for_session(view_session.as_ref(), address)
                .map(|_| ())
        });
        self.handle_broadcaster_preference_mutation_result(result, "ban broadcaster", cx)
    }

    pub(super) fn remove_banned_broadcaster(
        &mut self,
        address: &str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let result = self.with_broadcaster_preference_store(|store, view_session| {
            store
                .remove_banned_broadcaster_for_session(view_session.as_ref(), address)
                .map(|_| ())
        });
        self.handle_broadcaster_preference_mutation_result(result, "unban broadcaster", cx)
    }

    pub(super) fn toggle_favorite_broadcaster(
        &mut self,
        address: &str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.is_favorite_broadcaster(address) {
            self.remove_favorite_broadcaster(address, cx)
        } else {
            self.add_favorite_broadcaster(address, cx)
        }
    }

    pub(super) fn toggle_banned_broadcaster(
        &mut self,
        address: &str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.is_banned_broadcaster(address) {
            self.remove_banned_broadcaster(address, cx)
        } else {
            self.add_banned_broadcaster(address, cx)
        }
    }

    pub(super) fn is_favorite_broadcaster(&self, address: &str) -> bool {
        broadcaster_preference_is_favorite(&self.broadcaster_preferences, address)
    }

    pub(super) fn is_banned_broadcaster(&self, address: &str) -> bool {
        broadcaster_preference_is_banned(&self.broadcaster_preferences, address)
    }

    pub(super) fn set_broadcaster_preferences(
        &mut self,
        preferences: BroadcasterPreferences,
        cx: &mut Context<'_, Self>,
    ) {
        self.broadcaster_preferences = preferences.clone();
        match self.broadcaster_preference_snapshot.write() {
            Ok(mut snapshot) => {
                *snapshot = preferences;
            }
            Err(error) => {
                tracing::warn!(%error, "update broadcaster preference snapshot failed");
            }
        }
        self.monitor.update(cx, |monitor, cx| {
            monitor.refresh_preference_status(cx);
        });
    }

    fn with_broadcaster_preference_store(
        &self,
        f: impl FnOnce(
            &wallet_ops::vault::DesktopVaultStore,
            &Arc<wallet_ops::vault::DesktopViewSession>,
        ) -> Result<(), VaultError>,
    ) -> Result<(), VaultError> {
        let store = self.vault_store.as_ref().ok_or(VaultError::VaultNotFound)?;
        let view_session = self
            .view_session
            .as_ref()
            .ok_or(VaultError::VaultNotFound)?;
        f(store.as_ref(), view_session)
    }

    fn handle_broadcaster_preference_mutation_result(
        &mut self,
        result: Result<(), VaultError>,
        operation: &'static str,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match result {
            Ok(()) => {
                self.reload_broadcaster_preferences(cx);
                self.refresh_public_broadcaster_estimates_after_preference_change(cx);
                true
            }
            Err(error) => {
                tracing::warn!(
                    error_kind = vault_error_kind(&error),
                    operation,
                    "broadcaster preference operation failed"
                );
                self.broadcaster_preference_error =
                    Some(Arc::from(broadcaster_preference_error_message(&error)));
                cx.notify();
                false
            }
        }
    }

    fn refresh_public_broadcaster_estimates_after_preference_change(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        let send_keys = self
            .send_forms
            .iter()
            .filter_map(|(key, form)| {
                (form.delivery_mode == DeliveryMode::PublicBroadcaster && !form.generating)
                    .then_some(*key)
            })
            .collect::<Vec<UnshieldAssetKey>>();
        let unshield_keys = self
            .unshield_forms
            .iter()
            .filter_map(|(key, form)| {
                (form.delivery_mode == DeliveryMode::PublicBroadcaster && !form.generating)
                    .then_some(*key)
            })
            .collect::<Vec<UnshieldAssetKey>>();

        for key in send_keys {
            self.reset_stale_send_broadcaster_choice_after_preference_change(key);
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
        }
        for key in unshield_keys {
            self.reset_stale_unshield_broadcaster_choice_after_preference_change(key);
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
        }
    }

    fn reset_stale_send_broadcaster_choice_after_preference_change(
        &mut self,
        key: UnshieldAssetKey,
    ) {
        let Some((chain_id, fee_token, choice, allow_suspicious, favorites_only)) = self
            .send_forms
            .get(&key)
            .filter(|form| matches!(form.broadcaster_choice, BroadcasterChoice::Specific { .. }))
            .map(|form| {
                (
                    form.asset.chain_id,
                    form.selected_fee_token,
                    form.broadcaster_choice.clone(),
                    form.allow_suspicious_broadcasters,
                    form.favorites_only_broadcasters,
                )
            })
        else {
            return;
        };
        let policy = self.public_broadcaster_fee_policy(allow_suspicious);
        let candidates = self.current_public_broadcaster_candidates(
            chain_id,
            fee_token,
            false,
            false,
            favorites_only,
            policy,
        );
        if broadcaster_choice_supported_by_candidates(&choice, &candidates, policy) {
            return;
        }
        if let Some(form) = self.send_forms.get_mut(&key) {
            form.broadcaster_choice = BroadcasterChoice::Random;
            form.custom_fee_amount = None;
            form.error = None;
            form.result = None;
        }
    }

    fn reset_stale_unshield_broadcaster_choice_after_preference_change(
        &mut self,
        key: UnshieldAssetKey,
    ) {
        let Some((
            chain_id,
            fee_token,
            unwrap,
            native_top_up,
            choice,
            allow_suspicious,
            favorites_only,
        )) = self
            .unshield_forms
            .get(&key)
            .filter(|form| matches!(form.broadcaster_choice, BroadcasterChoice::Specific { .. }))
            .map(|form| {
                (
                    form.asset.chain_id,
                    form.selected_fee_token,
                    form.unwrap,
                    form.native_top_up_enabled && form.native_top_up.is_some(),
                    form.broadcaster_choice.clone(),
                    form.allow_suspicious_broadcasters,
                    form.favorites_only_broadcasters,
                )
            })
        else {
            return;
        };
        let policy = self.public_broadcaster_fee_policy(allow_suspicious);
        let candidates = self.current_public_broadcaster_candidates(
            chain_id,
            fee_token,
            unwrap,
            native_top_up,
            favorites_only,
            policy,
        );
        if broadcaster_choice_supported_by_candidates(&choice, &candidates, policy) {
            return;
        }
        if let Some(form) = self.unshield_forms.get_mut(&key) {
            form.broadcaster_choice = BroadcasterChoice::Random;
            form.custom_fee_amount = None;
            form.error = None;
            form.result = None;
        }
    }
}

pub(super) fn broadcaster_preference_is_favorite(
    preferences: &BroadcasterPreferences,
    address: &str,
) -> bool {
    !broadcaster_preference_is_banned(preferences, address)
        && preferences
            .favorites
            .iter()
            .any(|entry| broadcaster_addresses_match(&entry.address, address))
}

pub(super) fn broadcaster_preference_is_banned(
    preferences: &BroadcasterPreferences,
    address: &str,
) -> bool {
    preferences
        .banned
        .iter()
        .any(|entry| broadcaster_addresses_match(&entry.address, address))
}

fn broadcaster_addresses_match(left: &str, right: &str) -> bool {
    match (
        parse_railgun_recipient(left),
        parse_railgun_recipient(right),
    ) {
        (Ok(left), Ok(right)) => {
            left.master_public_key == right.master_public_key
                && left.viewing_public_key == right.viewing_public_key
        }
        _ => false,
    }
}

fn broadcaster_preference_error_message(error: &VaultError) -> String {
    match error {
        VaultError::InvalidBroadcasterPreferenceAddress => {
            "Enter a valid broadcaster 0zk address".to_owned()
        }
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcaster_address_matching_rejects_invalid_values() {
        assert!(!broadcaster_addresses_match("not-a-0zk", "not-a-0zk"));
    }
}
