use std::str::FromStr;

use alloy::primitives::{Address, U64, U256};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Result, WalletConnectError};

pub const WALLETCONNECT_EIP155_NAMESPACE: &str = "eip155";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WalletConnectSupportedMethod {
    EthAccounts,
    EthRequestAccounts,
    PersonalSign,
    EthSendTransaction,
    EthSignTypedData,
    EthSignTypedDataV4,
    WalletSwitchEthereumChain,
}

impl WalletConnectSupportedMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EthAccounts => "eth_accounts",
            Self::EthRequestAccounts => "eth_requestAccounts",
            Self::PersonalSign => "personal_sign",
            Self::EthSendTransaction => "eth_sendTransaction",
            Self::EthSignTypedData => "eth_signTypedData",
            Self::EthSignTypedDataV4 => "eth_signTypedData_v4",
            Self::WalletSwitchEthereumChain => "wallet_switchEthereumChain",
        }
    }
}

impl FromStr for WalletConnectSupportedMethod {
    type Err = WalletConnectError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "eth_accounts" => Ok(Self::EthAccounts),
            "eth_requestAccounts" => Ok(Self::EthRequestAccounts),
            "personal_sign" => Ok(Self::PersonalSign),
            "eth_sendTransaction" => Ok(Self::EthSendTransaction),
            "eth_signTypedData" => Ok(Self::EthSignTypedData),
            "eth_signTypedData_v4" => Ok(Self::EthSignTypedDataV4),
            "wallet_switchEthereumChain" => Ok(Self::WalletSwitchEthereumChain),
            method => Err(WalletConnectError::UnsupportedMethod(method.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WalletConnectSupportedEvent {
    AccountsChanged,
    ChainChanged,
}

impl WalletConnectSupportedEvent {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccountsChanged => "accountsChanged",
            Self::ChainChanged => "chainChanged",
        }
    }
}

impl FromStr for WalletConnectSupportedEvent {
    type Err = WalletConnectError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "accountsChanged" => Ok(Self::AccountsChanged),
            "chainChanged" => Ok(Self::ChainChanged),
            event => Err(WalletConnectError::UnsupportedEvent(event.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletConnectSessionRequest {
    pub id: u64,
    pub topic: String,
    pub chain_id: String,
    pub method: WalletConnectSupportedMethod,
    pub params: Value,
    pub expiry_timestamp: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletConnectTransactionRequest {
    pub from: Address,
    pub to: Option<Address>,
    pub value: Option<U256>,
    pub data: Option<String>,
    pub gas: Option<U256>,
    #[serde(rename = "gasPrice")]
    pub gas_price: Option<U256>,
    #[serde(rename = "maxFeePerGas")]
    pub max_fee_per_gas: Option<U256>,
    #[serde(rename = "maxPriorityFeePerGas")]
    pub max_priority_fee_per_gas: Option<U256>,
    #[serde(rename = "chainId")]
    pub chain_id: Option<U64>,
    pub nonce: Option<U256>,
}
