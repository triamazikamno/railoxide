#[allow(clippy::wildcard_imports)]
use super::*;

/// A native shield from a stealth account of the selected private wallet, funded and sent by
/// the admitted payer `signer`, in the transaction profile it was prepared for.
pub(super) async fn submit_stealth_native_shield(
    mut request: PublicShieldRequest,
    preparation: &NativeShieldPreparation,
    signer: VaultedPublicSigner,
    chain: &PublicChainRuntimeConfig,
    pools: &PublicActionPools,
    recipient: &broadcaster_core::crypto::railgun::AddressData,
    activity: &ActivitySubmission,
    http: &HttpContext,
    client: &OperationHttpClient,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<ShieldSendOutput> {
    let mut command_rx = request.command_rx.take();
    let event_tx = request.event_tx.take();
    // Checked before the shield key or any transaction is signed.
    let contracts = stealth_native_shield_contracts(
        request.chain_id,
        &request.effective_chain,
        NativeShieldPayer::of(&signer, request.authorization.as_ref()),
    )?;
    let payer = signer.address();
    preparation.ensure_matches(
        &contracts,
        request.profile,
        payer,
        recipient,
        request.amount,
    )?;
    let owner = request.executor_owner.as_ref().ok_or_else(|| {
        eyre!("Wait for the private wallet's network to load, then shield again.")
    })?;
    let (Some(operation), Some(authorization)) =
        (request.native_operation, request.authorization.as_ref())
    else {
        return Err(eyre!(
            "native shielding needs a fresh preparation and review"
        ));
    };
    let shield_private_key = signer.derive_shield_private_key().await?;
    let shield_request = preparation.shield_request(&shield_private_key)?;
    let purpose_summary = format!(
        "Shield {} → {}",
        request
            .effective_chain
            .native_currency
            .format_amount(request.amount),
        request
            .view_session
            .receive_address()
            .wrap_err("derive selected private wallet receive address")?,
    );
    // An executor-derived payer holds its owner's activity, which issuing takes, so it is
    // released and admitted again to send.
    let readmit = matches!(signer, VaultedPublicSigner::Executor(..));
    let signer = if readmit {
        drop(signer);
        None
    } else {
        Some(signer)
    };
    let issued = owner
        .issue_native_shield(
            operation,
            &request.view_session,
            payer,
            authorization,
            contracts.wrapped_native,
            request.amount,
            shield_request,
            &purpose_summary,
            client,
        )
        .await?;
    let stealth_account = issued
        .transaction()
        .to
        .and_then(|to| to.to().copied())
        .ok_or_else(|| eyre!("the native shield has no stealth account"))?;
    let signer = match signer {
        Some(signer) => signer,
        None => {
            admitted_public_signer_authorized(
                &request.vault_store,
                &request.view_session,
                Some(authorization),
                &request.public_account_uuid,
                None,
                None,
                Some(owner),
                request.chain_id,
                client,
            )
            .await?
        }
    };
    if signer.address() != payer {
        return Err(eyre!("the Public account changed; review the shield again"));
    }
    let mut handoff = owner.native_shield_handoff(&issued)?;
    let handoff: &mut PublicActionHandoff<'_> = &mut handoff;
    let fee_policy = if request.profile == PublicShieldTransactionProfile::Railway
        && request.gas_fee_mode == PublicActionGasFeeMode::Auto
    {
        PublicActionStepFeePolicy::Captured
    } else {
        PublicActionStepFeePolicy::Custom
    };
    let receipt = signer
        .while_active(async {
            Ok(submit_public_action_step_session(
                PublicActionProgressStep::Shield,
                issued.transaction().clone(),
                request.profile,
                request.asset,
                PublicActionGasLimitStrategy::NativeShieldPrepared {
                    reviewed_gas_limit: preparation.gas_limit(),
                    profile: request.profile,
                },
                &signer,
                "public-shield",
                pools,
                chain.finality_depth,
                http,
                request.transaction_tracking.as_ref(),
                Some((activity, ActivityStep::Main)),
                request.chain_id,
                payer,
                &chain.gas,
                Some(PublicShieldFundingBudget {
                    gas_units: preparation.gas_limit(),
                    remaining_shield_gas_units: 0,
                    remaining_max_fee_per_gas: None,
                }),
                None,
                None,
                request.gas_fee,
                fee_policy,
                Some(request.authorized_fee_ceiling),
                &mut command_rx,
                event_tx.as_ref(),
                Some(handoff),
                PublicActionStepEnd::Evidence,
                &mut *progress,
            )
            .await?
            .into_receipt()?
            .receipt)
        })
        .await?;
    // Reconciliation below takes the owner's activity.
    drop(signer);
    // The session reported the revert, which returned the value to the payer.
    if !receipt.status {
        return Err(eyre!(
            "Native shield transaction reverted ({}); prepare and review a new shield to try again",
            receipt.tx_hash
        ));
    }
    // Native delivery needs a Shield event, not only the receipt's status.
    let logs = async {
        let hash = receipt
            .tx_hash
            .parse()
            .wrap_err("read the native shield receipt")?;
        let source =
            resolve_source_transaction_by_block(&pools.observe, receipt.block_number, hash).await?;
        Ok::<_, eyre::Report>(source.logs)
    }
    .await;
    if let Err(error) =
        ensure_native_shield_delivered(&contracts, stealth_account, &receipt.tx_hash, logs)
    {
        let error = eyre::Report::new(error);
        let message = report_chain_string(&error);
        progress(public_action_progress_update(
            PublicActionProgressStep::Shield,
            PublicActionProgressStatus::Error,
            Some(receipt.tx_hash.clone()),
            Some(message.clone()),
        ));
        emit_public_action_event(
            event_tx.as_ref(),
            PublicActionSessionEvent::StepFailed {
                step: PublicActionProgressStep::Shield,
                message,
            },
        );
        return Err(error);
    }
    progress(public_action_progress_update(
        PublicActionProgressStep::Shield,
        PublicActionProgressStatus::Done,
        Some(receipt.tx_hash.clone()),
        None,
    ));
    // A settled account needs no attention; a failed write only leaves its row visible.
    let _ = owner.set_hidden(operation, true);
    // The shield is proven; a failed read leaves the account to its next reconciliation.
    let _ = owner.reconcile_account(operation, client).await;
    Ok(ShieldSendOutput {
        wrap: None,
        approve: None,
        shield: receipt,
    })
}
