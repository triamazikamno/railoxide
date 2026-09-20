//! ENS resolution uses Alloy's contracts through the wallet's existing broker route.
use alloy::{
    ens::{EnsResolver, UNIVERSAL_RESOLVER_ADDRESS, UniversalResolver, dns_encode, namehash},
    primitives::{Address, Bytes},
    sol_types::SolCall,
};
use eyre::{Result, eyre};

use crate::rpc_broker::{RpcRead, RpcRoute, RpcSubmission, WalletRpcOrigin};
use crate::{
    HttpContext,
    settings::{EffectiveChainConfig, resolve_effective_chain_rpc_route},
};

/// ENS records are resolved on Ethereum using the desktop's configured Ethereum route.
/// Alloy's provider helper cannot use our broker directly. Reuse its ABI and encoding,
/// with ENSIP-15 normalization, while submitting the single read with a wallet origin.
pub async fn resolve_public_ens_recipient(
    name: &str,
    ethereum: &EffectiveChainConfig,
    http: &HttpContext,
) -> Result<Address> {
    let normalized = ens_normalize_rs::normalize(name).map_err(|_| eyre!("Invalid ENS name"))?;
    // Alloy 2.3's DNS encoder truncates a label length to u8. Enforce that API's
    // representability precondition before encoding, without truncating the name.
    if normalized.is_empty()
        || normalized
            .split('.')
            .any(|label| label.len() > usize::from(u8::MAX))
    {
        return Err(eyre!("ENS name cannot be encoded for the resolver"));
    }
    let call = UniversalResolver::resolveCall {
        name: dns_encode(&normalized).into(),
        data: EnsResolver::addrCall {
            node: namehash(&normalized),
        }
        .abi_encode()
        .into(),
    };
    let route = resolve_effective_chain_rpc_route(1, ethereum)?;
    let mut results = http
        .rpc_broker()
        .submit(RpcSubmission::new(
            RpcRoute::from(route),
            vec![RpcRead::eth_call(
                UNIVERSAL_RESOLVER_ADDRESS,
                call.abi_encode().into(),
            )],
            WalletRpcOrigin::PublicWallet.into(),
        ))
        .await
        .map_err(|_| eyre!("ENS lookup unavailable. Use a public address or try again."))?;
    let result = results
        .pop()
        .ok_or_else(|| eyre!("ENS lookup returned no result"))?
        .map_err(|_| {
            eyre!("ENS lookup failed. This name may require an unsupported offchain lookup.")
        })?;
    let bytes: Bytes = serde_json::from_value(result.into_value())?;
    let resolved = UniversalResolver::resolveCall::abi_decode_returns(&bytes)?;
    let address = EnsResolver::addrCall::abi_decode_returns(&resolved._0)?;
    if address.is_zero() {
        return Err(eyre!("ENS name has no public address record"));
    }
    Ok(address)
}
