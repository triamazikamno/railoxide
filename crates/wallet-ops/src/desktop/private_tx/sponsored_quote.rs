use super::*;

#[derive(Clone, Copy)]
pub(super) enum SponsoredPrivateIntent {
    Send {
        token: Address,
        amount: U256,
        recipient: AddressData,
    },
    Unshield {
        token: Address,
        amount: U256,
        recipient: Address,
        unwrap: bool,
    },
}

impl SponsoredPrivateIntent {
    pub(super) const fn action(self) -> SponsoredActionKind {
        match self {
            Self::Send { .. } => SponsoredActionKind::Send,
            Self::Unshield { .. } => SponsoredActionKind::Unshield,
        }
    }

    pub(super) fn mixed_request(
        self,
        authorization: &SponsoredAuthorization,
        native_top_up: Option<&DesktopNativeTopUpPlan>,
        verify_proof: bool,
        rebuild: Option<MixedPrivateActionRebuildConstraint>,
    ) -> std::result::Result<MixedPrivateActionRequest, SponsorshipError> {
        let private_sends = match self {
            Self::Send {
                token,
                amount,
                recipient,
            } => vec![MixedPrivateSend {
                token_address: token,
                amount,
                recipient,
                role: MixedPrivateSendRole::Primary,
            }],
            Self::Unshield { .. } => Vec::new(),
        };
        let mut public_unshields = Vec::new();
        let mut calls = if authorization.builder_payment.is_zero() {
            Vec::new()
        } else {
            vec![
                CompositeRelayAction::UnwrapBase {
                    amount: authorization.builder_payment,
                },
                CompositeRelayAction::Transfer {
                    token: CompositeRelayActionToken::BaseNative,
                    recipient: authorization.coinbase_payer,
                    amount: authorization.builder_payment,
                },
            ]
        };
        if let Self::Unshield {
            token,
            amount,
            recipient,
            unwrap,
        } = self
        {
            if let Some(native_top_up) = native_top_up {
                let top_up = native_top_up_composite_unshield_request(
                    token,
                    amount,
                    recipient,
                    unwrap,
                    false,
                    native_top_up,
                )
                .expect("validated sponsored native top-up");
                public_unshields.extend(top_up.legs);
                calls.extend(
                    top_up
                        .relay_actions
                        .expect("native top-up uses RelayAdapt")
                        .calls,
                );
            } else {
                let route_wrapped_native_output = !authorization.builder_payment.is_zero()
                    && !unwrap
                    && token == authorization.wrapped_native_token;
                public_unshields.push(CompositeUnshieldLeg {
                    token_address: token,
                    amount,
                    recipient: if unwrap || route_wrapped_native_output {
                        CompositeUnshieldRecipient::RelayAdapt
                    } else {
                        CompositeUnshieldRecipient::Public(recipient)
                    },
                    role: CompositeUnshieldLegRole::Primary,
                });
                let recipient_amount = native_top_up_net_after_protocol_fee(amount);
                if unwrap {
                    calls.extend([
                        CompositeRelayAction::UnwrapBase {
                            amount: recipient_amount,
                        },
                        CompositeRelayAction::Transfer {
                            token: CompositeRelayActionToken::BaseNative,
                            recipient,
                            amount: recipient_amount,
                        },
                    ]);
                } else if route_wrapped_native_output {
                    calls.push(CompositeRelayAction::Transfer {
                        token: CompositeRelayActionToken::Erc20(token),
                        recipient,
                        amount: recipient_amount,
                    });
                }
            }
        }
        if !calls.is_empty() {
            coalesce_sponsored_wrapped_native_leg(
                &mut public_unshields,
                &calls,
                authorization.wrapped_native_token,
            )?;
        }
        // A zero builder payment can leave a direct plan, which must not name an adapter.
        let adapter = (!calls.is_empty()).then_some(CompositeAdapter::RelayAdapt(
            authorization.relay_adapt_contract,
        ));
        let relay_actions = (!calls.is_empty()).then_some(CompositeRelayActions {
            min_gas_limit: U256::ZERO,
            calls,
        });
        Ok(MixedPrivateActionRequest {
            adapter,
            executor_calls: Vec::new(),
            private_sends,
            public_unshields,
            relay_actions,
            min_gas_price: 0,
            verify_proof,
            spend_up_to: false,
            rebuild,
        })
    }
}

fn coalesce_sponsored_wrapped_native_leg(
    public_unshields: &mut Vec<CompositeUnshieldLeg>,
    calls: &[CompositeRelayAction],
    wrapped_native: Address,
) -> std::result::Result<U256, SponsorshipError> {
    let net_wrapped_native = calls.iter().try_fold(U256::ZERO, |total, call| {
        let consumed = match call {
            CompositeRelayAction::UnwrapBase { amount } => *amount,
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::Erc20(token),
                amount,
                ..
            } if *token == wrapped_native => *amount,
            CompositeRelayAction::Transfer { .. } => U256::ZERO,
        };
        total
            .checked_add(consumed)
            .ok_or(SponsorshipError::ArithmeticOverflow)
    })?;
    let gross_wrapped_native = gross_up_sponsorship_payment(net_wrapped_native)?;
    if native_top_up_net_after_protocol_fee(gross_wrapped_native) != net_wrapped_native {
        return Err(SponsorshipError::ArithmeticOverflow);
    }
    public_unshields.retain(|leg| {
        leg.token_address != wrapped_native
            || leg.recipient != CompositeUnshieldRecipient::RelayAdapt
    });
    public_unshields.insert(
        0,
        CompositeUnshieldLeg {
            token_address: wrapped_native,
            amount: gross_wrapped_native,
            recipient: CompositeUnshieldRecipient::RelayAdapt,
            role: CompositeUnshieldLegRole::SponsoredWrappedNative,
        },
    );
    Ok(gross_wrapped_native)
}

fn sponsored_action_fingerprint_bytes(
    chain_id: u64,
    action: SponsoredActionKind,
    token: Address,
    amount: U256,
) -> Vec<u8> {
    let mut bytes = b"railoxide:sponsored-action:v1".to_vec();
    bytes.extend_from_slice(&chain_id.to_be_bytes());
    bytes.push(match action {
        SponsoredActionKind::Send => 1,
        SponsoredActionKind::Unshield => 2,
        SponsoredActionKind::BlockedShield => 3,
        SponsoredActionKind::PublicAction => 4,
    });
    bytes.extend_from_slice(token.as_slice());
    bytes.extend_from_slice(&amount.to_be_bytes::<32>());
    bytes
}

#[must_use]
pub fn sponsored_send_action_fingerprint(
    chain_id: u64,
    token: Address,
    amount: U256,
    recipient: &AddressData,
) -> FixedBytes<32> {
    let mut bytes =
        sponsored_action_fingerprint_bytes(chain_id, SponsoredActionKind::Send, token, amount);
    bytes.extend_from_slice(&recipient.master_public_key.to_be_bytes::<32>());
    bytes.extend_from_slice(&recipient.viewing_public_key);
    keccak256(bytes)
}

#[must_use]
pub fn sponsored_unshield_action_fingerprint(
    chain_id: u64,
    token: Address,
    amount: U256,
    recipient: Address,
    unwrap: bool,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
) -> FixedBytes<32> {
    let mut bytes =
        sponsored_action_fingerprint_bytes(chain_id, SponsoredActionKind::Unshield, token, amount);
    bytes.extend_from_slice(recipient.as_slice());
    bytes.push(u8::from(unwrap));
    if let Some(top_up) = native_top_up {
        bytes.push(1);
        bytes.extend_from_slice(top_up.recipient.as_slice());
        bytes.extend_from_slice(top_up.wrapped_native_token.as_slice());
        bytes.extend_from_slice(&top_up.native_amount.to_be_bytes::<32>());
        bytes.extend_from_slice(&top_up.wrapped_native_amount.to_be_bytes::<32>());
    } else {
        bytes.push(0);
    }
    keccak256(bytes)
}

/// Gas in `mode` without the buffer, which [`sponsorship_payment_from_estimate`] adds.
fn sponsored_approximate_gas(
    model: &RailgunGasModel,
    mode: GasEstimateMode,
    request: &MixedPrivateActionRequest,
    preview: &railgun_wallet::tx::MixedPrivateActionPreview,
) -> u64 {
    let unwrap_count = request.relay_actions.as_ref().map_or(0, |actions| {
        actions
            .calls
            .iter()
            .filter(|action| matches!(action, CompositeRelayAction::UnwrapBase { .. }))
            .count()
    });
    approximate_public_broadcaster_gas(
        model,
        mode,
        ApproximateTransactionShape {
            transaction_count: preview.shape.transaction_count,
            input_count: preview.shape.input_count,
            private_output_count: preview.shape.private_output_count,
            public_output_count: preview.shape.public_output_count,
            max_receiver_amount: U256::ZERO,
            relay_call_count: preview.shape.relay_call_count,
            uses_relay_adapt: preview.shape.uses_relay_adapt,
            unwrap_count,
            executor: false,
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn sponsored_provisional_payment_for_intent(
    chain_id: u64,
    chain: &EffectiveDesktopChainConfig,
    utxos: &[Utxo],
    wrapped_native: Address,
    payer: Address,
    relay_adapt: Address,
    signer: Address,
    intent: SponsoredPrivateIntent,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
    mode: GasEstimateMode,
) -> Result<SponsorshipPayment> {
    let tx_builder = TransactionBuilder {
        chain_type: 0,
        chain_id,
        railgun_contract: chain.railgun_contract,
    };
    let poi_spendable_wrapped_native = poi_spendable_token_balance(utxos, wrapped_native);
    let gas_model = RailgunGasModel::for_chain(chain_id);
    let preview_gas = |payment: SponsorshipPayment| -> Result<u64> {
        let authorization = sponsored_authorization(
            intent.action(),
            wrapped_native,
            payer,
            relay_adapt,
            payment,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            signer,
        );
        let request = intent.mixed_request(&authorization, native_top_up, false, None)?;
        let public_wrapped_native_spend = request_public_token_spend(&request, wrapped_native)?;
        let required_wrapped_native = sponsored_total_wrapped_native_spend(
            intent,
            public_wrapped_native_spend,
            wrapped_native,
        )?;
        if poi_spendable_wrapped_native < required_wrapped_native {
            return Err(SponsorshipError::InsufficientWrappedNativeForQuote {
                available: poi_spendable_wrapped_native,
            }
            .into());
        }
        let preview = tx_builder.preview_mixed_private_action_plan(utxos, &request)?;
        Ok(sponsored_approximate_gas(
            gas_model, mode, &request, &preview,
        ))
    };
    let initial_payment = sponsorship_payment(
        0,
        max_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
    )?;
    let mut estimated_gas = preview_gas(initial_payment)?;
    for _ in 0..SPONSORED_PROVISIONAL_MAX_STEPS {
        let payment = sponsorship_payment_from_estimate(
            estimated_gas,
            chain.gas.gas_limit_buffer,
            max_fee_per_gas,
            signer_native_balance_snapshot,
            incentive,
        )?;
        let next_estimated_gas = preview_gas(payment)?;
        if next_estimated_gas <= estimated_gas {
            return Ok(payment);
        }
        estimated_gas = next_estimated_gas;
    }
    Err(SponsorshipError::ProvisionalPaymentDidNotConverge.into())
}

#[allow(clippy::too_many_arguments)]
fn quote_sponsored_authorization_limit(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    utxos: &[Utxo],
    intent: SponsoredPrivateIntent,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
    signer: Address,
) -> Result<SponsoredAuthorizationLimit> {
    if railgun_ui::is_testnet_chain(chain_id) {
        return Err(eyre!(
            "Sponsored self-broadcast is unavailable on testnets."
        ));
    }
    if effective_chain.chain_id != chain_id {
        return Err(eyre!(
            "effective chain config is for chain {}, not {chain_id}",
            effective_chain.chain_id
        ));
    }
    let chain = effective_desktop_chain_config(chain_id, effective_chain)?;
    let wrapped_native = chain
        .wrapped_native_token
        .ok_or(SponsorshipError::MissingWrappedNativeToken)?;
    let payer = effective_chain
        .require_railgun()?
        .coinbase_payer
        .ok_or(SponsorshipError::MissingCoinbasePayer)?;
    let relay_adapt = chain
        .relay_adapt_contract
        .ok_or(SponsorshipError::MissingRelayAdapt)?;
    if effective_chain
        .require_railgun()?
        .sponsored_bundle_relays
        .is_empty()
    {
        return Err(SponsorshipError::MissingRelay.into());
    }
    let provisional_payment = |mode| {
        sponsored_provisional_payment_for_intent(
            chain_id,
            &chain,
            utxos,
            wrapped_native,
            payer,
            relay_adapt,
            signer,
            intent,
            native_top_up,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            signer_native_balance_snapshot,
            incentive,
            mode,
        )
    };
    let expected_payment = provisional_payment(GasEstimateMode::Expected)?;
    // The ceiling bounds the signed gas limit and the wrapped-native spend. Its quote-time
    // balance check, and admission against the limit, require POI-spendable wrapped native for
    // the ceiling, so they may refuse sends whose exact cost would fit.
    let ceiling_payment = provisional_payment(GasEstimateMode::UpperBound)?;
    let action_fingerprint = match intent {
        SponsoredPrivateIntent::Send {
            token,
            amount,
            recipient,
        } => sponsored_send_action_fingerprint(chain_id, token, amount, &recipient),
        SponsoredPrivateIntent::Unshield {
            token,
            amount,
            recipient,
            unwrap,
        } => sponsored_unshield_action_fingerprint(
            chain_id,
            token,
            amount,
            recipient,
            unwrap,
            native_top_up,
        ),
    };
    let maximum_authorization = sponsored_authorization(
        intent.action(),
        wrapped_native,
        payer,
        relay_adapt,
        ceiling_payment,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        signer,
    );
    let maximum_request =
        intent.mixed_request(&maximum_authorization, native_top_up, false, None)?;
    let public_wrapped_native_spend = maximum_request
        .public_unshields
        .iter()
        .filter(|leg| leg.token_address == wrapped_native)
        .try_fold(U256::ZERO, |total, leg| {
            total
                .checked_add(leg.amount)
                .ok_or(SponsorshipError::ArithmeticOverflow)
        })?;
    let max_total_wrapped_native_spend =
        sponsored_total_wrapped_native_spend(intent, public_wrapped_native_spend, wrapped_native)?;
    let limit = sponsored_authorization_limit(
        action_fingerprint,
        ceiling_payment.outer_gas_limit,
        intent.action(),
        wrapped_native,
        payer,
        relay_adapt,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
        signer,
        max_total_wrapped_native_spend,
    )?;
    Ok(SponsoredAuthorizationLimit {
        expected_transaction_gas_limit: expected_payment.outer_gas_limit,
        ..limit
    })
}

#[allow(clippy::too_many_arguments)]
pub fn quote_sponsored_send_authorization_limit(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    utxos: &[Utxo],
    token: Address,
    amount: U256,
    recipient: &AddressData,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
    signer: Address,
) -> Result<SponsoredAuthorizationLimit> {
    quote_sponsored_authorization_limit(
        chain_id,
        effective_chain,
        utxos,
        SponsoredPrivateIntent::Send {
            token,
            amount,
            recipient: *recipient,
        },
        None,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
        signer,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn quote_sponsored_unshield_authorization_limit(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    utxos: &[Utxo],
    token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    recipient: Address,
    unwrap: bool,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
    signer: Address,
) -> Result<SponsoredAuthorizationLimit> {
    let amount = unshield_receiver_amount_for_fee_mode(entered_amount, fee_mode)?;
    quote_sponsored_authorization_limit(
        chain_id,
        effective_chain,
        utxos,
        SponsoredPrivateIntent::Unshield {
            token,
            amount,
            recipient,
            unwrap,
        },
        native_top_up,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
        signer,
    )
}

pub(super) fn request_public_token_spend(
    request: &MixedPrivateActionRequest,
    token: Address,
) -> std::result::Result<U256, SponsorshipError> {
    request
        .public_unshields
        .iter()
        .filter(|leg| leg.token_address == token)
        .try_fold(U256::ZERO, |total, leg| {
            total
                .checked_add(leg.amount)
                .ok_or(SponsorshipError::ArithmeticOverflow)
        })
}

pub(super) fn sponsored_total_wrapped_native_spend(
    intent: SponsoredPrivateIntent,
    public_total: U256,
    wrapped_native: Address,
) -> std::result::Result<U256, SponsorshipError> {
    match intent {
        SponsoredPrivateIntent::Send { token, amount, .. } if token == wrapped_native => {
            public_total
                .checked_add(amount)
                .ok_or(SponsorshipError::ArithmeticOverflow)
        }
        _ => Ok(public_total),
    }
}
