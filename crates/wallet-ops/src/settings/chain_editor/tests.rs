use super::*;
use crate::settings::build_effective_chain_configs;

#[test]
fn native_pricing_round_trips_modes_and_rejects_bad_oracles() {
    let mut settings = WalletSettings::default();
    settings
        .chains
        .custom
        .insert(31337, super::super::tests::custom_evm_chain());
    for id in [1, 31337] {
        let preset = presets::EvmPreset::for_chain(id).and_then(|preset| preset.native_usd_oracle);
        for choice in [
            NativeUsdChoice::Disabled,
            NativeUsdChoice::Oracle,
            NativeUsdChoice::Default,
        ] {
            let mut draft = chain_editor_draft(&settings, id).unwrap();
            draft.native_usd_pricing = choice;
            draft.fields.insert(
                ChainField::NativeUsdOracle,
                "0x0000000000000000000000000000000000000001".into(),
            );
            settings = save_draft(&settings, draft);
            settings = crate::settings::decode_wallet_settings(
                &crate::settings::encode_wallet_settings(&settings).unwrap(),
            )
            .unwrap();
            let reopened = chain_editor_draft(&settings, id).unwrap();
            assert_eq!(reopened.native_usd_pricing, choice);
            let chains = super::super::build_effective_chain_configs(&settings).unwrap();
            let chain = chains.get(id).unwrap();
            assert_eq!(
                chain.native_usd_oracle,
                match choice {
                    NativeUsdChoice::Default => preset,
                    NativeUsdChoice::Disabled => None,
                    NativeUsdChoice::Oracle => Some(alloy::primitives::Address::with_last_byte(1)),
                }
            );
            if id == 31337 {
                assert!(chain.railgun.is_none());
                assert!(chain.wrapped_native_token.is_none());
            }
        }
        let mut draft = chain_editor_draft(&settings, id).unwrap();
        draft.native_usd_pricing = NativeUsdChoice::Oracle;
        draft
            .fields
            .insert(ChainField::NativeUsdOracle, "invalid".into());
        assert!(
            draft_mutation(&settings, &draft, true)
                .err()
                .unwrap()
                .contains("Oracle contract")
        );
    }
}

#[test]
fn tron_cannot_be_added_as_a_custom_chain_but_a_saved_one_stays_editable() {
    let mut draft = ChainDraft::new();
    draft.chain_id = crate::tron::TRON_CHAIN_ID.to_string();
    for (field, value) in [
        (ChainField::Name, "Tron"),
        (ChainField::NativeName, "Tron"),
        (ChainField::NativeSymbol, "TRX"),
        (ChainField::NativeDecimals, "6"),
    ] {
        draft.fields.insert(field, value.into());
    }
    draft.rpc_endpoints = vec![RpcEndpointDraft::new("https://rpc.example".into())];
    let mut settings = WalletSettings::default();
    let add = ChainEditorCommand::Save {
        draft: draft.clone(),
        existing: false,
    };
    assert_eq!(
        chain_editor_mutation(&settings, &add).err(),
        Some(TRON_CHAIN_REFUSAL.to_owned())
    );
    assert!(chain_editor_probe_chain(&settings, &draft).is_err());

    settings.chains.custom.insert(
        crate::tron::TRON_CHAIN_ID,
        super::super::tests::custom_evm_chain(),
    );
    let edit = ChainEditorCommand::Save {
        draft,
        existing: true,
    };
    assert!(chain_editor_mutation(&settings, &edit).is_ok());
}

fn save_draft(settings: &WalletSettings, draft: ChainDraft) -> WalletSettings {
    chain_editor_mutation(
        settings,
        &ChainEditorCommand::Save {
            draft,
            existing: true,
        },
    )
    .unwrap()
    .prepare(settings)
    .unwrap()
}

#[test]
fn populated_defaults_save_as_inheritance_and_restored_values_remove_overrides() {
    let mut settings = WalletSettings::default();
    settings.gas.gas_limit_buffer += 123;
    let effective = build_effective_chain_configs(&settings).unwrap();
    for &id in railgun_ui::DEFAULT_CHAINS {
        let mut draft = chain_editor_draft(&settings, id).unwrap();
        let config = effective.get(id).unwrap();
        assert_eq!(
            draft.defaults[&ChainField::GasLimitBuffer],
            settings.gas.gas_limit_buffer.to_string(),
            "the draft carries the inherited value the editor shows as the default"
        );
        let preset_rows: Vec<_> = presets::EvmPreset::for_chain(id)
            .unwrap()
            .rpc_endpoints
            .into_iter()
            .map(RpcEndpointDraft::new)
            .collect();
        assert!(
            draft.rpc_endpoints == preset_rows,
            "presets prefill with the default flags"
        );
        assert_eq!(
            draft
                .value(ChainField::MulticallContract)
                .parse::<alloy::primitives::Address>()
                .unwrap(),
            config.rpc_route.multicall().unwrap()
        );
        assert_eq!(
            optional_number(&draft, ChainField::GasLimitBuffer).unwrap(),
            Some(settings.gas.gas_limit_buffer)
        );
        let private = config.railgun.as_ref().unwrap();
        assert_eq!(
            optional_number(&draft, ChainField::ArchiveUntilBlock).unwrap(),
            Some(private.sync.archive_until_block)
        );
        assert_eq!(
            optional_number(&draft, ChainField::BlockRange).unwrap(),
            Some(private.sync.block_range)
        );
        assert_eq!(
            draft.value(ChainField::QuickSyncEndpoint),
            private
                .sync
                .quick_sync_endpoint
                .as_ref()
                .map_or("", url::Url::as_str)
        );
        assert_eq!(
            lines(&draft, ChainField::SponsoredBundleRelays).len(),
            private.sponsored_bundle_relays.len()
        );
        assert_eq!(save_draft(&settings, draft.clone()), settings);

        // A second endpoint must retain Sensitive coverage when the first loses it.
        if preset_rows.len() > 1 {
            // Flagging one preset makes the whole preset list the override.
            let mut flagged = draft.clone();
            flagged.rpc_endpoints[0].sensitive = false;
            let mut expected = settings.clone();
            expected
                .chains
                .per_chain
                .get_mut(&id)
                .unwrap()
                .rpc_endpoints = preset_rows
                .iter()
                .enumerate()
                .map(|(index, preset)| RpcEndpointSetting {
                    sensitive: index != 0,
                    ..preset.url.clone().into()
                })
                .collect();
            assert_eq!(save_draft(&settings, flagged), expected);
        }

        draft.fields.insert(
            ChainField::GasLimitBuffer,
            (settings.gas.gas_limit_buffer + 1).to_string(),
        );
        let changed = save_draft(&settings, draft);
        let mut expected = settings.clone();
        expected
            .chains
            .per_chain
            .get_mut(&id)
            .unwrap()
            .gas
            .gas_limit_buffer = Some(settings.gas.gas_limit_buffer + 1);
        assert_eq!(
            changed, expected,
            "only the edited field becomes an override"
        );

        let summaries = chain_editor_snapshot(&changed, None).unwrap().chains;
        let summary = |chain_id: u64| {
            summaries
                .iter()
                .find(|summary| summary.chain_id == chain_id.to_string())
                .expect("chain summary")
        };
        assert!(
            summary(id).modified,
            "an overridden chain reads as modified"
        );
        assert_eq!(
            summary(id).rpc_endpoints,
            preset_rows.len(),
            "summaries count effective endpoints without carrying the URLs"
        );
        let untouched = railgun_ui::DEFAULT_CHAINS
            .iter()
            .copied()
            .find(|&other| other != id)
            .unwrap();
        assert!(
            !summary(untouched).modified,
            "an untouched chain stays unmarked"
        );

        let mut restored = chain_editor_draft(&changed, id).unwrap();
        restored.fields.insert(
            ChainField::GasLimitBuffer,
            format!(" 0{} ", settings.gas.gas_limit_buffer),
        );
        restored.fields.insert(
            ChainField::MulticallContract,
            restored
                .value(ChainField::MulticallContract)
                .to_ascii_lowercase(),
        );
        assert_eq!(save_draft(&changed, restored), settings);
    }
}

#[test]
fn snapshot_counts_malformed_stored_urls_without_panicking() {
    let summary_count = |settings: &WalletSettings, id: u64| {
        chain_editor_snapshot(settings, None)
            .unwrap()
            .chains
            .into_iter()
            .find(|summary| summary.chain_id == id.to_string())
            .unwrap()
            .rpc_endpoints
    };
    let mut settings = WalletSettings::default();
    settings.chains.per_chain.get_mut(&1).unwrap().rpc_endpoints =
        vec![String::from("not-a-url").into()];
    let mut custom = super::super::tests::custom_evm_chain();
    custom.rpc_endpoints = vec![String::from("not-a-url").into()];
    settings.chains.custom.insert(31337, custom);
    assert_eq!(summary_count(&settings, 1), 1);
    assert_eq!(summary_count(&settings, 31337), 1);
}

#[test]
fn public_preset_drafts_carry_no_railgun_settings() {
    let settings = WalletSettings::default();
    let draft = chain_editor_draft(&settings, 10).unwrap();
    assert!(draft.built_in && !draft.railgun);
    assert!(draft.enabled, "built-in presets ship enabled");
    assert_eq!(
        draft
            .rpc_endpoints
            .iter()
            .map(|endpoint| endpoint.url.clone())
            .collect::<Vec<_>>(),
        presets::EvmPreset::for_chain(10).unwrap().rpc_endpoints
    );

    let mut private = draft;
    private
        .fields
        .insert(ChainField::ArchiveRpcUrl, "https://archive.example".into());
    assert!(
        draft_mutation(&settings, &private, true)
            .err()
            .unwrap()
            .contains("public EVM operations only")
    );
}

#[test]
fn inherited_sync_range_follows_edits_without_overwriting_explicit_values() {
    let mut settings = WalletSettings::default();
    settings
        .chains
        .per_chain
        .get_mut(&1)
        .unwrap()
        .railgun
        .indexed_wallet_block_range = Some(1234);
    let mut draft = chain_editor_draft(&settings, 1).unwrap();
    assert_eq!(
        draft.value(ChainField::QuickSyncIndexedWalletBlockRange),
        "1234"
    );
    assert_eq!(save_draft(&settings, draft.clone()), settings);
    draft
        .fields
        .insert(ChainField::IndexedWalletBlockRange, "2345".into());
    let changed = save_draft(&settings, draft);
    assert_eq!(
        build_effective_chain_configs(&changed)
            .unwrap()
            .get(1)
            .unwrap()
            .railgun
            .as_ref()
            .unwrap()
            .sync
            .indexed_wallet_block_range,
        2345
    );
    assert_eq!(
        changed.chains.per_chain[&1]
            .railgun
            .quick_sync
            .indexed_wallet_block_range,
        None
    );

    let mut draft = chain_editor_draft(&changed, 1).unwrap();
    draft
        .fields
        .insert(ChainField::QuickSyncIndexedWalletBlockRange, "3456".into());
    let explicit = save_draft(&changed, draft);
    assert_eq!(
        explicit.chains.per_chain[&1]
            .railgun
            .quick_sync
            .indexed_wallet_block_range,
        Some(3456)
    );
    assert_eq!(
        save_draft(&explicit, chain_editor_draft(&explicit, 1).unwrap()),
        explicit
    );
    let mut draft = chain_editor_draft(&explicit, 1).unwrap();
    draft
        .fields
        .insert(ChainField::QuickSyncIndexedWalletBlockRange, "2345".into());
    assert_eq!(save_draft(&explicit, draft), changed);
}

#[test]
fn portable_editor_preserves_override_semantics_and_exact_chain_identity() {
    let mut settings = WalletSettings::default();
    let chain = settings.chains.per_chain.get_mut(&1).unwrap();
    chain.rpc_endpoints = vec![
        String::from("https://synthetic:credential@first.example").into(),
        String::from("https://second.example").into(),
        RpcEndpointSetting {
            url: "https://direct.example".into(),
            direct: true,
            submit: false,
            ..RpcEndpointSetting::default()
        },
    ];
    chain.railgun.sponsored_bundle_relays = Some(vec![]);
    chain.railgun.quick_sync.enabled = false;
    chain.railgun.quick_sync.endpoint = Some("https://quick.example".into());
    chain.railgun.deployment.archive_until_block = Some(0);
    chain.railgun.deployment.archive_rpc_url = Some("https://archive.example".into());
    chain.railgun.deployment.archive_rpc_direct = true;
    chain.gas.gas_price_buffer_numerator = Some(12);
    let draft = chain_editor_draft(&settings, 1).unwrap();
    let wire = serde_json::to_vec(&ChainEditorCommand::Save {
        draft,
        existing: true,
    })
    .unwrap();
    let restored = serde_json::from_slice(&wire).unwrap();
    let unchanged = chain_editor_mutation(&settings, &restored)
        .unwrap()
        .prepare(&settings)
        .unwrap();
    assert_eq!(
        unchanged, settings,
        "opening and saving must not materialize or erase overrides"
    );

    let mut draft = ChainDraft::new();
    draft.chain_id = u64::MAX.to_string();
    for (field, value) in [
        (ChainField::Name, "Custom"),
        (ChainField::NativeName, "Custom coin"),
        (ChainField::NativeSymbol, "CSTM"),
        (ChainField::NativeDecimals, "6"),
    ] {
        draft.fields.insert(field, value.into());
    }
    draft.rpc_endpoints = vec![RpcEndpointDraft::new("https://rpc.example".into())];
    let wire = serde_json::to_vec(&ChainEditorCommand::Save {
        draft,
        existing: false,
    })
    .unwrap();
    let command = serde_json::from_slice(&wire).unwrap();
    let next = chain_editor_mutation(&settings, &command)
        .unwrap()
        .prepare(&settings)
        .unwrap();
    let snapshot = chain_editor_snapshot(&next, Some(u64::MAX)).unwrap();
    let draft = snapshot.draft.unwrap();
    assert_eq!(draft.chain_id, "18446744073709551615");
    assert_ne!(draft.value(ChainField::FinalityDepth), "");
    assert_ne!(draft.value(ChainField::GasLimitBuffer), "");
    assert_eq!(draft.value(ChainField::MulticallContract), "");
    assert_eq!(save_draft(&next, draft), next);
    assert!(
        build_effective_chain_configs(&next)
            .unwrap()
            .get(u64::MAX)
            .unwrap()
            .railgun
            .is_none()
    );
    assert_eq!(next.chains.per_chain, settings.chains.per_chain);
}
