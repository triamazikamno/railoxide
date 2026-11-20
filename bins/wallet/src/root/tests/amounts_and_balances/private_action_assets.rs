use super::*;

#[test]
fn max_unshield_amount_from_snapshot_uses_batched_top_chunks() {
    let token = Address::from([0x11; 20]);
    let other = Address::from([0x22; 20]);
    let mut utxos = (0..20)
        .map(|position| unshield_utxo_output(token, 1, 0, position))
        .collect::<Vec<_>>();
    utxos.extend((0..5).map(|position| unshield_utxo_output(token, 3, 1, position)));
    utxos.push(unshield_utxo_output(other, 100, 1, 99));
    let mut unknown = unshield_utxo_output(token, 100, 2, 1);
    unknown.poi_statuses.clear();
    unknown.poi_spendable = false;
    utxos.push(unknown);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: utxos.len(),
        unspent_count: utxos.len(),
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos,
        totals: Vec::new(),
    };

    assert_eq!(
        max_unshield_amount_from_snapshot(&snapshot, token),
        uint!(35_U256)
    );
}

#[test]
fn refreshed_form_asset_tracks_new_utxos() {
    let token = Address::from([0x11; 20]);
    let original_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![unshield_utxo_output(token, 5, 0, 1)],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "5".to_string(),
            poi_verified_total: "5".to_string(),
        }],
    };
    let original_row = format_private_asset_rows(1, &original_snapshot.totals, None, None)
        .pop()
        .expect("formatted row");
    let original_asset =
        build_unshield_asset(&original_snapshot, &original_row).expect("original asset");
    let updated_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            unshield_utxo_output(token, 5, 0, 1),
            unshield_utxo_output(token, 3, 0, 2),
        ],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "8".to_string(),
            poi_verified_total: "8".to_string(),
        }],
    };

    let updated = refresh_form_asset_from_snapshot(&updated_snapshot, &original_asset, false, None);

    assert_eq!(updated.total, uint!(8_U256));
    assert_eq!(updated.poi_verified_total, uint!(8_U256));
    assert_eq!(updated.max_batched, uint!(8_U256));
}

#[test]
fn refreshed_form_asset_tracks_spent_out_token() {
    let token = Address::from([0x11; 20]);
    let original_asset = UnshieldAsset {
        chain_id: 1,
        token,
        label: "WETH".to_string(),
        decimals: Some(18),
        total: uint!(5_U256),
        poi_verified_total: uint!(5_U256),
        max_batched: uint!(5_U256),
        icon_path: None,
    };
    let mut spent = unshield_utxo_output(token, 5, 0, 1);
    spent.is_spent = true;
    spent.poi_spendable = false;
    spent.spent_tx_hash =
        Some("0x2222222222222222222222222222222222222222222222222222222222222222".to_string());
    spent.spent_block_number = Some(21);
    let updated_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 0,
        spent_count: 1,
        local_pending_spent_count: 0,
        utxos: vec![spent],
        totals: Vec::new(),
    };

    let updated = refresh_form_asset_from_snapshot(&updated_snapshot, &original_asset, false, None);

    assert_eq!(updated.label, "WETH");
    assert_eq!(updated.decimals, Some(18));
    assert_eq!(updated.total, U256::ZERO);
    assert_eq!(updated.poi_verified_total, U256::ZERO);
    assert_eq!(updated.max_batched, U256::ZERO);
}

#[test]
fn max_send_amount_from_snapshot_uses_batched_top_chunks() {
    let token = Address::from([0x12; 20]);
    let other = Address::from([0x22; 20]);
    let mut utxos = (0..20)
        .map(|position| unshield_utxo_output(token, 1, 0, position))
        .collect::<Vec<_>>();
    utxos.extend((0..5).map(|position| unshield_utxo_output(token, 3, 1, position)));
    utxos.push(unshield_utxo_output(other, 100, 1, 99));
    let mut unknown = unshield_utxo_output(token, 100, 2, 1);
    unknown.poi_statuses.clear();
    unknown.poi_spendable = false;
    utxos.push(unknown);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: utxos.len(),
        unspent_count: utxos.len(),
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos,
        totals: Vec::new(),
    };

    assert_eq!(
        max_send_amount_from_snapshot(&snapshot, token),
        uint!(35_U256)
    );
}

#[test]
fn build_unshield_asset_includes_max_batched_transaction() {
    let token = Address::from([0x33; 20]);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            unshield_utxo_output(token, 5, 0, 1),
            unshield_utxo_output(token, 7, 0, 2),
        ],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "12".to_string(),
            poi_verified_total: "12".to_string(),
        }],
    };
    let row = format_private_asset_rows(1, &snapshot.totals, None, None)
        .into_iter()
        .next()
        .expect("asset row");

    let asset = build_unshield_asset(&snapshot, &row).expect("unshield asset");

    assert_eq!(asset.total, uint!(12_U256));
    assert_eq!(asset.max_batched, uint!(12_U256));
}

#[test]
fn build_send_asset_includes_max_batched_transaction() {
    let token = Address::from([0x34; 20]);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            unshield_utxo_output(token, 5, 0, 1),
            unshield_utxo_output(token, 7, 0, 2),
        ],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "12".to_string(),
            poi_verified_total: "12".to_string(),
        }],
    };
    let row = format_private_asset_rows(1, &snapshot.totals, None, None)
        .into_iter()
        .next()
        .expect("asset row");

    let asset = build_send_asset(&snapshot, &row).expect("send asset");

    assert_eq!(asset.total, uint!(12_U256));
    assert_eq!(asset.max_batched, uint!(12_U256));
}

#[test]
fn private_action_assets_from_snapshot_include_only_switchable_assets() {
    let weth = Address::from([0x35; 20]);
    let dai = Address::from([0x36; 20]);
    let stale = Address::from([0x37; 20]);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            unshield_utxo_output(weth, 5, 0, 1),
            unshield_utxo_output(dai, 7, 0, 2),
        ],
        totals: vec![
            wallet_ops::TokenTotal {
                token: weth.to_checksum(None),
                total: "5".to_string(),
                poi_verified_total: "5".to_string(),
            },
            wallet_ops::TokenTotal {
                token: stale.to_checksum(None),
                total: "3".to_string(),
                poi_verified_total: "3".to_string(),
            },
            wallet_ops::TokenTotal {
                token: dai.to_checksum(None),
                total: "7".to_string(),
                poi_verified_total: "7".to_string(),
            },
        ],
    };

    let send_assets =
        private_action_assets_from_snapshot(DeliveryFormKind::Send, &snapshot, None, None);
    let unshield_assets =
        private_action_assets_from_snapshot(DeliveryFormKind::Unshield, &snapshot, None, None);

    assert_eq!(
        send_assets
            .iter()
            .map(|asset| asset.token)
            .collect::<Vec<_>>(),
        [weth, dai]
    );
    assert_eq!(
        unshield_assets
            .iter()
            .map(|asset| asset.token)
            .collect::<Vec<_>>(),
        [weth, dai]
    );
}

#[test]
fn unshield_key_matches_only_selected_asset() {
    let token = Address::from([0x44; 20]);
    let other = Address::from([0x45; 20]);
    let rows = format_private_asset_rows(
        1,
        &[
            wallet_ops::TokenTotal {
                token: token.to_checksum(None),
                total: "5".to_string(),
                poi_verified_total: "5".to_string(),
            },
            wallet_ops::TokenTotal {
                token: other.to_checksum(None),
                total: "7".to_string(),
                poi_verified_total: "7".to_string(),
            },
        ],
        None,
        None,
    );
    let key = UnshieldAssetKey::new(1, token);

    assert_eq!(unshield_asset_key_from_formatted(&rows[0]), Some(key));
    assert!(unshield_key_matches_asset(key, &rows[0]));
    assert!(!unshield_key_matches_asset(key, &rows[1]));
}

#[test]
fn send_key_matches_only_selected_asset() {
    let token = Address::from([0x46; 20]);
    let other = Address::from([0x47; 20]);
    let rows = format_private_asset_rows(
        1,
        &[
            wallet_ops::TokenTotal {
                token: token.to_checksum(None),
                total: "5".to_string(),
                poi_verified_total: "5".to_string(),
            },
            wallet_ops::TokenTotal {
                token: other.to_checksum(None),
                total: "7".to_string(),
                poi_verified_total: "7".to_string(),
            },
        ],
        None,
        None,
    );
    let key = UnshieldAssetKey::new(1, token);

    assert_eq!(send_asset_key_from_formatted(&rows[0]), Some(key));
    assert!(send_key_matches_asset(key, &rows[0]));
    assert!(!send_key_matches_asset(key, &rows[1]));
}

#[test]
fn unshield_element_ids_are_asset_scoped() {
    let first = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let second = UnshieldAssetKey::new(1, Address::from([0x22; 20]));

    assert_ne!(
        unshield_element_id(first, "cancel").as_ref(),
        unshield_element_id(second, "cancel").as_ref()
    );
    assert_ne!(
        unshield_element_id(first, "copy-to").as_ref(),
        unshield_element_id(first, "copy-data").as_ref()
    );
}

#[test]
fn send_element_ids_are_asset_scoped() {
    let first = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let second = UnshieldAssetKey::new(1, Address::from([0x22; 20]));

    assert_ne!(
        send_element_id(first, "cancel").as_ref(),
        send_element_id(second, "cancel").as_ref()
    );
    assert_ne!(
        send_element_id(first, "copy-to").as_ref(),
        send_element_id(first, "copy-data").as_ref()
    );
}
