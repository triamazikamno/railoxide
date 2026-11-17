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
    let (steps, calls) = recovery_calls(ExecutorAsset::Native, amount, U256::ONE, &shield, source);
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
