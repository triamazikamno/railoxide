//! A debug-only fixture for looking at the Public account swap screens, at a private swap's
//! retry review, failed quote, speed-up, split outcome, reported fill, setup progress,
//! cancelled preparation and delivery to a non-EVM network, at the Stealth accounts inspector, at the refund dialog of a blocked
//! Shield, at a private Send's broadcaster progress and review, at the rows of the UTXO table,
//! at the privacy hints of Shield and Unshield and at a hardware wallet's Derive from private
//! dialog without a device: it opens
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
//! - `utxos` shows nine synthesized rows in the UTXO table of the UTXOs tab in place of the
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
//! - `activity` shows synthesized rows in the Activity tab in place of the wallet's journal, on
//!   any EVM network and on Tron, whatever the sync state. On an EVM network: a public send
//!   still submitting, pinned on top; a swap of Ethereum's WETH from an in-memory stealth-account
//!   record, its first order expired and its retry open, so it is also pinned, shown bridging to
//!   the public send's recipient on Base (Arbitrum when the network is Base), whose detail
//!   lists its setup transaction and whose Open swap… shows My orders, which doesn't hold that
//!   record; a private send confirmed by its spent input note, with
//!   its fee; an unshield from a browser site whose broadcaster timed out (unconfirmed); a
//!   Shield whose notes are in the wallet, so it reads as into this wallet; a Shield with only
//!   its approval sent (incomplete); a public send that reverted, with its fee and gas payer,
//!   selected so the pane beside the list shows it; a contract call the Uniswap dapp requested
//!   that moved three tokens, so its row shows "+2 more"; a governance transaction never
//!   observed (submitted); and a refund that expired without inclusion (failed, no fee). Below
//!   them are a received transaction of USDC and WETH, a Shield received and a blocked Shield
//!   received. The received transaction is newer than the network's last-seen block, so the
//!   Activity tab shows its unread dot until Activity is opened. On Tron: a confirmed USDT
//!   send and an expired TRX send. Clear history… opens the dialog, whose Clear removes the
//!   chosen rows in memory and reports the count. The rows take the network's USDC and WETH,
//!   or Ethereum's, and the copy buttons work. Nothing is read from or written to the vault.
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
//! - `non-evm:<state>` shows a synthesized swap of 0.05 WETH that NEAR Intents delivers as JUP
//!   to a receiver on Solana, through 100.46 USDC on Ethereum, in place of a Public account's
//!   form, on Ethereum with its private session started. The Private tab's Swap opens it too,
//!   and is offered there without private funds. `review` opens the form of the swap's retry,
//!   whose first order expired, holding a synthesized quote of at least 180.42 JUP, about
//!   182.10, which 1Click values at $100.73. No anchor prices JUP, so the form asks for the
//!   unverified-price acknowledgement before Review… opens the review, whose Receive card names
//!   Solana and the receiver with 1Click's estimate of the minimum, $99.80, and
//!   whose rows show NEAR Intents, JUP's mint and Deposit to 1Click. The form's Provider row
//!   reads NEAR Intents with its warning hint, and the Buy panel shows JUP's mint with its copy
//!   button. The retry locks the Buy select, so 1Click's synthesized Solana tokens (SOL, USDC,
//!   the look-alike sUSDC with a stand-in mint, and JUP) only back the quote; a fresh form's
//!   Buy picker shows 1Click's live list with its mints. Approving the
//!   review takes the vault password and ends in an error on the form. The other states open
//!   the swap dialog on the detail of that swap's one order, traded and handed to 1Click:
//!   `in-progress` has no report yet, `delivered` 1Click's report of 181.25 JUP with a stand-in
//!   Solana signature as its destination transaction, and `needs-attention` 1Click's report of
//!   a failed deposit. The detail's actions do nothing, and no provider is asked.
//!   `non-evm:zcash:<state>` shows the same swap delivering ZEC, the only token 1Click lists
//!   on Zcash, to a full-length unified address with Orchard, Sapling and P2PKH receivers:
//!   at least 1.8042 ZEC, about 1.8210, and 1.8125 reported with a stand-in Zcash transaction
//!   ID.
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
//!   reads Route closed. `refused` is that wait ended with the broadcaster's refusal ("Gas
//!   cost is too high…") over the `ready` route: the step offers Retry with custom fee… and
//!   Try another broadcaster, which a stand-in Send form backs. `direct` is `ready` in direct
//!   network mode, where the chip reads Not isolated, whatever the wallet's network mode is.
//!   The route's chip and its hover card show the synthesized route, which never changes.
//!   Stop waiting marks the wait stopped, and Close closes the dialog. No job runs, nothing is
//!   proved or sent, and no record is written.
//! - `broadcaster-review` lets the Private tab's Send offer the same stand-in 1.0 as
//!   `broadcaster-progress:`. The network's private session must be started and synced. Send
//!   then opens the review of a private Send via broadcaster in place of the form: 0.5 of the
//!   token to the wallet's own address, over a stand-in broadcaster's quote of two plain
//!   transactions with a fee of 1.25 USDC (Ethereum's USDC, in raw units, where the network
//!   lists none). The review is the wallet's own, so it shows the notice that a broadcaster
//!   could unbundle the batch, with its sentence about paying the fee in the payment's token.
//!   The quote is never estimated again. Approving the review takes the vault password and ends
//!   with nothing proved or sent, and no record is written.
//! - `delivery:<state>` lets the Private tab's Send and Unshield offer a stand-in 1.0 of the
//!   network's wrapped native token, with its icon, when the wallet holds nothing it can send
//!   or unshield. The network's private session must be started. Each form then opens held at
//!   the state, with its recipient and amount empty; a real asset's form is held too. Unshield
//!   opens as a plain withdrawal, with unwrap off. `estimate` shows a stand-in broadcaster
//!   estimate of two plain transactions with a fee of 1.25 USDC, and `refreshing` that estimate
//!   while it refreshes. `custom-fee` has no estimate and a custom fee of 1.25 USDC, with its edit
//!   button. `retry` shows a failed estimate with
//!   its Retry, and `pin-conflict` that failure on a form whose pinned notes can't make the
//!   attempt, where Retry is hidden and Submit opens the pinned-notes prompt. `estimate` and
//!   `pin-conflict` offer the fee's USDC as a fee token with one eligible broadcaster, so their
//!   Submit is enabled; the other broadcaster states keep the wallet's own fee tokens, so on an
//!   unfunded wallet their Submit stays disabled and the fee-token warning shows. In `estimate` and
//!   `pin-conflict` a paired Gateway client's private broadcaster drafts get the same stand-in USDC
//!   option, 1,000 spendable with one eligible broadcaster, in place of the wallet's own fee
//!   tokens. The fee is paid in the network's USDC, or Ethereum's where the network lists none,
//!   which then shows in raw units. Both forms show the same Send-shaped estimate, with no public
//!   output and no Unshield protocol fee, so the withdrawal fee row isn't covered. `sponsored` is
//!   Self-broadcast funded by private sponsorship, on a network that offers it (Ethereum by
//!   default), whose estimate card shows too little private wrapped native token for its fees:
//!   0 available, 0.01 required. No gas quote is requested on open, so Max fee and Max tip
//!   initially read unavailable. Refresh requests a live gas quote. `submitted` is the stand-in
//!   estimate accepted by a broadcaster, whose button reads Submitted and is disabled. In both
//!   forms, View status does nothing because this fixture does not create submission progress.
//!   The forms' scheduled broadcaster and sponsorship estimates don't run, and sync snapshots
//!   leave the forms alone, so Retry changes nothing.
//!   Editing an input still clears the estimate, error, result or sponsored card, as on any form,
//!   and Max, the broadcaster filters, Random or Specific, favoriting or blocking a broadcaster and
//!   the estimate's refresh or F5 drop the stand-in estimate too, with nothing in its place;
//!   reopening the form restores the state. Other estimates stay live: the broadcaster picker and
//!   the fee editor can estimate on a Ready chain with a live broadcaster for the fee token, and
//!   show the result only in that picker or editor. A paired Gateway client's draft estimates and
//!   Self-broadcast gas quotes remain live. Whatever asset the form holds, an approved Send or
//!   Unshield stops with the fixture's error before executor account preparation, proof
//!   generation or submission, so no record of that action is written.
//! - `hints:<state>` replaces the amount statistics of Shield's and Unshield's privacy hints with
//!   a stand-in snapshot: fresh in `fresh`, `direct-exit` and `no-repeat`, out of date in
//!   `out-of-date`. Nothing is fetched or stored. Public's Add account button becomes Preview
//!   Shield hints…, which opens a Shield form of the network's USDC, or Ethereum's in raw units
//!   where the network lists none, with 1,370 entered, a stand-in balance of 1,500 and tree 2 as
//!   the current tree, as in the `shield:` modes, whose Shield does nothing. The snapshot counts
//!   no deposit of 1,370, 32 of 1,200 and 7 in tree 2, with 2,350,000 in the pool. The Show
//!   common amounts button under the amount factor opens the common amounts, any of which fills
//!   in the amount: the five nearest below 1,370, from 250 to 1,200 with 1,204 of 1,000, then
//!   1,370 itself, then 2,000 dimmed because it is above the balance. The snapshot's 100 is a
//!   sixth below.
//!   `no-repeat` lists no repeated amount, so the form says none was used twice and offers no
//!   common amounts, and `out-of-date` shows its notice and the network factor alone. The
//!   Private tab's Unshield offers a stand-in 1.0 of the network's wrapped native token when the
//!   wallet holds nothing it can unshield, held as `delivery:estimate` holds it but with fees
//!   deducted from the amount, and filled with 0.5 to the first Public account, or to
//!   0x1111…1111 without one, over a stand-in estimate that plans one unshield of 0.5 from
//!   tree 0.
//!   In `direct-exit` it spends a 0.5 deposit made two days ago in full, with 1 other deposit of
//!   that value in tree 0, 12 deposits there above its value bucket and 9 exits of the token in
//!   30 days. In `out-of-date` it spends half of a deposit made 20 minutes ago, and in `fresh`
//!   and `no-repeat` half of a note received in a private transfer. In `fresh` the wallet has a
//!   stand-in deposit of 0.51 in tree 0, so the exit is within 5% of it, with the 1,201 deposits
//!   of 0.5 in that range, and the Show common exit amounts button at the end of the hints lists
//!   the amounts at least 5 exits used in 30 days, any of which fills in the amount: 0.1, 0.25
//!   and 0.3, then 0.5 itself with 90, then 0.75 and 1, then 2 dimmed because it is above the
//!   form's maximum of 1.0. On a started and synced private session, Submit opens the review,
//!   which repeats the factors, and approving it stops with the fixture's error as in
//!   `delivery:`. Editing the Unshield form, or choosing a common exit amount, drops the
//!   stand-in estimate and its plan, as in `delivery:`; reopening the form restores them.
//!   Switching the form to Add on top makes the common exit amounts browse-only, but here the
//!   switch is such an edit: it drops the plan, and the list with it, so the fixture can't show
//!   the browse-only list. Reopening the form restores Deduct and the estimate.
//! - `hardware-derive` shows the Derive from private dialog, from Public's Add account, as a
//!   hardware wallet's: its authorize state with Show addresses, its preview and the progress
//!   of an add. Whatever wallet is selected, the dialog treats it as Ledger-derived and asks a
//!   stand-in Ledger, which needs no unlocked hardware profile. Show addresses answers after
//!   about 2 seconds with a fixed account key, the compressed secp256k1 generator with a fixed
//!   chain code, so every wallet previews the same addresses. Add holds forever on the device
//!   confirmation of the first selected index that has no account, which the stand-in never
//!   answers. Close the dialog to leave it. Nothing is read from a device, no account is
//!   stored, and nothing is sent.
//! - `tron:<state>` replaces the selected wallet's Tron accounts, on Tron in the network
//!   selector, with three synthesized ones and their balances, priced at a stand-in $0.25 per
//!   TRX until Tron's price refresh replaces it: Savings holds 2,480.35 TRX and 1,250 USDT,
//!   Trading 35.12 TRX and 312.5 USDT, and Deposit only 25 USDT, so it doesn't exist on chain.
//!   Their balances and resources are never read: the rows show 100,000 energy, 1,500 staked
//!   and 600 free bandwidth on Savings, only 600 free bandwidth on Trading, and that Deposit
//!   isn't on chain. A Send form's reads are answered in memory, with stand-in fee parameters,
//!   the same resources and an energy estimate of 64,285, or 130,285 to a receiver without the
//!   token, and its Review opens the review, whose approval ends in an error on the form. These
//!   live in `public_account/tron_send.rs`. `accounts` opens Send forms empty; Deposit's says
//!   Send is unavailable until the account receives TRX. `new-account` fills a form with 25 to
//!   a receiver that doesn't exist on chain and holds no token, so a TRX send's review adds the
//!   account creation fee and says the send creates the account, and a token send's notes that
//!   the receiver doesn't hold the token yet. `waiting`, `complete`, `failed`,
//!   `token-did-not-move` and `dropped` fill the form with 25 to another of the accounts and
//!   open the progress of a synthesized send over it, handed off with its transaction id:
//!   waiting for solidified blocks after the endpoint didn't answer the broadcast, completed,
//!   failed after burning 7.7142 TRX, included without moving the token after burning
//!   0.345 TRX, or expired with no TRX spent. A swap form takes these modes as active, as it
//!   does every mode.
//! - `update-check:behind`, `update-check:current`, `update-check:ahead` and
//!   `update-check:failed` open the update check dialog over the first frame of the
//!   locked-vault screen, on a canned result placed against this build's version, with no
//!   request to GitHub. `behind` lists three releases newer than the build, highlighted and
//!   expanded, with notes that hold a link, an image, a code block, a list and raw HTML, then
//!   the build's own release and eleven older ones, enough to scroll; a newer prerelease in
//!   the canned list doesn't show. `current` has the build's release as the latest, expanded,
//!   above the older ones, and `ahead` only the older ones, below the installed marker.
//!   `failed` is a check GitHub rejected with 403 Forbidden, with the Tor hint when the
//!   profile is in Tor mode. Check for updates and Retry show the canned result again, and a
//!   later opening starts from the disclosure. A swap form doesn't take these modes as
//!   active.
//! - `logs` fills the logs panel's buffer once with 300 synthesized lines, one a second over
//!   the five minutes before the wallet opened: every level, the sibling targets
//!   `arti_client::client`, `arti_client::status` and `arti_client::dirmgr`,
//!   `wallet_ops::rpc_broker` with its submodule `wallet_ops::rpc_broker::warmup` beside
//!   `wallet_ops::rpc_broker_metrics`, and `wallet_ops::poi`, with long messages that wrap and
//!   endpoint URLs in their redacted form. The wallet's own lines follow them. A swap form
//!   doesn't take this mode as active.
//! - `first-start:old-notice` treats the accepted responsibility notice as version 0 the first
//!   time the setup record is read, so a profile that finished first-start setup shows only
//!   the notice step again. The fixture writes no record; the notice's Continue saves the
//!   current version as it always does. A swap form doesn't take this mode as active.
//! - `walletconnect-request` opens the Dapp request dialog, the first time the unlocked wallet
//!   renders, on a pending `eth_sendTransaction` from Uniswap at `https://app.uniswap.org`: an
//!   unlimited approval of Ethereum's WETH to Uniswap's Permit2 from the first software Public
//!   account on Ethereum. The request is staged in memory, with no session, route or record,
//!   and a wallet without such an account gets none. Its fee is a fixed estimate at a base fee
//!   of 1 gwei and a tip of 0.1 gwei, which Refresh shows again without asking the network.
//!   Approve shows "The debug UI fixture's request can't be approved." and does nothing else,
//!   and Reject fails for lack of a route and sends nothing. A swap form doesn't take this mode
//!   as active.
//!
//! In every mode the jobs of a swap are scripted or held back: nothing is claimed, proved,
//! signed, paid or sent, and no record is written. The synthesized records live in memory
//! only, and nothing reads, tracks or acts on them. A `retry-review`, `non-evm:review` or
//! speed-up mode, and the
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
    KnownTokenInfo, format_scaled_amount, format_token_amount, group_thousands,
    known_tokens_for_chain, token_icon_asset_path,
};
use wallet_ops::bridge::{NearAssets, NonEvmBridgeDestination, NonEvmNetwork};
use wallet_ops::cow::{CowOrderStatusHint, CowOrderStatusReport, OrderLimitError, OrderUid};
use wallet_ops::release_check::{
    GithubRelease, RAILOXIDE_REPOSITORY_URL, ReleaseCheckError, ReleaseTimeline, classify,
};
use wallet_ops::settings::SwapProfile;
use wallet_ops::vault::{
    AcrossOrderTerms, ActivityAsset, ActivityAttempt, ActivityAttemptOutcome, ActivityCounterparty,
    ActivityDirection, ActivityEntry, ActivityEntryId, ActivityFee, ActivityInput, ActivityKind,
    ActivityLeg, ActivityMarkers, ActivityMatch, ActivityNetwork, ActivityOwner, ActivitySource,
    ActivityStep, BridgeDelivery, BridgeOrderTerms, BridgePrivateDelivery, BridgeProvider,
    BridgeShieldFailure, BridgeSurplus, ExecutorNonceObservation, ExecutorNonceWatermark,
    ExecutorOperationId, ExecutorPayloadContext, ExecutorPayloadPurpose, ExecutorRecord,
    IssuedExecutorPayload, NearIntentsOrderTerms, NonEvmDelivery, PublicSwapApproval,
    PublicSwapDeposited, PublicSwapIntent, PublicSwapObservations, PublicSwapRecord, SwapApproval,
    SwapApprovalTokens, SwapApprovedAccount, SwapApprovedAccounts, SwapApprovedBounds,
    SwapBridgeHandoff, SwapBridgeOutcome, SwapDelivery, SwapObservation, SwapOrderObservations,
    SwapPreHookDeath, SwapPreHookDeathCause, SwapProof, SwapRecipient, SwapSetupFeeLimit,
    SwapTerms, SwapTradeAmounts, SwapUseId,
};
use wallet_ops::{
    BlockedShieldRescueBlocker, BlockedShieldRescueInfo, BlockedShieldRescueUtxoId,
    ExecutorRecoveryFeeEstimate, OperationNetworkIsolation, PairAnchorRate,
    PublicBroadcasterResultKind, SUBMISSION_ROUTE_RECOVERY_LIMIT, SubmissionRouteProgress,
    SubmissionRouteSender, SwapBridgeReport, SwapContinuingApproval, SwapDeliverySeen,
    SwapHandoffHint, SwapQuoteAnchor, SwapQuoteDiagnostics, SwapReview, SwapReviewChange,
    SwapRowRecipient, SwapSettlementHint, SwapSpeedUpRefusal, TransactionGenerationStage,
    UtxoOutput, UtxoPpoiState, swap_rows,
};

use super::dialog::SwapDialogView;
use super::form::SetupSide;
use super::model::SwapIdentity;
use super::setup_progress::SetupProgress;
use super::{PrivateSwapsView, SwapEarlyHint, SwapJob, SwapJobKind, SwapOrderHint};
use crate::assets::WalletIconSource;
use crate::root::WalletRoot;
use crate::root::private_broadcaster::{
    apply_private_broadcaster_progress_stage, finish_private_broadcaster_progress_steps_at_stage,
};
use crate::root::ui_helpers::now_unix;
use crate::root::utxo::{BlockedShieldRefundDialogState, UtxoDisplayRow, UtxoFinalityContext};
use crate::root::wallet_activity::{ActivityData, ActivityRowId, ActivityStandIn};

mod activity_stand_in;
mod amount_hints;
mod swap_screens;
mod wallet_screens;
mod wallet_stand_ins;

mod account_records;
mod grammar;
mod non_evm_records;
mod queries;
mod setup_records;
mod swap_records;

pub(in crate::root) use account_records::*;
#[allow(clippy::wildcard_imports)]
use grammar::*;
pub(super) use non_evm_records::*;
pub(in crate::root) use queries::*;
pub(super) use setup_records::*;
#[allow(clippy::wildcard_imports)]
use swap_records::*;

pub(in crate::root) use activity_stand_in::activity;
pub(in crate::root) use amount_hints::{
    amount_statistics, hints_active, hints_shield_token, own_deposit_stand_ins,
    shield_hint_stand_ins,
};
pub(in crate::root) use wallet_stand_ins::{
    fill_logs, hardware_derive, holds_delivery, holds_send_review, send_asset, setup_swap_token,
    shows_utxos, tor_rotation, tron, unshield_asset, update_check, utxo_finality_context,
    utxo_rows, walletconnect_request,
};
pub(super) use wallet_stand_ins::{locked_sell_value, max_asset};

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

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
    BroadcasterReview,
    Delivery(Delivery),
    Hints(Hints),
    HardwareDerive,
    NonEvm(NonEvm, NonEvmNetwork),
    Tron(Tron),
    Activity,
    UpdateCheck(UpdateCheck),
    Logs,
    FirstStartOldNotice,
    WalletConnectRequest,
}

/// The canned result an `update-check:` mode shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateCheck {
    Behind,
    Current,
    Ahead,
    Failed,
}

/// What a `tron:` mode shows of the synthesized Tron accounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum Tron {
    /// The accounts, whose Send forms open empty.
    Accounts,
    /// A Send form filled with a TRX send that creates its receiver's account.
    NewAccount,
    /// The progress of a handed-off send, waiting for solidified blocks.
    Waiting,
    Complete,
    /// Included and failed, burning TRX.
    Failed,
    /// Included without the token's `Transfer` log, burning TRX.
    TokenDidNotMove,
    /// Expired without inclusion.
    Dropped,
}

/// What a `non-evm:` mode shows of the synthesized swap delivered by NEAR Intents to a
/// receiver on Solana.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NonEvm {
    /// The retry's form, holding a synthesized quote to review.
    Review,
    /// Handed off to 1Click, which hasn't reported the delivery yet.
    InProgress,
    /// 1Click reports the delivery, with its Solana transaction.
    Delivered,
    /// 1Click reports the deposit as failed or incomplete.
    NeedsAttention,
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
    /// The broadcaster refused the request.
    Refused,
    /// Waiting in direct network mode, with no route of its own.
    Direct,
}

/// What a `delivery:` mode holds on a Send or Unshield form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivery {
    /// A stand-in broadcaster estimate.
    Estimate,
    /// That estimate while it is refreshed.
    Refreshing,
    /// A custom broadcaster fee and no estimate.
    CustomFee,
    /// A failed estimate with its Retry.
    Retry,
    /// That failure once the estimate found the form's pinned notes can't make the attempt.
    PinConflict,
    /// Self-broadcast funded by private sponsorship, short of wrapped native token.
    Sponsored,
    /// The broadcaster accepted the submission.
    Submitted,
}

/// The amount statistics and plan a `hints:` mode shows on Shield and Unshield.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hints {
    /// A fresh snapshot and an exit that doesn't spend a deposit directly.
    Fresh,
    /// An out-of-date snapshot and an exit soon after a deposit.
    OutOfDate,
    /// A fresh snapshot and an exit that spends a deposit directly.
    DirectExit,
    /// A fresh snapshot in which no amount of either token repeats.
    NoRepeat,
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

/// The origin block a `detail:` mode's destination record trades and approves in.
const STAGED_ORIGIN_BLOCK: u64 = 31_204_518;
/// The destination block a `detail:` mode's destination record is set up in.
const STAGED_DESTINATION_BLOCK: u64 = 402_118_977;
/// The transaction a `detail:` mode's destination record observes its steps in.
const STAGED_TRANSACTION: B256 = B256::repeat_byte(0x7a);
/// The destination account a `detail:` mode's record is for.
const STAGED_ACCOUNT: Address = Address::repeat_byte(0x3a);
/// The delegate a `detail:` mode's destination account is set up with.
const STAGED_DELEGATE: Address = Address::repeat_byte(0x5d);

/// The Solana receiver of a `non-evm:` mode's swap: 32 bytes no wallet holds, in base58.
const NON_EVM_RECEIVER: &str = "9jQtwHhZT1H2TYSMt74msmBmy8UPen4GUysNynPUVkkv";
/// The Zcash receiver of a `non-evm:zcash:` mode's swap: a unified address with stand-in
/// Orchard, Sapling and P2PKH receivers, at full length.
const NON_EVM_ZCASH_RECEIVER: &str = "u1jk27zmdnx2pswc7ev0tfeghytfwj4swzkqlsfkhzffd59ee8tw0p2eqvrdtj4j6lrajupn2m5qgk0u9sfdmh3c4f7ulan3yqe3uweqw0s2gc26ezg766xg6r3u9xkcyc492ke9832xnrnja0phgk6r73rzd742dlpv67yxjael8e8ll9fm69kj9ce372cz85jx426s39uxhsc352anx";
/// 1Click's Solana transaction of a `non-evm:delivered` mode's delivery, a stand-in signature.
const NON_EVM_TRANSACTION: &str =
    "2zf1D3JpcScRk9TTCWSQmbKr8hB9nfiLK9iA7mHvNuxqSrDDLCRFY7mb48txGysNHWns8autYb6meqyhRAPs23qT";
/// 1Click's Zcash transaction of a `non-evm:zcash:delivered` mode's delivery, a stand-in ID.
const NON_EVM_ZCASH_TRANSACTION: &str =
    "5f2c9e0a7b41d3866e1fa09c2d47b8e5130f6ac92b84d7e1c05a3f9b6d2e8174";
/// The base units a `non-evm:` mode's deposit is quoted to deliver, and the units a delivery
/// reports: at least 180.42 JUP, about 182.10, and 181.25 reported, or with ZEC's 8 decimals
/// 1.8042, 1.8210 and 1.8125 ZEC.
const NON_EVM_MINIMUM: u64 = 180_420_000;
const NON_EVM_EXPECTED: u64 = 182_100_000;
const NON_EVM_REPORTED: u64 = 181_250_000;

/// The block a `setup:included-order-open` mode's private sync would show its swap's own setup
/// in, just past the confirmed nonce read of its record.
const SETUP_INCLUDED_BLOCK: u64 = SWAP_ORIGIN_BLOCK + 1_503;

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
