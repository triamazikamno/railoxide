use super::*;

pub(in crate::root) fn display_token_entries(settings: &WalletSettings) -> Vec<DisplayTokenEntry> {
    let custom_indexes = settings
        .tokens
        .custom_tokens
        .iter()
        .enumerate()
        .map(|(index, token)| {
            (
                normalized_token_key(token.chain_id, &token.token_address),
                index,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut entries = match build_effective_token_registry(settings) {
        Ok(registry) => registry
            .tokens
            .into_values()
            .map(|token| {
                let key = normalized_token_key(token.chain_id, &token.token_address);
                DisplayTokenEntry {
                    chain_id: token.chain_id,
                    token_address: token.token_address,
                    symbol: token.symbol,
                    decimals: token.decimals,
                    icon_path: token.icon_path,
                    built_in: token.built_in,
                    custom_index: custom_indexes.get(&key).copied(),
                }
            })
            .collect(),
        Err(_) => default_token_entries(),
    };
    entries.sort_by(|left, right| {
        (
            left.chain_id,
            left.symbol.to_ascii_lowercase(),
            left.token_address.to_ascii_lowercase(),
        )
            .cmp(&(
                right.chain_id,
                right.symbol.to_ascii_lowercase(),
                right.token_address.to_ascii_lowercase(),
            ))
    });
    entries
}

pub(in crate::root) fn default_token_entries() -> Vec<DisplayTokenEntry> {
    railgun_ui::DEFAULT_CHAINS
        .iter()
        .flat_map(|chain_id| railgun_ui::known_tokens_for_chain(*chain_id))
        .map(|token| DisplayTokenEntry {
            chain_id: token.chain_id,
            token_address: token.token.to_string(),
            symbol: token.symbol.to_string(),
            decimals: token.decimals,
            icon_path: None,
            built_in: true,
            custom_index: None,
        })
        .collect()
}

pub(in crate::root) fn display_price_anchor_entries(
    settings: &WalletSettings,
) -> Vec<DisplayPriceAnchorEntry> {
    let token_symbols = display_token_entries(settings)
        .into_iter()
        .map(|token| {
            (
                normalized_token_key(token.chain_id, &token.token_address),
                token.symbol,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut entries = default_token_price_anchor_overrides()
        .into_iter()
        .map(|anchor| {
            let key = token_key_tuple(&anchor.key);
            (
                key.clone(),
                DisplayPriceAnchorEntry {
                    key: anchor.key,
                    price_anchor: anchor.price_anchor,
                    token_symbol: token_symbols.get(&key).cloned(),
                    built_in_default: true,
                    override_index: None,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    for (index, anchor) in settings.tokens.price_anchors.iter().enumerate() {
        let key = token_key_tuple(&anchor.key);
        entries.insert(
            key.clone(),
            DisplayPriceAnchorEntry {
                key: anchor.key.clone(),
                price_anchor: anchor.price_anchor.clone(),
                token_symbol: token_symbols.get(&key).cloned(),
                built_in_default: false,
                override_index: Some(index),
            },
        );
    }

    entries.into_values().collect()
}

pub(in crate::root) fn remove_display_price_anchor_override(
    settings: &mut WalletSettings,
    entry: &DisplayPriceAnchorEntry,
) {
    if let Some(index) = display_price_anchor_override_index(settings, entry) {
        settings.tokens.price_anchors.remove(index);
    }
}

pub(in crate::root) fn display_price_anchor_override_index(
    settings: &WalletSettings,
    entry: &DisplayPriceAnchorEntry,
) -> Option<usize> {
    entry
        .override_index
        .filter(|index| *index < settings.tokens.price_anchors.len())
        .or_else(|| {
            settings
                .tokens
                .price_anchors
                .iter()
                .position(|anchor| token_keys_match(&anchor.key, &entry.key))
        })
}

pub(in crate::root) fn price_anchor_dialog_values(
    settings: &WalletSettings,
    target: &PriceAnchorEditTarget,
) -> PriceAnchorDialogValues {
    match target {
        PriceAnchorEditTarget::Add => default_price_anchor_dialog_values(),
        PriceAnchorEditTarget::Edit(entry) => {
            let anchor = current_display_price_anchor_for_entry(settings, entry);
            price_anchor_dialog_values_from_override(&anchor)
        }
    }
}

pub(in crate::root) fn current_display_price_anchor_for_entry(
    settings: &WalletSettings,
    entry: &DisplayPriceAnchorEntry,
) -> TokenPriceAnchorOverride {
    display_price_anchor_override_index(settings, entry).map_or_else(
        || TokenPriceAnchorOverride {
            key: entry.key.clone(),
            price_anchor: entry.price_anchor.clone(),
        },
        |index| settings.tokens.price_anchors[index].clone(),
    )
}

pub(in crate::root) fn default_price_anchor_dialog_values() -> PriceAnchorDialogValues {
    PriceAnchorDialogValues {
        chain_id: railgun_ui::DEFAULT_CHAINS[0],
        token_address: Address::ZERO.to_string(),
        anchor_type: "fixed",
        fixed_rate: fixed_anchor_rate_value(&PriceAnchorSettings::default()),
        oracle_chain_id: railgun_ui::DEFAULT_CHAINS[0],
        oracle_address: Address::ZERO.to_string(),
        oracle_token_decimals: "18".to_string(),
        oracle_decimals: "8".to_string(),
        oracle_is_inversed: false,
        twap_pool_address: Address::ZERO.to_string(),
        twap_base_token_address: Address::ZERO.to_string(),
        twap_quote_token_address: Address::ZERO.to_string(),
        twap_base_token_decimals: "18".to_string(),
        twap_window_seconds: "1800".to_string(),
        product_scale_decimals: "18".to_string(),
        product_components: default_price_anchor_component_dialog_values(),
    }
}

pub(in crate::root) fn default_price_anchor_component_dialog_values()
-> Vec<PriceAnchorComponentDialogValues> {
    vec![
        price_anchor_component_dialog_values_from_anchor(&default_price_anchor_for_type("oracle")),
        price_anchor_component_dialog_values_from_anchor(&default_price_anchor_for_type("oracle")),
    ]
}

pub(in crate::root) fn price_anchor_dialog_values_from_override(
    anchor: &TokenPriceAnchorOverride,
) -> PriceAnchorDialogValues {
    let mut values = default_price_anchor_dialog_values();
    values.chain_id = anchor.key.chain_id;
    values.token_address.clone_from(&anchor.key.token_address);
    match &anchor.price_anchor {
        PriceAnchorSettings::Fixed { rate } => {
            values.anchor_type = "fixed";
            values.fixed_rate.clone_from(rate);
        }
        PriceAnchorSettings::Oracle {
            chain_id,
            oracle_address,
            token_decimals,
            oracle_decimals,
            is_inversed,
        } => {
            values.anchor_type = "oracle";
            values.oracle_chain_id = *chain_id;
            values.oracle_address.clone_from(oracle_address);
            values.oracle_token_decimals = token_decimals.to_string();
            values.oracle_decimals = oracle_decimals.to_string();
            values.oracle_is_inversed = *is_inversed;
        }
        PriceAnchorSettings::UniswapV3Twap {
            pool_address,
            base_token_address,
            quote_token_address,
            base_token_decimals,
            window_seconds,
        } => {
            values.anchor_type = "uniswap-v3-twap";
            values.twap_pool_address.clone_from(pool_address);
            values
                .twap_base_token_address
                .clone_from(base_token_address);
            values
                .twap_quote_token_address
                .clone_from(quote_token_address);
            values.twap_base_token_decimals = base_token_decimals.to_string();
            values.twap_window_seconds = window_seconds.to_string();
        }
        PriceAnchorSettings::Product {
            components,
            scale_decimals,
        } => {
            values.anchor_type = "product";
            values.product_scale_decimals = scale_decimals.to_string();
            values.product_components = components
                .iter()
                .take(2)
                .map(price_anchor_component_dialog_values_from_anchor)
                .collect();
            while values.product_components.len() < 2 {
                values
                    .product_components
                    .push(price_anchor_component_dialog_values_from_anchor(
                        &default_price_anchor_for_type("oracle"),
                    ));
            }
        }
    }
    values
}

#[cfg(test)]
pub(in crate::root) fn price_anchor_dialog_values_from_entry(
    entry: &DisplayPriceAnchorEntry,
) -> PriceAnchorDialogValues {
    price_anchor_dialog_values_from_override(&TokenPriceAnchorOverride {
        key: entry.key.clone(),
        price_anchor: entry.price_anchor.clone(),
    })
}

pub(in crate::root) fn price_anchor_component_dialog_values_from_anchor(
    anchor: &PriceAnchorSettings,
) -> PriceAnchorComponentDialogValues {
    match anchor {
        PriceAnchorSettings::Fixed { rate } => PriceAnchorComponentDialogValues {
            anchor_type: "fixed",
            fixed_rate: rate.clone(),
            ..default_price_anchor_component_dialog_value()
        },
        PriceAnchorSettings::Oracle {
            chain_id,
            oracle_address,
            token_decimals,
            oracle_decimals,
            is_inversed,
        } => PriceAnchorComponentDialogValues {
            anchor_type: "oracle",
            fixed_rate: "1000000000000000000".to_string(),
            oracle_chain_id: *chain_id,
            oracle_address: oracle_address.clone(),
            oracle_token_decimals: token_decimals.to_string(),
            oracle_decimals: oracle_decimals.to_string(),
            oracle_is_inversed: *is_inversed,
            ..default_price_anchor_component_dialog_value()
        },
        PriceAnchorSettings::UniswapV3Twap {
            pool_address,
            base_token_address,
            quote_token_address,
            base_token_decimals,
            window_seconds,
        } => PriceAnchorComponentDialogValues {
            anchor_type: "uniswap-v3-twap",
            fixed_rate: "1000000000000000000".to_string(),
            oracle_chain_id: railgun_ui::DEFAULT_CHAINS[0],
            oracle_address: Address::ZERO.to_string(),
            oracle_token_decimals: "18".to_string(),
            oracle_decimals: "8".to_string(),
            oracle_is_inversed: false,
            twap_pool_address: pool_address.clone(),
            twap_base_token_address: base_token_address.clone(),
            twap_quote_token_address: quote_token_address.clone(),
            twap_base_token_decimals: base_token_decimals.to_string(),
            twap_window_seconds: window_seconds.to_string(),
        },
        PriceAnchorSettings::Product { .. } => default_price_anchor_component_dialog_value(),
    }
}

pub(in crate::root) fn default_price_anchor_component_dialog_value()
-> PriceAnchorComponentDialogValues {
    PriceAnchorComponentDialogValues {
        anchor_type: "oracle",
        fixed_rate: "1000000000000000000".to_string(),
        oracle_chain_id: railgun_ui::DEFAULT_CHAINS[0],
        oracle_address: Address::ZERO.to_string(),
        oracle_token_decimals: "18".to_string(),
        oracle_decimals: "8".to_string(),
        oracle_is_inversed: false,
        twap_pool_address: Address::ZERO.to_string(),
        twap_base_token_address: Address::ZERO.to_string(),
        twap_quote_token_address: Address::ZERO.to_string(),
        twap_base_token_decimals: "18".to_string(),
        twap_window_seconds: "1800".to_string(),
    }
}

pub(in crate::root) fn apply_price_anchor_dialog_values(
    settings: &mut WalletSettings,
    target: &PriceAnchorEditTarget,
    anchor: TokenPriceAnchorOverride,
) {
    match target {
        PriceAnchorEditTarget::Add => settings.tokens.price_anchors.push(anchor),
        PriceAnchorEditTarget::Edit(entry) => set_price_anchor_override(settings, entry, anchor),
    }
}

pub(in crate::root) fn set_price_anchor_override(
    settings: &mut WalletSettings,
    entry: &DisplayPriceAnchorEntry,
    anchor: TokenPriceAnchorOverride,
) {
    if let Some(index) = display_price_anchor_override_index(settings, entry) {
        settings.tokens.price_anchors[index] = anchor;
    } else {
        settings.tokens.price_anchors.push(anchor);
    }
}

pub(in crate::root) fn token_dialog_values(
    settings: &WalletSettings,
    target: &TokenEditTarget,
) -> TokenDialogValues {
    match target {
        TokenEditTarget::AddCustom => TokenDialogValues {
            chain_id: railgun_ui::DEFAULT_CHAINS[0],
            token_address: Address::ZERO.to_string(),
            symbol: String::new(),
            decimals: 18,
            icon_path: None,
        },
        TokenEditTarget::BuiltIn(key) => display_token_entries(settings)
            .into_iter()
            .find(|entry| token_key_matches_entry(key, entry))
            .map_or_else(
                || TokenDialogValues {
                    chain_id: key.chain_id,
                    token_address: key.token_address.clone(),
                    symbol: String::new(),
                    decimals: 18,
                    icon_path: None,
                },
                |entry| TokenDialogValues {
                    chain_id: entry.chain_id,
                    token_address: entry.token_address,
                    symbol: entry.symbol,
                    decimals: entry.decimals,
                    icon_path: entry.icon_path,
                },
            ),
        TokenEditTarget::Custom(index) => settings.tokens.custom_tokens.get(*index).map_or_else(
            || token_dialog_values(settings, &TokenEditTarget::AddCustom),
            |token| TokenDialogValues {
                chain_id: token.chain_id,
                token_address: token.token_address.clone(),
                symbol: token.symbol.clone(),
                decimals: token.decimals,
                icon_path: token.icon_path.clone(),
            },
        ),
    }
}

pub(in crate::root) fn token_dialog_values_from_inputs(
    inputs: &TokenDialogInputs,
    cx: &App,
) -> Result<TokenDialogValues, String> {
    let chain_id = inputs
        .chain_id
        .read(cx)
        .value()
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("Invalid token chain ID: {error}"))?;
    let decimals = inputs
        .decimals
        .read(cx)
        .value()
        .trim()
        .parse::<u8>()
        .map_err(|error| format!("Invalid token decimals: {error}"))?;
    Ok(TokenDialogValues {
        chain_id,
        token_address: inputs.token_address.read(cx).value().trim().to_string(),
        symbol: inputs.symbol.read(cx).value().trim().to_string(),
        decimals,
        icon_path: non_empty_setting(inputs.icon_path.read(cx).value().as_ref()),
    })
}

pub(in crate::root) fn waku_direct_peer_from_dialog_inputs(
    inputs: &WakuDirectPeerDialogInputs,
    cx: &App,
) -> WakuDirectPeerSetting {
    WakuDirectPeerSetting {
        peer_id: inputs.peer_id.read(cx).value().trim().to_string(),
        addr: inputs.addr.read(cx).value().trim().to_string(),
    }
}

pub(in crate::root) fn price_anchor_override_from_dialog_inputs(
    inputs: &PriceAnchorDialogInputs,
    cx: &App,
) -> Result<TokenPriceAnchorOverride, String> {
    let chain_id = inputs
        .chain_id
        .read(cx)
        .selected_value()
        .copied()
        .ok_or_else(|| "Select a token chain".to_string())?;
    let anchor_type = inputs
        .anchor_type
        .read(cx)
        .selected_value()
        .copied()
        .ok_or_else(|| "Select an anchor type".to_string())?;
    let oracle_chain_id = inputs
        .oracle_chain_id
        .read(cx)
        .selected_value()
        .copied()
        .ok_or_else(|| "Select an oracle chain".to_string())?;
    let oracle_is_inversed = inputs
        .oracle_is_inversed
        .read(cx)
        .selected_value()
        .copied()
        .ok_or_else(|| "Select whether the oracle is inverse".to_string())?;
    let product_components = inputs
        .product_components
        .iter()
        .map(|component| price_anchor_component_dialog_values(component, cx))
        .collect::<Result<Vec<_>, _>>()?;

    price_anchor_override_from_dialog_values(&PriceAnchorDialogValues {
        chain_id,
        token_address: inputs.token_address.read(cx).value().trim().to_string(),
        anchor_type,
        fixed_rate: inputs.fixed_rate.read(cx).value().trim().to_string(),
        oracle_chain_id,
        oracle_address: inputs.oracle_address.read(cx).value().trim().to_string(),
        oracle_token_decimals: inputs
            .oracle_token_decimals
            .read(cx)
            .value()
            .trim()
            .to_string(),
        oracle_decimals: inputs.oracle_decimals.read(cx).value().trim().to_string(),
        oracle_is_inversed,
        twap_pool_address: inputs.twap_pool_address.read(cx).value().trim().to_string(),
        twap_base_token_address: inputs
            .twap_base_token_address
            .read(cx)
            .value()
            .trim()
            .to_string(),
        twap_quote_token_address: inputs
            .twap_quote_token_address
            .read(cx)
            .value()
            .trim()
            .to_string(),
        twap_base_token_decimals: inputs
            .twap_base_token_decimals
            .read(cx)
            .value()
            .trim()
            .to_string(),
        twap_window_seconds: inputs
            .twap_window_seconds
            .read(cx)
            .value()
            .trim()
            .to_string(),
        product_scale_decimals: inputs
            .product_scale_decimals
            .read(cx)
            .value()
            .trim()
            .to_string(),
        product_components,
    })
}

pub(in crate::root) fn price_anchor_component_dialog_values(
    inputs: &ProductAnchorComponentDialogInputs,
    cx: &App,
) -> Result<PriceAnchorComponentDialogValues, String> {
    let anchor_type = inputs
        .anchor_type
        .read(cx)
        .selected_value()
        .copied()
        .ok_or_else(|| "Select a component type".to_string())?;
    let oracle_chain_id = inputs
        .oracle_chain_id
        .read(cx)
        .selected_value()
        .copied()
        .ok_or_else(|| "Select a component oracle chain".to_string())?;
    let oracle_is_inversed = inputs
        .oracle_is_inversed
        .read(cx)
        .selected_value()
        .copied()
        .ok_or_else(|| "Select whether the component oracle is inverse".to_string())?;

    Ok(PriceAnchorComponentDialogValues {
        anchor_type,
        fixed_rate: inputs.fixed_rate.read(cx).value().trim().to_string(),
        oracle_chain_id,
        oracle_address: inputs.oracle_address.read(cx).value().trim().to_string(),
        oracle_token_decimals: inputs
            .oracle_token_decimals
            .read(cx)
            .value()
            .trim()
            .to_string(),
        oracle_decimals: inputs.oracle_decimals.read(cx).value().trim().to_string(),
        oracle_is_inversed,
        twap_pool_address: inputs.twap_pool_address.read(cx).value().trim().to_string(),
        twap_base_token_address: inputs
            .twap_base_token_address
            .read(cx)
            .value()
            .trim()
            .to_string(),
        twap_quote_token_address: inputs
            .twap_quote_token_address
            .read(cx)
            .value()
            .trim()
            .to_string(),
        twap_base_token_decimals: inputs
            .twap_base_token_decimals
            .read(cx)
            .value()
            .trim()
            .to_string(),
        twap_window_seconds: inputs
            .twap_window_seconds
            .read(cx)
            .value()
            .trim()
            .to_string(),
    })
}

pub(in crate::root) fn price_anchor_override_from_dialog_values(
    values: &PriceAnchorDialogValues,
) -> Result<TokenPriceAnchorOverride, String> {
    let token_address = values.token_address.trim();
    if token_address.is_empty() {
        return Err("Token address must not be empty".to_string());
    }
    let anchor_type = parse_price_anchor_type(values.anchor_type)?;
    let price_anchor = match anchor_type {
        "oracle" => PriceAnchorSettings::Oracle {
            chain_id: values.oracle_chain_id,
            oracle_address: values.oracle_address.trim().to_string(),
            token_decimals: parse_price_anchor_u8(
                "Oracle token decimals",
                &values.oracle_token_decimals,
            )?,
            oracle_decimals: parse_price_anchor_u8("Oracle decimals", &values.oracle_decimals)?,
            is_inversed: values.oracle_is_inversed,
        },
        "uniswap-v3-twap" => PriceAnchorSettings::UniswapV3Twap {
            pool_address: values.twap_pool_address.trim().to_string(),
            base_token_address: values.twap_base_token_address.trim().to_string(),
            quote_token_address: values.twap_quote_token_address.trim().to_string(),
            base_token_decimals: parse_price_anchor_u8(
                "TWAP base-token decimals",
                &values.twap_base_token_decimals,
            )?,
            window_seconds: values
                .twap_window_seconds
                .trim()
                .parse::<u32>()
                .map_err(|error| format!("Invalid TWAP window seconds: {error}"))?,
        },
        "product" => PriceAnchorSettings::Product {
            components: product_components_from_dialog_values(&values.product_components)?,
            scale_decimals: parse_price_anchor_u8(
                "Product scale decimals",
                &values.product_scale_decimals,
            )?,
        },
        _ => PriceAnchorSettings::Fixed {
            rate: values.fixed_rate.trim().to_string(),
        },
    };

    Ok(TokenPriceAnchorOverride {
        key: TokenKey {
            chain_id: values.chain_id,
            token_address: token_address.to_string(),
        },
        price_anchor,
    })
}

pub(in crate::root) fn product_components_from_dialog_values(
    values: &[PriceAnchorComponentDialogValues],
) -> Result<Vec<PriceAnchorSettings>, String> {
    if values.is_empty() {
        return Err("Product anchor must include at least one component".to_string());
    }
    values
        .iter()
        .enumerate()
        .map(|(index, component)| price_anchor_component_from_dialog_values(index, component))
        .collect()
}

pub(in crate::root) fn price_anchor_component_from_dialog_values(
    index: usize,
    values: &PriceAnchorComponentDialogValues,
) -> Result<PriceAnchorSettings, String> {
    match parse_product_component_anchor_type(values.anchor_type)? {
        "oracle" => Ok(PriceAnchorSettings::Oracle {
            chain_id: values.oracle_chain_id,
            oracle_address: values.oracle_address.trim().to_string(),
            token_decimals: parse_price_anchor_u8(
                &format!("Component {} token decimals", index + 1),
                &values.oracle_token_decimals,
            )?,
            oracle_decimals: parse_price_anchor_u8(
                &format!("Component {} oracle decimals", index + 1),
                &values.oracle_decimals,
            )?,
            is_inversed: values.oracle_is_inversed,
        }),
        "uniswap-v3-twap" => Ok(PriceAnchorSettings::UniswapV3Twap {
            pool_address: values.twap_pool_address.trim().to_string(),
            base_token_address: values.twap_base_token_address.trim().to_string(),
            quote_token_address: values.twap_quote_token_address.trim().to_string(),
            base_token_decimals: parse_price_anchor_u8(
                "TWAP base-token decimals",
                &values.twap_base_token_decimals,
            )?,
            window_seconds: values
                .twap_window_seconds
                .trim()
                .parse::<u32>()
                .map_err(|error| format!("Invalid TWAP window seconds: {error}"))?,
        }),
        _ => Ok(PriceAnchorSettings::Fixed {
            rate: values.fixed_rate.trim().to_string(),
        }),
    }
}

pub(in crate::root) fn parse_price_anchor_u8(field: &str, value: &str) -> Result<u8, String> {
    value
        .trim()
        .parse::<u8>()
        .map_err(|error| format!("Invalid {field}: {error}"))
}

pub(in crate::root) fn apply_token_dialog_values(
    settings: &mut WalletSettings,
    target: &TokenEditTarget,
    values: TokenDialogValues,
) {
    match target {
        TokenEditTarget::AddCustom => settings.tokens.custom_tokens.push(CustomTokenSettings {
            chain_id: values.chain_id,
            token_address: values.token_address,
            symbol: values.symbol,
            decimals: values.decimals,
            icon_path: values.icon_path,
            price_anchor: None,
        }),
        TokenEditTarget::BuiltIn(key) => set_built_in_token_override(settings, key, values),
        TokenEditTarget::Custom(index) => {
            if let Some(token) = settings.tokens.custom_tokens.get_mut(*index) {
                token.chain_id = values.chain_id;
                token.token_address = values.token_address;
                token.symbol = values.symbol;
                token.decimals = values.decimals;
                token.icon_path = values.icon_path;
            }
        }
    }
}

pub(in crate::root) fn set_built_in_token_override(
    settings: &mut WalletSettings,
    key: &TokenKey,
    values: TokenDialogValues,
) {
    let default = key
        .token_address
        .parse::<Address>()
        .ok()
        .and_then(|address| railgun_ui::lookup_token(key.chain_id, &address));
    let position = settings
        .tokens
        .built_in_overrides
        .iter()
        .position(|override_settings| token_keys_match(&override_settings.key, key));
    let existing_anchor = position.and_then(|index| {
        settings.tokens.built_in_overrides[index]
            .price_anchor
            .clone()
    });
    let mut override_settings = BuiltInTokenOverride {
        key: key.clone(),
        price_anchor: existing_anchor,
        ..BuiltInTokenOverride::default()
    };
    override_settings.symbol = default.map_or_else(
        || non_empty_setting(&values.symbol),
        |token| (values.symbol != token.symbol).then_some(values.symbol.clone()),
    );
    override_settings.decimals = default.map_or(Some(values.decimals), |token| {
        (values.decimals != token.decimals).then_some(values.decimals)
    });
    override_settings.icon_path = values.icon_path;

    let is_empty = override_settings.symbol.is_none()
        && override_settings.decimals.is_none()
        && override_settings.icon_path.is_none()
        && override_settings.price_anchor.is_none();
    match (position, is_empty) {
        (Some(index), true) => {
            settings.tokens.built_in_overrides.remove(index);
        }
        (Some(index), false) => settings.tokens.built_in_overrides[index] = override_settings,
        (None, false) => settings.tokens.built_in_overrides.push(override_settings),
        (None, true) => {}
    }
}

pub(in crate::root) fn remove_custom_token(settings: &mut WalletSettings, index: usize) {
    if index < settings.tokens.custom_tokens.len() {
        settings.tokens.custom_tokens.remove(index);
    }
}

pub(in crate::root) fn token_key_matches_entry(key: &TokenKey, entry: &DisplayTokenEntry) -> bool {
    normalized_token_key(key.chain_id, &key.token_address)
        == normalized_token_key(entry.chain_id, &entry.token_address)
}

pub(in crate::root) fn token_keys_match(left: &TokenKey, right: &TokenKey) -> bool {
    token_key_tuple(left) == token_key_tuple(right)
}

pub(in crate::root) fn token_key_tuple(key: &TokenKey) -> (u64, String) {
    normalized_token_key(key.chain_id, &key.token_address)
}

pub(in crate::root) fn normalized_token_key(chain_id: u64, token_address: &str) -> (u64, String) {
    (chain_id, normalize_token_address(token_address))
}

pub(in crate::root) fn normalize_token_address(token_address: &str) -> String {
    token_address.parse::<Address>().map_or_else(
        |_| token_address.trim().to_ascii_lowercase(),
        |address| address.to_string().to_ascii_lowercase(),
    )
}
