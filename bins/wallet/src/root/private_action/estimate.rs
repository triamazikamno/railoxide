//! Immutable estimate inputs shared by native forms and gateway drafts.
use super::{
    Address, Arc, BroadcasterChoice, ChainUtxoState, DeliveryFormKind, DesktopNativeTopUpPlan,
    FeeHandlingMode, PublicBroadcasterCostEstimate, U256, UnshieldAsset, WalletRoot,
    effective_fee_handling_mode, format_send_amount_input, native_top_up_request_from_plan,
    parse_send_amount, parse_unshield_amount, select_public_broadcaster_with_policy_and_trust,
    send_public_broadcaster_estimate_input_error, unshield_public_broadcaster_estimate_input_error,
};
use wallet_ops::{
    BroadcasterFeePolicy, DesktopSendPublicBroadcasterEstimateRequest,
    DesktopUnshieldPublicBroadcasterEstimateRequest, PublicBroadcasterCandidate,
    PublicBroadcasterInputPin, PublicBroadcasterSelection, PublicBroadcasterTrustFilter,
    estimate_desktop_send_public_broadcaster_cost,
    estimate_desktop_unshield_public_broadcaster_cost, vault::BroadcasterPreferenceEntry,
};

#[derive(Clone)]
pub(in crate::root) enum PrivateEstimateOutput {
    Send,
    Unshield {
        unwrap: bool,
        native_top_up: Option<DesktopNativeTopUpPlan>,
    },
}

#[derive(Clone)]
pub(in crate::root) struct PrivateEstimateInput {
    pub(in crate::root) custom_fee_amount: Option<U256>,
    pub(in crate::root) asset: UnshieldAsset,
    pub(in crate::root) recipient: String,
    pub(in crate::root) amount: String,
    pub(in crate::root) broadcaster: BroadcasterChoice,
    pub(in crate::root) shown_broadcaster: Option<String>,
    pub(in crate::root) refused_broadcasters: Vec<String>,
    pub(in crate::root) input_pin: Option<PublicBroadcasterInputPin>,
    /// The form's executor operation, which a same-action submission reuses with its notes.
    pub(in crate::root) executor_operation: Option<super::executor::ExecutorUnshieldOperation>,
    pub(in crate::root) fee_token: Address,
    pub(in crate::root) fee_mode: FeeHandlingMode,
    pub(in crate::root) allow_out_of_range: bool,
    pub(in crate::root) favorites_only: bool,
    pub(in crate::root) output: PrivateEstimateOutput,
}

impl PrivateEstimateInput {
    fn for_picker(mut self, recipient: String) -> Self {
        self.recipient = recipient;
        // Comparison estimates must work even if the form's chosen broadcaster is unavailable.
        self.broadcaster = BroadcasterChoice::Random;
        self.shown_broadcaster = None;
        self.refused_broadcasters.clear();
        // They compare broadcasters' rates; the form's own estimate keeps its note pin.
        self.input_pin = None;
        if self.amount.trim().is_empty() {
            // Blank forms still need a representative amount for this display estimate.
            self.amount = format_send_amount_input(self.asset.max_batched, self.asset.decimals);
        }
        self
    }
}

pub(in crate::root) enum PrivateEstimateRequest {
    Send(DesktopSendPublicBroadcasterEstimateRequest),
    Unshield(DesktopUnshieldPublicBroadcasterEstimateRequest),
}

impl PrivateEstimateRequest {
    pub(in crate::root) async fn estimate(
        self,
        http: &wallet_ops::HttpContext,
    ) -> eyre::Result<PublicBroadcasterCostEstimate> {
        match self {
            Self::Send(request) => {
                estimate_desktop_send_public_broadcaster_cost(request, http).await
            }
            Self::Unshield(request) => {
                estimate_desktop_unshield_public_broadcaster_cost(request, http).await
            }
        }
    }
}

impl WalletRoot {
    /// Picker gas-shape estimates do not require completed recipient or amount fields.
    pub(in crate::root) fn prepare_private_picker_estimate(
        &self,
        input: PrivateEstimateInput,
    ) -> Option<PrivateEstimateRequest> {
        let recipient = match &input.output {
            PrivateEstimateOutput::Send => self.view_session.as_ref()?.receive_address().ok()?,
            PrivateEstimateOutput::Unshield { .. } => Address::ZERO.to_string(),
        };
        let input = input.for_picker(recipient);
        self.prepare_private_broadcaster_estimate(&input)
            .ok()
            .flatten()
    }

    /// Missing inputs, sync, and unavailable candidates do not start an estimate.
    pub(in crate::root) fn prepare_private_broadcaster_estimate(
        &self,
        input: &PrivateEstimateInput,
    ) -> eyre::Result<Option<PrivateEstimateRequest>> {
        let asset = &input.asset;
        let recipient = input.recipient.trim();
        // A blank amount is the untouched form, not an input error.
        if input.amount.trim().is_empty() {
            return Ok(None);
        }
        let (kind, unwrap, native_top_up) = match &input.output {
            PrivateEstimateOutput::Send => (DeliveryFormKind::Send, false, None),
            PrivateEstimateOutput::Unshield {
                unwrap,
                native_top_up,
            } => (
                DeliveryFormKind::Unshield,
                *unwrap,
                native_top_up_request_from_plan(native_top_up.as_ref()),
            ),
        };
        let error = match kind {
            DeliveryFormKind::Send => {
                send_public_broadcaster_estimate_input_error(recipient, &input.amount, asset)
            }
            DeliveryFormKind::Unshield => {
                unshield_public_broadcaster_estimate_input_error(recipient, &input.amount, asset)
            }
        };
        if let Some(error) = error {
            return Err(eyre::eyre!(error));
        }
        if recipient.is_empty() {
            return Ok(None);
        }
        let amount = match kind {
            DeliveryFormKind::Send => parse_send_amount(&input.amount, asset.decimals),
            DeliveryFormKind::Unshield => parse_unshield_amount(&input.amount, asset.decimals),
        };
        let Ok(amount) = amount else { return Ok(None) };
        let Some(ChainUtxoState::Ready { session, .. }) = self.chain_states.get(&asset.chain_id)
        else {
            return Ok(None);
        };
        let fee_mode =
            effective_fee_handling_mode(kind, asset.token, input.fee_token, input.fee_mode);
        let fee_policy = self.public_broadcaster_fee_policy(input.allow_out_of_range);
        let candidates = self.current_public_broadcaster_candidates(
            asset.chain_id,
            input.fee_token,
            unwrap,
            native_top_up.is_some(),
            input.favorites_only,
            fee_policy,
        );
        let mut trust_filter = self.public_broadcaster_trust_filter(input.favorites_only);
        if input.broadcaster == BroadcasterChoice::Random {
            trust_filter = exclude_refused_broadcasters(
                trust_filter,
                &input.refused_broadcasters,
                &candidates,
                fee_policy,
            );
        }
        let selection = estimate_broadcaster_selection(
            &input.broadcaster,
            input.shown_broadcaster.as_deref(),
            &candidates,
            fee_policy,
            &trust_filter,
        );
        if select_public_broadcaster_with_policy_and_trust(
            &candidates,
            &selection,
            fee_policy,
            &trust_filter,
        )
        .is_err()
        {
            return Ok(None);
        }
        let fee_rows = self.monitor_fee_rows();
        let effective_chain = self
            .effective_chain_configs
            .railgun(asset.chain_id)
            .cloned()?;
        let anchor_cache = Some(Arc::clone(&self.public_broadcaster_anchor_cache));
        let session = Arc::clone(session);
        Ok(Some(match kind {
            DeliveryFormKind::Send => {
                PrivateEstimateRequest::Send(DesktopSendPublicBroadcasterEstimateRequest {
                    custom_fee_amount: input.custom_fee_amount,
                    input_pin: input.input_pin.clone(),
                    chain_id: asset.chain_id,
                    effective_chain,
                    session,
                    token: asset.token,
                    fee_token: input.fee_token,
                    amount,
                    recipient: recipient.to_owned(),
                    fee_rows,
                    selection,
                    fee_mode,
                    fee_policy,
                    trust_filter,
                    anchor_cache,
                })
            }
            DeliveryFormKind::Unshield => {
                let Ok(recipient) = recipient.parse::<Address>() else {
                    return Ok(None);
                };
                let reused_executor_operation = match &input.output {
                    PrivateEstimateOutput::Unshield {
                        unwrap,
                        native_top_up,
                    } => input.executor_operation.as_ref().and_then(|operation| {
                        operation.reused_for((
                            asset.chain_id,
                            asset.token,
                            recipient,
                            amount,
                            *unwrap,
                            native_top_up.as_ref(),
                            fee_mode,
                        ))
                    }),
                    PrivateEstimateOutput::Send => None,
                };
                PrivateEstimateRequest::Unshield(DesktopUnshieldPublicBroadcasterEstimateRequest {
                    custom_fee_amount: input.custom_fee_amount,
                    input_pin: input.input_pin.clone(),
                    executor: None,
                    reused_executor_operation,
                    chain_id: asset.chain_id,
                    effective_chain,
                    session,
                    token: asset.token,
                    fee_token: input.fee_token,
                    amount,
                    recipient,
                    unwrap,
                    native_top_up,
                    fee_rows,
                    selection,
                    fee_mode,
                    fee_policy,
                    trust_filter,
                    anchor_cache,
                })
            }
        }))
    }
}

/// Random keeps the broadcaster the estimate card showed while it stays eligible, so only the
/// user's re-roll picks a new one.
fn estimate_broadcaster_selection(
    choice: &BroadcasterChoice,
    shown: Option<&str>,
    candidates: &[PublicBroadcasterCandidate],
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
) -> PublicBroadcasterSelection {
    if let (BroadcasterChoice::Random, Some(shown)) = (choice, shown) {
        let selection = PublicBroadcasterSelection::Specific {
            railgun_address: shown.to_owned(),
        };
        if select_public_broadcaster_with_policy_and_trust(
            candidates,
            &selection,
            policy,
            trust_filter,
        )
        .is_ok()
        {
            return selection;
        }
    }
    WalletRoot::public_broadcaster_selection(choice)
}

/// Random skips broadcasters that refused this form, unless no other candidate remains.
fn exclude_refused_broadcasters(
    trust_filter: PublicBroadcasterTrustFilter,
    refused: &[String],
    candidates: &[PublicBroadcasterCandidate],
    policy: BroadcasterFeePolicy,
) -> PublicBroadcasterTrustFilter {
    if refused.is_empty() {
        return trust_filter;
    }
    let mut excluding = trust_filter.clone();
    excluding
        .preferences
        .banned
        .extend(refused.iter().map(|address| BroadcasterPreferenceEntry {
            address: address.clone(),
        }));
    if select_public_broadcaster_with_policy_and_trust(
        candidates,
        &PublicBroadcasterSelection::Random,
        policy,
        &excluding,
    )
    .is_ok()
    {
        excluding
    } else {
        trust_filter
    }
}

#[cfg(test)]
mod tests;
