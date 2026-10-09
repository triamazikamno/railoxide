//! What ran at a resolved execution nonce.
//!
//! A consumed nonce says that none of the payloads signed at it can still execute, not which
//! of them did. This names the action that ran from the record and from evidence the caller
//! holds, and only from evidence that the action itself left. Missing evidence for one action
//! never names another, so the answer is often "resolved" with nothing named. Nothing here
//! reads the chain or is stored, and only labels depend on it.

use std::sync::Arc;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::{SolCall as _, SolValue};
use broadcaster_core::contracts::cow::OrderUid;
use broadcaster_core::contracts::railgun::{Call, RelayAdapt7702};
use railgun_wallet::WalletUtxo;
use sync_service::WalletCurrentSnapshot;

use super::ExecutorOwner;
use super::recovery::is_shield_of;
use super::swap::invalidates_order;
use crate::desktop::executor_observation::expected_shields;
use crate::vault::{
    ExecutorNonceWatermark, ExecutorPayloadPurpose, ExecutorRecord, IssuedExecutorPayload,
    SwapOrderRecord, SwapPreHookDeathCause,
};

/// What a caller knows about an account besides its record.
#[derive(Clone, Copy)]
pub struct ExecutorAttributionEvidence<'a> {
    /// The chain's Railgun contract, which a payload's shield calls are read against.
    pub railgun: Address,
    /// Private sync's snapshot. `None` while sync is unavailable.
    pub sync: Option<&'a WalletCurrentSnapshot>,
    /// Contract state of the account's swap orders. `None` when it was not read.
    pub invalidated_orders: Option<InvalidatedSwapOrders<'a>>,
}

impl ExecutorAttributionEvidence<'static> {
    /// The record alone: private sync is unavailable and no order state was read. The Railgun
    /// contract is read only against sync's notes, so none is named.
    #[must_use]
    pub const fn record_only() -> Self {
        Self {
            railgun: Address::ZERO,
            sync: None,
            invalidated_orders: None,
        }
    }
}

/// Swap orders that read as invalidated on chain while their pre-hook's nullifiers are
/// unspent, so the pre-hook did not run.
#[derive(Clone, Copy)]
pub struct InvalidatedSwapOrders<'a> {
    pub settlement: Address,
    pub orders: &'a [OrderUid],
}

/// What the owner knows about one account with no chain read, held by value so a caller can
/// keep it: private sync's current snapshot, and the account's swap orders whose recorded
/// pre-hook death names a cancellation or a recovery. Swap observation records such a cause
/// only for an order it read as invalidated with its pre-hook's nullifiers unspent, so the
/// record carries that read.
#[derive(Clone)]
pub struct ExecutorAttribution {
    railgun: Address,
    sync: Option<Arc<WalletCurrentSnapshot>>,
    settlement: Option<Address>,
    invalidated: Vec<OrderUid>,
}

impl ExecutorAttribution {
    /// The evidence, lent to [`attributed_action`] and [`payload_outcome`].
    #[must_use]
    pub fn evidence(&self) -> ExecutorAttributionEvidence<'_> {
        ExecutorAttributionEvidence {
            railgun: self.railgun,
            sync: self.sync.as_deref(),
            invalidated_orders: self.settlement.map(|settlement| InvalidatedSwapOrders {
                settlement,
                orders: &self.invalidated,
            }),
        }
    }
}

#[cfg(feature = "test-support")]
impl ExecutorAttribution {
    /// Evidence for UI tests, whose sessions don't sync: private sync showing the note of
    /// each of `shields`, received in the block given with it.
    #[must_use]
    pub fn with_shields_for_tests(
        railgun: Address,
        shields: &[(broadcaster_core::contracts::railgun::ShieldRequest, u64)],
    ) -> Self {
        use railgun_wallet::{Utxo, UtxoCommitmentKind, UtxoSource};

        let utxos = shields
            .iter()
            .zip(0..)
            .map(|((request, block), position)| {
                WalletUtxo::new(Utxo::new(
                    request.preimage.note_with_random([0; 16]),
                    0,
                    position,
                    UtxoSource {
                        tx_hash: B256::ZERO,
                        block_number: *block,
                        block_timestamp: 0,
                    },
                    UtxoCommitmentKind::Shield,
                ))
            })
            .collect::<Vec<_>>();
        Self {
            railgun,
            sync: Some(WalletCurrentSnapshot::new(
                shields.iter().map(|(_, block)| *block).max().unwrap_or(0),
                0,
                0,
                utxos,
                sync_service::WalletPendingOverlay::default(),
            )),
            settlement: None,
            invalidated: Vec::new(),
        }
    }
}

impl ExecutorOwner {
    /// The evidence that attributes `record`'s resolved nonces, from private sync and the
    /// record alone. `None` on a chain without Railgun.
    #[must_use]
    pub fn attribution(&self, record: &ExecutorRecord) -> Option<ExecutorAttribution> {
        let railgun = self.chain.railgun.as_ref()?.deployment.contract;
        let invalidated = record.swap().map_or_else(Vec::new, |swap| {
            swap.orders()
                .iter()
                .filter(|order| {
                    order.observations().pre_hook_dead.is_some_and(|death| {
                        matches!(
                            death.cause,
                            SwapPreHookDeathCause::Cancellation | SwapPreHookDeathCause::Recovery
                        )
                    })
                })
                .map(SwapOrderRecord::uid)
                .collect()
        });
        Some(ExecutorAttribution {
            railgun,
            sync: self.synced_snapshot(),
            settlement: self
                .chain
                .swap_profile()
                .map(|profile| profile.settlement()),
            invalidated,
        })
    }
}

/// The action that ran at a nonce: every signed payload that is a fee round of it. Fee rounds
/// reuse one set of prepared calls, so nothing tells them apart and none is singled out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorAttributedAction {
    purpose: ExecutorPayloadPurpose,
    payloads: Vec<B256>,
}

impl ExecutorAttributedAction {
    #[must_use]
    pub const fn purpose(&self) -> ExecutorPayloadPurpose {
        self.purpose
    }
    #[must_use]
    pub fn payloads(&self) -> &[B256] {
        &self.payloads
    }
}

/// What became of one signed payload, from its nonce and the action attributed at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorPayloadOutcome {
    /// The payload's nonce is not known to be consumed.
    Pending,
    /// The payload's action is the one that ran at its nonce.
    Executed,
    /// Another action ran at the payload's nonce.
    Superseded,
    /// The nonce is consumed and no evidence names what ran.
    Resolved,
}

/// One action signed for an account, with what became of it: every fee round of one set of
/// prepared calls at one nonce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorSignedAction {
    nonce: U256,
    purpose: ExecutorPayloadPurpose,
    payloads: Vec<B256>,
    outcome: ExecutorPayloadOutcome,
    spend_block: Option<u64>,
}

impl ExecutorSignedAction {
    #[must_use]
    pub const fn nonce(&self) -> U256 {
        self.nonce
    }
    #[must_use]
    pub const fn purpose(&self) -> ExecutorPayloadPurpose {
        self.purpose
    }
    /// The action's fee rounds, in the order they were signed.
    #[must_use]
    pub fn payloads(&self) -> &[B256] {
        &self.payloads
    }
    #[must_use]
    pub const fn outcome(&self) -> ExecutorPayloadOutcome {
        self.outcome
    }
    /// The block private sync reports for the action's spend, in a transaction recorded for
    /// one of its payloads. `None` when sync shows no such spend.
    #[must_use]
    pub const fn spend_block(&self) -> Option<u64> {
        self.spend_block
    }
}

/// Every action signed for `record`, by nonce and then in signing order, with the outcome
/// [`payload_outcome`] gives each of its payloads.
#[must_use]
pub fn signed_actions(
    record: &ExecutorRecord,
    evidence: &ExecutorAttributionEvidence<'_>,
) -> Vec<ExecutorSignedAction> {
    let nonces = record
        .issued()
        .iter()
        .map(IssuedExecutorPayload::nonce)
        .collect::<std::collections::BTreeSet<_>>();
    let mut signed = Vec::new();
    for nonce in nonces {
        let actions = actions_at(record, nonce);
        let resolved = record.nonce_resolved(nonce);
        let named = resolved
            .then(|| named_action(record, nonce, &actions, evidence))
            .flatten()
            .map(|action| action.payloads[0].hash());
        signed.extend(actions.iter().map(|action| {
            ExecutorSignedAction {
                nonce,
                purpose: action.purpose,
                payloads: action
                    .payloads
                    .iter()
                    .map(|payload| payload.hash())
                    .collect(),
                outcome: match named {
                    Some(named) if named == action.payloads[0].hash() => {
                        ExecutorPayloadOutcome::Executed
                    }
                    Some(_) => ExecutorPayloadOutcome::Superseded,
                    None if resolved => ExecutorPayloadOutcome::Resolved,
                    None => ExecutorPayloadOutcome::Pending,
                },
                spend_block: evidence.sync.and_then(|sync| {
                    action
                        .payloads
                        .iter()
                        .flat_map(|payload| recorded_spends(payload, &sync.utxos))
                        .min()
                }),
            }
        }));
    }
    signed
}

/// Payloads at one nonce with the same purpose and the same decoded calls.
struct Action<'a> {
    purpose: ExecutorPayloadPurpose,
    calls: Vec<Call>,
    /// The encoded calls. `None` for calldata that does not decode, which is its own action.
    identity: Option<Vec<u8>>,
    payloads: Vec<&'a IssuedExecutorPayload>,
}

/// The outcome of the issued payload `hash`, or `None` when the record holds no such payload.
#[must_use]
pub fn payload_outcome(
    record: &ExecutorRecord,
    hash: B256,
    evidence: &ExecutorAttributionEvidence<'_>,
) -> Option<ExecutorPayloadOutcome> {
    let payload = record
        .issued()
        .iter()
        .find(|payload| payload.hash() == hash)?;
    if !record.nonce_resolved(payload.nonce()) {
        return Some(ExecutorPayloadOutcome::Pending);
    }
    Some(match attributed_action(record, payload.nonce(), evidence) {
        Some(action) if action.payloads.contains(&hash) => ExecutorPayloadOutcome::Executed,
        Some(_) => ExecutorPayloadOutcome::Superseded,
        None => ExecutorPayloadOutcome::Resolved,
    })
}

/// The action that ran at `nonce`. `None` while the nonce is not resolved, and when it is
/// resolved and no evidence names an action.
#[must_use]
pub fn attributed_action(
    record: &ExecutorRecord,
    nonce: U256,
    evidence: &ExecutorAttributionEvidence<'_>,
) -> Option<ExecutorAttributedAction> {
    if !record.nonce_resolved(nonce) {
        return None;
    }
    let actions = actions_at(record, nonce);
    named_action(record, nonce, &actions, evidence).map(|action| ExecutorAttributedAction {
        purpose: action.purpose,
        payloads: action
            .payloads
            .iter()
            .map(|payload| payload.hash())
            .collect(),
    })
}

/// The first signal that names an action decides, in the order of signals that cannot
/// change later. Evidence for two actions under one signal names nothing.
fn named_action<'a, 'r>(
    record: &'r ExecutorRecord,
    nonce: U256,
    actions: &'a [Action<'r>],
    evidence: &ExecutorAttributionEvidence<'_>,
) -> Option<&'a Action<'r>> {
    // A hook runs inside a solver's settlement and has no transaction recorded for it. The
    // swap's own observations are its evidence.
    if let Some(hook) = record.resolved_swap_hook(nonce) {
        return actions
            .iter()
            .find(|action| action.payloads.iter().any(|payload| payload.hash() == hook));
    }
    if let [action] = actions {
        return Some(action);
    }
    if let Some(sync) = evidence.sync {
        let mut spent = actions
            .iter()
            .filter(|action| recorded_spend(action, &sync.utxos));
        match (spent.next(), spent.next()) {
            (Some(action), None) => return Some(action),
            (Some(_), Some(_)) => return None,
            _ => {}
        }
        if let Some(executor) = record.address() {
            let mut shielded = actions
                .iter()
                .filter(|action| shield_received(action, executor, evidence.railgun, &sync.utxos));
            match (shielded.next(), shielded.next()) {
                (Some(action), None) => return Some(action),
                (Some(_), Some(_)) => return None,
                _ => {}
            }
        }
    }
    invalidating_recovery(record, nonce, actions, evidence.invalidated_orders?)
}

fn actions_at(record: &ExecutorRecord, nonce: U256) -> Vec<Action<'_>> {
    let mut actions: Vec<Action<'_>> = Vec::new();
    for payload in record
        .issued()
        .iter()
        .filter(|payload| payload.nonce() == nonce)
    {
        let calls = payload_calls(payload);
        let identity = calls.as_ref().map(SolValue::abi_encode);
        if let Some(action) = actions.iter_mut().find(|action| {
            identity.is_some() && action.purpose == payload.purpose() && action.identity == identity
        }) {
            action.payloads.push(payload);
        } else {
            actions.push(Action {
                purpose: payload.purpose(),
                calls: calls.unwrap_or_default(),
                identity,
                payloads: vec![payload],
            });
        }
    }
    actions
}

pub(super) fn payload_calls(payload: &IssuedExecutorPayload) -> Option<Vec<Call>> {
    let data = payload.context().calldata();
    RelayAdapt7702::executeCall::abi_decode(data)
        .map(|call| call._actionData.calls)
        .or_else(|_| RelayAdapt7702::multicallCall::abi_decode(data).map(|call| call._calls))
        .ok()
}

/// The watermark that private sync's own evidence gives `record`, with no chain read: one
/// past the highest nonce of a pending payload that `sync` shows executed at or below
/// `safe_head`, with the block of that evidence. A payload executed when one of its notes is
/// spent in a transaction recorded for it, or when a shield it requested is received. A
/// payload with neither, as an operation after a lost broadcaster reply, stays pending.
pub(super) fn synced_resolution(
    record: &ExecutorRecord,
    railgun: Address,
    sync: &WalletCurrentSnapshot,
    safe_head: Option<u64>,
) -> Option<ExecutorNonceWatermark> {
    record
        .issued()
        .iter()
        .filter(|payload| !record.nonce_resolved(payload.nonce()))
        .filter_map(|payload| {
            let shields = record.address().map_or_else(Vec::new, |executor| {
                let calls = payload_calls(payload).unwrap_or_default();
                received_shields(&calls, executor, railgun, &sync.utxos)
            });
            let block = recorded_spends(payload, &sync.utxos)
                .chain(shields)
                .filter(|block| safe_head.is_none_or(|head| *block <= head))
                .min()?;
            Some(ExecutorNonceWatermark::new(
                payload.nonce().saturating_add(U256::ONE),
                block,
            ))
        })
        .max_by_key(|watermark| watermark.nonce())
}

/// A note of one of the action's payloads is spent in a transaction recorded for that
/// payload. A payload's spend sits inside its own execute call, so that payload ran. Spent
/// notes with any other hash prove nothing: a superseded payload's notes are released and
/// anything can spend them later.
fn recorded_spend(action: &Action<'_>, utxos: &[WalletUtxo]) -> bool {
    action
        .payloads
        .iter()
        .any(|payload| recorded_spends(payload, utxos).next().is_some())
}

/// The blocks where a note of `payload` is spent in a transaction recorded for it.
fn recorded_spends<'a>(
    payload: &'a IssuedExecutorPayload,
    utxos: &'a [WalletUtxo],
) -> impl Iterator<Item = u64> + 'a {
    utxos.iter().filter_map(move |entry| {
        let spent = entry.spent.as_ref()?;
        (payload.transaction_hashes().contains(&spent.tx_hash)
            && payload
                .context()
                .inputs()
                .iter()
                .any(|input| input.matches(&entry.utxo)))
        .then_some(spent.block_number)
    })
}

/// A shield the action requested is received. Each request is salted, so its note names it.
fn shield_received(
    action: &Action<'_>,
    executor: Address,
    railgun: Address,
    utxos: &[WalletUtxo],
) -> bool {
    !received_shields(&action.calls, executor, railgun, utxos).is_empty()
}

/// The blocks where a shield that `calls` request is received.
fn received_shields(
    calls: &[Call],
    executor: Address,
    railgun: Address,
    utxos: &[WalletUtxo],
) -> Vec<u64> {
    let Ok(requests) = expected_shields(executor, railgun, calls) else {
        return Vec::new();
    };
    utxos
        .iter()
        .filter(|entry| {
            requests
                .iter()
                .any(|request| is_shield_of(&entry.utxo, request))
        })
        .map(|entry| entry.utxo.source.block_number)
        .collect()
}

/// The one recovery at a pre-hook's nonce whose calls invalidate that pre-hook's order, for
/// an order the caller read as invalidated. A resolved payload at another nonce that
/// invalidates the same order could have set that state instead, so it names nothing.
fn invalidating_recovery<'a, 'r>(
    record: &'r ExecutorRecord,
    nonce: U256,
    actions: &'a [Action<'r>],
    invalidated: InvalidatedSwapOrders<'_>,
) -> Option<&'a Action<'r>> {
    let mut named: Option<usize> = None;
    for order in record.swap()?.orders() {
        let uid = order.uid();
        if order.pre_hook().nonce() != nonce
            || !invalidated.orders.contains(&uid)
            || record.issued().iter().any(|payload| {
                payload.nonce() != nonce
                    && record.nonce_resolved(payload.nonce())
                    && payload_calls(payload)
                        .is_some_and(|calls| invalidates_order(&calls, invalidated.settlement, uid))
            })
        {
            continue;
        }
        for (index, action) in actions.iter().enumerate() {
            if action.purpose != ExecutorPayloadPurpose::Recovery
                || !invalidates_order(&action.calls, invalidated.settlement, uid)
            {
                continue;
            }
            if named.is_some_and(|named| named != index) {
                return None;
            }
            named = Some(index);
        }
    }
    named.map(|index| &actions[index])
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;

    use alloy::eips::BlockNumHash;
    use alloy::primitives::{Bytes, FixedBytes, Uint};
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::cow::invalidate_order_calldata;
    use broadcaster_core::contracts::railgun::{
        CommitmentPreimage, RelayAdapt7702ActionData, ShieldCiphertext, ShieldRequest, TokenData,
        shieldCall,
    };
    use railgun_wallet::{Utxo, UtxoCommitmentKind, UtxoSource};
    use sync_service::WalletPendingOverlay;

    use super::ExecutorPayloadOutcome::{Executed, Resolved, Superseded};
    use super::*;
    use crate::vault::ExecutorPayloadPurpose::{Operation, Recovery, SwapPreHook};
    use crate::vault::{
        ExecutorInputIdentity, ExecutorNonceObservation, ExecutorNonceWatermark,
        ExecutorOperationId, ExecutorPayloadContext, SwapDelivery, SwapObservation,
        SwapOrderObservations, SwapProof, SwapRecipient, SwapTerms,
    };

    const EXECUTOR: Address = Address::repeat_byte(1);
    pub(crate) const RAILGUN: Address = Address::repeat_byte(3);
    pub(crate) const SETTLEMENT: Address = Address::repeat_byte(4);
    const ORDER: OrderUid = OrderUid(FixedBytes::repeat_byte(8));
    /// The block of the read that resolved the record's nonces.
    pub(crate) const RESOLVED_AT: u64 = 20;

    fn stored(issued: Vec<serde_json::Value>, consumed: u64) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "derivation": "Railgun7702V1",
            "origin": "Reserved",
            "operation": ExecutorOperationId::random().unwrap(),
            "index": 0,
            "address": EXECUTOR,
            "delegate": Address::repeat_byte(2),
            "retired": true,
            "issued": serde_json::Value::Array(issued),
            "nonce_watermark": ExecutorNonceWatermark::new(U256::from(consumed), RESOLVED_AT),
        })
    }

    pub(crate) fn record(issued: Vec<serde_json::Value>, consumed: u64) -> ExecutorRecord {
        serde_json::from_value(stored(issued, consumed)).unwrap()
    }

    /// A swap whose one order has its pre-hook, payload 6, at nonce 1.
    fn swap_record(
        issued: Vec<serde_json::Value>,
        consumed: u64,
        observations: &SwapOrderObservations,
    ) -> ExecutorRecord {
        let mut saved = stored(issued, consumed);
        saved["swap"] = serde_json::json!({
            "terms": SwapTerms::new(
                Address::repeat_byte(5),
                Address::repeat_byte(6),
                SwapRecipient::new(U256::ONE, [0; 32]),
                B256::repeat_byte(9),
            ),
            "proof": SwapProof::new(B256::repeat_byte(9), Vec::new()),
            "orders": [{
                "attempt": 0, "uid": ORDER.0,
                "delivery": SwapDelivery::External { receiver: Address::repeat_byte(7) },
                "bounds": {
                    "sell_amount": U256::ONE, "buy_amount": U256::ONE,
                    "private_minimum": U256::ONE, "shield_fee_bps": U256::ZERO,
                    "slippage_bps": 0, "pre_hook_gas_limit": 0, "post_hook_gas_limit": 0,
                    "anchors": [],
                },
                "pre_hook": { "nonce": U256::ONE, "payload": B256::repeat_byte(6) },
                "post_hook": null, "invalidates": null, "observations": observations,
            }],
        });
        serde_json::from_value(saved).unwrap()
    }

    pub(crate) fn execute(nonce: u64, calls: Vec<Call>) -> RelayAdapt7702::executeCall {
        RelayAdapt7702::executeCall {
            _transactions: Vec::new(),
            _actionData: RelayAdapt7702ActionData {
                requireSuccess: true,
                minGasLimit: U256::ZERO,
                calls,
            },
            _nonce: U256::from(nonce),
            _signature: Bytes::default(),
        }
    }

    fn multicall(nonce: u64, calls: Vec<Call>) -> RelayAdapt7702::multicallCall {
        RelayAdapt7702::multicallCall {
            _requireSuccess: true,
            _calls: calls,
            _nonce: U256::from(nonce),
            _signature: Bytes::default(),
        }
    }

    pub(crate) fn payload(
        nonce: u64,
        hash: u8,
        purpose: ExecutorPayloadPurpose,
        call: &impl SolCall,
        inputs: &[&WalletUtxo],
        transactions: &[B256],
    ) -> serde_json::Value {
        let mut payload = serde_json::to_value(IssuedExecutorPayload::new(
            U256::from(nonce),
            Address::repeat_byte(2),
            B256::repeat_byte(hash),
            purpose,
            ExecutorPayloadContext::new(
                call.abi_encode().into(),
                ExecutorNonceObservation::new(
                    BlockNumHash::new(10, B256::repeat_byte(10)),
                    U256::from(nonce),
                ),
                inputs
                    .iter()
                    .map(|input| ExecutorInputIdentity::from_utxo(&input.utxo))
                    .collect(),
            ),
        ))
        .unwrap();
        payload["transaction_hashes"] = serde_json::json!(transactions);
        payload
    }

    /// A shield request; `salt` stands for the random key that makes each request its own.
    pub(crate) fn shield(salt: u8) -> ShieldRequest {
        ShieldRequest {
            preimage: CommitmentPreimage {
                npk: B256::repeat_byte(salt),
                token: TokenData::erc20(Address::repeat_byte(5)),
                value: Uint::from(100),
            },
            ciphertext: ShieldCiphertext {
                encryptedBundle: [B256::ZERO; 3],
                shieldKey: B256::ZERO,
            },
        }
    }

    pub(crate) fn shield_call(request: &ShieldRequest) -> Call {
        Call {
            to: EXECUTOR,
            data: shieldCall {
                _shieldRequests: vec![request.clone()],
            }
            .abi_encode()
            .into(),
            value: U256::ZERO,
        }
    }

    fn cancellation_call() -> Call {
        Call {
            to: SETTLEMENT,
            data: invalidate_order_calldata(&ORDER),
            value: U256::ZERO,
        }
    }

    fn source(transaction: u8, block: u64) -> UtxoSource {
        UtxoSource {
            tx_hash: B256::repeat_byte(transaction),
            block_number: block,
            block_timestamp: 0,
        }
    }

    /// An unspent note a payload can spend.
    pub(crate) fn note(position: u64) -> WalletUtxo {
        WalletUtxo::new(Utxo::new(
            shield(9).preimage.note_with_random([0; 16]),
            0,
            position,
            source(40, 5),
            UtxoCommitmentKind::Transact,
        ))
    }

    pub(crate) fn spent(mut note: WalletUtxo, transaction: u8, block: u64) -> WalletUtxo {
        note.spent = Some(source(transaction, block));
        note
    }

    /// The note `request` created, less the protocol fee.
    pub(crate) fn shielded(request: &ShieldRequest) -> WalletUtxo {
        let mut received = request.preimage.note_with_random([0; 16]);
        received.value = U256::from(99);
        WalletUtxo::new(Utxo::new(
            received,
            0,
            50,
            source(41, 15),
            UtxoCommitmentKind::Shield,
        ))
    }

    pub(crate) fn synced(last_scanned: u64, utxos: Vec<WalletUtxo>) -> Arc<WalletCurrentSnapshot> {
        WalletCurrentSnapshot::new(last_scanned, 0, 0, utxos, WalletPendingOverlay::default())
    }

    fn outcomes(
        record: &ExecutorRecord,
        sync: Option<&Arc<WalletCurrentSnapshot>>,
        invalidated: &[OrderUid],
        payloads: &[u8],
    ) -> Vec<ExecutorPayloadOutcome> {
        let evidence = ExecutorAttributionEvidence {
            railgun: RAILGUN,
            sync: sync.map(Arc::as_ref),
            invalidated_orders: Some(InvalidatedSwapOrders {
                settlement: SETTLEMENT,
                orders: invalidated,
            }),
        };
        payloads
            .iter()
            .map(|hash| payload_outcome(record, B256::repeat_byte(*hash), &evidence).unwrap())
            .collect()
    }

    /// An operation at nonce 0, payload 1, that spends `input`.
    fn operation(input: &WalletUtxo, transactions: &[B256]) -> serde_json::Value {
        payload(
            0,
            1,
            Operation,
            &execute(0, Vec::new()),
            &[input],
            transactions,
        )
    }

    /// A recovery the account sends itself at nonce 0, which shields `request`.
    pub(crate) fn recovery(hash: u8, request: &ShieldRequest) -> serde_json::Value {
        payload(
            0,
            hash,
            Recovery,
            &multicall(0, vec![shield_call(request)]),
            &[],
            &[],
        )
    }

    /// An operation, payload 1, and a recovery, payload 2, at nonce 0, which is consumed.
    pub(crate) fn operation_and_recovery(
        input: &WalletUtxo,
        transactions: &[B256],
        request: &ShieldRequest,
    ) -> ExecutorRecord {
        record(
            vec![operation(input, transactions), recovery(2, request)],
            1,
        )
    }

    /// A pre-hook, payload 6, and a cancellation of its order, payload 4, whose broadcaster
    /// reply was lost, both at nonce 1. `later` holds payloads at later nonces.
    pub(crate) fn pre_hook_and_cancellation(
        fee: &WalletUtxo,
        later: Vec<serde_json::Value>,
        observations: &SwapOrderObservations,
    ) -> ExecutorRecord {
        let consumed = 2 + u64::try_from(later.len()).unwrap();
        let mut issued = vec![
            payload(1, 6, SwapPreHook, &execute(1, Vec::new()), &[], &[]),
            payload(
                1,
                4,
                Recovery,
                &execute(1, vec![cancellation_call()]),
                &[fee],
                &[],
            ),
        ];
        issued.extend(later);
        swap_record(issued, consumed, observations)
    }

    pub(crate) fn pre_hook_executed() -> SwapOrderObservations {
        SwapOrderObservations {
            pre_hook_executed: Some(SwapObservation {
                block: BlockNumHash::new(15, B256::repeat_byte(15)),
                transaction_hash: None,
            }),
            ..SwapOrderObservations::default()
        }
    }

    #[test]
    fn recorded_spend_names_the_operation_over_a_recovery() {
        let input = note(1);
        let saved = operation_and_recovery(&input, &[B256::repeat_byte(30)], &shield(7));
        let sync = synced(RESOLVED_AT, vec![spent(input, 30, 15)]);
        assert_eq!(
            outcomes(&saved, Some(&sync), &[], &[1, 2]),
            [Executed, Superseded]
        );
    }

    #[test]
    fn a_missing_shield_does_not_name_the_operation() {
        let input = note(1);
        let saved = operation_and_recovery(&input, &[], &shield(7));
        for sync in [
            None,
            Some(synced(RESOLVED_AT - 10, vec![input.clone()])),
            Some(synced(RESOLVED_AT + 10, vec![input])),
        ] {
            assert_eq!(
                outcomes(&saved, sync.as_ref(), &[], &[1, 2]),
                [Resolved, Resolved]
            );
        }
    }

    #[test]
    fn one_of_two_independent_recoveries_is_named_by_its_own_shield() {
        let input = note(1);
        let first = shield(7);
        let saved = record(
            vec![
                operation(&input, &[]),
                recovery(2, &first),
                recovery(3, &shield(8)),
            ],
            1,
        );
        let sync = synced(RESOLVED_AT, vec![input, shielded(&first)]);
        assert_eq!(
            outcomes(&saved, Some(&sync), &[], &[1, 2, 3]),
            [Superseded, Executed, Superseded]
        );
    }

    #[test]
    fn cancellation_with_a_lost_reply_names_nothing_without_order_state() {
        let fee = note(1);
        let saved = pre_hook_and_cancellation(&fee, Vec::new(), &SwapOrderObservations::default());
        assert_eq!(outcomes(&saved, None, &[], &[6, 4]), [Resolved, Resolved]);
        // The cancellation's fee note is spent, in a transaction the wallet never recorded.
        let sync = synced(RESOLVED_AT + 10, vec![spent(fee, 31, 15)]);
        assert_eq!(
            outcomes(&saved, Some(&sync), &[], &[6, 4]),
            [Resolved, Resolved]
        );
    }

    #[test]
    fn recorded_pre_hook_beats_a_cancellation_at_its_nonce() {
        let saved = pre_hook_and_cancellation(&note(1), Vec::new(), &pre_hook_executed());
        assert_eq!(outcomes(&saved, None, &[], &[6, 4]), [Executed, Superseded]);
    }

    #[test]
    fn later_recovery_that_invalidates_the_order_keeps_it_from_naming_the_cancellation() {
        let later = vec![payload(
            2,
            5,
            Recovery,
            &multicall(2, vec![cancellation_call()]),
            &[],
            &[],
        )];
        // The later recovery could have set the order's state, so that state does not name
        // the cancellation at the pre-hook's nonce.
        let unobserved =
            pre_hook_and_cancellation(&note(1), later, &SwapOrderObservations::default());
        assert_eq!(
            outcomes(&unobserved, None, &[ORDER], &[6, 4]),
            [Resolved, Resolved]
        );
    }

    #[test]
    fn invalidated_order_alone_names_the_cancellation() {
        let saved =
            pre_hook_and_cancellation(&note(1), Vec::new(), &SwapOrderObservations::default());
        assert_eq!(
            outcomes(&saved, None, &[ORDER], &[6, 4]),
            [Superseded, Executed]
        );
    }

    #[test]
    fn fee_variants_are_one_action_and_neither_is_singled_out() {
        let input = note(1);
        let fee = note(2);
        let request = shield(7);
        let variant = |hash: u8, gas: u64| {
            let mut call = execute(0, vec![shield_call(&request)]);
            call._actionData.minGasLimit = U256::from(gas);
            payload(
                0,
                hash,
                Recovery,
                &call,
                &[&fee],
                &[B256::repeat_byte(30 + hash)],
            )
        };
        let saved = record(
            vec![operation(&input, &[]), variant(2, 100), variant(3, 200)],
            1,
        );
        // Only the second variant's transaction is seen.
        let sync = synced(RESOLVED_AT, vec![input, spent(fee, 33, 15)]);
        assert_eq!(
            outcomes(&saved, Some(&sync), &[], &[1, 2, 3]),
            [Superseded, Executed, Executed]
        );
        let evidence = ExecutorAttributionEvidence {
            railgun: RAILGUN,
            sync: Some(&sync),
            invalidated_orders: None,
        };
        assert_eq!(
            attributed_action(&saved, U256::ZERO, &evidence)
                .unwrap()
                .payloads(),
            [B256::repeat_byte(2), B256::repeat_byte(3)]
        );
    }

    #[test]
    fn later_spend_of_a_superseded_payloads_notes_changes_nothing() {
        let input = note(1);
        let request = shield(7);
        let saved = operation_and_recovery(&input, &[B256::repeat_byte(30)], &request);
        // The operation's released note is spent by a transaction not recorded for it.
        let sync = synced(
            RESOLVED_AT + 10,
            vec![spent(input, 31, 25), shielded(&request)],
        );
        assert_eq!(
            outcomes(&saved, Some(&sync), &[], &[1, 2]),
            [Superseded, Executed]
        );
    }

    #[test]
    fn private_sync_resolves_a_pending_payload_only_from_its_own_spend_or_shield() {
        let input = note(1);
        let request = shield(7);
        // Nonce 0 is unconsumed as far as the record knows.
        let resolved = |issued: Vec<serde_json::Value>, utxos: Vec<WalletUtxo>, safe_head| {
            let sync = synced(RESOLVED_AT, utxos);
            synced_resolution(&record(issued, 0), RAILGUN, &sync, safe_head)
        };
        let executed = Some(ExecutorNonceWatermark::new(U256::ONE, 15));
        // A recovery the account sent itself spends no notes. Its shield arrives in block 15.
        let self_sent = || vec![recovery(2, &request)];
        assert_eq!(
            resolved(self_sent(), vec![shielded(&request)], Some(15)),
            executed
        );
        assert_eq!(
            resolved(self_sent(), vec![shielded(&request)], Some(14)),
            None
        );
        let sent = || vec![operation(&input, &[B256::repeat_byte(30)])];
        assert_eq!(
            resolved(sent(), vec![spent(input.clone(), 30, 15)], None),
            executed
        );
        // A spend in a transaction that is not recorded for the payload proves nothing.
        assert_eq!(
            resolved(sent(), vec![spent(input.clone(), 31, 15)], None),
            None
        );
        // After a lost broadcaster reply there is no recorded transaction and no shield.
        assert_eq!(
            resolved(
                vec![operation(&input, &[])],
                vec![spent(input, 30, 15)],
                None
            ),
            None
        );
    }
}
