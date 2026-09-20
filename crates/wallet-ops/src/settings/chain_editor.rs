//! Native conversion for the shared, untrusted editor draft. No network work occurs here.
use super::{
    ChainContractSettings, ChainDeploymentSettings, ChainGasSettings, ChainMutation,
    ChainSettingsOverride, CustomChainSettings, MAX_CHAIN_MUTATION_BYTES, NativeCurrency,
    QuickSyncSettings, RailgunContractSettings, RailgunSettingsOverride, WalletSettings, presets,
    settings_revision,
};
use railgun_ui::chain_editor::{
    ChainDraft, ChainEditorCommand, ChainEditorSnapshot, ChainField, ChainSummary,
};

pub fn chain_editor_snapshot(
    settings: &WalletSettings,
    selected: Option<u64>,
) -> Result<ChainEditorSnapshot, String> {
    let mut snapshot = ChainEditorSnapshot::default();
    snapshot.revision = settings_revision(settings)
        .map_err(|error| error.to_string())?
        .to_string();
    snapshot.chains = railgun_ui::DEFAULT_CHAINS
        .iter()
        .copied()
        .chain(settings.chains.custom.keys().copied())
        .map(|id| {
            let mut summary = ChainSummary::default();
            summary.chain_id = id.to_string();
            if let Some(chain) = settings.chains.custom.get(&id) {
                summary.name.clone_from(&chain.name);
                summary.enabled = chain.enabled;
                summary.rpc_endpoints = chain.rpc_endpoints.len();
            } else {
                summary.built_in = true;
                railgun_ui::chain_name(id)
                    .unwrap_or("Chain")
                    .clone_into(&mut summary.name);
                let overrides = settings.chains.per_chain.get(&id);
                summary.enabled = overrides.is_none_or(|chain| chain.enabled);
                // Enabled is its own row affordance, so it never counts as a preset deviation.
                summary.modified = overrides.is_some_and(|chain| {
                    let unchanged = ChainSettingsOverride::default();
                    let mut compared = chain.clone();
                    compared.enabled = unchanged.enabled;
                    compared != unchanged
                });
                summary.rpc_endpoints = match overrides {
                    Some(chain) if !chain.rpc_endpoints.is_empty() => chain.rpc_endpoints.len(),
                    _ => presets::EvmPreset::for_chain(id)
                        .map_or(0, |preset| preset.rpc_endpoints.len()),
                };
            }
            summary
        })
        .collect();
    snapshot.draft = selected
        .map(|id| chain_editor_draft(settings, id))
        .transpose()?;
    Ok(snapshot)
}

pub fn chain_editor_draft(settings: &WalletSettings, id: u64) -> Result<ChainDraft, String> {
    let mut draft = stored_draft(settings, id)?;
    let mut defaults = inherited_draft(settings, id)?;
    // The quick-sync range inherits the general indexed range before the preset.
    if !draft.value(ChainField::IndexedWalletBlockRange).is_empty() {
        defaults.fields.insert(
            ChainField::QuickSyncIndexedWalletBlockRange,
            draft.value(ChainField::IndexedWalletBlockRange).to_owned(),
        );
    }
    draft.defaults = defaults.fields.clone();
    for (field, value) in defaults.fields {
        if draft.value(field).is_empty()
            && (field != ChainField::SponsoredBundleRelays || draft.use_default_relays)
        {
            draft.fields.insert(field, value);
        }
    }
    Ok(draft)
}

fn stored_draft(settings: &WalletSettings, id: u64) -> Result<ChainDraft, String> {
    let mut draft = ChainDraft::new();
    draft.chain_id = id.to_string();
    let Some(chain) = settings.chains.custom.get(&id) else {
        return built_in_draft(settings, id);
    };
    fill_general_draft(
        &mut draft,
        &chain.name,
        &chain.native_currency,
        &chain.rpc_endpoints,
        &chain.explorer_urls,
        chain.enabled,
        &chain.contracts,
        chain.finality_depth,
        &chain.gas,
    );
    Ok(draft)
}

fn built_in_draft(settings: &WalletSettings, id: u64) -> Result<ChainDraft, String> {
    let preset = presets::EvmPreset::for_chain(id).ok_or("Chain is no longer configured")?;
    let defaults = ChainSettingsOverride::default();
    let chain = settings.chains.per_chain.get(&id).unwrap_or(&defaults);
    let mut draft = ChainDraft::new();
    draft.chain_id = id.to_string();
    draft.built_in = true;
    fill_general_draft(
        &mut draft,
        preset.name,
        &preset.native_currency,
        &chain.rpc_endpoints,
        &[],
        chain.enabled,
        &chain.contracts,
        chain.finality_depth,
        &chain.gas,
    );
    let private = &chain.railgun;
    draft.quick_sync_enabled = private.quick_sync.enabled;
    draft.use_default_relays = private.sponsored_bundle_relays.is_none();
    for (field, value) in [
        (
            ChainField::RailgunContract,
            &private.contracts.railgun_contract,
        ),
        (
            ChainField::RelayAdaptContract,
            &private.contracts.relay_adapt_contract,
        ),
        (
            ChainField::RelayAdapt7702Contract,
            &private.contracts.relay_adapt_7702_contract,
        ),
        (ChainField::CoinbasePayer, &private.contracts.coinbase_payer),
        (
            ChainField::ArchiveRpcUrl,
            &private.deployment.archive_rpc_url,
        ),
        (ChainField::QuickSyncEndpoint, &private.quick_sync.endpoint),
    ] {
        draft
            .fields
            .insert(field, value.clone().unwrap_or_default());
    }
    for (field, value) in [
        (
            ChainField::DeploymentBlock,
            private.deployment.deployment_block,
        ),
        (ChainField::V2StartBlock, private.deployment.v2_start_block),
        (
            ChainField::LegacyShieldBlock,
            private.deployment.legacy_shield_block,
        ),
        (
            ChainField::ArchiveUntilBlock,
            private.deployment.archive_until_block,
        ),
        (
            ChainField::QuickSyncIndexedWalletBlockRange,
            private.quick_sync.indexed_wallet_block_range,
        ),
        (ChainField::BlockRange, private.block_range),
        (ChainField::PollIntervalSecs, private.poll_interval_secs),
        (
            ChainField::IndexedWalletBlockRange,
            private.indexed_wallet_block_range,
        ),
    ] {
        draft.fields.insert(
            field,
            value.map(|value| value.to_string()).unwrap_or_default(),
        );
    }
    draft.fields.insert(
        ChainField::SponsoredBundleRelays,
        private
            .sponsored_bundle_relays
            .as_ref()
            .map(|values| values.join("\n"))
            .unwrap_or_default(),
    );
    Ok(draft)
}

/// Use the runtime resolver as the source of inherited values, including global gas settings.
fn inherited_draft(settings: &WalletSettings, id: u64) -> Result<ChainDraft, String> {
    let mut inherited = settings.clone();
    if let Some(chain) = inherited.chains.custom.get_mut(&id) {
        chain.contracts = ChainContractSettings::default();
        chain.finality_depth = None;
        chain.gas = ChainGasSettings::default();
    } else {
        inherited.chains.per_chain.remove(&id);
    }
    let chain = super::build_effective_chain_configs(&inherited)
        .map_err(|error| error.to_string())?
        .get(id)
        .cloned()
        .ok_or("Chain is no longer configured")?;
    let mut draft = ChainDraft::new();
    // Custom identity, endpoints and optional contracts are definitions, never inherited.
    for (field, value) in [
        (ChainField::FinalityDepth, chain.finality_depth),
        (ChainField::GasLimitBuffer, chain.gas.gas_limit_buffer),
        (
            ChainField::GasPriceBufferNumerator,
            chain.gas.gas_price_buffer_numerator,
        ),
        (
            ChainField::GasPriceBufferDenominator,
            chain.gas.gas_price_buffer_denominator,
        ),
    ] {
        draft.fields.insert(field, value.to_string());
    }
    if !chain.built_in {
        return Ok(draft);
    }
    for (field, value) in [
        (
            ChainField::RpcEndpoints,
            chain
                .rpc_route
                .endpoints()
                .iter()
                .map(|url| url.expose_url().as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            ChainField::WrappedNativeToken,
            chain
                .wrapped_native_token
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
        (
            ChainField::MulticallContract,
            chain
                .rpc_route
                .multicall()
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
    ] {
        draft.fields.insert(field, value);
    }
    let private = chain.railgun.expect("built-in Railgun configuration");
    for (field, value) in [
        (
            ChainField::RailgunContract,
            Some(private.deployment.contract),
        ),
        (
            ChainField::RelayAdaptContract,
            Some(private.deployment.relay_adapt_contract),
        ),
        (
            ChainField::RelayAdapt7702Contract,
            Some(private.deployment.relay_adapt_7702_contract),
        ),
        (ChainField::CoinbasePayer, private.coinbase_payer),
    ] {
        draft.fields.insert(
            field,
            value.map(|value| value.to_string()).unwrap_or_default(),
        );
    }
    for (field, value) in [
        (
            ChainField::DeploymentBlock,
            private.deployment.deployment_block,
        ),
        (ChainField::V2StartBlock, private.deployment.v2_start_block),
        (
            ChainField::LegacyShieldBlock,
            private.deployment.legacy_shield_block,
        ),
        (
            ChainField::ArchiveUntilBlock,
            private.sync.archive_until_block,
        ),
        (ChainField::BlockRange, private.sync.block_range),
        (
            ChainField::PollIntervalSecs,
            private.sync.poll_interval.as_secs(),
        ),
        (
            ChainField::IndexedWalletBlockRange,
            private.sync.indexed_wallet_block_range,
        ),
        (
            ChainField::QuickSyncIndexedWalletBlockRange,
            private.sync.indexed_wallet_block_range,
        ),
    ] {
        draft.fields.insert(field, value.to_string());
    }
    draft.fields.insert(
        ChainField::QuickSyncEndpoint,
        private
            .sync
            .quick_sync_endpoint
            .map(|url| url.to_string())
            .unwrap_or_default(),
    );
    draft.fields.insert(
        ChainField::SponsoredBundleRelays,
        private
            .sponsored_bundle_relays
            .iter()
            .map(|url| url.expose_url().as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    Ok(draft)
}

fn remove_inherited_values(
    settings: &WalletSettings,
    draft: &mut ChainDraft,
    id: u64,
) -> Result<(), String> {
    let mut defaults = inherited_draft(settings, id)?;
    if draft.built_in {
        let original = stored_draft(settings, id)?;
        let original_display = chain_editor_draft(settings, id)?;
        let field = ChainField::QuickSyncIndexedWalletBlockRange;
        if original.value(field).is_empty()
            && same_field_value(field, draft.value(field), original_display.value(field))
        {
            draft.fields.remove(&field);
        }
        if let Some(range) = optional_number(draft, ChainField::IndexedWalletBlockRange)? {
            defaults.fields.insert(field, range.to_string());
        }
    }
    for (field, value) in defaults.fields {
        // Keep explicit empty relay lists distinct from preset inheritance.
        if field == ChainField::SponsoredBundleRelays {
            if !lines(draft, field).is_empty()
                && same_field_value(field, draft.value(field), &value)
            {
                draft.use_default_relays = true;
            }
        } else if same_field_value(field, draft.value(field), &value) {
            draft.fields.remove(&field);
        }
    }
    Ok(())
}

fn same_field_value(field: ChainField, left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left == right {
        return true;
    }
    match field {
        ChainField::WrappedNativeToken
        | ChainField::MulticallContract
        | ChainField::RailgunContract
        | ChainField::RelayAdaptContract
        | ChainField::RelayAdapt7702Contract
        | ChainField::CoinbasePayer => {
            matches!((left.parse::<alloy::primitives::Address>(), right.parse::<alloy::primitives::Address>()), (Ok(left), Ok(right)) if left == right)
        }
        ChainField::RpcEndpoints | ChainField::SponsoredBundleRelays => {
            let parse = |value: &str| {
                value
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(url::Url::parse)
                    .collect::<Result<Vec<_>, _>>()
            };
            matches!((parse(left), parse(right)), (Ok(left), Ok(right)) if left == right)
        }
        ChainField::QuickSyncEndpoint => {
            matches!((url::Url::parse(left), url::Url::parse(right)), (Ok(left), Ok(right)) if left == right)
        }
        _ => {
            matches!((left.parse::<u64>(), right.parse::<u64>()), (Ok(left), Ok(right)) if left == right)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_general_draft(
    draft: &mut ChainDraft,
    name: &str,
    native: &NativeCurrency,
    endpoints: &[String],
    explorers: &[String],
    enabled: bool,
    contracts: &ChainContractSettings,
    finality: Option<u64>,
    gas: &ChainGasSettings,
) {
    draft.enabled = enabled;
    for (field, value) in [
        (ChainField::Name, name.to_owned()),
        (ChainField::NativeName, native.name.clone()),
        (ChainField::NativeSymbol, native.symbol.clone()),
        (ChainField::NativeDecimals, native.decimals.to_string()),
        (ChainField::RpcEndpoints, endpoints.join("\n")),
        (ChainField::ExplorerUrls, explorers.join("\n")),
        (
            ChainField::WrappedNativeToken,
            contracts.wrapped_native_token.clone().unwrap_or_default(),
        ),
        (
            ChainField::MulticallContract,
            contracts.multicall_contract.clone().unwrap_or_default(),
        ),
    ] {
        draft.fields.insert(field, value);
    }
    for (field, value) in [
        (ChainField::FinalityDepth, finality),
        (ChainField::GasLimitBuffer, gas.gas_limit_buffer),
        (
            ChainField::GasPriceBufferNumerator,
            gas.gas_price_buffer_numerator,
        ),
        (
            ChainField::GasPriceBufferDenominator,
            gas.gas_price_buffer_denominator,
        ),
    ] {
        draft.fields.insert(
            field,
            value.map(|value| value.to_string()).unwrap_or_default(),
        );
    }
}

pub fn chain_editor_mutation(
    settings: &WalletSettings,
    command: &ChainEditorCommand,
) -> Result<ChainMutation, String> {
    if serde_json::to_vec(command)
        .map_err(|_| "Invalid chain edit")?
        .len()
        > MAX_CHAIN_MUTATION_BYTES
    {
        return Err("Chain edit is too large".to_owned());
    }
    match command {
        ChainEditorCommand::Save { draft, existing } => draft_mutation(settings, draft, *existing),
        ChainEditorCommand::Remove { chain_id } => Ok(ChainMutation::Remove {
            chain_id: editor_chain_id(chain_id)?,
        }),
        ChainEditorCommand::Reset { chain_id } => Ok(ChainMutation::ResetBuiltIn {
            chain_id: editor_chain_id(chain_id)?,
        }),
        ChainEditorCommand::List | ChainEditorCommand::Inspect { .. } => {
            Err("This command does not change a chain".to_owned())
        }
    }
}

pub fn editor_chain_id(value: &str) -> Result<u64, String> {
    value.parse().map_err(|_| {
        "Chain ID must be a decimal integer between 0 and 18446744073709551615".to_owned()
    })
}

fn optional_number(draft: &ChainDraft, field: ChainField) -> Result<Option<u64>, String> {
    let value = draft.value(field).trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| format!("{} must be an unsigned integer", field.label()))
}

fn optional_text(draft: &ChainDraft, field: ChainField) -> Option<String> {
    let value = draft.value(field).trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn lines(draft: &ChainDraft, field: ChainField) -> Vec<String> {
    draft
        .value(field)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn draft_mutation(
    settings: &WalletSettings,
    draft: &ChainDraft,
    existing: bool,
) -> Result<ChainMutation, String> {
    let id = editor_chain_id(&draft.chain_id)?;
    let built_in = railgun_ui::DEFAULT_CHAINS.contains(&id);
    if draft.built_in != built_in || settings.chains.contains(id) != existing {
        return Err("Chain identity changed. Reload the chain list before saving".to_owned());
    }
    let mut normalized = draft.clone();
    if existing {
        remove_inherited_values(settings, &mut normalized, id)?;
    }
    let draft = &normalized;
    let contracts = ChainContractSettings {
        wrapped_native_token: optional_text(draft, ChainField::WrappedNativeToken),
        multicall_contract: optional_text(draft, ChainField::MulticallContract),
    };
    let gas = ChainGasSettings {
        gas_limit_buffer: optional_number(draft, ChainField::GasLimitBuffer)?,
        gas_price_buffer_numerator: optional_number(draft, ChainField::GasPriceBufferNumerator)?,
        gas_price_buffer_denominator: optional_number(
            draft,
            ChainField::GasPriceBufferDenominator,
        )?,
    };
    let finality_depth = optional_number(draft, ChainField::FinalityDepth)?;
    if built_in {
        let original = built_in_draft(settings, id)?;
        if ChainField::ALL
            .iter()
            .any(|field| field.is_identity() && draft.value(*field) != original.value(*field))
        {
            return Err("Built-in chain identity cannot be changed".to_owned());
        }
        let railgun = RailgunSettingsOverride {
            sponsored_bundle_relays: (!draft.use_default_relays)
                .then(|| lines(draft, ChainField::SponsoredBundleRelays)),
            quick_sync: QuickSyncSettings {
                enabled: draft.quick_sync_enabled,
                endpoint: optional_text(draft, ChainField::QuickSyncEndpoint),
                indexed_wallet_block_range: optional_number(
                    draft,
                    ChainField::QuickSyncIndexedWalletBlockRange,
                )?,
            },
            contracts: RailgunContractSettings {
                railgun_contract: optional_text(draft, ChainField::RailgunContract),
                relay_adapt_contract: optional_text(draft, ChainField::RelayAdaptContract),
                relay_adapt_7702_contract: optional_text(draft, ChainField::RelayAdapt7702Contract),
                coinbase_payer: optional_text(draft, ChainField::CoinbasePayer),
            },
            deployment: ChainDeploymentSettings {
                deployment_block: optional_number(draft, ChainField::DeploymentBlock)?,
                v2_start_block: optional_number(draft, ChainField::V2StartBlock)?,
                legacy_shield_block: optional_number(draft, ChainField::LegacyShieldBlock)?,
                archive_until_block: optional_number(draft, ChainField::ArchiveUntilBlock)?,
                archive_rpc_url: optional_text(draft, ChainField::ArchiveRpcUrl),
            },
            block_range: optional_number(draft, ChainField::BlockRange)?,
            poll_interval_secs: optional_number(draft, ChainField::PollIntervalSecs)?,
            indexed_wallet_block_range: optional_number(
                draft,
                ChainField::IndexedWalletBlockRange,
            )?,
        };
        Ok(ChainMutation::EditBuiltIn {
            chain_id: id,
            overrides: ChainSettingsOverride {
                enabled: draft.enabled,
                rpc_endpoints: lines(draft, ChainField::RpcEndpoints),
                contracts,
                finality_depth,
                gas,
                railgun,
            },
        })
    } else {
        if draft
            .fields
            .iter()
            .any(|(field, value)| field.is_railgun() && !value.is_empty())
        {
            return Err("Custom chains support public EVM operations only".to_owned());
        }
        let definition = CustomChainSettings {
            name: draft.value(ChainField::Name).trim().to_owned(),
            native_currency: NativeCurrency {
                name: draft.value(ChainField::NativeName).trim().to_owned(),
                symbol: draft.value(ChainField::NativeSymbol).trim().to_owned(),
                decimals: draft
                    .value(ChainField::NativeDecimals)
                    .trim()
                    .parse()
                    .map_err(|_| "Native currency decimals must be between 0 and 255")?,
            },
            rpc_endpoints: lines(draft, ChainField::RpcEndpoints),
            explorer_urls: lines(draft, ChainField::ExplorerUrls),
            enabled: draft.enabled,
            contracts,
            finality_depth,
            gas,
        };
        Ok(if existing {
            ChainMutation::EditCustom {
                chain_id: id,
                definition,
            }
        } else {
            ChainMutation::Add {
                chain_id: id,
                definition,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::build_effective_chain_configs;

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
            assert_eq!(
                lines(&draft, ChainField::RpcEndpoints),
                config
                    .rpc_route
                    .endpoints()
                    .iter()
                    .map(|url| url.expose_url().to_string())
                    .collect::<Vec<_>>()
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
                optional_number(&draft, ChainField::DeploymentBlock).unwrap(),
                Some(private.deployment.deployment_block)
            );
            assert_eq!(
                optional_number(&draft, ChainField::BlockRange).unwrap(),
                Some(private.sync.block_range)
            );
            assert_eq!(
                draft.value(ChainField::QuickSyncEndpoint),
                private.sync.quick_sync_endpoint.as_ref().unwrap().as_str()
            );
            assert_eq!(
                lines(&draft, ChainField::SponsoredBundleRelays).len(),
                private.sponsored_bundle_relays.len()
            );
            assert_eq!(save_draft(&settings, draft.clone()), settings);

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
                config.rpc_route.endpoints().len(),
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
            "https://synthetic:credential@first.example".into(),
            "https://second.example".into(),
        ];
        chain.railgun.sponsored_bundle_relays = Some(vec![]);
        chain.railgun.quick_sync.enabled = false;
        chain.railgun.quick_sync.endpoint = Some("https://quick.example".into());
        chain.railgun.contracts.relay_adapt_7702_contract =
            Some("0x1111111111111111111111111111111111111111".into());
        chain.railgun.deployment.archive_until_block = Some(0);
        chain.railgun.deployment.archive_rpc_url = Some("https://archive.example".into());
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
            (ChainField::RpcEndpoints, "https://rpc.example"),
        ] {
            draft.fields.insert(field, value.into());
        }
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
        assert!(!draft.value(ChainField::FinalityDepth).is_empty());
        assert!(!draft.value(ChainField::GasLimitBuffer).is_empty());
        assert!(draft.value(ChainField::MulticallContract).is_empty());
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
}
