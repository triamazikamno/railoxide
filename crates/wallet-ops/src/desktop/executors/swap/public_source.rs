//! The destination side of a swap paid from a Public account on another chain: the claim of
//! this chain's destination stealth account, its setup when it is new, its delegation check,
//! its guarded shield, the handler message and the Across quote requested while signing.
//!
//! The swap has no source stealth account, so everything here runs on the destination chain's
//! owner. Nothing here signs or sends anything for the Public account.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, Bytes, U256, keccak256};
use broadcaster_core::contracts::across::private_delivery_message;
use broadcaster_core::contracts::executor::AcrossPrivateDelivery;
use eyre::{Result, eyre};

use super::bridge::{BridgeSigning, across_order_terms};
use super::order::{approval_cushion, scaled_bridge_deposit};
use super::{
    DelegatedSwapExecutor, SwapPairSide, SwapReviewChange, SwapShieldNotes, existing_swap_account,
    is_live_swap_use, trace_step,
};
use crate::bridge::{
    AcrossClient, AcrossFeeQuote, AcrossFeeRequest, AcrossMessageFeeRequest, BridgeApiError,
};
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, BridgePrivateDelivery, BridgeProvider,
    BridgeShieldFailure, BridgeSurplus, ExecutorOperationId, ExecutorRecord, ExecutorStoreError,
    PublicAccountScope, PublicSwapApproval, PublicSwapClaim, PublicSwapIntent,
    PublicSwapSourceTerms, SwapAccountChoice, SwapUseId, SwapUseRecord, SwapUseRelease,
    SwapUseRole,
};
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorOwner, PublicBroadcasterCandidate,
    RAILGUN_PROTOCOL_FEE_BPS,
};

/// The claim of a swap paid from a Public account: see [`ExecutorOwner::claim_public_swap`].
pub struct PublicSwapUseClaim {
    pub id: SwapUseId,
    pub origin_chain: u64,
    /// The Public account that pays.
    pub source: Address,
    pub source_scope: PublicAccountScope,
    pub account: SwapAccountChoice,
    pub destination_token: Address,
    pub intent: PublicSwapIntent,
    pub approval: PublicSwapApproval,
}

/// What signing the delivery of a Public-paid swap needs.
pub struct PublicSwapDeliverySigning<'a> {
    pub operation: ExecutorOperationId,
    pub swap_use: SwapUseId,
    pub delegated: DelegatedSwapExecutor,
    /// The swap's own chain, for its pinned `SpokePool`.
    pub origin: &'a EffectiveChainConfig,
    pub across: &'a AcrossClient,
    /// The amount the deposit is quoted for: the sold amount for a direct deposit, the order's
    /// approved buy amount otherwise.
    pub input_amount: U256,
    /// Unix seconds until which the deposit can be made: the order's `validTo`, or the
    /// deposit's own deadline.
    pub valid_to: u32,
    pub authorization: &'a DesktopPrivateSpendAuthorization,
    pub notes: Option<&'a dyn SwapShieldNotes>,
}

/// The signed delivery, or the review change that stopped it.
// The signed terms are returned once per swap and never stored in bulk.
#[allow(clippy::large_enum_variant)]
pub enum PublicSwapDelivery {
    /// The destination account's shield is signed and recorded. `message` is the handler
    /// message that runs it, and `terms` are the deposit terms Across quoted for it. An
    /// order's `terms.input_amount` is the buy amount to sign it with: the approved one, or
    /// one raised within the approval's cushion.
    Signed {
        delivery: AcrossPrivateDelivery,
        message: Bytes,
        terms: AcrossOrderTerms,
    },
    /// A term changed since the review. The Public account must sign nothing for it.
    ReviewRequired {
        change: SwapReviewChange,
        /// The real delivery quote at the originally reviewed input amount, when its output
        /// fell below the approved minimum. It can replace the preview for another review.
        quote: Option<PublicSwapDeliveryQuote>,
    },
}

/// A signing-time delivery quote bound to the original reviewed route and deposit amount.
/// It carries no account, handler message or signed payload.
#[derive(Debug, Clone)]
pub struct PublicSwapDeliveryQuote {
    pub(super) request: AcrossFeeRequest,
    pub(super) fees: AcrossFeeQuote,
}

impl ExecutorOwner {
    /// Claim this chain's destination stealth account for a swap paid from a Public account on
    /// another chain, before any of its preparation. Reads no chain state, derives no key and
    /// signs nothing.
    pub fn claim_public_swap(&self, request: PublicSwapUseClaim) -> Result<ExecutorRecord> {
        let PublicSwapUseClaim {
            id,
            origin_chain,
            source,
            source_scope,
            account,
            destination_token,
            intent,
            approval,
        } = request;
        self.ensure_active()?;
        let delegate = self.swap_destination_profile()?.delegate();
        if origin_chain == self.chain.chain_id {
            return Err(eyre!(
                "a swap paid from a Public account delivers to another network than it pays on"
            ));
        }
        let setup = matches!(account, SwapAccountChoice::New(_));
        // A new account's setup is paid within the limit approved for this chain.
        if setup && approval.bounds.destination_setup_fee.is_none() {
            return Err(eyre!(
                "the swap's approval has no destination setup fee limit"
            ));
        }
        if approval.bounds.destination_minimum.is_none() {
            return Err(eyre!("the swap's approval has no destination minimum"));
        }
        if approval.destination.setup != setup {
            return Err(eyre!(
                "the swap's approval names another setup need than its destination stealth account has; review the swap again"
            ));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| eyre!("the system clock is before the Unix epoch"))?
            .as_secs();
        let record = self.store.claim_public_swap(PublicSwapClaim {
            id,
            origin_chain,
            source,
            source_scope,
            account,
            delegate,
            destination_token,
            bridged_token: intent.bridged_token,
            order: intent.order,
            approval,
            now,
        })?;
        self.notify_change();
        Ok(record)
    }

    /// The refusal [`Self::claim_public_swap`] would give a swap with the source terms `terms`
    /// for its Public account, before a destination account is chosen or anything is approved:
    /// `None` when the account can pay for it. Reads this wallet's records only and writes
    /// nothing.
    pub fn public_swap_source_conflict(
        &self,
        terms: &PublicSwapSourceTerms,
    ) -> Result<Option<ExecutorStoreError>> {
        self.ensure_active()?;
        if terms.origin_chain == self.chain.chain_id {
            return Err(eyre!(
                "a swap paid from a Public account delivers to another network than it pays on"
            ));
        }
        Ok(self.store.public_swap_source_conflict(terms)?)
    }

    /// Persist a full new review before retrying the unsigned swap's delivery. Its source and
    /// destination identity stay fixed, and issued shields remain recorded under the use.
    pub fn reapprove_public_swap(
        &self,
        operation: ExecutorOperationId,
        swap_use: SwapUseId,
        approval: PublicSwapApproval,
    ) -> Result<ExecutorRecord> {
        self.ensure_active()?;
        let record = self
            .store
            .reapprove_public_swap(operation, swap_use, approval)?;
        self.notify_change();
        Ok(record)
    }

    /// Cancel a claimed swap the Public account has signed nothing for.
    pub fn cancel_public_swap(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
    ) -> Result<SwapUseRelease> {
        self.ensure_active()?;
        let released = self.store.cancel_public_swap(operation, id)?;
        self.notify_change();
        Ok(released)
    }

    /// Prepare the destination account of a claimed swap: a new account is derived and
    /// inspected for its setup, an existing one is checked against the authorization. The
    /// approval then binds the account's address. `candidate` is the setup's broadcaster,
    /// exactly for a new account. The setup itself is submitted with
    /// [`Self::submit_swap_setup`].
    pub async fn prepare_public_swap_destination(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        candidate: Option<PublicBroadcasterCandidate>,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<SwapPairSide> {
        self.ensure_active()?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("stealth account is unavailable"))?;
        let (claimed, _) = record.public_swap_use(id).ok_or_else(|| {
            eyre!("this stealth account isn't claimed by a swap paid from a Public account")
        })?;
        if !is_live_swap_use(&record, id) {
            return Err(eyre!("this swap was stopped"));
        }
        if candidate.is_some() != claimed.is_fresh() {
            return Err(eyre!(
                "a setup broadcaster belongs to a new stealth account only"
            ));
        }
        let side = match candidate {
            Some(candidate) => SwapPairSide::Setup(
                Box::pin(self.resume_swap_setup(operation, candidate, authorization)).await?,
            ),
            None => SwapPairSide::Existing {
                operation,
                executor: existing_swap_account(
                    self,
                    SwapAccountChoice::Existing(operation),
                    authorization,
                )?
                .ok_or_else(|| eyre!("stealth account is unavailable"))?,
            },
        };
        self.ensure_active()?;
        self.store
            .bind_public_swap_destination(operation, id, side.executor())?;
        self.notify_change();
        self.require_live_swap_use(operation, Some(id))?;
        Ok(side)
    }

    /// Confirm that the destination account of the swap `swap_use` is delegated at `confirmed`,
    /// as `delegated_swap_destination` does for a stealth pair.
    pub async fn delegated_public_swap_destination(
        &self,
        operation: ExecutorOperationId,
        confirmed: u64,
        swap_use: SwapUseId,
        notes: Option<&dyn SwapShieldNotes>,
    ) -> Result<DelegatedSwapExecutor> {
        self.delegated_destination(operation, confirmed, swap_use, notes, |record| {
            record.public_swap_use(swap_use).is_some() && record.address().is_some()
        })
        .await
    }

    /// Sign the delivery of a swap paid from a Public account, before that account signs its
    /// deposit or order. The destination account signs its guarded shield for the approved
    /// destination minimum, which is persisted in its record before anything leaves the
    /// wallet. Across then quotes the deposit with this chain's handler and the message that
    /// runs that shield, so it simulates the real fill.
    ///
    /// A destination shield fee rate other than the approved one stops before the shield is
    /// signed. A quote below the approved destination minimum stops after it, and the shield
    /// stays recorded, as it does for a private swap that stops there.
    ///
    /// An order whose quote is below that minimum by little is not stopped. Its buy amount is
    /// raised as a private swap's deposit is: `ceil(buy * approved / quoted)` plus the margin.
    /// When that is at most a fifth of the approved allowed gas above the approved buy amount,
    /// Across quotes the raised amount and the terms are for it. The approved minimums stand,
    /// and the order leaves solvers that much less for gas. A direct deposit sells an exact
    /// amount and has no such cushion.
    pub async fn sign_public_swap_delivery(
        &self,
        signing: PublicSwapDeliverySigning<'_>,
    ) -> Result<PublicSwapDelivery> {
        let PublicSwapDeliverySigning {
            operation,
            swap_use,
            delegated,
            origin,
            across,
            input_amount,
            valid_to,
            authorization,
            notes,
        } = signing;
        self.ensure_active()?;
        let record = self
            .swap_account_record(operation)?
            .ok_or_else(|| eyre!("the destination stealth account is unavailable"))?;
        let Some(SwapUseRole::PublicSourceDestination {
            origin_chain,
            destination_token,
            swap,
            ..
        }) = record.swap_use(swap_use).map(SwapUseRecord::role)
        else {
            return Err(eyre!(
                "this stealth account isn't claimed by a swap paid from a Public account"
            ));
        };
        if !is_live_swap_use(&record, swap_use) {
            return Err(eyre!("this swap was stopped"));
        }
        let (origin_chain, destination_token) = (*origin_chain, *destination_token);
        if origin.chain_id != origin_chain {
            return Err(eyre!(
                "the network the swap pays on differs from the one it was claimed for"
            ));
        }
        let spoke_pool = origin
            .bridge_origin_profile()
            .ok_or_else(|| eyre!("this network has no pinned Across SpokePool"))?
            .spoke_pool();
        let handler = self
            .chain
            .bridge_profile()
            .ok_or_else(|| eyre!("private Bridge delivery is unavailable on this chain"))?
            .multicall_handler();
        let (approval, intent) = (swap.approval(), swap.intent());
        let destination_executor = delegated.executor();
        if delegated.operation() != operation
            || record.address() != Some(destination_executor)
            || approval.destination.address != Some(destination_executor)
        {
            return Err(eyre!(
                "the destination stealth account changed; review the swap again"
            ));
        }
        let destination_minimum = approval
            .bounds
            .destination_minimum
            .ok_or_else(|| eyre!("the swap's approval has no destination minimum"))?;
        let on_shield_failure = approval.on_shield_failure;
        // The destination chain shields at the shared protocol fee, as for a private swap.
        let current = RAILGUN_PROTOCOL_FEE_BPS;
        let approved = approval
            .bounds
            .destination_shield_fee_bps
            .unwrap_or_default();
        if approved != current {
            return Ok(PublicSwapDelivery::ReviewRequired {
                change: SwapReviewChange::DestinationShieldFee { approved, current },
                quote: None,
            });
        }

        // The shield's payload is durable in this account's record before the quote request
        // carries it out of the wallet.
        let shield_multicall = trace_step(
            "public_swap_destination_shield",
            self.issue_swap_destination_shield(
                delegated,
                swap_use,
                destination_token,
                destination_minimum,
                authorization,
                notes,
            ),
        )
        .await?;
        // Without a fallback a failing shield reverts the fill, and Across refunds the deposit
        // on the chain it was made on.
        let fallback = (on_shield_failure == BridgeShieldFailure::KeepOnDestination)
            .then_some(destination_executor);
        let message = private_delivery_message(
            handler,
            destination_token,
            destination_executor,
            shield_multicall.clone(),
            fallback,
        );
        let delivery = AcrossPrivateDelivery {
            handler,
            destination_executor,
            shield_multicall,
            fallback,
        };
        let mut request = AcrossMessageFeeRequest {
            fee: AcrossFeeRequest {
                input_token: intent.bridged_token,
                output_token: destination_token,
                origin_chain,
                destination_chain: self.chain.chain_id,
                amount: input_amount,
            },
            recipient: handler,
            message: message.clone(),
        };
        let fees = self
            .public_swap_bridge_quote(across, &request, intent.order)
            .await?;
        // The deposit pays the handler, so the terms take a private delivery. Across terms
        // read no surplus choice.
        let bridge = BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: self.chain.chain_id,
            receiver: destination_executor,
            destination_token,
            surplus: BridgeSurplus::Reshield,
            private: Some(BridgePrivateDelivery { on_shield_failure }),
        };
        let handler_message = Some((handler, keccak256(&message)));
        let quote = PublicSwapDeliveryQuote {
            request: request.fee,
            fees,
        };
        let mut signing = across_order_terms(
            &fees,
            spoke_pool,
            intent.bridged_token,
            bridge,
            input_amount,
            destination_minimum,
            valid_to,
            handler_message,
        )?;
        // An order's quote that fell short within the approval's cushion is taken again for
        // the buy amount that delivers the approved minimum.
        if let BridgeSigning::Changed(SwapReviewChange::DestinationMinimum { current, .. }) =
            signing
            && intent.order
            && let Some(allowance) = approval.bounds.gas_allowance
            && let Some(raised) = scaled_bridge_deposit(input_amount, destination_minimum, current)
                .map(|raised| raised.max(input_amount))
            && raised - input_amount <= approval_cushion(allowance)
        {
            request.fee.amount = raised;
            let fees = self
                .public_swap_bridge_quote(across, &request, true)
                .await?;
            let raised_signing = across_order_terms(
                &fees,
                spoke_pool,
                intent.bridged_token,
                bridge,
                raised,
                destination_minimum,
                valid_to,
                handler_message,
            )?;
            // A still-short quote for the raised amount can't replace the review at its
            // original input. Keep that first quote and change together.
            if matches!(raised_signing, BridgeSigning::Terms(_)) {
                signing = raised_signing;
            }
        }
        match signing {
            BridgeSigning::Changed(change) => Ok(PublicSwapDelivery::ReviewRequired {
                quote: matches!(change, SwapReviewChange::DestinationMinimum { .. })
                    .then_some(quote),
                change,
            }),
            BridgeSigning::Terms(BridgeOrderTerms::Across(terms)) => {
                Ok(PublicSwapDelivery::Signed {
                    delivery,
                    message,
                    terms,
                })
            }
            BridgeSigning::Terms(BridgeOrderTerms::NearIntents(_)) => {
                Err(eyre!("the bridge quote's terms aren't Across's"))
            }
        }
    }

    /// Across's fee quote for `request`, the deposit of a swap paid from a Public account
    /// with its handler message. `order` words the refusal of a fill that fails in Across's
    /// simulation, and of an amount Across won't bridge with the message's gas.
    async fn public_swap_bridge_quote(
        &self,
        across: &AcrossClient,
        request: &AcrossMessageFeeRequest,
        order: bool,
    ) -> Result<AcrossFeeQuote> {
        trace_step(
            "public_swap_bridge_quote",
            self.while_active(async {
                across
                    .suggested_fees_with_message(request)
                    .await
                    .map_err(|error| {
                        let unplaced = if order {
                            "The order wasn't placed."
                        } else {
                            "The deposit wasn't made."
                        };
                        match error {
                            BridgeApiError::FillSimulationFailed => eyre!(
                                "Across can't deliver this swap now: the shield on the destination network fails in its simulation. {unplaced}"
                            ),
                            BridgeApiError::AmountTooLow => eyre!(
                                "Across won't bridge this amount: the shield's gas on the destination network is too large a share of it. {unplaced} Try a larger amount."
                            ),
                            error => error.into(),
                        }
                    })
            }),
        )
        .await
    }
}
