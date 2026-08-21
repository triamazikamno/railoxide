use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

use crate::hardware::HardwareTypedDataSigningMode;
use crate::vault::{PublicAccountSource, WalletConnectApprovedNamespace};

use super::eip155::{
    WALLETCONNECT_EIP155_NAMESPACE, WalletConnectSupportedEvent, WalletConnectSupportedMethod,
};
use super::{Result, WalletConnectError};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WalletConnectNamespaceProposal {
    pub chains: Vec<String>,
    pub methods: Vec<String>,
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnectUnsupportedNamespaceItem {
    pub namespace: String,
    pub item: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalletConnectNamespaceAccountSupport {
    pub account_source: PublicAccountSource,
    pub hardware_typed_data_signing_mode: HardwareTypedDataSigningMode,
    pub hardware_typed_data_capability_known: bool,
}

impl WalletConnectNamespaceAccountSupport {
    #[must_use]
    pub const fn for_account_source(account_source: PublicAccountSource) -> Self {
        Self {
            account_source,
            hardware_typed_data_signing_mode: HardwareTypedDataSigningMode::Unsupported,
            hardware_typed_data_capability_known: true,
        }
    }

    #[must_use]
    pub const fn hardware(hardware_typed_data_signing_mode: HardwareTypedDataSigningMode) -> Self {
        Self {
            account_source: PublicAccountSource::HardwareDerived,
            hardware_typed_data_signing_mode,
            hardware_typed_data_capability_known: true,
        }
    }

    #[must_use]
    pub const fn hardware_typed_data_capability_unknown() -> Self {
        Self {
            account_source: PublicAccountSource::HardwareDerived,
            hardware_typed_data_signing_mode: HardwareTypedDataSigningMode::Unsupported,
            hardware_typed_data_capability_known: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConnectNamespaceNegotiation {
    pub approved_namespaces: BTreeMap<String, WalletConnectApprovedNamespace>,
    pub excluded_optional: Vec<WalletConnectUnsupportedNamespaceItem>,
}

pub fn negotiate_walletconnect_namespaces(
    required: &BTreeMap<String, WalletConnectNamespaceProposal>,
    optional: &BTreeMap<String, WalletConnectNamespaceProposal>,
    supported_chain_ids: &BTreeSet<u64>,
    selected_account: Address,
    selected_account_source: PublicAccountSource,
) -> Result<WalletConnectNamespaceNegotiation> {
    negotiate_walletconnect_namespaces_with_account_support(
        required,
        optional,
        supported_chain_ids,
        selected_account,
        WalletConnectNamespaceAccountSupport::for_account_source(selected_account_source),
    )
}

pub fn negotiate_walletconnect_namespaces_with_account_support(
    required: &BTreeMap<String, WalletConnectNamespaceProposal>,
    optional: &BTreeMap<String, WalletConnectNamespaceProposal>,
    supported_chain_ids: &BTreeSet<u64>,
    selected_account: Address,
    selected_account_support: WalletConnectNamespaceAccountSupport,
) -> Result<WalletConnectNamespaceNegotiation> {
    let mut unsupported_required = Vec::new();
    let mut approved_namespaces = BTreeMap::new();

    if required.is_empty() && optional.is_empty() {
        let approved = default_eip155_namespace(supported_chain_ids, selected_account)?;
        approved_namespaces.insert(WALLETCONNECT_EIP155_NAMESPACE.to_owned(), approved);
        return Ok(WalletConnectNamespaceNegotiation {
            approved_namespaces,
            excluded_optional: Vec::new(),
        });
    }

    for (key, namespace) in required {
        match negotiate_namespace(
            key,
            namespace,
            supported_chain_ids,
            selected_account,
            selected_account_support,
            true,
        ) {
            NamespaceNegotiation::Approved { approved, .. } => {
                merge_approved_namespace(&mut approved_namespaces, key, approved);
            }
            NamespaceNegotiation::Unsupported(items) => unsupported_required.extend(items),
            NamespaceNegotiation::Excluded => {
                unsupported_required.push(WalletConnectUnsupportedNamespaceItem {
                    namespace: key.clone(),
                    item: key.clone(),
                    reason: "required namespace has no supported chains, methods, or events"
                        .to_owned(),
                });
            }
        }
    }

    if !unsupported_required.is_empty() {
        let message = unsupported_required
            .iter()
            .map(|item| format!("{}:{} ({})", item.namespace, item.item, item.reason))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(WalletConnectError::UnsatisfiedNamespaces(message));
    }

    let mut excluded_optional = Vec::new();
    for (key, namespace) in optional {
        match negotiate_namespace(
            key,
            namespace,
            supported_chain_ids,
            selected_account,
            selected_account_support,
            false,
        ) {
            NamespaceNegotiation::Approved {
                approved,
                unsupported,
            } => {
                merge_optional_approved_namespace(&mut approved_namespaces, key, approved);
                excluded_optional.extend(unsupported);
            }
            NamespaceNegotiation::Unsupported(items) => excluded_optional.extend(items),
            NamespaceNegotiation::Excluded => {}
        }
    }

    Ok(WalletConnectNamespaceNegotiation {
        approved_namespaces,
        excluded_optional,
    })
}

enum NamespaceNegotiation {
    Approved {
        approved: WalletConnectApprovedNamespace,
        unsupported: Vec<WalletConnectUnsupportedNamespaceItem>,
    },
    Unsupported(Vec<WalletConnectUnsupportedNamespaceItem>),
    Excluded,
}

fn negotiate_namespace(
    key: &str,
    namespace: &WalletConnectNamespaceProposal,
    supported_chain_ids: &BTreeSet<u64>,
    selected_account: Address,
    selected_account_support: WalletConnectNamespaceAccountSupport,
    required: bool,
) -> NamespaceNegotiation {
    let Some(requested_chains) = requested_eip155_chains(key, &namespace.chains) else {
        return NamespaceNegotiation::Unsupported(vec![WalletConnectUnsupportedNamespaceItem {
            namespace: key.to_owned(),
            item: key.to_owned(),
            reason: "only EIP-155 namespaces are supported".to_owned(),
        }]);
    };

    let mut unsupported = Vec::new();
    let mut approved_chains = Vec::new();
    for chain in requested_chains {
        match parse_eip155_chain_id(&chain) {
            Some(chain_id) if supported_chain_ids.contains(&chain_id) => {
                approved_chains.push((chain, chain_id));
            }
            _ => unsupported.push(WalletConnectUnsupportedNamespaceItem {
                namespace: key.to_owned(),
                item: chain,
                reason: "unsupported chain".to_owned(),
            }),
        }
    }

    let mut approved_methods = Vec::new();
    for method in &namespace.methods {
        match WalletConnectSupportedMethod::from_str(method) {
            Ok(method_kind)
                if !walletconnect_method_supported_for_account_support(
                    method_kind,
                    selected_account_support,
                ) =>
            {
                unsupported.push(WalletConnectUnsupportedNamespaceItem {
                    namespace: key.to_owned(),
                    item: method.clone(),
                    reason: unsupported_method_reason(method_kind, selected_account_support),
                });
            }
            Ok(_) => approved_methods.push(method.clone()),
            Err(_) => unsupported.push(WalletConnectUnsupportedNamespaceItem {
                namespace: key.to_owned(),
                item: method.clone(),
                reason: "unsupported method".to_owned(),
            }),
        }
    }

    let mut approved_events = Vec::new();
    for event in &namespace.events {
        if WalletConnectSupportedEvent::from_str(event).is_ok() {
            approved_events.push(event.clone());
        } else {
            unsupported.push(WalletConnectUnsupportedNamespaceItem {
                namespace: key.to_owned(),
                item: event.clone(),
                reason: "unsupported event".to_owned(),
            });
        }
    }

    if required && !unsupported.is_empty() {
        return NamespaceNegotiation::Unsupported(unsupported);
    }
    let requested_no_capabilities = namespace.methods.is_empty() && namespace.events.is_empty();
    if approved_chains.is_empty()
        || (!requested_no_capabilities && approved_methods.is_empty() && approved_events.is_empty())
    {
        return if required {
            NamespaceNegotiation::Unsupported(if unsupported.is_empty() {
                vec![WalletConnectUnsupportedNamespaceItem {
                    namespace: key.to_owned(),
                    item: key.to_owned(),
                    reason: "required namespace has no supported chain, method, or event"
                        .to_owned(),
                }]
            } else {
                unsupported
            })
        } else if unsupported.is_empty() {
            NamespaceNegotiation::Excluded
        } else {
            NamespaceNegotiation::Unsupported(unsupported)
        };
    }

    let address = selected_account.to_string();
    NamespaceNegotiation::Approved {
        approved: WalletConnectApprovedNamespace {
            chains: approved_chains
                .iter()
                .map(|(chain, _)| chain.clone())
                .collect(),
            accounts: approved_chains
                .iter()
                .map(|(_, chain_id)| format!("eip155:{chain_id}:{address}"))
                .collect(),
            methods: approved_methods,
            events: approved_events,
        },
        unsupported,
    }
}

fn default_eip155_namespace(
    supported_chain_ids: &BTreeSet<u64>,
    selected_account: Address,
) -> Result<WalletConnectApprovedNamespace> {
    if supported_chain_ids.is_empty() {
        return Err(WalletConnectError::UnsatisfiedNamespaces(
            "no enabled EIP-155 chains".to_owned(),
        ));
    }

    let address = selected_account.to_string();
    let chains = supported_chain_ids
        .iter()
        .map(|chain_id| format!("eip155:{chain_id}"))
        .collect::<Vec<_>>();
    let accounts = supported_chain_ids
        .iter()
        .map(|chain_id| format!("eip155:{chain_id}:{address}"))
        .collect::<Vec<_>>();
    Ok(WalletConnectApprovedNamespace {
        chains,
        accounts,
        methods: Vec::new(),
        events: Vec::new(),
    })
}

pub(crate) const fn walletconnect_method_supported_for_account_support(
    method: WalletConnectSupportedMethod,
    selected_account_support: WalletConnectNamespaceAccountSupport,
) -> bool {
    let selected_account_source = selected_account_support.account_source;
    match selected_account_source {
        PublicAccountSource::HardwareDerived => {
            walletconnect_method_supported_for_hardware_account(
                method,
                selected_account_support.hardware_typed_data_signing_mode,
            )
        }
        PublicAccountSource::Derived | PublicAccountSource::Imported => true,
    }
}

const fn walletconnect_method_supported_for_hardware_account(
    method: WalletConnectSupportedMethod,
    typed_data_signing_mode: HardwareTypedDataSigningMode,
) -> bool {
    match method {
        WalletConnectSupportedMethod::EthSignTypedData
        | WalletConnectSupportedMethod::EthSignTypedDataV4 => {
            typed_data_signing_mode.is_supported()
        }
        #[cfg(not(feature = "hardware"))]
        WalletConnectSupportedMethod::PersonalSign
        | WalletConnectSupportedMethod::EthSendTransaction => false,
        WalletConnectSupportedMethod::EthAccounts
        | WalletConnectSupportedMethod::EthRequestAccounts
        | WalletConnectSupportedMethod::WalletSwitchEthereumChain => true,
        #[cfg(feature = "hardware")]
        WalletConnectSupportedMethod::PersonalSign
        | WalletConnectSupportedMethod::EthSendTransaction => true,
    }
}

fn unsupported_method_reason(
    method: WalletConnectSupportedMethod,
    selected_account_support: WalletConnectNamespaceAccountSupport,
) -> String {
    if selected_account_support.account_source == PublicAccountSource::HardwareDerived
        && matches!(
            method,
            WalletConnectSupportedMethod::EthSignTypedData
                | WalletConnectSupportedMethod::EthSignTypedDataV4
        )
        && !selected_account_support
            .hardware_typed_data_signing_mode
            .is_supported()
    {
        return "hardware Public account session does not support typed-data signing".to_owned();
    }
    "unsupported method for hardware Public account".to_owned()
}

fn merge_optional_approved_namespace(
    approved_namespaces: &mut BTreeMap<String, WalletConnectApprovedNamespace>,
    key: &str,
    approved: WalletConnectApprovedNamespace,
) {
    if approved_namespaces.contains_key(key) {
        merge_approved_namespace_per_chain(approved_namespaces, &approved);
    } else {
        merge_approved_namespace(approved_namespaces, key, approved);
    }
}

fn merge_approved_namespace_per_chain(
    approved_namespaces: &mut BTreeMap<String, WalletConnectApprovedNamespace>,
    approved: &WalletConnectApprovedNamespace,
) {
    for chain in &approved.chains {
        let account_prefix = format!("{chain}:");
        let accounts = approved
            .accounts
            .iter()
            .filter(|account| account.starts_with(&account_prefix))
            .cloned()
            .collect();
        merge_approved_namespace(
            approved_namespaces,
            chain,
            WalletConnectApprovedNamespace {
                chains: vec![chain.clone()],
                accounts,
                methods: approved.methods.clone(),
                events: approved.events.clone(),
            },
        );
    }
}

fn merge_approved_namespace(
    approved_namespaces: &mut BTreeMap<String, WalletConnectApprovedNamespace>,
    key: &str,
    approved: WalletConnectApprovedNamespace,
) {
    let entry = approved_namespaces.entry(key.to_owned()).or_default();
    extend_unique(&mut entry.chains, approved.chains);
    extend_unique(&mut entry.accounts, approved.accounts);
    extend_unique(&mut entry.methods, approved.methods);
    extend_unique(&mut entry.events, approved.events);
}

fn extend_unique(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

fn requested_eip155_chains(namespace_key: &str, chains: &[String]) -> Option<Vec<String>> {
    if namespace_key == WALLETCONNECT_EIP155_NAMESPACE {
        return Some(chains.to_vec());
    }
    parse_eip155_chain_id(namespace_key).map(|_| {
        if chains.is_empty() {
            vec![namespace_key.to_owned()]
        } else {
            chains.to_vec()
        }
    })
}

fn parse_eip155_chain_id(value: &str) -> Option<u64> {
    value.strip_prefix("eip155:")?.parse::<u64>().ok()
}
