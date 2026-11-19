use super::*;

#[test]
fn public_broadcaster_transact_envelope_roundtrips() {
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::transactCall;

    let (candidate, broadcaster) = sample_public_broadcaster_candidate(9);
    let call = transactCall {
        _transactions: Vec::new(),
    };
    let params = public_broadcaster_transact_params(
        &candidate,
        TransactionRequest::default()
            .to(address(0x33))
            .gas_limit(210_000)
            .input(call.abi_encode().into()),
        20_000_000_000,
        BTreeMap::new(),
    )
    .unwrap();

    let encrypted = EncryptedTransactRequest::encrypt_with_seed(
        candidate.viewing_public_key,
        &params,
        [8u8; 32],
    )
    .expect("encrypt request");
    let payload = encrypted.to_transact_payload().expect("serialize envelope");
    let value: serde_json::Value = serde_json::from_slice(&payload).expect("json envelope");
    assert_eq!(value["method"], "transact");
    // Check the wire spelling before typed decoding normalizes the public key.
    assert_eq!(value["params"]["pubkey"], hex::encode(encrypted.pubkey));
    assert_eq!(
        value["params"]["encryptedData"],
        serde_json::to_value(&encrypted.encrypted_data).unwrap()
    );
    assert_eq!(transact_topic(1), "/railgun/v2/0-1-transact/json");

    let pubkey: FixedBytes<32> =
        serde_json::from_value(value["params"]["pubkey"].clone()).expect("envelope public key");
    let encrypted_data = serde_json::from_value(value["params"]["encryptedData"].clone())
        .expect("envelope encrypted data");
    let decrypted =
        try_decrypt_transact_request(&broadcaster.viewing_private_key, pubkey.0, &encrypted_data)
            .expect("decrypt request")
            .expect("request for broadcaster");
    assert_eq!(decrypted.params.fees_id.as_deref(), Some("fees-id"));
    assert_eq!(decrypted.params.other["minVersion"], "8.0.0");
    assert_eq!(decrypted.params.other["maxVersion"], "8.999.0");
    assert!(!decrypted.params.other.contains_key("gasLimit"));
    assert_eq!(decrypted.params.other["useRelayAdapt"], false);
    // SDK broadcasters suppress unrecognized errors unless this wire flag is set.
    assert_eq!(decrypted.params.other["devLog"], true);
    assert_eq!(
        decrypted.params.min_gas_price,
        Some(uint!(20_000_000_000_U256))
    );
    assert!(
        decrypted
            .params
            .pre_transaction_pois_per_txid_leaf_per_list
            .is_empty()
    );

    let error = "execution reverted: recovery authorization nonce is already used";
    let response =
        DecryptedTransactResponse::encrypted_error_message(None, &decrypted.shared_key, error)
            .expect("encrypt detailed error");
    assert_eq!(
        decode_public_broadcaster_response(&encrypted.shared_key, &response).unwrap(),
        Some(PublicBroadcasterResultKind::Failed {
            error: error.into()
        })
    );
}

#[test]
fn public_broadcaster_transact_payload_includes_single_chunk_poi() {
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::{ActionData, relayCall};

    let (mut candidate, broadcaster) = sample_public_broadcaster_candidate(10);
    let call = relayCall {
        _transactions: Vec::new(),
        _actionData: ActionData {
            random: FixedBytes::ZERO,
            requireSuccess: true,
            minGasLimit: U256::ZERO,
            calls: Vec::new(),
        },
    };
    let list_key = FixedBytes::from([0x88; 32]);
    let txid_leaf = FixedBytes::from([0x99; 32]);
    candidate.required_poi_list_keys = vec![hex::encode(list_key)];
    let required_keys = candidate
        .parsed_required_poi_list_keys()
        .expect("required list keys");
    let params = public_broadcaster_transact_params(
        &candidate,
        TransactionRequest::default()
            .to(candidate.relay_adapt.expect("fixture relay adapt"))
            .input(call.abi_encode().into()),
        20_000_000_000,
        sample_poi_map(&required_keys, &[txid_leaf]),
    )
    .unwrap();

    let encrypted = EncryptedTransactRequest::encrypt_with_seed(
        candidate.viewing_public_key,
        &params,
        [8u8; 32],
    )
    .expect("encrypt request");
    let decrypted = try_decrypt_transact_request(
        &broadcaster.viewing_private_key,
        encrypted.pubkey,
        &encrypted.encrypted_data,
    )
    .expect("decrypt request")
    .expect("request for broadcaster");

    assert_eq!(decrypted.params.other["useRelayAdapt"], true);
    let per_leaf = decrypted
        .params
        .pre_transaction_pois_per_txid_leaf_per_list
        .get(&list_key)
        .expect("list key");
    assert_eq!(per_leaf.len(), 1);
    assert!(per_leaf.contains_key(&txid_leaf));
}

#[test]
fn public_broadcaster_transact_payload_preserves_executor_authorization_and_all_pois() {
    use alloy::eips::eip7702::Authorization;
    use alloy::signers::{SignerSync, local::PrivateKeySigner};
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::{
        BoundParams, CommitmentPreimage, RelayAdapt7702, RelayAdapt7702ActionData, SnarkProof,
        Transaction,
    };
    use broadcaster_core::crypto::aes_gcm::{decrypt_in_place_16b_iv, split_iv_tag};
    use broadcaster_core::transact::{BroadcasterTransactRequestType, compute_railgun_txid};
    let (mut candidate, broadcaster) = sample_public_broadcaster_candidate(11);
    let list_keys = [FixedBytes::from([0x81; 32]), FixedBytes::from([0x82; 32])];
    let owner = PrivateKeySigner::from_bytes(&FixedBytes::repeat_byte(7)).unwrap();
    let chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let delegate = chain.accepted_executor_profile().unwrap().delegate();
    candidate.version = "alternative-implementation".to_owned();
    candidate.relay_adapt_7702 = Some(delegate);
    let transactions = [1, 2].map(|tree| Transaction {
        proof: SnarkProof::default(),
        merkleRoot: FixedBytes::repeat_byte(tree),
        nullifiers: vec![FixedBytes::repeat_byte(tree + 1)],
        commitments: vec![FixedBytes::repeat_byte(tree + 2)],
        boundParams: BoundParams::new_transact(
            u32::from(tree),
            0,
            1,
            Vec::new(),
            owner.address(),
            FixedBytes::ZERO,
        ),
        unshieldPreimage: CommitmentPreimage::empty(),
    });
    let leaves = transactions
        .iter()
        .map(|transaction| {
            FixedBytes::from(railgun_txid_leaf_hash(
                compute_railgun_txid(
                    transaction,
                    Some(broadcaster_core::transact::DEFAULT_TXID_VERSION),
                )
                .unwrap(),
                u64::from(transaction.boundParams.treeNumber),
            ))
        })
        .collect::<Vec<_>>();
    let action = RelayAdapt7702ActionData {
        requireSuccess: true,
        minGasLimit: U256::ZERO,
        calls: Vec::new(),
    };
    let hash = broadcaster_core::contracts::executor::execute_signing_hash(
        &transactions,
        &action,
        U256::from(3),
        1,
        owner.address(),
    );
    let call = RelayAdapt7702::executeCall {
        _transactions: transactions.into(),
        _actionData: action,
        _nonce: U256::from(3),
        _signature: owner.sign_hash_sync(&hash).unwrap().as_bytes().into(),
    };
    let authorization = Authorization {
        chain_id: U256::ONE,
        address: delegate,
        nonce: 7,
    };
    let signature = owner
        .sign_hash_sync(&authorization.signature_hash())
        .unwrap();
    let signed_authorization = authorization.into_signed(signature);
    let mut transaction = TransactionRequest::default()
        .to(owner.address())
        .input(call.abi_encode().into());
    transaction.transaction_type = Some(4);
    transaction.authorization_list = Some(vec![signed_authorization.clone()]);
    transaction.max_fee_per_gas = Some(25_000_000_000);
    transaction.max_priority_fee_per_gas = Some(1_000_000_000);
    transaction.gas = Some(1_495_776);
    candidate.required_poi_list_keys = list_keys.iter().map(hex::encode).collect();
    let required_keys = candidate
        .parsed_required_poi_list_keys()
        .expect("required list keys");
    let params = public_broadcaster_transact_params(
        &candidate,
        transaction.clone(),
        20_000_000_000,
        sample_poi_map(&required_keys, &leaves),
    )
    .unwrap();
    let mut missing_gas = transaction.clone();
    missing_gas.gas = None;
    assert!(
        public_broadcaster_transact_params(
            &candidate,
            missing_gas,
            20_000_000_000,
            sample_poi_map(&required_keys, &leaves),
        )
        .is_err()
    );
    let mut delegated = transaction.clone();
    delegated.transaction_type = Some(2);
    delegated.authorization_list = None;
    // An installed delegation still needs fresh authorization to select the
    // SDK broadcaster's TX7702 route instead of its ordinary contract route.
    assert!(
        public_broadcaster_transact_params(
            &candidate,
            delegated,
            20_000_000_000,
            sample_poi_map(&required_keys, &leaves),
        )
        .is_err()
    );
    for list in list_keys {
        let mut incomplete = sample_poi_map(&required_keys, &leaves);
        incomplete.get_mut(&list).unwrap().remove(&leaves[1]);
        assert!(
            public_broadcaster_transact_params(
                &candidate,
                transaction.clone(),
                20_000_000_000,
                incomplete
            )
            .is_err()
        );
    }
    let mut incompatible = candidate.clone();
    incompatible.relay_adapt_7702 = Some(Address::repeat_byte(0x23));
    assert!(
        public_broadcaster_transact_params(
            &incompatible,
            transaction,
            20_000_000_000,
            sample_poi_map(&required_keys, &leaves)
        )
        .is_err()
    );

    let encrypted = EncryptedTransactRequest::encrypt_with_seed(
        candidate.viewing_public_key,
        &params,
        [8u8; 32],
    )
    .expect("encrypt request");
    let decrypted = try_decrypt_transact_request(
        &broadcaster.viewing_private_key,
        encrypted.pubkey,
        &encrypted.encrypted_data,
    )
    .expect("decrypt request")
    .expect("request for broadcaster");

    assert_eq!(
        decrypted.params.transact_type,
        Some(BroadcasterTransactRequestType::Tx7702)
    );
    assert_eq!(
        decrypted.params.other.get("gasLimit"),
        Some(&serde_json::json!("1495776"))
    );
    assert_eq!(
        decrypted.params.other.get("useRelayAdapt"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        decrypted.params.max_fee_per_gas,
        Some(U256::from(25_000_000_000_u64))
    );
    assert_eq!(
        decrypted.params.max_priority_fee_per_gas,
        Some(U256::from(1_000_000_000_u64))
    );
    assert_eq!(
        decrypted
            .params
            .authorization
            .unwrap()
            .signed_authorization()
            .unwrap(),
        signed_authorization
    );
    assert_eq!(decrypted.params.data.as_ref(), call.abi_encode());
    // Inspect the encrypted JSON itself: typed decoding accepts both prefixes
    // and would hide an incompatible spelling from the SDK's string lookups.
    let (iv, tag) = split_iv_tag(encrypted.encrypted_data[0].as_ref().try_into().unwrap());
    let mut plaintext = encrypted.encrypted_data[1].to_vec();
    decrypt_in_place_16b_iv(&decrypted.shared_key, &iv, &tag, &mut plaintext).unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
    let poi_map = decrypted.params.pre_transaction_pois_per_txid_leaf_per_list;
    assert_eq!(poi_map.len(), 2);
    for list_key in list_keys {
        let per_leaf = poi_map.get(&list_key).expect("list key");
        assert_eq!(per_leaf.len(), 2);
        for leaf in &leaves {
            assert!(per_leaf.contains_key(leaf));
            let wire_poi = &wire["preTransactionPOIsPerTxidLeafPerList"][hex::encode(list_key)]
                [hex::encode(leaf)];
            assert_eq!(
                wire_poi["txidMerkleroot"],
                hex::encode(per_leaf[leaf].txid_merkleroot)
            );
            assert_eq!(
                wire_poi["poiMerkleroots"],
                serde_json::json!(
                    per_leaf[leaf]
                        .poi_merkleroots
                        .iter()
                        .map(hex::encode)
                        .collect::<Vec<_>>()
                )
            );
        }
    }
}

#[test]
fn public_broadcaster_invalid_poi_list_key_fails_preparation() {
    let (mut candidate, _) = sample_public_broadcaster_candidate(12);
    candidate.required_poi_list_keys = vec!["poi-list".to_string()];

    let error = candidate
        .parsed_required_poi_list_keys()
        .expect_err("invalid POI list key should fail");

    assert!(error.to_string().contains("invalid required POI list key"));
}

#[test]
fn public_broadcaster_response_decodes_tx_hash() {
    let shared_key = [7u8; 32];
    let tx_hash = TxHash::from([3u8; 32]);
    let response = DecryptedTransactResponse::encrypted_tx_hash_message(None, &shared_key, tx_hash)
        .expect("response payload");

    let decoded = decode_public_broadcaster_response(&shared_key, &response)
        .expect("decode response")
        .expect("decryptable response");

    assert_eq!(
        decoded,
        PublicBroadcasterResultKind::Submitted {
            tx_hash: tx_hash.to_string()
        }
    );
}
