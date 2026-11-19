use super::swap_order::delegate_setup;
use super::*;
use crate::vault::SwapUseSide;
use crate::{
    BroadcasterFeePolicyStatus, DesktopPrivateSpendAuthorization, ExecutorAccountUsed,
    ExecutorPrivateFeeLimitExceeded, ExecutorPrivateFeeTokenChanged, HardwareExecutorAction,
    PublicBroadcasterCandidate, SWAP_ACCOUNT_REPLACEMENT_LIMIT, SwapAccountReplacement,
    SwapAccountReplacements, SwapPairPreparation, SwapPairSide, SwapSetupStatus, SwapUseClaim,
    is_swap_destination_record, prepare_swap_pair, submit_swap_pair_setups_with, swap_setup_status,
};
use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use alloy::primitives::address;
use broadcaster_core::crypto::railgun::AddressData;

pub(super) const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
pub(super) const USDC: Address = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
pub(super) const DESTINATION_CHAIN: u64 = 137;
pub(super) const DESTINATION_TOKEN: Address = address!("3c499c542cef5e3811e1192ce70d8cc03d5c3359");

pub(super) fn broadcaster(delegate: Address) -> PublicBroadcasterCandidate {
    PublicBroadcasterCandidate {
        chain_id: 1,
        railgun_address: "0zk-broadcaster".to_owned(),
        identifier: None,
        token: Address::repeat_byte(0x33),
        fee: U256::from(10),
        fees_id: "fees-id".to_owned(),
        fee_expiration: std::time::SystemTime::now() + Duration::from_mins(1),
        reliability: 0.9,
        available_wallets: 1,
        version: "8.2.3".to_owned(),
        relay_adapt: Some(Address::repeat_byte(0x44)),
        relay_adapt_7702: Some(delegate),
        required_poi_list_keys: Vec::new(),
        viewing_public_key: [1; 32],
        address_data: AddressData {
            master_public_key: U256::ONE,
            viewing_public_key: [1; 32],
        },
        fee_policy_status: BroadcasterFeePolicyStatus::UnknownAnchor,
    }
}

pub(super) fn password() -> DesktopPrivateSpendAuthorization {
    DesktopPrivateSpendAuthorization::VaultPassword(Zeroizing::new(TEST_PASSWORD.into()))
}

/// A setup fee limit of `amount` in the fee token of `broadcaster`.
pub(super) fn setup_fee_limit(amount: u64) -> SwapSetupFeeLimit {
    SwapSetupFeeLimit {
        token: Address::repeat_byte(0x33),
        amount: U256::from(amount),
    }
}

/// Terms approved with a new swap's setup. Only the pair and delivery bind its first order; a
/// fresh review sets the order's bounds.
pub(super) fn setup_approval(sell: Address, buy: Address, delivery: SwapDelivery) -> SwapApproval {
    SwapApproval {
        bounds: SwapApprovedBounds {
            sell_amount: U256::from(997_500),
            unshield_amount: Some(U256::from(1_000_000)),
            unshield_fee_bps: U256::from(25),
            buy_amount: U256::ONE,
            private_minimum: U256::ONE,
            shield_fee_bps: U256::from(25),
            slippage_bps: 50,
            pre_hook_gas_limit: 1,
            post_hook_gas_limit: Some(1),
            hook_cost: Some(U256::ZERO),
            anchors: Vec::new(),
            destination_minimum: None,
            gas_share_bps: None,
            gas_estimate: None,
            gas_allowance: None,
            gas_price_wei: None,
            valid_for_secs: None,
            destination_shield_fee_bps: None,
            delivery_allowance: None,
            destination_setup_fee: None,
            source_setup_fee: None,
        },
        price_verified: Some(false),
        price_acknowledged: true,
        delivery,
        tokens: Some(crate::vault::SwapApprovalTokens { sell, buy }),
        accounts: None,
    }
}

/// Terms of a private Bridge swap to the destination chain, before its destination stealth
/// account is derived, with a setup fee limit for each chain.
pub(super) fn private_bridge_approval() -> SwapApproval {
    let mut approval = setup_approval(
        WETH,
        USDC,
        SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: DESTINATION_CHAIN,
            receiver: Address::ZERO,
            destination_token: DESTINATION_TOKEN,
            surplus: BridgeSurplus::Reshield,
            private: Some(BridgePrivateDelivery {
                on_shield_failure: BridgeShieldFailure::RefundOnOrigin,
            }),
        }),
    );
    approval.bounds.destination_setup_fee = Some(setup_fee_limit(1_000));
    approval.bounds.source_setup_fee = Some(setup_fee_limit(2_000));
    approval
}

/// A set-up account at `executor` on `store`'s chain with nothing unfinished: its setup won
/// nonce 0 and it signed nothing else.
pub(super) fn existing_account(
    store: &ExecutorStore,
    profile: crate::settings::ExecutorProfile,
    executor: Address,
) -> ExecutorOperationId {
    let operation = ExecutorOperationId::random().unwrap();
    store
        .reserve(operation, profile.delegate(), Some("Private swap"), &[])
        .unwrap();
    store.bind_address(operation, executor).unwrap();
    delegate_setup(store, operation, profile);
    operation
}

/// The destination chain's configuration over the shared mock, which answers as chain 1. A
/// relay answers `eth_chainId` for the destination chain and passes every other request on.
pub(super) async fn destination_chain_config(rpc: &Rpc) -> crate::settings::EffectiveChainConfig {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url: url::Url = format!("http://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let upstream = format!("127.0.0.1:{}", rpc.url.port().unwrap());
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(relay(stream, upstream.clone()));
        }
    });
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(DESTINATION_CHAIN)
            .cloned()
            .unwrap();
    chain.finality_depth = 1;
    chain.rpc_route =
        crate::RpcChainRoute::new(DESTINATION_CHAIN, vec![url]).with_multicall(multicall());
    chain
}

async fn relay(stream: tokio::net::TcpStream, upstream: String) -> std::io::Result<()> {
    let mut stream = BufReader::new(stream);
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = length.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body).await?;
    let request: Value = serde_json::from_slice(&body).unwrap();
    let response = if request["method"] == "eth_chainId" {
        let body = json!({
            "jsonrpc": "2.0", "id": request["id"], "result": format!("0x{DESTINATION_CHAIN:x}")
        })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    } else {
        let mut upstream = tokio::net::TcpStream::connect(upstream).await?;
        upstream
            .write_all(
                format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
            )
            .await?;
        upstream.write_all(&body).await?;
        let mut response = Vec::new();
        upstream.read_to_end(&mut response).await?;
        response
    };
    stream.get_mut().write_all(&response).await
}

mod account_choice;
mod pairs_and_completion;
mod private_bridge;
