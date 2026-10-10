//! What a reused destination stealth account must show besides its record before it signs a
//! shield: an empty balance of the token it receives, and no earlier shield whose POI verdict
//! or refund is still open.
//!
//! The balance is one read of that token through the existing account inspection, on the
//! destination chain's owner and its network context. The verdicts come from the wallet's
//! local notes on that chain. Neither adds a POI request or polls an account for a list.

use alloy::primitives::{Address, U256};
use eyre::Result;
use poi::poi::default_active_poi_list_keys;

use crate::desktop::executor_discovery::inspect_for_signing;
use crate::utxos::utxo_ppoi_state;
use crate::vault::{
    ExecutorRecord, SwapEarlierShield, SwapShieldVerdict, SwapUseId, swap_earlier_shield_refusal,
    swap_receiving_balance_refusal,
};
use crate::{
    ExecutorAsset, ExecutorOwner, UtxoCommitmentKind, UtxoPpoiState, WalletSession, WalletUtxo,
};

/// The wallet's local notes on a destination chain, read for the verdicts of earlier shields.
pub trait SwapShieldNotes: Sync {
    /// The local notes `shield` created, spent ones included. `None` while the wallet has no
    /// snapshot of its notes.
    fn shield_notes(&self, shield: &SwapEarlierShield) -> Option<Vec<WalletUtxo>>;
}

impl SwapShieldNotes for WalletSession {
    fn shield_notes(&self, shield: &SwapEarlierShield) -> Option<Vec<WalletUtxo>> {
        let snapshot = self.handle.current_snapshot()?;
        Some(notes_of_shield(&snapshot.utxos, shield))
    }
}

/// The notes among `utxos` that `shield` created: shield commitments of its token from its
/// transaction.
pub(crate) fn notes_of_shield(utxos: &[WalletUtxo], shield: &SwapEarlierShield) -> Vec<WalletUtxo> {
    let token = U256::from_be_slice(shield.token.as_slice());
    utxos
        .iter()
        .filter(|entry| {
            entry.utxo.source.tx_hash == shield.transaction_hash
                && entry.utxo.note.token_hash == token
                && matches!(entry.utxo.poi.commitment_kind, UtxoCommitmentKind::Shield)
        })
        .cloned()
        .collect()
}

/// What the wallet's local notes say about `shield`. A spent note needs no refund. A note the
/// wallet doesn't hold proves nothing, and neither does a missing snapshot.
fn shield_verdict(
    notes: Option<&dyn SwapShieldNotes>,
    shield: &SwapEarlierShield,
) -> SwapShieldVerdict {
    let Some(created) = notes
        .and_then(|notes| notes.shield_notes(shield))
        .filter(|created| !created.is_empty())
    else {
        return SwapShieldVerdict::Unknown;
    };
    let lists = default_active_poi_list_keys();
    let mut pending = false;
    for entry in created.iter().filter(|entry| !entry.is_spent()) {
        match utxo_ppoi_state(&entry.utxo.poi, &lists) {
            UtxoPpoiState::Valid => {}
            UtxoPpoiState::ShieldBlocked => return SwapShieldVerdict::Blocked,
            UtxoPpoiState::Missing
            | UtxoPpoiState::ProofSubmitted
            | UtxoPpoiState::Unknown
            | UtxoPpoiState::Mixed => pending = true,
        }
    }
    if pending {
        SwapShieldVerdict::Pending
    } else {
        SwapShieldVerdict::Resolved
    }
}

/// Refuse the destination use `swap_use` of `record`'s account while a shield one of its
/// earlier uses delivered is blocked, awaits a verdict, or has no local note.
pub(super) fn require_earlier_shields_resolved(
    record: &ExecutorRecord,
    swap_use: SwapUseId,
    notes: Option<&dyn SwapShieldNotes>,
) -> Result<()> {
    for shield in record.earlier_swap_shields(Some(swap_use)) {
        if let Some(refusal) = swap_earlier_shield_refusal(&shield, shield_verdict(notes, &shield))
        {
            return Err(refusal.into());
        }
    }
    Ok(())
}

impl ExecutorOwner {
    /// Refuse a shield from the reused destination `executor` unless it holds none of `token`,
    /// read now at the chain tip. A failed read leaves the balance unknown, which also refuses.
    /// Only `token` is judged: native currency and other tokens don't fail this rule.
    pub(super) async fn require_empty_receiving_balance(
        &self,
        executor: Address,
        delegate: Address,
        token: Address,
    ) -> Result<()> {
        let asset = ExecutorAsset::Erc20(token);
        // Like every read of a recorded account, the nonce layout is its own delegate's.
        let chain = self
            .chain_for_delegate(delegate)
            .unwrap_or_else(|| self.chain.clone());
        let inspected = self
            .while_active(inspect_for_signing(&chain, &self.http, executor, &[asset]))
            .await;
        self.ensure_active()?;
        let balance = inspected
            .ok()
            .and_then(|(inspection, _)| inspection.balances().get(&asset).copied().flatten());
        swap_receiving_balance_refusal(balance).map_or(Ok(()), |refusal| Err(refusal.into()))
    }
}
