//! Private evaluation shares the draft book, never an editable native form.
use super::*;
use crate::root::{
    ChainUtxoState, DeliveryFormKind, UnshieldAsset,
    broadcaster_picker::BroadcasterChoice,
    is_effective_wrapped_native_token,
    private_action::{
        PrivateEstimateInput, PrivateEstimateOutput, private_action_metric_display_amount,
    },
};
use wallet_ops::{
    FeeHandlingMode, ListUtxosOutput, PublicBroadcasterCostEstimate,
    gateway::{
        GatewayBroadcasterChoice, GatewayPrivateAssetChoice, GatewayPrivateDelivery,
        GatewayPrivateDisplayRow, GatewayPrivateDraftEstimate, GatewayPrivateDraftInput,
        GatewayPrivateDraftKind, GatewayPrivateDraftOptions, GatewayPrivateDraftPicker,
        GatewayPrivateFeeMode, GatewayPrivateFunding, GatewayPrivateGasFee,
        GatewayPrivateIncentive, GatewayPrivateSelfBroadcastInput, GatewayPrivateTopUpOption,
    },
};

mod draft_options;
mod draft_refresh;
mod draft_submission;

pub(super) struct PrivateDraftPicker {
    pub(super) view_id: String,
    lifetime: Arc<()>,
    pub(super) query: String,
    estimate: Option<PreparedPrivateDraft>,
    estimated_at: Option<Instant>,
    job: Option<tokio::task::AbortHandle>,
    retry: crate::root::broadcaster_picker::BroadcasterPickerFeeEstimateRetryState,
    retry_at: Option<Instant>,
}

impl PrivateDraftPicker {
    pub(super) fn new(view_id: String, query: String) -> Self {
        Self {
            view_id,
            query,
            lifetime: Arc::new(()),
            estimate: None,
            estimated_at: None,
            job: None,
            retry: crate::root::broadcaster_picker::BroadcasterPickerFeeEstimateRetryState::default(
            ),
            retry_at: None,
        }
    }
}

impl Drop for PrivateDraftPicker {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            job.abort();
        }
    }
}

#[derive(Clone)]
pub(super) struct PreparedPrivateDraft {
    pub(super) input: PrivateEstimateInput,
    pub(super) estimate: PublicBroadcasterCostEstimate,
    snapshot: Arc<ListUtxosOutput>,
}

impl PreparedPrivateDraft {
    pub(super) fn is_current(&self, root: &WalletRoot, raw: &GatewayDraftPayload) -> bool {
        let GatewayDraftPayload::Private(raw) = raw else {
            return false;
        };
        if !matches!(root.chain_states.get(&self.input.asset.chain_id), Some(ChainUtxoState::Ready { snapshot, .. }) if Arc::ptr_eq(snapshot, &self.snapshot))
            || self.estimate.entered_amount > self.estimate.max_entered_amount
            || root.last_evm_chain != self.input.asset.chain_id
            || root.private_draft_recipient(raw).is_err()
            || root.private_draft_asset(raw).is_err()
        {
            return false;
        }
        let (unwrap, top_up) = match &self.input.output {
            PrivateEstimateOutput::Send => (false, false),
            PrivateEstimateOutput::Unshield {
                unwrap,
                native_top_up,
            } => (*unwrap, native_top_up.is_some()),
        };
        let policy = root.public_broadcaster_fee_policy(self.input.allow_out_of_range);
        let candidates = root.current_public_broadcaster_candidates(
            self.input.asset.chain_id,
            self.input.fee_token,
            unwrap,
            top_up,
            self.input.favorites_only,
            policy,
        );
        let selection = wallet_ops::PublicBroadcasterSelection::Specific {
            railgun_address: self.estimate.broadcaster.railgun_address.clone(),
        };
        wallet_ops::select_public_broadcaster_with_policy_and_trust(
            &candidates,
            &selection,
            policy,
            &root.public_broadcaster_trust_filter(self.input.favorites_only),
        )
        .is_ok_and(|candidate| {
            candidate.fees_id == self.estimate.broadcaster.fees_id
                && candidate.fee == self.estimate.broadcaster.fee
                && candidate.fee_expiration == self.estimate.broadcaster.fee_expiration
        })
    }
}

const fn gateway_private_sponsored(input: &GatewayPrivateDraftInput) -> bool {
    matches!(
        input.delivery,
        GatewayPrivateDelivery::SelfBroadcast {
            delivery: GatewayPrivateSelfBroadcastInput::SelfBroadcast {
                funding: GatewayPrivateFunding::Sponsorship { .. },
                ..
            },
        }
    )
}

pub(super) fn valid_private_input_size(input: &GatewayPrivateDraftInput) -> bool {
    let delivery_valid = match &input.delivery {
        GatewayPrivateDelivery::Broadcaster(delivery) => {
            delivery.fee_token.len() <= 128
                && match &delivery.broadcaster {
                    GatewayBroadcasterChoice::Random => true,
                    GatewayBroadcasterChoice::Specific { id } => id.len() <= 1024,
                }
        }
        GatewayPrivateDelivery::SelfBroadcast {
            delivery:
                GatewayPrivateSelfBroadcastInput::SelfBroadcast {
                    signer,
                    funding,
                    fee,
                },
        } => {
            signer.as_ref().is_none_or(|id| id.len() <= 128)
                && match funding {
                    GatewayPrivateFunding::Sponsorship {
                        incentive: GatewayPrivateIncentive::Custom { percent },
                    } => percent.len() <= 100,
                    _ => true,
                }
                && match fee {
                    GatewayPrivateGasFee::Auto {} => true,
                    GatewayPrivateGasFee::Custom {
                        max_fee_gwei,
                        priority_fee_gwei,
                    } => max_fee_gwei.len() <= 100 && priority_fee_gwei.len() <= 100,
                }
        }
    };
    delivery_valid
        && input.wallet.len() <= 128
        && input.asset.len() <= 128
        && input.amount.len() <= 100
        && input.recipient.len() <= 1024
        && input
            .address_book_entry
            .as_ref()
            .is_none_or(|id| id.len() <= 128)
}

const fn kind(input: &GatewayPrivateDraftInput) -> DeliveryFormKind {
    match input.kind {
        GatewayPrivateDraftKind::PrivateSend => DeliveryFormKind::Send,
        GatewayPrivateDraftKind::Unshield => DeliveryFormKind::Unshield,
    }
}

pub(super) fn resolve_private_draft_contact(
    store: &DesktopVaultStore,
    wallet: &DesktopViewSession,
    input: &GatewayPrivateDraftInput,
) -> Result<String, String> {
    let Some(id) = &input.address_book_entry else {
        return Ok(input.recipient.clone());
    };
    let address = match input.kind {
        GatewayPrivateDraftKind::PrivateSend => store
            .list_private_address_book_entries_for_session(wallet)
            .map_err(|_| "Address book is unavailable")?
            .into_iter()
            .find(|entry| entry.entry_uuid == *id)
            .map(|entry| entry.address),
        GatewayPrivateDraftKind::Unshield => store
            .list_public_address_book_entries_for_session(wallet)
            .map_err(|_| "Address book is unavailable")?
            .into_iter()
            .find(|entry| entry.entry_uuid == *id)
            .map(|entry| entry.address.to_checksum(None)),
    }
    .ok_or("Address book entry is unavailable. Choose another recipient.")?;
    if address != input.recipient {
        return Err("Address book entry changed. Choose the recipient again.".into());
    }
    Ok(address)
}

fn private_estimate_display(
    root: &WalletRoot,
    input: &PrivateEstimateInput,
    estimate: &PublicBroadcasterCostEstimate,
) -> GatewayPrivateDraftEstimate {
    use crate::root::public_broadcaster_cost::PublicBroadcasterCostDisplay;
    let display = PublicBroadcasterCostDisplay::from_estimate(
        &input.asset,
        estimate,
        crate::root::broadcaster_candidate_anchor_rate(&estimate.broadcaster),
        Some(&root.effective_token_registry),
    );
    let mut projection = GatewayPrivateDraftEstimate::default();
    projection.amount = format_send_amount_input(estimate.entered_amount, input.asset.decimals);
    projection.amount_label = display.action_amount(estimate.entered_amount);
    projection.max_amount = Some(format_send_amount_input(
        estimate.max_entered_amount,
        input.asset.decimals,
    ));
    projection.max_amount_label = Some(private_action_metric_display_amount(
        estimate.max_entered_amount,
        input.asset.decimals,
    ));
    projection.recipient = Some(input.recipient.clone());
    projection.broadcaster =
        crate::root::broadcaster_picker::broadcaster_candidate_label(&estimate.broadcaster);
    projection.shape =
        crate::root::public_broadcaster_cost::public_broadcaster_estimate_shape(estimate);
    projection.network_gas = display.gas_value();
    let to_wire = |row: ui::private_action::DisplayRow| {
        let mut wire = GatewayPrivateDisplayRow::default();
        wire.label = row.label;
        wire.value = row.value;
        wire.suffix = row.suffix;
        wire
    };
    projection.outcome = display
        .outcome_rows(&root.public_broadcaster_anchor_cache)
        .into_iter()
        .map(to_wire)
        .collect();
    projection.fee_breakdown = display
        .fee_rows(&root.public_broadcaster_anchor_cache)
        .into_iter()
        .map(to_wire)
        .collect();
    projection.transaction_fee = display.fee_amount_with_usd(&root.public_broadcaster_anchor_cache);
    projection
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn unwrap_without_an_executor_route_is_refused_before_authorization(
        cx: &mut gpui::TestAppContext,
    ) {
        let path = std::env::temp_dir().join(format!(
            "gateway-private-executor-route-{:032x}",
            rand::random::<u128>()
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entered = runtime.enter();
        cx.update(gpui_component::init);
        let mut root = None;
        let (host, cx) = cx.add_window_view(|window, cx| {
            let wallet =
                crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
            root = Some(wallet.clone());
            gpui_component::Root::new(wallet, window, cx)
        });
        let root = root.unwrap();
        cx.update(|window, cx| {
            root.update(cx, |root, cx| {
                // The fixture has no ready session, so no executor owner.
                let asset = UnshieldAsset {
                    chain_id: 1,
                    token: root
                        .effective_chain_configs
                        .get(1)
                        .unwrap()
                        .wrapped_native_token
                        .unwrap(),
                    label: "WETH".into(),
                    decimals: Some(18),
                    total: U256::from(1_000),
                    poi_verified_total: U256::from(1_000),
                    max_batched: U256::from(1_000),
                    icon_path: None,
                };
                let key = crate::root::UnshieldAssetKey::from_asset(&asset);
                root.initialize_unshield_form(asset.clone(), window, cx);
                root.set_unshield_unwrap(key, true, cx);
                assert!(!root.unshield_forms[&key].unwrap);

                let mut input = GatewayPrivateDraftInput::default();
                input.chain_id = 1;
                input.kind = GatewayPrivateDraftKind::Unshield;
                input.unwrap = true;
                let recipient = Address::repeat_byte(1).to_string();
                let output = |root: &WalletRoot, input: &GatewayPrivateDraftInput| {
                    root.private_draft_output(
                        input,
                        &asset,
                        &recipient,
                        FeeHandlingMode::DeductFromAmount,
                    )
                };
                assert_eq!(
                    output(root, &input).err(),
                    Some(wallet_ops::ExecutorRouteUnavailable::NoExecutorOwner.to_string())
                );
                // Private sponsorship keeps its legacy RelayAdapt route.
                input.delivery = GatewayPrivateDelivery::SelfBroadcast {
                    delivery: GatewayPrivateSelfBroadcastInput::SelfBroadcast {
                        signer: None,
                        funding: GatewayPrivateFunding::Sponsorship {
                            incentive: GatewayPrivateIncentive::Standard {},
                        },
                        fee: GatewayPrivateGasFee::Auto {},
                    },
                };
                assert!(matches!(
                    output(root, &input),
                    Ok(PrivateEstimateOutput::Unshield {
                        unwrap: true,
                        native_top_up: None
                    })
                ));
            });
        });
        cx.update(|window, _| window.remove_window());
        drop(host);
        drop(root);
        cx.run_until_parked();
        drop(entered);
        drop(runtime);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[tokio::test]
    async fn retiring_a_picker_aborts_its_pending_estimate() {
        let picker_job = tokio::spawn(std::future::pending::<()>());
        let mut picker = PrivateDraftPicker::new("view".into(), String::new());
        picker.job = Some(picker_job.abort_handle());
        drop(picker);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), picker_job)
                .await
                .expect("bounded cancellation")
                .expect_err("closed view cancels its work")
                .is_cancelled()
        );
    }
}
