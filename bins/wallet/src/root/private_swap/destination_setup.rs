#[allow(clippy::wildcard_imports)]
use super::*;

/// Whether a private asset row, or the header, can start a swap of `token`. `None` hides the
/// action: the chain has no swap profile. `Err` disables it with the reason.
pub(in crate::root) fn swap_entry_availability(
    profile: Option<&SwapProfile>,
    token: Option<Address>,
    spendable: bool,
    actions_available: bool,
) -> Option<Result<(), &'static str>> {
    use wallet_ops::settings::{SwapIneligibility, SwapTokenRole};
    let profile = profile?;
    Some(if actions_available {
        match token.map(|token| profile.token_eligibility(token, &SwapTokenRole::Sell)) {
            None | Some(SwapTokenEligibility::Ineligible(SwapIneligibility::NativeAsset)) => {
                Err("Native assets can't be swapped privately")
            }
            Some(SwapTokenEligibility::Ineligible(_)) => {
                Err("This token isn't supported for private swaps")
            }
            Some(SwapTokenEligibility::Eligible) if !spendable => {
                Err("No spendable private balance for this token")
            }
            Some(SwapTokenEligibility::Eligible) => Ok(()),
        }
    } else {
        Err("Available after wallet session starts")
    })
}

/// Whether a source use's saved approval has the swap set its destination account up, as
/// recorded by the destination account the approval binds.
pub(super) fn sets_up_destination(swap_use: &SwapUseRecord) -> bool {
    swap_use.approval().is_some_and(|approval| {
        approval
            .accounts
            .and_then(|accounts| accounts.destination)
            .is_some_and(|account| account.setup)
    })
}

/// The destination stealth accounts on `chain_id` whose shield a verified fill of the swap
/// account `operation` ran while their recorded execution nonce hasn't passed it. Read from
/// both owners' records, without a network request.
pub(super) fn delivered_destinations(
    origin: &ExecutorOwner,
    destination: &ExecutorOwner,
    operation: ExecutorOperationId,
    chain_id: u64,
) -> Vec<ExecutorOperationId> {
    let record = origin.records().ok().and_then(|records| {
        records
            .into_iter()
            .find(|record| record.operation() == operation)
    });
    let (Some(record), Ok(accounts)) = (record, destination.records()) else {
        return Vec::new();
    };
    let orders = record
        .swap()
        .map_or(&[][..], wallet_ops::vault::SwapOperationRecord::orders);
    orders
        .iter()
        .filter(|order| {
            matches!(
                order.observations().bridge_outcome,
                Some(SwapBridgeOutcome::DeliveredVerified { shielded: true, .. })
            ) && matches!(
                order.delivery(),
                SwapDelivery::Bridge(delivery) if delivery.destination_chain == chain_id
            )
        })
        .filter_map(|order| {
            let swap_use = order
                .use_id()
                .unwrap_or_else(|| SwapUseId::first(operation));
            let (_, account) = model::swap_use_destination(&record, swap_use)?;
            accounts
                .iter()
                .find(|candidate| candidate.operation() == account)
                .filter(|candidate| has_unpassed_shield(candidate))
                .map(ExecutorRecord::operation)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Whether a destination shield `record`'s account signed sits at or above its recorded
/// execution nonce, or the record has none, so the records can't tell that the shield ran.
fn has_unpassed_shield(record: &ExecutorRecord) -> bool {
    let observed = record
        .nonce_observation()
        .map(ExecutorNonceObservation::nonce);
    record.issued().iter().any(|payload| {
        payload.purpose() == ExecutorPayloadPurpose::SwapDestinationShield
            && observed.is_none_or(|nonce| nonce <= payload.nonce())
    })
}

/// Whether `record`'s setup is recorded as executed for the accepted delegate of `chain_id`,
/// the record's own network.
fn setup_recorded_executed_on(chain_id: u64, record: &ExecutorRecord) -> bool {
    ExecutorProfile::accepted(chain_id, record.delegate())
        .is_some_and(|profile| swap_setup_recorded_executed(record, profile))
}

/// The destination record and the freshness reported by its matching claim and saved
/// source approval. Callers choose which record is available and how missing claims fall back.
pub(super) struct DestinationAccountMetadata<'a> {
    pub(super) record: Option<&'a ExecutorRecord>,
    claimed_fresh: Option<bool>,
    approved_fresh: Option<bool>,
}

impl DestinationAccountMetadata<'_> {
    /// A loaded record is authoritative for setup and review; without its claim, treat the
    /// account as fresh. Only an unloaded record falls back to the saved approval.
    pub(super) fn fresh_from_record_or_approval(&self) -> bool {
        if self.record.is_some() {
            self.claimed_fresh.unwrap_or(true)
        } else {
            self.approved_fresh.unwrap_or(true)
        }
    }

    /// Cancellation retains the approval's account choice when the loaded record has no
    /// matching claim, so its explanation can still distinguish an existing account.
    pub(super) fn fresh_from_claim_or_approval(&self) -> bool {
        self.claimed_fresh.or(self.approved_fresh).unwrap_or(true)
    }
}

pub(super) fn destination_account_metadata<'a>(
    record: Option<&'a ExecutorRecord>,
    swap_use: Option<SwapUseId>,
    approval: Option<&SwapApproval>,
) -> DestinationAccountMetadata<'a> {
    DestinationAccountMetadata {
        record,
        claimed_fresh: swap_use
            .and_then(|id| record?.swap_use(id))
            .map(SwapUseRecord::is_fresh),
        approved_fresh: approval
            .and_then(|approval| approval.accounts?.destination)
            .map(|account| account.setup),
    }
}

/// The stage `record` gives on `chain_id`, its own network, with `setup` this session's last
/// observation of its setup and `attribution` its owner's evidence for it.
pub(super) fn account_stage(
    record: &ExecutorRecord,
    chain_id: u64,
    setup: Option<SwapSetupStatus>,
    submitting: bool,
    attribution: Option<&wallet_ops::ExecutorAttribution>,
) -> SwapStage {
    swap_stage(
        record,
        match setup {
            // A setup recorded as executed for the accepted delegate is ready without
            // this session reading the account, so `swap_stage` uses the recorded
            // outcome. That readiness can outlast a reorg until reconciliation; order
            // preparation checks the delegation afresh before anything is signed.
            None | Some(SwapSetupStatus::Pending)
                if setup_recorded_executed_on(chain_id, record) =>
            {
                None
            }
            // Any other stored execution waits for this session's account check, and a
            // failure that check reported stands.
            setup => setup.or(Some(SwapSetupStatus::Pending)),
        },
        submitting,
        attribution,
    )
}

/// How far one stealth account's setup is, apart from the other account of a private Bridge
/// swap. A setup job sends both setups, so only an account that isn't set up shows as
/// submitting.
pub(super) fn account_setup_progress(
    record: &ExecutorRecord,
    chain_id: u64,
    setup: Option<SwapSetupStatus>,
    submitting: bool,
) -> SwapSetupProgress {
    // No setup stage depends on what ran at a later nonce.
    let stage = account_stage(record, chain_id, setup, false, None);
    if submitting
        && matches!(
            stage,
            SwapStage::SetupNotSent | SwapStage::SetupPending | SwapStage::SetupFailed
        )
    {
        SwapSetupProgress::Submitting
    } else {
        model::swap_setup_progress(stage)
    }
}

/// The accounts among `records` whose pending setup private sync's `utxos` show in a
/// transaction recorded for it: a swap's setup before its order, or the swap's own setup an
/// order was placed on while the record leaves that setup unresolved.
pub(super) fn synced_setup_inclusions(
    records: &[ExecutorRecord],
    utxos: &[wallet_ops::UtxoOutput],
) -> BTreeSet<ExecutorOperationId> {
    records
        .iter()
        .filter(|record| {
            (record.swap().is_none() || wallet_ops::own_pending_swap_setup(record).is_some())
                && matches!(
                    model::swap_setup_confirmation(record, utxos),
                    Some(model::SwapSetupConfirmation::Included(_))
                )
        })
        .map(ExecutorRecord::operation)
        .collect()
}

/// Whether a setup was signed for the record's account, so its outcome can be read.
pub(super) fn has_issued_setup(record: &ExecutorRecord) -> bool {
    record
        .issued()
        .iter()
        .any(|payload| payload.purpose() == wallet_ops::vault::ExecutorPayloadPurpose::Operation)
}
