use super::*;
use alloy::dyn_abi::TypedData;
use alloy::primitives::Signature;
use alloy_signer::SignerSync;

pub(crate) trait EvmTransactionSigner {
    fn address(&self) -> Address;

    fn ethereum_wallet(&self) -> EthereumWallet;
}

pub(crate) trait EvmMessageSigner {
    fn derive_shield_private_key(&self) -> Result<[u8; 32]>;
}

pub(crate) struct SoftwareEvmSigner {
    private_key: [u8; 32],
    signer: PrivateKeySigner,
}

impl SoftwareEvmSigner {
    pub(crate) fn from_private_key(private_key: [u8; 32]) -> Result<Self> {
        let signer = PrivateKeySigner::from(
            SigningKey::from_bytes((&private_key).into()).wrap_err("invalid signing key")?,
        );
        Ok(Self {
            private_key,
            signer,
        })
    }

    pub(crate) fn sign_personal_message(&self, message: &[u8]) -> Result<Signature> {
        self.signer
            .sign_message_sync(message)
            .wrap_err("software personal_sign")
    }

    pub(crate) fn sign_typed_data_v4(&self, typed_data: &TypedData) -> Result<Signature> {
        self.signer
            .sign_dynamic_typed_data_sync(typed_data)
            .wrap_err("software EIP-712 typed-data signing")
    }
}

impl EvmTransactionSigner for SoftwareEvmSigner {
    fn address(&self) -> Address {
        self.signer.address()
    }

    fn ethereum_wallet(&self) -> EthereumWallet {
        EthereumWallet::from(self.signer.clone())
    }
}

impl EvmMessageSigner for SoftwareEvmSigner {
    fn derive_shield_private_key(&self) -> Result<[u8; 32]> {
        derive_shield_private_key(&self.private_key).wrap_err("derive shield private key")
    }
}

impl Drop for SoftwareEvmSigner {
    fn drop(&mut self) {
        self.private_key.zeroize();
    }
}
