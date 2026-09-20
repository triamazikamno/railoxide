//! Prospective chain edits. Persistence and active-operation admission belong to the desktop owner.

use super::{
    ChainSettingsOverride, CustomChainSettings, Deserialize, Serialize, WalletSettings,
    WalletSettingsError, WalletSettingsValidationError, encode_wallet_settings,
};
use alloy::primitives::{B256, keccak256};

pub type SettingsRevision = B256;

pub fn settings_revision(
    settings: &WalletSettings,
) -> Result<SettingsRevision, WalletSettingsError> {
    Ok(keccak256(encode_wallet_settings(settings)?))
}

pub const MAX_CUSTOM_CHAINS: usize = 64;
pub const MAX_CHAIN_ENDPOINTS: usize = 16;
pub const MAX_CHAIN_MUTATION_BYTES: usize = 64 * 1024;

// IDs are Rust u64 values here. Frontend adapters must transport them as strings.
#[derive(Clone, Serialize, Deserialize)]
pub enum ChainMutation {
    Add {
        chain_id: u64,
        definition: CustomChainSettings,
    },
    EditCustom {
        chain_id: u64,
        definition: CustomChainSettings,
    },
    EditBuiltIn {
        chain_id: u64,
        overrides: ChainSettingsOverride,
    },
    SetEnabled {
        chain_id: u64,
        enabled: bool,
    },
    Remove {
        chain_id: u64,
    },
    ResetBuiltIn {
        chain_id: u64,
    },
}

impl ChainMutation {
    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        match self {
            Self::Add { chain_id, .. }
            | Self::EditCustom { chain_id, .. }
            | Self::EditBuiltIn { chain_id, .. }
            | Self::SetEnabled { chain_id, .. }
            | Self::Remove { chain_id }
            | Self::ResetBuiltIn { chain_id } => *chain_id,
        }
    }

    pub fn prepare(&self, saved: &WalletSettings) -> Result<WalletSettings, WalletSettingsError> {
        let invalid = |message: &str| {
            WalletSettingsError::Validation(WalletSettingsValidationError::new(vec![
                message.to_owned(),
            ]))
        };
        if rmp_serde::to_vec_named(self)?.len() > MAX_CHAIN_MUTATION_BYTES {
            return Err(invalid("Chain edit is too large"));
        }
        let mut next = saved.clone();
        let id = self.chain_id();
        match self {
            Self::Add { definition, .. } => {
                if next.chains.contains(id) {
                    return Err(invalid("A chain with this ID is already configured"));
                }
                next.chains.custom.insert(id, definition.clone());
            }
            Self::EditCustom { definition, .. } => {
                let current = next
                    .chains
                    .custom
                    .get_mut(&id)
                    .ok_or_else(|| invalid("Custom chain is no longer configured"))?;
                *current = definition.clone();
            }
            Self::EditBuiltIn { overrides, .. } => {
                if !railgun_ui::DEFAULT_CHAINS.contains(&id) {
                    return Err(invalid("Chain has no built-in preset"));
                }
                next.chains.per_chain.insert(id, overrides.clone());
            }
            Self::SetEnabled { enabled, .. } => {
                if railgun_ui::DEFAULT_CHAINS.contains(&id) {
                    next.chains.per_chain.entry(id).or_default().enabled = *enabled;
                } else {
                    next.chains
                        .custom
                        .get_mut(&id)
                        .ok_or_else(|| invalid("Chain is no longer configured"))?
                        .enabled = *enabled;
                }
            }
            Self::Remove { .. } => {
                if next.chains.custom.remove(&id).is_none() {
                    return Err(invalid("Only a configured custom chain can be removed"));
                }
            }
            Self::ResetBuiltIn { .. } => {
                if !railgun_ui::DEFAULT_CHAINS.contains(&id) {
                    return Err(invalid("Chain has no built-in preset"));
                }
                next.chains.per_chain.remove(&id);
            }
        }
        next.validate()?;
        Ok(next)
    }
}
