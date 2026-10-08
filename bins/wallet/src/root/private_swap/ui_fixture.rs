//! A debug-only fixture for looking at the Public account swap screens: it opens them on a
//! scratch wallet without private funds and holds a chosen state on screen. It is compiled
//! only with `debug_assertions`, and does nothing unless `RAILOXIDE_UI_FIXTURE` is set.
//!
//! The variable names one mode, read once:
//!
//! - `form` lets the Buy picker pick a network without private funds there, and gives a new
//!   destination account a stand-in setup fee in place of an estimate, so a live quote can
//!   be reviewed. Approving the review ends in an error.
//! - `flow:<stage>` does what `form` does, and after the review is approved shows the
//!   detail of the reviewed swap, as a synthesized record, and holds it at `setup-sending`,
//!   `setup-waiting`, `finishing`, `placing` or `error`. `changed` ends on the form again,
//!   with the terms that changed to review. The record is staged once, at the stage the mode
//!   names, and the steps past the first wait start on the view's next observation pass. No
//!   record of the wallet is read, and the detail's actions do nothing.
//! - `detail:<stage>` opens the swap dialog of a Public account on the detail of one
//!   synthesized swap, at `setup-not-sent`, `setup-pending`, `account-ready`, `approving`,
//!   `order-open`, `bridging`, `delivered`, `held-on-destination`, `refunded` or `expired`.
//!   The detail's actions do nothing.
//!
//! In every mode the jobs of a swap paid from a Public account are scripted: nothing is
//! claimed, signed, paid or sent, and no record is written.

use std::sync::OnceLock;
use std::time::Duration;

use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes, U256};
use gpui::{App, Context, Window};
use wallet_ops::SwapReviewChange;
use wallet_ops::vault::{
    BridgeShieldFailure, ExecutorExecutionResult, ExecutorNonceObservation, ExecutorOperationId,
    ExecutorPayloadContext, ExecutorPayloadInclusion, ExecutorPayloadPurpose, ExecutorRecord,
    IssuedExecutorPayload, PublicSwapApproval, PublicSwapDeposited, PublicSwapIntent,
    PublicSwapObservations, PublicSwapRecord, SwapApprovedAccount, SwapApprovedBounds,
    SwapBridgeHandoff, SwapBridgeOutcome, SwapObservation, SwapUseId,
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
    Flow(Flow),
    Detail(Detail),
}

/// Where a reviewed swap is held.
#[derive(Clone, Copy, Debug)]
enum Flow {
    SetupSending,
    SetupWaiting,
    Finishing,
    Placing,
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
    Bridging,
    Delivered,
    HeldOnDestination,
    Refunded,
    Expired,
}

fn parse(value: &str) -> Option<Mode> {
    let value = value.trim();
    if value == "form" {
        return Some(Mode::Form);
    }
    if let Some(stage) = value.strip_prefix("flow:") {
        return Some(Mode::Flow(match stage {
            "setup-sending" => Flow::SetupSending,
            "setup-waiting" => Flow::SetupWaiting,
            "finishing" => Flow::Finishing,
            "placing" => Flow::Placing,
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
        "bridging" => Detail::Bridging,
        "delivered" => Detail::Delivered,
        "held-on-destination" => Detail::HeldOnDestination,
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
    matches!(mode(), Some(Mode::Flow(Flow::Finishing | Flow::Placing)))
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
    Changed(SwapReviewChange),
    Fail(&'static str),
}

/// The scripted answer at `phase`, and how long the job takes to give it: a hold wherever the
/// mode scripts nothing else. `None` unless a fixture mode is on.
pub(super) fn step(phase: Phase) -> Option<(Duration, Step)> {
    use Flow::{Changed, Error, Finishing, Placing, SetupWaiting};
    Some(match (mode()?, phase) {
        (Mode::Form | Mode::Detail(_), Phase::Approved) => (
            Duration::ZERO,
            Step::Fail("Fixture: this mode stops at the review. Nothing was sent."),
        ),
        (Mode::Flow(_), Phase::Approved) => (BEAT, Step::Claimed),
        (Mode::Flow(SetupWaiting | Finishing | Placing), Phase::Claimed) => (BEAT, Step::Waiting),
        (Mode::Flow(Changed), Phase::Claimed) => (BEAT, Step::Changed(SwapReviewChange::Delivery)),
        (Mode::Flow(Error), Phase::Claimed) => (
            BEAT,
            Step::Fail("Fixture: the broadcaster couldn't submit the destination setup."),
        ),
        (Mode::Flow(Placing), Phase::Setup) => (BEAT, Step::Ready),
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
            Flow::Finishing | Flow::Placing | Flow::Changed => Detail::AccountReady,
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
        | Detail::OrderOpen => PublicSwapObservations::default(),
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
        Detail::HeldOnDestination => handed_off(Some(SwapBridgeOutcome::HeldOnDestination {
            block: fill,
            transaction_hash: hash,
            amount: swap.received,
        })),
        Detail::Refunded => PublicSwapObservations {
            bridge_refund: Some(at(ORIGIN_BLOCK + 900)),
            ..handed_off(Some(SwapBridgeOutcome::Refunding))
        },
    };
    let account = Address::repeat_byte(0x3a);
    let delegate = Address::repeat_byte(0x5d);
    let bounds: SwapApprovedBounds = serde_json::from_value(serde_json::json!({
        "sell_amount": swap.sold, "buy_amount": swap.bought,
        "private_minimum": swap.received * U256::from(99_u8) / U256::from(100_u8),
        "shield_fee_bps": "0x0", "slippage_bps": 50, "pre_hook_gas_limit": 0, "anchors": []
    }))
    .ok()?;
    let saved = PublicSwapRecord::new(
        PublicSwapApproval {
            bounds,
            price_verified: Some(true),
            price_acknowledged: false,
            sell_token: swap.sell,
            on_shield_failure: if stage == Detail::HeldOnDestination {
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
    if approves {
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
    }
    let setup_block = BlockNumHash::new(DESTINATION_BLOCK, B256::repeat_byte(0xb3));
    let mut setup = serde_json::to_value(IssuedExecutorPayload::new(
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
    if set_up {
        setup["inclusion"] = serde_json::to_value(ExecutorPayloadInclusion::new(
            setup_block,
            B256::repeat_byte(0x5e),
            ExecutorExecutionResult::Executed,
        ))
        .ok()?;
    }
    // A setup that wasn't sent issued no payload.
    let issued = if stage == Detail::SetupNotSent {
        Vec::new()
    } else {
        vec![setup]
    };
    serde_json::from_value(serde_json::json!({
        "version": 2, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": identity.operation, "index": 3,
        "address": account, "delegate": delegate, "retired": false, "issued": issued,
        "swap_uses": [{ "id": identity.swap_use, "started_at": now.saturating_sub(240),
            "fresh": true,
            "role": { "PublicSourceDestination": { "origin_chain": swap.origin,
                "source": swap.source, "destination_token": swap.delivered, "swap": saved
            }}
        }]
    }))
    .ok()
}
