//! A debug-only fixture for looking at the Public account swap screens, at a private swap's
//! retry review, failed quote, speed-up, split outcome, reported fill, setup progress and
//! cancelled preparation, at the Stealth accounts inspector, at the refund dialog of a blocked
//! Shield, at a private Send's broadcaster progress, at the rows of the UTXO table and at a
//! hardware wallet's Derive from private dialog without a device: it opens
//! them on a scratch wallet without private
//! funds and holds a chosen state on screen. It is compiled only with `debug_assertions`, and
//! does nothing unless `RAILOXIDE_UI_FIXTURE` is set.
//!
//! The variable names one mode, read once:
//!
//! - `shield:estimate-error` and `shield:funding-error` replace a Public asset dialog with
//!   its wrapped-native-token Shield progress dialog: an estimate failure after approval, or
//!   insufficient native balance before approval. These fixtures live in
//!   `public_action/root.rs`, submit no action, and have no retry session. Details and Close
//!   work normally. Public's Add account button becomes Preview Shield failure… so an
//!   unfunded scratch wallet can open them without an account or asset balance.
//! - `shield:mimic-prepared`, `shield:native-prepared`, `shield:mimic-unavailable`,
//!   `shield:fallback`, `shield:native-pending` and `shield:fallback-pending` show a
//!   native-token Shield, also from a Public asset dialog or from Add account, which becomes
//!   Preview native shield…. These live in `public_action/native_shield.rs`. The forms are
//!   for 0.5 of the native token with a stand-in quote. `mimic-prepared` and
//!   `native-prepared` open the stealth route of a software payer, in Mimic Railway and in
//!   Railoxide, with a prepared estimate of 241,380 gas buffered to 289,656.
//!   `mimic-unavailable` and `fallback` open a hardware Public account's wrap fallback, with
//!   the Mimic Railway preference on and off: the checkbox is disabled and unchecked, its
//!   hover tooltip gives the reason, and the Token approval switch and its hint show. The fee
//!   card sums the wrap, approval and shield maximums. `native-pending` holds the single
//!   stealth-route Shield step broadcast and waiting for confirmation, with its transaction
//!   hash. `fallback-pending` holds a hardware payer's wrap fallback with Wrap done, Approve
//!   waiting for confirmation and Shield not started. Nothing is prepared or sent, and no
//!   record is read or written. Shield does nothing in the forms, and neither does Max on the
//!   stealth route.
//! - `route-error` opens the Public form with a failed Across route lookup and its Retry.
//! - `form` lets the Buy picker pick a network without private funds there, and gives a new
//!   destination account a stand-in setup fee in place of an estimate, so a live quote can
//!   be reviewed. Approving the review ends in an error. The quote is the wallet's own, so
//!   the review signs a permit when the Sell token has one and the account's allowance is
//!   short, as USDC's is on Ethereum, Base and Arbitrum. Such an order asks the account for
//!   no gas, so an account without native balance can review it.
//! - `fee:checking` and `fee:failed` do what `form` does, except that the Buy picker holds
//!   every other private network at its setup-fee check: still running, or failed and being
//!   tried again. A new destination account can't be reviewed there.
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
//! - `accounts` shows three synthesized accounts in Stealth accounts in place of the wallet's
//!   own. Account #7 is a swap account whose inspector shows each result tag: an executed
//!   setup, a pre-hook that executed with the cancellation it superseded at the same nonce,
//!   a consumed nonce that names neither of two recoveries, and an unconfirmed recovery.
//!   Account #8 holds one executed operation, and account #9 a settled native shield to a
//!   0zk address, which is hidden until Show hidden. The wallet holds none, so Check
//!   balances and the other actions fail.
//! - `refund:<case>` opens the refund dialog of a synthesized blocked Shield in place of
//!   Stealth accounts, the first time they are opened. Later openings show Stealth accounts,
//!   so the dialog's own actions lead there. `checking` holds the dialog on its origin
//!   check. `available` is a refund that goes back to a Public account labelled Savings, and
//!   its Refund… only closes the dialog. The other cases can't be refunded: the origin of
//!   `unknown` is an account the wallet doesn't hold, that of `stealth` is account #7 of the
//!   `accounts` mode, whose accounts Stealth accounts then shows, and that of `inactive`
//!   is an inactive Public account labelled Savings. `unresolved` is a Shield transaction
//!   that couldn't be read, which has no origin.
//! - `utxos` shows nine synthesized rows in the UTXO table of Wallet activity in place of the
//!   wallet's own, on any network and whatever its sync state. Each shows another form of a
//!   cell: an output received at 3 of its 12 finality blocks, a pending spend at 7 of 12, a
//!   Shield whose PPOI is due in about 40 minutes, a private output whose proof was submitted
//!   23 minutes ago, with its Retry, a verified Shield, a blocked Shield with its Refund, an
//!   amount with all 18 decimals under a long token symbol, a spent Shield, and a Shield
//!   whose PPOI is taking longer than usual. The blocks count from a head block of the
//!   fixture's own, so they hold still. The rows take the network's USDC and WETH, or
//!   Ethereum's where the network lists none. Search filters them, and Show spent doesn't.
//!   Refund does nothing. Retry is enabled while the network has a session, and retries the
//!   wallet's own submissions, as the button above the table does.
//! - `retry-review` opens the private swap form in place of a Public account's, on Ethereum
//!   with its private session started: the retry of a synthesized swap that sells 0.05 WETH
//!   for USDC delivered by Across to a public receiver on Arbitrum One. Its first order
//!   expired with an approved delivery of 90 USDC, and the form holds a synthesized quote that
//!   delivers 100. An ordinary retry discloses no earlier approval, so Review… is enabled and
//!   opens the review without an Earlier approval row or its warning. Approving the review
//!   takes the vault password and ends in an error on the form. The form asks for no quote
//!   and no routes, so another gas share leaves Review… waiting for a bridge quote until the
//!   share is set back.
//! - `quote-error` opens the private swap form in place of a Public account's, on Ethereum: a
//!   new swap of 0.05 WETH for USDC whose quote failed while pricing. The failure is
//!   synthesized, a sell token price outside the range the gas rate can represent, with the
//!   values of a quote on BNB Chain whose pair has no price anchor. The Buy panel shows the
//!   failure's message with its collapsed Details, which open on those values and their copy
//!   button. The form has no synthesized record or review. It asks for no quote and no routes,
//!   so Retry and changed terms leave the failure on screen, and Retry doesn't collapse the
//!   Details as it does on a form that quotes. The wallet holds no WETH, so the Sell select
//!   lists nothing.
//! - `native-buy` opens the private swap form in place of a Public account's, on Ethereum: a
//!   new swap of USDC for WETH, so a Public address can pick ETH from the Buy picker. The
//!   Private tab's Swap opens it too, and is offered there without private funds. The form
//!   has no synthesized record and asks for no quote or routes, so it can't be reviewed.
//! - `split:<stage>` opens the swap dialog, in place of a Public account's form, on the detail
//!   of that swap once its retry traded and the settlement made the expired first order's
//!   deposit: `bridging` has the hand-off and no outcome, `delivered` a verified delivery of 90
//!   USDC under the first order's minimum while the retry approved 100, `refunding` Across's
//!   report of a refund that isn't verified, and `refund-verified` the verified refund with
//!   the deposit found in the stealth account, where Recover… is offered. `unverified` is one
//!   order whose verified trade showed no deposit, for comparison. Copy buttons work, and the
//!   detail's other actions do nothing.
//! - `speed-up-review` opens the private swap form as `retry-review` does, on the speed-up of
//!   that swap's open retry: the first order expired with an approved delivery of 90 USDC, the
//!   retry that delivers 100 is open, and the form holds a synthesized quote that delivers 95
//!   for a lower deposit. Its terms are fixed, and under the Minimum controls it shows the open
//!   order's signed minimum beside the new one. Ticking the earlier order's checkbox enables
//!   Review…, which opens the speed-up's review with both signed minimums, its Earlier approval
//!   row and its warning. Approving the review takes the vault password and ends in an error on
//!   the form. As in `retry-review`, another gas share leaves Review… waiting for a bridge quote.
//! - `speed-up:open` opens the swap dialog, in place of a Public account's form, on the detail
//!   of that swap with its open retry, where Speed up… sits in the order step under the
//!   order's ID and opens the form of `speed-up-review`. `speed-up:just-placed` is that detail
//!   with the retry signed as the mode opens: it shows no Speed up… at first, and the action
//!   appears by itself 30 seconds later. `speed-up:two-orders` is the detail of the swap once a
//!   speed-up was placed and both orders can still fill, where Speed up… is disabled with its
//!   reason and the Orders section lists both orders with their IDs, minimums and expiries.
//!   `speed-up:rejected` is that detail when the orderbook rejected the speed-up: the swap
//!   stays open on the original order, the Orders section reads Open and Rejected, and
//!   Speed up… is disabled with the same reason.
//!   The detail's other actions do nothing.
//! - `speed-up:original-bridging`, `speed-up:original-delivered` and
//!   `speed-up:replacement-delivered` are that swap once one of its two orders traded, as one
//!   swap that follows the traded order: the original order, which approved 100 USDC, with its
//!   own deposit on its way or delivered, or the speed-up, which approved 95, delivered. The
//!   other order reads Not filled in the Orders section, and both IDs keep their copy buttons.
//!   `speed-up:original-private` is the original order's trade for a swap that delivers to the
//!   private balance on Ethereum instead, shielded by its own settlement.
//!   Each opens on the detail. Its Back button opens My orders, which lists the swap once:
//!   under Open while it is on its way, and under Ended once delivered. After the dialog is
//!   closed, the Private tab on Ethereum shows the status card for `speed-up:two-orders`,
//!   `speed-up:rejected` and `speed-up:original-bridging`. A delivered swap has ended and
//!   leaves the card, as any does.
//! - `setup:<state>` opens the swap dialog, in place of a Public account's form, on the detail
//!   of a synthesized swap that sells 0.05 WETH for USDC and sets up a new stealth account,
//!   on Ethereum with its private session started. The Private tab's Swap opens it too, and is
//!   offered there without private funds. Its review is approved and it has no order
//!   yet. `running` holds the setup at its proof, with a setup job that never answers.
//!   `waiting` holds it at the broadcaster's answer, 27 seconds after it was published, over a
//!   ready route with 3 peers that published the request 6 times, where the stage tells the
//!   re-sends and the time left as of the mode's start; they don't advance. Past five re-sends
//!   the row offers Retry setup…, which stops the wait and opens the retry dialog.
//!   `reconnecting` is that wait while its route reconnects, with 1 of its reconnects used and
//!   3 publishes, so the row tells 2 re-sends and offers no retry.
//!   While a setup job runs, the dialog's title shows its route's chip, whose hover card tells
//!   the route's state, peers and reconnects. The routes are synthesized and never change.
//!   `failed` is a setup that failed at its fee estimate, before anything was published.
//!   `broadcaster-error` and `timeout` are a published setup that a broadcaster refused, or
//!   that no broadcaster answered. `restart` is a setup that was sent and isn't confirmed, as
//!   a restarted wallet shows it, with no stages. `ready` is the confirmed setup, where the
//!   detail shows the order's steps and Place order…. `retired` is a setup whose preparation
//!   found the reserved account used before and retired it: nothing was sent, the detail has
//!   one step that says so, and the footer offers Retry with new account…. That opens the
//!   retry dialog for a new account, with a stand-in estimate that no broadcaster gave, so
//!   its Continue is enabled and opens the review: it names New account for the stealth
//!   account and limits the fee at 0.0002 WETH, above the 0.00015 WETH the swap approved,
//!   which its warning says. Approving the review takes the vault password and ends in an
//!   error on the detail. No account is reserved.
//!   `replaced` is that swap once it went on with a new account: account #5 holds the swap,
//!   which started four minutes ago, and its setup is held at its proof, with a setup job
//!   that never answers. Stealth accounts then shows two synthesized accounts in place of
//!   the wallet's own: #5 as the swap's, and the retired #4 with no swap. The wallet holds
//!   neither, so their actions fail.
//!   `fee-token` is a setup that failed at its fee estimate because the fee token changed: the
//!   swap approved up to 0.52 USDC, and the setup would pay in WETH. Over the detail it opens
//!   the review of that setup sent again, with a stand-in estimate that no broadcaster gave
//!   and limits the fee at 0.0002 WETH, so the review names both limits, the fee token's
//!   change and, at stand-in prices of $3,000 for ETH and $1 for USDC, the change in USD.
//!   Those prices replace the session's cached ones on Ethereum until their next refresh.
//!   Approving the review takes the vault password and ends in an error on the detail.
//!   `setup:two-accounts` is that swap delivered by Across to the private balance on Arbitrum
//!   One, with a new stealth account on each network: the setup on Ethereum was submitted
//!   and waits for its confirmation, and the one on Arbitrum One is at its POI proofs, where
//!   its route is still being prepared, so in built-in Tor the chip reads Isolated route
//!   without a peer count.
//!   `setup:two-waiting` is that swap with both setups waiting for their broadcasters' answers
//!   as `waiting` does, over ready routes with 3 peers on Ethereum and 2 on Arbitrum One: the
//!   chip counts 5 peers, and its hover card lists each network's route.
//!   `setup:destination-failed` is that swap once its setup on Ethereum is confirmed and a
//!   broadcaster refused the one on Arbitrum One.
//!   No private note shows a setup included, so a confirmation never counts blocks. Copy
//!   buttons work. Retry setup… sits on the row a setup failed or waits at, and opens the
//!   retry dialog of that account alone. A wallet without private funds has no broadcaster to
//!   pay there, so the dialog says so and its Continue stays disabled, except in `retired`.
//!   The footer has no Retry setup… or Continue…, and the detail's other actions do nothing.
//! - `setup:included-order-open`, `setup:dropped-order-open` and `setup:dropped-order-expired`
//!   open the swap dialog, in place of a Public account's form, on the detail of a synthesized
//!   swap of 0.05 WETH for USDC back to the private balance on Ethereum, whose one order was
//!   placed while its new stealth account's setup was included and not yet confirmed. The
//!   setup is at nonce 0, which the confirmed nonce read leaves unresolved, and the order's
//!   hooks are at nonces 1 and 2. In `included-order-open` the order is open and the setup is
//!   included 3 blocks below a head of the fixture's own, so the stealth account fact reads
//!   Confirming (3/12 blocks) and the count holds still. Speed up… is offered, as the latest
//!   read that admitted the order offers it, and opens the route of the order's orderbook and
//!   nothing else. In `dropped-order-open` the order is open and nothing shows the setup
//!   included, so the fact says the setup is no longer included and offers Retry setup…,
//!   and Speed up… isn't shown. `dropped-order-expired` is that order once it expired with
//!   its pre-hook never run, so nothing was unshielded, and the fact offers the same Retry
//!   setup…. Under the open order Cancel order… is disabled with its reason while the setup is
//!   unconfirmed. Retry setup… opens the retry dialog of the swap's own account, which on a
//!   wallet without private funds has no broadcaster to pay, as in the other `setup:` states.
//!   The detail's other actions do nothing.
//! - `reported:<state>` opens the swap dialog, in place of a Public account's form, on the
//!   detail of a synthesized swap of 0.05 WETH for USDC on Ethereum whose one order is open on
//!   its record while the orderbook reports it filled for 101.31 USDC. Nothing of the fill is
//!   on the record: it is this session's report, counted from a head block of the fixture's
//!   own, so the count holds still. In every state the reported block was read and holds the
//!   trade. `fill` is a swap back to the private balance, 3 blocks after the reported
//!   settlement, whose block shows the shield of 101.06 USDC. `handoff` is that swap delivered
//!   by Across to a public receiver on Arbitrum One, also 3 blocks after it, whose block shows
//!   the deposit of 100.46 USDC, with no answer from Across yet. `delivered` is the same 2
//!   blocks after it, with Across's report of a delivery of 100 USDC. `shielded` and `held`
//!   are that report for the swap delivered to the private balance on Arbitrum One instead,
//!   through stealth account #12 there, once the fill block on Arbitrum One was read: it
//!   shows the shield, which credits 99.75 USDC privately, or the 100 USDC held by that
//!   account, whose shield didn't run. Copy buttons work, and the detail's other actions do
//!   nothing.
//! - `cancelled:<state>` opens the private form of a new swap that sells WETH for USDC, in place
//!   of a Public account's, on Ethereum with its private session started. It holds two
//!   synthesized stealth accounts: #5 on Ethereum and #12 on Arbitrum One, which a private
//!   Bridge swap of 0.05 WETH for USDC to the private balance on Arbitrum One reserved fresh 15
//!   minutes ago. Both setups are confirmed, and that swap was cancelled before its order with
//!   no signed work left, which stopped its use of each account and retired #12. In
//!   `confirmed` nothing claimed #5 since, so the Source picker lists it under New account,
//!   which stays the default. Choosing #5 shows its reuse warning in place of a setup fee. In
//!   `reused` a later swap of 0.02 WETH for USDC back to the private balance claimed #5 two
//!   minutes ago and has no order yet, so the picker lists New account alone. The form asks
//!   for no quote, so it can't be reviewed, and a change of account estimates no setup fee.
//!   My orders, opened from the form, lists the cancelled swap under Ended, and in `reused`
//!   the later swap too. The cancelled swap's entry opens its detail, which says that #5 can
//!   be chosen for another swap in `confirmed`, that it can't be until no other swap holds it
//!   in `reused`, and that #12 isn't used for another swap. On the later swap's detail,
//!   Cancel preparation… ends in an error, as the wallet's store holds no such swap, and the
//!   other actions do nothing.
//! - `unshield` lets the Private tab's Unshield open a stand-in 1.0 of the network's wrapped
//!   native token when the wallet holds nothing it can unshield, so the form's Output toggle
//!   and gas top-up can be looked at. The network's private session must be started. Nothing
//!   about the asset is real, so its estimates and Unshield fail.
//! - `max:fragmented` and `max:locked` let the Private tab's Send and Unshield offer a
//!   stand-in 12.345678901234567891 of the network's wrapped native token, with its icon, whose
//!   Max is 7.123456789012345678, when the wallet holds nothing it can spend. Swap is offered
//!   too, and the private swap form's Sell panel offers the same stand-in when the wallet holds
//!   nothing it can swap. `max:locked` also shows 0.5 of it locked there. The network's private
//!   session must be started. Nothing about the assets is real, so estimates, quotes and
//!   submission fail or do nothing.
//! - `rotation:in-progress` and `rotation:waku-failed` change what New Tor session does, in
//!   the native popover and from the extension. `in-progress` holds the rotation pending, so
//!   the shared control shows it in progress for as long as the wallet runs. `waku-failed`
//!   ends it at once as if the new session had started and the broadcaster network couldn't
//!   restart. Neither stops Waku or changes the Tor session.
//! - `broadcaster-progress:<state>` lets the Private tab's Send offer a stand-in 1.0 of the
//!   network's wrapped native token, with its icon, when the wallet holds nothing it can send.
//!   The network's private session must be started. Send then opens the "Send via
//!   broadcaster" progress dialog of a synthesized submission in place of the form, with the
//!   wallet's own address as the recipient, held at the response wait: `ready` waits over a
//!   ready route with 3 peers that published the request 4 times and used no reconnect, 17
//!   seconds after the wait began, so the step tells 3 re-sends and the time left as of the
//!   mode's start, and the footer offers Stop waiting. `reconnecting` waits while the route
//!   reconnects, with 1 reconnect used and 3 publishes. `timeout` is that wait ended without
//!   an answer, after both reconnects: the step shows the timed-out message and the chip
//!   reads Route closed. `direct` is `ready` in direct network mode, where the chip reads Not
//!   isolated, whatever the wallet's network mode is. The route's chip and its hover card show
//!   the synthesized route, which never changes. Stop waiting marks the wait stopped, and
//!   Close closes the dialog. No job runs, nothing is proved or sent, and no record is written.
//! - `hardware-derive` shows the Derive from private dialog, from Public's Add account, as a
//!   hardware wallet's: its authorize state with Show addresses, its preview and the progress
//!   of an add. Whatever wallet is selected, the dialog treats it as Ledger-derived and asks a
//!   stand-in Ledger, which needs no unlocked hardware profile. Show addresses answers after
//!   about 2 seconds with a fixed account key, the compressed secp256k1 generator with a fixed
//!   chain code, so every wallet previews the same addresses. Add holds forever on the device
//!   confirmation of the first selected index that has no account, which the stand-in never
//!   answers. Close the dialog to leave it. Nothing is read from a device, no account is
//!   stored, and nothing is sent.
//!
//! In every mode the jobs of a swap are scripted or held back: nothing is claimed, proved,
//! signed, paid or sent, and no record is written. The synthesized records live in memory
//! only, and nothing reads, tracks or acts on them. A `retry-review` or speed-up mode, and the
//! Speed up… of `setup:included-order-open`, opens the route its orderbook client would use,
//! and sends nothing on it. A `quote-error`, `native-buy` or `cancelled:` mode opens no route. A `reported:`
//! mode asks no orderbook, chain or bridge provider about its swap. The review of a
//! `setup:fee-token` or `setup:retired` mode holds the broadcaster network's client, as any
//! setup's review does, and publishes nothing on it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes, U256, address, keccak256};
use gpui::{App, Context, Window};
use railgun_ui::{
    KnownTokenInfo, format_scaled_amount, format_token_amount, known_tokens_for_chain,
    token_icon_asset_path,
};
use wallet_ops::cow::{CowOrderStatusHint, CowOrderStatusReport, OrderLimitError, OrderUid};
use wallet_ops::settings::SwapProfile;
use wallet_ops::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, BridgePrivateDelivery, BridgeProvider,
    BridgeShieldFailure, BridgeSurplus, ExecutorNonceObservation, ExecutorNonceWatermark,
    ExecutorOperationId, ExecutorPayloadContext, ExecutorPayloadPurpose, ExecutorRecord,
    IssuedExecutorPayload, PublicSwapApproval, PublicSwapDeposited, PublicSwapIntent,
    PublicSwapObservations, PublicSwapRecord, SwapApproval, SwapApprovalTokens,
    SwapApprovedAccount, SwapApprovedAccounts, SwapApprovedBounds, SwapBridgeHandoff,
    SwapBridgeOutcome, SwapDelivery, SwapObservation, SwapOrderObservations, SwapPreHookDeath,
    SwapPreHookDeathCause, SwapProof, SwapRecipient, SwapSetupFeeLimit, SwapTerms,
    SwapTradeAmounts, SwapUseId,
};
use wallet_ops::{
    BlockedShieldRescueBlocker, BlockedShieldRescueInfo, BlockedShieldRescueUtxoId,
    ExecutorRecoveryFeeEstimate, OperationNetworkIsolation, PairAnchorRate,
    PublicBroadcasterResultKind, SUBMISSION_ROUTE_RECOVERY_LIMIT, SubmissionRouteProgress,
    SubmissionRouteSender, SwapBridgeReport, SwapContinuingApproval, SwapDeliverySeen,
    SwapHandoffHint, SwapQuoteAnchor, SwapQuoteDiagnostics, SwapReview, SwapReviewChange,
    SwapSettlementHint, SwapSpeedUpRefusal, TransactionGenerationStage, UtxoPpoiState,
};

use super::dialog::SwapDialogView;
use super::form::SetupSide;
use super::model::SwapIdentity;
use super::setup_progress::SetupProgress;
use super::{PrivateSwapsView, SwapEarlyHint, SwapJob, SwapJobKind, SwapOrderHint, now_unix};
use crate::assets::WalletIconSource;
use crate::root::WalletRoot;
use crate::root::private_broadcaster::{
    apply_private_broadcaster_progress_stage, finish_private_broadcaster_progress_steps_at_stage,
};
use crate::root::utxo::{BlockedShieldRefundDialogState, UtxoDisplayRow, UtxoFinalityContext};

const VARIABLE: &str = "RAILOXIDE_UI_FIXTURE";
/// How long a scripted job takes before it answers.
const BEAT: Duration = Duration::from_secs(2);
/// The blocked Shield a `refund:` mode opens its dialog for. No wallet holds it.
const REFUND_UTXO: BlockedShieldRescueUtxoId = BlockedShieldRescueUtxoId {
    tree: u32::MAX,
    position: u64::MAX,
    commitment: B256::ZERO,
    blinded_commitment: B256::ZERO,
};
/// The head block the `utxos` mode counts finality from.
const UTXO_HEAD: u64 = 21_480_215;
/// The finality depth of the `utxos` mode, whose safe head is that many blocks below its head.
const UTXO_FINALITY_DEPTH: u64 = 12;

#[derive(Clone, Copy, Debug)]
enum Mode {
    Form,
    RouteError,
    Fee(Fee),
    Flow(Flow),
    Detail(Detail),
    Accounts,
    Refund(Refund),
    Utxos,
    Unshield,
    Max(Max),
    Rotation { waku_failed: bool },
    RetryReview,
    QuoteError,
    NativeBuy,
    Split(Split),
    SpeedUp(SpeedUp),
    Setup(Setup),
    SetupUnderOrder(SetupUnderOrder),
    Reported(Reported),
    Cancelled(Cancelled),
    BroadcasterProgress(BroadcasterProgress),
    HardwareDerive,
}

/// Where a `broadcaster-progress:` mode holds its synthesized Send via broadcaster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BroadcasterProgress {
    /// Waiting for the broadcaster's answer over a ready route.
    Ready,
    /// Waiting while the route reconnects.
    Reconnecting,
    /// The wait ended without an answer.
    Timeout,
    /// Waiting in direct network mode, with no route of its own.
    Direct,
}

/// Whether a `max:` mode's stand-in Sell asset has locked notes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Max {
    Fragmented,
    Locked,
}

/// What a `cancelled:` mode holds of stealth account #5 once the fresh source preparation that
/// reserved it was cancelled with its setup confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cancelled {
    /// Nothing claimed the account since, so a new swap can choose it.
    Confirmed,
    /// A later swap claimed the account and has no order yet.
    Reused,
}

/// What a `setup:` mode holds of the swap's own setup below its one order, which was placed
/// while that setup was included and not yet confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupUnderOrder {
    /// The order is open and the setup is still included.
    Included,
    /// The order is open and nothing shows the setup included any more.
    Dropped,
    /// The order expired, its pre-hook never ran, and the setup never returned.
    Expired,
}

/// What a `reported:` mode's session holds about the synthesized private swap's open order,
/// which the orderbook reports filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reported {
    /// A swap back to the private balance whose shield was read from the reported block.
    Fill,
    /// A Bridge swap whose deposit was read from the reported block, with no provider report.
    Handoff,
    /// A Bridge swap whose provider reports the delivery.
    Delivered,
    /// A private Bridge swap whose reported delivery's fill block shows the shield.
    Shielded,
    /// A private Bridge swap whose reported delivery's fill block shows the proceeds held by
    /// the destination stealth account.
    Held,
}

/// Where a `setup:` mode holds the setup of the synthesized private swap's stealth accounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setup {
    Running,
    /// Published, and waiting for a broadcaster's answer for 27 seconds.
    Waiting,
    /// Waiting as `Waiting` does while its route reconnects.
    Reconnecting,
    /// The setup failed before it was published.
    Failed,
    BroadcasterError,
    Timeout,
    /// Sent and unconfirmed, with none of this session's stages.
    Restart,
    Ready,
    /// Preparation found the reserved account used before and retired it. Nothing was sent.
    Retired,
    /// The swap went on with a new account in place of a retired one, whose setup is running.
    Replaced,
    /// The setup failed because its fee token changed, and the review of its retry is open.
    FeeToken,
    /// A private Bridge swap whose two new accounts are each at another point of their setup.
    TwoAccounts,
    /// A private Bridge swap whose two new accounts' setups both wait for a broadcaster's
    /// answer.
    TwoWaiting,
    /// A private Bridge swap whose own setup is confirmed and whose destination setup a
    /// broadcaster refused.
    DestinationFailed,
}

/// What a speed-up mode shows of the synthesized private swap: the speed-up's form, the detail
/// of the swap with its open retry, the detail with two orders that can still fill, or the
/// detail once one of those two traded and the other can't.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpeedUp {
    Review,
    Open,
    /// The open retry was signed just now, so the detail doesn't offer Speed up yet.
    JustPlaced,
    TwoOrders,
    /// Two orders that can still fill, the speed-up's submission rejected by the orderbook.
    Rejected,
    /// The original order traded and made its own deposit, which Across hasn't filled yet.
    OriginalBridging,
    /// The original order traded and its own deposit was delivered.
    OriginalDelivered,
    /// The original order of a swap to the private balance traded and was shielded.
    OriginalPrivate,
    /// The speed-up traded and its own deposit was delivered.
    ReplacementDelivered,
}

/// What the detail of a `split:` mode's private swap shows of its Across deposit, which the
/// expired first order holds while the retry holds the trade. `Unverified` is one order whose
/// trade showed no deposit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Split {
    Bridging,
    Delivered,
    Refunding,
    RefundVerified,
    Unverified,
}

/// Where the Buy picker holds a network's setup-fee check.
#[derive(Clone, Copy, Debug)]
enum Fee {
    Checking,
    Failed,
}

/// What the refund dialog of a `refund:` mode shows: the origin check, an available refund,
/// or why the blocked Shield can't be refunded.
#[derive(Clone, Copy, Debug)]
enum Refund {
    Checking,
    Available,
    Unknown,
    Stealth,
    Inactive,
    Unresolved,
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
    if value == "utxos" {
        return Some(Mode::Utxos);
    }
    if value == "unshield" {
        return Some(Mode::Unshield);
    }
    if value == "hardware-derive" {
        return Some(Mode::HardwareDerive);
    }
    if let Some(state) = value.strip_prefix("max:") {
        return Some(Mode::Max(match state {
            "fragmented" => Max::Fragmented,
            "locked" => Max::Locked,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("broadcaster-progress:") {
        return Some(Mode::BroadcasterProgress(match state {
            "ready" => BroadcasterProgress::Ready,
            "reconnecting" => BroadcasterProgress::Reconnecting,
            "timeout" => BroadcasterProgress::Timeout,
            "direct" => BroadcasterProgress::Direct,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("rotation:") {
        return Some(Mode::Rotation {
            waku_failed: match state {
                "in-progress" => false,
                "waku-failed" => true,
                _ => return None,
            },
        });
    }
    if value == "retry-review" {
        return Some(Mode::RetryReview);
    }
    if value == "quote-error" {
        return Some(Mode::QuoteError);
    }
    if value == "native-buy" {
        return Some(Mode::NativeBuy);
    }
    if value == "speed-up-review" {
        return Some(Mode::SpeedUp(SpeedUp::Review));
    }
    if let Some(state) = value.strip_prefix("speed-up:") {
        return Some(Mode::SpeedUp(match state {
            "open" => SpeedUp::Open,
            "just-placed" => SpeedUp::JustPlaced,
            "two-orders" => SpeedUp::TwoOrders,
            "rejected" => SpeedUp::Rejected,
            "original-bridging" => SpeedUp::OriginalBridging,
            "original-delivered" => SpeedUp::OriginalDelivered,
            "original-private" => SpeedUp::OriginalPrivate,
            "replacement-delivered" => SpeedUp::ReplacementDelivered,
            _ => return None,
        }));
    }
    if let Some(stage) = value.strip_prefix("split:") {
        return Some(Mode::Split(match stage {
            "bridging" => Split::Bridging,
            "delivered" => Split::Delivered,
            "refunding" => Split::Refunding,
            "refund-verified" => Split::RefundVerified,
            "unverified" => Split::Unverified,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("setup:") {
        let under_order = match state {
            "included-order-open" => Some(SetupUnderOrder::Included),
            "dropped-order-open" => Some(SetupUnderOrder::Dropped),
            "dropped-order-expired" => Some(SetupUnderOrder::Expired),
            _ => None,
        };
        if let Some(under_order) = under_order {
            return Some(Mode::SetupUnderOrder(under_order));
        }
        return Some(Mode::Setup(match state {
            "running" => Setup::Running,
            "waiting" => Setup::Waiting,
            "failed" => Setup::Failed,
            "broadcaster-error" => Setup::BroadcasterError,
            "timeout" => Setup::Timeout,
            "restart" => Setup::Restart,
            "ready" => Setup::Ready,
            "retired" => Setup::Retired,
            "replaced" => Setup::Replaced,
            "fee-token" => Setup::FeeToken,
            "two-accounts" => Setup::TwoAccounts,
            "two-waiting" => Setup::TwoWaiting,
            "reconnecting" => Setup::Reconnecting,
            "destination-failed" => Setup::DestinationFailed,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("reported:") {
        return Some(Mode::Reported(match state {
            "fill" => Reported::Fill,
            "handoff" => Reported::Handoff,
            "delivered" => Reported::Delivered,
            "shielded" => Reported::Shielded,
            "held" => Reported::Held,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("cancelled:") {
        return Some(Mode::Cancelled(match state {
            "confirmed" => Cancelled::Confirmed,
            "reused" => Cancelled::Reused,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("fee:") {
        return Some(Mode::Fee(match state {
            "checking" => Fee::Checking,
            "failed" => Fee::Failed,
            _ => return None,
        }));
    }
    if let Some(case) = value.strip_prefix("refund:") {
        return Some(Mode::Refund(match case {
            "checking" => Refund::Checking,
            "available" => Refund::Available,
            "unknown" => Refund::Unknown,
            "stealth" => Refund::Stealth,
            "inactive" => Refund::Inactive,
            "unresolved" => Refund::Unresolved,
            _ => return None,
        }));
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

/// Whether the Buy picker holds a network's setup-fee check, and whether that check failed.
/// `None` in a mode that holds none, where a network takes a new account.
pub(super) fn setup_fee_check_failed() -> Option<bool> {
    match mode()? {
        Mode::Fee(fee) => Some(matches!(fee, Fee::Failed)),
        _ => None,
    }
}

/// Whether the Public account swap records are the fixture's own, which nothing reads again,
/// tracks or acts on. A `flow:` mode has none until its review is approved.
pub(super) fn holds_records() -> bool {
    matches!(mode(), Some(Mode::Detail(_) | Mode::Flow(_)))
}

/// Whether the private swap records are the fixture's own, which nothing reads again, quotes,
/// tracks or acts on. A `retry-review` or `speed-up-review` mode has none until its form is
/// opened, and a `quote-error` mode has none at all: its form holds a failed quote, which no
/// request replaces.
pub(super) fn holds_swaps() -> bool {
    matches!(
        mode(),
        Some(
            Mode::RetryReview
                | Mode::QuoteError
                | Mode::NativeBuy
                | Mode::Split(_)
                | Mode::SpeedUp(_)
                | Mode::Setup(_)
                | Mode::SetupUnderOrder(_)
                | Mode::Reported(_)
                | Mode::Cancelled(_)
        )
    )
}

/// The block a `setup:included-order-open` mode holds its swap's own setup included in, as
/// private sync would show it, and the head that setup's confirmations count from in place of
/// the synced one, which nothing finalizes. `None` in every other mode.
pub(super) fn setup_included() -> Option<(u64, u64)> {
    matches!(mode()?, Mode::SetupUnderOrder(SetupUnderOrder::Included))
        .then_some((SETUP_INCLUDED_BLOCK, SETUP_INCLUDED_BLOCK + 3))
}

/// The head block a `reported:` mode counts its reported settlement's confirmations from, in
/// place of the synced head. `None` in every other mode.
pub(super) fn reported_head() -> Option<u64> {
    let Mode::Reported(reported) = mode()? else {
        return None;
    };
    Some(REPORTED_TRADE_BLOCK + reported.confirmations())
}

/// What approving the review of a `retry-review`, speed-up, `setup:fee-token` or
/// `setup:retired` mode ends in, in place of placing its order or sending its setup. `None` in
/// every other mode.
pub(super) fn review_refusal() -> Option<&'static str> {
    matches!(
        mode(),
        Some(Mode::RetryReview | Mode::SpeedUp(_) | Mode::Setup(Setup::FeeToken | Setup::Retired))
    )
    .then_some("Fixture: this mode stops at the review. Nothing was sent.")
}

/// What the owner's rule would answer about speeding up the synthesized swap of a speed-up or
/// `setup:included-order-open` mode, which no owner holds: `Some(Ok(()))` offers the speed-up.
/// `None` in every other mode.
pub(super) fn speed_up_refusal() -> Option<Result<(), SwapSpeedUpRefusal>> {
    match mode()? {
        Mode::SpeedUp(SpeedUp::TwoOrders | SpeedUp::Rejected) => {
            Some(Err(SwapSpeedUpRefusal::TwoOrdersFillable))
        }
        // The latest read that admitted the order on its included setup admits its speed-up.
        Mode::SpeedUp(SpeedUp::Review | SpeedUp::Open | SpeedUp::JustPlaced)
        | Mode::SetupUnderOrder(SetupUnderOrder::Included) => Some(Ok(())),
        _ => None,
    }
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
        (
            Mode::Form
            | Mode::RouteError
            | Mode::Fee(_)
            | Mode::Detail(_)
            | Mode::Accounts
            | Mode::Refund(_)
            | Mode::RetryReview
            | Mode::QuoteError
            | Mode::Split(_)
            | Mode::Setup(_)
            | Mode::SetupUnderOrder(_),
            Phase::Approved,
        ) => (
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

/// The network the private swap of a `retry-review`, `split:`, speed-up, `setup:`,
/// `reported:` or `cancelled:` mode sells on.
const SWAP_ORIGIN: u64 = 1;
/// The block the synthesized private swap's observations count from.
const SWAP_ORIGIN_BLOCK: u64 = 24_612_480;
const WETH: Address = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
const USDC: Address = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
/// The swap's stealth account. No wallet holds it.
const SWAP_ACCOUNT: Address = address!("b41d7e0c95a23f6841c7d02e9a5b63f18c4e7a2d");
/// Across delivers the swap's USDC on Arbitrum One to a public receiver no wallet holds.
const SWAP_DELIVERY: BridgeDelivery = BridgeDelivery {
    provider: BridgeProvider::Across,
    destination_chain: 42_161,
    receiver: address!("7f2c9d3a41b8e65c0d9a4b17e3f8a6c25d1b09e4"),
    destination_token: address!("af88d065e77c8cc2239327c5edb3a432268e5831"),
    surplus: BridgeSurplus::Reshield,
    private: None,
};
/// The private spend, 0.05 WETH, and what Railgun's unshield fee leaves of it to sell.
const SWAP_SPEND: u64 = 50_000_000_000_000_000;
const SWAP_SELL: u64 = 49_875_000_000_000_000;
/// The USDC each order deposits with Across and the USDC that deposit delivers: 90 for the
/// first order, 100 for its retry, and 95 for the retry's speed-up.
const FIRST_ORDER: (u64, u64) = (90_420_000, 90_000_000);
const RETRY: (u64, u64) = (100_460_000, 100_000_000);
const SPED_UP: (u64, u64) = (95_440_000, 95_000_000);
/// What the trade bought, 101.31 USDC.
const SWAP_BOUGHT: u64 = 101_312_480;
/// What a `reported:fill` mode's shield credits privately, 101.06 USDC: the bought amount less
/// the 0.25% shield fee.
const REPORTED_SHIELDED: u64 = 101_059_199;

/// The block the orderbook reports a `reported:` mode's trade in.
const REPORTED_TRADE_BLOCK: u64 = SWAP_ORIGIN_BLOCK + 160;

impl Reported {
    /// How many blocks the head is past the reported trade block.
    const fn confirmations(self) -> u64 {
        match self {
            Self::Fill | Self::Handoff => 3,
            Self::Delivered | Self::Shielded | Self::Held => 2,
        }
    }

    /// Whether Across reports the swap's delivery.
    const fn delivered(self) -> bool {
        matches!(self, Self::Delivered | Self::Shielded | Self::Held)
    }

    /// Whether the swap delivers to the private balance on Arbitrum One.
    const fn private(self) -> bool {
        matches!(self, Self::Shielded | Self::Held)
    }
}

/// The record of a `reported:` mode's swap: one open order, signed five minutes ago, with
/// nothing observed. `Fill` delivers to the private balance on Ethereum, `Shielded` and `Held`
/// by Across to the private balance on Arbitrum One with a 0.25% shield fee there, and the
/// other states by Across to the public receiver on Arbitrum One.
fn staged_reported(reported: Reported) -> Option<ExecutorRecord> {
    let signed = now_unix().saturating_sub(300);
    let open = serde_json::to_value(SwapOrderObservations::default()).ok()?;
    let mut order = staged_order(0, signed, RETRY, 0xa1, &open)?;
    if reported == Reported::Fill {
        order["delivery"] = serde_json::to_value(SwapDelivery::Reshield).ok()?;
        order["bridge"] = serde_json::Value::Null;
        order["bounds"]["destination_minimum"] = serde_json::Value::Null;
    }
    if reported.private() {
        order["delivery"] = serde_json::to_value(SwapDelivery::Bridge(PRIVATE_DELIVERY)).ok()?;
        order["bounds"]["destination_shield_fee_bps"] = serde_json::json!(U256::from(25_u8));
    }
    staged_swap_record(&[order], 1, signed)
}

/// Order `attempt` of the synthesized private swap, as its record stores it: signed at `signed`
/// for 20 minutes, with a deposit that Across has four hours to fill. `hash` tells its ID and
/// its hooks, at nonces 1 and 2, apart from another order's.
fn staged_order(
    attempt: u32,
    signed: u64,
    (deposit, minimum): (u64, u64),
    hash: u8,
    observations: &serde_json::Value,
) -> Option<serde_json::Value> {
    let quoted = u32::try_from(signed).ok()?;
    let uid = OrderUid::new(keccak256([hash]), SWAP_ACCOUNT, quoted.checked_add(1_200)?);
    Some(serde_json::json!({
        "attempt": attempt, "uid": uid.0, "delivery": SwapDelivery::Bridge(SWAP_DELIVERY),
        "bounds": {
            "sell_amount": U256::from(SWAP_SELL), "unshield_amount": U256::from(SWAP_SPEND),
            "unshield_fee_bps": U256::from(25_u8), "buy_amount": U256::from(deposit),
            "private_minimum": U256::from(deposit), "shield_fee_bps": U256::ZERO,
            "slippage_bps": 50, "pre_hook_gas_limit": 1_045_000, "post_hook_gas_limit": 396_000,
            "anchors": [], "destination_minimum": U256::from(minimum), "gas_share_bps": 2_500,
            "gas_estimate": U256::from(492_000_u32), "gas_allowance": U256::from(123_000_u32),
            "gas_price_wei": 120_000_000, "valid_for_secs": 1_200
        },
        "pre_hook": { "nonce": U256::ONE, "payload": B256::repeat_byte(hash) },
        "post_hook": { "nonce": U256::from(2_u8), "payload": B256::repeat_byte(hash + 1) },
        "invalidates": null, "observations": observations, "submission_status": "Accepted",
        "bridge": BridgeOrderTerms::Across(AcrossOrderTerms {
            spoke_pool: address!("5c7bcd6e7de5423a257d81b442095a1a6ced35c5"),
            input_token: USDC,
            output_token: SWAP_DELIVERY.destination_token,
            input_amount: U256::from(deposit),
            output_amount: U256::from(minimum),
            quote_timestamp: quoted,
            fill_deadline: quoted.checked_add(14_400)?,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
            recipient: None,
            message_hash: None,
        }),
    }))
}

/// The record of the synthesized private swap, decoded from the JSON a stored record has.
/// Without `split`, its first order expired and nothing ran. With it, the retry traded and its
/// settlement made the first order's deposit, whose outcome the stage names. `Unverified` is
/// one order instead, whose trade showed no deposit. With `speed_up` and no `split`, the
/// expired first order is followed by its open retry, or for `TwoOrders` the swap holds the
/// retry and its speed-up, both open. `Rejected` is `TwoOrders` with the speed-up's submission
/// rejected. The other speed-up stages hold those two orders once one of them traded, as
/// [`staged_overlap`] stages them.
fn staged_swap(split: Option<Split>, speed_up: Option<SpeedUp>) -> Option<ExecutorRecord> {
    let now = now_unix();
    // A refund follows the deposit's fill deadline.
    let refunds = matches!(split, Some(Split::Refunding | Split::RefundVerified));
    let signed = now.saturating_sub(if refunds { 18_000 } else { 3_000 });
    let at = |block: u64, transaction: Option<&str>| SwapObservation {
        block: BlockNumHash::new(SWAP_ORIGIN_BLOCK + block, keccak256(block.to_be_bytes())),
        transaction_hash: transaction.map(keccak256),
    };
    let value = |observed: SwapOrderObservations| serde_json::to_value(observed).ok();
    // The first order's 20 minutes are 100 blocks, and the retry is signed 25 blocks later.
    let expired = SwapPreHookDeath {
        cause: SwapPreHookDeathCause::Expired,
        observation: at(110, None),
    };
    let single = split == Some(Split::Unverified);
    let settlement = at(if single { 40 } else { 160 }, Some("settlement"));
    let traded = SwapOrderObservations {
        traded: Some(settlement),
        trade_amounts: Some(SwapTradeAmounts {
            sell_amount: U256::from(SWAP_SELL),
            buy_amount: U256::from(SWAP_BOUGHT),
            fee_amount: U256::ZERO,
            settlement_gas_used: Some(1_386_204),
            settlement_effective_gas_price: Some(410_000_000),
            executed_fee: Some(U256::from(11_400_000_000_000_u64)),
            executed_fee_token: Some(WETH),
        }),
        ..Default::default()
    };
    // The orders, and the account's nonce once their hooks ran.
    let (orders, nonce) = match split {
        None => {
            let ended = value(SwapOrderObservations {
                pre_hook_dead: Some(expired),
                expired: Some(expired.observation),
                ..Default::default()
            })?;
            let open = value(SwapOrderObservations::default())?;
            // An open order was signed five minutes ago, and the one before it five earlier.
            // A just-placed one is signed now.
            let orders = match speed_up {
                None => vec![staged_order(0, signed, FIRST_ORDER, 0xa1, &ended)?],
                Some(SpeedUp::Review | SpeedUp::Open) => vec![
                    staged_order(0, signed, FIRST_ORDER, 0xa1, &ended)?,
                    staged_order(1, now.saturating_sub(300), RETRY, 0xb1, &open)?,
                ],
                Some(SpeedUp::JustPlaced) => vec![
                    staged_order(0, signed, FIRST_ORDER, 0xa1, &ended)?,
                    staged_order(1, now, RETRY, 0xb1, &open)?,
                ],
                Some(SpeedUp::TwoOrders | SpeedUp::Rejected) => {
                    let mut placed =
                        staged_order(1, now.saturating_sub(300), SPED_UP, 0xb1, &open)?;
                    if speed_up == Some(SpeedUp::Rejected) {
                        placed["submission_status"] = serde_json::json!("Rejected");
                    }
                    vec![
                        staged_order(0, now.saturating_sub(600), RETRY, 0xa1, &open)?,
                        placed,
                    ]
                }
                Some(won) => return staged_overlap(won, settlement, &traded, now),
            };
            (orders, 1_u8)
        }
        // The pre-hook ran and the post-hook, which deposits, didn't.
        Some(Split::Unverified) => {
            let traded = value(SwapOrderObservations {
                pre_hook_executed: Some(settlement),
                ..traded
            })?;
            (vec![staged_order(0, signed, RETRY, 0xa1, &traded)?], 2)
        }
        Some(split) => {
            let outcome = match split {
                Split::Delivered => Some(SwapBridgeOutcome::DeliveredVerified {
                    block: BlockNumHash::new(402_118_977, keccak256("fill block")),
                    transaction_hash: keccak256("fill"),
                    output_amount: U256::from(FIRST_ORDER.1),
                    shielded: false,
                }),
                Split::Refunding | Split::RefundVerified => Some(SwapBridgeOutcome::Refunding),
                Split::Bridging | Split::Unverified => None,
            };
            let mut deposit = value(SwapOrderObservations {
                pre_hook_dead: Some(expired),
                delivered: Some(settlement),
                bridge_handoff: Some(SwapBridgeHandoff {
                    observation: settlement,
                    deposit_id: Some(U256::from(3_481_207_u32)),
                }),
                bridge_outcome: outcome,
                bridge_refund: (split == Split::RefundVerified).then(|| at(1_420, Some("refund"))),
                ..Default::default()
            })?;
            // The settlement reshielded what the trade bought beyond the deposit, less the
            // shield fee.
            deposit["settlement_credit"] = serde_json::json!({
                "observation": settlement,
                "private_amount": U256::from(10_865_249_u32), "fee": U256::from(27_231_u32)
            });
            (
                vec![
                    staged_order(0, signed, FIRST_ORDER, 0xa1, &deposit)?,
                    staged_order(1, signed + 1_500, RETRY, 0xb1, &value(traded)?)?,
                ],
                3,
            )
        }
    };
    staged_swap_record(&orders, nonce, signed)
}

/// The synthesized swap's two competing orders once one of them traded, which `won` names:
/// the original, signed ten minutes ago, and its speed-up, signed five minutes ago at the same
/// pre-hook nonce. The traded order holds `traded` at `settlement` with its own delivery, and
/// the other holds nothing, so it reads as not filled.
fn staged_overlap(
    won: SpeedUp,
    settlement: SwapObservation,
    traded: &SwapOrderObservations,
    now: u64,
) -> Option<ExecutorRecord> {
    let private = won == SpeedUp::OriginalPrivate;
    let (terms, delivers) = if won == SpeedUp::ReplacementDelivered {
        (SPED_UP, true)
    } else {
        (RETRY, won == SpeedUp::OriginalDelivered)
    };
    let mut filled = serde_json::to_value(SwapOrderObservations {
        pre_hook_executed: Some(settlement),
        delivered: Some(settlement),
        bridge_handoff: (!private).then_some(SwapBridgeHandoff {
            observation: settlement,
            deposit_id: Some(U256::from(3_481_207_u32)),
        }),
        bridge_outcome: delivers.then(|| SwapBridgeOutcome::DeliveredVerified {
            block: BlockNumHash::new(402_118_977, keccak256("fill block")),
            transaction_hash: keccak256("fill"),
            output_amount: U256::from(terms.1),
            shielded: false,
        }),
        ..*traded
    })
    .ok()?;
    // The settlement shielded everything a private swap bought, and for a Bridge swap what
    // the trade bought beyond the deposit, less the shield fee.
    if private {
        filled["shielded"] = serde_json::json!({
            "observation": settlement,
            "private_amount": U256::from(SWAP_BOUGHT), "fee": U256::ZERO
        });
    } else {
        let surplus = SWAP_BOUGHT.checked_sub(terms.0)?;
        let fee = surplus / 400;
        filled["settlement_credit"] = serde_json::json!({
            "observation": settlement,
            "private_amount": U256::from(surplus - fee), "fee": U256::from(fee)
        });
    }
    let unfilled = serde_json::to_value(SwapOrderObservations::default()).ok()?;
    let (original, speed_up) = if won == SpeedUp::ReplacementDelivered {
        (&unfilled, &filled)
    } else {
        (&filled, &unfilled)
    };
    let mut orders = vec![
        staged_order(0, now.saturating_sub(600), RETRY, 0xa1, original)?,
        staged_order(1, now.saturating_sub(300), SPED_UP, 0xb1, speed_up)?,
    ];
    if private {
        for order in &mut orders {
            order["delivery"] = serde_json::to_value(SwapDelivery::Reshield).ok()?;
            order["bridge"] = serde_json::Value::Null;
            order["bounds"]["destination_minimum"] = serde_json::Value::Null;
        }
    }
    staged_swap_record(&orders, 3, now.saturating_sub(600))
}

/// The synthesized swap's record around `orders`, whose hooks left the account at `nonce`,
/// reserved five minutes before `signed`.
fn staged_swap_record(
    orders: &[serde_json::Value],
    nonce: u8,
    signed: u64,
) -> Option<ExecutorRecord> {
    serde_json::from_value(staged_swap_json(orders, nonce, signed)?).ok()
}

/// [`staged_swap_record`] as the JSON a stored record has.
fn staged_swap_json(
    orders: &[serde_json::Value],
    nonce: u8,
    signed: u64,
) -> Option<serde_json::Value> {
    use ExecutorPayloadPurpose::{Operation, SwapPostHook, SwapPreHook};
    let mut issued = vec![staged_payload(0, 0xa0, Operation, Some(0xc0))];
    for hash in [0xa1, 0xb1].into_iter().take(orders.len()) {
        issued.push(staged_payload(1, hash, SwapPreHook, None));
        issued.push(staged_payload(2, hash + 1, SwapPostHook, None));
    }
    Some(serde_json::json!({
        "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": ExecutorOperationId::random().ok()?, "index": 4,
        "address": SWAP_ACCOUNT, "delegate": Address::repeat_byte(0x5d), "retired": false,
        "created_at": signed.saturating_sub(300),
        "issued": issued.into_iter().collect::<Option<Vec<_>>>()?,
        "nonce_watermark": ExecutorNonceWatermark::new(
            U256::from(nonce),
            SWAP_ORIGIN_BLOCK + 1_500,
        ),
        "swap": {
            "terms": SwapTerms::new(
                WETH,
                USDC,
                SwapRecipient::new(U256::ONE, [0; 32]),
                B256::repeat_byte(0xa0),
            ),
            "proof": SwapProof::new(keccak256("proof"), Vec::new()),
            "orders": orders,
        },
    }))
}

/// The block a `setup:included-order-open` mode's private sync would show its swap's own setup
/// in, just past the confirmed nonce read of its record.
const SETUP_INCLUDED_BLOCK: u64 = SWAP_ORIGIN_BLOCK + 1_503;

/// The record of a `setup:` mode's swap of 0.05 WETH for USDC back to the private balance,
/// whose one order was placed on its own setup while that setup was included: the setup at
/// nonce 0, which the record's confirmed nonce read leaves unresolved, and the order's hooks at
/// nonces 1 and 2. The order was signed five minutes ago, or with `expired` 25 minutes ago, past
/// its 20-minute validity, and ended at its expiry with its pre-hook never run.
fn staged_setup_under_order(expired: bool) -> Option<ExecutorRecord> {
    let signed = now_unix().saturating_sub(if expired { 1_500 } else { 300 });
    let observations = if expired {
        let observation = SwapObservation {
            block: BlockNumHash::new(SWAP_ORIGIN_BLOCK + 1_600, keccak256("expired")),
            transaction_hash: None,
        };
        SwapOrderObservations {
            pre_hook_dead: Some(SwapPreHookDeath {
                cause: SwapPreHookDeathCause::Expired,
                observation,
            }),
            expired: Some(observation),
            ..Default::default()
        }
    } else {
        SwapOrderObservations::default()
    };
    let mut order = staged_order(
        0,
        signed,
        RETRY,
        0xa1,
        &serde_json::to_value(observations).ok()?,
    )?;
    order["delivery"] = serde_json::to_value(SwapDelivery::Reshield).ok()?;
    order["bridge"] = serde_json::Value::Null;
    order["bounds"]["destination_minimum"] = serde_json::Value::Null;
    let mut record = staged_swap_json(&[order], 0, signed)?;
    // The account was last read at nonce 0, below the setup's inclusion.
    record["nonce_observation"] = serde_json::to_value(ExecutorNonceObservation::new(
        BlockNumHash::new(SWAP_ORIGIN_BLOCK + 1_500, keccak256("observed")),
        U256::ZERO,
    ))
    .ok()?;
    // The approval saved with the swap, which keeps the fee limit of a setup sent again.
    record["swap_approval"] = serde_json::to_value(staged_setup_approval(None)?).ok()?;
    serde_json::from_value(record).ok()
}

/// What a `quote-error` mode's form is opened to sell, to buy and for which amount, and the
/// failure it holds in place of a quote: a pricing failure with the values of a quote on BNB
/// Chain, whose pair has no price anchor. `None` in every other mode.
pub(super) fn quote_error() -> Option<(Address, Address, U256, eyre::Report)> {
    if !matches!(mode()?, Mode::QuoteError) {
        return None;
    }
    let diagnostics = SwapQuoteDiagnostics {
        chain_id: 56,
        // USDC and USDT on BNB Chain, which both have 18 decimals.
        sell_token: address!("8ac76a51cc950d9822d68b83fe1ad97b32cd580d"),
        buy_token: address!("55d398326f99059ff775485246999027b3197955"),
        sell_amount: U256::from(100_000_000_000_000_000_000_u128),
        buy_amount: U256::from(99_912_504_117_302_255_616_u128),
        sell_token_price: "0.0013443372127831241559514641181749539100565016269683837890625"
            .to_owned(),
        gas_amount: "163218".to_owned(),
        gas_price: "50000000".to_owned(),
        anchor: SwapQuoteAnchor::NotConfigured,
    };
    Some((
        WETH,
        USDC,
        U256::from(SWAP_SPEND),
        eyre::Report::from(OrderLimitError::QuotePriceOutOfRange).wrap_err(diagnostics),
    ))
}

/// The record and the review a `retry-review` mode holds its form on: the swap's expired first
/// order, and a quote for its retry that delivers 100 USDC with no continuing approval. A
/// `speed-up-review`, `speed-up:open` or `speed-up:just-placed` mode holds the swap with that
/// retry open instead, a quote that delivers 95 USDC for a lower deposit while the first
/// order's approval of 90 can still decide the delivery, and the retry's ID, the order the
/// form speeds up. `None` in every other mode.
pub(super) fn retry_review(
    profile: &SwapProfile,
    isolation: OperationNetworkIsolation,
) -> Option<(ExecutorRecord, SwapReview, Option<OrderUid>)> {
    let speed_up = match mode()? {
        Mode::RetryReview => None,
        Mode::SpeedUp(SpeedUp::Review | SpeedUp::Open | SpeedUp::JustPlaced) => {
            Some(SpeedUp::Review)
        }
        _ => return None,
    };
    let record = staged_swap(None, speed_up)?;
    let open = speed_up
        .and_then(|_| record.swap()?.orders().last())
        .map(wallet_ops::vault::SwapOrderRecord::uid);
    // What the orderbook quotes, and what the bridge leg delivers of it.
    let (bought, delivered) = if open.is_some() {
        (96_140_000_u32, SPED_UP.1)
    } else {
        (101_180_000_u32, RETRY.1)
    };
    let first = record.swap()?.orders().first()?;
    let BridgeOrderTerms::Across(deposit) = first.bridge()? else {
        return None;
    };
    let earlier = SwapContinuingApproval {
        uid: first.uid(),
        attempt: first.attempt(),
        destination_minimum: deposit.output_amount,
        post_hook_nonce: first.post_hook()?.nonce(),
        quote_timestamp: deposit.quote_timestamp,
    };
    // Only a speed-up discloses an earlier approval.
    let continuing = if open.is_some() {
        vec![earlier]
    } else {
        Vec::new()
    };
    let review = SwapReview::for_ui_fixture(
        profile,
        &record,
        WETH,
        USDC,
        U256::from(SWAP_SPEND),
        U256::from(bought),
        // 2,030 USDC for one ETH, each in base units for one whole native token.
        PairAnchorRate {
            sell_rate: U256::from(1_000_000_000_000_000_000_u64),
            buy_rate: U256::from(2_030_000_000_u32),
        },
        120_000_000,
        SWAP_DELIVERY,
        U256::from(delivered),
        U256::from(delivered),
        continuing,
        isolation,
    );
    match review {
        Ok(review) => Some((record, review, open)),
        Err(error) => {
            tracing::warn!(%error, "the debug UI fixture couldn't synthesize its review");
            None
        }
    }
}

/// The destination stealth account of a `setup:` mode's private Bridge swap, on Arbitrum One.
/// No wallet holds it.
const SETUP_DESTINATION_ACCOUNT: Address = address!("51fc8a2e6d0b94c37e15a8f26d3c40b97e2ab7a2");
/// The synthesized swap's delivery by Across to the private balance on Arbitrum One, through
/// that account.
const PRIVATE_DELIVERY: BridgeDelivery = BridgeDelivery {
    receiver: SETUP_DESTINATION_ACCOUNT,
    private: Some(BridgePrivateDelivery {
        on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
    }),
    ..SWAP_DELIVERY
};

/// The setup fee limit a `setup:fee-token` mode's swap approved, 0.52 USDC, and the fee its
/// retry is estimated at, 0.00016 WETH, which the review limits at 0.0002 WETH.
const SETUP_APPROVED_FEE: u64 = 520_000;
const SETUP_RETRY_FEE: u64 = 160_000_000_000_000;
/// The setup fee limit a `setup:retired` mode's swap approved, 0.00015 WETH, which the stand-in
/// estimate of its retry with a new account is above.
const SETUP_RETIRED_APPROVED_FEE: u64 = 150_000_000_000_000;

/// The two accounts of a `setup:replaced` mode, synthesized once: the swap's new account #5,
/// reserved a minute ago with the use the swap started with four minutes ago, and the retired
/// account #4 it replaced, which holds no swap. `None` when they fail to decode.
fn replaced_accounts() -> Option<&'static (ExecutorRecord, ExecutorRecord)> {
    static ACCOUNTS: OnceLock<Option<(ExecutorRecord, ExecutorRecord)>> = OnceLock::new();
    ACCOUNTS
        .get_or_init(|| {
            let delegate = Address::repeat_byte(0x5d);
            let retired = ExecutorOperationId::random().ok()?;
            // The use keeps the ID it took from the account it started with.
            let swap_use = SwapUseId::first(retired);
            let now = now_unix();
            let swap = staged_setup_account(
                ExecutorOperationId::random().ok()?,
                (5, SWAP_ACCOUNT, delegate),
                (false, false),
                &serde_json::json!({
                    "version": 2, "created_at": now.saturating_sub(60),
                    "purpose_summary": "Private swap", "active_swap_use": swap_use,
                    "swap_uses": [{
                        "id": swap_use, "started_at": now.saturating_sub(240), "fresh": true,
                        "role": { "Source": { "approval": staged_setup_approval(None)? } },
                    }],
                }),
            )?;
            let retired = staged_setup_account(
                retired,
                (4, Address::repeat_byte(0x3a), delegate),
                (false, false),
                &serde_json::json!({ "retired": true }),
            )?;
            Some((swap, retired))
        })
        .as_ref()
}

/// The record of one stealth account of a `setup:` mode's swap, decoded from the JSON a stored
/// record has, with `links` its own swap fields. A sent setup signed one payload, and a
/// confirmed one's nonce was read as consumed.
fn staged_setup_account(
    operation: ExecutorOperationId,
    (index, address, delegate): (u32, Address, Address),
    (sent, confirmed): (bool, bool),
    links: &serde_json::Value,
) -> Option<ExecutorRecord> {
    let issued = if sent {
        vec![staged_payload(
            0,
            0xa0,
            ExecutorPayloadPurpose::Operation,
            Some(0xc0),
        )?]
    } else {
        Vec::new()
    };
    let mut record = serde_json::json!({
        "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": operation, "index": index, "address": address, "delegate": delegate,
        "retired": false, "created_at": now_unix().saturating_sub(240), "issued": issued,
        "nonce_watermark": confirmed
            .then(|| ExecutorNonceWatermark::new(U256::ONE, SWAP_ORIGIN_BLOCK + 12)),
    });
    for (field, value) in links.as_object()? {
        record[field] = value.clone();
    }
    serde_json::from_value(record).ok()
}

/// The approval saved with a `setup:` mode's swap, which sets up a new account of its own and,
/// for the private Bridge `delivery`, a new one on the destination network.
fn staged_setup_approval(delivery: Option<BridgeDelivery>) -> Option<SwapApproval> {
    let bounds: SwapApprovedBounds = serde_json::from_value(serde_json::json!({
        "sell_amount": U256::from(SWAP_SELL), "unshield_amount": U256::from(SWAP_SPEND),
        "unshield_fee_bps": U256::from(25_u8), "buy_amount": U256::from(RETRY.0),
        "private_minimum": U256::from(RETRY.0), "shield_fee_bps": U256::from(25_u8),
        "slippage_bps": 50, "pre_hook_gas_limit": 1_045_000, "post_hook_gas_limit": 396_000,
        "anchors": [], "destination_minimum": delivery.map(|_| U256::from(RETRY.1)),
        "gas_share_bps": 2_500, "gas_estimate": U256::from(492_000_u32),
        "gas_allowance": U256::from(123_000_u32), "gas_price_wei": 120_000_000,
        "valid_for_secs": 1_200
    }))
    .ok()?;
    let new = |address| SwapApprovedAccount {
        address: Some(address),
        setup: true,
    };
    Some(SwapApproval {
        bounds,
        price_verified: Some(true),
        price_acknowledged: false,
        delivery: delivery.map_or(SwapDelivery::Reshield, SwapDelivery::Bridge),
        tokens: Some(SwapApprovalTokens {
            sell: WETH,
            buy: USDC,
        }),
        accounts: Some(SwapApprovedAccounts {
            source: new(SWAP_ACCOUNT),
            destination: delivery.map(|delivery| new(delivery.receiver)),
        }),
    })
}

/// The records of a `cancelled:` mode, each decoded from the JSON a stored record has: account
/// #5 on Ethereum, for the accepted `delegate`, and destination account #12 on Arbitrum One.
/// Fifteen minutes ago a private Bridge swap of 0.05 WETH for USDC reserved both fresh. Both
/// setups, at nonce 0, are confirmed, and the swap was cancelled before its order, which
/// stopped its use of each account and retired #12. With `Reused`, a later swap of 0.02 WETH
/// for USDC back to the private balance claimed #5 two minutes ago and has no order yet.
/// Between the two records is the cancelled swap's identity, under which the view finds #12.
fn staged_cancelled(
    cancelled: Cancelled,
    delegate: Address,
) -> Option<(ExecutorRecord, SwapIdentity, ExecutorRecord)> {
    let now = now_unix();
    let started = now.saturating_sub(900);
    let operation = ExecutorOperationId::random().ok()?;
    let destination_operation = ExecutorOperationId::random().ok()?;
    let swap = SwapIdentity {
        operation,
        swap_use: SwapUseId::first(operation),
    };
    let mut uses = vec![serde_json::json!({
        "id": swap.swap_use, "started_at": started, "fresh": true, "stopped": true,
        "stopped_before_order": true,
        "role": { "Source": {
            "approval": staged_setup_approval(Some(PRIVATE_DELIVERY))?,
            "destination_operation": destination_operation,
        }},
    })];
    let mut active = swap.swap_use;
    if cancelled == Cancelled::Reused {
        // 0.02 WETH, less Railgun's unshield fee, for 40.18 USDC, on the account as it is.
        let mut approval = staged_setup_approval(None)?;
        approval.bounds.unshield_amount = Some(U256::from(20_000_000_000_000_000_u64));
        approval.bounds.sell_amount = U256::from(19_950_000_000_000_000_u64);
        approval.bounds.buy_amount = U256::from(40_184_000_u32);
        approval.bounds.private_minimum = U256::from(40_184_000_u32);
        approval.accounts = Some(SwapApprovedAccounts {
            source: SwapApprovedAccount {
                address: Some(SWAP_ACCOUNT),
                setup: false,
            },
            destination: None,
        });
        active = SwapUseId::random().ok()?;
        uses.push(serde_json::json!({
            "id": active, "started_at": now.saturating_sub(120), "fresh": false,
            "role": { "Source": { "approval": approval } },
        }));
    }
    let source = staged_setup_account(
        operation,
        (5, SWAP_ACCOUNT, delegate),
        (true, true),
        &serde_json::json!({
            "version": 2, "created_at": started, "purpose_summary": "Private swap",
            "swap_setup_stopped": cancelled == Cancelled::Confirmed,
            "active_swap_use": active, "swap_uses": uses,
        }),
    )?;
    let destination = staged_setup_account(
        destination_operation,
        (12, SETUP_DESTINATION_ACCOUNT, Address::repeat_byte(0x5d)),
        (true, true),
        &serde_json::json!({
            "version": 2, "created_at": started, "retired": true, "swap_setup_stopped": true,
            "active_swap_use": swap.swap_use,
            "swap_uses": [{
                "id": swap.swap_use, "started_at": started, "fresh": true, "stopped": true,
                "stopped_before_order": true,
                "role": { "Destination": {
                    "origin_chain": SWAP_ORIGIN, "origin_operation": operation,
                    "destination_token": PRIVATE_DELIVERY.destination_token,
                }},
            }],
        }),
    )?;
    Some((source, swap, destination))
}

/// This session's stages of a setup whose job is at `stage`, or ended there with `result`.
/// A running one's route is where a job's route starts.
fn staged_setup_progress(
    stage: TransactionGenerationStage,
    result: Option<Result<&PublicBroadcasterResultKind, String>>,
) -> SetupProgress {
    let mut progress = running_setup_progress(stage, SubmissionRouteProgress::default());
    if let Some(result) = result {
        progress.finish(result);
    }
    progress
}

/// This session's stages of a setup whose job is at `stage`, with its route at `route`.
fn running_setup_progress(
    stage: TransactionGenerationStage,
    route: SubmissionRouteProgress,
) -> SetupProgress {
    let (sender, receiver) =
        tokio::sync::watch::channel(TransactionGenerationStage::SelectingPrivateNotes);
    // The job reported the stage it is at, as a real one does from its first stage on.
    let _ = sender.send(stage);
    let (route_sender, route) = tokio::sync::watch::channel(route);
    hold_route(route_sender);
    SetupProgress::running(receiver, route, None)
}

/// Keep a mode's route sender for as long as the wallet runs, as a running job keeps its own,
/// so the route never reads as closed.
fn hold_route(sender: SubmissionRouteSender) {
    static HELD: Mutex<Vec<SubmissionRouteSender>> = Mutex::new(Vec::new());
    HELD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(sender);
}

impl PrivateSwapsView {
    /// The estimate that stands in for a broadcaster's in the retry dialog of a
    /// `setup:retired` mode, 0.00016 WETH, so the dialog can open its review on a wallet
    /// without private funds. `None` in every other mode.
    pub(super) fn ui_fixture_setup_estimate(
        &self,
        cx: &App,
    ) -> Option<ExecutorRecoveryFeeEstimate> {
        if !matches!(mode(), Some(Mode::Setup(Setup::Retired))) {
            return None;
        }
        let root = self.root.upgrade()?;
        let delegate = root
            .read(cx)
            .effective_chain_configs
            .get(SWAP_ORIGIN)?
            .accepted_executor_profile()?
            .delegate();
        Some(ExecutorRecoveryFeeEstimate::for_ui_fixture(
            SWAP_ORIGIN,
            WETH,
            delegate,
            U256::from(SETUP_RETRY_FEE),
        ))
    }

    /// In a `setup:` mode, show the detail of the synthesized swap with its setup held at
    /// `setup`: the records of its accounts, and the stages this session would hold of each
    /// setup. A running setup has a job that never answers.
    fn open_ui_fixture_setup(
        &mut self,
        setup: Setup,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        use TransactionGenerationStage::{
            EstimatingBroadcasterFee, GeneratingPoiProofs, ProvingTransaction,
            WaitingForBroadcasterResponse,
        };
        use wallet_ops::SubmissionRouteState::{Ready, Reconnecting};
        // A confirmed setup is recorded for the network's accepted delegate.
        let delegate = self.root.upgrade().and_then(|root| {
            let chains = &root.read(cx).effective_chain_configs;
            Some(
                chains
                    .get(SWAP_ORIGIN)?
                    .accepted_executor_profile()?
                    .delegate(),
            )
        });
        let bridged = matches!(
            setup,
            Setup::TwoAccounts | Setup::TwoWaiting | Setup::DestinationFailed
        );
        let delivery = bridged.then_some(PRIVATE_DELIVERY);
        let operation = ExecutorOperationId::random().ok();
        let destination_operation = ExecutorOperationId::random().ok();
        let staged = || {
            let (operation, destination_operation) = (operation?, destination_operation?);
            // Each account's setup: whether it was sent, and whether it is confirmed.
            let (origin, destination) = match setup {
                Setup::Running
                | Setup::Waiting
                | Setup::Reconnecting
                | Setup::TwoWaiting
                | Setup::Failed
                | Setup::Retired
                | Setup::Replaced
                | Setup::FeeToken => ((false, false), (false, false)),
                Setup::BroadcasterError | Setup::Timeout | Setup::Restart | Setup::TwoAccounts => {
                    ((true, false), (false, false))
                }
                Setup::Ready => ((true, true), (false, false)),
                Setup::DestinationFailed => ((true, true), (true, false)),
            };
            let mut approval = staged_setup_approval(delivery)?;
            if setup == Setup::FeeToken {
                approval.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
                    token: USDC,
                    amount: U256::from(SETUP_APPROVED_FEE),
                });
            }
            if setup == Setup::Retired {
                approval.bounds.source_setup_fee = Some(SwapSetupFeeLimit {
                    token: WETH,
                    amount: U256::from(SETUP_RETIRED_APPROVED_FEE),
                });
            }
            // Both accounts' uses started when the accounts were reserved.
            let swap_use = SwapUseId::first(operation);
            let started = now_unix().saturating_sub(240);
            let record = staged_setup_account(
                operation,
                (4, SWAP_ACCOUNT, delegate?),
                origin,
                &serde_json::json!({
                    "version": 2, "purpose_summary": "Private swap",
                    "retired": setup == Setup::Retired, "active_swap_use": swap_use,
                    "swap_uses": [{
                        "id": swap_use, "started_at": started, "fresh": true,
                        "role": { "Source": {
                            "approval": approval,
                            "destination_operation": delivery.map(|_| destination_operation),
                        }},
                    }],
                }),
            )?;
            let destination = match delivery {
                Some(delivery) => Some(staged_setup_account(
                    destination_operation,
                    (12, delivery.receiver, Address::repeat_byte(0x5d)),
                    destination,
                    &serde_json::json!({
                        "version": 2, "active_swap_use": swap_use,
                        "swap_uses": [{
                            "id": swap_use, "started_at": started, "fresh": true,
                            "role": { "Destination": {
                                "origin_chain": SWAP_ORIGIN, "origin_operation": operation,
                                "destination_token": delivery.destination_token,
                            }},
                        }],
                    }),
                )?),
                None => None,
            };
            Some((record, destination))
        };
        // A replaced account's swap is the one Stealth accounts lists in this mode.
        let staged = match setup {
            Setup::Replaced => replaced_accounts().map(|(swap, _)| (swap.clone(), None)),
            _ => staged(),
        };
        let Some((record, destination)) = staged else {
            tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        let operation = record.operation();
        let refused = PublicBroadcasterResultKind::Failed {
            error: "fee too low".to_owned(),
        };
        let submitted = PublicBroadcasterResultKind::Submitted {
            tx_hash: keccak256("setup").to_string(),
        };
        // What the setup job ended with. A failure its stages show isn't the swap's error too.
        let failed = "Fixture: the setup fee is above the limit approved with the swap.";
        // A setup that waits for a broadcaster's answer over `route`, published 27 seconds ago.
        let waiting = |route| {
            running_setup_progress(WaitingForBroadcasterResponse, route).waiting_since(
                std::rc::Rc::new(std::cell::Cell::new(
                    std::time::Instant::now().checked_sub(std::time::Duration::from_secs(27)),
                )),
            )
        };
        let (origin, destination_progress, error) = match setup {
            Setup::Running | Setup::Replaced => (
                Some(staged_setup_progress(ProvingTransaction, None)),
                None,
                None,
            ),
            Setup::Waiting => (
                Some(waiting(SubmissionRouteProgress {
                    state: Ready,
                    peers: 3,
                    recoveries_used: 0,
                    publishes: 6,
                })),
                None,
                None,
            ),
            Setup::Reconnecting => (
                Some(waiting(SubmissionRouteProgress {
                    state: Reconnecting,
                    peers: 0,
                    recoveries_used: 1,
                    publishes: 3,
                })),
                None,
                None,
            ),
            Setup::TwoWaiting => (
                Some(waiting(SubmissionRouteProgress {
                    state: Ready,
                    peers: 3,
                    recoveries_used: 0,
                    publishes: 6,
                })),
                Some(waiting(SubmissionRouteProgress {
                    state: Ready,
                    peers: 2,
                    recoveries_used: 0,
                    publishes: 6,
                })),
                None,
            ),
            Setup::Failed => (
                Some(staged_setup_progress(
                    EstimatingBroadcasterFee,
                    Some(Err(failed.to_owned())),
                )),
                None,
                None,
            ),
            // The stage says what a setup refused for its fee token says there.
            Setup::FeeToken => {
                let root = self.root.upgrade();
                let changed = super::setup_fee_token_changed(
                    SWAP_ORIGIN,
                    root.as_ref()
                        .map(|root| &root.read(cx).effective_token_registry),
                    (USDC, U256::from(SETUP_APPROVED_FEE)),
                    WETH,
                    "Retry setup",
                );
                (
                    Some(staged_setup_progress(
                        EstimatingBroadcasterFee,
                        Some(Err(changed)),
                    )),
                    None,
                    None,
                )
            }
            Setup::BroadcasterError => (
                Some(staged_setup_progress(
                    WaitingForBroadcasterResponse,
                    Some(Ok(&refused)),
                )),
                None,
                None,
            ),
            Setup::Timeout => (
                Some(staged_setup_progress(
                    WaitingForBroadcasterResponse,
                    Some(Ok(&PublicBroadcasterResultKind::TimedOut)),
                )),
                None,
                None,
            ),
            // A retired account is told by the swap's stage, with no error beside it.
            Setup::Restart | Setup::Ready | Setup::Retired => (None, None, None),
            Setup::TwoAccounts => (
                Some(staged_setup_progress(
                    WaitingForBroadcasterResponse,
                    Some(Ok(&submitted)),
                )),
                Some(staged_setup_progress(GeneratingPoiProofs, None)),
                None,
            ),
            Setup::DestinationFailed => (
                None,
                Some(staged_setup_progress(
                    WaitingForBroadcasterResponse,
                    Some(Ok(&refused)),
                )),
                None,
            ),
        };
        let tracking = self.tracking.entry(operation).or_default();
        *tracking.setup_progress_mut(SetupSide::Origin) = origin;
        *tracking.setup_progress_mut(SetupSide::Destination) = destination_progress;
        tracking.error = error;
        // The review that approved the setup has the wallet place the order after it.
        tracking.auto_place = setup != Setup::Ready;
        // The view tells a running setup by its job, so one that never answers stands in.
        if matches!(
            setup,
            Setup::Running
                | Setup::Waiting
                | Setup::Reconnecting
                | Setup::Replaced
                | Setup::TwoAccounts
                | Setup::TwoWaiting
        ) {
            self.job = Some(SwapJob {
                operation,
                kind: SwapJobKind::Setup,
                abort: self
                    .runtime
                    .spawn(std::future::pending::<()>())
                    .abort_handle(),
            });
        }
        if let Some(destination) = destination {
            self.destinations.insert(
                SwapIdentity {
                    operation,
                    swap_use: SwapUseId::first(operation),
                },
                Some(destination),
            );
        }
        self.records = vec![record];
        self.show_detail(operation, window, cx);
        if setup == Setup::FeeToken
            && let (Some(delegate), Some(root)) = (delegate, self.root.upgrade())
        {
            // ETH at $3,000 and USDC at $1: a token's rate is its base units for one ETH.
            {
                let cache = &root.read(cx).public_broadcaster_anchor_cache;
                cache.store_native_usd_rate(SWAP_ORIGIN, U256::from(3_000_000_000_u64), 18);
                cache.store_rate(SWAP_ORIGIN, USDC, U256::from(3_000_000_000_u64));
                cache.store_rate(SWAP_ORIGIN, WETH, U256::from(1_000_000_000_000_000_000_u64));
            }
            let estimate = ExecutorRecoveryFeeEstimate::for_ui_fixture(
                SWAP_ORIGIN,
                WETH,
                delegate,
                U256::from(SETUP_RETRY_FEE),
            );
            self.open_ui_fixture_setup_retry_review(operation, &estimate, window, cx);
        }
    }

    /// In a `reported:` mode, show the detail of the synthesized swap with what this session
    /// would hold of its reported fill: the orderbook's report with the bought amount and the
    /// trade block, and what that block gave once read: the trade, a private swap's shield,
    /// and for a Bridge swap the hand-off, with the provider's report. A private Bridge swap
    /// also has its destination stealth account's record, and its report holds what the fill
    /// block on the destination network showed.
    fn open_ui_fixture_reported(
        &mut self,
        reported: Reported,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(record) = staged_reported(reported) else {
            tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        let operation = record.operation();
        let Some(order) = record.swap().and_then(|swap| swap.orders().last()) else {
            return;
        };
        let uid = order.uid();
        if reported.private() {
            let swap_use = order
                .use_id()
                .unwrap_or_else(|| SwapUseId::first(operation));
            let destination = ExecutorOperationId::random().ok().and_then(|destination| {
                staged_setup_account(
                    destination,
                    (12, SETUP_DESTINATION_ACCOUNT, Address::repeat_byte(0x5d)),
                    (true, true),
                    &serde_json::json!({
                        "version": 2, "active_swap_use": swap_use,
                        "swap_uses": [{
                            "id": swap_use, "started_at": now_unix().saturating_sub(240),
                            "fresh": true,
                            "role": { "Destination": {
                                "origin_chain": SWAP_ORIGIN, "origin_operation": operation,
                                "destination_token": PRIVATE_DELIVERY.destination_token,
                            }},
                        }],
                    }),
                )
            });
            self.destinations.insert(
                SwapIdentity {
                    operation,
                    swap_use,
                },
                destination,
            );
        }
        let tracking = self.tracking.entry(operation).or_default();
        tracking.order_hints.insert(
            uid,
            SwapOrderHint {
                report: CowOrderStatusReport {
                    status: CowOrderStatusHint::Fulfilled,
                    executed_buy_amount: Some(U256::from(SWAP_BOUGHT)),
                },
                trade_block: Some(REPORTED_TRADE_BLOCK),
            },
        );
        let block = BlockNumHash::new(REPORTED_TRADE_BLOCK, keccak256("settlement"));
        let bridged = reported != Reported::Fill;
        let delivered = U256::from(RETRY.1);
        tracking.early_hints.insert(
            uid,
            SwapEarlyHint {
                trade_block: Some(REPORTED_TRADE_BLOCK),
                block_read: true,
                settlement: Some(SwapSettlementHint {
                    block,
                    bought: U256::from(SWAP_BOUGHT),
                    shielded: (!bridged).then_some(U256::from(REPORTED_SHIELDED)),
                    // The settlement transaction runs the post-hook's shield.
                    shield_transaction: (!bridged).then(|| keccak256("settlement transaction")),
                    handoff: bridged.then_some(SwapHandoffHint {
                        block,
                        deposit_id: Some(U256::from(3_481_207_u32)),
                        sent: U256::from(RETRY.0),
                    }),
                }),
                report: reported.delivered().then_some(SwapBridgeReport::Delivered {
                    amount_out: Some(delivered),
                    seen: match reported {
                        Reported::Shielded => {
                            Some(SwapDeliverySeen::Shielded { amount: delivered })
                        }
                        Reported::Held => Some(SwapDeliverySeen::Held { amount: delivered }),
                        Reported::Fill | Reported::Handoff | Reported::Delivered => None,
                    },
                    transaction: Some(keccak256("fill")),
                }),
                reads_fill: reported.private(),
            },
        );
        self.records = vec![record];
        self.show_detail(operation, window, cx);
    }

    /// In a `cancelled:` mode, stage account #5 and the destination account of its cancelled
    /// swap, then open the private form of a new swap that sells WETH. Its Source picker lists
    /// #5 when local admission offers it.
    fn open_ui_fixture_cancelled(
        &mut self,
        cancelled: Cancelled,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Local admission offers a source only for the network's accepted delegate.
        let delegate = self.root.upgrade().and_then(|root| {
            let chains = &root.read(cx).effective_chain_configs;
            Some(
                chains
                    .get(SWAP_ORIGIN)?
                    .accepted_executor_profile()?
                    .delegate(),
            )
        });
        let Some((record, swap, destination)) =
            delegate.and_then(|delegate| staged_cancelled(cancelled, delegate))
        else {
            tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        self.destinations.insert(swap, Some(destination));
        // The form lists its Source accounts from the records as it opens.
        self.records = vec![record];
        self.open_ui_fixture_new_swap(WETH, USDC, window, cx);
    }

    /// In a `retry-review`, `quote-error`, `split:`, speed-up, `setup:`, `reported:` or
    /// `cancelled:` mode, show the synthesized private swap in place of the Public account form
    /// that was asked for. `true` when such a mode is on.
    pub(super) fn open_ui_fixture_swap(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        // The detail's stage, or none for a mode that opens the form.
        let detail = match mode() {
            Some(
                Mode::RetryReview
                | Mode::QuoteError
                | Mode::NativeBuy
                | Mode::SpeedUp(SpeedUp::Review)
                | Mode::Setup(_)
                | Mode::SetupUnderOrder(_)
                | Mode::Reported(_)
                | Mode::Cancelled(_),
            ) => None,
            Some(Mode::Split(split)) => Some((Some(split), None)),
            Some(Mode::SpeedUp(speed_up)) => Some((None, Some(speed_up))),
            _ => return false,
        };
        if self.origin_chain_id != SWAP_ORIGIN {
            tracing::warn!("the debug UI fixture's private swap is on Ethereum: select it first");
            return true;
        }
        if let Some(Mode::Setup(setup)) = mode() {
            self.open_ui_fixture_setup(setup, window, cx);
            return true;
        }
        if let Some(Mode::SetupUnderOrder(under_order)) = mode() {
            let Some(record) = staged_setup_under_order(under_order == SetupUnderOrder::Expired)
            else {
                tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
                return true;
            };
            let operation = record.operation();
            self.records = vec![record];
            self.show_detail(operation, window, cx);
            return true;
        }
        if let Some(Mode::Reported(reported)) = mode() {
            self.open_ui_fixture_reported(reported, window, cx);
            return true;
        }
        if let Some(Mode::Cancelled(cancelled)) = mode() {
            self.open_ui_fixture_cancelled(cancelled, window, cx);
            return true;
        }
        if matches!(mode(), Some(Mode::QuoteError)) {
            self.open_ui_fixture_quote_error(window, cx);
            return true;
        }
        if matches!(mode(), Some(Mode::NativeBuy)) {
            self.open_ui_fixture_new_swap(USDC, WETH, window, cx);
            return true;
        }
        let Some((split, speed_up)) = detail else {
            self.open_ui_fixture_retry_review(window, cx);
            return true;
        };
        let Some(record) = staged_swap(split, speed_up) else {
            tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
            return true;
        };
        let operation = record.operation();
        // What an explicit check found in the stealth account: the refunded deposit.
        if let Some(refund) = record
            .swap()
            .and_then(|swap| swap.orders().first())
            .and_then(|order| order.observations().bridge_refund)
        {
            self.tracking.entry(operation).or_default().stealth_balance =
                Some((U256::from(FIRST_ORDER.0), refund.block));
        }
        self.records = vec![record];
        self.show_detail(operation, window, cx);
        true
    }
}

/// The stealth accounts an `accounts`, `refund:stealth` or `setup:replaced` mode shows in
/// place of the wallet's own, synthesized once. `None` in every other mode.
pub(in crate::root) fn stealth_accounts() -> Option<Vec<ExecutorRecord>> {
    static ACCOUNTS: OnceLock<Vec<ExecutorRecord>> = OnceLock::new();
    if matches!(mode(), Some(Mode::Setup(Setup::Replaced))) {
        return replaced_accounts().map(|(swap, retired)| vec![swap.clone(), retired.clone()]);
    }
    matches!(mode(), Some(Mode::Accounts | Mode::Refund(Refund::Stealth))).then(|| {
        ACCOUNTS
            .get_or_init(|| {
                let accounts = staged_accounts();
                if accounts.len() != 3 {
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

/// The accounts of the `accounts` mode, decoded from the JSON a stored record has. An
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
    // A settled native shield, hidden as one is once its Shield event is proven.
    let native_shield = || {
        let mut shield = account(
            9,
            0x3d,
            1,
            vec![staged_payload(0, 0xa1, Operation, Some(0xc9))],
        )?;
        shield["purpose_summary"] = serde_json::json!(
            "Shield 0.01 ETH → 0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp"
        );
        shield["hidden"] = serde_json::json!(true);
        Some(shield)
    };
    [swap(), operation(), native_shield()]
        .into_iter()
        .flatten()
        .filter_map(|account| serde_json::from_value(account).ok())
        .collect()
}

/// What the refund dialog shows for the blocked Shield of a `refund:` mode. `None` for any
/// other Shield, in every other mode, and in `refund:stealth` without its staged accounts.
pub(in crate::root) fn refund_dialog_state(
    utxo_id: BlockedShieldRescueUtxoId,
) -> Option<BlockedShieldRefundDialogState> {
    let Mode::Refund(refund) = mode()? else {
        return None;
    };
    if utxo_id != REFUND_UTXO {
        return None;
    }
    let unheld = Some(Address::repeat_byte(0x4e));
    // Each reason repeats the sentence wallet-ops gives that case.
    let (reason, blocker, origin) = match refund {
        Refund::Checking => return Some(BlockedShieldRefundDialogState::Checking),
        Refund::Available => {
            return Some(BlockedShieldRefundDialogState::Available(
                BlockedShieldRescueInfo {
                    eligible: true,
                    disabled_reason: None,
                    origin_address: unheld.map(|origin| origin.to_checksum(None)),
                    public_account_uuid: Some("ui-fixture".to_owned()),
                    public_account_label: Some("Savings".to_owned()),
                },
            ));
        }
        Refund::Unknown => (
            "The Shield came from an account that isn't in this wallet.".to_owned(),
            Some(BlockedShieldRescueBlocker::OriginUnknown),
            unheld,
        ),
        Refund::Stealth => {
            // The first account Stealth accounts shows, so the dialog's action finds its row.
            let accounts = stealth_accounts().unwrap_or_default();
            let account = accounts.first()?;
            let index = account.index();
            (
                format!("The Shield came from stealth account #{index}, which isn't in Public."),
                Some(BlockedShieldRescueBlocker::OriginStealth {
                    operation: account.operation(),
                    index,
                }),
                account.address(),
            )
        }
        Refund::Inactive => (
            "The Shield came from Public account \"Savings\", which is inactive.".to_owned(),
            Some(BlockedShieldRescueBlocker::OriginInactive {
                public_account_uuid: "ui-fixture".to_owned(),
                label: Some("Savings".to_owned()),
            }),
            unheld,
        ),
        Refund::Unresolved => (
            "Couldn't read the Shield transaction. Check the connection and try again.".to_owned(),
            None,
            None,
        ),
    };
    Some(BlockedShieldRefundDialogState::Unavailable {
        info: BlockedShieldRescueInfo {
            eligible: false,
            disabled_reason: Some(reason),
            origin_address: origin.map(|origin| origin.to_checksum(None)),
            public_account_uuid: None,
            public_account_label: None,
        },
        blocker,
    })
}

impl WalletRoot {
    /// In a `refund:` mode, open the refund dialog of its blocked Shield in place of Stealth
    /// accounts, the first time they are opened. `true` when the dialog opened.
    pub(in crate::root) fn open_ui_fixture_refund_dialog(
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        static OPENED: AtomicBool = AtomicBool::new(false);
        // Without a state there is no dialog to open: another mode, or `refund:stealth`
        // without its staged accounts.
        if refund_dialog_state(REFUND_UTXO).is_none() || OPENED.swap(true, Ordering::Relaxed) {
            return false;
        }
        Self::open_blocked_shield_refund_dialog(REFUND_UTXO, window, cx);
        true
    }

    /// In a `broadcaster-progress:` mode, open the progress dialog of a synthesized Send via
    /// broadcaster of `asset` in place of its form, held at the mode's state. `true` when the
    /// dialog opened.
    pub(in crate::root) fn open_ui_fixture_broadcaster_progress(
        &mut self,
        asset: &crate::root::UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        use TransactionGenerationStage::WaitingForBroadcasterResponse;
        use wallet_ops::SubmissionRouteState::{Ready, Reconnecting};
        let Some(Mode::BroadcasterProgress(state)) = mode() else {
            return false;
        };
        // The recipient is the wallet's own address, so the dialog shows no stranger's.
        let recipient = self
            .view_session
            .as_ref()
            .and_then(|session| session.receive_address().ok())
            .unwrap_or_default();
        let route = self.start_private_broadcaster_progress(
            crate::root::DeliveryFormKind::Send,
            crate::root::UnshieldAssetKey::from_asset(asset),
            u64::MAX,
            asset.label.clone(),
            asset.icon_path.clone(),
            recipient,
            None,
            Duration::from_mins(2),
            cx,
        );
        route.send_replace(match state {
            BroadcasterProgress::Ready | BroadcasterProgress::Direct => SubmissionRouteProgress {
                state: Ready,
                peers: 3,
                recoveries_used: 0,
                publishes: 4,
            },
            BroadcasterProgress::Reconnecting => SubmissionRouteProgress {
                state: Reconnecting,
                peers: 0,
                recoveries_used: 1,
                publishes: 3,
            },
            // The route was lost again after its last reconnect, and no answer came.
            BroadcasterProgress::Timeout => SubmissionRouteProgress {
                state: Reconnecting,
                peers: 0,
                recoveries_used: SUBMISSION_ROUTE_RECOVERY_LIMIT,
                publishes: 9,
            },
        });
        hold_route(route);
        if let Some(progress) = self.private_broadcaster_progress.as_mut() {
            apply_private_broadcaster_progress_stage(
                &mut progress.steps,
                WaitingForBroadcasterResponse,
            );
            progress.stage_seen = true;
            progress.stop_available = false;
            progress.public_broadcaster_wait_started_at =
                std::time::Instant::now().checked_sub(Duration::from_secs(17));
            progress.route_isolated = state != BroadcasterProgress::Direct;
            if state == BroadcasterProgress::Timeout {
                finish_private_broadcaster_progress_steps_at_stage(
                    &mut progress.steps,
                    WaitingForBroadcasterResponse,
                    &PublicBroadcasterResultKind::TimedOut,
                );
            }
        }
        self.show_private_broadcaster_progress_dialog(window, cx);
        true
    }
}

/// 1.0 of `chain_id`'s wrapped native token `wrapped_native`, which stands in for a private
/// asset the wallet doesn't hold.
fn stand_in_asset(chain_id: u64, wrapped_native: Address) -> crate::root::UnshieldAsset {
    let amount = U256::from(10).pow(U256::from(18));
    crate::root::UnshieldAsset {
        chain_id,
        token: wrapped_native,
        label: known_tokens_for_chain(chain_id)
            .find(|token| token.token == wrapped_native)
            .map_or("WETH", |token| token.symbol)
            .to_owned(),
        decimals: Some(18),
        total: amount,
        poi_verified_total: amount,
        max_batched: amount,
        icon_path: None,
    }
}

/// The stand-in asset of a `max:` mode: 12.345678901234567891 of `chain_id`'s wrapped native
/// token `wrapped_native`, with its icon, in notes whose largest spend is 7.123456789012345678.
/// Send, Unshield and the swap's Sell panel offer it. `None` in every other mode.
pub(super) fn max_asset(
    chain_id: u64,
    wrapped_native: Address,
) -> Option<crate::root::UnshieldAsset> {
    let total = U256::from(12_345_678_901_234_567_891_u64);
    matches!(mode(), Some(Mode::Max(_))).then(|| crate::root::UnshieldAsset {
        total,
        poi_verified_total: total,
        max_batched: U256::from(7_123_456_789_012_345_678_u64),
        icon_path: token_icon_asset_path(chain_id, &wrapped_native).map(WalletIconSource::embedded),
        ..stand_in_asset(chain_id, wrapped_native)
    })
}

/// What a `max:locked` mode reports locked of `token`, 0.5 when it is the stand-in Sell asset
/// `wrapped_native`. `None` in every other mode.
pub(super) fn locked_sell_value(token: Address, wrapped_native: Option<Address>) -> Option<U256> {
    (matches!(mode(), Some(Mode::Max(Max::Locked))) && wrapped_native == Some(token))
        .then_some(U256::from(500_000_000_000_000_000_u64))
}

/// The stand-in asset of the `unshield` or a `max:` mode. `None` in every other mode.
pub(in crate::root) fn unshield_asset(
    chain_id: u64,
    wrapped_native: Address,
) -> Option<crate::root::UnshieldAsset> {
    max_asset(chain_id, wrapped_native).or_else(|| {
        matches!(mode(), Some(Mode::Unshield)).then(|| stand_in_asset(chain_id, wrapped_native))
    })
}

/// The stand-in asset Send offers in a `broadcaster-progress:` or `max:` mode, with its token's
/// icon. `None` in every other mode.
pub(in crate::root) fn send_asset(
    chain_id: u64,
    wrapped_native: Address,
) -> Option<crate::root::UnshieldAsset> {
    max_asset(chain_id, wrapped_native).or_else(|| {
        matches!(mode(), Some(Mode::BroadcasterProgress(_))).then(|| crate::root::UnshieldAsset {
            icon_path: token_icon_asset_path(chain_id, &wrapped_native)
                .map(WalletIconSource::embedded),
            ..stand_in_asset(chain_id, wrapped_native)
        })
    })
}

/// The token the Private tab's Swap offers in a `setup:`, `retry-review`, `native-buy` or `max:`
/// mode, `wrapped_native`, so an unfunded wallet can open the mode's swap. `None` in every other
/// mode.
pub(in crate::root) fn setup_swap_token(wrapped_native: Address) -> Option<Address> {
    matches!(
        mode(),
        Some(Mode::Setup(_) | Mode::RetryReview | Mode::NativeBuy | Mode::Max(_))
    )
    .then_some(wrapped_native)
}

/// What New Tor session stages in a `rotation:` mode: `Some(true)` when the broadcaster
/// network fails after the commit, `Some(false)` when the rotation stays pending.
pub(in crate::root) fn tor_rotation() -> Option<bool> {
    match mode()? {
        Mode::Rotation { waku_failed } => Some(waku_failed),
        _ => None,
    }
}

/// Whether the UTXO table shows the rows of the `utxos` mode in place of the wallet's own,
/// whatever the network's sync state.
pub(in crate::root) fn shows_utxos() -> bool {
    matches!(mode(), Some(Mode::Utxos))
}

/// Whether the Derive dialog treats the selected wallet as Ledger-derived and asks the
/// `hardware-derive` mode's stand-in Ledger.
pub(in crate::root) fn hardware_derive() -> bool {
    matches!(mode(), Some(Mode::HardwareDerive))
}

/// The finality the `utxos` mode counts from its own head block, in place of the network's.
/// `None` in every other mode.
pub(in crate::root) fn utxo_finality_context() -> Option<UtxoFinalityContext> {
    shows_utxos().then_some(UtxoFinalityContext::new(
        Some(UTXO_HEAD),
        Some(UTXO_HEAD - UTXO_FINALITY_DEPTH),
        Some(UTXO_FINALITY_DEPTH),
    ))
}

/// The rows of the `utxos` mode, newest first, with ages from the time of the call. They take
/// the USDC and WETH of `chain_id`, or Ethereum's where it lists none. `None` in every other
/// mode, or without either token.
pub(in crate::root) fn utxo_rows(chain_id: u64) -> Option<Vec<UtxoDisplayRow>> {
    const HOUR: u64 = 3_600;
    const DAY: u64 = 24 * HOUR;
    /// 123,456.789012345678901234 units of an 18-decimal token.
    const LONG_AMOUNT: u128 = 123_456_789_012_345_678_901_234;
    if !shows_utxos() {
        return None;
    }
    let token = |symbol: &str| {
        [chain_id, 1]
            .into_iter()
            .find_map(|chain| known_tokens_for_chain(chain).find(|token| token.symbol == symbol))
    };
    let (usdc, weth) = (token("USDC")?, token("WETH")?);
    let now = now_unix();
    let ago = |seconds: u64| now.saturating_sub(seconds);
    let hash = |seed: &str| Some(alloy::hex::encode_prefixed(keccak256(seed)));
    let private = || "Private Output".to_owned();
    Some(vec![
        // Received 3 blocks below the head, with 3 of its 12 finality blocks passed.
        UtxoDisplayRow {
            activity_classification: private(),
            poi_status: "Pending receive".to_owned(),
            ppoi_state: UtxoPpoiState::Unknown,
            poi_spendable: false,
            pending_new: true,
            ..utxo_row(usdc, 250_000_000, 65_020, UTXO_HEAD - 3, ago(36))
        },
        // Spent 7 blocks below the head, with 7 of its 12 finality blocks passed.
        UtxoDisplayRow {
            poi_status: "Pending spend".to_owned(),
            spent_tx_hash: hash("ui-fixture-utxo-spend-65019"),
            spent_block_number: Some(UTXO_HEAD - 7),
            pending_spent: true,
            ..utxo_row(
                usdc,
                1_500_000_000,
                65_019,
                UTXO_HEAD - 3_300,
                ago(11 * HOUR),
            )
        },
        // A Shield 20 minutes old, whose PPOI is due in about 40 minutes.
        UtxoDisplayRow {
            poi_status: "Missing".to_owned(),
            ppoi_state: UtxoPpoiState::Missing,
            poi_spendable: false,
            ..utxo_row(
                weth,
                200_000_000_000_000_000,
                65_018,
                UTXO_HEAD - 100,
                ago(1_200),
            )
        },
        // A proof submitted 23 minutes ago and not verified yet, which can be retried.
        UtxoDisplayRow {
            activity_classification: private(),
            poi_status: "ProofSubmitted".to_owned(),
            ppoi_state: UtxoPpoiState::ProofSubmitted,
            ppoi_last_submission_at: Some(ago(1_380)),
            poi_spendable: false,
            ..utxo_row(usdc, 42_500_000, 65_017, UTXO_HEAD - 600, ago(2 * HOUR))
        },
        utxo_row(
            usdc,
            1_000_000_000,
            65_016,
            UTXO_HEAD - 4_100,
            ago(14 * HOUR),
        ),
        // A blocked Shield whose origin can refund it. It has no UTXO ID, so Refund does
        // nothing.
        UtxoDisplayRow {
            activity_classification: "BlockedShield".to_owned(),
            poi_status: "ShieldBlocked".to_owned(),
            ppoi_state: UtxoPpoiState::ShieldBlocked,
            poi_spendable: false,
            blocked_shield_rescue: Some(BlockedShieldRescueInfo {
                eligible: true,
                disabled_reason: None,
                origin_address: Some(Address::repeat_byte(0x4e).to_checksum(None)),
                public_account_uuid: Some("ui-fixture".to_owned()),
                public_account_label: Some("Savings".to_owned()),
            }),
            ..utxo_row(
                usdc,
                5_000_000_000,
                65_015,
                UTXO_HEAD - 7_800,
                ago(26 * HOUR),
            )
        },
        // Every decimal of an 18-decimal amount, under a long symbol.
        UtxoDisplayRow {
            token: "stkWETH-USDC-LPv2".to_owned(),
            amount: format_scaled_amount(U256::from(LONG_AMOUNT), weth.decimals),
            activity_classification: private(),
            ..utxo_row(weth, LONG_AMOUNT, 65_014, UTXO_HEAD - 21_600, ago(3 * DAY))
        },
        UtxoDisplayRow {
            spent_tx_hash: hash("ui-fixture-utxo-spend-65013"),
            spent_block_number: Some(UTXO_HEAD - 14_400),
            is_spent: true,
            ..utxo_row(
                weth,
                500_000_000_000_000_000,
                65_013,
                UTXO_HEAD - 43_200,
                ago(6 * DAY),
            )
        },
        // A Shield whose PPOI is taking longer than the usual hour.
        UtxoDisplayRow {
            poi_status: "Missing".to_owned(),
            ppoi_state: UtxoPpoiState::Missing,
            poi_spendable: false,
            ..utxo_row(
                weth,
                300_000_000_000_000_000,
                65_012,
                UTXO_HEAD - 50_400,
                ago(7 * DAY),
            )
        },
    ])
}

/// An unspent, verified Shield of `raw` units of `token` at `position` in tree 1, made in
/// `block` at `timestamp`, which each row of the `utxos` mode changes as it needs.
fn utxo_row(
    token: KnownTokenInfo,
    raw: u128,
    position: u64,
    block: u64,
    timestamp: u64,
) -> UtxoDisplayRow {
    let raw_value = U256::from(raw);
    UtxoDisplayRow {
        utxo_id: None,
        tree_position: format!("1/{position}"),
        token: token.symbol.to_owned(),
        token_icon_path: token_icon_asset_path(token.chain_id, &token.token)
            .map(WalletIconSource::embedded),
        amount: format_token_amount(raw_value, token.decimals),
        raw_value: Some(raw_value),
        activity_classification: "Shield".to_owned(),
        poi_status: "Valid".to_owned(),
        ppoi_state: UtxoPpoiState::Valid,
        ppoi_last_submission_at: None,
        poi_spendable: true,
        source_tx_hash: alloy::hex::encode_prefixed(keccak256(format!(
            "ui-fixture-utxo-{position}"
        ))),
        source_block_number: block,
        source_block_timestamp: timestamp,
        spent_tx_hash: None,
        spent_block_number: None,
        token_address: token.token.to_checksum(None),
        is_spent: false,
        pending_new: false,
        pending_spent: false,
        local_pending_spent: false,
        blocked_shield_rescue: None,
    }
}
