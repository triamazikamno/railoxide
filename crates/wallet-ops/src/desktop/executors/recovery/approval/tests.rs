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
        expected_active_use: None,
        generation: 0,
        owner: tokio::sync::watch::channel(false).0,
        source: approval.source,
        delegate,
        current_delegate: Some(delegate),
        account_nonce: 1,
        recipient: approval.recipient.clone(),
        asset: approval.asset,
        amount: approval.amount,
        funding: approval.funding.clone(),
        execution,
        calls: Vec::new(),
        steps: Vec::new(),
        gas_limits: Vec::new(),
        maximum_native_fee: approval.maximum_native_fee,
        shield: Some(ShieldRequest {
            preimage: CommitmentPreimage {
                npk: B256::ZERO,
                token: TokenData::erc20(Address::repeat_byte(2)),
                value: alloy::primitives::Uint::from(100),
            },
            ciphertext: ShieldCiphertext {
                encryptedBundle: [B256::ZERO; 3],
                shieldKey: B256::ZERO,
            },
        }),
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
            relay_adapt: None,
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
    let ExecutorRecoveryFunding::PublicBroadcaster { candidate, .. } = &mut prepared.funding else {
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
    let mut saved = serde_json::to_value(record).unwrap();
    saved["issued"] = serde_json::to_value(vec![IssuedExecutorPayload::new(
        U256::ZERO,
        prepared.delegate,
        B256::repeat_byte(5),
        ExecutorPayloadPurpose::Operation,
        ExecutorPayloadContext::new(
            Bytes::new(),
            ExecutorNonceObservation::new(BlockNumHash::new(1, B256::repeat_byte(6)), U256::ZERO),
            Vec::new(),
        ),
    )])
    .unwrap();
    let record = serde_json::from_value(saved).unwrap();
    assert!(!approval.covers(&prepared, &record));
    approval.competing_payloads = true;
    assert!(approval.covers(&prepared, &record));
}
