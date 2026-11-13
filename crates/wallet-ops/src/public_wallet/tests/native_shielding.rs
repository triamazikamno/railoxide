use super::*;

/// The SDK vector address in `vault/tests/key_export.rs`.
const TEST_SHIELD_RECIPIENT: &str = "0zk1qykzjxctynyz4z43pukckpv43jyzhyvy0ehrd5wuc54l5enqf9qfrrv7j6fe3z53la7enqphqvxys9aqyp9xx0km95ehqslx8apmu8l7anc7emau4tvsultrkvd";

fn test_shield_recipient() -> broadcaster_core::crypto::railgun::AddressData {
    broadcaster_core::crypto::railgun::AddressData::try_from(
        &broadcaster_core::crypto::railgun::Address::from(TEST_SHIELD_RECIPIENT),
    )
    .unwrap()
}

/// A native Mimic Railway preparation with a fixed raw estimate.
fn test_native_preparation(
    chain_id: u64,
    payer: Address,
    recipient: broadcaster_core::crypto::railgun::AddressData,
    amount: U256,
    raw_gas_limit: u64,
) -> NativeShieldPreparation {
    let chain = default_effective_chain(chain_id);
    let contracts =
        stealth_native_shield_contracts(chain_id, &chain, NativeShieldPayer::Software).unwrap();
    NativeShieldPreparation::new(
        contracts,
        PublicShieldTransactionProfile::Railway,
        payer,
        recipient,
        amount,
        raw_gas_limit,
        chain.gas.gas_limit_buffer,
    )
}

#[test]
fn native_shield_eligibility_explains_unavailable_routes() {
    use NativeShieldPayer::{Hardware, HardwareWallet, Software};
    use PublicShieldTransactionProfile::{Railoxide, Railway};

    for chain_id in [1, 56, 137, 42161] {
        let chain = default_effective_chain(chain_id);
        let contracts = stealth_native_shield_contracts(chain_id, &chain, Software).unwrap();
        assert_eq!(
            contracts.delegate,
            broadcaster_core::deployment::RailgunDeployment::for_chain(chain_id)
                .unwrap()
                .relay_adapt_7702_contract
        );
        assert_eq!(Some(contracts.wrapped_native), chain.wrapped_native_token);
    }
    assert_eq!(
        stealth_native_shield_contracts(10, &default_effective_chain(1), Software),
        Err(NativeShieldUnavailable::UnsupportedChain)
    );

    // The route follows the payer, never the profile; only Railoxide has a fallback.
    let chain = default_effective_chain(1);
    let stealth =
        NativeShieldRoute::Stealth(stealth_native_shield_contracts(1, &chain, Software).unwrap());
    let fallback = NativeShieldRoute::WrapFallback {
        wrapped_native: chain.wrapped_native_token.unwrap(),
    };
    let mut no_wrapped_native = default_effective_chain(1);
    no_wrapped_native.wrapped_native_token = None;
    let base_fee = Some(EvmFeeModel::Eip1559);
    let no_base_fee = Some(EvmFeeModel::Legacy);
    for (chain, payer, profile, fee_model, route) in [
        // A software payer, or a stealth payer of a software wallet.
        (&chain, Software, Railoxide, base_fee, Ok(stealth)),
        (&chain, Software, Railway, base_fee, Ok(stealth)),
        // A stealth payer of a Ledger or Trezor wallet.
        (&chain, HardwareWallet, Railoxide, base_fee, Ok(fallback)),
        (
            &chain,
            HardwareWallet,
            Railway,
            base_fee,
            Err(NativeShieldUnavailable::HardwareWallet),
        ),
        // A hardware-native payer.
        (&chain, Hardware, Railoxide, base_fee, Ok(fallback)),
        (
            &chain,
            Hardware,
            Railway,
            base_fee,
            Err(NativeShieldUnavailable::HardwarePayer),
        ),
        // No type-4 envelope without a base fee.
        (&chain, Software, Railoxide, no_base_fee, Ok(fallback)),
        (
            &chain,
            Software,
            Railway,
            no_base_fee,
            Err(NativeShieldUnavailable::NoBaseFee),
        ),
        (
            &no_wrapped_native,
            Hardware,
            Railoxide,
            base_fee,
            Err(NativeShieldUnavailable::NoWrappedNative),
        ),
    ] {
        assert_eq!(
            select_native_shield_route(1, chain, payer, profile, fee_model),
            route,
            "{payer:?} in {profile:?} with {fee_model:?}"
        );
    }
}

#[test]
fn native_preparation_binds_its_inputs_chain_and_profile() {
    use PublicShieldTransactionProfile::{Railoxide, Railway};

    let payer = Address::repeat_byte(0x11);
    let recipient = test_shield_recipient();
    let amount = U256::from(1_000_000_000_000_000_000_u128);
    let preparation = test_native_preparation(56, payer, recipient, amount, 100_000);

    // A changed chain, profile, payer, amount or recipient needs a new preparation.
    let contracts = stealth_native_shield_contracts(
        56,
        &default_effective_chain(56),
        NativeShieldPayer::Software,
    )
    .unwrap();
    preparation
        .ensure_matches(&contracts, Railway, payer, &recipient, amount)
        .unwrap();
    let ethereum = stealth_native_shield_contracts(
        1,
        &default_effective_chain(1),
        NativeShieldPayer::Software,
    )
    .unwrap();
    let mut other_recipient = recipient;
    other_recipient.master_public_key += U256::from(1);
    for (contracts, profile, payer, recipient, amount) in [
        (&ethereum, Railway, payer, &recipient, amount),
        (&contracts, Railoxide, payer, &recipient, amount),
        (
            &contracts,
            Railway,
            Address::repeat_byte(0x12),
            &recipient,
            amount,
        ),
        (
            &contracts,
            Railway,
            payer,
            &recipient,
            amount + U256::from(1),
        ),
        (&contracts, Railway, payer, &other_recipient, amount),
    ] {
        assert!(
            preparation
                .ensure_matches(contracts, profile, payer, recipient, amount)
                .is_err()
        );
    }
}

#[tokio::test]
async fn native_shield_signs_the_prepared_type_4_envelope() {
    use alloy::consensus::TxEnvelope;
    use alloy::eips::Decodable2718;

    let signer =
        VaultedPublicSigner::Software(SoftwareEvmSigner::from_private_key([7; 32]).unwrap());
    let payer = signer.address();
    let amount = U256::from(5_000_u64);
    let preparation = test_native_preparation(1, payer, test_shield_recipient(), amount, 100_000);
    let contracts = stealth_native_shield_contracts(
        1,
        &default_effective_chain(1),
        NativeShieldPayer::Software,
    )
    .unwrap();
    // The shape of the stealth account's issued transaction.
    let account = alloy::signers::local::PrivateKeySigner::random();
    let prepared = super::native_shield::native_shield_transaction_request(
        &contracts,
        payer,
        account.address(),
        amount,
        Bytes::from_static(&[1, 2, 3]),
        super::native_shield::native_shield_authorization(&account, &contracts, 0).unwrap(),
    );
    let input = prepared.input.input().cloned().unwrap();
    let authorization = prepared.authorization_list.clone().unwrap();
    let approved = public_action_eip1559_transaction_request(prepared, 1, payer, 100, 2, 9)
        .with_gas_limit(preparation.gas_limit());
    let signed = signer
        .sign_transaction_request(approved, "test")
        .await
        .unwrap();
    let TxEnvelope::Eip7702(signed) = TxEnvelope::decode_2718(&mut signed.as_slice()).unwrap()
    else {
        panic!("a type-4 transaction");
    };
    let tx = signed.tx();
    assert_eq!(
        (tx.chain_id, tx.to, tx.value, &tx.input, tx.nonce),
        (1, account.address(), amount, &input, 9)
    );
    assert_eq!(tx.authorization_list, authorization);
    assert_eq!(
        (tx.gas_limit, tx.max_fee_per_gas),
        (preparation.gas_limit(), 100)
    );
}

#[tokio::test]
async fn native_max_refits_the_amount_to_each_new_estimate() {
    let payer = Address::repeat_byte(0x11);
    let recipient = test_shield_recipient();
    let balance = U256::from(10_000_000_u64);
    // The second estimate rises, so its first fitted amount no longer leaves enough gas.
    let mut raw_estimates = [100_000_u64, 110_000, 110_000].into_iter();
    let preparation = super::native_shield::fit_native_shield_max(balance, 10, |amount| {
        std::future::ready(Ok(test_native_preparation(
            1,
            payer,
            recipient,
            amount,
            raw_estimates.next().unwrap(),
        )))
    })
    .await
    .unwrap();
    assert_eq!(preparation.gas_limit(), 132_000);
    assert_eq!(preparation.amount(), U256::from(10_000_000_u64 - 1_320_000));
    assert!(preparation.amount() + U256::from(preparation.gas_limit()) * U256::from(10) <= balance);

    assert!(
        super::native_shield::fit_native_shield_max(U256::from(1_200_000), 10, |amount| {
            std::future::ready(Ok(test_native_preparation(
                1, payer, recipient, amount, 100_000,
            )))
        })
        .await
        .is_err(),
        "no amount remains after the maximum gas cost"
    );
}

#[tokio::test]
async fn native_preparation_estimates_the_full_request() {
    let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let logged = calls.clone();
    let responder: crate::rpc_broker::tests::RpcResponder = Arc::new(move |request: Value| {
        logged.lock().unwrap().push(request.clone());
        let result = match request["method"].as_str().unwrap_or_default() {
            "eth_estimateGas" => json!("0x186a0"),
            // The executor's `nonce()` through the delegation, and the request at its estimate.
            "eth_call" => json!(B256::ZERO),
            // eth_chainId and anything else.
            _ => json!("0x1"),
        };
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    });
    let (url, server) =
        crate::rpc_broker::tests::spawn_rpc_mock(responder, Arc::default(), Arc::default()).await;
    let mut chain = default_effective_chain(1);
    chain.rpc_route = RpcChainRoute::new(1, vec![url]);
    let http = HttpContext::direct_for_tests();
    let payer = Address::repeat_byte(0x11);
    let amount = U256::from(5_000_u64);

    let preparation = prepare_native_shield(
        1,
        &chain,
        PublicShieldTransactionProfile::Railway,
        NativeShieldPayer::Software,
        payer,
        test_shield_recipient(),
        amount,
        &http,
        &http.operation_http_client().await.unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        (preparation.raw_gas_limit(), preparation.gas_limit()),
        (100_000, 120_000)
    );
    let calls = calls.lock().unwrap().clone();
    let estimate = calls
        .iter()
        .find(|call| call["method"] == "eth_estimateGas")
        .unwrap();
    let estimate: TransactionRequest =
        serde_json::from_value(estimate["params"][0].clone()).unwrap();
    assert_eq!(estimate.from, Some(payer));
    assert_eq!(estimate.value, Some(amount));
    assert_eq!(estimate.transaction_type, Some(4));
    // The stand-in account delegates itself and is the call's target.
    let [authorization] = estimate.authorization_list.as_deref().unwrap() else {
        panic!("one delegation");
    };
    assert_eq!(*authorization.address(), preparation.delegate());
    assert_eq!(
        estimate.to,
        Some(TxKind::Call(authorization.recover_authority().unwrap()))
    );
    server.abort();
}

#[tokio::test]
async fn native_quote_route_follows_the_profile_and_asset() {
    let (url, server, methods) = logged_fee_endpoint(56, 60_000_000, None, None).await;
    let mut chain = default_effective_chain(56);
    chain.rpc_route = RpcChainRoute::new(56, vec![url]);
    let http = HttpContext::direct_for_tests();
    let native = quote_public_action_gas_fee_bundle_with_profile(
        56,
        &chain,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Native,
        &http,
    )
    .await
    .unwrap();
    // Fee history: floor(5 * 110 / 100) plus the 60th-percentile tip 2.
    assert_eq!(native.standard.suggested_max_fee_per_gas, 7);
    assert_eq!(native.standard.suggested_max_priority_fee_per_gas, 2);
    assert!(
        !methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_gasPrice")
    );
    let erc20 = quote_public_action_gas_fee_bundle_with_profile(
        56,
        &chain,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Erc20(Address::repeat_byte(7)),
        &http,
    )
    .await
    .unwrap();
    assert_eq!(erc20.standard.rpc_gas_price, 55_000_000);
    assert_eq!(erc20.standard.suggested_max_priority_fee_per_gas, 0);
    // Railoxide's native shield keeps the ordinary quote, which observes the fee model.
    let railoxide = quote_public_action_gas_fee_bundle_with_profile(
        56,
        &chain,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        &http,
    )
    .await
    .unwrap();
    assert_eq!(
        railoxide.standard.observed_fee_model,
        Some(EvmFeeModel::Eip1559)
    );
    assert!(
        methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_getBlockByNumber")
    );
    server.abort();
}

#[test]
fn prepared_native_shield_needs_a_base_fee() {
    use super::types::PublicActionGasLimitStrategy;
    use PublicShieldTransactionProfile::{Railoxide, Railway};

    let quote = |observed_fee_model| PublicActionGasFeeQuote {
        observed_fee_model: Some(observed_fee_model),
        rpc_gas_price: 1,
        current_base_fee_per_gas: None,
        suggested_max_fee_per_gas: 1,
        suggested_max_priority_fee_per_gas: 0,
    };
    for (profile, quote, refused) in [
        (Railoxide, Some(quote(EvmFeeModel::Legacy)), true),
        (Railoxide, Some(quote(EvmFeeModel::Eip1559)), false),
        (Railway, None, false),
    ] {
        let legacy_envelope =
            public_action_uses_legacy_envelope(1, profile, PublicAssetId::Native, quote);
        let prepared = PublicActionGasLimitStrategy::NativeShieldPrepared {
            reviewed_gas_limit: 100_000,
            profile,
        };
        assert_eq!(
            super::submission::ensure_native_shield_envelope(prepared, legacy_envelope).is_err(),
            refused,
            "{profile:?}"
        );
    }
    // Other actions follow the observed fee model.
    assert!(
        super::submission::ensure_native_shield_envelope(
            PublicActionGasLimitStrategy::ChainBuffer,
            true
        )
        .is_ok()
    );
}

#[test]
fn native_shield_is_delivered_only_with_its_shield_event() {
    use alloy::sol_types::SolEvent;
    use broadcaster_core::contracts::railgun::Shield;

    use super::native_shield::ensure_native_shield_delivered;

    let contracts = stealth_native_shield_contracts(
        1,
        &default_effective_chain(1),
        NativeShieldPayer::Software,
    )
    .unwrap();
    let shield_log = |address: Address| alloy::rpc::types::Log {
        inner: alloy::primitives::Log {
            address,
            data: Shield {
                treeNumber: U256::ZERO,
                startPosition: U256::ZERO,
                commitments: Vec::new(),
                shieldCiphertext: Vec::new(),
                fees: Vec::new(),
            }
            .encode_log_data(),
        },
        ..Default::default()
    };
    let stealth_account = Address::repeat_byte(3);
    for (logs, delivered) in [
        (Ok(vec![shield_log(contracts.railgun)]), true),
        // Without Railgun's event the value is at the stealth account.
        (Ok(vec![shield_log(Address::repeat_byte(2))]), false),
        (Err(eyre!("logs unavailable")), false),
    ] {
        let settled = ensure_native_shield_delivered(&contracts, stealth_account, "0x01", logs);
        assert_eq!(settled.is_ok(), delivered);
        if let Err(error) = settled {
            // The failure names the account to recover.
            assert!(format!("{error:#}").contains(&stealth_account.to_string()));
        }
    }
}

#[test]
fn native_shield_signing_gas_limit_follows_the_profile_buffer() {
    use PublicShieldTransactionProfile::{Railoxide, Railway};

    const BUFFER: u64 = 30_000;
    // One prepared raw estimate of 1_000_000: Mimic's 120%, or Railoxide's chain buffer.
    for (profile, reviewed, below) in [
        (Railway, 1_200_000, 1_080_000),
        (Railoxide, 1_030_000, 930_000),
    ] {
        assert_eq!(
            super::gas::native_shield_gas_limit(profile, 1_000_000, BUFFER),
            reviewed
        );
        for (raw, signed) in [
            (900_000, Some(below)),
            // The final estimate is slightly above the prepared one.
            (1_000_012, Some(reviewed)),
            (reviewed, Some(reviewed)),
            (reviewed + 1, None),
        ] {
            assert_eq!(
                super::submission::native_shield_signing_gas_limit(profile, BUFFER, raw, reviewed)
                    .ok(),
                signed,
                "{profile:?} raw estimate {raw}"
            );
        }
    }
}
