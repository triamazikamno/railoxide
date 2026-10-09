use std::time::Duration;

use alloy::eips::BlockId;
use alloy::primitives::ruint::UintTryFrom;
use alloy::primitives::{Address, U256, Uint};
use alloy::providers::Provider as _;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::railgun::{Call, ShieldRequest, TokenData, shieldCall};
use broadcaster_core::contracts::shield::build_shield_request;
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, eyre};
use railgun_wallet::tx::RailgunGasModel;
use zeroize::Zeroizing;

use super::{ExecutorDelivery, ExecutorOwner};
use crate::desktop::executor_discovery::{
    ExecutorErc721, executor_delegation, inspect_recovery_executor,
};
use crate::public_wallet::{PublicErc20, public_native_action_gas_units_with_buffer};
use crate::settings::ExecutorProfile;
use crate::vault::{ExecutorOperationId, ExecutorRecord, SwapUseId};
use crate::{
    DesktopPrivateSpendAuthorization, ExecutorAsset, ExecutorInspection,
    PublicActionGasFeeSelection, PublicActionProgressStep, PublicBroadcasterCandidate,
};

mod approval;
mod batch;
mod output;
mod paid;
pub use approval::ExecutorRecoveryApproval;
pub(super) use output::is_shield_of;
pub use output::{
    ExecutorRecoveryCompletion, executor_recovered_since, executor_recovered_token_since,
    executor_recovery_completion, executor_recovery_remaining_amount,
};
pub use paid::{
    ExecutorPaidRecoveryOutcome, ExecutorPaidRecoveryRequest, ExecutorPrivateFeeLimitExceeded,
    ExecutorRecoveryFeeEstimate,
};
pub(super) use paid::{PaidExecutionPurpose, require_private_fee_limit};

/// Describe retained recovery calls for history display, without authorizing execution.
#[must_use]
pub fn executor_payload_recovery_steps(
    payload: &crate::vault::IssuedExecutorPayload,
) -> Vec<PublicActionProgressStep> {
    use broadcaster_core::contracts::railgun::{RelayAdapt7702, approveCall};

    if payload.purpose() != crate::vault::ExecutorPayloadPurpose::Recovery {
        return Vec::new();
    }
    let data = payload.context().calldata();
    let calls = if let Ok(call) = RelayAdapt7702::executeCall::abi_decode(data) {
        call._actionData.calls
    } else if let Ok(call) = RelayAdapt7702::multicallCall::abi_decode(data) {
        call._calls
    } else {
        return Vec::new();
    };
    let mut steps = Vec::new();
    for call in calls {
        let step =
            if crate::walletconnect::WrappedNative::depositCall::abi_decode(&call.data).is_ok() {
                PublicActionProgressStep::Wrap
            } else if approveCall::abi_decode(&call.data).is_ok() {
                // ERC-20 and ERC-721 approval share this ABI and the same display purpose.
                PublicActionProgressStep::Approve
            } else if shieldCall::abi_decode(&call.data)
                .is_ok_and(|call| !call._shieldRequests.is_empty())
            {
                PublicActionProgressStep::Shield
            } else {
                continue;
            };
        if !steps.contains(&step) {
            steps.push(step);
        }
    }
    steps
}

/// Recovery always keeps its selected payer. Builder sponsorship is not a recovery route.
#[derive(Clone)]
pub enum ExecutorRecoveryFunding {
    ExecutorNative {
        gas_fee: PublicActionGasFeeSelection,
    },
    PublicBroadcaster {
        candidate: Box<PublicBroadcasterCandidate>,
        maximum_private_fee: U256,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorRecoveryExecution {
    SignedMulticall { nonce: U256 },
    PaidExecute { nonce: U256 },
}

/// A recovery batch the account sent itself. The receipt is the send path's own and says
/// at the time whether the transaction reverted; it is not recorded as an outcome.
pub struct ExecutorRecoveryBatchOutcome {
    pub receipt: crate::TxReceiptOutput,
    pub completion: ExecutorRecoveryCompletion,
}

/// Exact calls and fee ceilings for a native read-only review. No signed data is exposed.
pub struct PreparedExecutorRecovery {
    operation: ExecutorOperationId,
    recovery: ExecutorOperationId,
    pub(super) expected_active_use: Option<SwapUseId>,
    generation: u64,
    owner: tokio::sync::watch::Sender<bool>,
    source: Address,
    delegate: Address,
    current_delegate: Option<Address>,
    account_nonce: u64,
    recipient: String,
    asset: ExecutorAsset,
    amount: U256,
    funding: ExecutorRecoveryFunding,
    execution: ExecutorRecoveryExecution,
    calls: Vec<Call>,
    steps: Vec<PublicActionProgressStep>,
    gas_limits: Vec<u64>,
    maximum_native_fee: U256,
    shield: Option<ShieldRequest>,
}

impl PreparedExecutorRecovery {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }
    #[must_use]
    pub const fn recovery(&self) -> ExecutorOperationId {
        self.recovery
    }
    #[must_use]
    pub const fn source(&self) -> Address {
        self.source
    }
    #[must_use]
    pub const fn delegate(&self) -> Address {
        self.delegate
    }
    #[must_use]
    pub const fn current_delegate(&self) -> Option<Address> {
        self.current_delegate
    }
    #[must_use]
    pub fn changes_delegation(&self) -> bool {
        self.current_delegate != Some(self.delegate)
    }

    pub(super) fn validate_inspection(&self, inspection: &ExecutorInspection) -> Result<()> {
        let code = inspection
            .code()
            .ok_or_else(|| eyre!("recovery delegation is unavailable"))?;
        if inspection.account_nonce() != Some(self.account_nonce)
            || !code.is_empty() && executor_delegation(code).is_none()
            || executor_delegation(code) != self.current_delegate
        {
            return Err(eyre!(
                "account nonce or delegation changed; review recovery again"
            ));
        }
        Ok(())
    }
    #[must_use]
    pub fn recipient(&self) -> &str {
        &self.recipient
    }
    /// For a swap's early cancellation, the sell token, with a zero amount.
    #[must_use]
    pub const fn asset(&self) -> ExecutorAsset {
        self.asset
    }
    #[must_use]
    pub const fn amount(&self) -> U256 {
        self.amount
    }
    #[must_use]
    pub const fn funding(&self) -> &ExecutorRecoveryFunding {
        &self.funding
    }
    #[must_use]
    pub const fn execution(&self) -> ExecutorRecoveryExecution {
        self.execution
    }
    #[must_use]
    pub fn steps(&self) -> &[PublicActionProgressStep] {
        &self.steps
    }
    #[must_use]
    pub fn gas_limits(&self) -> &[u64] {
        &self.gas_limits
    }
    #[must_use]
    pub const fn maximum_native_fee(&self) -> U256 {
        self.maximum_native_fee
    }
    #[must_use]
    pub fn calls(&self) -> &[Call] {
        &self.calls
    }
    /// `None` for a swap's early cancellation, which recovers no asset.
    #[must_use]
    pub const fn shield(&self) -> Option<&ShieldRequest> {
        self.shield.as_ref()
    }
}

impl ExecutorOwner {
    /// Prepare one explicitly selected asset in a retained account. This never reserves an index.
    pub async fn prepare_recovery(
        &self,
        operation: ExecutorOperationId,
        asset: ExecutorAsset,
        amount: U256,
        funding: ExecutorRecoveryFunding,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<PreparedExecutorRecovery> {
        self.prepare_recovery_amount(
            operation,
            Some(asset),
            amount,
            funding,
            authorization,
            false,
        )
        .await
    }

    /// Prepare at most the reviewed native amount, retaining a conservative gas
    /// reserve from the fresh balance. The resulting exact plan still needs review.
    pub async fn prepare_native_recovery_up_to(
        &self,
        operation: ExecutorOperationId,
        maximum_amount: U256,
        funding: ExecutorRecoveryFunding,
        authorization: &DesktopPrivateSpendAuthorization,
    ) -> Result<PreparedExecutorRecovery> {
        if !matches!(funding, ExecutorRecoveryFunding::ExecutorNative { .. }) {
            return Err(eyre!(
                "reserving native gas requires account-funded recovery"
            ));
        }
        self.prepare_recovery_amount(
            operation,
            Some(ExecutorAsset::Native),
            maximum_amount,
            funding,
            authorization,
            true,
        )
        .await
    }

    /// `asset` is `None` only for a swap's early cancellation, which recovers no asset and
    /// is always broadcaster-funded.
    pub(super) async fn prepare_recovery_amount(
        &self,
        operation: ExecutorOperationId,
        asset: Option<ExecutorAsset>,
        mut amount: U256,
        funding: ExecutorRecoveryFunding,
        authorization: &DesktopPrivateSpendAuthorization,
        reserve_native_gas: bool,
    ) -> Result<PreparedExecutorRecovery> {
        self.ensure_active()?;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        let record = self.recovery_record(operation)?;
        let source = record
            .address()
            .ok_or_else(|| eyre!("derive this historical account before recovery"))?;
        let profile = ExecutorProfile::accepted(self.chain.chain_id, record.delegate())
            .ok_or_else(|| eyre!("this historical account has no supported recovery profile"))?;
        if let ExecutorRecoveryFunding::PublicBroadcaster {
            candidate,
            maximum_private_fee,
        } = &funding
        {
            ExecutorDelivery::PublicBroadcaster(candidate.clone()).admit(profile)?;
            if maximum_private_fee.is_zero() {
                return Err(eyre!("review a private fee limit for broadcaster recovery"));
            }
        }
        // A cancellation inspects the sell token, which the executor holds none of before its
        // pre-hook runs.
        let inspected = match (asset, record.swap()) {
            (Some(asset), _) => asset,
            (None, Some(swap))
                if matches!(funding, ExecutorRecoveryFunding::PublicBroadcaster { .. }) =>
            {
                ExecutorAsset::Erc20(swap.terms().sell_token())
            }
            (None, _) => {
                return Err(eyre!(
                    "early cancellation needs a swap order and broadcaster funding"
                ));
            }
        };
        let value = asset
            .map(|asset| -> Result<Uint<120, 2>> {
                let value = Uint::<120, 2>::uint_try_from(amount)
                    .map_err(|_| eyre!("recovery amount exceeds the shield value range"))?;
                if amount.is_zero()
                    || matches!(asset, ExecutorAsset::Erc721 { .. }) && amount != U256::ONE
                {
                    return Err(eyre!("select a positive token amount or exactly one NFT"));
                }
                Ok(value)
            })
            .transpose()?;
        let signer = self.authorized_executor_signer(
            authorization,
            &super::HardwareExecutorAction::Recover(record.operation()),
            record.operation(),
            record.index(),
        )?;
        if signer.address() != source {
            return Err(eyre!(
                "recovery signer does not match the historical account"
            ));
        }
        // Reading a recorded accepted profile also works while new allocation is disabled.
        let mut chain = self
            .chain_for_delegate(record.delegate())
            .ok_or_else(|| eyre!("chain does not support Railgun"))?;
        chain.enabled = true;
        let inspection = self
            .while_active(inspect_recovery_executor(
                &chain,
                &self.http,
                source,
                &[inspected],
            ))
            .await?;
        let code = inspection
            .code()
            .ok_or_else(|| eyre!("executor delegation is unavailable"))?;
        let current_delegate = executor_delegation(code);
        if !code.is_empty() && current_delegate.is_none() {
            return Err(eyre!(
                "recovery requires an externally owned account or an EIP-7702 delegation"
            ));
        }
        let nonce = inspection
            .execution_nonce()
            .ok_or_else(|| eyre!("recovery execution nonce is unknown"))?;
        if record.records_future_nonce(nonce) {
            return Err(eyre!(
                "recorded execution state is ahead of the current chain; refresh recovery after confirmation"
            ));
        }
        let account_nonce = inspection
            .account_nonce()
            .ok_or_else(|| eyre!("executor account nonce is unavailable"))?;
        let execution = match funding {
            ExecutorRecoveryFunding::ExecutorNative { .. } => {
                ExecutorRecoveryExecution::SignedMulticall { nonce }
            }
            ExecutorRecoveryFunding::PublicBroadcaster { .. } => {
                ExecutorRecoveryExecution::PaidExecute { nonce }
            }
        };
        // A swap executor's orders become unfillable in the same batch that recovers assets.
        let swap_calls = self
            .swap_recovery_preparation(&chain, &record, &inspection, nonce, asset.is_none())
            .await?;
        let recipient = self.view.receive_address()?;
        let buffer = self.chain.gas.gas_limit_buffer;
        let gas_model = RailgunGasModel::for_chain(self.chain.chain_id);
        let (steps, calls, gas_limits, shield) = if let Some((asset, value)) = asset.zip(value) {
            let railgun = self.chain.require_railgun()?.deployment.contract;
            let token = recovery_token(asset, self.chain.wrapped_native_token)?;
            let allowance = self
                .while_active(recovery_allowance(
                    &chain,
                    &self.http,
                    &inspection,
                    &token,
                    railgun,
                ))
                .await?;
            let address = broadcaster_core::crypto::railgun::Address::from(recipient.as_str());
            let recipient_data =
                broadcaster_core::crypto::railgun::AddressData::try_from(&address)?;
            let secret = Zeroizing::new(signer.to_bytes().0);
            let shield_key = Zeroizing::new(
                broadcaster_core::contracts::shield::derive_shield_private_key(&secret)?,
            );
            let mut shield = build_shield_request(
                recipient_data.master_public_key,
                &recipient_data.viewing_public_key,
                token.clone(),
                value,
                &shield_key,
            )?;
            drop(signer);
            let (mut steps, mut calls) = with_swap_preparation(
                &swap_calls,
                recovery_calls(asset, amount, allowance, &shield, source),
            );
            let mut gas_limits = recovery_gas_limits(gas_model, &steps, buffer);
            if reserve_native_gas {
                let reserve = recovery_funding_admission(
                    &inspection,
                    asset,
                    U256::ZERO,
                    &funding,
                    &gas_limits,
                )?;
                let native = inspection
                    .balances()
                    .get(&ExecutorAsset::Native)
                    .copied()
                    .flatten()
                    .ok_or_else(|| eyre!("native balance is unavailable"))?;
                amount = native_recovery_remainder(amount, native, reserve)?;
                shield = build_shield_request(
                    recipient_data.master_public_key,
                    &recipient_data.viewing_public_key,
                    token,
                    Uint::<120, 2>::uint_try_from(amount)
                        .map_err(|_| eyre!("recovery amount exceeds the shield value range"))?,
                    &shield_key,
                )?;
                (steps, calls) = with_swap_preparation(
                    &swap_calls,
                    recovery_calls(asset, amount, allowance, &shield, source),
                );
                // Lowering the amount can remove an approval, never add a step. Keep
                // the conservative remainder and review the final plan's gas ceiling.
                gas_limits = recovery_gas_limits(gas_model, &steps, buffer);
            }
            (steps, calls, gas_limits, Some(shield))
        } else {
            drop(signer);
            let (steps, calls) = with_swap_preparation(&swap_calls, (Vec::new(), Vec::new()));
            let gas_limits = recovery_gas_limits(gas_model, &steps, buffer);
            (steps, calls, gas_limits, None)
        };
        let maximum_native_fee =
            recovery_funding_admission(&inspection, inspected, amount, &funding, &gas_limits)?;
        self.ensure_active()?;
        // An admitted recovery of an asset stops the account's swap use: no further setup,
        // order or shield is issued for it on either chain. An early cancellation recovers no
        // asset and leaves its swap free to retry.
        let record = if asset.is_some() {
            let record = self.store.stop_swap_use(operation)?;
            self.notify_change();
            record
        } else {
            self.recovery_record(operation)?
        };
        let recovery = ExecutorOperationId::random()?;
        if let DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) = authorization {
            hardware.bind_recovery(recovery)?;
        }
        Ok(PreparedExecutorRecovery {
            operation,
            recovery,
            expected_active_use: record.active_swap_use(),
            generation: self.generation,
            owner: self.closed.clone(),
            source,
            delegate: profile.delegate(),
            current_delegate,
            account_nonce,
            recipient,
            asset: inspected,
            amount,
            funding,
            execution,
            calls,
            steps,
            gas_limits,
            maximum_native_fee,
            shield,
        })
    }

    pub fn validate_recovery(&self, prepared: &PreparedExecutorRecovery) -> Result<ExecutorRecord> {
        self.ensure_active()?;
        if !self.closed.same_channel(&prepared.owner) || self.generation != prepared.generation {
            return Err(eyre!("recovery belongs to an inactive wallet session"));
        }
        let record = self.recovery_record(prepared.operation)?;
        if record.active_swap_use() != prepared.expected_active_use {
            return Err(eyre!("account swap claim changed; review recovery again"));
        }
        if record.address() != Some(prepared.source)
            || record.delegate() != prepared.delegate
            || self.view.receive_address()? != prepared.recipient
        {
            return Err(eyre!("recovery source or private destination changed"));
        }
        if let ExecutorRecoveryFunding::PublicBroadcaster { candidate, .. } = &prepared.funding {
            let profile = ExecutorProfile::accepted(self.chain.chain_id, record.delegate())
                .ok_or_else(|| eyre!("historical executor profile is unavailable"))?;
            ExecutorDelivery::PublicBroadcaster(candidate.clone()).admit(profile)?;
        }
        Ok(record)
    }

    fn recovery_record(&self, operation: ExecutorOperationId) -> Result<ExecutorRecord> {
        self.store
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
            .ok_or_else(|| eyre!("historical executor record is unavailable"))
    }
}

fn recovery_token(asset: ExecutorAsset, wrapped: Option<Address>) -> Result<TokenData> {
    Ok(match asset {
        ExecutorAsset::Native => TokenData::erc20(
            wrapped.ok_or_else(|| eyre!("native shielding is unavailable on this chain"))?,
        ),
        ExecutorAsset::Erc20(token) => TokenData::erc20(token),
        ExecutorAsset::Erc721 {
            collection,
            token_id,
        } => TokenData {
            tokenType: 1,
            tokenAddress: collection,
            tokenSubID: token_id,
        },
    })
}

pub(super) async fn recovery_allowance(
    chain: &crate::settings::EffectiveChainConfig,
    http: &crate::HttpContext,
    inspection: &ExecutorInspection,
    token: &TokenData,
    railgun: Address,
) -> Result<U256> {
    let data = if token.tokenType == 0 {
        PublicErc20::allowanceCall {
            owner: inspection.address(),
            spender: railgun,
        }
        .abi_encode()
    } else {
        ExecutorErc721::getApprovedCall {
            tokenId: token.tokenSubID,
        }
        .abi_encode()
    };
    let pool = QueryRpcPool::with_http_client(
        chain.rpc_route.endpoint_urls(),
        Duration::from_secs(30),
        http.rpc_client.clone(),
    );
    for endpoint in pool.available_providers() {
        let provider = &endpoint.provider;
        if provider.get_chain_id().await.ok() != Some(chain.chain_id) {
            continue;
        }
        let request = TransactionRequest::default()
            .to(token.tokenAddress)
            .input(data.clone().into());
        let Ok(result) = provider
            .call(request)
            .block(BlockId::hash_canonical(inspection.block().hash))
            .await
        else {
            continue;
        };
        if token.tokenType == 0 {
            if let Ok(allowance) = PublicErc20::allowanceCall::abi_decode_returns(&result) {
                return Ok(allowance);
            }
        } else if let Ok(approved) = ExecutorErc721::getApprovedCall::abi_decode_returns(&result) {
            return Ok(if approved == railgun {
                U256::ONE
            } else {
                U256::ZERO
            });
        }
    }
    Err(eyre!(
        "asset approval state is unavailable; refresh before recovery"
    ))
}

/// The asset steps and calls of an atomic recovery batch that `executor` runs on itself.
fn recovery_calls(
    asset: ExecutorAsset,
    amount: U256,
    allowance: U256,
    shield: &ShieldRequest,
    executor: Address,
) -> (Vec<PublicActionProgressStep>, Vec<Call>) {
    let token = &shield.preimage.token;
    let mut steps = Vec::new();
    let mut calls = Vec::new();
    if asset == ExecutorAsset::Native {
        steps.push(PublicActionProgressStep::Wrap);
        calls.push(Call {
            to: token.tokenAddress,
            data: crate::walletconnect::WrappedNative::depositCall {}
                .abi_encode()
                .into(),
            value: amount,
        });
    }
    // Multicall forbids direct Railgun calls. Its self-only shield helper
    // performs the approval itself, including an ERC-20 reset when required.
    if token.tokenType == 0 && !allowance.is_zero() {
        steps.push(PublicActionProgressStep::Approve);
    }
    steps.push(PublicActionProgressStep::Approve);
    steps.push(PublicActionProgressStep::Shield);
    calls.push(Call {
        to: executor,
        data: shieldCall {
            _shieldRequests: vec![shield.clone()],
        }
        .abi_encode()
        .into(),
        value: U256::ZERO,
    });
    (steps, calls)
}

/// A swap executor's batch first runs the calls that make its orders unfillable and reset the
/// settlement approval. Each is priced like an approval, a single storage write.
fn with_swap_preparation(
    swap_calls: &[Call],
    (steps, calls): (Vec<PublicActionProgressStep>, Vec<Call>),
) -> (Vec<PublicActionProgressStep>, Vec<Call>) {
    let mut batch_steps = vec![PublicActionProgressStep::Approve; swap_calls.len()];
    batch_steps.extend(steps);
    let mut batch_calls = swap_calls.to_vec();
    batch_calls.extend(calls);
    (batch_steps, batch_calls)
}

pub(super) fn recovery_gas_limits(
    model: &RailgunGasModel,
    steps: &[PublicActionProgressStep],
    buffer: u64,
) -> Vec<u64> {
    // Same execution overhead used by private executor quotes. Final RPC estimation must
    // fit this reviewed ceiling; dependent calls cannot be estimated alone.
    vec![steps.iter().fold(model.executor(), |sum, step| {
        sum.saturating_add(public_native_action_gas_units_with_buffer(
            model,
            &[*step],
            buffer,
        ))
    })]
}

/// `swap_calls` bounds the calls a swap executor's batch runs before its asset steps.
fn maximum_recovery_gas_limit(
    model: &RailgunGasModel,
    asset: ExecutorAsset,
    swap_calls: usize,
    buffer: u64,
) -> u64 {
    let mut steps = vec![PublicActionProgressStep::Approve; swap_calls];
    if asset == ExecutorAsset::Native {
        steps.push(PublicActionProgressStep::Wrap);
    }
    if !matches!(asset, ExecutorAsset::Erc721 { .. }) {
        steps.push(PublicActionProgressStep::Approve);
    }
    steps.extend([
        PublicActionProgressStep::Approve,
        PublicActionProgressStep::Shield,
    ]);
    recovery_gas_limits(model, &steps, buffer)[0]
}

fn native_recovery_remainder(maximum: U256, balance: U256, reserve: U256) -> Result<U256> {
    let amount = maximum.min(balance.saturating_sub(reserve));
    if amount.is_zero() {
        return Err(eyre!(
            "no native amount remains after reserving recovery gas"
        ));
    }
    Ok(amount)
}

pub(super) fn recovery_funding_admission(
    inspection: &ExecutorInspection,
    asset: ExecutorAsset,
    amount: U256,
    funding: &ExecutorRecoveryFunding,
    gas_limits: &[u64],
) -> Result<U256> {
    let balance = inspection
        .balances()
        .get(&asset)
        .copied()
        .flatten()
        .ok_or_else(|| eyre!("selected asset balance is unknown; refresh before recovery"))?;
    if balance < amount {
        return Err(eyre!(
            "selected executor has insufficient assets for this recovery"
        ));
    }
    let ExecutorRecoveryFunding::ExecutorNative { gas_fee } = funding else {
        return Ok(U256::ZERO);
    };
    let PublicActionGasFeeSelection::Custom {
        max_fee_per_gas,
        max_priority_fee_per_gas,
    } = gas_fee
    else {
        return Err(eyre!(
            "resolve and review fixed gas fees before executor-funded recovery"
        ));
    };
    if *max_fee_per_gas == 0 || max_priority_fee_per_gas > max_fee_per_gas {
        return Err(eyre!("executor recovery gas fees are invalid"));
    }
    let gas = gas_limits
        .iter()
        .fold(U256::ZERO, |sum, limit| sum + U256::from(*limit));
    let reserve = gas * U256::from(*max_fee_per_gas);
    let native = inspection
        .balances()
        .get(&ExecutorAsset::Native)
        .copied()
        .flatten()
        .ok_or_else(|| eyre!("executor native gas balance is unknown"))?;
    let spend = if asset == ExecutorAsset::Native {
        amount
    } else {
        U256::ZERO
    };
    if native < reserve || native - reserve < spend {
        return Err(eyre!(
            "insufficient executor native funds after reserving recovery gas; fund this account's native balance or review recovery using private fee funds"
        ));
    }
    Ok(reserve)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::executor_discovery::inspect_at;
    use crate::settings::{WalletSettings, build_effective_chain_configs};
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{B256, Bytes};
    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;
    use broadcaster_core::contracts::railgun::{CommitmentPreimage, ShieldCiphertext};

    fn shield_fixture(token: TokenData, value: U256) -> ShieldRequest {
        ShieldRequest {
            preimage: CommitmentPreimage {
                npk: B256::repeat_byte(3),
                token,
                value: Uint::from(value),
            },
            ciphertext: ShieldCiphertext {
                encryptedBundle: [B256::ZERO; 3],
                shieldKey: B256::ZERO,
            },
        }
    }

    #[tokio::test]
    async fn executor_native_recovery_reserves_all_steps_and_never_wraps_the_reserve() {
        let chain = build_effective_chain_configs(&WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
        let source = Address::repeat_byte(1);
        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        let amount = U256::from(1_000_000);
        let shield = shield_fixture(TokenData::erc20(Address::repeat_byte(2)), amount);
        let (steps, calls) =
            recovery_calls(ExecutorAsset::Native, amount, U256::ONE, &shield, source);
        let gas_model = RailgunGasModel::for_chain(chain.chain_id);
        let limits = recovery_gas_limits(gas_model, &steps, chain.gas.gas_limit_buffer);
        let reserve = U256::from(limits[0]) * U256::from(2);
        responses.push_success(&"0x7");
        // Delegation alone does not make an executor unable to originate transactions.
        responses.push_success(&Bytes::from_static(b"unknown delegate"));
        responses.push_success(&(amount + reserve));
        let inspection = inspect_at(
            &provider,
            &chain,
            source,
            &[],
            BlockNumHash::new(10, B256::repeat_byte(10)),
            false,
        )
        .await;
        let funding = ExecutorRecoveryFunding::ExecutorNative {
            gas_fee: PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 2,
                max_priority_fee_per_gas: 1,
            },
        };
        assert_eq!(
            recovery_funding_admission(
                &inspection,
                ExecutorAsset::Native,
                amount,
                &funding,
                &limits
            )
            .unwrap(),
            reserve
        );
        assert!(
            recovery_funding_admission(
                &inspection,
                ExecutorAsset::Native,
                amount + U256::ONE,
                &funding,
                &limits
            )
            .is_err()
        );
        assert_eq!(
            native_recovery_remainder(amount + reserve, amount + reserve, reserve).unwrap(),
            amount
        );
        assert_eq!(
            native_recovery_remainder(amount / U256::from(2), amount + reserve, reserve).unwrap(),
            amount / U256::from(2)
        );
        assert!(native_recovery_remainder(amount, reserve, reserve).is_err());
        // The batch wraps the amount and shields it through the account's own helper,
        // which approves by itself.
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].value, amount);
        assert_eq!(calls[1].to, source);
        assert_eq!(
            shieldCall::abi_decode(&calls[1].data)
                .unwrap()
                ._shieldRequests[0]
                .preimage
                .value,
            Uint::from(amount)
        );
    }

    #[test]
    fn recovery_history_distinguishes_preparation_from_shielding() {
        use crate::vault::{
            ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
            IssuedExecutorPayload,
        };
        use alloy::{eips::BlockNumHash, primitives::B256};
        use broadcaster_core::contracts::railgun::RelayAdapt7702;
        use broadcaster_core::contracts::shield::build_approve_calldata;

        let amount = U256::from(10);
        let token = Address::repeat_byte(2);
        let shield = shield_fixture(TokenData::erc20(token), amount);
        // A swap account's batch first resets its sell token's approval.
        let swap_calls = [Call {
            to: token,
            data: build_approve_calldata(Address::repeat_byte(3), U256::ZERO).into(),
            value: U256::ZERO,
        }];
        let assets = recovery_calls(
            ExecutorAsset::Native,
            amount,
            U256::ZERO,
            &shield,
            Address::repeat_byte(1),
        );
        for (batch, expected) in [
            // Nothing is left to shield, so the batch only prepares the swap account.
            (
                (Vec::new(), Vec::new()),
                vec![PublicActionProgressStep::Approve],
            ),
            (
                assets,
                vec![
                    PublicActionProgressStep::Approve,
                    PublicActionProgressStep::Wrap,
                    PublicActionProgressStep::Shield,
                ],
            ),
        ] {
            let (_, calls) = with_swap_preparation(&swap_calls, batch);
            let calldata = RelayAdapt7702::multicallCall {
                _requireSuccess: true,
                _calls: calls,
                _nonce: U256::ZERO,
                _signature: Bytes::new(),
            }
            .abi_encode();
            let payload = IssuedExecutorPayload::new(
                U256::ZERO,
                Address::ZERO,
                B256::ZERO,
                ExecutorPayloadPurpose::Recovery,
                ExecutorPayloadContext::new(
                    calldata.into(),
                    ExecutorNonceObservation::new(BlockNumHash::default(), U256::ZERO),
                    Vec::new(),
                ),
            );
            assert_eq!(executor_payload_recovery_steps(&payload), expected);
        }
    }
}
