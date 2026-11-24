#[allow(clippy::wildcard_imports)]
use super::*;

/// Submit one public action with an already-derived signer.  Governance workflows use this
/// narrow entry point to keep one signer session and command channel across ordered calls while
/// allowing every call to run the normal RPC nonce, gas, signing, and confirmation machinery.
pub(crate) async fn submit_public_action_step_with_signer(
    step: PublicActionProgressStep,
    operation_label: &str,
    revert_subject: &str,
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    intent: &PublicTransactionIntent,
    signer: &crate::public_wallet::signer::VaultedPublicSigner,
    advanced_authorization: Option<PublicAdvancedTransactionAuthorization>,
    reviewed_gas_limit: Option<u64>,
    dynamic_gas_preflight: bool,
    gas_fee: PublicActionGasFeeSelection,
    command_rx: &mut Option<PublicActionCommandReceiver>,
    event_tx: Option<&PublicActionSessionEventSender>,
    http: &HttpContext,
    client: &OperationHttpClient,
    transaction_tracking: Option<&crate::PublicTransactionTrackingContext>,
    activity: Option<(&ActivitySubmission, ActivityStep)>,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<crate::TxReceiptOutput> {
    validate_public_transaction_intent(intent)?;
    let chain = verified_public_chain_runtime_config(chain_id, effective_chain, http, None).await?;
    let from_address = signer.address();
    let authorized_gas_limit = public_action_authorized_gas_limit(
        chain_id,
        from_address,
        intent,
        advanced_authorization,
        dynamic_gas_preflight,
        gas_fee,
    )?;
    let pools = public_action_pools(&chain.rpc_route, http, client)?;
    let tx_req = public_send_transaction_request(chain_id, from_address, intent)?;
    let tx = submit_public_action_step_session(
        step,
        tx_req,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        public_action_gas_limit_strategy(intent, reviewed_gas_limit)?,
        signer,
        operation_label,
        &pools,
        chain.finality_depth,
        http,
        transaction_tracking,
        activity,
        chain_id,
        from_address,
        &chain.gas,
        None,
        authorized_gas_limit,
        None,
        gas_fee,
        PublicActionStepFeePolicy::Custom,
        None,
        command_rx,
        event_tx,
        None,
        PublicActionStepEnd::Receipt,
        progress,
    )
    .await?
    .into_receipt()?
    .receipt;
    if !tx.status {
        return Err(eyre!("{revert_subject} reverted ({})", tx.tx_hash));
    }
    Ok(tx)
}

pub(super) const fn activity_asset(asset: PublicAssetId) -> ActivityAsset {
    match asset {
        PublicAssetId::Native => ActivityAsset::Native,
        PublicAssetId::Erc20(token) => ActivityAsset::Token(token),
    }
}

pub(super) const fn activity_leg(
    asset: ActivityAsset,
    amount: U256,
    direction: ActivityDirection,
) -> ActivityLeg {
    ActivityLeg {
        asset,
        amount,
        direction,
    }
}

/// The journal entry of `intent` sent from `account_uuid`: a transfer is a public send to its
/// recipient, anything else a contract call to its target.
pub(super) fn public_transaction_activity(
    account_uuid: &str,
    chain_id: u64,
    intent: &PublicTransactionIntent,
) -> PublicActivity {
    use ActivityDirection::{In, Out};
    let (kind, legs, target) = match *intent {
        PublicTransactionIntent::Transfer {
            asset,
            amount,
            recipient,
        } => (
            ActivityKind::PublicSend,
            vec![activity_leg(activity_asset(asset), amount, Out)],
            Some(recipient),
        ),
        PublicTransactionIntent::Raw { to, value, .. } => (
            ActivityKind::ContractCall,
            if value.is_zero() {
                Vec::new()
            } else {
                vec![activity_leg(ActivityAsset::Native, value, Out)]
            },
            to,
        ),
        PublicTransactionIntent::WrapNative { wrapped, amount } => (
            ActivityKind::ContractCall,
            vec![
                activity_leg(ActivityAsset::Native, amount, Out),
                activity_leg(ActivityAsset::Token(wrapped), amount, In),
            ],
            Some(wrapped),
        ),
        PublicTransactionIntent::UnwrapNative { wrapped, amount } => (
            ActivityKind::ContractCall,
            vec![
                activity_leg(ActivityAsset::Token(wrapped), amount, Out),
                activity_leg(ActivityAsset::Native, amount, In),
            ],
            Some(wrapped),
        ),
    };
    PublicActivity {
        account_uuid: account_uuid.to_owned(),
        chain_id,
        kind,
        legs,
        counterparty: target.map(ActivityCounterparty::Evm),
    }
}

pub(super) fn public_action_authorized_gas_limit(
    chain_id: u64,
    from: Address,
    intent: &PublicTransactionIntent,
    authorization: Option<PublicAdvancedTransactionAuthorization>,
    dynamic_gas_preflight: bool,
    gas_fee: PublicActionGasFeeSelection,
) -> Result<Option<u64>> {
    if dynamic_gas_preflight {
        if !matches!(intent, PublicTransactionIntent::Raw { .. }) || authorization.is_some() {
            return Err(eyre!(
                "dynamic public action gas preflight is restricted to an unauthorised raw workflow step"
            ));
        }
        return Ok(None);
    }
    public_send_authorized_gas_limit(chain_id, from, intent, authorization, gas_fee)
}

/// A transfer, wrap or unwrap signs within the gas limit its fee was reviewed at. A raw call is
/// bounded by its advanced authorization, or estimated freely as a dynamic workflow step.
fn public_action_gas_limit_strategy(
    intent: &PublicTransactionIntent,
    reviewed_gas_limit: Option<u64>,
) -> Result<PublicActionGasLimitStrategy> {
    let profile = PublicShieldTransactionProfile::Railoxide;
    match (intent, reviewed_gas_limit) {
        (PublicTransactionIntent::Raw { .. }, None) => Ok(profile.gas_limit_strategy()),
        (PublicTransactionIntent::Raw { .. }, Some(_)) => Err(eyre!(
            "a reviewed gas limit cannot be used for a raw transaction"
        )),
        (_, Some(reviewed_gas_limit)) => Ok(PublicActionGasLimitStrategy::Reviewed {
            reviewed_gas_limit,
            profile,
        }),
        (_, None) => Err(eyre!(
            "a transfer, wrap or unwrap requires the gas limit its fee was reviewed at"
        )),
    }
}
