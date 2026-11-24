//! ENS resolution uses Alloy's contracts through the wallet's existing broker route.
use std::collections::HashSet;
use std::time::Duration;

use alloy::{
    ens::{EnsResolver, UNIVERSAL_RESOLVER_ADDRESS, UniversalResolver, dns_encode, namehash},
    primitives::{Address, Bytes},
    sol_types::SolCall,
};
use eyre::{Result, eyre};
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use poi::SensitiveUrl;
use tokio::time::Instant;

use crate::rpc_broker::{
    ClassRoute, RpcBrokerError, RpcRead, RpcRoute, RpcSubmission, RpcTrafficClass, WalletRpcOrigin,
};
use crate::{
    HttpContext, OperationHttpClient,
    settings::{EffectiveChainConfig, resolve_effective_chain_rpc_route},
};

/// The limit for the whole lookup, over every endpoint it asks.
const ENS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);

/// ENS records are resolved on Ethereum using the desktop's configured Ethereum route, isolated
/// over the operation's `client`.
/// Alloy's provider helper cannot use our broker directly. Reuse its ABI and encoding,
/// with ENSIP-15 normalization, while submitting the reads with a wallet origin.
/// One lying endpoint could redirect a payment, so when two or more distinct Sensitive endpoints
/// are configured, two of them must return the same address. Endpoints are told apart by
/// configured URL, so URLs that reach one provider through an alias, a redirect or a shared
/// backend count separately. Which providers to list, including a single one, is the user's
/// choice. A lookup that cannot collect two answers fails, but endpoints it already asked have
/// seen the name, and each of them is allowed Sensitive reads.
pub async fn resolve_public_ens_recipient(
    name: &str,
    ethereum: &EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
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
    let calldata: Bytes = call.abi_encode().into();
    let deadline = Instant::now() + ENS_LOOKUP_TIMEOUT;
    let sensitive =
        resolve_effective_chain_rpc_route(1, ethereum)?.for_class(RpcTrafficClass::Sensitive)?;
    // Counted from settings alone, so no endpoint's reply can lower it.
    let required = sensitive
        .endpoints()
        .iter()
        .map(SensitiveUrl::expose_url)
        .collect::<HashSet<_>>()
        .len()
        .min(2);
    let mut attempts = attempt_routes(http, &sensitive, required, client, deadline)
        .await?
        .into_iter();
    let mut in_flight = FuturesUnordered::new();
    let mut answers = Vec::with_capacity(required);
    // A further endpoint is asked only after one gives no answer, so a successful lookup reaches
    // only `required` endpoints.
    while answers.len() < required {
        while in_flight.len() + answers.len() < required
            && let Some(attempt) = attempts.next()
        {
            in_flight.push(resolver_answer(http, attempt, calldata.clone(), deadline));
        }
        let Some(answer) = in_flight.next().await else {
            break;
        };
        if let Ok(answer) = answer {
            answers.push(answer);
        }
    }
    agreed_address(&answers, required)
}

fn unavailable() -> eyre::Report {
    eyre!("ENS lookup unavailable. Use a public address or try again.")
}

/// `route` itself when one answer is `required`, otherwise its eligible endpoints, each URL once.
/// Each gets its own isolation over `client`, so no attempt is answered from another's cached
/// result.
async fn attempt_routes(
    http: &HttpContext,
    route: &ClassRoute,
    required: usize,
    client: &OperationHttpClient,
    deadline: Instant,
) -> Result<Vec<ClassRoute>> {
    // The full route keeps the all-withdrawn fallback that candidates lose during warmup recovery.
    if required == 1 {
        return Ok(vec![route.isolated(client.clone())]);
    }
    let mut seen = HashSet::new();
    http.rpc_broker()
        .endpoint_candidates(route.route(), Some(deadline))
        .await
        .map_err(|_| unavailable())?
        .into_iter()
        .filter(|endpoint| seen.insert(endpoint.expose_url().clone()))
        .map(|endpoint| -> Result<ClassRoute> {
            Ok(route
                .route()
                .for_endpoint(endpoint)
                .for_class(route.class())?
                .isolated(client.clone()))
        })
        .collect()
}

/// One endpoint's answer: the address it resolved, or `None` when the resolver reverted. An error
/// means the endpoint gave no answer.
async fn resolver_answer(
    http: &HttpContext,
    route: ClassRoute,
    calldata: Bytes,
    deadline: Instant,
) -> Result<Option<Address>> {
    let route = RpcRoute::from(route)
        .with_request_timeout(deadline.saturating_duration_since(Instant::now()));
    let result = http
        .rpc_broker()
        .submit(RpcSubmission::new(
            route,
            vec![RpcRead::eth_call(UNIVERSAL_RESOLVER_ADDRESS, calldata)],
            WalletRpcOrigin::PublicWallet.into(),
        ))
        .await?
        .pop()
        .ok_or_else(|| eyre!("ENS lookup returned no result"))?;
    let value = match result {
        Ok(value) => value,
        Err(RpcBrokerError::InnerRevert(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let bytes: Bytes = serde_json::from_value(value.into_value())?;
    let resolved = UniversalResolver::resolveCall::abi_decode_returns(&bytes)?;
    Ok(Some(EnsResolver::addrCall::abi_decode_returns(
        &resolved._0,
    )?))
}

/// The nonzero address that `required` answers all agree on.
fn agreed_address(answers: &[Option<Address>], required: usize) -> Result<Address> {
    let first = match answers {
        [first, ..] if answers.len() == required => *first,
        _ => return Err(unavailable()),
    };
    if answers.iter().any(|answer| *answer != first) {
        return Err(eyre!(
            "ENS endpoints returned different addresses. Enter the recipient's public address."
        ));
    }
    let address = first.ok_or_else(|| {
        eyre!("ENS lookup failed. This name may require an unsupported offchain lookup.")
    })?;
    if address.is_zero() {
        return Err(eyre!("ENS name has no public address record"));
    }
    Ok(address)
}
