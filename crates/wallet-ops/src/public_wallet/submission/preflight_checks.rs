#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) async fn public_action_preflight(
    provider_handle: ProviderHandle,
    chain_id: u64,
    from: Address,
    base_tx_req: TransactionRequest,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
    gas: &EffectiveChainGasSettings,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
    stage: &mut &'static str,
    funding_budget: Option<PublicShieldFundingBudget>,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    let provider = &provider_handle.provider;
    let resolved = resolve_public_action_gas_fee(chain_id, profile, asset, gas_fee, quote)?;
    if railway_auto {
        *stage = "fee_authorization";
        let within_ceiling = railway_auto_fee_within_authorized_ceiling(
            chain_id,
            asset,
            authorized_fee_ceiling
                .ok_or_else(|| eyre!("missing Railway fee authorization ceiling"))?,
            &resolved,
        );
        if !within_ceiling {
            return Err(PublicActionPreflightError::FeeAuthorizationRequired {
                max_fee_per_gas: resolved.max_fee_per_gas,
                max_priority_fee_per_gas: resolved.max_priority_fee_per_gas,
                message:
                    "Network fees changed while approval confirmed. Review the updated fee to continue."
                        .to_string(),
            });
        }
    }
    let remaining_reserve = funding_budget.map_or(U256::ZERO, |budget| {
        budget.remaining_reserve(resolved.max_fee_per_gas)
    });
    let funding_balance = if let Some(budget) = funding_budget {
        *stage = "balance";
        let balance = match rpc_reads {
            Some(reads) => reads
                .get_balance(provider_handle.url.clone().into(), chain_id, from)
                .await
                .map_err(eyre::Report::new),
            None => provider.get_balance(from).await.map_err(eyre::Report::new),
        }
        .wrap_err("fetch public Shield native balance")?;
        *stage = "shield_funding_validation";
        // Replacements already have a signed limit; use the model only before a limit is known.
        let funding_gas_limit = authorized_gas_limit
            .or(gas_limit)
            .unwrap_or(budget.gas_units);
        let estimated_cost = public_action_native_exposure(
            base_tx_req.value.unwrap_or_default(),
            funding_gas_limit,
            resolved.max_fee_per_gas,
        )
        .saturating_add(remaining_reserve);
        if balance < estimated_cost {
            return Err(PublicActionPreflightError::InsufficientShieldFunding(
                eyre!(
                    "insufficient native balance for estimated Shield fees and transaction value; include fees for any required approval and the remaining Shield"
                ),
            ));
        }
        Some(balance)
    } else {
        None
    };
    *stage = "nonce";
    let nonce = if let Some(nonce) = nonce {
        nonce
    } else {
        match rpc_reads {
            Some(reads) => reads
                .get_transaction_count(provider_handle.url.clone().into(), chain_id, from)
                .await
                .map_err(eyre::Report::new),
            None => provider
                .get_transaction_count(from)
                .await
                .map_err(eyre::Report::new),
        }
        .wrap_err("fetch public action nonce")?
    };
    let legacy_envelope = crate::public_wallet::gas::public_action_uses_legacy_envelope(
        chain_id, profile, asset, quote,
    );
    ensure_native_shield_envelope(gas_limit_strategy, legacy_envelope)?;
    let tx_req = if legacy_envelope {
        public_action_legacy_transaction_request(
            base_tx_req,
            chain_id,
            from,
            resolved.max_fee_per_gas,
            nonce,
        )
    } else {
        public_action_eip1559_transaction_request(
            base_tx_req,
            chain_id,
            from,
            resolved.max_fee_per_gas,
            resolved.max_priority_fee_per_gas,
            nonce,
        )
    };
    let max_fee_per_gas = tx_req
        .max_fee_per_gas
        .or(tx_req.gas_price)
        .unwrap_or(resolved.max_fee_per_gas);
    let max_priority_fee_per_gas = tx_req.max_priority_fee_per_gas.unwrap_or_else(|| {
        if tx_req.gas_price.is_some() {
            0
        } else {
            resolved.max_priority_fee_per_gas
        }
    });
    *stage = "estimate";
    let gas_limit = if let Some(authorized_gas_limit) = authorized_gas_limit {
        let estimated_gas =
            public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                .await
                .wrap_err("re-estimate authorized advanced public transaction gas")?;
        *stage = "gas_limit_authorization";
        ensure_advanced_gas_estimate_authorized(estimated_gas, authorized_gas_limit)?;
        authorized_gas_limit
    } else if let Some(gas_limit) = gas_limit {
        gas_limit
    } else {
        match gas_limit_strategy {
            PublicActionGasLimitStrategy::NativeShieldPrepared {
                reviewed_gas_limit,
                profile,
            } => {
                // The complete type-4 request, sender, value and authorization included.
                let raw_gas_limit =
                    public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                        .await
                        .wrap_err("estimate native shield gas")?;
                *stage = "gas_limit_authorization";
                native_shield_signing_gas_limit(
                    profile,
                    gas.gas_limit_buffer,
                    raw_gas_limit,
                    reviewed_gas_limit,
                )?
            }
            PublicActionGasLimitStrategy::Reviewed {
                reviewed_gas_limit,
                profile,
            } => {
                let raw_gas_limit =
                    public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                        .await
                        .wrap_err("estimate public action gas")?;
                *stage = "gas_limit_authorization";
                reviewed_signing_gas_limit(profile, raw_gas_limit, reviewed_gas_limit)?
            }
            PublicActionGasLimitStrategy::ChainBuffer => {
                public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                    .await
                    .wrap_err("estimate public action gas")?
                    .saturating_add(gas.gas_limit_buffer)
            }
            PublicActionGasLimitStrategy::RailwayEstimate120 => {
                crate::public_wallet::gas::railway_gas_limit(
                    public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                        .await
                        .wrap_err("estimate public action gas")?,
                )
            }
        }
    };
    let estimated_native_gas_cost =
        public_action_native_exposure(tx_req.value.unwrap_or_default(), gas_limit, max_fee_per_gas);
    *stage = "balance";
    let live_native_balance = if let Some(balance) = funding_balance {
        balance
    } else {
        match rpc_reads {
            Some(reads) => reads
                .get_balance(provider_handle.url.clone().into(), chain_id, from)
                .await
                .map_err(eyre::Report::new),
            None => provider.get_balance(from).await.map_err(eyre::Report::new),
        }
        .wrap_err("fetch public action native balance")?
    };
    *stage = "native_balance_validation";
    let required_native_balance = estimated_native_gas_cost.saturating_add(remaining_reserve);
    if live_native_balance < required_native_balance {
        let action = if authorized_gas_limit.is_some() {
            "advanced public transaction"
        } else {
            "public action"
        };
        let error = eyre!(
            "insufficient native balance for {action}: live balance {live_native_balance}, required value plus maximum gas cost {required_native_balance}"
        );
        return Err(if funding_budget.is_some() {
            PublicActionPreflightError::InsufficientShieldFunding(error)
        } else {
            PublicActionPreflightError::Other(error)
        });
    }
    Ok(PublicActionPreflight {
        tx_req: tx_req.with_gas_limit(gas_limit),
        nonce,
        gas_limit,
        rpc_gas_price: resolved.rpc_gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
    })
}

async fn public_action_estimate_gas(
    provider: &ProviderHandle,
    chain_id: u64,
    tx: &TransactionRequest,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
) -> Result<u64> {
    match rpc_reads {
        Some(reads) => reads
            .estimate_gas(provider.url.clone().into(), chain_id, tx)
            .await
            .map_err(eyre::Report::new),
        None => provider
            .provider
            .estimate_gas(tx.clone())
            .await
            .map_err(eyre::Report::new),
    }
}
