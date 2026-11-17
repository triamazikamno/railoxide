use super::*;

/// Keccak-256 of the runtime returned by `eth_getCode` for the reviewed Ethereum deployment.
pub const REVIEWED_COINBASE_PAYER_RUNTIME_HASH: FixedBytes<32> =
    alloy::primitives::b256!("0xe67fe51b007a8c007ebebd6ea1096c43377fdcdfd643750715a5d92b1a3324b5");
pub const SPONSORED_FUNDING_GAS_LIMIT: u64 = 21_000;
pub const SPONSORED_PROVISIONAL_MAX_STEPS: usize = 4;
pub const SPONSORED_CUSTOM_INCENTIVE_MIN_PERCENT: u8 = 1;
pub const SPONSORED_CUSTOM_INCENTIVE_MAX_PERCENT: u8 = 100;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SponsoredIncentive {
    Economy,
    #[default]
    Standard,
    Priority,
    Custom(u8),
}

impl SponsoredIncentive {
    pub fn percent(self) -> Result<u8, SponsorshipError> {
        match self {
            Self::Economy => Ok(1),
            Self::Standard => Ok(5),
            Self::Priority => Ok(15),
            Self::Custom(percent)
                if (SPONSORED_CUSTOM_INCENTIVE_MIN_PERCENT
                    ..=SPONSORED_CUSTOM_INCENTIVE_MAX_PERCENT)
                    .contains(&percent) =>
            {
                Ok(percent)
            }
            Self::Custom(percent) => Err(SponsorshipError::InvalidCustomIncentive(percent)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SponsorshipPayment {
    pub outer_gas_limit: u64,
    pub outer_gas_cap: U256,
    pub signer_native_balance_snapshot: U256,
    pub balance_credit: U256,
    pub funding_principal: U256,
    pub funding_gas_limit: u64,
    pub funding_gas_cap: U256,
    pub reimbursement_base: U256,
    pub builder_payment: U256,
    pub gross_wrapped_native_spend: U256,
    pub protocol_fee: U256,
    pub incentive: SponsoredIncentive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SponsoredActionKind {
    Send,
    Unshield,
    BlockedShield,
    PublicAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateDeliveryMode {
    SelfBroadcast,
    PublicBroadcaster,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SponsoredAdmission {
    pub action: SponsoredActionKind,
    pub delivery: PrivateDeliveryMode,
    pub has_relays: bool,
    pub wrapped_native_token: Option<Address>,
    pub coinbase_payer: Option<Address>,
    pub relay_adapt_contract: Option<Address>,
    pub payer_verified: bool,
    pub signer_eligible: bool,
    pub poi_spendable_wrapped_native: U256,
    pub required_wrapped_native: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SponsoredAuthorization {
    pub action: SponsoredActionKind,
    pub wrapped_native_token: Address,
    pub coinbase_payer: Address,
    pub relay_adapt_contract: Address,
    pub builder_payment: U256,
    pub gross_wrapped_native_spend: U256,
    pub protocol_fee: U256,
    pub transaction_gas_limit: u64,
    pub outer_gas_cap: U256,
    pub signer_native_balance_snapshot: U256,
    pub balance_credit: U256,
    pub funding_principal: U256,
    pub funding_gas_limit: u64,
    pub funding_gas_cap: U256,
    pub reimbursement_base: U256,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub priority_fee_is_additional: bool,
    pub incentive: SponsoredIncentive,
    pub signer: Address,
    pub delivery: PrivateDeliveryMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SponsoredAuthorizationLimit {
    pub action_fingerprint: FixedBytes<32>,
    pub max_transaction_gas_limit: u64,
    /// Gas limit of the expected payment, for display only. Authorization enforcement uses the
    /// maximum fields. [`sponsored_authorization_limit`] sets it to the maximum; the quote
    /// replaces it with its `Expected` estimate.
    pub expected_transaction_gas_limit: u64,
    pub signer_native_balance_snapshot: U256,
    pub action: SponsoredActionKind,
    pub wrapped_native_token: Address,
    pub coinbase_payer: Address,
    pub relay_adapt_contract: Address,
    pub max_total_wrapped_native_spend: U256,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub incentive: SponsoredIncentive,
    pub signer: Address,
    pub delivery: PrivateDeliveryMode,
}

impl SponsoredAuthorizationLimit {
    pub fn maximum_payment(self) -> Result<SponsorshipPayment, SponsorshipError> {
        sponsorship_payment(
            self.max_transaction_gas_limit,
            self.max_fee_per_gas,
            self.signer_native_balance_snapshot,
            self.incentive,
        )
    }

    #[must_use]
    pub const fn allows_transaction_gas_limit(self, transaction_gas_limit: u64) -> bool {
        transaction_gas_limit <= self.max_transaction_gas_limit
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SponsorshipError {
    #[error("custom sponsored incentive must be an integer from 1% through 100%; got {0}%")]
    InvalidCustomIncentive(u8),
    #[error("sponsorship payment arithmetic overflowed")]
    ArithmeticOverflow,
    #[error("sponsored provisional payment did not converge within four steps")]
    ProvisionalPaymentDidNotConverge,
    #[error("sponsored plan shape or pinned inputs changed; fresh authorization is required")]
    SponsoredPlanShapeChangeRequired,
    #[error("final sponsored payment is underfunded: embedded {embedded}, required {required}")]
    FinalPaymentUnderfunded { embedded: U256, required: U256 },
    #[error("sponsored action changed after authorization")]
    AuthorizationMismatch,
    #[error("exact sponsored economics exceed the authorized maximum")]
    AuthorizationLimitExceeded,
    #[error("coinbase payer runtime does not match the reviewed deployment")]
    PayerRuntimeMismatch,
    #[error("sponsored transaction signer must be an EOA with empty code")]
    SignerHasCode,
    #[error("sponsored code preflight failed through the active query RPC context")]
    CodeQueryFailed,
    #[error("sponsorship is available only for self-broadcast delivery")]
    DeliveryUnsupported,
    #[error("Blocked-Shield rescue cannot use sponsored funding")]
    BlockedShieldUnsupported,
    #[error("public actions cannot use private wrapped-native sponsorship")]
    ActionUnsupported,
    #[error("no compatible sponsored relay is configured")]
    MissingRelay,
    #[error("the selected chain has no wrapped-native token")]
    MissingWrappedNativeToken,
    #[error("the selected chain has no coinbase payer")]
    MissingCoinbasePayer,
    #[error("the selected chain has no legacy RelayAdapt contract")]
    MissingRelayAdapt,
    #[error("coinbase payer verification has not succeeded")]
    PayerNotVerified,
    #[error("the selected signer is not eligible for sponsored funding")]
    SignerIneligible,
    #[error(
        "insufficient POI-spendable wrapped-native balance: available {available}, required {required}"
    )]
    InsufficientWrappedNative { available: U256, required: U256 },
    #[error(
        "insufficient POI-spendable wrapped-native balance to derive the maximum sponsored plan: available {available}"
    )]
    InsufficientWrappedNativeForQuote { available: U256 },
}

pub fn sponsored_gas_limit_with_buffer(
    estimated_gas: u64,
    gas_limit_buffer: u64,
) -> Result<u64, SponsorshipError> {
    estimated_gas
        .checked_add(gas_limit_buffer)
        .ok_or(SponsorshipError::ArithmeticOverflow)
}

pub fn sponsorship_payment_from_estimate(
    estimated_gas: u64,
    gas_limit_buffer: u64,
    max_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
) -> Result<SponsorshipPayment, SponsorshipError> {
    let gas_limit = sponsored_gas_limit_with_buffer(estimated_gas, gas_limit_buffer)?;
    sponsorship_payment(
        gas_limit,
        max_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
    )
}

pub fn provisional_sponsorship_payment(
    initial_estimated_gas: u64,
    gas_limit_buffer: u64,
    max_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
    mut estimate_for_gross_spend: impl FnMut(U256) -> Result<u64, SponsorshipError>,
) -> Result<SponsorshipPayment, SponsorshipError> {
    let mut estimated_gas = initial_estimated_gas;
    for _ in 0..SPONSORED_PROVISIONAL_MAX_STEPS {
        let payment = sponsorship_payment_from_estimate(
            estimated_gas,
            gas_limit_buffer,
            max_fee_per_gas,
            signer_native_balance_snapshot,
            incentive,
        )?;
        let next_estimated_gas = estimate_for_gross_spend(payment.gross_wrapped_native_spend)?;
        if next_estimated_gas <= estimated_gas {
            return Ok(payment);
        }
        estimated_gas = next_estimated_gas;
    }
    Err(SponsorshipError::ProvisionalPaymentDidNotConverge)
}

#[must_use]
pub fn sponsored_payment_requires_rebuild(
    embedded: SponsorshipPayment,
    required: SponsorshipPayment,
) -> bool {
    embedded.builder_payment < required.builder_payment
}

pub fn validate_final_sponsorship_payment(
    embedded: SponsorshipPayment,
    required: SponsorshipPayment,
) -> Result<(), SponsorshipError> {
    if sponsored_payment_requires_rebuild(embedded, required) {
        return Err(SponsorshipError::FinalPaymentUnderfunded {
            embedded: embedded.builder_payment,
            required: required.builder_payment,
        });
    }
    Ok(())
}

#[must_use]
pub fn poi_spendable_token_balance(utxos: &[Utxo], token: Address) -> U256 {
    utxos
        .iter()
        .filter(|utxo| utxo.token_address() == token)
        .fold(U256::ZERO, |total, utxo| {
            total.saturating_add(utxo.note.value)
        })
}

#[must_use]
pub const fn sponsored_authorization(
    action: SponsoredActionKind,
    wrapped_native_token: Address,
    coinbase_payer: Address,
    relay_adapt_contract: Address,
    payment: SponsorshipPayment,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer: Address,
) -> SponsoredAuthorization {
    SponsoredAuthorization {
        action,
        wrapped_native_token,
        coinbase_payer,
        relay_adapt_contract,
        builder_payment: payment.builder_payment,
        gross_wrapped_native_spend: payment.gross_wrapped_native_spend,
        protocol_fee: payment.protocol_fee,
        transaction_gas_limit: payment.outer_gas_limit,
        outer_gas_cap: payment.outer_gas_cap,
        signer_native_balance_snapshot: payment.signer_native_balance_snapshot,
        balance_credit: payment.balance_credit,
        funding_principal: payment.funding_principal,
        funding_gas_limit: payment.funding_gas_limit,
        funding_gas_cap: payment.funding_gas_cap,
        reimbursement_base: payment.reimbursement_base,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        priority_fee_is_additional: true,
        incentive: payment.incentive,
        signer,
        delivery: PrivateDeliveryMode::SelfBroadcast,
    }
}

pub fn sponsored_authorization_limit(
    action_fingerprint: FixedBytes<32>,
    max_transaction_gas_limit: u64,
    action: SponsoredActionKind,
    wrapped_native_token: Address,
    coinbase_payer: Address,
    relay_adapt_contract: Address,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
    signer: Address,
    max_total_wrapped_native_spend: U256,
) -> Result<SponsoredAuthorizationLimit, SponsorshipError> {
    sponsorship_payment(
        max_transaction_gas_limit,
        max_fee_per_gas,
        signer_native_balance_snapshot,
        incentive,
    )?;
    Ok(SponsoredAuthorizationLimit {
        action_fingerprint,
        max_transaction_gas_limit,
        expected_transaction_gas_limit: max_transaction_gas_limit,
        signer_native_balance_snapshot,
        action,
        wrapped_native_token,
        coinbase_payer,
        relay_adapt_contract,
        max_total_wrapped_native_spend,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        incentive,
        signer,
        delivery: PrivateDeliveryMode::SelfBroadcast,
    })
}

pub fn validate_sponsored_authorization_limit(
    limit: SponsoredAuthorizationLimit,
    action_fingerprint: FixedBytes<32>,
    exact: SponsoredAuthorization,
    total_wrapped_native_spend: U256,
) -> Result<(), SponsorshipError> {
    if action_fingerprint != limit.action_fingerprint
        || exact.action != limit.action
        || exact.wrapped_native_token != limit.wrapped_native_token
        || exact.coinbase_payer != limit.coinbase_payer
        || exact.relay_adapt_contract != limit.relay_adapt_contract
        || exact.max_fee_per_gas != limit.max_fee_per_gas
        || exact.max_priority_fee_per_gas != limit.max_priority_fee_per_gas
        || exact.signer_native_balance_snapshot != limit.signer_native_balance_snapshot
        || exact.incentive != limit.incentive
        || exact.signer != limit.signer
        || exact.delivery != limit.delivery
    {
        return Err(SponsorshipError::AuthorizationMismatch);
    }
    if !limit.allows_transaction_gas_limit(exact.transaction_gas_limit) {
        return Err(SponsorshipError::AuthorizationLimitExceeded);
    }
    let exact_payment = sponsorship_payment(
        exact.transaction_gas_limit,
        exact.max_fee_per_gas,
        exact.signer_native_balance_snapshot,
        exact.incentive,
    )?;
    if exact.outer_gas_cap != exact_payment.outer_gas_cap
        || exact.balance_credit != exact_payment.balance_credit
        || exact.funding_principal != exact_payment.funding_principal
        || exact.funding_gas_limit != exact_payment.funding_gas_limit
        || exact.funding_gas_cap != exact_payment.funding_gas_cap
        || exact.reimbursement_base != exact_payment.reimbursement_base
    {
        return Err(SponsorshipError::AuthorizationMismatch);
    }
    let maximum_payment = limit.maximum_payment()?;
    if exact.builder_payment > maximum_payment.builder_payment
        || exact.gross_wrapped_native_spend > maximum_payment.gross_wrapped_native_spend
        || exact.protocol_fee > maximum_payment.protocol_fee
        || total_wrapped_native_spend > limit.max_total_wrapped_native_spend
    {
        return Err(SponsorshipError::AuthorizationLimitExceeded);
    }
    if exact.builder_payment != exact_payment.builder_payment
        || exact.gross_wrapped_native_spend != exact_payment.gross_wrapped_native_spend
        || exact.protocol_fee != exact_payment.protocol_fee
    {
        return Err(SponsorshipError::AuthorizationMismatch);
    }
    Ok(())
}

pub fn sponsorship_payment(
    outer_gas_limit: u64,
    max_fee_per_gas: u128,
    signer_native_balance_snapshot: U256,
    incentive: SponsoredIncentive,
) -> Result<SponsorshipPayment, SponsorshipError> {
    let fee = U256::from(max_fee_per_gas);
    let outer_gas_cap = U256::from(outer_gas_limit)
        .checked_mul(fee)
        .ok_or(SponsorshipError::ArithmeticOverflow)?;
    let balance_credit = signer_native_balance_snapshot.min(outer_gas_cap);
    let funding_principal = outer_gas_cap - balance_credit;
    let funding_gas_limit = if funding_principal.is_zero() {
        0
    } else {
        SPONSORED_FUNDING_GAS_LIMIT
    };
    let funding_gas_cap = U256::from(funding_gas_limit)
        .checked_mul(fee)
        .ok_or(SponsorshipError::ArithmeticOverflow)?;
    let reimbursement_base = funding_principal
        .checked_add(funding_gas_cap)
        .ok_or(SponsorshipError::ArithmeticOverflow)?;
    let incentive_percent = incentive.percent()?;
    let builder_payment = if reimbursement_base.is_zero() {
        U256::ZERO
    } else {
        let multiplier = U256::from(100_u8 + incentive_percent);
        reimbursement_base
            .checked_mul(multiplier)
            .and_then(|value| value.checked_add(U256::from(99_u8)))
            .ok_or(SponsorshipError::ArithmeticOverflow)?
            / U256::from(100_u8)
    };
    let gross_wrapped_native_spend = gross_up_sponsorship_payment(builder_payment)?;
    let protocol_fee = gross_wrapped_native_spend - builder_payment;
    Ok(SponsorshipPayment {
        outer_gas_limit,
        outer_gas_cap,
        signer_native_balance_snapshot,
        balance_credit,
        funding_principal,
        funding_gas_limit,
        funding_gas_cap,
        reimbursement_base,
        builder_payment,
        gross_wrapped_native_spend,
        protocol_fee,
        incentive,
    })
}

pub fn gross_up_sponsorship_payment(net: U256) -> Result<U256, SponsorshipError> {
    if net.is_zero() {
        return Ok(U256::ZERO);
    }
    let denominator = U256::from(FEE_BASIS_POINTS_DENOMINATOR);
    let net_basis_points = denominator
        .checked_sub(U256::from(RAILGUN_PROTOCOL_FEE_BPS))
        .ok_or(SponsorshipError::ArithmeticOverflow)?;
    net.checked_sub(U256::from(1_u8))
        .and_then(|value| value.checked_mul(denominator))
        .and_then(|value| value.checked_div(net_basis_points))
        .and_then(|value| value.checked_add(U256::from(1_u8)))
        .ok_or(SponsorshipError::ArithmeticOverflow)
}

pub fn verify_coinbase_payer_runtime(code: &[u8]) -> Result<(), SponsorshipError> {
    if code.is_empty() || keccak256(code) != REVIEWED_COINBASE_PAYER_RUNTIME_HASH {
        return Err(SponsorshipError::PayerRuntimeMismatch);
    }
    Ok(())
}

pub const fn verify_sponsored_signer_code(code: &[u8]) -> Result<(), SponsorshipError> {
    if code.is_empty() {
        Ok(())
    } else {
        Err(SponsorshipError::SignerHasCode)
    }
}

pub async fn sponsored_code_preflight(
    provider: &impl Provider,
    payer: Address,
    signer: Address,
) -> Result<(), SponsorshipError> {
    let payer_code = provider
        .get_code_at(payer)
        .await
        .map_err(|_| SponsorshipError::CodeQueryFailed)?;
    verify_coinbase_payer_runtime(&payer_code)?;
    let signer_code = provider
        .get_code_at(signer)
        .await
        .map_err(|_| SponsorshipError::CodeQueryFailed)?;
    verify_sponsored_signer_code(&signer_code)
}

pub fn validate_sponsored_admission(admission: SponsoredAdmission) -> Result<(), SponsorshipError> {
    if admission.action == SponsoredActionKind::BlockedShield {
        return Err(SponsorshipError::BlockedShieldUnsupported);
    }
    if admission.action == SponsoredActionKind::PublicAction {
        return Err(SponsorshipError::ActionUnsupported);
    }
    if admission.delivery != PrivateDeliveryMode::SelfBroadcast {
        return Err(SponsorshipError::DeliveryUnsupported);
    }
    if !admission.has_relays {
        return Err(SponsorshipError::MissingRelay);
    }
    if admission.wrapped_native_token.is_none() {
        return Err(SponsorshipError::MissingWrappedNativeToken);
    }
    if admission.coinbase_payer.is_none() {
        return Err(SponsorshipError::MissingCoinbasePayer);
    }
    if admission.relay_adapt_contract.is_none() {
        return Err(SponsorshipError::MissingRelayAdapt);
    }
    if !admission.payer_verified {
        return Err(SponsorshipError::PayerNotVerified);
    }
    if !admission.signer_eligible {
        return Err(SponsorshipError::SignerIneligible);
    }
    if admission.poi_spendable_wrapped_native < admission.required_wrapped_native {
        return Err(SponsorshipError::InsufficientWrappedNative {
            available: admission.poi_spendable_wrapped_native,
            required: admission.required_wrapped_native,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
