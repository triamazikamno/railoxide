use std::collections::BTreeSet;
use std::sync::Arc;

use alloy::eips::eip7702::Authorization;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Signature, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::railgun::RelayAdapt7702;
use eyre::{Result, WrapErr, eyre};
use railgun_wallet::tx::ExecutorContext;
use railgun_wallet::{TransactionCall, Utxo};

use super::{
    ExecutorOwner, ExecutorRecoveryExecution, ExecutorRecoveryFunding, PreparedExecutorRecovery,
};
use crate::desktop::executor_discovery::{inspect_for_signing, matches_executor_delegation};
use crate::settings::{EffectiveChainConfig, ExecutorProfile};
use crate::vault::{
    ExecutorInputIdentity, ExecutorOperationId, ExecutorPayloadContext, ExecutorPayloadPurpose,
    IssuedExecutorPayload,
};
use crate::{
    ExecutorActivity, ExecutorAsset, ExecutorInspection, OperationHttpClient,
    PublicBroadcasterCandidate,
};

mod issuance;
mod preparation;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

/// Funding selection precedes allocation. Existing sponsored Send and Unshield
/// select their legacy route before reaching this owner.
#[derive(Clone)]
pub enum ExecutorDelivery {
    PublicBroadcaster(Box<PublicBroadcasterCandidate>),
    SelfBroadcast { sender: Address, sponsored: bool },
    ManualExport,
}

impl ExecutorDelivery {
    /// Admit this delivery for an operation needing `profile`'s capability. Returns the
    /// profile its delegation authorization targets: a broadcaster's advertised delegate.
    pub fn admit(&self, profile: ExecutorProfile) -> Result<ExecutorProfile> {
        match self {
            Self::PublicBroadcaster(candidate) => {
                let target = candidate
                    .relay_adapt_7702
                    .and_then(|advertised| profile.interchangeable(advertised))
                    // Fee expiry and wallet count are informational: the broadcaster simulates
                    // and settles any profitable request.
                    .filter(|_| candidate.chain_id == profile.chain_id())
                    .ok_or_else(|| {
                        eyre!("selected broadcaster does not support this executor operation")
                    })?;
                candidate.parsed_required_poi_list_keys()?;
                Ok(target)
            }
            Self::SelfBroadcast {
                sponsored: false, ..
            } => Ok(profile),
            Self::SelfBroadcast {
                sponsored: true, ..
            } => Err(eyre!(
                "builder sponsorship is unavailable for executor operations"
            )),
            Self::ManualExport => Err(eyre!(
                "manual calldata export cannot represent executor delegation authorizations"
            )),
        }
    }

    const fn sender(&self) -> Option<Address> {
        match self {
            Self::SelfBroadcast { sender, .. } => Some(*sender),
            Self::PublicBroadcaster(_) | Self::ManualExport => None,
        }
    }
}

/// Outside private sponsorship, unwrap-to-native and native top-up Unshields run only
/// through the executor; there is no legacy `RelayAdapt` fallback.
#[must_use]
pub const fn unshield_requires_executor(
    unwrap: bool,
    native_top_up: bool,
    sponsored: bool,
) -> bool {
    (unwrap || native_top_up) && !sponsored
}

/// Why an Unshield that requires the executor cannot use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExecutorRouteUnavailable {
    #[error(
        "Unwrap and gas top-up need this wallet's stealth account, which this session doesn't have."
    )]
    NoExecutorOwner,
    #[error("Unwrap and gas top-up need the accepted RelayAdapt7702 contract on this network.")]
    NoAcceptedProfile,
}

/// The executor route for an Unshield that requires it.
pub fn unshield_executor_route(
    has_executor_owner: bool,
    chain: &EffectiveChainConfig,
) -> std::result::Result<ExecutorProfile, ExecutorRouteUnavailable> {
    if !has_executor_owner {
        return Err(ExecutorRouteUnavailable::NoExecutorOwner);
    }
    chain
        .accepted_executor_profile()
        .ok_or(ExecutorRouteUnavailable::NoAcceptedProfile)
}

/// A newly reserved account that was found used before it issued anything. It is retired.
#[derive(Debug, thiserror::Error)]
#[error("reserved executor has prior activity; it remains available for recovery")]
pub struct ExecutorAccountUsed {
    operation: ExecutorOperationId,
}

impl ExecutorAccountUsed {
    /// The retired account's operation.
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }
}

/// Native preparation identity carried through quotes and proof generation.
/// It contains no signing key and cannot be constructed from browser input.
pub struct PreparedExecutorOperation {
    operation: ExecutorOperationId,
    generation: u64,
    wallet_id: String,
    owner: tokio::sync::watch::Sender<bool>,
    context: ExecutorContext,
    delivery: ExecutorDelivery,
    inspection_assets: Vec<ExecutorAsset>,
    recovery: Option<std::sync::Arc<PreparedExecutorRecovery>>,
}

impl PreparedExecutorOperation {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }

    #[must_use]
    pub const fn context(&self) -> ExecutorContext {
        self.context
    }

    #[must_use]
    pub const fn delivery(&self) -> &ExecutorDelivery {
        &self.delivery
    }

    /// A retained executor's recovery, including a swap's early cancellation.
    pub(crate) const fn is_recovery(&self) -> bool {
        self.recovery.is_some()
    }

    pub(crate) fn require_broadcaster(&self, candidate: &PublicBroadcasterCandidate) -> Result<()> {
        let ExecutorDelivery::PublicBroadcaster(prepared) = &self.delivery else {
            return Err(eyre!("executor preparation selected another funding route"));
        };
        if prepared.chain_id != candidate.chain_id
            || prepared.railgun_address != candidate.railgun_address
            || prepared.fees_id != candidate.fees_id
            || prepared.fee != candidate.fee
            || prepared.token != candidate.token
            || prepared.fee_expiration != candidate.fee_expiration
            || prepared.viewing_public_key != candidate.viewing_public_key
            || prepared.required_poi_list_keys != candidate.required_poi_list_keys
        {
            return Err(eyre!(
                "executor broadcaster selection changed; refresh preparation"
            ));
        }
        Ok(())
    }
}

/// What a prepared operation's reservation stores beside its account.
pub(super) enum OperationReservation<'a> {
    /// An ordinary reservation, or a new swap's, whose record is created holding
    /// `swap_approval` and the link to its destination stealth account.
    Operation {
        purpose_summary: Option<&'a str>,
        swap_approval: Option<&'a crate::vault::SwapApproval>,
        destination_operation: Option<ExecutorOperationId>,
    },
    /// A private Bridge swap's destination stealth account on this chain.
    SwapDestination(crate::vault::SwapDestinationRecord),
}

/// Returning this value is signed-data handoff: its payload is already durable.
pub struct IssuedExecutorTransaction {
    operation: ExecutorOperationId,
    payload_hash: B256,
    transaction: TransactionRequest,
}

/// The caller-specific part of one execute payload: the hash the executor signs,
/// the transaction value, the private inputs it spends, and how the signature
/// becomes the signed call.
pub(super) struct PayloadToSign<'a> {
    pub(super) hash: B256,
    pub(super) value: U256,
    pub(super) inputs: Vec<ExecutorInputIdentity>,
    /// Sign an authorization even when this delegate is installed.
    pub(super) authorize_delegation: bool,
    pub(super) authorize: Box<dyn FnOnce(Signature) -> Result<TransactionCall> + Send + 'a>,
}

/// Chain evidence that one paid fee-convergence loop reuses to sign repriced
/// variants of its own payload. The loop owns this value and drops it before
/// broadcaster submission, on error, and on exhaustion, so it never outlives
/// one signing approval.
///
/// Reuse is safe because every variant signs the same execution nonce, so at
/// most one of them can execute. A fresh read would only notice a third party
/// executing an exposed variant a few seconds earlier. Persistence, input
/// reservations, authorization, and the durable nonce observation are
/// unchanged. The context holds no signing material.
#[derive(Default)]
pub(crate) struct IssueRetry {
    seed: Option<IssueRetrySeed>,
}

struct IssueRetrySeed {
    operation: ExecutorOperationId,
    execution_nonce: U256,
    /// The unused-account inspection behind the first payload, including its
    /// nonce observation.
    checked: Arc<super::spare::CheckedExecutor>,
    /// Every payload this context issued. Any other payload blocks reuse.
    issued: BTreeSet<B256>,
}

impl IssueRetry {
    /// Returns the saved inspection only while the durable record shows nothing
    /// but this context's own unsent payloads under the same observation. History
    /// reconciliation can replace that observation without an unused-cache change.
    fn reusable(
        &self,
        prepared: &PreparedExecutorOperation,
        record: &crate::vault::ExecutorRecord,
    ) -> Option<Arc<super::spare::CheckedExecutor>> {
        let seed = self.seed.as_ref()?;
        (prepared.recovery.is_none()
            && seed.operation == prepared.operation
            && seed.execution_nonce == prepared.context.execution_nonce
            && record.issued().iter().all(|payload| {
                seed.issued.contains(&payload.hash())
                    && payload.transaction_hashes().is_empty()
                    && !record.nonce_resolved(payload.nonce())
            })
            && seed.checked.ensure_valid().is_ok()
            && record.nonce_observation() == Some(seed.checked.observed))
        .then(|| Arc::clone(&seed.checked))
    }
}

fn require_unfinished_operation(record: &crate::vault::ExecutorRecord) -> Result<()> {
    if record
        .issued()
        .iter()
        .any(|payload| record.nonce_resolved(payload.nonce()))
    {
        return Err(eyre!(
            "this executor operation has already completed; start a new action"
        ));
    }
    Ok(())
}

impl IssuedExecutorTransaction {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }

    #[must_use]
    pub const fn payload_hash(&self) -> B256 {
        self.payload_hash
    }

    #[must_use]
    pub const fn transaction(&self) -> &TransactionRequest {
        &self.transaction
    }
}

/// Execution and recovery share the Ethereum authorization nonce rules. The
/// contract execution nonce is signed separately into their respective calls.
pub(super) fn authorize_delegation(
    transaction: &mut TransactionRequest,
    profile: ExecutorProfile,
    inspection: &ExecutorInspection,
    signer: &PrivateKeySigner,
    recovery: bool,
    require_authorization: bool,
) -> Result<()> {
    let code = inspection
        .code()
        .ok_or_else(|| eyre!("executor delegation is unavailable"))?;
    let installed = crate::desktop::executor_discovery::executor_delegation(code);
    let delegated = installed == Some(profile.delegate());
    // SDK broadcaster TX7702 requests require an authorization even when this
    // delegate is already installed. Self-broadcast execution reuses an installed
    // delegate without one unless its payload requires it, as a native shield does.
    // An account on another delegate of this capability moves to the delivery's delegate.
    if code.is_empty()
        || !delegated
            && (recovery && installed.is_some() || matches_executor_delegation(code, profile))
        || require_authorization && delegated
    {
        let account_nonce = inspection
            .account_nonce()
            .ok_or_else(|| eyre!("executor account nonce is unavailable"))?;
        let nonce = if transaction.from == Some(signer.address()) {
            account_nonce
                .checked_add(1)
                .ok_or_else(|| eyre!("executor account nonce is exhausted"))?
        } else {
            account_nonce
        };
        let authorization = Authorization {
            chain_id: U256::from(profile.chain_id()),
            address: profile.delegate(),
            nonce,
        };
        let signature = signer.sign_hash_sync(&authorization.signature_hash())?;
        transaction.authorization_list = Some(vec![authorization.into_signed(signature)]);
        transaction.transaction_type = Some(4);
    } else if !delegated {
        return Err(eyre!("executor has an unrecognized delegation"));
    }
    if transaction.from == Some(signer.address()) {
        transaction.nonce = inspection.account_nonce();
    }
    Ok(())
}

/// `request`'s gas estimate. A type-4 request is checked against RPCs that silently ignore
/// authorizations and simulate an empty account, which estimates only intrinsic and calldata
/// gas and leaves the executor's execution out of the limit.
pub(crate) async fn estimate_delegated_gas(
    provider: &impl Provider,
    request: &TransactionRequest,
) -> Result<u64> {
    let delegated = request.transaction_type == Some(4);
    if delegated {
        // Require the executor's getter to return ABI data through this delegation. The
        // getter takes no value.
        let nonce_request = request
            .clone()
            .with_value(U256::ZERO)
            .with_input(RelayAdapt7702::nonceCall {}.abi_encode());
        let nonce_result = provider
            .call(nonce_request)
            .pending()
            .await
            .wrap_err("simulate executor delegation")?;
        RelayAdapt7702::nonceCall::abi_decode_returns(&nonce_result)
            .wrap_err("RPC did not simulate executor delegation")?;
    }
    let estimated_gas = provider
        .estimate_gas(request.clone())
        .await
        .wrap_err("estimate gas")?;
    if delegated {
        // eth_call and eth_estimateGas can have different authorization support.
        // Check the estimate before a buffer can conceal an execution shortfall.
        provider
            .call(request.clone().with_gas_limit(estimated_gas))
            .pending()
            .await
            .wrap_err("simulate transaction at estimated gas")?;
    }
    Ok(estimated_gas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{WalletSettings, build_effective_chain_configs};

    #[test]
    fn unwrap_and_top_up_route_through_the_executor_or_are_unavailable() {
        let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        let accepted = chains.get(1).unwrap().clone();
        let profile = accepted.accepted_executor_profile().unwrap();
        let mut unaccepted = accepted.clone();
        unaccepted
            .railgun
            .as_mut()
            .unwrap()
            .deployment
            .relay_adapt_7702_contract = Address::repeat_byte(7);
        let mut disabled = accepted.clone();
        disabled.enabled = false;
        // (unwrap, top-up, sponsored, executor owner, chain, route when required)
        let cases = [
            (true, false, false, true, &accepted, Some(Ok(profile))),
            (false, true, false, true, &accepted, Some(Ok(profile))),
            (
                true,
                false,
                false,
                false,
                &accepted,
                Some(Err(ExecutorRouteUnavailable::NoExecutorOwner)),
            ),
            (
                false,
                true,
                false,
                true,
                &unaccepted,
                Some(Err(ExecutorRouteUnavailable::NoAcceptedProfile)),
            ),
            (
                true,
                false,
                false,
                true,
                &disabled,
                Some(Err(ExecutorRouteUnavailable::NoAcceptedProfile)),
            ),
            (true, true, true, false, &unaccepted, None),
            (false, false, false, true, &accepted, None),
        ];
        for (unwrap, top_up, sponsored, owner, chain, expected) in cases {
            let required = unshield_requires_executor(unwrap, top_up, sponsored);
            assert_eq!(required, expected.is_some());
            if let Some(route) = expected {
                assert_eq!(unshield_executor_route(owner, chain), route);
            }
        }
    }

    /// RPCs that ignore authorizations simulate an empty account: `eth_call` returns no data
    /// and `eth_estimateGas` leaves out the executor's execution. Neither estimate is used.
    #[tokio::test]
    async fn delegated_gas_rejects_rpcs_that_ignore_authorizations() {
        use alloy::primitives::Bytes;
        use alloy::providers::{Provider, ProviderBuilder};
        use alloy::transports::mock::Asserter;

        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        let delegated = TransactionRequest {
            transaction_type: Some(4),
            ..Default::default()
        };
        let nonce = Bytes::from(RelayAdapt7702::nonceCall::abi_encode_returns(&U256::ZERO));

        // eth_call ignores the authorization.
        responses.push_success(&Bytes::new());
        assert!(estimate_delegated_gas(&provider, &delegated).await.is_err());

        // eth_estimateGas ignores it, so the request fails at the estimate.
        responses.push_success(&nonce);
        responses.push_success(&"0x5352");
        responses.push_failure_msg("out of gas");
        assert!(estimate_delegated_gas(&provider, &delegated).await.is_err());

        responses.push_success(&nonce);
        responses.push_success(&"0xbea0");
        responses.push_success(&Bytes::new());
        assert_eq!(
            estimate_delegated_gas(&provider, &delegated).await.unwrap(),
            48_800
        );

        // Other requests are estimated as they are.
        responses.push_success(&"0x5208");
        assert_eq!(
            estimate_delegated_gas(&provider, &TransactionRequest::default())
                .await
                .unwrap(),
            21_000
        );
        assert!(responses.read_q().is_empty());
    }

    #[tokio::test]
    async fn delegation_authorization_moves_an_account_to_the_delivery_delegate() {
        use alloy::eips::BlockNumHash;
        use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
        use alloy::primitives::Bytes;
        use alloy::providers::{Provider, ProviderBuilder};
        use alloy::transports::mock::Asserter;

        let chain = build_effective_chain_configs(&WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
        let primary = chain.accepted_executor_profile().unwrap();
        let earlier = broadcaster_core::deployment::RailgunDeployment::for_chain(1)
            .unwrap()
            .relay_adapt_7702_delegates[0]
            .address;
        let earlier = ExecutorProfile::accepted(1, earlier).unwrap();
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(7)).unwrap();
        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        // (installed delegate, delivery's target, broadcaster delivery, authorized delegate)
        for (installed, target, broadcaster, authorized) in [
            // Self-broadcast moves an account on the earlier delegate to the primary.
            (earlier.delegate(), primary, false, Some(primary.delegate())),
            (primary.delegate(), primary, false, None),
            // A broadcaster advertising the earlier delegate moves the account to it.
            (primary.delegate(), earlier, true, Some(earlier.delegate())),
            (Address::repeat_byte(2), primary, false, None),
        ] {
            let listed = installed != Address::repeat_byte(2);
            responses.push_success(&"0x2");
            responses.push_success(&Bytes::from(
                [
                    EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                    installed.as_slice(),
                ]
                .concat(),
            ));
            responses.push_success(&"0x0");
            if listed {
                responses.push_success(&Bytes::from(
                    RelayAdapt7702::nonceCall::abi_encode_returns(&U256::ZERO),
                ));
            }
            let inspection = crate::desktop::executor_discovery::inspect_at(
                &provider,
                &chain,
                signer.address(),
                &[],
                BlockNumHash::new(10, B256::repeat_byte(10)),
                false,
            )
            .await;
            let mut transaction = TransactionRequest::default();
            let result = authorize_delegation(
                &mut transaction,
                target,
                &inspection,
                &signer,
                false,
                broadcaster,
            );
            // An unlisted delegation is never authorized away outside recovery.
            assert_eq!(result.is_ok(), listed);
            assert_eq!(
                transaction
                    .authorization_list
                    .map(|list| list[0].inner().address),
                authorized
            );
        }
    }
}
