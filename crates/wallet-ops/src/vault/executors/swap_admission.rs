//! Whether a stealth account can take a swap use. The store's lock-held claim, the account
//! lists, quote previews and signing admission all judge an account here. Nothing is stored
//! about the result: every rule reads the record.

use broadcaster_core::contracts::cow::OrderUid;

use super::{
    Address, B256, ExecutorPayloadPurpose, ExecutorRecord, SwapBridgeOutcome, SwapDelivery,
    SwapDestinationOutcome, SwapOperationRecord, SwapOrderRecord, SwapUseId, SwapUseRole, U256,
};
use crate::settings::ExecutorProfile;

/// What a swap asks of an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapAccountRole {
    /// The account places the swap's orders.
    Source,
    /// The account receives `token` from a private Bridge swap and shields it.
    Destination { token: Address },
}

/// The swap use an account is judged for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapAccountUse {
    /// A swap the account takes no part in yet.
    New,
    /// The use that already claims the account: its first order or shield, or a retry.
    Claimed(SwapUseId),
    /// A second order of the use that already claims the account, signed while the use's
    /// latest order is still open, at that order's pre-hook nonce.
    SpeedUp(SwapSpeedUp),
}

/// The open order a speed-up was reviewed against. Planning, review, signing and the store's
/// write all name the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapSpeedUp {
    /// The swap use both orders are attempts of.
    pub swap_use: SwapUseId,
    /// The open order, which the new order's pre-hook invalidates.
    pub order: OrderUid,
    /// The nonce the open order's pre-hook was signed at, which the new pre-hook shares.
    pub pre_hook_nonce: U256,
}

/// What an account's work is judged from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapAdmissionEvidence {
    /// The last recorded outcomes, which a restart keeps. For account lists, quote previews and
    /// the lock-held claim. A pass never authorizes signing.
    Recorded,
    /// The record after reconciliation or the settled refresh, with its nonce observation.
    Fresh,
    /// The record with its nonce observation, as for `Fresh`, and a read of the account at the
    /// latest block that showed the accepted delegation at `nonce`. It is never recorded. It
    /// admits only an order and its speed-ups for the claimed fresh source use whose setup at
    /// the observed nonce is unresolved, with `nonce` the next one.
    Included { block: u64, nonce: U256 },
}

/// Why an account can't take a swap use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SwapAccountRefusal {
    #[error("stealth account is unavailable")]
    AddressUnknown,
    #[error("this swap executor's delegate is not supported")]
    UnsupportedDelegate,
    #[error("this account's setup is not confirmed; finish or retry its setup first")]
    SetupUnconfirmed,
    /// The account is retired or its swap setup was stopped.
    #[error("this account is retained for recovery or public use and cannot place a swap")]
    RecoveryOnly,
    #[error("this account is retained for recovery or public use and cannot place a swap")]
    RegisteredInPublic,
    /// Another swap's use claims the account and has not reached an order or a shield, or the
    /// use named is not the one that claims it.
    #[error("this account is reserved by another swap")]
    ClaimedByAnotherSwap,
    /// A payload the account signed can still execute.
    #[error("this account still has unfinished work; resolve it before starting a swap")]
    UnfinishedWork,
    #[error("the previous order of this swap can still execute; retry once it has ended")]
    PreviousOrderLive,
    /// The order named by a speed-up is not the swap's latest open order any more, or another
    /// attempt at its nonce traded.
    #[error("the order being sped up is no longer this swap's open order; review the swap again")]
    SpeedUpOrderChanged,
    /// A pre-hook at the shared nonce ran, or the account's nonce is not that nonce.
    #[error(
        "this swap's unshield already ran or its account changed, so its order can't be sped up"
    )]
    SpeedUpNonceUsed,
    /// Only an order paid from the private balance without a private Bridge delivery is sped up.
    #[error("this swap can't be sped up")]
    SpeedUpUnsupported,
    #[error("the previous swap's bridge hasn't delivered yet; retry once it has")]
    BridgeUndelivered,
    #[error("the previous swap's bridge is refunding to this account; recover the funds instead")]
    BridgeRefunding,
    #[error("the previous swap's bridge needs attention; check its status first")]
    BridgeNeedsAttention,
    #[error("the previous swap's proceeds are held on the destination network; recover them there")]
    BridgeHeldOnDestination,
    /// An earlier swap's fill left its token in this account without shielding it.
    #[error(
        "an earlier swap's proceeds are held in this account; recover them before using it for another swap"
    )]
    EarlierDeliveryHeld,
    /// A shield was signed for an earlier swap whose delivery has no shielded outcome. A
    /// consumed nonce does not say which payload ran or whether the bridge can still deliver.
    #[error(
        "an earlier swap's delivery to this account isn't resolved yet; check that swap's status first"
    )]
    EarlierDeliveryUnresolved,
    /// Fresh evidence needs the account's reconciled execution nonce.
    #[error("this account's current state isn't confirmed yet; try again")]
    StateUnverified,
    #[error(
        "this account already holds the token this swap delivers; recover that balance or choose another account"
    )]
    ReceivingBalance,
    #[error(
        "this account's balance of the token this swap delivers couldn't be read; try again or choose another account"
    )]
    ReceivingBalanceUnknown,
    /// The shield of the transaction is blocked, so its refund is still to do.
    #[error(
        "an earlier shield from this account was blocked; check its status or refund it before using the account again"
    )]
    EarlierShieldBlocked { transaction_hash: B256 },
    #[error(
        "an earlier shield from this account is awaiting its POI verdict; wait for it before using the account again"
    )]
    EarlierShieldPending { transaction_hash: B256 },
    /// The wallet's local notes hold nothing for the shield. Absence is not acceptance.
    #[error(
        "an earlier shield from this account isn't in the wallet's synced notes yet; wait for the wallet to sync it"
    )]
    EarlierShieldUnknown { transaction_hash: B256 },
}

impl SwapAccountRefusal {
    /// Whether observing the chain can lift the refusal. The others follow from the record's
    /// identity, restrictions and claim, which no observation changes. Nothing observed makes
    /// an order open again for a speed-up either.
    #[must_use]
    pub const fn awaits_observation(self) -> bool {
        !matches!(
            self,
            Self::AddressUnknown
                | Self::UnsupportedDelegate
                | Self::RecoveryOnly
                | Self::RegisteredInPublic
                | Self::ClaimedByAnotherSwap
                | Self::SpeedUpOrderChanged
                | Self::SpeedUpNonceUsed
                | Self::SpeedUpUnsupported
        )
    }
}

/// The stored reference to the notes of a shield an earlier destination use delivered: the
/// fill's transaction and the token it shielded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapEarlierShield {
    pub swap_use: SwapUseId,
    pub token: Address,
    pub transaction_hash: B256,
}

/// What the wallet's local notes say about an earlier shield.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapShieldVerdict {
    /// Every note of the shield is accepted on the required POI lists, or spent.
    Resolved,
    /// An unspent note of the shield is blocked.
    Blocked,
    /// An unspent note of the shield has no decided verdict on a required POI list.
    Pending,
    /// No local note belongs to the shield.
    Unknown,
}

/// Why `record`'s account can't take `swap_use` in `role`, judged from `evidence`.
///
/// A destination's receiving-token balance and its earlier shields' POI verdicts are not in
/// the record, so they are not judged here on either kind of evidence. Signing admission
/// reads them and judges them with [`swap_receiving_balance_refusal`] and
/// [`swap_earlier_shield_refusal`], over [`ExecutorRecord::earlier_swap_shields`].
#[must_use]
pub fn swap_account_refusal(
    record: &ExecutorRecord,
    chain_id: u64,
    role: SwapAccountRole,
    swap_use: SwapAccountUse,
    evidence: SwapAdmissionEvidence,
) -> Option<SwapAccountRefusal> {
    use SwapAccountRefusal as Refusal;
    if record.address.is_none() {
        return Some(Refusal::AddressUnknown);
    }
    if ExecutorProfile::accepted(chain_id, record.delegate).is_none() {
        return Some(Refusal::UnsupportedDelegate);
    }
    if record.public_account_uuid.is_some() {
        return Some(Refusal::RegisteredInPublic);
    }
    // A cancellation retires its account and leaves its swap. Such an account still places
    // orders, and the store refuses a shield from any retired account.
    let retired = match role {
        SwapAccountRole::Source => record.retired && record.swap.is_none(),
        SwapAccountRole::Destination { .. } => record.retired,
    };
    // A new source use can take the account of a cancelled fresh source preparation. Its
    // setup and every payload it signed must still resolve, which the rules below judge.
    let cancelled_source = role == SwapAccountRole::Source
        && swap_use == SwapAccountUse::New
        && cancelled_fresh_source(record);
    if retired || record.swap_setup_stopped && !cancelled_source {
        return Some(Refusal::RecoveryOnly);
    }
    let claimed = match swap_use {
        SwapAccountUse::New => {
            if !cancelled_source && !active_use_finished(record) {
                return Some(Refusal::ClaimedByAnotherSwap);
            }
            None
        }
        SwapAccountUse::Claimed(id) | SwapAccountUse::SpeedUp(SwapSpeedUp { swap_use: id, .. }) => {
            let fits = record
                .swap_use(id)
                .is_none_or(|claimed| match (&claimed.role, role) {
                    (SwapUseRole::Source { .. }, SwapAccountRole::Source) => true,
                    (
                        SwapUseRole::Destination {
                            destination_token, ..
                        }
                        | SwapUseRole::PublicSourceDestination {
                            destination_token, ..
                        },
                        SwapAccountRole::Destination { token },
                    ) => *destination_token == token,
                    _ => false,
                });
            if record.active_swap_use != Some(id) || !fits {
                return Some(Refusal::ClaimedByAnotherSwap);
            }
            Some(id)
        }
    };
    if evidence != SwapAdmissionEvidence::Recorded && record.nonce_observation.is_none() {
        return Some(Refusal::StateUnverified);
    }
    // Recorded evidence is also read while the claiming use still sets up the account it
    // reserved fresh. Fresh evidence comes after that setup.
    let own_setup = evidence == SwapAdmissionEvidence::Recorded
        && claimed.is_some_and(|id| record.swap_use(id).is_none_or(|claimed| claimed.fresh));
    // The setup the order follows, while the record leaves its nonce unresolved.
    let pending_setup = match role {
        SwapAccountRole::Source => claimed.and_then(|id| record.unresolved_swap_setup_nonce(id)),
        SwapAccountRole::Destination { .. } => None,
    };
    let next_to_setup =
        |nonce: U256| pending_setup.and_then(|setup| setup.checked_add(U256::ONE)) == Some(nonce);
    // Included evidence stands in only for the claimed fresh source use's own setup.
    let setup_confirmed = match evidence {
        SwapAdmissionEvidence::Recorded => own_setup || setup_executed(record),
        SwapAdmissionEvidence::Fresh => setup_executed(record),
        SwapAdmissionEvidence::Included { nonce, .. } => next_to_setup(nonce),
    };
    if !setup_confirmed {
        return Some(Refusal::SetupUnconfirmed);
    }
    match swap_use {
        SwapAccountUse::New | SwapAccountUse::Claimed(_) => {
            if let Some(swap) = record.swap.as_ref().filter(|swap| !swap.admits_attempt()) {
                return Some(
                    swap.orders()
                        .iter()
                        .find_map(|order| bridge_refusal(swap, order))
                        .unwrap_or(Refusal::PreviousOrderLive),
                );
            }
        }
        // A speed-up is judged against the one open order it names, not by the rule for a
        // retry, which that order's being live would refuse.
        SwapAccountUse::SpeedUp(speed_up) => {
            if role != SwapAccountRole::Source {
                return Some(Refusal::SpeedUpUnsupported);
            }
            let Some(swap) = &record.swap else {
                return Some(Refusal::SpeedUpOrderChanged);
            };
            if let Some(refusal) = swap.speed_up_refusal(&speed_up) {
                return Some(refusal);
            }
            // Another attempt's pre-hook can consume the shared nonce without this order's
            // own pre-hook being recorded as executed. Fresh and included evidence always
            // have the observation here. An order on an included setup holds the nonce after
            // the setup's, which the record leaves unresolved.
            let at_observed = record
                .nonce_observation
                .is_none_or(|observed| observed.nonce == speed_up.pre_hook_nonce);
            let nonce_current = match evidence {
                SwapAdmissionEvidence::Fresh => at_observed,
                SwapAdmissionEvidence::Recorded => {
                    at_observed || next_to_setup(speed_up.pre_hook_nonce)
                }
                SwapAdmissionEvidence::Included { nonce, .. } => {
                    nonce == speed_up.pre_hook_nonce && next_to_setup(nonce)
                }
            };
            if !nonce_current {
                return Some(Refusal::SpeedUpNonceUsed);
            }
        }
    }
    // Earlier shields need a completed delivery.
    let unresolved_delivery = record
        .swap_uses
        .iter()
        .filter(|earlier| Some(earlier.id) != claimed)
        .find_map(|earlier| match &earlier.role {
            SwapUseRole::Destination {
                shields, outcome, ..
            }
            | SwapUseRole::PublicSourceDestination {
                shields, outcome, ..
            } if !shields.is_empty() => match outcome {
                Some(SwapDestinationOutcome::Shielded { .. }) => None,
                Some(SwapDestinationOutcome::Held { .. }) => Some(Refusal::EarlierDeliveryHeld),
                Some(SwapDestinationOutcome::Unfilled) | None => {
                    Some(Refusal::EarlierDeliveryUnresolved)
                }
            },
            _ => None,
        });
    if unresolved_delivery.is_some() {
        return unresolved_delivery;
    }
    let setup_included = matches!(evidence, SwapAdmissionEvidence::Included { .. });
    if !own_setup && has_executable_work(record, claimed, setup_included) {
        return Some(Refusal::UnfinishedWork);
    }
    None
}

/// Why a destination with `balance` of its receiving token can't sign a shield. `None` is a
/// failed read. The shield takes the account's whole balance, so anything already there would
/// satisfy its guard early and be swept with the delivery.
#[must_use]
pub fn swap_receiving_balance_refusal(balance: Option<U256>) -> Option<SwapAccountRefusal> {
    match balance {
        Some(balance) if balance == U256::ZERO => None,
        Some(_) => Some(SwapAccountRefusal::ReceivingBalance),
        None => Some(SwapAccountRefusal::ReceivingBalanceUnknown),
    }
}

/// Why `shield`'s `verdict` keeps its account from serving as another swap's destination.
#[must_use]
pub const fn swap_earlier_shield_refusal(
    shield: &SwapEarlierShield,
    verdict: SwapShieldVerdict,
) -> Option<SwapAccountRefusal> {
    let transaction_hash = shield.transaction_hash;
    match verdict {
        SwapShieldVerdict::Resolved => None,
        SwapShieldVerdict::Blocked => {
            Some(SwapAccountRefusal::EarlierShieldBlocked { transaction_hash })
        }
        SwapShieldVerdict::Pending => {
            Some(SwapAccountRefusal::EarlierShieldPending { transaction_hash })
        }
        SwapShieldVerdict::Unknown => {
            Some(SwapAccountRefusal::EarlierShieldUnknown { transaction_hash })
        }
    }
}

impl ExecutorRecord {
    /// The nonce of the confirmed observation, when the use `id` reserved this account fresh
    /// as a source and a setup issued at that nonce is unresolved. An order of the use on that
    /// setup, once a read at the latest block shows it included, takes the next nonce. Setups
    /// belong to the use that reserved the account fresh, so each is an attempt of that use.
    #[must_use]
    pub(crate) fn unresolved_swap_setup_nonce(&self, id: SwapUseId) -> Option<U256> {
        let observed = self.nonce_observation?.nonce;
        let fresh_source = self.swap_use(id).is_none_or(|swap_use| {
            swap_use.fresh && matches!(swap_use.role, SwapUseRole::Source { .. })
        });
        (fresh_source
            && !self.nonce_resolved(observed)
            && self.issued.iter().any(|payload| {
                payload.purpose == ExecutorPayloadPurpose::Operation && payload.nonce == observed
            }))
        .then_some(observed)
    }

    /// The shields this account's destination uses delivered, besides the use `except`. Each
    /// names the notes whose POI verdict a later destination use waits for.
    #[must_use]
    pub fn earlier_swap_shields(&self, except: Option<SwapUseId>) -> Vec<SwapEarlierShield> {
        self.swap_uses
            .iter()
            .filter(|swap_use| Some(swap_use.id) != except)
            .filter_map(|swap_use| match &swap_use.role {
                SwapUseRole::Destination {
                    destination_token,
                    outcome:
                        Some(SwapDestinationOutcome::Shielded {
                            transaction_hash, ..
                        }),
                    ..
                }
                | SwapUseRole::PublicSourceDestination {
                    destination_token,
                    outcome:
                        Some(SwapDestinationOutcome::Shielded {
                            transaction_hash, ..
                        }),
                    ..
                } => Some(SwapEarlierShield {
                    swap_use: swap_use.id,
                    token: *destination_token,
                    transaction_hash: *transaction_hash,
                }),
                _ => None,
            })
            .collect()
    }
}

/// Whether the use that claims the account, if any, got as far as an order or a shield.
fn active_use_finished(record: &ExecutorRecord) -> bool {
    record.active_swap_use.is_none_or(|active| {
        record
            .swap_use(active)
            .is_some_and(|swap_use| match &swap_use.role {
                SwapUseRole::Source { .. } => record.has_swap_use_order(active),
                SwapUseRole::Destination { shields, .. }
                | SwapUseRole::PublicSourceDestination { shields, .. } => !shields.is_empty(),
            })
    })
}

/// Whether the use that claims the account reserved it fresh as a source and was stopped
/// before any order. Its claim and the account's stopped setup then hold only that use back.
fn cancelled_fresh_source(record: &ExecutorRecord) -> bool {
    record.active_use().is_some_and(|swap_use| {
        swap_use.fresh
            && swap_use.stopped
            && matches!(swap_use.role, SwapUseRole::Source { .. })
            && !record.has_swap_use_order(swap_use.id)
    })
}

/// Whether a setup's nonce is resolved. Fresh evidence has a nonce observation by now, and
/// recorded evidence reads the watermark that outlives it.
fn setup_executed(record: &ExecutorRecord) -> bool {
    record.issued.iter().any(|payload| {
        payload.purpose == ExecutorPayloadPurpose::Operation && record.nonce_resolved(payload.nonce)
    })
}

/// Whether a payload the account signed can still execute, besides the shields of the use
/// `claimed`, which a retry signs again at the same nonce. A destination shield resolves only
/// once the reconciled nonce passes its own. The order rule judges its own hooks; recovery
/// payloads must resolve independently of those orders. With `setup_included`, the claimed
/// use's unresolved setups are the included setup the order follows.
fn has_executable_work(
    record: &ExecutorRecord,
    claimed: Option<SwapUseId>,
    setup_included: bool,
) -> bool {
    // Historical swap orders do not account for a later recovery. These signatures can still
    // spend the account after its orderless claim released.
    let recovery_outstanding = record.issued.iter().any(|payload| {
        payload.purpose == ExecutorPayloadPurpose::Recovery && !record.nonce_resolved(payload.nonce)
    });
    if recovery_outstanding {
        return true;
    }
    let own_shields = claimed
        .and_then(|id| record.swap_use(id))
        .map_or(&[][..], |claimed| match &claimed.role {
            SwapUseRole::Destination { shields, .. }
            | SwapUseRole::PublicSourceDestination { shields, .. } => shields.as_slice(),
            SwapUseRole::Source { .. } => &[][..],
        });
    let shield_outstanding = record.issued.iter().any(|payload| {
        payload.purpose == ExecutorPayloadPurpose::SwapDestinationShield
            && !own_shields.contains(&payload.hash)
            && !record.swap_hook_nonce_passed(payload)
    });
    if shield_outstanding || record.swap.is_some() {
        return shield_outstanding;
    }
    record.issued.iter().any(|payload| {
        payload.purpose != ExecutorPayloadPurpose::SwapDestinationShield
            && !(setup_included && payload.purpose == ExecutorPayloadPurpose::Operation)
            && !record.nonce_resolved(payload.nonce)
    })
}

/// Why a Bridge order keeps its account from another swap, when its bridge is the reason.
/// The order that holds the hand-off answers, whether it traded itself or handed off the
/// trade of another attempt of its use.
fn bridge_refusal(
    swap: &SwapOperationRecord,
    order: &SwapOrderRecord,
) -> Option<SwapAccountRefusal> {
    let observed = order.observations();
    if observed.delivered.is_none()
        || !matches!(order.delivery(), SwapDelivery::Bridge(_))
        || observed.traded.is_none() && swap.trade_order(order).is_none()
    {
        return None;
    }
    match observed.bridge_outcome {
        None => Some(SwapAccountRefusal::BridgeUndelivered),
        Some(SwapBridgeOutcome::Refunding) => Some(SwapAccountRefusal::BridgeRefunding),
        Some(SwapBridgeOutcome::NeedsAttention) => Some(SwapAccountRefusal::BridgeNeedsAttention),
        Some(SwapBridgeOutcome::HeldOnDestination { .. }) => {
            Some(SwapAccountRefusal::BridgeHeldOnDestination)
        }
        Some(
            SwapBridgeOutcome::DeliveredVerified { .. }
            | SwapBridgeOutcome::DeliveredReported { .. },
        ) => None,
    }
}
