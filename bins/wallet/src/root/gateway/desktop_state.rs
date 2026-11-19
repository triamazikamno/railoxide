#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn gateway_has_connected_browser(&self) -> bool {
        self.gateway
            .snapshot
            .peers
            .iter()
            .any(|peer| peer.connected_sessions > 0)
    }

    /// The paired browser `peer_id` as Settings names it, which its drafts' journal entries
    /// give as their origin.
    pub(in crate::root) fn gateway_peer_name(&self, peer_id: &str) -> String {
        self.gateway
            .snapshot
            .peers
            .iter()
            .find(|peer| alloy::hex::encode(peer.id.to_bytes()) == peer_id)
            .and_then(|peer| peer.label.clone())
            .unwrap_or_else(|| short_identity(peer_id))
    }

    pub(in crate::root) fn publish_gateway_summaries(&self, summaries: Vec<(String, String)>) {
        if let Some(state) = &self.gateway.desktop_state {
            state.send_if_modified(|(_, _, current)| {
                if *current == summaries {
                    return false;
                }
                *current = summaries;
                true
            });
        }
    }

    pub(in crate::root) fn retire_gateway_unlock(&self) {
        self.gateway.retire_unlock();
        self.publish_gateway_desktop_state();
    }

    pub(in crate::root) fn begin_gateway_unlock(&self) -> Option<u64> {
        let continuation = self
            .gateway
            .unlock
            .borrow_mut()
            .begin(self.view_session.is_some());
        self.publish_gateway_desktop_state();
        continuation
    }

    pub(in crate::root) fn gateway_unlock_continuation(&self) -> Option<u64> {
        self.gateway.unlock.borrow().continuation
    }

    pub(in crate::root) fn gateway_unlock_is_current(&self, continuation: Option<u64>) -> bool {
        self.gateway.unlock.borrow().is_current(continuation)
    }

    pub(in crate::root) fn transfer_gateway_unlock_to_installation(
        &self,
        continuation: Option<u64>,
    ) {
        let mut unlock = self.gateway.unlock.borrow_mut();
        unlock.installing = unlock.is_current(continuation);
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn abandon_gateway_unlock_dialog(&self, continuation: Option<u64>) {
        let should_retire = {
            let unlock = self.gateway.unlock.borrow();
            unlock.is_current(continuation) && !unlock.installing
        };
        if should_retire {
            self.retire_gateway_unlock();
        }
    }

    pub(in crate::root) fn complete_gateway_unlock(&self, continuation: Option<u64>) {
        self.gateway
            .unlock
            .borrow_mut()
            .complete(continuation, self.view_session.is_some());
    }

    pub(in crate::root) fn current_public_balance_scope(
        &self,
        chain_id: u64,
    ) -> Option<wallet_ops::PublicBalanceScope> {
        let config = self.effective_chain_configs.get(chain_id)?;
        let route =
            wallet_ops::settings::resolve_effective_chain_rpc_route(chain_id, config).ok()?;
        self.public_balance_scope_for_route(route)
    }

    /// The current wallet's balance authority over `route`, or `None` while reads are blocked.
    pub(in crate::root) fn public_balance_scope_for_route(
        &self,
        route: wallet_ops::RpcChainRoute,
    ) -> Option<wallet_ops::PublicBalanceScope> {
        if self.public_transaction_cleanup.is_some()
            || self.public_sync_cache_resetting
            || self.merkle_forest_cache_resetting
            || self.manage_wallets.deleting_wallet_id.is_some()
            || !matches!(self.vault_state, VaultState::ViewUnlocked)
        {
            return None;
        }
        let view = self.view_session.as_ref()?;
        Some(wallet_ops::PublicBalanceScope::new(
            view.wallet_id().to_owned(),
            self.active_wallet_generation,
            self.http.rpc_broker(),
            route,
        ))
    }

    pub(in crate::root) fn publish_gateway_desktop_state(&self) {
        self.publish_gateway_private_progress();
        if self.view_session.is_none() {
            self.private_asset_presentation_cache.borrow_mut().clear();
        }
        self.gateway
            .drafts
            .borrow_mut()
            .reconcile_wallet(self.view_session.as_ref(), self.active_wallet_generation);
        // Keep effective networking visible to cohort retirement even while the vault is locked.
        let mut network = GatewayWalletState::default();
        network.set_rpc_context(self.http.clone(), &self.effective_chain_configs);
        let mut previous_network = self.gateway.request_network.borrow_mut();
        if previous_network
            .as_ref()
            .is_some_and(|previous| !previous.same_authority(&network))
        {
            self.gateway.retire_unlock();
        }
        *previous_network = Some(network);
        drop(previous_network);
        self.public_balance_cache.reconcile_scopes(
            self.effective_chain_configs
                .keys()
                .filter_map(|&chain_id| self.current_public_balance_scope(chain_id))
                .collect(),
        );
        for scope in self
            .effective_chain_configs
            .keys()
            .filter_map(|&chain_id| self.current_public_balance_scope(chain_id))
        {
            self.public_transaction_tracker.reconcile_scope(
                &scope,
                &self.public_accounts,
                &self.public_balance_cache,
            );
        }
        if let Some(state) = self.gateway.desktop_state.as_ref() {
            // Hardware startup can unlock metadata before installing a usable wallet view.
            let unlocked =
                matches!(self.vault_state, VaultState::ViewUnlocked) && self.view_session.is_some();
            // The gateway follows the last EVM chain while Tron is selected.
            let has_railgun = self
                .effective_chain_configs
                .railgun(self.last_evm_chain)
                .is_ok();
            let mut snapshot = GatewayWalletState {
                network_view: unlocked.then(|| self.gateway_network_view()),
                private_view_supported: true,
                private_actions_supported: unlocked && has_railgun,
                private_self_broadcast_supported: unlocked && has_railgun,
                private_view: unlocked.then(|| self.gateway_private_view()),
                wallet_selection_generation: self.wallet_switch_generation,
                wallet_transition: matches!(self.vault_state, VaultState::SwitchingWallet),
                public_view: if unlocked {
                    self.gateway_public_view()
                } else {
                    wallet_ops::gateway::GatewayPublicView::default()
                },
                waiting_unlock: self.gateway.unlock.borrow().state,
                view: if unlocked {
                    self.view_session.clone()
                } else {
                    None
                },
                active_wallet_generation: self.active_wallet_generation,
                public_balance_cache: self.public_balance_cache.clone(),
                public_transaction_tracker: self.public_transaction_tracker.clone(),
                public_accounts: if unlocked {
                    self.public_accounts.clone()
                } else {
                    Vec::new()
                },
                chain_ids: if unlocked {
                    self.effective_chain_configs.keys().copied().collect()
                } else {
                    Vec::new()
                },
                default_chain_id: unlocked.then_some(self.last_evm_chain),
                ..GatewayWalletState::default()
            };
            if unlocked {
                snapshot.native_usd_pricing =
                    self.public_broadcaster_anchor_cache.native_usd_statuses();
                snapshot.token_registry = Some(Arc::new(self.effective_token_registry.clone()));
                snapshot.set_rpc_context(self.http.clone(), &self.effective_chain_configs);
            }
            snapshot.wallet_switch = self.gateway_wallet_switch_transition(&snapshot);
            if let Some(handle) = &self.gateway.handle {
                handle.set_wallet_authority(snapshot.clone());
            }
            // Advance at publication, so coalescing lock/unlock notifications cannot reuse an epoch.
            let summaries = self.gateway_request_summaries();
            state.send_if_modified(|(previous, generation, previous_summaries)| {
                if previous.same_state(&snapshot) && *previous_summaries == summaries {
                    return false;
                }
                if !previous.same_authority(&snapshot) {
                    *generation = generation.wrapping_add(1);
                }
                *previous = snapshot;
                *previous_summaries = summaries;
                true
            });
        }
    }

    /// Resolve the accounts behind the current permissions once per snapshot,
    /// so permission rows can show labels and addresses without vault lookups
    /// during render. Permissions can reference accounts of another wallet,
    /// which are absent from `public_accounts`.
    pub(super) fn refresh_gateway_permission_accounts(&mut self) {
        let accounts = self
            .view_session
            .as_ref()
            .zip(self.vault_store.as_ref())
            .filter(|_| !self.gateway.snapshot.permissions.is_empty())
            .and_then(|(view_session, store)| store.list_all_public_accounts(view_session).ok());
        let Some(accounts) = accounts else {
            self.gateway.permission_accounts.clear();
            return;
        };
        self.gateway.permission_accounts = self
            .gateway
            .snapshot
            .permissions
            .iter()
            .filter_map(|permission| {
                let account = accounts.iter().find(|account| {
                    account.public_account_uuid == permission.public_account_uuid
                })?;
                Some((
                    permission.public_account_uuid.clone(),
                    GatewayPermissionAccount {
                        label: account.label.clone(),
                        address: account.address,
                    },
                ))
            })
            .collect();
    }
}
