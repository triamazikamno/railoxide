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
mod tests {
    use super::*;

    #[test]
    fn custom_incentive_rejects_out_of_range_percentages() {
        assert_eq!(SponsoredIncentive::Custom(1).percent(), Ok(1));
        assert_eq!(SponsoredIncentive::Custom(100).percent(), Ok(100));
        assert!(SponsoredIncentive::Custom(0).percent().is_err());
        assert!(SponsoredIncentive::Custom(101).percent().is_err());
    }

    #[test]
    fn payment_rounds_incentive_and_protocol_gross_up_upward() {
        let payment =
            sponsorship_payment(1, 1, U256::ZERO, SponsoredIncentive::Standard).expect("payment");
        assert_eq!(payment.reimbursement_base, U256::from(21_001_u64));
        assert_eq!(payment.builder_payment, U256::from(22_052_u64));
        assert_eq!(
            native_top_up_net_after_protocol_fee(payment.gross_wrapped_native_spend),
            payment.builder_payment
        );
    }

    #[test]
    fn payment_applies_buffer_outer_funding_and_incentive_rules_in_order() {
        let payment =
            sponsorship_payment_from_estimate(100, 20, 2, U256::ZERO, SponsoredIncentive::Economy)
                .expect("payment");
        assert_eq!(payment.outer_gas_cap, U256::from(240_u64));
        assert_eq!(payment.funding_gas_cap, U256::from(42_000_u64));
        assert_eq!(payment.reimbursement_base, U256::from(42_240_u64));
        assert_eq!(payment.builder_payment, U256::from(42_663_u64));
    }

    #[test]
    fn payment_credits_partial_balance_and_conditionally_adds_funding_gas() {
        let payment = sponsorship_payment(100, 2, U256::from(80_u8), SponsoredIncentive::Economy)
            .expect("payment");

        assert_eq!(payment.outer_gas_cap, U256::from(200_u16));
        assert_eq!(payment.balance_credit, U256::from(80_u8));
        assert_eq!(payment.funding_principal, U256::from(120_u8));
        assert_eq!(payment.funding_gas_limit, SPONSORED_FUNDING_GAS_LIMIT);
        assert_eq!(payment.funding_gas_cap, U256::from(42_000_u64));
        assert_eq!(payment.reimbursement_base, U256::from(42_120_u64));
        assert_eq!(payment.builder_payment, U256::from(42_542_u64));
    }

    #[test]
    fn payment_caps_overfunded_balance_credit_and_has_zero_delta() {
        let snapshot = U256::from(250_u16);
        let payment =
            sponsorship_payment(100, 2, snapshot, SponsoredIncentive::Standard).expect("payment");

        assert_eq!(payment.signer_native_balance_snapshot, snapshot);
        assert_eq!(payment.outer_gas_cap, U256::from(200_u16));
        assert_eq!(payment.balance_credit, payment.outer_gas_cap);
        assert_eq!(payment.funding_principal, U256::ZERO);
        assert_eq!(payment.funding_gas_limit, 0);
        assert_eq!(payment.funding_gas_cap, U256::ZERO);
        assert_eq!(payment.reimbursement_base, U256::ZERO);
        assert_eq!(payment.builder_payment, U256::ZERO);
        assert_eq!(payment.gross_wrapped_native_spend, U256::ZERO);
        assert_eq!(payment.protocol_fee, U256::ZERO);
    }

    #[test]
    fn provisional_payment_is_monotone_and_bounded_to_four_steps() {
        let mut estimates = vec![101_u64, 102, 103, 103].into_iter();
        let payment = provisional_sponsorship_payment(
            100,
            0,
            1,
            U256::ZERO,
            SponsoredIncentive::Economy,
            |_| Ok(estimates.next().expect("bounded estimate")),
        )
        .expect("converged payment");
        assert_eq!(payment.outer_gas_cap, U256::from(103_u64));

        let mut next = 101_u64;
        assert_eq!(
            provisional_sponsorship_payment(
                100,
                0,
                1,
                U256::ZERO,
                SponsoredIncentive::Economy,
                |_| {
                    let estimate = next;
                    next += 1;
                    Ok(estimate)
                },
            ),
            Err(SponsorshipError::ProvisionalPaymentDidNotConverge)
        );
    }

    #[test]
    fn exact_payment_decision_converges_or_reports_second_estimate_amounts() {
        let embedded =
            sponsorship_payment(100, 2, U256::ZERO, SponsoredIncentive::Standard).expect("payment");
        let covered =
            sponsorship_payment(99, 2, U256::ZERO, SponsoredIncentive::Standard).expect("payment");
        let required =
            sponsorship_payment(101, 2, U256::ZERO, SponsoredIncentive::Standard).expect("payment");
        assert_eq!(
            validate_final_sponsorship_payment(embedded, covered),
            Ok(())
        );
        assert_eq!(
            validate_final_sponsorship_payment(embedded, required),
            Err(SponsorshipError::FinalPaymentUnderfunded {
                embedded: embedded.builder_payment,
                required: required.builder_payment,
            })
        );
    }

    #[test]
    fn authorization_limit_accepts_lower_exact_economics_and_rejects_changes() {
        let wrapped_native = Address::from([4; 20]);
        let payer = Address::from([5; 20]);
        let signer = Address::from([6; 20]);
        let relay_adapt = Address::from([7; 20]);
        let additional_spend = U256::from(7_u8);
        let signer_native_balance_snapshot = U256::from(1_000_000_u64);
        let action_fingerprint = FixedBytes::from([7; 32]);
        let max_total = sponsorship_payment(
            2_000_000,
            2,
            signer_native_balance_snapshot,
            SponsoredIncentive::Standard,
        )
        .expect("maximum payment")
        .gross_wrapped_native_spend
            + additional_spend;
        let limit = sponsored_authorization_limit(
            action_fingerprint,
            2_000_000,
            SponsoredActionKind::Send,
            wrapped_native,
            payer,
            relay_adapt,
            2,
            1,
            signer_native_balance_snapshot,
            SponsoredIncentive::Standard,
            signer,
            max_total,
        )
        .expect("authorization limit");
        let exact_payment = sponsorship_payment(
            1_000_000,
            2,
            signer_native_balance_snapshot,
            SponsoredIncentive::Standard,
        )
        .expect("exact payment");
        let exact = sponsored_authorization(
            SponsoredActionKind::Send,
            wrapped_native,
            payer,
            relay_adapt,
            exact_payment,
            2,
            1,
            signer,
        );
        let total = additional_spend + exact.gross_wrapped_native_spend;
        assert_eq!(
            validate_sponsored_authorization_limit(limit, action_fingerprint, exact, total),
            Ok(())
        );
        assert_eq!(
            validate_sponsored_authorization_limit(limit, FixedBytes::from([9; 32]), exact, total,),
            Err(SponsorshipError::AuthorizationMismatch)
        );
        for changed in [
            SponsoredAuthorization {
                action: SponsoredActionKind::Unshield,
                ..exact
            },
            SponsoredAuthorization {
                wrapped_native_token: Address::from([8; 20]),
                ..exact
            },
            SponsoredAuthorization {
                coinbase_payer: Address::from([8; 20]),
                ..exact
            },
            SponsoredAuthorization {
                relay_adapt_contract: Address::from([8; 20]),
                ..exact
            },
            SponsoredAuthorization {
                max_fee_per_gas: 3,
                ..exact
            },
            SponsoredAuthorization {
                max_priority_fee_per_gas: 2,
                ..exact
            },
            SponsoredAuthorization {
                signer_native_balance_snapshot: signer_native_balance_snapshot + U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                outer_gas_cap: exact.outer_gas_cap - U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                balance_credit: U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                funding_principal: exact.funding_principal - U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                funding_gas_limit: exact.funding_gas_limit - 1,
                ..exact
            },
            SponsoredAuthorization {
                funding_gas_cap: exact.funding_gas_cap - U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                reimbursement_base: exact.reimbursement_base - U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                builder_payment: exact.builder_payment - U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                gross_wrapped_native_spend: exact.gross_wrapped_native_spend - U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                protocol_fee: exact.protocol_fee - U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                incentive: SponsoredIncentive::Economy,
                ..exact
            },
            SponsoredAuthorization {
                signer: Address::from([8; 20]),
                ..exact
            },
            SponsoredAuthorization {
                delivery: PrivateDeliveryMode::PublicBroadcaster,
                ..exact
            },
        ] {
            assert_eq!(
                validate_sponsored_authorization_limit(limit, action_fingerprint, changed, total,),
                Err(SponsorshipError::AuthorizationMismatch)
            );
        }
    }

    #[test]
    fn authorization_limit_rejects_exact_economics_above_maximum() {
        let wrapped_native = Address::from([4; 20]);
        let payer = Address::from([5; 20]);
        let signer = Address::from([6; 20]);
        let relay_adapt = Address::from([7; 20]);
        let action_fingerprint = FixedBytes::from([8; 32]);
        let max_total = sponsorship_payment(1_000_000, 2, U256::ZERO, SponsoredIncentive::Standard)
            .expect("maximum payment")
            .gross_wrapped_native_spend;
        let limit = sponsored_authorization_limit(
            action_fingerprint,
            1_000_000,
            SponsoredActionKind::Unshield,
            wrapped_native,
            payer,
            relay_adapt,
            2,
            1,
            U256::ZERO,
            SponsoredIncentive::Standard,
            signer,
            max_total,
        )
        .expect("authorization limit");
        let maximum_payment = limit.maximum_payment().expect("maximum payment");
        let exact = sponsored_authorization(
            SponsoredActionKind::Unshield,
            wrapped_native,
            payer,
            relay_adapt,
            maximum_payment,
            2,
            1,
            signer,
        );

        assert_eq!(
            validate_sponsored_authorization_limit(limit, action_fingerprint, exact, max_total),
            Ok(())
        );
        for exceeded in [
            SponsoredAuthorization {
                transaction_gas_limit: limit.max_transaction_gas_limit + 1,
                ..exact
            },
            SponsoredAuthorization {
                builder_payment: maximum_payment.builder_payment + U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                gross_wrapped_native_spend: maximum_payment.gross_wrapped_native_spend + U256::ONE,
                ..exact
            },
            SponsoredAuthorization {
                protocol_fee: maximum_payment.protocol_fee + U256::ONE,
                ..exact
            },
        ] {
            assert_eq!(
                validate_sponsored_authorization_limit(
                    limit,
                    action_fingerprint,
                    exceeded,
                    max_total,
                ),
                Err(SponsorshipError::AuthorizationLimitExceeded)
            );
        }
        assert_eq!(
            validate_sponsored_authorization_limit(
                limit,
                action_fingerprint,
                exact,
                max_total + U256::ONE,
            ),
            Err(SponsorshipError::AuthorizationLimitExceeded)
        );
    }

    #[test]
    fn poi_spendable_balance_uses_only_the_requested_token() {
        let token = Address::from([4; 20]);
        let other = Address::from([5; 20]);
        let utxo = |token, value| {
            Utxo::new(
                Note::new_unshield(Address::ZERO, token, U256::from(value)),
                0,
                value,
                UtxoSource {
                    tx_hash: FixedBytes::ZERO,
                    block_number: 0,
                    block_timestamp: 0,
                },
                UtxoCommitmentKind::Transact,
            )
        };
        assert_eq!(
            poi_spendable_token_balance(&[utxo(token, 7), utxo(other, 11)], token),
            U256::from(7_u8)
        );
    }

    #[test]
    fn payment_overflow_is_rejected() {
        assert_eq!(
            gross_up_sponsorship_payment(U256::MAX),
            Err(SponsorshipError::ArithmeticOverflow)
        );
    }

    #[test]
    fn payer_and_signer_code_checks_fail_closed() {
        assert_eq!(
            verify_coinbase_payer_runtime(&[]),
            Err(SponsorshipError::PayerRuntimeMismatch)
        );
        assert_eq!(verify_sponsored_signer_code(&[]), Ok(()));
        assert_eq!(
            verify_sponsored_signer_code(&[0xef, 0x01]),
            Err(SponsorshipError::SignerHasCode)
        );
    }

    #[test]
    fn blocked_shield_and_missing_prerequisites_are_rejected() {
        let valid = SponsoredAdmission {
            action: SponsoredActionKind::Send,
            delivery: PrivateDeliveryMode::SelfBroadcast,
            has_relays: true,
            wrapped_native_token: Some(Address::from([1; 20])),
            coinbase_payer: Some(Address::from([2; 20])),
            relay_adapt_contract: Some(Address::from([3; 20])),
            payer_verified: true,
            signer_eligible: true,
            poi_spendable_wrapped_native: U256::from(10_u8),
            required_wrapped_native: U256::from(10_u8),
        };
        assert_eq!(validate_sponsored_admission(valid), Ok(()));
        assert_eq!(
            validate_sponsored_admission(SponsoredAdmission {
                action: SponsoredActionKind::PublicAction,
                ..valid
            }),
            Err(SponsorshipError::ActionUnsupported)
        );
        assert_eq!(
            validate_sponsored_admission(SponsoredAdmission {
                action: SponsoredActionKind::BlockedShield,
                ..valid
            }),
            Err(SponsorshipError::BlockedShieldUnsupported)
        );
        assert_eq!(
            validate_sponsored_admission(SponsoredAdmission {
                poi_spendable_wrapped_native: U256::from(9_u8),
                ..valid
            }),
            Err(SponsorshipError::InsufficientWrappedNative {
                available: U256::from(9_u8),
                required: U256::from(10_u8),
            })
        );
        for (admission, expected) in [
            (
                SponsoredAdmission {
                    delivery: PrivateDeliveryMode::PublicBroadcaster,
                    ..valid
                },
                SponsorshipError::DeliveryUnsupported,
            ),
            (
                SponsoredAdmission {
                    has_relays: false,
                    ..valid
                },
                SponsorshipError::MissingRelay,
            ),
            (
                SponsoredAdmission {
                    wrapped_native_token: None,
                    ..valid
                },
                SponsorshipError::MissingWrappedNativeToken,
            ),
            (
                SponsoredAdmission {
                    coinbase_payer: None,
                    ..valid
                },
                SponsorshipError::MissingCoinbasePayer,
            ),
            (
                SponsoredAdmission {
                    relay_adapt_contract: None,
                    ..valid
                },
                SponsorshipError::MissingRelayAdapt,
            ),
            (
                SponsoredAdmission {
                    payer_verified: false,
                    ..valid
                },
                SponsorshipError::PayerNotVerified,
            ),
            (
                SponsoredAdmission {
                    signer_eligible: false,
                    ..valid
                },
                SponsorshipError::SignerIneligible,
            ),
        ] {
            assert_eq!(validate_sponsored_admission(admission), Err(expected));
        }
    }
}
