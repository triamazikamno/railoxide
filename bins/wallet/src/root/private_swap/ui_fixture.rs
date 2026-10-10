//! A debug-only fixture for looking at the Public account swap screens and at the Stealth
//! accounts inspector: it opens them on a scratch wallet without private funds and holds a
//! chosen state on screen. It is compiled only with `debug_assertions`, and does nothing
//! unless `RAILOXIDE_UI_FIXTURE` is set.
//!
//! The variable names one mode, read once:
//!
//! - `route-error` opens the Public form with a failed Across route lookup and its Retry.
//! - `form` lets the Buy picker pick a network without private funds there, and gives a new
//!   destination account a stand-in setup fee in place of an estimate, so a live quote can
//!   be reviewed. Approving the review ends in an error. The quote is the wallet's own, so
//!   the review signs a permit when the Sell token has one and the account's allowance is
//!   short, as USDC's is on Ethereum, Base and Arbitrum. Such an order asks the account for
//!   no gas, so an account without native balance can review it.
//! - `flow:<stage>` does what `form` does, and after the review is approved shows the
//!   detail of the reviewed swap, as a synthesized record, and holds it at `setup-sending`,
//!   `setup-waiting`, `finishing`, `placing` or `error`. `changed` ends on the form again,
//!   with the terms that changed to review. `signatures` opens the review a hardware
//!   account sees before its device prompts, for any account and without a device: two
//!   groups for an order, and three, the signed approval first, for an order whose review
//!   signs a permit. The record is staged once, at the stage the mode
//!   names, and the steps past the first wait start on the view's next observation pass. No
//!   record of the wallet is read, and the detail's actions do nothing.
//! - `detail:<stage>` opens the swap dialog of a Public account on the detail of one
//!   synthesized swap, at `setup-not-sent`, `setup-pending`, `account-ready`, `approving`,
//!   `order-open`, `bridging`, `delivered`, `held-on-destination`,
//!   `held-after-partial-recovery`, `refunded` or `expired`. The partial recovery holds a
//!   confirmed balance at 90% of the original fill without changing its bridge evidence.
//!   `permit-order-open` is an open order whose approval was signed as a permit, with no
//!   approval transaction, and `permit-used-up` is that order with the warning that its
//!   signed approval was used up.
//!   The detail's actions do nothing.
//! - `accounts` shows two synthesized accounts in Stealth accounts in place of the wallet's
//!   own. Account #7 is a swap account whose inspector shows each result tag: an executed
//!   setup, a pre-hook that executed with the cancellation it superseded at the same nonce,
//!   a consumed nonce that names neither of two recoveries, and an unconfirmed recovery.
//!   Account #8 holds one executed operation. The wallet holds neither account, so Check
//!   balances and the other actions fail.
//!
//! In every mode the jobs of a swap paid from a Public account are scripted: nothing is
//! claimed, signed, paid or sent, and no record is written. The synthesized records live in
//! memory only.

use std::sync::OnceLock;
use std::time::Duration;

use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes, U256};
use gpui::{App, Context, Window};
use wallet_ops::SwapReviewChange;
use wallet_ops::vault::{
    BridgeShieldFailure, ExecutorNonceObservation, ExecutorNonceWatermark, ExecutorOperationId,
    ExecutorPayloadContext, ExecutorPayloadPurpose, ExecutorRecord, IssuedExecutorPayload,
    PublicSwapApproval, PublicSwapDeposited, PublicSwapIntent, PublicSwapObservations,
    PublicSwapRecord, SwapApprovedAccount, SwapApprovedBounds, SwapBridgeHandoff,
    SwapBridgeOutcome, SwapDelivery, SwapObservation, SwapOrderObservations, SwapProof,
    SwapRecipient, SwapTerms, SwapUseId,
};

use super::dialog::SwapDialogView;
use super::model::SwapIdentity;
use super::{PrivateSwapsView, now_unix};

const VARIABLE: &str = "RAILOXIDE_UI_FIXTURE";
/// How long a scripted job takes before it answers.
const BEAT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug)]
enum Mode {
    Form,
    RouteError,
    Flow(Flow),
    Detail(Detail),
    Accounts,
}

/// Where a reviewed swap is held.
#[derive(Clone, Copy, Debug)]
enum Flow {
    SetupSending,
    SetupWaiting,
    Finishing,
    Placing,
    Signatures,
    Changed,
    Error,
}

/// The stage the synthesized swap's detail shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Detail {
    SetupNotSent,
    SetupPending,
    AccountReady,
    Approving,
    OrderOpen,
    PermitOrderOpen,
    PermitUsedUp,
    Bridging,
    Delivered,
    HeldOnDestination,
    HeldAfterPartialRecovery,
    Refunded,
    Expired,
}

fn parse(value: &str) -> Option<Mode> {
    let value = value.trim();
    if value == "form" {
        return Some(Mode::Form);
    }
    if value == "route-error" {
        return Some(Mode::RouteError);
    }
    if value == "accounts" {
        return Some(Mode::Accounts);
    }
    if let Some(stage) = value.strip_prefix("flow:") {
        return Some(Mode::Flow(match stage {
            "setup-sending" => Flow::SetupSending,
            "setup-waiting" => Flow::SetupWaiting,
            "finishing" => Flow::Finishing,
            "placing" => Flow::Placing,
            "signatures" => Flow::Signatures,
            "changed" => Flow::Changed,
            "error" => Flow::Error,
            _ => return None,
        }));
    }
    Some(Mode::Detail(match value.strip_prefix("detail:")? {
        "setup-not-sent" => Detail::SetupNotSent,
        "setup-pending" => Detail::SetupPending,
        "account-ready" => Detail::AccountReady,
        "approving" => Detail::Approving,
        "order-open" => Detail::OrderOpen,
        "permit-order-open" => Detail::PermitOrderOpen,
        "permit-used-up" => Detail::PermitUsedUp,
        "bridging" => Detail::Bridging,
        "delivered" => Detail::Delivered,
        "held-on-destination" => Detail::HeldOnDestination,
        "held-after-partial-recovery" => Detail::HeldAfterPartialRecovery,
        "refunded" => Detail::Refunded,
        "expired" => Detail::Expired,
        _ => return None,
    }))
}

/// The mode the variable names. Its first read says so in the log.
fn mode() -> Option<Mode> {
    static MODE: OnceLock<Option<Mode>> = OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = parse(&std::env::var(VARIABLE).ok()?);
        if let Some(mode) = mode {
            tracing::warn!(
                ?mode,
                "the debug UI fixture is active: swaps paid from a Public account are staged and nothing is sent"
            );
        } else {
            tracing::warn!("{VARIABLE} names no UI fixture mode and is ignored");
        }
        mode
    })
}

/// Whether the Public form should show its failed route lookup.
pub(super) fn route_error() -> bool {
    matches!(mode(), Some(Mode::RouteError))
}

/// Whether a fixture mode is on. The form of a swap paid from a Public account then works
/// without private funds, and its jobs are scripted.
pub(super) fn active() -> bool {
    mode().is_some()
}

/// Whether the Public account swap records are the fixture's own, which nothing reads again,
/// tracks or acts on. A `flow:` mode has none until its review is approved.
pub(super) fn holds_records() -> bool {
    matches!(mode(), Some(Mode::Detail(_) | Mode::Flow(_)))
}

/// The setup fee that stands in for a new destination account's estimate, in a token of
/// `decimals`. `None` unless a fixture mode is on.
pub(super) fn setup_fee(decimals: u8) -> Option<U256> {
    active().then(|| ten_thousandths(decimals, 500))
}

/// Whether the staged swap's destination setup counts as confirmed, so its status names what
/// follows it.
pub(super) fn setup_done() -> bool {
    matches!(
        mode(),
        Some(Mode::Flow(
            Flow::Finishing | Flow::Placing | Flow::Signatures
        ))
    )
}

/// `count` ten-thousandths of a token of `decimals`.
fn ten_thousandths(decimals: u8, count: u64) -> U256 {
    U256::from(count) * U256::from(10_u8).pow(U256::from(decimals.saturating_sub(4)))
}

/// Where a job of a swap paid from a Public account would start its real work.
pub(super) enum Phase {
    /// The review was approved.
    Approved,
    /// The swap is claimed, and its destination is prepared and set up.
    Claimed,
    /// The destination setup is read again, and the swap continues past it once it is confirmed.
    Setup,
    /// The signed order or deposit is sent.
    Submit,
}

/// What a scripted job answers in place of the real one.
pub(super) enum Step {
    /// Never answers, which holds the job's status on screen.
    Hold,
    Claimed,
    Waiting,
    Ready,
    /// The order's signatures are staged for the review a hardware account sees.
    Signatures,
    Changed(SwapReviewChange),
    Fail(&'static str),
}

/// The scripted answer at `phase`, and how long the job takes to give it: a hold wherever the
/// mode scripts nothing else. `None` unless a fixture mode is on.
pub(super) fn step(phase: Phase) -> Option<(Duration, Step)> {
    use Flow::{Changed, Error, Finishing, Placing, SetupWaiting, Signatures};
    Some(match (mode()?, phase) {
        (Mode::Form | Mode::RouteError | Mode::Detail(_) | Mode::Accounts, Phase::Approved) => (
            Duration::ZERO,
            Step::Fail("Fixture: this mode stops at the review. Nothing was sent."),
        ),
        (Mode::Flow(_), Phase::Approved) => (BEAT, Step::Claimed),
        (Mode::Flow(SetupWaiting | Finishing | Placing | Signatures), Phase::Claimed) => {
            (BEAT, Step::Waiting)
        }
        (Mode::Flow(Changed), Phase::Claimed) => (BEAT, Step::Changed(SwapReviewChange::Delivery)),
        (Mode::Flow(Error), Phase::Claimed) => (
            BEAT,
            Step::Fail("Fixture: the broadcaster couldn't submit the destination setup."),
        ),
        (Mode::Flow(Placing), Phase::Setup) => (BEAT, Step::Ready),
        (Mode::Flow(Signatures), Phase::Setup) => (BEAT, Step::Signatures),
        _ => (Duration::ZERO, Step::Hold),
    })
}

/// What the synthesized swap trades: `sold` of `sell` from `source` on `origin` for `bought`
/// of `bridged` there, delivered as `received` of `delivered` on the destination network.
/// `order` is false for a deposit of the Sell token itself.
pub(super) struct Staged {
    pub(super) origin: u64,
    pub(super) source: Address,
    pub(super) sell: Address,
    pub(super) bridged: Address,
    pub(super) delivered: Address,
    pub(super) order: bool,
    pub(super) sold: U256,
    pub(super) bought: U256,
    pub(super) received: U256,
}

impl PrivateSwapsView {
    /// In a `detail:` mode, show the detail of one synthesized swap that `source` pays with
    /// `sell`, in place of the form that was just opened.
    pub(super) fn open_ui_fixture_detail(
        &mut self,
        source: Address,
        sell: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(Mode::Detail(stage)) = mode() else {
            return;
        };
        let Some((chain, identity, record)) = self.ui_fixture_record(stage, source, sell, cx)
        else {
            tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        if stage == Detail::HeldAfterPartialRecovery
            && let Some(SwapBridgeOutcome::HeldOnDestination { amount, block, .. }) = record
                .public_swap_use(identity.swap_use)
                .and_then(|(_, swap)| swap.observations().bridge_outcome)
        {
            self.public_destination_balances.insert(
                (chain, identity.operation, identity.swap_use),
                (
                    amount * U256::from(9_u8) / U256::from(10_u8),
                    BlockNumHash::new(block.number + 1, block.hash),
                ),
            );
        }
        if stage == Detail::PermitUsedUp {
            self.public_permit_used_up
                .insert((identity.operation, identity.swap_use));
        }
        self.public_records = vec![(chain, record)];
        self.navigate(SwapDialogView::PublicDetail(identity), window, cx);
    }

    /// In a `flow:` mode, stand the record of the reviewed swap `identity`, which trades
    /// `swap` and delivers on `destination`, in for the claim that would have written it.
    pub(super) fn stage_ui_fixture_flow(
        &mut self,
        identity: SwapIdentity,
        destination: u64,
        swap: &Staged,
    ) {
        let Some(Mode::Flow(flow)) = mode() else {
            return;
        };
        let stage = match flow {
            Flow::SetupSending | Flow::Error => Detail::SetupNotSent,
            Flow::SetupWaiting => Detail::SetupPending,
            // Terms change at signing time, after the account is set up.
            Flow::Finishing | Flow::Placing | Flow::Signatures | Flow::Changed => {
                Detail::AccountReady
            }
        };
        let Some(record) = staged_record(stage, identity, swap) else {
            tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        self.public_records = vec![(destination, record)];
    }

    /// A swap at `stage` to the first other network with private swaps: an order that buys
    /// the origin's wrapped native token and delivers the destination's, so both are named.
    fn ui_fixture_record(
        &self,
        stage: Detail,
        source: Address,
        sell: Address,
        cx: &App,
    ) -> Option<(u64, SwapIdentity, ExecutorRecord)> {
        let origin = self.origin_chain_id;
        let root = self.root.upgrade()?;
        let (bridged, destination, delivered) = {
            let chains = &root.read(cx).effective_chain_configs;
            let bridged = chains.get(origin)?.wrapped_native_token?;
            let (destination, delivered) = chains.values().find_map(|chain| {
                let token = chain.wrapped_native_token?;
                (chain.chain_id != origin && chain.swap_profile().is_some())
                    .then_some((chain.chain_id, token))
            })?;
            (bridged, destination, delivered)
        };
        let amount = |chain, token, count| {
            let decimals = self
                .chain_token_metadata(chain, token, cx)
                .map_or(18, |metadata| metadata.decimals);
            ten_thousandths(decimals, count)
        };
        let identity = SwapIdentity {
            operation: ExecutorOperationId::random().ok()?,
            swap_use: SwapUseId::random().ok()?,
        };
        let record = staged_record(
            stage,
            identity,
            &Staged {
                origin,
                source,
                sell,
                bridged,
                delivered,
                order: true,
                sold: amount(origin, sell, 1_000_000),
                bought: amount(origin, bridged, 300),
                received: amount(destination, delivered, 299),
            },
        )?;
        Some((destination, identity, record))
    }
}

/// The destination record of `swap` at `stage`, decoded from the JSON a stored record has.
fn staged_record(stage: Detail, identity: SwapIdentity, swap: &Staged) -> Option<ExecutorRecord> {
    const ORIGIN_BLOCK: u64 = 31_204_518;
    const DESTINATION_BLOCK: u64 = 402_118_977;
    let now = now_unix();
    // An open order's validity is still ahead. An expired order's is behind.
    let valid_to = u32::try_from(if stage == Detail::Expired {
        now.saturating_sub(600)
    } else {
        now.saturating_add(1_200)
    })
    .ok()?;
    let hash = B256::repeat_byte(0x7a);
    let at = |number| SwapObservation {
        block: BlockNumHash::new(number, B256::repeat_byte(0xb1)),
        transaction_hash: Some(hash),
    };
    let fill = BlockNumHash::new(DESTINATION_BLOCK + 640, B256::repeat_byte(0xb2));
    let handed_off = |outcome| PublicSwapObservations {
        traded: Some(at(ORIGIN_BLOCK + 21)),
        bridge_handoff: Some(SwapBridgeHandoff {
            observation: at(ORIGIN_BLOCK + 21),
            deposit_id: Some(U256::from(1_279_830_u32)),
        }),
        deposited: Some(PublicSwapDeposited {
            input_amount: swap.bought,
            output_amount: swap.received,
        }),
        bridge_outcome: outcome,
        ..Default::default()
    };
    let observed = match stage {
        Detail::SetupNotSent
        | Detail::SetupPending
        | Detail::AccountReady
        | Detail::Approving
        | Detail::OrderOpen
        | Detail::PermitOrderOpen
        | Detail::PermitUsedUp => PublicSwapObservations::default(),
        Detail::Expired => PublicSwapObservations {
            expired: Some(SwapObservation {
                transaction_hash: None,
                ..at(ORIGIN_BLOCK + 60)
            }),
            ..Default::default()
        },
        Detail::Bridging => handed_off(None),
        Detail::Delivered => handed_off(Some(SwapBridgeOutcome::DeliveredVerified {
            block: fill,
            transaction_hash: hash,
            output_amount: swap.received,
            shielded: true,
        })),
        Detail::HeldOnDestination | Detail::HeldAfterPartialRecovery => {
            handed_off(Some(SwapBridgeOutcome::HeldOnDestination {
                block: fill,
                transaction_hash: hash,
                amount: swap.received,
            }))
        }
        Detail::Refunded => PublicSwapObservations {
            bridge_refund: Some(at(ORIGIN_BLOCK + 900)),
            ..handed_off(Some(SwapBridgeOutcome::Refunding))
        },
    };
    let account = Address::repeat_byte(0x3a);
    let delegate = Address::repeat_byte(0x5d);
    // An approval signed as a permit binds its pre-hook's gas limit and sends no transaction.
    let permit = matches!(stage, Detail::PermitOrderOpen | Detail::PermitUsedUp);
    let pre_hook_gas_limit = if permit { 110_000_u64 } else { 0 };
    let bounds: SwapApprovedBounds = serde_json::from_value(serde_json::json!({
        "sell_amount": swap.sold, "buy_amount": swap.bought,
        "private_minimum": swap.received * U256::from(99_u8) / U256::from(100_u8),
        "shield_fee_bps": "0x0", "slippage_bps": 50,
        "pre_hook_gas_limit": pre_hook_gas_limit, "anchors": []
    }))
    .ok()?;
    let saved = PublicSwapRecord::new(
        PublicSwapApproval {
            bounds,
            price_verified: Some(true),
            price_acknowledged: false,
            sell_token: swap.sell,
            on_shield_failure: if matches!(
                stage,
                Detail::HeldOnDestination | Detail::HeldAfterPartialRecovery
            ) {
                BridgeShieldFailure::KeepOnDestination
            } else {
                BridgeShieldFailure::RefundOnOrigin
            },
            destination: SwapApprovedAccount {
                address: Some(account),
                setup: true,
            },
            max_gas_cost: U256::ZERO,
        },
        PublicSwapIntent {
            bridged_token: swap.bridged,
            order: swap.order,
        },
    );
    let mut saved = serde_json::to_value(saved).ok()?;
    let traded = observed.traded.is_some();
    saved["observations"] = serde_json::to_value(observed).ok()?;
    if traded {
        saved["observations"]["trade_amounts"] = serde_json::json!({
            "sell_amount": swap.sold, "buy_amount": swap.bought, "fee_amount": "0x0"
        });
    }
    // The setup is on its way until the Public account approves, and its order follows.
    let set_up = !matches!(stage, Detail::SetupNotSent | Detail::SetupPending);
    let approves = set_up && stage != Detail::AccountReady;
    if approves && !permit {
        let inclusion = (stage != Detail::Approving).then(|| {
            serde_json::json!({
                "observation": at(ORIGIN_BLOCK), "finalized": true, "succeeded": true
            })
        });
        saved["transactions"] = serde_json::json!([{
            "kind": "Approval", "transaction": {}, "hash": hash, "inclusion": inclusion
        }]);
    }
    if approves && stage != Detail::Approving {
        let mut uid = [0_u8; 56];
        uid[52..].copy_from_slice(&valid_to.to_be_bytes());
        saved["path"] = serde_json::json!({ "Order": {
            "uid": alloy::hex::encode_prefixed(uid),
            "buy_token": swap.bridged, "proxy": Address::repeat_byte(0x4c),
            "batch": { "calldata": "0x", "nonce": B256::ZERO, "deadline": valid_to },
            "submission": {
                "signature": alloy::hex::encode_prefixed([0_u8; 65]), "quote_id": null
            },
            "submission_status": "Accepted"
        }});
        if permit {
            saved["path"]["Order"]["permit"] = serde_json::json!({
                "nonce": U256::ZERO, "deadline": valid_to, "value": swap.sold,
                "signature": alloy::hex::encode_prefixed([0_u8; 65])
            });
        }
    }
    let setup_block = BlockNumHash::new(DESTINATION_BLOCK, B256::repeat_byte(0xb3));
    let setup = serde_json::to_value(IssuedExecutorPayload::new(
        U256::ZERO,
        delegate,
        B256::repeat_byte(0x5e),
        ExecutorPayloadPurpose::Operation,
        ExecutorPayloadContext::new(
            Bytes::new(),
            ExecutorNonceObservation::new(setup_block, U256::ZERO),
            Vec::new(),
        ),
    ))
    .ok()?;
    // A setup that wasn't sent issued no payload.
    let issued = if stage == Detail::SetupNotSent {
        Vec::new()
    } else {
        vec![setup]
    };
    // A confirmed setup is one whose nonce an account read showed consumed.
    let consumed = set_up.then(|| ExecutorNonceWatermark::new(U256::ONE, DESTINATION_BLOCK));
    serde_json::from_value(serde_json::json!({
        "version": 2, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": identity.operation, "index": 3,
        "address": account, "delegate": delegate, "retired": false, "issued": issued,
        "nonce_watermark": consumed,
        "swap_uses": [{ "id": identity.swap_use, "started_at": now.saturating_sub(240),
            "fresh": true,
            "role": { "PublicSourceDestination": { "origin_chain": swap.origin,
                "source": swap.source, "destination_token": swap.delivered, "swap": saved
            }}
        }]
    }))
    .ok()
}

/// The stealth accounts an `accounts` mode shows in place of the wallet's own, synthesized
/// once. `None` in every other mode.
pub(in crate::root) fn stealth_accounts() -> Option<Vec<ExecutorRecord>> {
    static ACCOUNTS: OnceLock<Vec<ExecutorRecord>> = OnceLock::new();
    matches!(mode(), Some(Mode::Accounts)).then(|| {
        ACCOUNTS
            .get_or_init(|| {
                let accounts = staged_accounts();
                if accounts.len() != 2 {
                    tracing::warn!("the debug UI fixture couldn't synthesize its stealth accounts");
                }
                accounts
            })
            .clone()
    })
}

/// The payload `hash` signed at `nonce`, handed off in `transaction` when it has one. Its
/// calldata does not decode, so each payload is an action of its own.
fn staged_payload(
    nonce: u64,
    hash: u8,
    purpose: ExecutorPayloadPurpose,
    transaction: Option<u8>,
) -> Option<serde_json::Value> {
    let signed = BlockNumHash::new(402_118_900, B256::repeat_byte(0xb4));
    let mut payload = serde_json::to_value(IssuedExecutorPayload::new(
        U256::from(nonce),
        Address::repeat_byte(0x5d),
        B256::repeat_byte(hash),
        purpose,
        ExecutorPayloadContext::new(
            Bytes::new(),
            ExecutorNonceObservation::new(signed, U256::from(nonce)),
            Vec::new(),
        ),
    ))
    .ok()?;
    payload["transaction_hashes"] = serde_json::json!(
        transaction
            .map(B256::repeat_byte)
            .into_iter()
            .collect::<Vec<_>>()
    );
    Some(payload)
}

/// The two accounts of the `accounts` mode, decoded from the JSON a stored record has. An
/// account that fails to decode is left out.
fn staged_accounts() -> Vec<ExecutorRecord> {
    use ExecutorPayloadPurpose::{Operation, Recovery, SwapPreHook};
    const READ_AT: u64 = 402_119_640;
    let now = now_unix();
    let delegate = Address::repeat_byte(0x5d);
    let account = |index: u32,
                   address: u8,
                   consumed: u64,
                   issued: Vec<Option<serde_json::Value>>|
     -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
            "operation": ExecutorOperationId::random().ok()?, "index": index,
            "address": Address::repeat_byte(address), "delegate": delegate, "retired": false,
            "created_at": now.saturating_sub(3_600),
            "issued": issued.into_iter().collect::<Option<Vec<_>>>()?,
            "nonce_watermark": ExecutorNonceWatermark::new(U256::from(consumed), READ_AT),
        }))
    };
    // The swap's order expired, with its pre-hook recorded as executed at nonce 1.
    let swap = || {
        let mut uid = [0_u8; 56];
        uid[52..].copy_from_slice(&u32::try_from(now.saturating_sub(600)).ok()?.to_be_bytes());
        let mut swap = account(
            7,
            0x3b,
            3,
            vec![
                staged_payload(0, 0xb0, Operation, Some(0xc0)),
                staged_payload(1, 0xb1, SwapPreHook, None),
                staged_payload(1, 0xb2, Recovery, Some(0xc2)),
                staged_payload(2, 0xb3, Recovery, Some(0xc3)),
                staged_payload(2, 0xb4, Recovery, None),
                staged_payload(3, 0xb5, Recovery, Some(0xc5)),
            ],
        )?;
        swap["swap"] = serde_json::json!({
            "terms": SwapTerms::new(
                Address::repeat_byte(0x11),
                Address::ZERO,
                SwapRecipient::new(U256::ONE, [0; 32]),
                B256::repeat_byte(0xb0),
            ),
            "proof": SwapProof::new(B256::repeat_byte(0xb6), Vec::new()),
            "orders": [{
                "attempt": 0, "uid": alloy::hex::encode_prefixed(uid),
                "delivery": SwapDelivery::External { receiver: Address::repeat_byte(0x42) },
                "bounds": {
                    "sell_amount": "0x5af3107a4000", "buy_amount": "0x5af3107a4000",
                    "private_minimum": "0x5af3107a4000", "shield_fee_bps": "0x0",
                    "slippage_bps": 50, "pre_hook_gas_limit": 0, "post_hook_gas_limit": 0,
                    "anchors": []
                },
                "pre_hook": { "nonce": U256::ONE, "payload": B256::repeat_byte(0xb1) },
                "post_hook": null, "invalidates": null,
                "observations": SwapOrderObservations {
                    pre_hook_executed: Some(SwapObservation {
                        block: BlockNumHash::new(READ_AT - 80, B256::repeat_byte(0xb7)),
                        transaction_hash: None,
                    }),
                    ..Default::default()
                },
            }],
        });
        Some(swap)
    };
    let operation = || {
        let mut operation = account(
            8,
            0x3c,
            1,
            vec![staged_payload(0, 0xa0, Operation, Some(0xc8))],
        )?;
        operation["purpose_summary"] = serde_json::json!(format!(
            "Unshield 0.001 WETH → {} (unwrap)",
            Address::repeat_byte(0x42)
        ));
        Some(operation)
    };
    [swap(), operation()]
        .into_iter()
        .flatten()
        .filter_map(|account| serde_json::from_value(account).ok())
        .collect()
}
