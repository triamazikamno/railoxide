use super::*;
use alloy::rpc::types::Log;
use eyre::eyre;

use crate::block_observer::resolve_source_transaction_by_block;
use crate::desktop::executors::Transfer;

pub(super) struct PreparedBlockedShieldRescuePlan {
    pub(super) plan: UnshieldPlan,
    pub(super) public_account_uuid: String,
}

pub async fn resolve_blocked_shield_rescue_eligibility(
    request: BlockedShieldRescueEligibilityRequest,
    http: &HttpContext,
) -> Result<BlockedShieldRescueEligibility> {
    let Some(snapshot) = request.session.handle.current_snapshot() else {
        return Ok(blocked_shield_rescue_disabled(
            "Wallet state is temporarily unavailable while synchronization resets.",
            None,
        ));
    };
    let Some(utxo) = blocked_shield_rescue_candidate_from_records(
        &snapshot.utxos,
        &snapshot.pending_overlay,
        &request.utxo_id,
    ) else {
        return Ok(blocked_shield_rescue_disabled(
            "Selected UTXO is not an unspent blocked Shield that can be refunded.",
            None,
        ));
    };

    let origin = match resolve_source_tx_origin(
        request.chain_id,
        &request.effective_chain,
        utxo.source.block_number,
        utxo.source.tx_hash,
        utxo.token_address(),
        utxo.note.value,
        http,
    )
    .await
    {
        Ok(origin) => origin,
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "resolve blocked Shield source origin failed");
            return Ok(blocked_shield_rescue_disabled(
                "Couldn't read the Shield transaction. Check the connection and try again.",
                None,
            ));
        }
    };

    // Inactive accounts are listed too, so an inactive origin is named as such.
    let accounts = request
        .vault_store
        .list_public_accounts_for_session(&request.view_session, true)
        .wrap_err("load public accounts")?;
    let eligibility = blocked_shield_rescue_eligibility_for_origin(Some(origin), &accounts);
    if eligibility.eligible {
        return Ok(eligibility);
    }
    let stealth_accounts: Vec<(Address, vault::ExecutorOperationId, u32)> = match request
        .session
        .executor_owner()
        .map(|owner| owner.records())
    {
        Some(Ok(records)) => records
            .iter()
            .filter_map(|record| Some((record.address()?, record.operation(), record.index())))
            .collect(),
        Some(Err(_)) => {
            tracing::warn!("read stealth accounts for blocked Shield origin failed");
            Vec::new()
        }
        None => Vec::new(),
    };
    Ok(blocked_shield_rescue_eligibility_for_resolved_origin(
        origin,
        &accounts,
        &stealth_accounts,
    ))
}

/// Resolve the account that funded a Shield: the refund origin of its blocked UTXO.
pub async fn resolve_source_tx_origin(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    source_block_number: u64,
    source_tx_hash: FixedBytes<32>,
    token: Address,
    value: U256,
    http: &HttpContext,
) -> Result<Address> {
    let chain = effective_desktop_chain_config(chain_id, effective_chain)?;
    // The delegate RelayAdapt7702 never holds tokens, so it is not a shared RelayAdapt here.
    let mut relay_adapts = vec![chain.relay_adapt_contract];
    relay_adapts.extend_from_slice(
        effective_chain
            .require_railgun()?
            .deployment
            .relay_adapt_history,
    );
    let query_rpc_pool = query_rpc_pool_with_http_client(chain.rpc_urls, http);
    let source =
        resolve_source_transaction_by_block(&query_rpc_pool, source_block_number, source_tx_hash)
            .await?;
    shield_funding_account(
        &source.logs,
        token,
        value,
        chain.railgun_contract,
        &relay_adapts,
        source.from,
    )
    .ok_or_else(|| eyre!("source transaction has ambiguous Shield funding accounts"))
}

/// The account the Railgun contract pulled a Shield's tokens from, read from the Shield
/// transaction's receipt logs. A shared `RelayAdapt` funder, or a receipt without a matching
/// Transfer, resolves to `from`. `None` means several accounts funded Shields of `token`
/// that `value` cannot tell apart.
pub(crate) fn shield_funding_account(
    logs: &[Log],
    token: Address,
    value: U256,
    railgun: Address,
    relay_adapts: &[Address],
    from: Address,
) -> Option<Address> {
    let transfers = logs
        .iter()
        .filter(|log| log.address() == token)
        .filter_map(|log| log.log_decode::<Transfer>().ok())
        .map(|log| log.inner.data)
        .filter(|transfer| transfer.to == railgun)
        .collect::<Vec<_>>();
    let Some(first) = transfers.first() else {
        return Some(from);
    };
    let funder = if transfers.iter().all(|transfer| transfer.from == first.from) {
        first.from
    } else {
        let mut matching = transfers
            .iter()
            .filter(|transfer| transfer.value == value)
            .map(|transfer| transfer.from);
        let funder = matching.next()?;
        if matching.any(|other| other != funder) {
            return None;
        }
        funder
    };
    Some(if relay_adapts.contains(&funder) {
        from
    } else {
        funder
    })
}

pub(crate) fn blocked_shield_rescue_candidate_from_records(
    utxos: &[WalletUtxo],
    pending_overlay: &WalletPendingOverlay,
    utxo_id: &BlockedShieldRescueUtxoId,
) -> Option<Utxo> {
    let chain_pending_spent_keys = chain_pending_spent_keys(pending_overlay);
    let active_poi_list_keys = default_active_poi_list_keys();
    utxos
        .iter()
        .filter(|entry| !entry.is_spent())
        .filter(|entry| {
            !chain_pending_spent_keys.contains(&(entry.utxo.tree, entry.utxo.position))
                && !pending_overlay
                    .local_pending_spent
                    .iter()
                    .any(|spent| spent.matches_local_utxo(entry))
        })
        .find(|entry| blocked_shield_rescue_utxo_matches(&entry.utxo, utxo_id))
        .filter(|entry| {
            utxos::activity_utxo_classification(&entry.utxo.poi, &active_poi_list_keys)
                == ActivityUtxoClassification::BlockedShield
        })
        .map(|entry| entry.utxo.clone())
}

pub(super) fn blocked_shield_rescue_utxo_matches(
    utxo: &Utxo,
    utxo_id: &BlockedShieldRescueUtxoId,
) -> bool {
    utxo.tree == utxo_id.tree
        && utxo.position == utxo_id.position
        && utxo.poi.commitment == utxo_id.commitment
        && utxo.poi.blinded_commitment == utxo_id.blinded_commitment
}

/// Eligibility against the wallet's Public accounts, active and inactive. An active account
/// at `origin` is eligible, an inactive one is named as the blocker, and any other origin
/// is unknown here.
pub(crate) fn blocked_shield_rescue_eligibility_for_origin(
    origin: Option<Address>,
    public_accounts: &[vault::PublicAccountMetadata],
) -> BlockedShieldRescueEligibility {
    let Some(origin) = origin else {
        return blocked_shield_rescue_disabled(
            "Couldn't read the Shield transaction. Check the connection and try again.",
            None,
        );
    };
    let account_with = |status: vault::PublicAccountStatus| {
        public_accounts
            .iter()
            .find(|account| account.address == origin && account.status == status)
    };
    let Some(account) = account_with(vault::PublicAccountStatus::Active) else {
        let blocker = account_with(vault::PublicAccountStatus::Inactive).map_or(
            BlockedShieldRescueBlocker::OriginUnknown,
            |account| BlockedShieldRescueBlocker::OriginInactive {
                public_account_uuid: account.public_account_uuid.clone(),
                label: account.label.clone(),
            },
        );
        return blocked_shield_rescue_blocked(blocker, origin);
    };

    BlockedShieldRescueEligibility {
        eligible: true,
        disabled_reason: None,
        blocker: None,
        origin_address: Some(origin),
        public_account_uuid: Some(account.public_account_uuid.clone()),
        public_account_label: account.label.clone(),
    }
}

/// Eligibility for a resolved origin. An origin that is not a Public account but is one of
/// the wallet's recorded stealth accounts, each given as its address, operation and index,
/// is blocked as that stealth account. An inactive Public account at the same address
/// stays the blocker.
pub(crate) fn blocked_shield_rescue_eligibility_for_resolved_origin(
    origin: Address,
    public_accounts: &[vault::PublicAccountMetadata],
    stealth_accounts: &[(Address, vault::ExecutorOperationId, u32)],
) -> BlockedShieldRescueEligibility {
    let eligibility = blocked_shield_rescue_eligibility_for_origin(Some(origin), public_accounts);
    if eligibility.blocker != Some(BlockedShieldRescueBlocker::OriginUnknown) {
        return eligibility;
    }
    let Some(&(_, operation, index)) = stealth_accounts
        .iter()
        .find(|(address, ..)| *address == origin)
    else {
        return eligibility;
    };
    blocked_shield_rescue_blocked(
        BlockedShieldRescueBlocker::OriginStealth { operation, index },
        origin,
    )
}

/// A refund that `blocker` holds back for the account at `origin`, with the sentence that
/// says so.
fn blocked_shield_rescue_blocked(
    blocker: BlockedShieldRescueBlocker,
    origin: Address,
) -> BlockedShieldRescueEligibility {
    let reason = match &blocker {
        BlockedShieldRescueBlocker::OriginUnknown => {
            "The Shield came from an account that isn't in this wallet.".to_owned()
        }
        BlockedShieldRescueBlocker::OriginStealth { index, .. } => {
            format!("The Shield came from stealth account #{index}, which isn't in Public.")
        }
        BlockedShieldRescueBlocker::OriginInactive {
            label: Some(label), ..
        } => {
            format!("The Shield came from Public account \"{label}\", which is inactive.")
        }
        BlockedShieldRescueBlocker::OriginInactive { label: None, .. } => {
            "The Shield came from a Public account that is inactive.".to_owned()
        }
    };
    BlockedShieldRescueEligibility {
        blocker: Some(blocker),
        ..blocked_shield_rescue_disabled(&reason, Some(origin))
    }
}

pub(super) fn blocked_shield_rescue_disabled(
    reason: &str,
    origin_address: Option<Address>,
) -> BlockedShieldRescueEligibility {
    BlockedShieldRescueEligibility {
        eligible: false,
        disabled_reason: Some(reason.to_string()),
        blocker: None,
        origin_address,
        public_account_uuid: None,
        public_account_label: None,
    }
}
