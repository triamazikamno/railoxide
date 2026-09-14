use super::{GatewayConfig, GatewayError};
use local_db::DbStore;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const KEY: &str = "gateway-state";
pub(super) const MAX_PEERS: usize = 64;
const MAX_STORAGE_BYTES: usize = 64 * 1024;

// Only transport credentials live here. This record is intentionally outside the vault.
#[derive(Clone, Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
pub(super) struct StoredSecret(pub [u8; 32]);

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Peer {
    pub id: [u8; 16],
    pub secret: StoredSecret,
    pub label: Option<String>,
    #[serde(default)]
    pub allow_unlock: bool,
    pub paired_at: u64,
    pub last_active_at: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Registry {
    pub version: u16,
    pub config: GatewayConfig,
    pub peers: Vec<Peer>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: 2,
            config: GatewayConfig::default(),
            peers: Vec::new(),
        }
    }
}

impl Registry {
    pub(super) fn load(db: &DbStore) -> Result<Self, GatewayError> {
        let Some(bytes) = db
            .get_app_settings_record(KEY)
            .map_err(|_| GatewayError::Storage)?
        else {
            return Ok(Self::default());
        };
        Self::decode(&Zeroizing::new(bytes))
    }

    fn decode(bytes: &[u8]) -> Result<Self, GatewayError> {
        if bytes.len() > MAX_STORAGE_BYTES {
            return Err(GatewayError::InvalidStorage);
        }
        let registry: Self =
            serde_json::from_slice(bytes).map_err(|_| GatewayError::InvalidStorage)?;
        registry.validate()?;
        Ok(registry)
    }

    fn validate(&self) -> Result<(), GatewayError> {
        if self.version != 2 || self.peers.len() > MAX_PEERS {
            return Err(GatewayError::InvalidStorage);
        }
        let mut ids = std::collections::HashSet::new();
        for peer in &self.peers {
            if !ids.insert(peer.id) || peer.label.as_ref().is_some_and(|label| label.len() > 128) {
                return Err(GatewayError::InvalidStorage);
            }
        }
        Ok(())
    }

    pub(super) fn save(&self, db: &DbStore) -> Result<(), GatewayError> {
        self.validate()?;
        let bytes = Zeroizing::new(serde_json::to_vec(self).map_err(|_| GatewayError::Storage)?);
        if bytes.len() > MAX_STORAGE_BYTES {
            return Err(GatewayError::InvalidStorage);
        }
        db.put_app_settings_record(KEY, &bytes)
            .map_err(|_| GatewayError::Storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_registry_never_becomes_an_empty_registry() {
        assert!(Registry::decode(b"garbage").is_err());
        let mut registry = Registry::default();
        registry.peers.push(Peer {
            id: [1; 16],
            secret: StoredSecret([2; 32]),
            label: None,
            allow_unlock: false,
            paired_at: 1,
            last_active_at: 1,
        });
        let bytes = serde_json::to_vec(&registry).unwrap();
        assert_eq!(Registry::decode(&bytes).unwrap().peers.len(), 1);
        registry.version = 3;
        assert!(Registry::decode(&serde_json::to_vec(&registry).unwrap()).is_err());
        registry.version = 2;
        registry.peers.push(registry.peers[0].clone());
        assert!(Registry::decode(&serde_json::to_vec(&registry).unwrap()).is_err());
    }
    #[test]
    fn released_pairings_default_unlock_off_and_preserve_explicit_permission() {
        let released = serde_json::json!({
            "version": 2,
            "config": GatewayConfig::default(),
            "peers": [{"id": vec![1; 16], "secret": vec![2; 32], "label": null, "paired_at": 1, "last_active_at": 1}]
        });
        let mut registry = Registry::decode(&serde_json::to_vec(&released).unwrap()).unwrap();
        assert!(!registry.peers[0].allow_unlock);
        registry.peers[0].allow_unlock = true;
        let restored = Registry::decode(&serde_json::to_vec(&registry).unwrap()).unwrap();
        assert!(restored.peers[0].allow_unlock);
        assert_eq!(restored.peers[0].id, registry.peers[0].id);
        assert_eq!(restored.peers[0].secret.0, registry.peers[0].secret.0);
    }
}
