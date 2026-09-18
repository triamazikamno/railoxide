//! Immutable estimate inputs shared by native forms and gateway drafts.
use super::{
    Address, Arc, BroadcasterChoice, ChainUtxoState, DeliveryFormKind, DesktopNativeTopUpPlan,
    FeeHandlingMode, PublicBroadcasterCostEstimate, U256, UnshieldAsset, WalletRoot,
    effective_fee_handling_mode, format_send_amount_input, native_top_up_request_from_plan,
    parse_send_amount, parse_unshield_amount, select_public_broadcaster_with_policy_and_trust,
    send_public_broadcaster_estimate_input_error, unshield_public_broadcaster_estimate_input_error,
};
use wallet_ops::{
    DesktopSendPublicBroadcasterEstimateRequest, DesktopUnshieldPublicBroadcasterEstimateRequest,
    estimate_desktop_send_public_broadcaster_cost,
    estimate_desktop_unshield_public_broadcaster_cost,
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
    pub(in crate::root) fee_token: Address,
    pub(in crate::root) fee_mode: FeeHandlingMode,
    pub(in crate::root) allow_out_of_range: bool,
    pub(in crate::root) favorites_only: bool,
    pub(in crate::root) output: PrivateEstimateOutput,
}

impl PrivateEstimateInput {
    fn for_picker(mut self, recipient: String) -> Self {
        self.recipient = recipient;
        if self.amount.trim().is_empty() {
            // Use the same initial amount as native forms, only for this display estimate.
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
    ) -> Result<Option<PrivateEstimateRequest>, String> {
        let asset = &input.asset;
        let recipient = input.recipient.trim();
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
            return Err(error);
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
        let selection = Self::public_broadcaster_selection(&input.broadcaster);
        let trust_filter = self.public_broadcaster_trust_filter(input.favorites_only);
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
        let effective_chain = self.effective_chain_configs.get(&asset.chain_id).cloned();
        let anchor_cache = Some(Arc::clone(&self.public_broadcaster_anchor_cache));
        let session = Arc::clone(session);
        Ok(Some(match kind {
            DeliveryFormKind::Send => {
                PrivateEstimateRequest::Send(DesktopSendPublicBroadcasterEstimateRequest {
                    custom_fee_amount: input.custom_fee_amount,
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
                PrivateEstimateRequest::Unshield(DesktopUnshieldPublicBroadcasterEstimateRequest {
                    custom_fee_amount: input.custom_fee_amount,
                    approved_fee_amount: None,
                    executor: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    #[test]
    fn picker_estimates_allow_blank_amount_without_relaxing_transaction_validation() {
        for output in [
            PrivateEstimateOutput::Send,
            PrivateEstimateOutput::Unshield {
                unwrap: false,
                native_top_up: None,
            },
        ] {
            let validate = match output {
                PrivateEstimateOutput::Send => send_public_broadcaster_estimate_input_error,
                PrivateEstimateOutput::Unshield { .. } => {
                    unshield_public_broadcaster_estimate_input_error
                }
            };
            let mut input = PrivateEstimateInput {
                custom_fee_amount: None,
                asset: UnshieldAsset {
                    chain_id: 1,
                    token: Address::ZERO,
                    label: "TEST".into(),
                    decimals: Some(18),
                    total: U256::from(9_000_000_000_000_000_000_u64),
                    poi_verified_total: U256::from(7_000_000_000_000_000_000_u64),
                    max_batched: U256::from(5_446_683_954_612_232_014_u64),
                    icon_path: None,
                },
                recipient: String::new(),
                amount: " ".into(),
                broadcaster: BroadcasterChoice::Random,
                fee_token: Address::ZERO,
                fee_mode: FeeHandlingMode::DeductFromAmount,
                allow_out_of_range: false,
                favorites_only: false,
                output,
            };
            assert!(validate("", &input.amount, &input.asset).is_some());
            let picker = input.clone().for_picker(String::new());
            assert!(validate("", &picker.amount, &picker.asset).is_none());
            assert_eq!(
                parse_send_amount(&picker.amount, picker.asset.decimals).unwrap(),
                input.asset.max_batched
            );
            input.amount = "1.123456789012345678".into();
            let picker = input.clone().for_picker(String::new());
            assert_eq!(picker.amount, input.amount);
            input.amount = "invalid".into();
            let picker = input.for_picker(String::new());
            assert!(validate("", &picker.amount, &picker.asset).is_some());
        }
    }
}
