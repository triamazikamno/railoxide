//! Native conversion for the shared, untrusted editor draft. No network work occurs here.
use super::{
    ChainContractSettings, ChainDeploymentSettings, ChainGasSettings, ChainMutation,
    ChainSettingsOverride, CustomChainSettings, MAX_CHAIN_MUTATION_BYTES, NativeCurrency,
    NativeUsdPricing, QuickSyncSettings, RailgunSettingsOverride, RpcEndpointSetting,
    WalletSettings, presets, settings_revision,
};
use railgun_ui::chain_editor::{
    ChainDraft, ChainEditorCommand, ChainEditorSnapshot, ChainField, ChainSummary, NativeUsdChoice,
    RpcEndpointDraft,
};

/// Why chain id 728126428 can't be added as a custom EVM chain.
pub const TRON_CHAIN_REFUSAL: &str =
    "Chain 728126428 is Tron, which is configured in Tron settings, not as an EVM chain";

pub fn chain_editor_snapshot(
    settings: &WalletSettings,
    selected: Option<u64>,
) -> Result<ChainEditorSnapshot, String> {
    let mut snapshot = ChainEditorSnapshot::default();
    snapshot.revision = settings_revision(settings)
        .map_err(|error| error.to_string())?
        .to_string();
    snapshot.chains = railgun_ui::built_in_chain_ids()
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
                summary.railgun = railgun_ui::DEFAULT_CHAINS.contains(&id);
                railgun_ui::chain_name(id)
                    .unwrap_or("Chain")
                    .clone_into(&mut summary.name);
                let overrides = settings.chains.per_chain.get(&id);
                summary.enabled = overrides.map_or_else(
                    || ChainSettingsOverride::for_chain(id).enabled,
                    |chain| chain.enabled,
                );
                // Enabled is its own row affordance, so it never counts as a preset deviation.
                summary.modified = overrides.is_some_and(|chain| {
                    let unchanged = ChainSettingsOverride::for_chain(id);
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
    snapshot.direct_network = settings.network.mode == super::NetworkModeSetting::Direct;
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
    if draft.rpc_endpoints.is_empty() {
        draft.rpc_endpoints = defaults.rpc_endpoints;
    }
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
    fill_native_pricing(&mut draft, chain.native_usd_pricing);
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

fn fill_native_pricing(draft: &mut ChainDraft, pricing: NativeUsdPricing) {
    draft.native_usd_pricing = match pricing {
        NativeUsdPricing::Default => NativeUsdChoice::Default,
        NativeUsdPricing::Disabled => NativeUsdChoice::Disabled,
        NativeUsdPricing::Oracle { contract_address } => {
            draft
                .fields
                .insert(ChainField::NativeUsdOracle, contract_address.to_string());
            NativeUsdChoice::Oracle
        }
    };
}

fn built_in_draft(settings: &WalletSettings, id: u64) -> Result<ChainDraft, String> {
    let preset = presets::EvmPreset::for_chain(id).ok_or("Chain is no longer configured")?;
    let defaults = ChainSettingsOverride::for_chain(id);
    let chain = settings.chains.per_chain.get(&id).unwrap_or(&defaults);
    let mut draft = ChainDraft::new();
    draft.chain_id = id.to_string();
    draft.built_in = true;
    draft.railgun = railgun_ui::DEFAULT_CHAINS.contains(&id);
    fill_native_pricing(&mut draft, chain.native_usd_pricing);
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
    if !draft.railgun {
        // Public presets carry no Railgun fields, exactly like a custom chain.
        return Ok(draft);
    }
    let private = &chain.railgun;
    draft.quick_sync_enabled = private.quick_sync.enabled;
    draft.archive_rpc_direct = private.deployment.archive_rpc_direct;
    draft.use_default_relays = private.sponsored_bundle_relays.is_none();
    for (field, value) in [
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
        chain.native_usd_pricing = NativeUsdPricing::Default;
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
    if let Some(oracle) = chain.native_usd_oracle {
        draft
            .fields
            .insert(ChainField::NativeUsdOracle, oracle.to_string());
    }
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
    // With the override removed above, a built-in chain resolves to its preset endpoints.
    draft.rpc_endpoints = presets::EvmPreset::for_chain(id)
        .map(|preset| {
            preset
                .rpc_endpoints
                .into_iter()
                .map(RpcEndpointDraft::new)
                .collect()
        })
        .unwrap_or_default();
    for (field, value) in [
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
    let Some(private) = chain.railgun else {
        return Ok(draft);
    };
    for (field, value) in [
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
    if same_endpoints(&draft.rpc_endpoints, &defaults.rpc_endpoints) {
        draft.rpc_endpoints.clear();
    }
    for (field, value) in defaults.fields {
        if field == ChainField::NativeUsdOracle {
            continue;
        }
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

/// Equal flags in the same order, with URLs compared like other inherited URL fields.
fn same_endpoints(left: &[RpcEndpointDraft], right: &[RpcEndpointDraft]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            (left.direct, left.general, left.sensitive, left.submit)
                == (right.direct, right.general, right.sensitive, right.submit)
                && same_url(&left.url, &right.url)
        })
}

fn same_url(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    left == right
        || matches!((url::Url::parse(left), url::Url::parse(right)), (Ok(left), Ok(right)) if left == right)
}

fn same_field_value(field: ChainField, left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left == right {
        return true;
    }
    match field {
        ChainField::WrappedNativeToken | ChainField::MulticallContract => {
            matches!((left.parse::<alloy::primitives::Address>(), right.parse::<alloy::primitives::Address>()), (Ok(left), Ok(right)) if left == right)
        }
        ChainField::SponsoredBundleRelays => {
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
        ChainField::QuickSyncEndpoint => same_url(left, right),
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
    endpoints: &[RpcEndpointSetting],
    explorers: &[String],
    enabled: bool,
    contracts: &ChainContractSettings,
    finality: Option<u64>,
    gas: &ChainGasSettings,
) {
    draft.enabled = enabled;
    draft.rpc_endpoints = endpoints
        .iter()
        .map(|endpoint| {
            let mut row = RpcEndpointDraft::new(endpoint.url.clone());
            row.direct = endpoint.direct;
            row.general = endpoint.general;
            row.sensitive = endpoint.sensitive;
            row.submit = endpoint.submit;
            row
        })
        .collect();
    for (field, value) in [
        (ChainField::Name, name.to_owned()),
        (ChainField::NativeName, native.name.clone()),
        (ChainField::NativeSymbol, native.symbol.clone()),
        (ChainField::NativeDecimals, native.decimals.to_string()),
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
        ChainEditorCommand::List
        | ChainEditorCommand::Inspect { .. }
        | ChainEditorCommand::Probe { .. } => {
            Err("This command does not change a chain".to_owned())
        }
    }
}

/// Validates a Test draft exactly like Save and resolves its effective chain. The candidate
/// settings are discarded here: no caller may persist them.
pub fn chain_editor_probe_chain(
    settings: &WalletSettings,
    draft: &ChainDraft,
) -> Result<super::EffectiveChainConfig, String> {
    let id = editor_chain_id(&draft.chain_id)?;
    let candidate = chain_editor_mutation(
        settings,
        &ChainEditorCommand::Save {
            draft: draft.clone(),
            existing: settings.chains.contains(id),
        },
    )?
    .prepare(settings)
    .map_err(|error| error.to_string())?;
    super::build_effective_chain_configs(&candidate)
        .map_err(|error| error.to_string())?
        .get(id)
        .cloned()
        .ok_or_else(|| "Chain is no longer configured".to_owned())
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

/// Endpoint rows with a URL, as settings entries. Empty rows are dropped like empty list lines.
fn endpoint_settings(draft: &ChainDraft) -> Vec<RpcEndpointSetting> {
    draft
        .rpc_endpoints
        .iter()
        .filter(|endpoint| !endpoint.url.trim().is_empty())
        .map(|endpoint| RpcEndpointSetting {
            url: endpoint.url.trim().to_owned(),
            direct: endpoint.direct,
            general: endpoint.general,
            sensitive: endpoint.sensitive,
            submit: endpoint.submit,
        })
        .collect()
}

fn draft_mutation(
    settings: &WalletSettings,
    draft: &ChainDraft,
    existing: bool,
) -> Result<ChainMutation, String> {
    let id = editor_chain_id(&draft.chain_id)?;
    // Not in settings validation: beta.13 accepted this id, and a saved chain must keep loading.
    if id == crate::tron::TRON_CHAIN_ID && !settings.chains.contains(id) {
        return Err(TRON_CHAIN_REFUSAL.to_owned());
    }
    let built_in = railgun_ui::is_built_in_chain(id);
    let has_railgun = railgun_ui::DEFAULT_CHAINS.contains(&id);
    if draft.built_in != built_in
        || draft.railgun != has_railgun
        || settings.chains.contains(id) != existing
    {
        return Err("Chain identity changed. Reload the chain list before saving".to_owned());
    }
    let mut normalized = draft.clone();
    if existing {
        remove_inherited_values(settings, &mut normalized, id)?;
    }
    let draft = &normalized;
    let native_usd_pricing = match draft.native_usd_pricing {
        NativeUsdChoice::Default => NativeUsdPricing::Default,
        NativeUsdChoice::Disabled => NativeUsdPricing::Disabled,
        NativeUsdChoice::Oracle => NativeUsdPricing::Oracle {
            contract_address: draft
                .value(ChainField::NativeUsdOracle)
                .trim()
                .parse()
                .map_err(|_| "Oracle contract must be a valid Ethereum address")?,
        },
    };
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
        let railgun = if has_railgun {
            RailgunSettingsOverride {
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
                deployment: ChainDeploymentSettings {
                    archive_until_block: optional_number(draft, ChainField::ArchiveUntilBlock)?,
                    archive_rpc_url: optional_text(draft, ChainField::ArchiveRpcUrl),
                    archive_rpc_direct: draft.archive_rpc_direct,
                },
                block_range: optional_number(draft, ChainField::BlockRange)?,
                poll_interval_secs: optional_number(draft, ChainField::PollIntervalSecs)?,
                indexed_wallet_block_range: optional_number(
                    draft,
                    ChainField::IndexedWalletBlockRange,
                )?,
            }
        } else {
            if draft
                .fields
                .iter()
                .any(|(field, value)| field.is_railgun() && !value.is_empty())
            {
                return Err("Built-in public chains support public EVM operations only".to_owned());
            }
            RailgunSettingsOverride::default()
        };
        Ok(ChainMutation::EditBuiltIn {
            chain_id: id,
            overrides: ChainSettingsOverride {
                native_usd_pricing,
                enabled: draft.enabled,
                rpc_endpoints: endpoint_settings(draft),
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
            native_usd_pricing,
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
            rpc_endpoints: endpoint_settings(draft),
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
mod tests;
