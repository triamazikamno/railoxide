#[allow(clippy::wildcard_imports)]
use super::*;

pub(crate) async fn query_erc20_allowance(
    chain_route: &crate::ClassRoute,
    http: &HttpContext,
    asset: PublicAssetId,
    owner: Address,
    spender: Address,
) -> Result<U256> {
    let PublicAssetId::Erc20(token) = asset else {
        return Err(eyre!("native shield has no ERC-20 allowance"));
    };
    let results = http
        .rpc_broker()
        .submit_eth_calls(
            RpcRoute::from(chain_route.clone()),
            vec![(
                token,
                PublicErc20::allowanceCall { owner, spender }
                    .abi_encode()
                    .into(),
            )],
            WalletRpcOrigin::PublicWallet.into(),
        )
        .await
        .wrap_err("query public shield ERC-20 allowance")?;
    let output = results
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("missing public shield ERC-20 allowance result"))?
        .wrap_err("query public shield ERC-20 allowance")?;
    PublicErc20::allowanceCall::abi_decode_returns_validate(&output)
        .wrap_err("decode public shield ERC-20 allowance")
}

/// `owner`'s balance of the ERC-20 `token`, read as [`query_erc20_allowance`] reads an
/// allowance.
pub(crate) async fn query_erc20_balance(
    chain_route: &crate::ClassRoute,
    http: &HttpContext,
    token: Address,
    owner: Address,
) -> Result<U256> {
    let results = http
        .rpc_broker()
        .submit_eth_calls(
            RpcRoute::from(chain_route.clone()),
            vec![(
                token,
                PublicErc20::balanceOfCall { account: owner }
                    .abi_encode()
                    .into(),
            )],
            WalletRpcOrigin::PublicWallet.into(),
        )
        .await
        .wrap_err("query ERC-20 balance")?;
    let output = results
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("missing ERC-20 balance result"))?
        .wrap_err("query ERC-20 balance")?;
    PublicErc20::balanceOfCall::abi_decode_returns_validate(&output)
        .wrap_err("decode ERC-20 balance")
}
