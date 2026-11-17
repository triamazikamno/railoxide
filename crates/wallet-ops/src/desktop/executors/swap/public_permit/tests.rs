use alloy::primitives::{address, keccak256, uint};
use alloy::sol_types::SolValue as _;
use railgun_wallet::tx::GasEstimateMode;

use super::super::order::SwapPrice;
use super::super::public_order::{price_public_order, public_order_hook_gas};
use super::super::public_transactions::public_swap_gas_plan;
use super::*;
use crate::cow::{
    CowQuote, GAS_SHARE_BALANCED_BPS, PUBLIC_PERMIT_HOOK_GAS, public_deposit_hook_gas,
};
use crate::{PairAnchorRate, RpcRevert};

const CHAIN: u64 = 8453;
const TOKEN: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const NONCE: U256 = uint!(7_U256);

fn reverted() -> Read {
    Err(RpcBrokerError::InnerRevert(RpcRevert::from_multicall(
        Bytes::new(),
    )))
}

// Returns a read's result, as the other helpers do.
#[allow(clippy::unnecessary_wraps)]
fn text(value: &str) -> Read {
    Ok(IERC20Permit::nameCall::abi_encode_returns(&value.to_owned()).into())
}

/// The reads of a token without `eip712Domain()` that reports `separator`.
fn token(name: Read, version: Read, separator: B256) -> PermitReads {
    PermitReads {
        eip712_domain: reverted(),
        name,
        version,
        domain_separator: Ok(
            IERC20Permit::DOMAIN_SEPARATORCall::abi_encode_returns(&separator).into(),
        ),
        nonce: Ok(IERC20Permit::noncesCall::abi_encode_returns(&NONCE).into()),
    }
}

fn domain(
    name: &'static str,
    version: Option<&'static str>,
    chain_id: Option<u64>,
    salt: Option<B256>,
) -> Eip712Domain {
    Eip712Domain::new(
        Some(Cow::Borrowed(name)),
        version.map(Cow::Borrowed),
        chain_id.map(U256::from),
        Some(TOKEN),
        salt,
    )
}

// A token's permit is confirmed only under a domain whose hash is the separator the token
// reports, each one encoded here by hand as its contract computes it. Anything else the
// token answers means it has no permit, and a read that failed decides nothing.
#[test]
fn a_permit_is_confirmed_under_the_domain_that_reproduces_the_tokens_separator() {
    let (name, two, one) = (keccak256("Token"), keccak256("2"), keccak256("1"));
    let chain = U256::from(CHAIN);
    let confirmed = |domain| PermitDetection::Confirmed {
        domain,
        nonce: NONCE,
    };

    // EIP-5267 names its fields: here a domain without a version.
    let separator = keccak256(
        (
            keccak256("EIP712Domain(string name,uint256 chainId,address verifyingContract)"),
            name,
            chain,
            TOKEN,
        )
            .abi_encode(),
    );
    let reported = IERC20Permit::eip712DomainReturn {
        fields: [0x0d].into(),
        name: "Token".to_owned(),
        version: "unused".to_owned(),
        chainId: chain,
        verifyingContract: TOKEN,
        salt: B256::ZERO,
        extensions: Vec::new(),
    };
    let reads = PermitReads {
        eip712_domain: Ok(IERC20Permit::eip712DomainCall::abi_encode_returns(&reported).into()),
        ..token(reverted(), reverted(), separator)
    };
    assert_eq!(
        detect_permit(CHAIN, TOKEN, &reads),
        confirmed(domain("Token", None, Some(CHAIN), None))
    );
    // A reported domain that names another contract or another chain is some other
    // token's. Its separator is that domain's, and it is still no permit of this token.
    for (contract, chain_id) in [
        (Address::repeat_byte(0x11), chain),
        (TOKEN, U256::from(CHAIN + 1)),
    ] {
        let separator = keccak256(
            (
                keccak256("EIP712Domain(string name,uint256 chainId,address verifyingContract)"),
                name,
                chain_id,
                contract,
            )
                .abi_encode(),
        );
        let reported = IERC20Permit::eip712DomainReturn {
            chainId: chain_id,
            verifyingContract: contract,
            ..reported.clone()
        };
        let reads = PermitReads {
            eip712_domain: Ok(IERC20Permit::eip712DomainCall::abi_encode_returns(&reported).into()),
            ..token(reverted(), reverted(), separator)
        };
        assert_eq!(detect_permit(CHAIN, TOKEN, &reads), PermitDetection::Absent);
    }

    // `name()` and `version()`, and version "1" for a token without `version()`.
    let versioned =
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
    for (version, hash, expected) in [(text("2"), two, "2"), (reverted(), one, "1")] {
        let separator = keccak256((keccak256(versioned), name, hash, chain, TOKEN).abi_encode());
        assert_eq!(
            detect_permit(CHAIN, TOKEN, &token(text("Token"), version, separator)),
            confirmed(domain("Token", Some(expected), Some(CHAIN), None))
        );
    }

    // A bridged token's domain carries the chain id as its salt.
    let salt = B256::from(chain);
    let separator = keccak256(
        (
            keccak256(
                "EIP712Domain(string name,string version,address verifyingContract,bytes32 salt)",
            ),
            name,
            one,
            TOKEN,
            salt,
        )
            .abi_encode(),
    );
    assert_eq!(
        detect_permit(CHAIN, TOKEN, &token(text("Token"), text("1"), separator)),
        confirmed(domain("Token", Some("1"), None, Some(salt)))
    );

    // A separator no candidate reproduces, as DAI's, is no permit the wallet can sign.
    let other = B256::repeat_byte(9);
    assert_eq!(
        detect_permit(CHAIN, TOKEN, &token(text("Token"), text("2"), other)),
        PermitDetection::Absent
    );
    // A read that failed decides nothing, whatever the others answered.
    let failed = || Err(RpcBrokerError::Timeout);
    let mut unread = token(text("Token"), text("2"), other);
    unread.nonce = failed();
    assert_eq!(
        detect_permit(CHAIN, TOKEN, &unread),
        PermitDetection::Unknown
    );
    assert_eq!(
        detect_permit(CHAIN, TOKEN, &token(failed(), text("2"), other)),
        PermitDetection::Unknown
    );
}

// A covered allowance takes nothing, a short one a permit where the token has one, and
// otherwise the exact approval, after a reset when the allowance isn't zero. Only the
// transactions cost the account gas.
#[test]
fn the_approval_plan_is_none_a_permit_or_exact_transactions() {
    let amount = U256::from(200);
    let permit = PublicSwapPermitPlan {
        domain: domain("Token", Some("2"), Some(CHAIN), None),
        nonce: NONCE,
    };
    let plan = |allowance: u64, permit: Option<&PublicSwapPermitPlan>| {
        PublicSwapApprovalPlan::new(U256::from(allowance), amount, permit.cloned())
    };
    assert_eq!(plan(200, Some(&permit)), PublicSwapApprovalPlan::None);
    assert_eq!(
        PublicSwapApprovalPlan::new(U256::MAX, amount, None),
        PublicSwapApprovalPlan::None
    );
    for allowance in [0, 100] {
        assert_eq!(
            plan(allowance, Some(&permit)),
            PublicSwapApprovalPlan::Permit(permit.clone())
        );
    }
    assert_eq!(plan(0, None).transactions(), [amount]);
    assert_eq!(plan(100, None).transactions(), [U256::ZERO, amount]);

    let chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let gas = |plan: &PublicSwapApprovalPlan| {
        public_swap_gas_plan(&chain, plan.transactions().len(), false, 7, 1).unwrap()
    };
    for free in [plan(200, None), plan(0, Some(&permit))] {
        let gas = gas(&free);
        assert_eq!(gas.approval_gas_limits, [] as [u64; 0]);
        assert_eq!(gas.max_gas_cost, U256::ZERO);
    }
    assert_eq!(gas(&plan(100, None)).approval_gas_limits.len(), 2);
}

// The order's limit prices the permit pre-hook with its post-hook: at 4 wei per gas, which
// the limit raises to 5, and one bought unit per wei, the estimate grows by five times the
// hook's gas.
#[test]
fn an_orders_gas_estimate_includes_its_permit_hook() {
    let permit = PublicSwapApprovalPlan::Permit(PublicSwapPermitPlan {
        domain: domain("Token", Some("2"), Some(CHAIN), None),
        nonce: NONCE,
    });
    let estimate = |plan: &PublicSwapApprovalPlan| {
        let quote: CowQuote = serde_json::from_value(json!({
            "quote": {
                "sellToken": TOKEN, "buyToken": Address::repeat_byte(6),
                "sellAmount": "997500", "buyAmount": "10000000",
                "validTo": 1, "feeAmount": "2500", "gasAmount": "100000", "gasPrice": "99",
                "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true, "protocolFeeBps": "2"
        }))
        .unwrap();
        let price = SwapPrice::Verified {
            rate: PairAnchorRate {
                sell_rate: U256::ONE,
                buy_rate: uint!(1_000_000_000_000_000_000_U256),
            },
            observations: Vec::new(),
        };
        price_public_order(
            quote,
            &price,
            4,
            U256::ZERO,
            public_order_hook_gas(true, plan),
            Address::repeat_byte(6),
            100,
            GAS_SHARE_BALANCED_BPS,
            Address::repeat_byte(0x70),
            0,
            1_800,
        )
        .unwrap()
        .limit
        .gas_estimate
    };
    assert_eq!(
        public_order_hook_gas(true, &PublicSwapApprovalPlan::None),
        public_deposit_hook_gas(true, GasEstimateMode::UpperBound)
    );
    assert_eq!(
        estimate(&permit) - estimate(&PublicSwapApprovalPlan::None),
        U256::from(PUBLIC_PERMIT_HOOK_GAS) * U256::from(5)
    );
}

// An internal error that reports the revert still says the permit doesn't execute, and is
// still a strike against the endpoint that sent it.
#[tokio::test]
async fn a_permit_reverted_under_an_internal_error_withdraws_its_endpoint() {
    use std::sync::Arc;

    let (failing, failing_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(|request: Value| match request["method"].as_str() {
            Some("eth_chainId") => {
                json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"})
            }
            _ => json!({"jsonrpc": "2.0", "id": request["id"], "error":
                {"code": -32603, "message": "execution reverted", "data": "0x"}}),
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    // Another chain's endpoint is never admitted, so the permit is called on the first.
    let (other, other_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(|request: Value| json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x2"})),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    chain.rpc_route =
        crate::RpcChainRoute::new(1, vec![failing, other]).with_identity_verification();
    let client = OperationHttpClient::for_tests(
        reqwest::Client::new(),
        crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
    );
    let http = crate::HttpContext::direct_for_tests();
    let endpoints =
        ObservationEndpoints::new(&chain, &http, RpcTrafficClass::Sensitive, &client).unwrap();
    let call = TransactionRequest::default().to(TOKEN);
    assert!(!permit_executes(&endpoints, &call).await.unwrap());

    // The strike reaches the broker on its advisory channel.
    let route = chain
        .rpc_route
        .for_class(RpcTrafficClass::Sensitive)
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while http
            .rpc_broker()
            .endpoint_candidates(route.route(), None)
            .await
            .unwrap()
            != route.endpoints()[1..]
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    failing_server.abort();
    other_server.abort();
}
