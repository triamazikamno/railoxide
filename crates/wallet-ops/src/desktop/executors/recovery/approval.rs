use alloy::primitives::{Address, U256};
use eyre::{Result, eyre};

use super::{
    ExecutorAsset, ExecutorOperationId, ExecutorOwner, ExecutorRecord, ExecutorRecoveryExecution,
    ExecutorRecoveryFunding, PreparedExecutorRecovery, PublicActionGasFeeSelection,
    maximum_recovery_gas_limit,
};
use crate::vault::ExecutorPayloadStatus;

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
                    asset,
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
            competing_payloads: record.issued().iter().any(|issued| {
                !matches!(
                    record.payload_status(issued.hash()),
                    Some(
                        ExecutorPayloadStatus::Executed | ExecutorPayloadStatus::Invalidated { .. }
                    )
                )
            }),
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
    /// New delegation or replacement consequences always need an explicit review.
    #[must_use]
    pub fn covers(&self, prepared: &PreparedExecutorRecovery, record: &ExecutorRecord) -> bool {
        let competing = match prepared.execution {
            ExecutorRecoveryExecution::Ordinary => return false,
            ExecutorRecoveryExecution::SignedMulticall { nonce }
            | ExecutorRecoveryExecution::PaidExecute { nonce } => {
                record.issued().iter().any(|issued| issued.nonce() == nonce)
            }
        };
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
            || prepared.replacement_nonce.is_some()
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
mod tests {
    use super::*;
    use crate::PublicBroadcasterCandidate;
    use crate::vault::{
        ExecutorDerivationScheme, ExecutorNonceObservation, ExecutorPayloadContext,
        ExecutorPayloadPurpose, ExecutorRecordOrigin, IssuedExecutorPayload,
    };
    use alloy::{
        eips::BlockNumHash,
        primitives::{B256, Bytes},
    };
    use broadcaster_core::contracts::railgun::{
        CommitmentPreimage, ShieldCiphertext, ShieldRequest, TokenData,
    };
    use broadcaster_core::crypto::railgun::AddressData;
    use std::time::Duration;

    fn fixture(
        funding: ExecutorRecoveryFunding,
    ) -> (
        ExecutorRecoveryApproval,
        PreparedExecutorRecovery,
        ExecutorRecord,
    ) {
        let approval = ExecutorRecoveryApproval {
            operation: ExecutorOperationId::random().unwrap(),
            source: Address::repeat_byte(1),
            recipient: "private destination".into(),
            asset: ExecutorAsset::Erc20(Address::repeat_byte(2)),
            amount: U256::from(100),
            funding,
            maximum_native_fee: U256::from(200),
            competing_payloads: false,
        };
        let delegate = Address::repeat_byte(3);
        let execution = match &approval.funding {
            ExecutorRecoveryFunding::ExecutorNative { .. } => {
                ExecutorRecoveryExecution::SignedMulticall { nonce: U256::ZERO }
            }
            ExecutorRecoveryFunding::PublicBroadcaster { .. } => {
                ExecutorRecoveryExecution::PaidExecute { nonce: U256::ZERO }
            }
        };
        let prepared = PreparedExecutorRecovery {
            operation: approval.operation,
            recovery: ExecutorOperationId::random().unwrap(),
            generation: 0,
            owner: tokio::sync::watch::channel(false).0,
            source: approval.source,
            delegate,
            current_delegate: Some(delegate),
            account_nonce: 1,
            replacement_nonce: None,
            recipient: approval.recipient.clone(),
            asset: approval.asset,
            amount: approval.amount,
            funding: approval.funding.clone(),
            execution,
            calls: Vec::new(),
            steps: Vec::new(),
            gas_limits: Vec::new(),
            maximum_native_fee: approval.maximum_native_fee,
            shield: ShieldRequest {
                preimage: CommitmentPreimage {
                    npk: B256::ZERO,
                    token: TokenData::erc20(Address::repeat_byte(2)),
                    value: alloy::primitives::Uint::from(100),
                },
                ciphertext: ShieldCiphertext {
                    encryptedBundle: [B256::ZERO; 3],
                    shieldKey: B256::ZERO,
                },
            },
        };
        let record = serde_json::from_value(serde_json::json!({
            "version": 1,
            "derivation": ExecutorDerivationScheme::Railgun7702V1,
            "origin": ExecutorRecordOrigin::Discovered,
            "operation": approval.operation,
            "index": 0,
            "address": approval.source,
            "delegate": delegate,
            "retired": true,
            "issued": [],
        }))
        .unwrap();
        (approval, prepared, record)
    }

    fn native_funding(max: u128, tip: u128) -> ExecutorRecoveryFunding {
        ExecutorRecoveryFunding::ExecutorNative {
            gas_fee: PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: max,
                max_priority_fee_per_gas: tip,
            },
        }
    }

    fn paid_funding() -> ExecutorRecoveryFunding {
        ExecutorRecoveryFunding::PublicBroadcaster {
            candidate: Box::new(PublicBroadcasterCandidate {
                chain_id: 1,
                railgun_address: "approved broadcaster".into(),
                identifier: None,
                token: Address::repeat_byte(2),
                fee: U256::ONE,
                fees_id: "approved quote".into(),
                fee_expiration: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(100),
                reliability: 1.0,
                available_wallets: 1,
                version: "8.3.0".into(),
                relay_adapt: Address::ZERO,
                relay_adapt_7702: Some(Address::repeat_byte(3)),
                required_poi_list_keys: Vec::new(),
                viewing_public_key: [4; 32],
                address_data: AddressData {
                    master_public_key: U256::ONE,
                    viewing_public_key: [4; 32],
                },
                fee_policy_status: crate::BroadcasterFeePolicyStatus::UnknownAnchor,
            }),
            maximum_private_fee: U256::from(10),
        }
    }

    #[test]
    fn recovery_approval_binds_assets_destination_and_private_payment() {
        let (approval, mut prepared, record) = fixture(paid_funding());
        assert!(approval.covers(&prepared, &record));
        for change in [
            |plan: &mut PreparedExecutorRecovery| plan.source = Address::ZERO,
            |plan: &mut PreparedExecutorRecovery| plan.recipient = "another destination".into(),
            |plan: &mut PreparedExecutorRecovery| plan.asset = ExecutorAsset::Erc20(Address::ZERO),
            |plan: &mut PreparedExecutorRecovery| plan.amount += U256::ONE,
            |plan: &mut PreparedExecutorRecovery| plan.funding = native_funding(2, 1),
        ] {
            let (approval, mut plan, record) = fixture(paid_funding());
            change(&mut plan);
            assert!(!approval.covers(&plan, &record));
        }
        let set_fee = |plan: &mut PreparedExecutorRecovery, amount| {
            let ExecutorRecoveryFunding::PublicBroadcaster {
                maximum_private_fee,
                ..
            } = &mut plan.funding
            else {
                unreachable!()
            };
            *maximum_private_fee = U256::from(amount);
        };
        set_fee(&mut prepared, 9);
        assert!(approval.covers(&prepared, &record));
        set_fee(&mut prepared, 11);
        assert!(!approval.covers(&prepared, &record));
        set_fee(&mut prepared, 10);
        let ExecutorRecoveryFunding::PublicBroadcaster { candidate, .. } = &mut prepared.funding
        else {
            unreachable!()
        };
        candidate.railgun_address = "another broadcaster".into();
        assert!(!approval.covers(&prepared, &record));
    }

    #[test]
    fn recovery_approval_allows_native_gas_reserve_but_bounds_amount_and_fees() {
        let (mut approval, mut prepared, record) = fixture(native_funding(2, 1));
        prepared.amount -= U256::ONE;
        assert!(
            !approval.covers(&prepared, &record),
            "Token recovery approves an exact amount"
        );
        approval.asset = ExecutorAsset::Native;
        prepared.asset = ExecutorAsset::Native;
        assert!(
            approval.covers(&prepared, &record),
            "Native recovery approves up to the requested amount after gas"
        );
        prepared.amount = approval.amount + U256::ONE;
        assert!(!approval.covers(&prepared, &record));
        prepared.amount = approval.amount;
        prepared.maximum_native_fee += U256::ONE;
        assert!(!approval.covers(&prepared, &record));
        prepared.maximum_native_fee = approval.maximum_native_fee;
        prepared.funding = native_funding(3, 1);
        assert!(!approval.covers(&prepared, &record));
        prepared.funding = native_funding(2, 2);
        assert!(!approval.covers(&prepared, &record));
        prepared.funding = native_funding(1, 1);
        assert!(approval.covers(&prepared, &record));
    }

    #[test]
    fn recovery_approval_requires_review_for_new_execution_consequences() {
        let (mut approval, mut prepared, record) = fixture(native_funding(2, 1));
        prepared.current_delegate = None;
        assert!(!approval.covers(&prepared, &record));
        prepared.current_delegate = Some(Address::repeat_byte(9));
        assert!(!approval.covers(&prepared, &record));
        prepared.current_delegate = Some(prepared.delegate);
        prepared.replacement_nonce = Some(1);
        assert!(!approval.covers(&prepared, &record));
        prepared.replacement_nonce = None;
        let mut saved = serde_json::to_value(record).unwrap();
        saved["issued"] = serde_json::to_value(vec![IssuedExecutorPayload::new(
            U256::ZERO,
            prepared.delegate,
            B256::repeat_byte(5),
            ExecutorPayloadPurpose::Operation,
            ExecutorPayloadContext::new(
                Bytes::new(),
                ExecutorNonceObservation::new(
                    BlockNumHash::new(1, B256::repeat_byte(6)),
                    U256::ZERO,
                ),
                Vec::new(),
            ),
        )])
        .unwrap();
        let record = serde_json::from_value(saved).unwrap();
        assert!(!approval.covers(&prepared, &record));
        approval.competing_payloads = true;
        assert!(approval.covers(&prepared, &record));
    }
}
