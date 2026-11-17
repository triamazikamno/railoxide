use alloy::primitives::{Address, U256};
use eyre::{Result, eyre};
use railgun_wallet::tx::RailgunGasModel;

use super::{
    ExecutorAsset, ExecutorOperationId, ExecutorOwner, ExecutorRecord, ExecutorRecoveryExecution,
    ExecutorRecoveryFunding, PreparedExecutorRecovery, PublicActionGasFeeSelection,
    maximum_recovery_gas_limit,
};
use crate::desktop::executors::swap::swap_recovery_call_bound;

/// Terms shown before recovery preparation. Possessing these terms does not authorize signing.
#[derive(Clone)]
pub struct ExecutorRecoveryApproval {
    operation: ExecutorOperationId,
    source: Address,
    recipient: String,
    asset: ExecutorAsset,
    amount: U256,
    funding: ExecutorRecoveryFunding,
    maximum_native_fee: U256,
    competing_payloads: bool,
}

impl ExecutorOwner {
    /// Build the initial review from local state, without deriving keys or making account reads.
    pub fn recovery_approval(
        &self,
        operation: ExecutorOperationId,
        asset: ExecutorAsset,
        amount: U256,
        funding: ExecutorRecoveryFunding,
    ) -> Result<ExecutorRecoveryApproval> {
        self.ensure_active()?;
        let record = self.recovery_record(operation)?;
        let source = record
            .address()
            .ok_or_else(|| eyre!("derive this historical account before recovery"))?;
        let maximum_native_fee = match &funding {
            ExecutorRecoveryFunding::ExecutorNative {
                gas_fee:
                    PublicActionGasFeeSelection::Custom {
                        max_fee_per_gas,
                        max_priority_fee_per_gas,
                    },
            } if *max_fee_per_gas > 0 && max_priority_fee_per_gas <= max_fee_per_gas => {
                U256::from(maximum_recovery_gas_limit(
                    RailgunGasModel::for_chain(self.chain.chain_id),
                    asset,
                    swap_recovery_call_bound(&record),
                    self.chain.gas.gas_limit_buffer,
                )) * U256::from(*max_fee_per_gas)
            }
            ExecutorRecoveryFunding::ExecutorNative { .. } => {
                return Err(eyre!("review valid fixed gas fees before recovery"));
            }
            ExecutorRecoveryFunding::PublicBroadcaster { .. } => U256::ZERO,
        };
        Ok(ExecutorRecoveryApproval {
            operation,
            source,
            recipient: self.view.receive_address()?,
            asset,
            amount,
            funding,
            maximum_native_fee,
            competing_payloads: record.has_competing_payloads(),
        })
    }
}

impl ExecutorRecoveryApproval {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }
    #[must_use]
    pub const fn source(&self) -> Address {
        self.source
    }
    #[must_use]
    pub fn recipient(&self) -> &str {
        &self.recipient
    }
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
    pub const fn maximum_native_fee(&self) -> U256 {
        self.maximum_native_fee
    }
    #[must_use]
    pub const fn has_competing_payloads(&self) -> bool {
        self.competing_payloads
    }

    /// Whether a freshly validated plan fits the terms already presented to the user.
    /// New delegation consequences always need an explicit review.
    #[must_use]
    pub fn covers(&self, prepared: &PreparedExecutorRecovery, record: &ExecutorRecord) -> bool {
        let (ExecutorRecoveryExecution::SignedMulticall { nonce }
        | ExecutorRecoveryExecution::PaidExecute { nonce }) = prepared.execution;
        let competing = record
            .issued()
            .iter()
            .any(|issued| record.is_outstanding_at(issued, nonce));
        record.operation() == self.operation
            && self.covers_terms(prepared)
            && (!competing || self.competing_payloads)
    }

    fn covers_terms(&self, prepared: &PreparedExecutorRecovery) -> bool {
        if self.operation != prepared.operation
            || self.source != prepared.source
            || self.recipient != prepared.recipient
            || self.asset != prepared.asset
            || prepared.changes_delegation()
            || prepared.maximum_native_fee > self.maximum_native_fee
        {
            return false;
        }
        let native_up_to = self.asset == ExecutorAsset::Native
            && matches!(self.funding, ExecutorRecoveryFunding::ExecutorNative { .. });
        if prepared.amount.is_zero()
            || if native_up_to {
                prepared.amount > self.amount
            } else {
                prepared.amount != self.amount
            }
        {
            return false;
        }
        match (&self.funding, &prepared.funding) {
            (
                ExecutorRecoveryFunding::ExecutorNative {
                    gas_fee:
                        PublicActionGasFeeSelection::Custom {
                            max_fee_per_gas: approved_max,
                            max_priority_fee_per_gas: approved_tip,
                        },
                },
                ExecutorRecoveryFunding::ExecutorNative {
                    gas_fee:
                        PublicActionGasFeeSelection::Custom {
                            max_fee_per_gas: prepared_max,
                            max_priority_fee_per_gas: prepared_tip,
                        },
                },
            ) => prepared_max <= approved_max && prepared_tip <= approved_tip,
            (
                ExecutorRecoveryFunding::PublicBroadcaster {
                    candidate: approved,
                    maximum_private_fee: limit,
                },
                ExecutorRecoveryFunding::PublicBroadcaster {
                    candidate: prepared,
                    maximum_private_fee: fee,
                },
            ) => {
                fee <= limit
                    && approved.chain_id == prepared.chain_id
                    && approved.railgun_address == prepared.railgun_address
                    && approved.token == prepared.token
                    && approved.fees_id == prepared.fees_id
                    && approved.fee == prepared.fee
                    && approved.fee_expiration == prepared.fee_expiration
                    && approved.viewing_public_key == prepared.viewing_public_key
                    && approved.address_data.master_public_key
                        == prepared.address_data.master_public_key
                    && approved.address_data.viewing_public_key
                        == prepared.address_data.viewing_public_key
                    && approved.required_poi_list_keys == prepared.required_poi_list_keys
                    && approved.relay_adapt == prepared.relay_adapt
                    && approved.relay_adapt_7702 == prepared.relay_adapt_7702
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests;
