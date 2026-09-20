//! Local balance eligibility after shared RPC parsing and individual read admission.

use super::provider::GatewayWalletState;
use crate::public_wallet::{PublicAssetId, PublicBalanceAsset, PublicBalanceScope};
use crate::rpc_broker::BalanceRead;
use crate::vault::PublicAccountMetadata;
use alloy::primitives::U256;
use serde_json::Value;
use std::sync::Arc;
use tokio::time::Instant;

/// Field presence is only shortcut policy; the broker remains the ingress validator.
pub(super) fn permits_balance_shortcut(method: &str, params: &Value) -> bool {
    method != "eth_call"
        || params.as_array().is_some_and(|params| {
            params.len() <= 2
                && params
                    .first()
                    .and_then(Value::as_object)
                    .is_some_and(|call| {
                        call.keys()
                            .all(|field| matches!(field.as_str(), "to" | "data" | "input"))
                    })
        })
}

#[derive(Clone)]
pub(super) struct LocalBalanceAnswer {
    candidate: BalanceRead,
    scope: PublicBalanceScope,
    account: PublicAccountMetadata,
    asset: PublicBalanceAsset,
    amount: U256,
    generation: u64,
    namespace: Arc<()>,
}

impl LocalBalanceAnswer {
    pub(super) fn prepare(
        wallet: &GatewayWalletState,
        chain_id: u64,
        account: PublicAccountMetadata,
        candidate: BalanceRead,
        now: Instant,
    ) -> Option<Self> {
        if candidate.account() != account.address || !wallet.public_accounts.contains(&account) {
            return None;
        }
        let scope = PublicBalanceScope::new(
            wallet.view.as_ref()?.wallet_id().to_owned(),
            wallet.active_wallet_generation,
            wallet.http.as_ref()?.rpc_broker(),
            wallet.routes.get(&chain_id)?.clone(),
        );
        let asset = balance_asset(wallet, chain_id, candidate)?;
        let (amount, generation, namespace) = wallet.public_balance_cache.eligible_balance_state(
            &scope,
            &account,
            &asset,
            now.into_std(),
        )?;
        Some(Self {
            candidate,
            scope,
            account,
            asset,
            amount,
            generation,
            namespace,
        })
    }

    pub(super) fn is_current(
        &self,
        wallet: &GatewayWalletState,
        account: &PublicAccountMetadata,
        now: Instant,
    ) -> bool {
        account == &self.account
            && wallet.public_accounts.contains(&self.account)
            && balance_asset(wallet, self.scope.chain_id(), self.candidate).as_ref()
                == Some(&self.asset)
            && wallet
                .public_balance_cache
                .eligible_balance_state(&self.scope, &self.account, &self.asset, now.into_std())
                .is_some_and(|(amount, generation, namespace)| {
                    amount == self.amount
                        && generation == self.generation
                        && Arc::ptr_eq(&namespace, &self.namespace)
                })
    }

    pub(super) fn value(&self) -> Value {
        self.candidate.encode_result(self.amount)
    }
}

fn balance_asset(
    wallet: &GatewayWalletState,
    chain_id: u64,
    candidate: BalanceRead,
) -> Option<PublicBalanceAsset> {
    match candidate {
        BalanceRead::Native { .. } => {
            let native = wallet.native_currencies.get(&chain_id)?;
            Some(PublicBalanceAsset {
                id: PublicAssetId::Native,
                symbol: native.symbol.clone(),
                decimals: native.decimals,
            })
        }
        BalanceRead::Token { token, .. } => {
            let token_info = wallet.token_registry.as_ref()?.get(chain_id, &token)?;
            Some(PublicBalanceAsset {
                id: PublicAssetId::Erc20(token),
                symbol: token_info.symbol.clone(),
                decimals: token_info.decimals,
            })
        }
    }
}
