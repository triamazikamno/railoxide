use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Instant, SystemTime};

use alloy::eips::BlockId;
use alloy::network::TransactionBuilder as _;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256, address};
use alloy::providers::bindings::IMulticall3;
use alloy::rpc::types::{TransactionRequest, transaction::AccessList};
use alloy::sol_types::{Revert, SolCall, SolError};
use eyre::eyre;
use local_db::{DbConfig, DbStore};
use railgun_wallet::tx::{ETHEREUM_GAS_MODEL, GasEstimateMode};
use reqwest::Url;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::types::PlannedPublicBalanceCall;
use super::*;
use crate::hardware::{
    ConfirmedHardwarePublicAccount, HardwareDerivationDescriptor, HardwareDeviceKind,
    HardwareOperationOutput, HardwarePublicAccountDescriptor, HardwareTypedDataSigningMode,
    HardwareWalletSyncIntent, hardware_view_access_key_from_hardware_output, parse_bip32_path,
    synthetic_entropy_from_hardware_output,
};
use crate::hardware_typed_data::HardwareEip712Model;
use crate::settings::{EffectiveChainConfig, EffectiveChainGasSettings};
use crate::signer::SoftwareEvmSigner;
use crate::vault::{
    CreateSoftwareContextResult, DesktopVaultStore, DesktopViewSession, HardwareProfileBinding,
    HardwareProfileSession, KdfParams, PublicAccountMetadata, PublicAccountScope,
    PublicAccountSource, PublicAccountStatus, SoftwareContextChainInput, SoftwareContextSyncIntent,
    SoftwareSeedSessionBinding, TrezorPassphraseMode, VaultError, VaultSessionId, WalletSource,
    bip39_seed_from_mnemonic,
};
use crate::{
    GAS_LIMIT_BUFFER, HttpContext, RpcChainRoute, RpcRead, RpcRoute, RpcSubmission,
    SelfBroadcastTipFallback, WalletNetworkMode, WalletRpcOrigin,
};
use crate::{WalletConnectDecodedCallKind, WalletConnectSupportedMethod};

pub(super) const TEST_PASSWORD: &str = "correct horse battery staple";

/// `route` restricted to Sensitive traffic, which its default-role endpoints all accept.
fn sensitive(route: &RpcChainRoute) -> crate::ClassRoute {
    route
        .for_class(crate::RpcTrafficClass::Sensitive)
        .expect("route has a Sensitive endpoint")
}
const TEST_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
pub(super) const TEST_IMPORTED_PRIVATE_KEY: &str =
    "0x59c6995e998f97a5a0044966f0945387e7d5e4a4dbd4b3f1b530b87d9b4a5c2f";
static TEMP_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
struct MockRpcCall {
    method: String,
    params: Value,
    route: Option<String>,
}

#[derive(Clone, Debug)]
enum MockRpcEstimateResult {
    Success(u64),
    Error {
        code: i64,
        message: String,
        data: Option<String>,
    },
}

struct MockRpcServer {
    url: String,
    calls: Arc<Mutex<Vec<MockRpcCall>>>,
    /// Fails every `eth_call` while set.
    fail_eth_call: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl MockRpcServer {
    async fn spawn(fail_requests: bool, estimate_gas: u64) -> Self {
        Self::spawn_with_estimate_result(
            fail_requests,
            MockRpcEstimateResult::Success(estimate_gas),
        )
        .await
    }

    async fn spawn_revert(reason: &str) -> Self {
        let data = Revert::from(reason).abi_encode();
        Self::spawn_with_estimate_result(
            false,
            MockRpcEstimateResult::Error {
                code: -32000,
                message: "execution reverted".to_owned(),
                data: Some(format!("0x{}", alloy::hex::encode(data))),
            },
        )
        .await
    }

    async fn spawn_with_estimate_result(
        fail_requests: bool,
        estimate_result: MockRpcEstimateResult,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind mock RPC listener");
        let address = listener.local_addr().expect("mock RPC address");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let task_calls = Arc::clone(&calls);
        let fail_eth_call = Arc::new(AtomicBool::new(false));
        let task_fail_eth_call = Arc::clone(&fail_eth_call);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let calls = Arc::clone(&task_calls);
                tokio::spawn(handle_mock_rpc_connection(
                    stream,
                    calls,
                    fail_requests,
                    estimate_result.clone(),
                    Arc::clone(&task_fail_eth_call),
                ));
            }
        });
        Self {
            url: format!("http://{address}"),
            calls,
            fail_eth_call,
            task,
        }
    }

    fn calls(&self) -> Vec<MockRpcCall> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for MockRpcServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_mock_rpc_connection(
    mut stream: tokio::net::TcpStream,
    calls: Arc<Mutex<Vec<MockRpcCall>>>,
    fail_requests: bool,
    estimate_result: MockRpcEstimateResult,
    fail_eth_call: Arc<AtomicBool>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (body_start, content_length) = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .starts_with("content-length:")
                    .then(|| {
                        line.split_once(':')
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                            .unwrap_or(0)
                    })
            })
            .unwrap_or(0);
        break (header_end, content_length);
    };
    while request.len() < body_start + content_length {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
    }

    let headers = String::from_utf8_lossy(&request[..body_start]);
    let route = headers.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .starts_with("x-railoxide-test-route:")
            .then(|| {
                line.split_once(':')
                    .map(|(_, value)| value.trim().to_owned())
            })
            .flatten()
    });
    let body: Value = serde_json::from_slice(&request[body_start..body_start + content_length])
        .expect("mock RPC JSON request");
    let method = body["method"].as_str().unwrap_or_default().to_owned();
    let params = body["params"].clone();
    calls
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(MockRpcCall {
            method: method.clone(),
            params,
            route,
        });

    let fail = fail_requests || (method == "eth_call" && fail_eth_call.load(Ordering::SeqCst));
    let result = if fail {
        json!({
            "jsonrpc": "2.0",
            "id": body["id"],
            "error": { "code": -32000, "message": "mock route failure" }
        })
    } else {
        let result = match method.as_str() {
            "eth_getBlockByNumber" => {
                let mut block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
                block.header.inner.base_fee_per_gas = Some(5);
                serde_json::to_value(block).unwrap()
            }
            "eth_gasPrice" => json!("0x64"),
            "eth_maxPriorityFeePerGas" => json!("0x2"),
            "eth_feeHistory" => json!({
                "oldestBlock": "0x1",
                "baseFeePerGas": ["0x5", "0x5"],
                "gasUsedRatio": [0.5],
                "reward": [["0x1", "0x2", "0x3", "0x4"]]
            }),
            "eth_estimateGas" => match &estimate_result {
                MockRpcEstimateResult::Success(estimate_gas) => {
                    json!(format!("0x{estimate_gas:x}"))
                }
                MockRpcEstimateResult::Error {
                    code,
                    message,
                    data,
                } => json!({
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "error": { "code": code, "message": message, "data": data }
                }),
            },
            "eth_getTransactionCount" => json!("0x7"),
            "eth_getBalance" => json!("0x3635c9adc5dea00000"),
            "eth_call" => json!(format!(
                "0x{}",
                alloy::hex::encode(U256::from(0x1234).to_be_bytes::<32>())
            )),
            _ => json!("0x1"),
        };
        if method == "eth_estimateGas"
            && matches!(&estimate_result, MockRpcEstimateResult::Error { .. })
        {
            result
        } else {
            json!({ "jsonrpc": "2.0", "id": body["id"], "result": result })
        }
    };
    let response = serde_json::to_vec(&result).expect("mock RPC response JSON");
    let response_headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    );
    stream.write_all(response_headers.as_bytes()).await?;
    stream.write_all(&response).await
}

fn default_effective_chain(id: u64) -> EffectiveChainConfig {
    crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
        .unwrap()
        .get(id)
        .cloned()
        .unwrap()
}

fn effective_chain_for_rpc(rpc_url: &str, gas_limit_buffer: u64) -> EffectiveChainConfig {
    let mut config =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    config.rpc_route = RpcChainRoute::new(1, vec![Url::parse(rpc_url).unwrap()])
        .with_multicall(config.rpc_route.multicall().unwrap());
    config.gas = EffectiveChainGasSettings {
        gas_limit_buffer,
        gas_price_buffer_numerator: 0,
        gas_price_buffer_denominator: 1,
    };
    config
}

fn http_context_for_route(mode: WalletNetworkMode, route: &str) -> HttpContext {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "x-railoxide-test-route",
        reqwest::header::HeaderValue::from_str(route).expect("route header"),
    );
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("mock RPC client");
    HttpContext::with_rpc_client_for_tests(client, mode)
}

fn rpc_calls_for<'a>(calls: &'a [MockRpcCall], method: &str) -> Vec<&'a MockRpcCall> {
    calls.iter().filter(|call| call.method == method).collect()
}

fn contains_transaction_field(value: &Value) -> bool {
    const TRANSACTION_FIELDS: [&str; 10] = [
        "from",
        "to",
        "input",
        "data",
        "value",
        "gas",
        "gasPrice",
        "maxFeePerGas",
        "maxPriorityFeePerGas",
        "nonce",
    ];
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            TRANSACTION_FIELDS.contains(&key.as_str()) || contains_transaction_field(value)
        }),
        Value::Array(values) => values.iter().any(contains_transaction_field),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

#[test]
fn public_action_attempt_errors_distinguish_signing_from_retryable_sending() {
    let signing = PublicActionAttemptError::Signing(eyre!("user rejected on device"));
    let sending = PublicActionAttemptError::Sending(eyre!("rpc rejected transaction"));

    assert!(matches!(signing, PublicActionAttemptError::Signing(_)));
    assert!(matches!(sending, PublicActionAttemptError::Sending(_)));
}

#[test]
fn public_action_pre_broadcast_checkpoint_yields_for_abort() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    runtime.block_on(async {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = ready_tx.send(());
            public_action_before_raw_broadcast_checkpoint().await;
            true
        });

        ready_rx.await.expect("checkpoint task started");
        task.abort();
        let error = task.await.expect_err("checkpoint task should abort");
        assert!(error.is_cancelled());
    });
}

#[test]
fn walletconnect_send_rejects_expired_request_before_raw_broadcast() {
    assert!(ensure_public_action_broadcast_not_expired(None, "walletconnect").is_ok());
    assert!(
        ensure_public_action_broadcast_not_expired(
            Some(public_action_current_unix_seconds() + 60),
            "walletconnect",
        )
        .is_ok()
    );

    let error = ensure_public_action_broadcast_not_expired(
        Some(public_action_current_unix_seconds()),
        "walletconnect",
    )
    .expect_err("expired request");

    assert!(
        error
            .to_string()
            .contains("request expired before transaction broadcast")
    );
}

fn test_kdf() -> KdfParams {
    KdfParams::new(1024, 1, 1)
}

fn temp_db_root() -> PathBuf {
    let dir = std::env::temp_dir().join("railoxide-public-wallet-tests");
    fs::create_dir_all(&dir).expect("create temp db dir");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = TEMP_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("db-{pid}-{nanos}-{counter}"))
}

pub(super) fn public_action_request_parts() -> (
    PathBuf,
    Arc<DbStore>,
    Arc<DesktopVaultStore>,
    Arc<DesktopViewSession>,
) {
    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = Arc::new(DesktopVaultStore::from_db(Arc::clone(&db)));
    let _created = store
        .create_vault_with_params(TEST_PASSWORD, test_kdf())
        .expect("create vault");
    let wallet_id = "public-action-wallet";
    let metadata = store
        .new_wallet_metadata(
            TEST_PASSWORD,
            wallet_id,
            0,
            WalletSource::Imported,
            "Public action",
        )
        .expect("wallet metadata");
    store
        .import_wallet_mnemonic_with_metadata(
            TEST_PASSWORD,
            wallet_id,
            0,
            "english",
            TEST_MNEMONIC,
            &metadata,
        )
        .expect("import wallet");
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, wallet_id)
            .expect("view session"),
    );
    (root_dir, db, store, view_session)
}

#[test]
fn balance_plan_batches_native_and_known_tokens_per_account() {
    let account = PublicAccountMetadata {
        public_account_uuid: "public-1".to_string(),
        address: address!("0x1111111111111111111111111111111111111111"),
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet-1".to_string(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let accounts = [
        account.clone(),
        PublicAccountMetadata {
            public_account_uuid: "public-2".to_string(),
            address: address!("0x2222222222222222222222222222222222222222"),
            ..account.clone()
        },
        PublicAccountMetadata {
            public_account_uuid: "public-3".to_string(),
            address: address!("0x3333333333333333333333333333333333333333"),
            ..account.clone()
        },
    ];
    let requested = [accounts[0].clone(), accounts[2].clone()];
    let calls = plan_public_balance_calls(
        &effective_chain_for_rpc("http://127.0.0.1:1", 0),
        &requested,
        None,
        BlockId::latest(),
    )
    .unwrap();
    // Only the two requested accounts of the three are read.
    let read_accounts: std::collections::BTreeSet<_> = calls
        .iter()
        .map(|call| call.public_account_uuid.as_str())
        .collect();
    assert_eq!(
        read_accounts,
        std::collections::BTreeSet::from(["public-1", "public-3"])
    );

    let native = calls.first().expect("native call");
    assert_eq!(native.asset.id, PublicAssetId::Native);
    assert_eq!(native.read, RpcRead::get_balance(account.address));
    let token = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let erc20 = calls
        .iter()
        .find(|call| call.asset.id == PublicAssetId::Erc20(token))
        .expect("known token call");
    assert_eq!(
        erc20.read,
        RpcRead::eth_call(
            token,
            PublicErc20::balanceOfCall {
                account: account.address,
            }
            .abi_encode()
            .into(),
        )
    );
}

#[tokio::test]
async fn balance_refresh_submits_more_wallet_reads_than_the_dapp_cap() {
    let accounts = (1_u8..=3)
        .map(|index| PublicAccountMetadata {
            public_account_uuid: format!("public-{index}"),
            address: alloy::primitives::Address::from([index; 20]),
            label: None,
            source: PublicAccountSource::Derived,
            scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: "wallet-1".to_string(),
            },
            derivation_index: Some(u32::from(index)),
            hardware_descriptor: None,
            status: PublicAccountStatus::Active,
            display_order: u32::from(index),
        })
        .collect::<Vec<_>>();
    let planned = plan_public_balance_calls(
        &effective_chain_for_rpc("http://127.0.0.1:1", 0),
        &accounts,
        None,
        BlockId::latest(),
    )
    .unwrap();
    assert!(
        planned.len() > 64,
        "a three-account chain 1 refresh must exceed the dapp read cap, planned {}",
        planned.len()
    );

    let (broker, _executions) = crate::rpc_broker::tests::spawn_counting_test_broker();
    let route = RpcRoute::from(sensitive(&RpcChainRoute::new(
        1,
        vec![Url::parse("https://balances.invalid").expect("route URL")],
    )));
    let results = broker
        .submit(RpcSubmission::new(
            route,
            planned.iter().map(|call| call.read.clone()).collect(),
            WalletRpcOrigin::PublicWallet.into(),
        ))
        .await
        .expect("wallet balance submission is admitted");
    assert_eq!(results.len(), planned.len());
    assert!(results.iter().all(std::result::Result::is_ok));
}

fn balance_word_for_test(value: u64) -> Bytes {
    Bytes::from(U256::from(value).to_be_bytes::<32>().to_vec())
}

fn aggregated_calls_for_test(request: &Value) -> Vec<IMulticall3::Call> {
    let input = alloy::hex::decode(request["params"][0]["input"].as_str().unwrap()).unwrap();
    IMulticall3::tryAggregateCall::abi_decode(&input)
        .expect("one aggregate per account")
        .calls
}

#[tokio::test]
async fn balance_refresh_pins_one_account_to_its_route_block_number() {
    const BLOCK: u64 = 10;
    enum Case {
        Accepted,
        BelowMinimum,
        FailedToken,
        // The first two-member aggregate answers -32000, so the broker splits it.
        Reduced,
    }
    let account = PublicAccountMetadata {
        public_account_uuid: "public-1".to_string(),
        address: Address::repeat_byte(1),
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet-1".to_string(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let token = Address::repeat_byte(2);
    let registry = crate::settings::EffectiveTokenRegistry {
        tokens: [(
            (1, token.to_string()),
            crate::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: token.to_string(),
                symbol: "TEST".to_string(),
                decimals: 18,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]
        .into(),
    };
    for case in [
        Case::Accepted,
        Case::BelowMinimum,
        Case::FailedToken,
        Case::Reduced,
    ] {
        let minimum = match case {
            Case::Accepted => Some(BLOCK),
            Case::BelowMinimum => Some(BLOCK + 1),
            Case::FailedToken | Case::Reduced => None,
        };
        let fail_token = matches!(case, Case::FailedToken);
        let reduce = Arc::new(AtomicBool::new(matches!(case, Case::Reduced)));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request| {
                recorded.lock().unwrap().push(request.clone());
                if request["method"] == "eth_blockNumber" {
                    return json!({"jsonrpc": "2.0", "id": request["id"], "result": "0xa"});
                }
                let members = aggregated_calls_for_test(&request);
                if members.len() > 1 && reduce.swap(false, Ordering::SeqCst) {
                    return json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "too large"}});
                }
                let returns = members
                    .into_iter()
                    .map(|call| {
                        let (success, data) = if call
                            .callData
                            .starts_with(&IMulticall3::getEthBalanceCall::SELECTOR)
                        {
                            (true, balance_word_for_test(7))
                        } else if fail_token {
                            (false, Bytes::new())
                        } else {
                            (true, balance_word_for_test(9))
                        };
                        IMulticall3::Result {
                            success,
                            returnData: data,
                        }
                    })
                    .collect::<Vec<_>>();
                let result =
                    Bytes::from(IMulticall3::tryAggregateCall::abi_encode_returns(&returns));
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            }),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let chain = effective_chain_for_rpc(endpoint.as_str(), 0);
        let http = HttpContext::direct_for_tests();
        let started = Instant::now();
        let result =
            refresh_public_balances_at_least(1, &account, &chain, Some(&registry), &http, minimum)
                .await;
        let calls = calls.lock().unwrap();
        // The route's block number comes first, then the balances pinned to it.
        assert_eq!(calls[0]["method"], "eth_blockNumber");
        let balance_calls = &calls[1..];
        for call in balance_calls {
            assert_eq!(call["method"], "eth_call");
            assert_eq!(call["params"][1], json!("0xa"), "pinned to block {BLOCK}");
            assert!(aggregated_calls_for_test(call).iter().all(|member| {
                !member
                    .callData
                    .starts_with(&IMulticall3::getBlockNumberCall::SELECTOR)
            }));
        }
        match case {
            Case::BelowMinimum => {
                assert!(result.is_err(), "a number below the minimum is not current");
                assert!(balance_calls.is_empty(), "no balance read is sent");
            }
            Case::Accepted | Case::Reduced => {
                if matches!(case, Case::Reduced) {
                    assert!(balance_calls.len() > 1, "the aggregate was split");
                } else {
                    assert_eq!(balance_calls.len(), 1);
                    assert_eq!(aggregated_calls_for_test(&balance_calls[0]).len(), 2);
                }
                let snapshot = result.unwrap();
                let row = &snapshot.accounts[0];
                assert_eq!(row.balances.len(), 2);
                assert!(
                    row.balances
                        .iter()
                        .all(|balance| balance.amount.amount().is_some())
                );
                assert_eq!(row.observed_block, Some(BLOCK));
                assert!(row.observed_at.unwrap() >= started);
            }
            Case::FailedToken => {
                // A failed balance member keeps the balances without an observation.
                assert_eq!(balance_calls.len(), 1);
                let snapshot = result.unwrap();
                let row = &snapshot.accounts[0];
                assert_eq!(row.balances.len(), 2);
                assert_eq!(
                    row.balances
                        .iter()
                        .filter(|balance| balance.amount.amount().is_some())
                        .count(),
                    1
                );
                assert_eq!(row.observed_block, None);
                assert_eq!(row.observed_at, None);
            }
        }
        server.abort();
    }

    // Without an isolated route the account fails and nothing goes over the shared one.
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request| {
            recorded.lock().unwrap().push(request.clone());
            json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x"})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let chain = effective_chain_for_rpc(endpoint.as_str(), 0);
    let http = http_context_for_route(WalletNetworkMode::Tor, "tor");
    assert!(
        refresh_public_balances_at_least(1, &account, &chain, Some(&registry), &http, None)
            .await
            .is_err()
    );
    assert!(calls.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn custom_balance_refresh_preserves_partial_results_without_multicall() {
    const CHAIN: u64 = 999_999;
    let account = PublicAccountMetadata {
        public_account_uuid: "public-1".to_string(),
        address: address!("1111111111111111111111111111111111111111"),
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet-1".to_string(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let tokens = [
        Address::from([1; 20]),
        Address::from([2; 20]),
        Address::from([3; 20]),
    ];
    let registry = crate::settings::EffectiveTokenRegistry {
        tokens: tokens
            .into_iter()
            .map(|token| {
                let address = token.to_string();
                (
                    (CHAIN, address.clone()),
                    crate::settings::EffectiveTokenInfo {
                        chain_id: CHAIN,
                        token_address: address,
                        symbol: "TEST".to_string(),
                        decimals: 18,
                        icon_path: None,
                        price_anchor: None,
                        built_in: false,
                    },
                )
            })
            .collect(),
    };
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request| {
            let result = if request["method"] == "eth_chainId" {
                json!(alloy::primitives::U64::from(CHAIN))
            } else if request["method"] == "eth_blockNumber" {
                json!("0xa")
            } else if request["params"][1] != json!("0xa") {
                // Each individual read must be pinned to the route's block number.
                return json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "unpinned"}});
            } else if request["method"] == "eth_getBalance" {
                json!("0x7")
            } else {
                let transaction: TransactionRequest = serde_json::from_value(request["params"][0].clone()).unwrap();
                match transaction.to {
                    Some(TxKind::Call(token)) if token == tokens[0] => json!(Bytes::copy_from_slice(&U256::from(9).to_be_bytes::<32>())),
                    Some(TxKind::Call(token)) if token == tokens[1] => json!("0x09"),
                    _ => return json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32602, "message": "unavailable"}}),
                }
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        }),
        Arc::default(),
        Arc::default(),
    ).await;
    let mut settings = crate::settings::WalletSettings::default();
    settings.chains.custom.insert(
        CHAIN,
        crate::settings::CustomChainSettings {
            native_usd_pricing: crate::settings::NativeUsdPricing::Default,
            name: "Custom test network".into(),
            native_currency: crate::settings::NativeCurrency {
                name: "Custom coin".into(),
                symbol: "CUSTOM".into(),
                decimals: 6,
            },
            rpc_endpoints: vec![endpoint.to_string().into()],
            explorer_urls: vec![],
            enabled: true,
            contracts: crate::settings::ChainContractSettings::default(),
            finality_depth: None,
            gas: crate::settings::ChainGasSettings::default(),
        },
    );
    let chain = crate::settings::build_effective_chain_configs(&settings)
        .unwrap()
        .get(CHAIN)
        .cloned()
        .unwrap();
    assert!(chain.railgun.is_none());
    assert!(chain.rpc_route.multicall().is_none());
    let http = HttpContext::direct_for_tests();
    let snapshot = refresh_public_balances(CHAIN, &account, &chain, Some(&registry), &http)
        .await
        .unwrap();
    let balances = &snapshot.accounts[0].balances;
    let native = &balances
        .iter()
        .find(|balance| balance.asset.id == PublicAssetId::Native)
        .unwrap()
        .asset;
    assert_eq!(native.symbol, "CUSTOM");
    assert_eq!(native.decimals, 6);
    let amount = |asset| {
        &balances
            .iter()
            .find(|balance| balance.asset.id == asset)
            .unwrap()
            .amount
    };
    assert_eq!(
        amount(PublicAssetId::Native),
        &PublicBalanceAmount::Available(U256::from(7))
    );
    assert_eq!(
        amount(PublicAssetId::Erc20(tokens[0])),
        &PublicBalanceAmount::Available(U256::from(9))
    );
    for token in &tokens[1..] {
        assert_eq!(
            amount(PublicAssetId::Erc20(*token)),
            &PublicBalanceAmount::Unavailable
        );
    }
    server.abort();
}

#[test]
fn walletconnect_personal_sign_uses_spend_authorized_public_signer() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("WalletConnect signer"),
            false,
        )
        .expect("import public account");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let denied = runtime.block_on(walletconnect_sign_personal_message(
        WalletConnectPersonalSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: None,
            view_session: Arc::clone(&view_session),
            vault_store: Arc::clone(&store),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new("wrong password".to_owned()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: account.public_account_uuid.clone(),
            message: b"hello".to_vec(),
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    ));
    assert!(denied.is_err());

    let signature = runtime
        .block_on(walletconnect_sign_personal_message(
            WalletConnectPersonalSignRequest {
                executor_owner: None,
                chain_id: 1,
                request_control: None,
                view_session: Arc::clone(&view_session),
                vault_store: Arc::clone(&store),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_owned()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid,
                message: b"hello".to_vec(),
                event_tx: None,
            },
            crate::OperationHttpClient::direct_for_tests(),
        ))
        .expect("personal sign");

    assert!(signature.starts_with("0x"));
    assert_eq!(signature.len(), 132);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn walletconnect_typed_data_signs_for_software_public_account() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("WalletConnect typed data"),
            false,
        )
        .expect("import public account");
    let typed_data = serde_json::json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "version", "type": "string" },
                { "name": "chainId", "type": "uint256" }
            ],
            "Message": [
                { "name": "contents", "type": "string" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "version": "1",
            "chainId": 1
        },
        "message": {
            "contents": "hello"
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let signature = runtime
        .block_on(walletconnect_sign_typed_data_v4(
            WalletConnectTypedDataSignRequest {
                executor_owner: None,
                chain_id: 1,
                request_control: None,
                view_session: Arc::clone(&view_session),
                vault_store: Arc::clone(&store),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_owned()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid,
                typed_data,
                hash_fallback_confirmed: false,
                event_tx: None,
            },
            crate::OperationHttpClient::direct_for_tests(),
        ))
        .expect("typed-data sign");

    assert!(signature.starts_with("0x"));
    assert_eq!(signature.len(), 132);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn walletconnect_typed_data_signs_primitive_prefixed_custom_types_for_software_public_account() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("WalletConnect custom typed data"),
            false,
        )
        .expect("import public account");
    let typed_data = serde_json::json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "chainId", "type": "uint256" }
            ],
            "bytesPayload": [
                { "name": "digest", "type": "bytes32" }
            ],
            "Message": [
                { "name": "payload", "type": "bytesPayload" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "chainId": 1
        },
        "message": {
            "payload": {
                "digest": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let signature = runtime
        .block_on(walletconnect_sign_typed_data_v4(
            WalletConnectTypedDataSignRequest {
                executor_owner: None,
                chain_id: 1,
                request_control: None,
                view_session: Arc::clone(&view_session),
                vault_store: Arc::clone(&store),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_owned()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid,
                typed_data,
                hash_fallback_confirmed: false,
                event_tx: None,
            },
            crate::OperationHttpClient::direct_for_tests(),
        ))
        .expect("typed-data sign");

    assert!(signature.starts_with("0x"));
    assert_eq!(signature.len(), 132);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn walletconnect_typed_data_signing_error_preserves_method_label() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("WalletConnect typed data diagnostics"),
            false,
        )
        .expect("import public account");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    for method in [
        WalletConnectSupportedMethod::EthSignTypedData,
        WalletConnectSupportedMethod::EthSignTypedDataV4,
    ] {
        let error = runtime
            .block_on(walletconnect_sign_typed_data(
                WalletConnectTypedDataSignRequest {
                    executor_owner: None,
                    chain_id: 1,
                    request_control: None,
                    view_session: Arc::clone(&view_session),
                    vault_store: Arc::clone(&store),
                    authorization: Some(
                        crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                            Zeroizing::new("wrong password".to_owned()),
                            None,
                        ),
                    ),
                    trezor_app_passphrase: None,
                    trezor_pin_matrix_provider: None,
                    public_account_uuid: account.public_account_uuid.clone(),
                    typed_data: serde_json::json!({}),
                    hash_fallback_confirmed: false,
                    event_tx: None,
                },
                method,
                crate::OperationHttpClient::direct_for_tests(),
            ))
            .expect_err("typed-data signing should fail with the wrong password");
        let message = error
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(": ");
        assert!(message.contains(method.as_str()));
        if method == WalletConnectSupportedMethod::EthSignTypedData {
            assert!(!message.contains(WalletConnectSupportedMethod::EthSignTypedDataV4.as_str()));
        } else {
            assert!(message.contains(WalletConnectSupportedMethod::EthSignTypedDataV4.as_str()));
        }
    }

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn hardware_typed_data_hash_fallback_confirmation_error_survives_context() {
    let session = HardwareProfileSession::matched(
        HardwareDeviceKind::Ledger,
        "profile-a",
        HardwareProfileBinding::evm_address_fingerprint("fingerprint-a"),
        None,
    );
    let error = eyre::Report::from(
        WalletConnectHardwareTypedDataHashFallbackConfirmationRequired::new(Some(session.clone())),
    )
    .wrap_err("WalletConnect eth_signTypedData_v4");

    assert!(is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required(&error));
    assert_eq!(
        walletconnect_hardware_typed_data_hash_fallback_confirmation_session(&error),
        Some(session)
    );
    assert_eq!(
        format!(
            "{:?}",
            error
                .downcast_ref::<WalletConnectHardwareTypedDataHashFallbackConfirmationRequired>()
                .expect("confirmation error")
        ),
        "WalletConnectHardwareTypedDataHashFallbackConfirmationRequired"
    );
}

fn hardware_typed_data_signer_with_mode(
    mode: HardwareTypedDataSigningMode,
) -> HardwarePublicEvmSigner {
    let descriptor =
        HardwarePublicAccountDescriptor::for_wallet_public_index(HardwareDeviceKind::Ledger, 0, 0)
            .expect("ledger descriptor");
    let mut hardware_session = HardwareProfileSession::unmatched(
        HardwareDeviceKind::Ledger,
        HardwareProfileBinding::evm_address_fingerprint(
            "ledger:evm:0x1111111111111111111111111111111111111111",
        ),
        None,
    );
    hardware_session
        .cache_typed_data_signing_mode(&descriptor, mode)
        .expect("cache typed-data mode");
    HardwarePublicEvmSigner {
        request_control: None,
        address: address!("0x1111111111111111111111111111111111111111"),
        descriptor,
        hardware_session: std::sync::Mutex::new(hardware_session),
        trezor_app_passphrase: std::sync::Mutex::new(None),
        trezor_pin_matrix_provider: None,
    }
}

fn hardware_typed_data_model_for_tests() -> HardwareEip712Model {
    HardwareEip712Model::from_walletconnect_typed_data_json(serde_json::json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "chainId", "type": "uint256" }
            ],
            "Message": [
                { "name": "contents", "type": "string" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "chainId": 1
        },
        "message": {
            "contents": "hello"
        }
    }))
    .expect("typed-data model")
}

#[test]
fn hardware_public_signer_requires_hash_fallback_confirmation_before_signing() {
    let signer =
        hardware_typed_data_signer_with_mode(HardwareTypedDataSigningMode::Eip712HashFallback);
    let model = hardware_typed_data_model_for_tests();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let error = runtime
        .block_on(signer.sign_typed_data_v4(
            &model,
            Some(HardwareTypedDataSigningMode::Eip712HashFallback),
            false,
        ))
        .expect_err("fallback confirmation required");

    assert!(is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required(&error));
    assert_eq!(
        walletconnect_hardware_typed_data_hash_fallback_confirmation_session(&error)
            .and_then(|session| session.typed_data_signing_mode(&signer.descriptor)),
        Some(HardwareTypedDataSigningMode::Eip712HashFallback)
    );
}

#[cfg(not(feature = "hardware"))]
#[test]
fn hardware_public_signer_rejects_confirmed_hash_fallback_without_hardware_feature() {
    let signer =
        hardware_typed_data_signer_with_mode(HardwareTypedDataSigningMode::Eip712HashFallback);
    let model = hardware_typed_data_model_for_tests();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let error = runtime
        .block_on(signer.sign_typed_data_v4(
            &model,
            Some(HardwareTypedDataSigningMode::Eip712HashFallback),
            true,
        ))
        .expect_err("default build cannot sign fallback");

    assert!(
        error
            .to_string()
            .contains("hardware public signing is not enabled in this build")
    );
}

#[test]
fn hardware_typed_data_signature_recovery_mismatch_rejects() {
    let model = hardware_typed_data_model_for_tests();
    let signer = SoftwareEvmSigner::from_private_key([7u8; 32]).expect("software signer");
    let signature = signer
        .sign_typed_data_v4(model.typed_data())
        .expect("typed-data signature");

    let error = verify_hardware_typed_data_signature_address(
        address!("0x1111111111111111111111111111111111111111"),
        &signature,
        &model,
    )
    .expect_err("recovery mismatch");

    assert!(
        error
            .to_string()
            .contains("hardware public signer address mismatch")
    );
}

#[test]
fn balance_assets_use_effective_token_registry_overlays() {
    let mut settings = crate::settings::WalletSettings::default();
    settings
        .tokens
        .built_in_tombstones
        .push(crate::settings::TokenKey {
            chain_id: 1,
            token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
        });
    settings
        .tokens
        .custom_tokens
        .push(crate::settings::CustomTokenSettings {
            chain_id: 1,
            token_address: "0x0000000000000000000000000000000000000002".to_string(),
            symbol: "CSTM".to_string(),
            decimals: 9,
            icon_path: None,
            price_anchor: None,
        });
    let registry = crate::settings::build_effective_token_registry(&settings)
        .expect("effective token registry");

    let assets = public_balance_assets_for_chain_with_registry(
        &effective_chain_for_rpc("http://127.0.0.1:1", 0),
        Some(&registry),
    );

    assert!(assets.iter().any(|asset| asset.id == PublicAssetId::Native));
    assert!(!assets.iter().any(|asset| {
        asset.id == PublicAssetId::Erc20(address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"))
    }));
    let custom = assets
        .iter()
        .find(|asset| {
            asset.id == PublicAssetId::Erc20(address!("0x0000000000000000000000000000000000000002"))
        })
        .expect("custom token asset");
    assert_eq!(custom.symbol, "CSTM");
    assert_eq!(custom.decimals, 9);
}

#[test]
fn balance_snapshot_preserves_partial_success() {
    let account = PublicAccountMetadata {
        public_account_uuid: "public-1".to_string(),
        address: address!("0x1111111111111111111111111111111111111111"),
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet-1".to_string(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let mut successful_account = account.clone();
    successful_account.public_account_uuid = "public-2".to_string();
    let mut planned = vec![
        PlannedPublicBalanceCall {
            public_account_uuid: account.public_account_uuid.clone(),
            asset: PublicBalanceAsset {
                id: PublicAssetId::Native,
                symbol: "ETH".to_string(),
                decimals: 18,
            },
            read: RpcRead::get_balance(account.address),
        },
        PlannedPublicBalanceCall {
            public_account_uuid: account.public_account_uuid.clone(),
            asset: PublicBalanceAsset {
                id: PublicAssetId::Erc20(address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")),
                symbol: "WETH".to_string(),
                decimals: 18,
            },
            read: RpcRead::eth_call(
                address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
                Bytes::new(),
            ),
        },
    ];

    let mut successful_call = planned[0].clone();
    successful_call.public_account_uuid = successful_account.public_account_uuid.clone();
    planned.push(successful_call);
    let observed_at = Instant::now();
    let observed_block = Some(10);
    let snapshot = public_balance_snapshot_from_results(
        1,
        &[account, successful_account],
        &planned,
        vec![Some(U256::from(7_u64)), None, Some(U256::from(9_u64))],
        observed_at,
        observed_block,
    );

    assert_eq!(snapshot.accounts[0].observed_at, None);
    assert_eq!(snapshot.accounts[0].observed_block, None);
    assert_eq!(snapshot.accounts[1].observed_at, Some(observed_at));
    assert_eq!(snapshot.accounts[1].observed_block, observed_block);
    let balances = &snapshot.accounts[0].balances;
    assert_eq!(balances[0].amount.amount(), Some(U256::from(7_u64)));
    assert!(matches!(
        balances[1].amount,
        PublicBalanceAmount::Unavailable
    ));

    let truncated = public_balance_snapshot_from_results(
        1,
        &[snapshot.accounts[0].account.clone()],
        &planned[..2],
        vec![Some(U256::from(7_u64))],
        observed_at,
        observed_block,
    );
    assert_eq!(truncated.accounts[0].observed_at, None);
    assert_eq!(truncated.accounts[0].observed_block, None);
}

#[test]
fn refresh_coordinator_prevents_overlap_and_releases() {
    let coordinator = PublicBalanceRefreshCoordinator::new();
    let guard = coordinator.try_begin().expect("first refresh guard");

    assert!(coordinator.is_refreshing());
    assert!(coordinator.try_begin().is_none());
    drop(guard);
    assert!(!coordinator.is_refreshing());
    assert!(coordinator.try_begin().is_some());
}

#[test]
fn public_native_action_gas_reserve_uses_buffered_units() {
    let send_steps = [PublicActionProgressStep::Send];
    assert_eq!(
        public_native_action_gas_units(1, &send_steps),
        PUBLIC_NATIVE_SEND_GAS_UNITS + GAS_LIMIT_BUFFER,
    );
    assert_eq!(
        public_native_action_gas_reserve(1, 2, &send_steps),
        U256::from((PUBLIC_NATIVE_SEND_GAS_UNITS + GAS_LIMIT_BUFFER) * 2),
    );

    let shield_steps = [
        PublicActionProgressStep::ShieldKey,
        PublicActionProgressStep::Shield,
    ];
    assert_eq!(
        public_native_action_gas_units(1, &shield_steps),
        public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::UpperBound)
            + GAS_LIMIT_BUFFER,
    );
    assert_eq!(
        public_native_action_gas_units_with_buffer(&ETHEREUM_GAS_MODEL, &send_steps, 7),
        PUBLIC_NATIVE_SEND_GAS_UNITS + 7,
    );
}

#[test]
fn public_action_gas_cost_separates_execution_and_signed_units() {
    let token = address!("0x3333333333333333333333333333333333333333");
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 2,
        current_base_fee_per_gas: Some(1),
        suggested_max_fee_per_gas: 3,
        suggested_max_priority_fee_per_gas: 1,
    };

    let native_send = estimate_public_action_gas_cost(
        1,
        &default_effective_chain(1),
        PublicActionKind::Send,
        PublicAssetId::Native,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("native send estimate");
    assert_eq!(
        native_send.expected_cost,
        U256::from(PUBLIC_NATIVE_SEND_GAS_UNITS * 3)
    );
    assert_eq!(
        native_send.maximum_cost,
        U256::from((PUBLIC_NATIVE_SEND_GAS_UNITS + GAS_LIMIT_BUFFER) * 3)
    );

    let erc20_send = estimate_public_action_gas_cost(
        1,
        &default_effective_chain(1),
        PublicActionKind::Send,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("erc20 send estimate");
    assert_eq!(erc20_send.expected_cost, U256::from(65_000_u64 * 3));
    assert_eq!(
        erc20_send.maximum_cost,
        U256::from((65_000 + GAS_LIMIT_BUFFER) * 3)
    );

    let native_shield = estimate_public_action_gas_cost(
        1,
        &default_effective_chain(1),
        PublicActionKind::Shield,
        PublicAssetId::Native,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
        },
        None,
    )
    .expect("native shield estimate");
    // The Railoxide wrap fallback: wrap, approval and Shield, each with its own buffer.
    assert_eq!(
        native_shield.expected_cost,
        U256::from(
            PUBLIC_NATIVE_WRAP_GAS_UNITS
                + PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::Expected)
        )
    );
    assert_eq!(
        native_shield.maximum_cost,
        U256::from(
            PUBLIC_NATIVE_WRAP_GAS_UNITS
                + PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::UpperBound)
                + (3 * GAS_LIMIT_BUFFER)
        )
    );
    let erc20_shield = estimate_public_action_gas_cost(
        1,
        &default_effective_chain(1),
        PublicActionKind::Shield,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
        },
        None,
    )
    .expect("erc20 shield estimate");
    assert_eq!(
        erc20_shield.expected_cost,
        U256::from(
            PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::Expected)
        )
    );
    assert_eq!(
        erc20_shield.maximum_cost,
        U256::from(
            PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::UpperBound)
                + (2 * GAS_LIMIT_BUFFER),
        )
    );
}

#[test]
fn walletconnect_decoded_intents_use_only_the_fixed_operation_table() {
    let address = address!("0x3333333333333333333333333333333333333333");
    let amount = U256::from(7_u64);
    let cases = [
        (
            WalletConnectDecodedCallKind::NativeTransfer,
            PUBLIC_NATIVE_SEND_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::Erc20Transfer {
                recipient: address,
                amount,
            },
            PUBLIC_ERC20_SEND_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::Erc20TransferFrom {
                from: address,
                to: address,
                amount,
            },
            PUBLIC_ERC20_SEND_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::Erc20Approve {
                spender: address,
                amount,
            },
            PUBLIC_NATIVE_APPROVE_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::WrappedDeposit,
            PUBLIC_NATIVE_WRAP_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::WrappedWithdraw { amount },
            PUBLIC_NATIVE_UNWRAP_GAS_UNITS,
        ),
    ];
    for (kind, expected) in cases {
        assert_eq!(
            public_native_action_gas_units_from_walletconnect_intent(&kind),
            Some(expected)
        );
    }
    assert_eq!(
        public_native_action_gas_units_from_walletconnect_intent(
            &WalletConnectDecodedCallKind::ContractCall { selector: None },
        ),
        None
    );
    assert_eq!(
        public_native_action_gas_units_from_walletconnect_intent(
            &WalletConnectDecodedCallKind::ContractCreation,
        ),
        None
    );
    assert_eq!(
        public_walletconnect_operation_gas_limit(
            &WalletConnectDecodedCallKind::NativeTransfer,
            123,
        ),
        Some(PUBLIC_NATIVE_SEND_GAS_UNITS + 123)
    );
    assert_eq!(
        public_walletconnect_operation_gas_limit(
            &WalletConnectDecodedCallKind::ContractCreation,
            123,
        ),
        None
    );
}

#[test]
fn walletconnect_fee_projection_keeps_raw_buffered_source_and_optional_usd() {
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(200),
        suggested_max_fee_per_gas: 120,
        suggested_max_priority_fee_per_gas: 3,
    };
    let resolved = PublicActionResolvedGasFee {
        rpc_gas_price: 100,
        max_fee_per_gas: 120,
        max_priority_fee_per_gas: 3,
    };
    let projection = project_public_action_fee(
        100,
        110,
        quote,
        resolved,
        PublicActionFeeSource::OperationTable,
        Some(U256::from(2_000_000_u64)),
        18,
    );
    assert_eq!(projection.expected_fee_per_gas, 120);
    assert_eq!(projection.expected_gas_cost, U256::from(12_000));
    assert_eq!(projection.maximum_gas_cost, U256::from(13_200));
    assert_eq!(projection.source, PublicActionFeeSource::OperationTable);
    assert_eq!(projection.expected_native_usd_micro_value, Some(U256::ZERO));
    assert_eq!(
        crate::expected_eip1559_fee_per_gas(quote, 120, 3),
        120,
        "fee history caps the projected effective fee at the selected maximum"
    );
    assert_eq!(
        crate::expected_eip1559_fee_per_gas(
            PublicActionGasFeeQuote {
                current_base_fee_per_gas: None,
                ..quote
            },
            120,
            3,
        ),
        120,
        "gas-price fallback uses the selected maximum as the expected fee"
    );
    let estimate = PublicAdvancedTransactionEstimate {
        payload_fingerprint: B256::ZERO,
        raw_gas_limit: 100,
        gas_limit: 110,
        max_fee_per_gas: 120,
        max_priority_fee_per_gas: 3,
        expected_fee_per_gas: 120,
        expected_gas_cost: U256::from(12_000_u64),
        max_gas_cost: U256::from(13_200_u64),
    };
    let custom_precision = estimate.fee_projection(Some(U256::from(2_000_000)), 6);
    assert_eq!(
        custom_precision.expected_native_usd_micro_value,
        Some(U256::from(24_000))
    );
    assert_eq!(
        custom_precision.maximum_native_usd_micro_value,
        Some(U256::from(26_400))
    );
    let estimate_projection =
        estimate.fee_projection(Some(U256::from(1_000_000_000_000_000_000_u128)), 18);
    assert_eq!(
        estimate_projection.source,
        PublicActionFeeSource::NetworkSimulation
    );
    assert_eq!(estimate_projection.raw_gas_limit, estimate.raw_gas_limit);
    assert_eq!(estimate_projection.gas_limit, estimate.gas_limit);
    assert_eq!(
        estimate_projection.expected_fee_per_gas,
        estimate.expected_fee_per_gas
    );
    assert_eq!(
        estimate_projection.expected_gas_cost,
        estimate.expected_gas_cost
    );
    assert_eq!(estimate_projection.maximum_gas_cost, estimate.max_gas_cost);
    assert_eq!(
        estimate_projection.expected_native_usd_micro_value,
        Some(estimate.expected_gas_cost)
    );
    assert_eq!(
        estimate_projection.maximum_native_usd_micro_value,
        Some(estimate.max_gas_cost)
    );
    assert!(public_action_maximum_gas_cost_is_significant(
        projection.expected_gas_cost,
        projection.maximum_gas_cost,
    ));

    assert!(!public_action_maximum_gas_cost_is_significant(
        U256::from(100_u64),
        U256::from(100_u64),
    ));
    assert!(!public_action_maximum_gas_cost_is_significant(
        U256::from(100_u64),
        U256::from(109_u64),
    ));
    assert!(public_action_maximum_gas_cost_is_significant(
        U256::ZERO,
        U256::from(1_u64),
    ));
}

#[test]
fn walletconnect_reviewed_fingerprint_rejects_changed_payload() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let request = TransactionRequest::default()
        .with_from(from)
        .with_to(to)
        .with_value(U256::from(1_u64));
    let fingerprint = walletconnect_transaction_payload_fingerprint(1, from, &request);
    assert!(
        validate_walletconnect_reviewed_transaction(1, from, &request, fingerprint, 55).is_ok()
    );
    let changed = request.clone().with_value(U256::from(2_u64));
    assert!(
        validate_walletconnect_reviewed_transaction(1, from, &changed, fingerprint, 55).is_err()
    );
    assert!(
        validate_walletconnect_reviewed_transaction(1, from, &request, fingerprint, 0).is_err()
    );
}

#[test]
fn walletconnect_custom_fee_resolves_without_an_automatic_quote() {
    let custom = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 17,
        max_priority_fee_per_gas: 3,
    };
    let resolved = super::gas::resolve_public_action_gas_fee(
        1,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        custom,
        None,
    )
    .expect("custom WalletConnect fee does not need an automatic quote");
    assert_eq!(resolved.max_fee_per_gas, 17);
    assert_eq!(resolved.max_priority_fee_per_gas, 3);
    assert!(
        super::gas::resolve_public_action_gas_fee(
            1,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            PublicActionGasFeeSelection::Auto,
            None,
        )
        .is_err()
    );
}

#[tokio::test]
async fn public_shield_allowance_uses_shared_rpc_client_and_decodes_result() {
    let server = MockRpcServer::spawn(false, 50_000).await;
    let chain_route = RpcChainRoute::new(1, vec![Url::parse(&server.url).expect("mock RPC URL")]);
    let http = http_context_for_route(WalletNetworkMode::Tor, "tor");
    let token = address!("1111111111111111111111111111111111111111");
    let owner = address!("2222222222222222222222222222222222222222");
    let spender = address!("3333333333333333333333333333333333333333");

    let allowance = super::actions::query_erc20_allowance(
        &sensitive(&chain_route),
        &http,
        PublicAssetId::Erc20(token),
        owner,
        spender,
    )
    .await
    .expect("query allowance through broker");
    assert_eq!(allowance, U256::from(0x1234));

    let calls = server.calls();
    let allowance_calls = rpc_calls_for(&calls, "eth_call");
    assert_eq!(allowance_calls.len(), 1);
    let call = allowance_calls[0];
    assert_eq!(call.route.as_deref(), Some("tor"));
    let transaction: TransactionRequest =
        serde_json::from_value(call.params[0].clone()).expect("allowance transaction");
    assert_eq!(transaction.to, Some(TxKind::Call(token)));
    let calldata = transaction.input.input().expect("allowance calldata");
    let decoded = PublicErc20::allowanceCall::abi_decode(calldata).expect("decode allowance call");
    assert_eq!(decoded.owner, owner);
    assert_eq!(decoded.spender, spender);
}

#[tokio::test]
async fn public_shield_allowance_propagates_rpc_member_error() {
    let server = MockRpcServer::spawn(true, 50_000).await;
    let chain_route = RpcChainRoute::new(1, vec![Url::parse(&server.url).expect("mock RPC URL")]);
    let http = http_context_for_route(WalletNetworkMode::Tor, "tor");
    let error = super::actions::query_erc20_allowance(
        &sensitive(&chain_route),
        &http,
        PublicAssetId::Erc20(address!("1111111111111111111111111111111111111111")),
        address!("2222222222222222222222222222222222222222"),
        address!("3333333333333333333333333333333333333333"),
    )
    .await
    .expect_err("failed allowance read must not produce an approval decision");

    assert_eq!(error.to_string(), "query public shield ERC-20 allowance");
    assert!(matches!(
        error.downcast_ref::<crate::RpcBrokerError>(),
        Some(crate::RpcBrokerError::Remote(_))
    ));
    assert!(!rpc_calls_for(&server.calls(), "eth_call").is_empty());
}

#[tokio::test]
async fn walletconnect_rpc_privacy_and_submission_boundaries_use_one_http_context() {
    let server = MockRpcServer::spawn(false, 50_000).await;
    let chain = effective_chain_for_rpc(&server.url, 12_345);

    for (mode, route) in [
        (WalletNetworkMode::Tor, "tor"),
        (WalletNetworkMode::Proxy, "proxy"),
        (WalletNetworkMode::Direct, "direct"),
    ] {
        let http = http_context_for_route(mode, route);
        quote_public_action_gas_fee(1, &chain, &http)
            .await
            .expect("request-independent fee quote");
    }

    let quote_calls = server.calls();
    assert!(!quote_calls.is_empty());
    assert!(quote_calls.iter().all(|call| {
        matches!(
            call.method.as_str(),
            "eth_getBlockByNumber" | "eth_gasPrice" | "eth_maxPriorityFeePerGas" | "eth_feeHistory"
        ) && !contains_transaction_field(&call.params)
    }));
    for route in ["tor", "proxy", "direct"] {
        assert!(
            quote_calls
                .iter()
                .any(|call| call.route.as_deref() == Some(route))
        );
    }
    assert!(rpc_calls_for(&quote_calls, "eth_estimateGas").is_empty());

    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let custom_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    let http = http_context_for_route(WalletNetworkMode::Direct, "direct");
    let pool = crate::query_rpc_pool_for_route(
        &sensitive(&RpcChainRoute::new(
            1,
            vec![Url::parse(&server.url).expect("mock RPC URL")],
        )),
        &http,
    );
    let recognized_request = TransactionRequest::default()
        .with_to(to)
        .with_value(U256::from(1_u64));
    super::submission::public_action_preflight_from_rpc_pool(
        &pool,
        &http,
        &sensitive(&chain.rpc_route),
        1,
        from,
        recognized_request,
        custom_fee,
        &chain.gas,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        super::types::PublicActionGasLimitStrategy::ChainBuffer,
        None,
        None,
        Some(65_000 + chain.gas.gas_limit_buffer),
        None,
        false,
        None,
    )
    .await
    .expect("recognized operation preflight");
    assert!(rpc_calls_for(&server.calls(), "eth_estimateGas").is_empty());

    let unknown_request = TransactionRequest::default()
        .with_to(to)
        .with_value(U256::from(1_u64))
        .with_input(Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]));
    let before_submission_estimate = rpc_calls_for(&server.calls(), "eth_estimateGas").len();
    super::submission::public_action_preflight_from_rpc_pool(
        &pool,
        &http,
        &sensitive(&chain.rpc_route),
        1,
        from,
        unknown_request,
        PublicActionGasFeeSelection::Auto,
        &chain.gas,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        super::types::PublicActionGasLimitStrategy::ChainBuffer,
        None,
        None,
        None,
        None,
        false,
        None,
    )
    .await
    .expect("managed post-approval resolution");
    let after_submission_calls = server.calls();
    assert_eq!(
        rpc_calls_for(&after_submission_calls, "eth_estimateGas").len(),
        before_submission_estimate + 1
    );
    assert!(
        rpc_calls_for(&after_submission_calls, "eth_estimateGas")
            .last()
            .is_some_and(|call| contains_transaction_field(&call.params))
    );

    let simulation_server = MockRpcServer::spawn(false, 42_000).await;
    let simulation_chain = effective_chain_for_rpc(&simulation_server.url, 8_000);
    let simulation_http = http_context_for_route(WalletNetworkMode::Direct, "direct");
    let simulation_request = PublicAdvancedTransactionEstimateRequest {
        chain_id: 1,
        effective_chain: simulation_chain,
        from,
        intent: PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(7_u64),
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        },
        gas_fee: custom_fee,
        access_list: None,
    };
    let simulation_quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(80),
        suggested_max_fee_per_gas: 100,
        suggested_max_priority_fee_per_gas: 2,
    };
    let simulation_resolved = PublicActionResolvedGasFee {
        rpc_gas_price: 100,
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    assert!(rpc_calls_for(&simulation_server.calls(), "eth_estimateGas").is_empty());
    let simulation = simulate_public_advanced_transaction_with_fee(
        simulation_request,
        simulation_quote,
        simulation_resolved,
        &simulation_http,
        &simulation_http.operation_http_client().await.unwrap(),
    )
    .await
    .expect("explicit simulation after action");
    assert_eq!(simulation.raw_gas_limit, 42_000);
    assert_eq!(
        rpc_calls_for(&simulation_server.calls(), "eth_estimateGas").len(),
        1
    );

    for (mode, route) in [
        (WalletNetworkMode::Tor, "tor"),
        (WalletNetworkMode::Proxy, "proxy"),
    ] {
        let failed_server = MockRpcServer::spawn(true, 50_000).await;
        let failed_chain = effective_chain_for_rpc(&failed_server.url, 0);
        let failed_http = http_context_for_route(mode, route);
        let _ = quote_public_action_gas_fee(1, &failed_chain, &failed_http).await;
        let failed_calls = failed_server.calls();
        assert!(!failed_calls.is_empty());
        assert!(
            failed_calls
                .iter()
                .all(|call| call.route.as_deref() == Some(route))
        );
        assert!(
            !failed_calls
                .iter()
                .any(|call| call.route.as_deref() == Some("direct"))
        );
    }
}

#[tokio::test]
async fn advanced_simulation_uses_each_provider_once_and_selects_revert_plurality() {
    let revert_servers = [
        MockRpcServer::spawn_revert("Order has expired").await,
        MockRpcServer::spawn_revert("Order has expired").await,
        MockRpcServer::spawn_revert("Order has expired").await,
    ];
    let unavailable_server = MockRpcServer::spawn(true, 50_000).await;
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let mut chain = effective_chain_for_rpc(&revert_servers[0].url, 8_000);
    chain.rpc_route = RpcChainRoute::new(
        1,
        revert_servers
            .iter()
            .map(|server| Url::parse(&server.url).expect("RPC URL"))
            .chain(std::iter::once(
                Url::parse(&unavailable_server.url).expect("RPC URL"),
            ))
            .collect(),
    )
    .with_multicall(chain.rpc_route.multicall().expect("multicall"));
    let request = PublicAdvancedTransactionEstimateRequest {
        chain_id: 1,
        effective_chain: chain,
        from,
        intent: PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(7_u64),
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        },
        gas_fee: PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 2,
        },
        access_list: None,
    };
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(80),
        suggested_max_fee_per_gas: 100,
        suggested_max_priority_fee_per_gas: 2,
    };
    let resolved = PublicActionResolvedGasFee {
        rpc_gas_price: 100,
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };

    let http = HttpContext::direct_for_tests();
    let result = simulate_public_advanced_transaction_with_fee(
        request,
        quote,
        resolved,
        &http,
        &http.operation_http_client().await.unwrap(),
    )
    .await;
    assert!(matches!(
        result,
        Err(PublicAdvancedTransactionSimulationError::Reverted(reason))
            if reason == "Order has expired"
    ));
    for server in &revert_servers {
        assert_eq!(
            rpc_calls_for(&server.calls(), "eth_estimateGas").len(),
            1,
            "each configured provider should receive one estimate request"
        );
    }
    assert_eq!(
        rpc_calls_for(&unavailable_server.calls(), "eth_estimateGas").len(),
        1
    );
}

/// A chain-1 fee endpoint that reports `chain_id` to identity checks, estimates `estimate` gas
/// or fails the estimate, and logs each method it receives.
async fn logged_fee_endpoint(
    chain_id: u64,
    gas_price: u128,
    estimate: Option<u64>,
    gate: Option<crate::rpc_broker::tests::RpcMockGate>,
) -> (Url, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    let methods = Arc::new(Mutex::new(Vec::new()));
    let logged = methods.clone();
    let responder: crate::rpc_broker::tests::RpcResponder = Arc::new(move |request: Value| {
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        logged.lock().unwrap().push(method.clone());
        let result = match (method.as_str(), estimate) {
            ("eth_chainId", _) => json!(alloy::primitives::U64::from(chain_id)),
            ("eth_getBlockByNumber", _) => {
                let mut block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
                block.header.inner.base_fee_per_gas = Some(5);
                serde_json::to_value(block).unwrap()
            }
            ("eth_gasPrice", _) => json!(format!("0x{gas_price:x}")),
            ("eth_maxPriorityFeePerGas", _) => json!("0x2"),
            ("eth_feeHistory", _) => json!({
                "oldestBlock": "0x1",
                "baseFeePerGas": ["0x5", "0x5"],
                "gasUsedRatio": [0.5],
                "reward": [["0x1", "0x2", "0x3", "0x4"]]
            }),
            ("eth_estimateGas", Some(estimate)) => json!(format!("0x{estimate:x}")),
            _ => {
                return json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"code": -32603, "message": "unavailable"},
                });
            }
        };
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    });
    let (url, server) = crate::rpc_broker::tests::spawn_rpc_mock_with_gate(
        responder,
        Arc::default(),
        Arc::default(),
        gate,
    )
    .await;
    (url, server, methods)
}

/// Chain 1 on `endpoints`, each of which must pass an identity check before a fee read.
fn identity_checked_chain(endpoints: Vec<Url>) -> EffectiveChainConfig {
    let mut chain = effective_chain_for_rpc(endpoints[0].as_str(), 8_000);
    chain.rpc_route = RpcChainRoute::new(1, endpoints)
        .with_identity_verification()
        .with_multicall(chain.rpc_route.multicall().expect("multicall"));
    chain
}

fn raw_estimate_request(chain: EffectiveChainConfig) -> PublicAdvancedTransactionEstimateRequest {
    PublicAdvancedTransactionEstimateRequest {
        chain_id: 1,
        effective_chain: chain,
        from: Address::repeat_byte(1),
        intent: PublicTransactionIntent::Raw {
            to: Some(Address::repeat_byte(2)),
            value: U256::from(7_u64),
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        },
        gas_fee: PublicActionGasFeeSelection::Auto,
        access_list: None,
    }
}

#[tokio::test]
async fn gas_only_public_reads_never_reach_a_wrong_chain_endpoint() {
    // Sampled, the wrong-chain endpoint would set the upper-quartile gas price.
    let (wrong, wrong_server, wrong_methods) =
        logged_fee_endpoint(5, 1_000, Some(99_000), None).await;
    let (valid, valid_server, _) = logged_fee_endpoint(1, 100, Some(42_000), None).await;
    let chain = identity_checked_chain(vec![wrong, valid]);
    let http = HttpContext::direct_for_tests();

    let quote = quote_public_action_gas_fee(1, &chain, &http)
        .await
        .expect("quote from the valid endpoint");
    assert_eq!(quote.rpc_gas_price, 100);
    assert_eq!(*wrong_methods.lock().unwrap(), ["eth_chainId"]);

    // The estimate's quote and estimate stages share one source, so the endpoint is checked
    // once more rather than once per stage.
    let estimate = estimate_public_advanced_transaction(
        raw_estimate_request(chain),
        &http,
        &http.operation_http_client().await.unwrap(),
    )
    .await
    .expect("estimate from the valid endpoint");
    assert_eq!(estimate.raw_gas_limit, 42_000);
    assert_eq!(
        *wrong_methods.lock().unwrap(),
        ["eth_chainId", "eth_chainId"]
    );
    wrong_server.abort();
    valid_server.abort();
}

#[tokio::test]
async fn advanced_estimate_uses_an_endpoint_admitted_after_its_quote() {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    // The first endpoint quotes but cannot estimate; the second passes its identity check only
    // once the quote has finished and the first estimate has failed.
    let (quoting, quoting_server, quoting_methods) = logged_fee_endpoint(1, 100, None, None).await;
    let gate = crate::rpc_broker::tests::RpcMockGate::default();
    let (late, late_server, late_methods) =
        logged_fee_endpoint(1, 100, Some(42_000), Some(gate.clone())).await;
    let chain = identity_checked_chain(vec![quoting, late]);
    let http = HttpContext::direct_for_tests();
    let release_late_endpoint = async {
        tokio::time::timeout(WAIT, gate.request_started.notified())
            .await
            .expect("the late endpoint's identity is checked");
        while !quoting_methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_estimateGas")
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Releases the identity check, then the estimate that follows it.
        gate.release_response.notify_one();
        gate.request_started.notified().await;
        gate.release_response.notify_one();
    };
    let client = http.operation_http_client().await.unwrap();
    let (estimate, released) = tokio::join!(
        tokio::time::timeout(
            WAIT,
            estimate_public_advanced_transaction(raw_estimate_request(chain), &http, &client),
        ),
        tokio::time::timeout(WAIT, release_late_endpoint),
    );
    let estimate = estimate
        .expect("the estimate finishes")
        .expect("the late endpoint estimates");
    released.expect("the quote finishes and the first estimate fails before the release");
    assert_eq!(estimate.raw_gas_limit, 42_000);
    assert_eq!(
        *late_methods.lock().unwrap(),
        ["eth_chainId", "eth_estimateGas"]
    );
    quoting_server.abort();
    late_server.abort();
}

#[test]
fn railway_profile_uses_floor_multiplier_and_no_native_gas_model() {
    assert_eq!(railway_gas_limit(100_001), 120_001);

    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 1,
        current_base_fee_per_gas: Some(1),
        suggested_max_fee_per_gas: 1,
        suggested_max_priority_fee_per_gas: 0,
    };
    // A native Mimic shield's fees come only from its prepared estimate, never a fixed limit.
    assert!(
        estimate_public_action_gas_cost_with_profile(
            1,
            &default_effective_chain(1),
            PublicActionKind::Shield,
            PublicAssetId::Native,
            PublicShieldTransactionProfile::Railway,
            PublicActionGasFeeSelection::Auto,
            Some(quote),
        )
        .is_err()
    );

    let token = address!("0x3333333333333333333333333333333333333333");
    let erc20 = estimate_public_action_gas_cost_with_profile(
        1,
        &default_effective_chain(1),
        PublicActionKind::Shield,
        PublicAssetId::Erc20(token),
        PublicShieldTransactionProfile::Railway,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railway ERC-20 shield estimate");
    assert_eq!(
        erc20.expected_cost,
        U256::from(
            PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::Expected)
        )
    );
    assert_eq!(
        erc20.maximum_cost,
        U256::from(
            railway_gas_limit(PUBLIC_NATIVE_APPROVE_GAS_UNITS)
                + railway_gas_limit(public_shield_gas_units(
                    &ETHEREUM_GAS_MODEL,
                    GasEstimateMode::UpperBound
                ))
        )
    );
}

#[test]
fn railway_bnb_legacy_fee_resolution_is_erc20_only() {
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 7,
        current_base_fee_per_gas: Some(5),
        suggested_max_fee_per_gas: 12,
        suggested_max_priority_fee_per_gas: 2,
    };
    let token = PublicAssetId::Erc20(address!("0x3333333333333333333333333333333333333333"));
    let auto = super::gas::resolve_public_action_gas_fee(
        56,
        PublicShieldTransactionProfile::Railway,
        token,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railway BNB auto fee");
    assert_eq!(auto.max_fee_per_gas, 7);
    assert_eq!(auto.max_priority_fee_per_gas, 0);
    let native = super::gas::resolve_public_action_gas_fee(
        56,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Native,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railway BNB native type-4 fee");
    assert_eq!(native.max_fee_per_gas, 12);
    assert_eq!(native.max_priority_fee_per_gas, 2);
    // Observing a legacy chain never downgrades the native type-4 envelope.
    let legacy_observed = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Legacy),
        ..quote
    };
    assert!(!super::gas::public_action_uses_legacy_envelope(
        56,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Native,
        Some(legacy_observed),
    ));

    let railoxide = super::gas::resolve_public_action_gas_fee(
        56,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railoxide generic fee");
    assert_eq!(railoxide.max_fee_per_gas, 12);
    assert_eq!(railoxide.max_priority_fee_per_gas, 2);

    let custom = super::gas::resolve_public_action_gas_fee(
        56,
        PublicShieldTransactionProfile::Railway,
        token,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 11,
            max_priority_fee_per_gas: 3,
        },
        Some(quote),
    )
    .expect("Railway BNB custom fee");
    assert_eq!(custom.max_fee_per_gas, 11);
    assert_eq!(custom.max_priority_fee_per_gas, 0);

    let projection = estimate_public_action_gas_cost_with_profile(
        56,
        &default_effective_chain(56),
        PublicActionKind::Shield,
        token,
        PublicShieldTransactionProfile::Railway,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railway BNB gas projection");
    assert_eq!(projection.expected_fee_per_gas, 7);
    assert_eq!(projection.maximum_fee_per_gas, 7);
    let projection_with_ceiling = estimate_public_action_gas_cost_with_profile_and_ceiling(
        56,
        &default_effective_chain(56),
        PublicActionKind::Shield,
        token,
        PublicShieldTransactionProfile::Railway,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
        Some(PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 13,
            max_priority_fee_per_gas: 0,
        }),
    )
    .expect("Railway BNB gas projection with ceiling");
    assert_eq!(
        projection_with_ceiling.expected_cost,
        projection.expected_cost
    );
    assert_eq!(
        projection_with_ceiling.maximum_cost,
        projection.maximum_cost / U256::from(7) * U256::from(13)
    );
}

#[test]
fn railway_standard_quote_uses_lower_median_60th_rewards_and_110_percent_base() {
    let base_fees = [80, 90, 100, 101];
    let rewards = vec![
        vec![1, 40, 80, 95],
        vec![1, 10, 80, 95],
        vec![1, 30, 80, 95],
        vec![1, 20, 80, 95],
    ];
    let quote = railway_standard_gas_fee_quote(&base_fees, Some(rewards.as_slice()))
        .expect("Railway standard quote");
    assert_eq!(quote.suggested_max_priority_fee_per_gas, 20);
    assert_eq!(quote.suggested_max_fee_per_gas, 131);
    assert_eq!(quote.current_base_fee_per_gas, Some(100));
    let bundle = railway_standard_gas_fee_quote_bundle(&base_fees, Some(rewards.as_slice()), None)
        .expect("Railway standard and aggressive quote");
    assert_eq!(bundle.standard, quote);
    // BNB native caps the next base fee before the 110% multiplier: floor(50 * 1.1) + 20.
    let capped =
        railway_standard_gas_fee_quote_bundle(&base_fees, Some(rewards.as_slice()), Some(50))
            .expect("Railway BNB native quote")
            .standard;
    assert_eq!(capped.suggested_max_fee_per_gas, 75);
    assert_eq!(capped.suggested_max_priority_fee_per_gas, 20);
    // Unusable observations fail this provider rather than being repaired.
    for (base_fees, rewards) in [
        (&[][..], Some(rewards.as_slice())),
        (&base_fees[..], None),
        (&base_fees[..], Some(&[][..])),
        (&base_fees[..], Some(&[vec![1]][..])),
    ] {
        assert!(railway_standard_gas_fee_quote_bundle(base_fees, rewards, Some(50)).is_err());
    }
    assert_eq!(
        bundle.authorization_ceiling,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 236,
            max_priority_fee_per_gas: 95,
        }
    );
}

#[test]
fn railway_bnb_quote_caps_provider_gas_price_before_110_percent_floor() {
    assert_eq!(
        railway_bnb_gas_fee_quote(60_000_000).rpc_gas_price,
        55_000_000
    );
    assert_eq!(
        railway_bnb_gas_fee_quote(40_000_000).rpc_gas_price,
        44_000_000
    );
    assert_eq!(
        railway_bnb_gas_fee_quote_bundle(60_000_000).authorization_ceiling,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 70_000_000,
            max_priority_fee_per_gas: 0,
        }
    );
    assert_eq!(
        railway_bnb_gas_fee_quote_bundle(40_000_000).authorization_ceiling,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 56_000_000,
            max_priority_fee_per_gas: 0,
        }
    );
}

#[test]
fn railway_auto_fee_ceiling_allows_decreases_but_rejects_increases() {
    let ceiling = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 130,
        max_priority_fee_per_gas: 20,
    };
    assert!(railway_auto_fee_within_authorized_ceiling(
        1,
        PublicAssetId::Erc20(Address::ZERO),
        ceiling,
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 130,
            max_priority_fee_per_gas: 20,
        },
    ));
    assert!(railway_auto_fee_within_authorized_ceiling(
        1,
        PublicAssetId::Erc20(Address::ZERO),
        ceiling,
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 129,
            max_priority_fee_per_gas: 19,
        },
    ));
    assert!(!railway_auto_fee_within_authorized_ceiling(
        1,
        PublicAssetId::Erc20(Address::ZERO),
        ceiling,
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 131,
            max_priority_fee_per_gas: 20,
        },
    ));
    assert!(!railway_auto_fee_within_authorized_ceiling(
        1,
        PublicAssetId::Erc20(Address::ZERO),
        ceiling,
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 130,
            max_priority_fee_per_gas: 21,
        },
    ));
    assert!(railway_auto_fee_within_authorized_ceiling(
        56,
        PublicAssetId::Erc20(Address::ZERO),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 55,
            max_priority_fee_per_gas: 0,
        },
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 55,
            max_priority_fee_per_gas: 999,
        },
    ));
}

#[test]
fn railway_auto_approval_and_shield_steps_start_from_auto_independently() {
    let authorized_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 130,
        max_priority_fee_per_gas: 20,
    };
    assert_eq!(
        public_action_step_initial_gas_fee_selection(
            PublicShieldTransactionProfile::Railway,
            PublicActionStepFeePolicy::RefreshRailwayStandard,
            authorized_fee,
        ),
        PublicActionGasFeeSelection::Auto
    );
    assert_eq!(
        public_action_step_initial_gas_fee_selection(
            PublicShieldTransactionProfile::Railway,
            PublicActionStepFeePolicy::Captured,
            authorized_fee,
        ),
        authorized_fee
    );
    assert_eq!(
        public_action_step_initial_gas_fee_selection(
            PublicShieldTransactionProfile::Railoxide,
            PublicActionStepFeePolicy::RefreshRailwayStandard,
            authorized_fee,
        ),
        authorized_fee
    );
}

#[test]
fn public_shield_approval_follows_mode_and_allowance() {
    use PublicShieldApprovalMode::{Exact, UnlimitedWhenShort};

    let amount = U256::from(10_u64);
    for (mode, allowance, required, approval_amount) in [
        // An exact approval is signed for every shield, whatever the allowance.
        (Exact, U256::MAX, true, amount),
        (Exact, U256::from(9_u64), true, amount),
        (UnlimitedWhenShort, amount, false, U256::MAX),
        (UnlimitedWhenShort, U256::from(9_u64), true, U256::MAX),
    ] {
        assert_eq!(
            public_shield_approval_required(mode, allowance, amount),
            required,
            "{mode:?} with allowance {allowance}"
        );
        assert_eq!(public_shield_approval_amount(mode, amount), approval_amount);
    }
    // Mimic Railway locks Railway's policy; Railoxide keeps the chosen mode.
    for chosen in [Exact, UnlimitedWhenShort] {
        assert_eq!(
            PublicShieldTransactionProfile::Railway.approval_mode(chosen),
            UnlimitedWhenShort
        );
        assert_eq!(
            PublicShieldTransactionProfile::Railoxide.approval_mode(chosen),
            chosen
        );
    }
}

#[test]
fn public_shield_protocol_fee_amount_uses_floor_rounding() {
    assert_eq!(
        public_shield_protocol_fee_amount(U256::from(12_345_u64)),
        U256::from(30_u64)
    );
}

#[test]
fn effective_public_chain_config_uses_settings_overrides() {
    let mut effective = effective_chain_for_rpc("https://rpc.example", 42);
    effective.rpc_route = effective
        .rpc_route
        .with_multicall(address!("0x0000000000000000000000000000000000000003"));
    let private = effective.railgun.as_mut().unwrap();
    private.deployment.contract = address!("0x0000000000000000000000000000000000000001");
    private.deployment.relay_adapt_contract =
        Some(address!("0x0000000000000000000000000000000000000004"));
    effective.wrapped_native_token = Some(address!("0x0000000000000000000000000000000000000002"));

    let config = public_chain_runtime_config(1, &effective).expect("effective config");

    assert_eq!(config.rpc_route.endpoint_urls().len(), 1);
    assert_eq!(
        config.rpc_route.endpoint_urls()[0].as_str(),
        "https://rpc.example/"
    );
    assert_eq!(
        config.require_railgun().unwrap().contract,
        address!("0x0000000000000000000000000000000000000001")
    );
    assert_eq!(
        config.require_railgun().unwrap().relay_adapt_contract,
        Some(address!("0x0000000000000000000000000000000000000004"))
    );
    assert_eq!(
        config.wrapped_native_token,
        Some(address!("0x0000000000000000000000000000000000000002"))
    );
    assert_eq!(
        config.rpc_route.multicall(),
        Some(address!("0x0000000000000000000000000000000000000003"))
    );
    assert_eq!(config.gas.gas_limit_buffer, 42);
}

#[test]
fn walletconnect_effective_public_chain_config_rejects_disabled_chain() {
    let mut effective = effective_chain_for_rpc("https://rpc.example", 42);
    effective.enabled = false;

    let Err(error) = public_chain_runtime_config(1, &effective) else {
        panic!("disabled chain was accepted")
    };

    assert!(error.to_string().contains("disabled"));
}

#[test]
fn public_send_request_uses_native_value_or_erc20_transfer() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let token = address!("0x3333333333333333333333333333333333333333");
    let amount = U256::from(5_u64);

    let native = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount,
            recipient,
        },
    )
    .expect("native transfer request");
    assert_eq!(native.to, Some(recipient.into()));
    assert_eq!(native.value, Some(amount));

    let erc20 = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Erc20(token),
            amount,
            recipient,
        },
    )
    .expect("ERC20 transfer request");
    assert_eq!(erc20.to, Some(token.into()));
    let expected_transfer = PublicErc20::transferCall { recipient, amount }.abi_encode();
    assert_eq!(
        erc20.input.input().expect("transfer input").as_ref(),
        expected_transfer.as_slice()
    );
}

#[test]
fn public_send_request_supports_raw_calls_and_contract_creation() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let call_data = Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]);
    let call = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(7_u64),
            data: call_data.clone(),
        },
    )
    .expect("raw call request");
    assert_eq!(call.to, Some(to.into()));
    assert_eq!(call.value, Some(U256::from(7_u64)));
    assert_eq!(call.input.input(), Some(&call_data));

    let init_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00]);
    let creation = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: None,
            value: U256::ZERO,
            data: init_code.clone(),
        },
    )
    .expect("contract creation request");
    assert_eq!(creation.to, Some(TxKind::Create));
    assert_eq!(creation.value, Some(U256::ZERO));
    assert_eq!(creation.input.input(), Some(&init_code));

    let mut managed_creation =
        public_action_eip1559_transaction_request(creation, 1, from, 10, 2, 3);
    managed_creation.gas = Some(100_000);
    let built = managed_creation
        .build_consensus_tx()
        .expect("build managed contract creation transaction");
    let alloy::consensus::TypedTransaction::Eip1559(built) = built else {
        panic!("expected EIP-1559 contract creation")
    };
    assert_eq!(built.to, TxKind::Create);
    assert_eq!(built.value, U256::ZERO);
    assert_eq!(built.input, init_code);

    let value_call = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(1_u64),
            data: Bytes::new(),
        },
    )
    .expect("positive-value empty-data call");
    assert_eq!(value_call.to, Some(to.into()));
    assert_eq!(value_call.value, Some(U256::from(1_u64)));
}

#[test]
fn public_send_request_rejects_empty_raw_intents() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");

    let empty_creation = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: None,
            value: U256::from(1_u64),
            data: Bytes::new(),
        },
    )
    .expect_err("empty creation must fail");
    assert!(empty_creation.to_string().contains("non-empty init code"));

    let no_op = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::ZERO,
            data: Bytes::new(),
        },
    )
    .expect_err("empty zero-value call must fail");
    assert!(no_op.to_string().contains("native value or data"));
}

#[test]
fn advanced_public_transaction_authorization_is_payload_and_gas_bounded() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let intent = PublicTransactionIntent::Raw {
        to: Some(to),
        value: U256::from(7_u64),
        data: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]),
    };
    let fingerprint = public_advanced_transaction_payload_fingerprint(1, from, &intent, 10, 2);
    let authorization = PublicAdvancedTransactionAuthorization {
        payload_fingerprint: fingerprint,
        gas_limit: buffered_advanced_gas_limit(50_000, 12_345),
    };

    assert_eq!(authorization.gas_limit, 62_345);
    assert_eq!(
        public_send_authorized_gas_limit(
            1,
            from,
            &intent,
            Some(authorization),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 10,
                max_priority_fee_per_gas: 2,
            },
        )
        .expect("matching authorization"),
        Some(62_345)
    );
    assert!(ensure_advanced_gas_estimate_authorized(62_345, 62_345).is_ok());
    assert!(ensure_advanced_gas_estimate_authorized(62_346, 62_345).is_err());

    let changed_intent = PublicTransactionIntent::Raw {
        to: Some(to),
        value: U256::from(8_u64),
        data: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]),
    };
    let stale = public_send_authorized_gas_limit(
        1,
        from,
        &changed_intent,
        Some(authorization),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 10,
            max_priority_fee_per_gas: 2,
        },
    )
    .expect_err("changed payload must invalidate authorization");
    assert!(stale.to_string().contains("changed after gas estimation"));

    let authorized_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 10,
        max_priority_fee_per_gas: 2,
    };
    assert!(
        ensure_public_action_command_gas_fee_authorized(Some(authorized_fee), authorized_fee)
            .is_ok()
    );
    assert!(
        ensure_public_action_command_gas_fee_authorized(
            Some(authorized_fee),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 11,
                max_priority_fee_per_gas: 2,
            },
        )
        .is_err()
    );
}

#[test]
fn advanced_native_balance_exposure_includes_value_and_authorized_gas() {
    assert_eq!(
        public_action_native_exposure(U256::from(7_u64), 50_000, 10),
        U256::from(500_007_u64)
    );
}

#[test]
fn transaction_receipt_output_omits_absent_contract_address() {
    let transfer = crate::TxReceiptOutput {
        tx_hash: "0x01".to_string(),
        status: true,
        block_number: 1,
        gas_used: 21_000,
        contract_address: None,
    };
    let transfer_json = serde_json::to_value(&transfer).expect("serialize transfer receipt");
    assert!(transfer_json.get("contract_address").is_none());

    let creation = crate::TxReceiptOutput {
        contract_address: Some("0x2222222222222222222222222222222222222222".to_string()),
        ..transfer
    };
    let creation_json = serde_json::to_value(&creation).expect("serialize creation receipt");
    assert_eq!(
        creation_json["contract_address"],
        "0x2222222222222222222222222222222222222222"
    );
}

#[test]
fn public_action_eip1559_request_sets_fee_caps_and_nonce() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let base = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount: U256::from(5_u64),
            recipient,
        },
    )
    .expect("native transfer request");

    let tx = public_action_eip1559_transaction_request(base, 1, from, 42, 3, 9);

    assert_eq!(tx.chain_id, Some(1));
    assert_eq!(tx.from, Some(from));
    assert_eq!(tx.to, Some(recipient.into()));
    assert_eq!(tx.max_fee_per_gas, Some(42));
    assert_eq!(tx.max_priority_fee_per_gas, Some(3));
    assert_eq!(tx.nonce, Some(9));
}

#[test]
fn railway_profile_uses_legacy_envelope_only_on_bnb() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let base = public_send_transaction_request(
        56,
        from,
        &PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount: U256::from(5_u64),
            recipient,
        },
    )
    .expect("native transfer request");
    let legacy = public_action_legacy_transaction_request(base, 56, from, 42, 9);
    assert_eq!(legacy.gas_price, Some(42));
    assert_eq!(legacy.max_fee_per_gas, None);
    assert_eq!(legacy.max_priority_fee_per_gas, None);
    let token = PublicAssetId::Erc20(Address::ZERO);
    assert!(PublicShieldTransactionProfile::Railway.uses_legacy_envelope(56, token));
    assert!(
        !PublicShieldTransactionProfile::Railway.uses_legacy_envelope(56, PublicAssetId::Native)
    );
    for chain_id in [1, 137, 42161] {
        assert!(!PublicShieldTransactionProfile::Railway.uses_legacy_envelope(chain_id, token));
    }
}

#[test]
fn walletconnect_transaction_sanitizer_discards_dapp_envelope_and_preserves_access_list() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let access_list = AccessList::default();
    let request = TransactionRequest {
        from: Some(from),
        to: Some(recipient.into()),
        gas_price: Some(9),
        gas: Some(21_000),
        nonce: Some(4),
        max_fee_per_gas: Some(99),
        max_priority_fee_per_gas: Some(3),
        transaction_type: Some(1),
        access_list: Some(access_list.clone()),
        ..Default::default()
    };
    let sanitized = sanitize_walletconnect_transaction_request(request, 1, from);
    assert_eq!(sanitized.to, Some(recipient.into()));
    assert_eq!(sanitized.gas, None);
    assert_eq!(sanitized.gas_price, None);
    assert_eq!(sanitized.max_fee_per_gas, None);
    assert_eq!(sanitized.max_priority_fee_per_gas, None);
    assert_eq!(sanitized.nonce, None);
    assert_eq!(sanitized.transaction_type, None);
    assert_eq!(sanitized.access_list, Some(access_list));
}

#[test]
fn public_action_replacement_bump_reuses_self_broadcast_policy() {
    assert_eq!(public_action_replacement_bumped_fee(8), 9);
    assert_eq!(public_action_replacement_bumped_fee(9), 11);
}

#[test]
fn public_action_tip_fallback_uses_rpc_gas_price_only_for_bnb() {
    assert_eq!(
        public_action_tip_fallback(56),
        SelfBroadcastTipFallback::RpcGasPrice,
    );
    assert_eq!(
        public_action_tip_fallback(1),
        SelfBroadcastTipFallback::Minimum,
    );
}

#[tokio::test]
async fn shield_submission_checks_remaining_funding_before_estimation_and_signing() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .list_active_public_accounts_for_session(&view_session)
        .unwrap()
        .into_iter()
        .find(|account| account.source == PublicAccountSource::Derived)
        .unwrap();
    let native_balance = 1_000_000_000_000_000_000_000_u128;
    let token = PublicAssetId::Erc20(Address::repeat_byte(7));
    for (case, asset, profile) in [
        ("approval", token, PublicShieldTransactionProfile::Railoxide),
        ("approval", token, PublicShieldTransactionProfile::Railway),
        ("allowance", token, PublicShieldTransactionProfile::Railway),
        (
            "auto_ceiling",
            token,
            PublicShieldTransactionProfile::Railway,
        ),
        (
            "native",
            PublicAssetId::Native,
            PublicShieldTransactionProfile::Railoxide,
        ),
        ("retry", token, PublicShieldTransactionProfile::Railoxide),
        (
            "estimate_reserve",
            token,
            PublicShieldTransactionProfile::Railoxide,
        ),
    ] {
        let mut chain = default_effective_chain(42161);
        let (_, approval_gas, shield_gas) = super::gas::public_shield_estimated_gas_units(
            railgun_wallet::tx::RailgunGasModel::for_chain(42161),
            asset,
            profile,
            chain.gas.gas_limit_buffer,
        )
        .unwrap();
        let estimate_gas = approval_gas.saturating_add(shield_gas.saturating_mul(2));
        let estimate = if case == "estimate_reserve" {
            MockRpcEstimateResult::Success(estimate_gas)
        } else {
            MockRpcEstimateResult::Error {
                code: -32000,
                message: "estimation reached".to_owned(),
                data: None,
            }
        };
        let server = MockRpcServer::spawn_with_estimate_result(false, estimate).await;
        chain.rpc_route = RpcChainRoute::new(42161, vec![Url::parse(&server.url).unwrap()]);
        let max_fee = match case {
            "approval" => native_balance / u128::from(approval_gas),
            "allowance" => native_balance / u128::from(shield_gas + approval_gas / 2),
            "estimate_reserve" => {
                native_balance
                    / u128::from(estimate_gas + chain.gas.gas_limit_buffer + shield_gas / 2)
            }
            _ => 1,
        };
        let fee = |max_fee_per_gas| PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas: 1,
        };
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        if case == "retry" {
            command_tx
                .send(PublicActionCommand {
                    kind: PublicActionCommandKind::Retry,
                    gas_fee: fee(native_balance / u128::from(approval_gas)),
                })
                .unwrap();
        }
        drop(command_tx);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let http = HttpContext::direct_for_tests();
        let error = submit_public_shield(
            PublicShieldRequest {
                executor_owner: None,
                transaction_tracking: None,
                chain_id: 42161,
                effective_chain: chain,
                view_session: view_session.clone(),
                vault_store: store.clone(),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_owned()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid.clone(),
                asset,
                amount: match case {
                    "native" => U256::from(native_balance),
                    "allowance" => U256::from(1),
                    _ => U256::from(5_000),
                },
                profile,
                approval_mode: PublicShieldApprovalMode::Exact,
                native_preparation: None,
                native_operation: None,
                gas_fee: fee(max_fee),
                gas_fee_mode: if case == "auto_ceiling" {
                    PublicActionGasFeeMode::Auto
                } else {
                    PublicActionGasFeeMode::Custom
                },
                authorized_fee_ceiling: fee(if case == "auto_ceiling" {
                    native_balance / u128::from(shield_gas) + 1
                } else {
                    max_fee
                }),
                command_rx: Some(command_rx),
                event_tx: Some(event_tx),
            },
            &http,
            &http.operation_http_client().await.unwrap(),
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        if case == "allowance" {
            assert!(message.contains("estimation reached"), "{message}");
            assert!(!message.contains("insufficient native balance"));
        } else {
            assert!(
                message.contains("insufficient native balance"),
                "{case}: {message}"
            );
            if case != "estimate_reserve" {
                assert!(message.contains("estimated Shield fees"), "{message}");
            }
        }
        let calls = server.calls();
        assert_eq!(
            rpc_calls_for(&calls, "eth_estimateGas").len(),
            usize::from(matches!(case, "allowance" | "retry" | "estimate_reserve")),
            "{case}",
        );
        assert_eq!(
            rpc_calls_for(&calls, "eth_getBalance").len(),
            if case == "retry" { 2 } else { 1 },
            "{case}",
        );
        assert!(rpc_calls_for(&calls, "eth_sendRawTransaction").is_empty());
        let expected_step = match (asset, case) {
            // The wrap fallback's value is the whole balance, which leaves no gas to wrap it.
            (PublicAssetId::Native, _) => PublicActionProgressStep::Wrap,
            (_, "allowance") => PublicActionProgressStep::Shield,
            _ => PublicActionProgressStep::Approve,
        };
        let mut failures = 0;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                PublicActionSessionEvent::StepFailed { step, .. } => {
                    assert_eq!(step, expected_step);
                    failures += 1;
                }
                PublicActionSessionEvent::AttemptHandoff { .. }
                | PublicActionSessionEvent::AttemptSubmitted { .. } => {
                    panic!("{case}: an unaffordable Shield must not reach transaction signing");
                }
                _ => {}
            }
        }
        assert_eq!(failures, if case == "retry" { 2 } else { 1 });
    }
    drop(view_session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).unwrap();
}

#[tokio::test]
async fn shield_allowance_read_failure_waits_for_retry_and_adopts_its_fee() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .list_active_public_accounts_for_session(&view_session)
        .unwrap()
        .into_iter()
        .find(|account| account.source == PublicAccountSource::Derived)
        .unwrap();
    let server = MockRpcServer::spawn_with_estimate_result(
        false,
        MockRpcEstimateResult::Error {
            code: -32000,
            message: "estimation reached".to_owned(),
            data: None,
        },
    )
    .await;
    server.fail_eth_call.store(true, Ordering::SeqCst);
    let mut chain = default_effective_chain(42161);
    chain.rpc_route = RpcChainRoute::new(42161, vec![Url::parse(&server.url).unwrap()]);
    let fee = |max_fee_per_gas| PublicActionGasFeeSelection::Custom {
        max_fee_per_gas,
        max_priority_fee_per_gas: 1,
    };
    // The request's fee reaches Shield estimation; the retry's fee cannot be funded.
    let retry_fee = fee(1_000_000_000_000_000_000_000);
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let http = HttpContext::direct_for_tests();
    let client = http.operation_http_client().await.unwrap();
    let submit = submit_public_shield(
        PublicShieldRequest {
            executor_owner: None,
            transaction_tracking: None,
            chain_id: 42161,
            effective_chain: chain,
            view_session: view_session.clone(),
            vault_store: store.clone(),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new(TEST_PASSWORD.to_owned()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: account.public_account_uuid.clone(),
            asset: PublicAssetId::Erc20(Address::repeat_byte(7)),
            // Below the mock allowance, so no approval is needed after the retried read.
            amount: U256::from(1),
            profile: PublicShieldTransactionProfile::Railoxide,
            approval_mode: PublicShieldApprovalMode::UnlimitedWhenShort,
            native_preparation: None,
            native_operation: None,
            gas_fee: fee(1),
            gas_fee_mode: PublicActionGasFeeMode::Custom,
            authorized_fee_ceiling: fee(1),
            command_rx: Some(command_rx),
            event_tx: Some(event_tx),
        },
        &http,
        &client,
    );
    let retry = async {
        loop {
            match event_rx.recv().await.expect("allowance failure event") {
                PublicActionSessionEvent::StepFailed { step, .. } => {
                    assert_eq!(step, PublicActionProgressStep::Approve);
                    break;
                }
                PublicActionSessionEvent::AttemptHandoff { .. }
                | PublicActionSessionEvent::AttemptSubmitted { .. } => {
                    panic!("a failed allowance read must not reach transaction signing");
                }
                _ => {}
            }
        }
        server.fail_eth_call.store(false, Ordering::SeqCst);
        let failed_reads = rpc_calls_for(&server.calls(), "eth_call").len();
        command_tx
            .send(PublicActionCommand {
                kind: PublicActionCommandKind::Retry,
                gas_fee: retry_fee,
            })
            .unwrap();
        drop(command_tx);
        failed_reads
    };
    let (result, failed_reads) = tokio::join!(submit, retry);

    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("insufficient native balance"), "{message}");
    let calls = server.calls();
    assert!(failed_reads > 0);
    assert!(rpc_calls_for(&calls, "eth_call").len() > failed_reads);
    assert!(rpc_calls_for(&calls, "eth_estimateGas").is_empty());
    assert!(rpc_calls_for(&calls, "eth_sendRawTransaction").is_empty());
    let mut failed_steps = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let PublicActionSessionEvent::StepFailed { step, .. } = event {
            failed_steps.push(step);
        }
    }
    assert_eq!(failed_steps, [PublicActionProgressStep::Shield]);
    drop(view_session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).unwrap();
}

#[tokio::test]
async fn shield_speedup_funding_uses_the_previously_signed_gas_limit() {
    let server = MockRpcServer::spawn(false, 50_000).await;
    let mut chain = default_effective_chain(42161);
    chain.rpc_route = RpcChainRoute::new(42161, vec![Url::parse(&server.url).unwrap()]);
    let (_, _, model_gas) = super::gas::public_shield_estimated_gas_units(
        railgun_wallet::tx::RailgunGasModel::for_chain(42161),
        PublicAssetId::Native,
        PublicShieldTransactionProfile::Railoxide,
        chain.gas.gas_limit_buffer,
    )
    .unwrap();
    let signed_gas_limit = model_gas / 2;
    let native_balance = U256::from(1_000_000_000_000_000_000_000_u128);
    let value = native_balance - U256::from(model_gas) * U256::from(100);
    let http = HttpContext::direct_for_tests();
    let pool = crate::query_rpc_pool_for_route(&sensitive(&chain.rpc_route), &http);
    for (max_fee_per_gas, affordable) in [(125_u128, true), (300, false)] {
        let actual_cost = value + U256::from(signed_gas_limit) * U256::from(max_fee_per_gas);
        assert_eq!(actual_cost <= native_balance, affordable);
        assert!(value + U256::from(model_gas) * U256::from(max_fee_per_gas) > native_balance);
        let result = super::submission::public_action_preflight_from_rpc_pool(
            &pool,
            &http,
            &sensitive(&chain.rpc_route),
            42161,
            Address::repeat_byte(1),
            TransactionRequest::default()
                .with_to(Address::repeat_byte(2))
                .with_value(value),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas: 1,
            },
            &chain.gas,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            super::types::PublicActionGasLimitStrategy::ChainBuffer,
            None,
            Some(7),
            Some(signed_gas_limit),
            None,
            false,
            Some(super::submission::PublicShieldFundingBudget {
                gas_units: model_gas,
                remaining_shield_gas_units: 0,
                remaining_max_fee_per_gas: None,
            }),
        )
        .await;
        if affordable {
            assert!(
                result.is_ok(),
                "affordable replacement failed: {:?}",
                result.err()
            );
        } else {
            assert!(matches!(
                result,
                Err(super::submission::PublicActionPreflightError::InsufficientShieldFunding(_))
            ));
        }
    }
    let calls = server.calls();
    assert!(rpc_calls_for(&calls, "eth_estimateGas").is_empty());
    assert!(rpc_calls_for(&calls, "eth_getTransactionCount").is_empty());
}

#[tokio::test]
async fn native_preflight_filters_broker_withdrawals_and_tries_each_candidate_once() {
    let failing = MockRpcServer::spawn(true, 50_000).await;
    let other = MockRpcServer::spawn_with_estimate_result(
        false,
        MockRpcEstimateResult::Error {
            code: -32000,
            message: "estimate unavailable".to_owned(),
            data: None,
        },
    )
    .await;
    let route = RpcChainRoute::new(
        1,
        vec![
            Url::parse(&failing.url).unwrap(),
            Url::parse(&other.url).unwrap(),
        ],
    );
    let http = HttpContext::direct_for_tests();
    // The original transaction pool predates the broker's withdrawal and still contains both.
    let pool = crate::query_rpc_pool_for_route(&sensitive(&route), &http);
    let warmup = http.rpc_broker().warm_identities(vec![route.clone()], None);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if http
                .rpc_broker()
                .endpoint_candidates(&route, None)
                .await
                .unwrap()
                .len()
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed startup responsiveness check withdraws one endpoint");
    let failed_before = failing.calls().len();
    for warmed in [true, false] {
        let fresh_http = HttpContext::direct_for_tests();
        let http = if warmed { &http } else { &fresh_http };
        let before_other = other.calls();
        let error = super::submission::public_action_preflight_from_rpc_pool(
            &pool,
            http,
            &sensitive(&route),
            1,
            Address::repeat_byte(1),
            TransactionRequest::default().with_to(Address::repeat_byte(2)),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 2,
            },
            &default_effective_chain(1).gas,
            PublicShieldTransactionProfile::Railway,
            PublicAssetId::Native,
            super::types::PublicActionGasLimitStrategy::ChainBuffer,
            None,
            None,
            None,
            None,
            false,
            None,
        )
        .await
        .err()
        .expect("all eligible candidates fail");
        assert!(matches!(
            error,
            super::submission::PublicActionPreflightError::Other(_)
        ));
        let after_other = other.calls();
        assert_eq!(
            rpc_calls_for(&after_other, "eth_estimateGas").len()
                - rpc_calls_for(&before_other, "eth_estimateGas").len(),
            1,
        );
        if warmed {
            assert_eq!(
                failing.calls().len(),
                failed_before,
                "withdrawn endpoint was bypassed"
            );
        } else {
            assert_eq!(
                rpc_calls_for(&failing.calls(), "eth_getTransactionCount").len(),
                1
            );
        }
    }
    drop(warmup);
}

#[test]
fn public_actions_reject_zero_amount_and_public_only_shield_before_signing() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let http = runtime.block_on(async { HttpContext::direct_for_tests() });
    let client = runtime.block_on(http.operation_http_client()).unwrap();
    let recipient = address!("0x2222222222222222222222222222222222222222");

    let send_result = runtime.block_on(submit_public_send(
        PublicSendRequest {
            executor_owner: None,
            transaction_tracking: None,
            chain_id: 1,
            effective_chain: default_effective_chain(1),
            view_session: Arc::clone(&view_session),
            vault_store: Arc::clone(&store),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new(TEST_PASSWORD.to_string()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: "unused".to_string(),
            intent: PublicTransactionIntent::Transfer {
                asset: PublicAssetId::Native,
                amount: U256::ZERO,
                recipient,
            },
            advanced_authorization: None,
            gas_fee: PublicActionGasFeeSelection::Auto,
            command_rx: None,
            event_tx: None,
        },
        &http,
        &client,
    ));
    match send_result {
        Ok(_) => panic!("zero-value public send unexpectedly succeeded"),
        Err(error) => assert!(error.to_string().contains("amount is required")),
    }

    for public_only in [false, true] {
        let mut chain = default_effective_chain(1);
        if public_only {
            chain.railgun = None;
        }
        let shield_result = runtime.block_on(submit_public_shield(
            PublicShieldRequest {
                executor_owner: None,
                transaction_tracking: None,
                chain_id: 1,
                effective_chain: chain,
                view_session: view_session.clone(),
                vault_store: store.clone(),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_string()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: "unused".to_string(),
                asset: PublicAssetId::Native,
                amount: if public_only {
                    U256::from(1)
                } else {
                    U256::ZERO
                },
                profile: PublicShieldTransactionProfile::Railoxide,
                approval_mode: PublicShieldApprovalMode::Exact,
                native_preparation: None,
                native_operation: None,
                gas_fee: PublicActionGasFeeSelection::Auto,
                gas_fee_mode: PublicActionGasFeeMode::Auto,
                authorized_fee_ceiling: PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas: 1,
                    max_priority_fee_per_gas: 1,
                },
                command_rx: None,
                event_tx: None,
            },
            &http,
            &client,
        ));
        match shield_result {
            Ok(_) => panic!("zero-value public shield unexpectedly succeeded"),
            Err(error) => assert!(error.to_string().contains(if public_only {
                "Railgun"
            } else {
                "amount is required"
            })),
        }
    }

    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn vaulted_public_signer_resolves_private_self_broadcast_gas_payers() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let derived = store
        .list_active_public_accounts_for_session(&view_session)
        .expect("active accounts")
        .into_iter()
        .find(|account| account.source == PublicAccountSource::Derived)
        .expect("derived account");
    let derived_secret_key = format!("public-account-secret|{}", derived.public_account_uuid);
    assert!(
        db.get_desktop_wallet_vault_record(&derived_secret_key)
            .expect("load derived secret record")
            .is_none()
    );

    let derived_signer = vaulted_public_signer(
        &store,
        &view_session,
        Some(TEST_PASSWORD),
        &derived.public_account_uuid,
        None,
        None,
        None,
    )
    .expect("derived signer");
    assert_eq!(derived_signer.address(), derived.address);
    let Err(missing_password) = vaulted_public_signer(
        &store,
        &view_session,
        None,
        &derived.public_account_uuid,
        None,
        None,
        None,
    ) else {
        panic!("software public signer without password unexpectedly succeeded");
    };
    assert!(
        missing_password
            .to_string()
            .contains("vault password required for software public account signer")
    );

    let hardware_index = store
        .next_derived_public_account_index_for_session(&view_session)
        .expect("next hardware public index");
    let hardware_descriptor = HardwarePublicAccountDescriptor::for_wallet_public_index(
        HardwareDeviceKind::Ledger,
        view_session.derivation_index(),
        hardware_index,
    )
    .expect("hardware descriptor");
    let hardware_address = address!("0x2222222222222222222222222222222222222222");
    let confirmed =
        ConfirmedHardwarePublicAccount::new_for_tests(hardware_descriptor, hardware_address);
    assert!(matches!(
        store.add_hardware_public_account(&view_session, &confirmed, Some("Ledger Gas")),
        Err(VaultError::HardwareWalletViewRequiresDevice)
    ));

    let hardware_wallet_id = "hardware-public-action-wallet";
    let hardware_private_descriptor = HardwareDerivationDescriptor::ledger_eip1024_v1(
        parse_bip32_path("m/44'/60'/0'/0/0").expect("hardware path"),
        0,
        "ledger:evm:0x1111111111111111111111111111111111111111".to_string(),
        HardwareWalletSyncIntent::CreateNew,
    );
    let output = HardwareOperationOutput::new([42; 32]);
    let view_access_key =
        hardware_view_access_key_from_hardware_output(&hardware_private_descriptor, &output)
            .expect("hardware view key");
    let entropy = synthetic_entropy_from_hardware_output(&hardware_private_descriptor, output)
        .expect("hardware entropy");
    let hardware_metadata = store
        .new_hardware_wallet_metadata(
            TEST_PASSWORD,
            hardware_wallet_id,
            "Hardware public action",
            hardware_private_descriptor.clone(),
        )
        .expect("hardware wallet metadata");
    store
        .store_hardware_derived_wallet_from_entropy_with_metadata(
            TEST_PASSWORD,
            hardware_wallet_id,
            hardware_private_descriptor.account_index,
            entropy.expose_secret(),
            &hardware_metadata,
            &view_access_key,
        )
        .expect("store hardware wallet");
    let hardware_session = store
        .hardware_profile_session_for_fingerprint(
            TEST_PASSWORD,
            HardwareDeviceKind::Ledger,
            &hardware_private_descriptor.profile_fingerprint,
            None,
        )
        .expect("hardware profile session");
    let hardware_view_session = store
        .load_hardware_view_session(
            TEST_PASSWORD,
            &hardware_session,
            hardware_wallet_id,
            &view_access_key,
        )
        .expect("hardware view session");
    let hardware_public_descriptor = HardwarePublicAccountDescriptor::for_wallet_public_index(
        HardwareDeviceKind::Ledger,
        hardware_view_session.derivation_index(),
        0,
    )
    .expect("hardware public descriptor");
    #[cfg(not(feature = "hardware"))]
    assert!(
        hardware_session
            .typed_data_signing_mode(&hardware_public_descriptor)
            .is_none()
    );
    let hardware_public = store
        .add_hardware_public_account(
            &hardware_view_session,
            &ConfirmedHardwarePublicAccount::new_for_tests(
                hardware_public_descriptor,
                address!("0x3333333333333333333333333333333333333333"),
            ),
            Some("Hardware Ledger Gas"),
        )
        .expect("hardware public account under hardware view");
    let hardware_secret_key = format!(
        "public-account-secret|{}",
        hardware_public.public_account_uuid
    );
    assert!(
        db.get_desktop_wallet_vault_record(&hardware_secret_key)
            .expect("load hardware public secret record")
            .is_none()
    );
    let hardware_signer = vaulted_public_signer(
        &store,
        &hardware_view_session,
        None,
        &hardware_public.public_account_uuid,
        None,
        None,
        None,
    )
    .expect("hardware signer with profile session");
    assert_eq!(hardware_signer.address(), hardware_public.address);
    assert!(hardware_signer.requires_device_approval());
    #[cfg(not(feature = "hardware"))]
    {
        // The uncached capability is admitted, then the default-build probe reports unsupported.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let error = runtime.block_on(walletconnect_sign_typed_data_v4(WalletConnectTypedDataSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: None,
            vault_store: store.clone(),
            view_session: Arc::new(hardware_view_session),
            authorization: Some(crate::DesktopPrivateSpendAuthorization::from_software_credentials(Zeroizing::new(TEST_PASSWORD.into()), None)),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: hardware_public.public_account_uuid,
            typed_data: serde_json::json!({
                "types": {"EIP712Domain": [], "Message": [{"name": "text", "type": "string"}]},
                "primaryType": "Message", "domain": {}, "message": {"text": "hello"}
            }),
            hash_fallback_confirmed: false,
            event_tx: Some(events),
        }, crate::OperationHttpClient::direct_for_tests())).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<crate::walletconnect::WalletConnectError>(),
            Some(crate::walletconnect::WalletConnectError::UnsupportedMethod(method)) if method == "eth_signTypedData_v4")
        );
        assert!(
            received.try_recv().is_err(),
            "unsupported capability must stop before device signing"
        );
    }

    let imported = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("Imported"),
            false,
        )
        .expect("import public account");
    let imported_signer = vaulted_public_signer(
        &store,
        &view_session,
        Some(TEST_PASSWORD),
        &imported.public_account_uuid,
        None,
        None,
        None,
    )
    .expect("imported signer");
    assert_eq!(imported_signer.address(), imported.address);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn passphrase_walletconnect_signer_requires_the_active_protected_session() {
    let (root_dir, db, store, base_view_session) = public_action_request_parts();
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("context creation grant");
    let CreateSoftwareContextResult::Created {
        public_account,
        protected_seed_session,
        ..
    } = store
        .create_software_context(
            &base_view_session.clone_vault_view_unlock(),
            &mut grant,
            base_view_session.wallet_id(),
            "walletconnect-passphrase",
            0,
            "WalletConnect passphrase",
            Zeroizing::new("TREZOR".to_owned()),
            Zeroizing::new("TREZOR".to_owned()),
            SoftwareContextSyncIntent::RecoverExisting,
            &[SoftwareContextChainInput {
                chain_type: 0,
                chain_id: 1,
                contract: "0xcontract".to_owned(),
                deployment_block: 0,
                current_safe_head: None,
            }],
            VaultSessionId::from_bytes([31; 16]),
        )
        .expect("create passphrase context")
    else {
        panic!("expected created context");
    };
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, "walletconnect-passphrase")
            .expect("passphrase view session"),
    );
    let protected_seed_session = Arc::new(protected_seed_session);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let missing = runtime.block_on(walletconnect_sign_personal_message(
        WalletConnectPersonalSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: None,
            view_session: Arc::clone(&view_session),
            vault_store: Arc::clone(&store),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new(TEST_PASSWORD.to_owned()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: public_account.public_account_uuid.clone(),
            message: b"passphrase session".to_vec(),
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    ));
    assert!(missing.is_err());

    let wrong_session = {
        let seed = bip39_seed_from_mnemonic(TEST_MNEMONIC, "TREZOR").expect("context seed");
        let wrong_grant = store
            .create_spend_grant(TEST_PASSWORD)
            .expect("wrong-session grant");
        Arc::new(
            wrong_grant
                .spend_unlock()
                .expect("wrong-session spend unlock")
                .seal_software_seed_session(
                    SoftwareSeedSessionBinding::new(
                        base_view_session.wallet_id(),
                        "wrong-context",
                        VaultSessionId::from_bytes([32; 16]),
                    ),
                    seed.as_ref(),
                )
                .expect("wrong protected session"),
        )
    };
    let wrong = runtime.block_on(walletconnect_sign_personal_message(
        WalletConnectPersonalSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: None,
            view_session: Arc::clone(&view_session),
            vault_store: Arc::clone(&store),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new(TEST_PASSWORD.to_owned()),
                    Some(wrong_session),
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: public_account.public_account_uuid.clone(),
            message: b"passphrase session".to_vec(),
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    ));
    assert!(wrong.is_err());

    let signature = runtime
        .block_on(walletconnect_sign_personal_message(
            WalletConnectPersonalSignRequest {
                executor_owner: None,
                chain_id: 1,
                request_control: None,
                view_session,
                vault_store: store,
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_owned()),
                        Some(protected_seed_session),
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: public_account.public_account_uuid,
                message: b"passphrase session".to_vec(),
                event_tx: None,
            },
            crate::OperationHttpClient::direct_for_tests(),
        ))
        .expect("passphrase WalletConnect signature");
    assert!(signature.starts_with("0x"));

    drop(base_view_session);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn hardware_public_signer_consumes_trezor_app_passphrase_once() {
    let mut hardware_session = HardwareProfileSession::unmatched(
        HardwareDeviceKind::Trezor,
        HardwareProfileBinding::evm_address_fingerprint(
            "trezor:evm:0x1111111111111111111111111111111111111111",
        ),
        Some(vec![1, 2, 3]),
    );
    hardware_session.set_trezor_passphrase_mode(TrezorPassphraseMode::EnterInApp);
    let signer = HardwarePublicEvmSigner {
        request_control: None,
        address: address!("0x1111111111111111111111111111111111111111"),
        descriptor: HardwarePublicAccountDescriptor::for_wallet_public_index(
            HardwareDeviceKind::Trezor,
            0,
            0,
        )
        .expect("trezor descriptor"),
        hardware_session: std::sync::Mutex::new(hardware_session),
        trezor_app_passphrase: std::sync::Mutex::new(Some(Zeroizing::new("app secret".to_owned()))),
        trezor_pin_matrix_provider: None,
    };

    let passphrase = signer
        .take_trezor_app_passphrase()
        .expect("first passphrase take");
    assert_eq!(passphrase.as_str(), "app secret");
    assert!(signer.take_trezor_app_passphrase().is_none());
}

#[test]
fn hardware_public_signer_updates_in_memory_trezor_session_id_preserving_typed_data_mode() {
    let mut hardware_session = HardwareProfileSession::unmatched(
        HardwareDeviceKind::Trezor,
        HardwareProfileBinding::evm_address_fingerprint(
            "trezor:evm:0x1111111111111111111111111111111111111111",
        ),
        Some(vec![1, 2, 3]),
    );
    hardware_session.set_trezor_passphrase_mode(TrezorPassphraseMode::EnterInApp);
    let descriptor =
        HardwarePublicAccountDescriptor::for_wallet_public_index(HardwareDeviceKind::Trezor, 0, 0)
            .expect("trezor descriptor");
    hardware_session
        .cache_typed_data_signing_mode(&descriptor, HardwareTypedDataSigningMode::ClearSign)
        .expect("cache typed-data mode");
    let signer = HardwarePublicEvmSigner {
        request_control: None,
        address: address!("0x1111111111111111111111111111111111111111"),
        descriptor,
        hardware_session: std::sync::Mutex::new(hardware_session),
        trezor_app_passphrase: std::sync::Mutex::new(None),
        trezor_pin_matrix_provider: None,
    };

    signer
        .replace_trezor_session_id_if_trezor(Some(vec![4, 5, 6]))
        .expect("replace Trezor session id");
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .trezor_session_id,
        Some(vec![4, 5, 6])
    );
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .typed_data_signing_mode(&signer.descriptor),
        Some(HardwareTypedDataSigningMode::ClearSign)
    );
    signer
        .replace_trezor_session_id_if_trezor(None)
        .expect("clear Trezor session id");
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .trezor_session_id,
        None
    );
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .typed_data_signing_mode(&signer.descriptor),
        Some(HardwareTypedDataSigningMode::ClearSign)
    );
}

#[cfg(not(feature = "hardware"))]
#[test]
fn hardware_typed_data_probe_is_unsupported_without_hardware_feature() {
    let hardware_session = HardwareProfileSession::unmatched(
        HardwareDeviceKind::Ledger,
        HardwareProfileBinding::evm_address_fingerprint(
            "ledger:evm:0x1111111111111111111111111111111111111111",
        ),
        None,
    );
    let signer = HardwarePublicEvmSigner {
        request_control: None,
        address: address!("0x1111111111111111111111111111111111111111"),
        descriptor: HardwarePublicAccountDescriptor::for_wallet_public_index(
            HardwareDeviceKind::Ledger,
            0,
            0,
        )
        .expect("ledger descriptor"),
        hardware_session: std::sync::Mutex::new(hardware_session),
        trezor_app_passphrase: std::sync::Mutex::new(None),
        trezor_pin_matrix_provider: None,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let mode = runtime
        .block_on(signer.typed_data_signing_mode())
        .expect("default-build typed-data mode");

    assert_eq!(mode, HardwareTypedDataSigningMode::Unsupported);
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .typed_data_signing_mode(&signer.descriptor),
        Some(HardwareTypedDataSigningMode::Unsupported)
    );
}

#[tokio::test]
async fn admitted_dapp_reads_preserve_fee_and_simulation_policy_and_cover_preflight() {
    let server = MockRpcServer::spawn(false, 42_000).await;
    let chain = effective_chain_for_rpc(&server.url, 8_000);
    let http = http_context_for_route(WalletNetworkMode::Direct, "native");
    let admitted_http = http_context_for_route(WalletNetworkMode::Direct, "admitted");
    let callback_calls = Arc::new(AtomicU64::new(0));
    let recorded = callback_calls.clone();
    let reads = DappRpcReadClient::new(move |endpoint, read| {
        let http = admitted_http.clone();
        recorded.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let route = RpcChainRoute::new(1, vec![endpoint.into_exposed_url()]);
            let mut results = http
                .rpc_broker()
                .submit(RpcSubmission::new(
                    RpcRoute::from(sensitive(&route)),
                    vec![read],
                    crate::RpcOrigin::dapp("test-peer", "https://example.test").unwrap(),
                ))
                .await?;
            results.remove(0).map(crate::RpcResult::into_value)
        })
    });
    let native_quote = quote_public_action_gas_fee(1, &chain, &http).await.unwrap();
    let before = server.calls().len();
    let quote = quote_public_action_gas_fee_with_reads(1, &chain, &http, Some(&reads))
        .await
        .unwrap();
    assert_eq!(quote, native_quote);
    assert_eq!(callback_calls.load(Ordering::SeqCst), 4);
    assert!(
        server.calls()[before..]
            .iter()
            .all(|call| call.route.as_deref() == Some("admitted"))
    );

    let from = Address::repeat_byte(1);
    let to = Address::repeat_byte(2);
    let gas_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    let resolved = resolve_public_action_gas_fee(
        1,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        gas_fee,
        Some(quote),
    )
    .unwrap();
    let request = || PublicAdvancedTransactionEstimateRequest {
        chain_id: 1,
        effective_chain: chain.clone(),
        from,
        intent: PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(7),
            data: Bytes::from_static(&[1, 2, 3, 4]),
        },
        gas_fee,
        access_list: Some(AccessList::default()),
    };
    let native = simulate_public_advanced_transaction_with_fee(
        request(),
        quote,
        resolved,
        &http,
        &http.operation_http_client().await.unwrap(),
    )
    .await
    .unwrap();
    let before = server.calls().len();
    let admitted = simulate_public_advanced_transaction_with_fee_and_reads(
        request(),
        quote,
        resolved,
        &http,
        &http.operation_http_client().await.unwrap(),
        Some(&reads),
    )
    .await
    .unwrap();
    assert_eq!(admitted, native);
    let calls = server.calls();
    assert_eq!(calls.len() - before, 1);
    assert_eq!(calls[before].route.as_deref(), Some("admitted"));
    assert_eq!(calls[before].params[1], json!("pending"));

    let pool = crate::query_rpc_pool_for_route(&sensitive(&chain.rpc_route), &http);
    let before = server.calls().len();
    let before_callbacks = callback_calls.load(Ordering::SeqCst);
    super::submission::public_action_preflight_from_rpc_pool_with_reads(
        &pool,
        &http,
        &sensitive(&chain.rpc_route),
        1,
        from,
        TransactionRequest::default()
            .with_to(to)
            .with_value(U256::from(7)),
        PublicActionGasFeeSelection::Auto,
        &chain.gas,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        super::types::PublicActionGasLimitStrategy::ChainBuffer,
        Some(admitted.gas_limit),
        None,
        None,
        None,
        false,
        Some(&reads),
        None,
    )
    .await
    .expect("admitted post-approval preflight");
    assert_eq!(callback_calls.load(Ordering::SeqCst) - before_callbacks, 7);
    let calls = server.calls();
    let preflight = &calls[before..];
    assert_eq!(preflight.len(), 7);
    assert!(
        preflight
            .iter()
            .all(|call| call.route.as_deref() == Some("admitted"))
    );
    for method in [
        "eth_getBlockByNumber",
        "eth_gasPrice",
        "eth_maxPriorityFeePerGas",
        "eth_feeHistory",
        "eth_getTransactionCount",
        "eth_estimateGas",
        "eth_getBalance",
    ] {
        assert_eq!(rpc_calls_for(preflight, method).len(), 1);
    }
    let estimate = rpc_calls_for(preflight, "eth_estimateGas")[0];
    let tx: TransactionRequest = serde_json::from_value(estimate.params[0].clone()).unwrap();
    assert_eq!(tx.nonce, Some(7));
    assert_eq!(tx.from, Some(from));
    assert_eq!(tx.max_fee_per_gas, Some(quote.suggested_max_fee_per_gas));

    // A failed live-balance read must fail preflight even when the selected provider
    // would return enough funds over its native client.
    let fail_balance = DappRpcReadClient::new(move |endpoint, read| {
        let reads = reads.clone();
        Box::pin(async move {
            if read == RpcRead::get_balance(from) {
                Err(crate::RpcBrokerError::Transport)
            } else {
                reads.read(endpoint, read).await
            }
        })
    });
    let before_balance = rpc_calls_for(&server.calls(), "eth_getBalance").len();
    let error = super::submission::public_action_preflight_from_rpc_pool_with_reads(
        &pool,
        &http,
        &sensitive(&chain.rpc_route),
        1,
        from,
        TransactionRequest::default()
            .with_to(to)
            .with_value(U256::from(7)),
        gas_fee,
        &chain.gas,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        super::types::PublicActionGasLimitStrategy::ChainBuffer,
        None,
        None,
        None,
        None,
        false,
        Some(&fail_balance),
        None,
    )
    .await
    .err()
    .expect("live balance failure must stop preflight");
    assert!(
        matches!(error, super::submission::PublicActionPreflightError::Other(error)
        if error.downcast_ref::<crate::RpcBrokerError>() == Some(&crate::RpcBrokerError::Transport))
    );
    assert_eq!(
        rpc_calls_for(&server.calls(), "eth_getBalance").len(),
        before_balance
    );
}

#[tokio::test]
async fn admitted_dapp_read_rejections_stop_retries_without_provider_fallback() {
    use crate::RpcBrokerError;

    let server = MockRpcServer::spawn(false, 42_000).await;
    let other = MockRpcServer::spawn(false, 42_000).await;
    let mut chain = effective_chain_for_rpc(&server.url, 8_000);
    // Gateway reads verify identities in their own admission; a native probe would reach a
    // server.
    chain.rpc_route = RpcChainRoute::new(
        1,
        vec![
            Url::parse(&server.url).unwrap(),
            Url::parse(&other.url).unwrap(),
        ],
    )
    .with_identity_verification();
    let http = HttpContext::direct_for_tests();
    let pool = crate::query_rpc_pool_for_route(&sensitive(&chain.rpc_route), &http);
    let from = Address::repeat_byte(1);
    let to = Address::repeat_byte(2);
    let gas_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(80),
        suggested_max_fee_per_gas: 100,
        suggested_max_priority_fee_per_gas: 2,
    };
    let resolved = resolve_public_action_gas_fee(
        1,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        gas_fee,
        Some(quote),
    )
    .unwrap();
    for rejection in [
        RpcBrokerError::OriginRejected,
        RpcBrokerError::AdmissionRejected,
        RpcBrokerError::TimeoutBeforeDispatch,
        RpcBrokerError::Shutdown,
    ] {
        let count = Arc::new(AtomicU64::new(0));
        let recorded = count.clone();
        let error = rejection.clone();
        let reads = DappRpcReadClient::new(move |_, _| {
            recorded.fetch_add(1, Ordering::SeqCst);
            let error = error.clone();
            Box::pin(async move { Err(error) })
        });
        let error = quote_public_action_gas_fee_with_reads(1, &chain, &http, Some(&reads))
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<RpcBrokerError>(), Some(&rejection));
        count.store(0, Ordering::SeqCst);
        let error = simulate_public_advanced_transaction_with_fee_and_reads(
            PublicAdvancedTransactionEstimateRequest {
                chain_id: 1,
                effective_chain: chain.clone(),
                from,
                intent: PublicTransactionIntent::Raw {
                    to: Some(to),
                    value: U256::from(1_u8),
                    data: Bytes::new(),
                },
                gas_fee,
                access_list: None,
            },
            quote,
            resolved,
            &http,
            &http.operation_http_client().await.unwrap(),
            Some(&reads),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, PublicAdvancedTransactionSimulationError::RpcRead(error) if error == rejection)
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        count.store(0, Ordering::SeqCst);
        let error = super::submission::public_action_preflight_from_rpc_pool_with_reads(
            &pool,
            &http,
            &sensitive(&chain.rpc_route),
            1,
            from,
            TransactionRequest::default().with_to(to),
            gas_fee,
            &chain.gas,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            super::types::PublicActionGasLimitStrategy::ChainBuffer,
            None,
            None,
            None,
            None,
            false,
            Some(&reads),
            None,
        )
        .await
        .err()
        .expect("rejected preflight");
        assert!(
            matches!(error, super::submission::PublicActionPreflightError::Other(error)
            if error.downcast_ref::<RpcBrokerError>() == Some(&rejection))
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    for rejection in [
        RpcBrokerError::Remote(crate::RpcRemoteError::from(serde_json::from_value::<alloy::serde::WithOtherFields<alloy::rpc::json_rpc::ErrorPayload<serde_json::Value>>>(serde_json::json!({
            "code": -32042, "message": "nonce rejected", "data": {"nested": [null, 5]}, "extension": true
        })).unwrap())),
        RpcBrokerError::InnerRevert(crate::RpcRevert::from_multicall(Bytes::from_static(&[1, 2, 3]))),
    ] {
        let expected = rejection.clone();
        let reads = DappRpcReadClient::new(move |_, read| {
            match read.method() {
                "eth_getBlockByNumber" => {
                    let mut block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
                    block.header.inner.base_fee_per_gas = Some(5);
                    return Box::pin(async move { Ok(serde_json::to_value(block).unwrap()) });
                }
                "eth_gasPrice" | "eth_maxPriorityFeePerGas" => return Box::pin(async { Ok(json!("0x64")) }),
                "eth_feeHistory" => return Box::pin(async { Err(RpcBrokerError::InvalidResponse) }),
                _ => {}
            }
            assert_eq!(read, RpcRead::from_method_params("eth_getTransactionCount", serde_json::json!([from, "latest"]), 1).unwrap());
            let error = rejection.clone();
            Box::pin(async move { Err(error) })
        });
        let error = super::submission::public_action_preflight_from_rpc_pool_with_reads(
            &pool, &http, &sensitive(&chain.rpc_route), 1, from, TransactionRequest::default().with_to(to),
            gas_fee, &chain.gas, PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            super::types::PublicActionGasLimitStrategy::ChainBuffer,
            None, None, None,
            None, false, Some(&reads),
            None,
        ).await.err().expect("remote nonce failure");
        assert!(matches!(error, super::submission::PublicActionPreflightError::Other(error)
            if error.downcast_ref::<RpcBrokerError>() == Some(&expected)));
    }
    assert!(server.calls().is_empty());
    assert!(other.calls().is_empty());
}

#[tokio::test]
async fn invalidated_dapp_signing_requests_stop_before_spend_authorization() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let control = crate::dapp_request::DappRequestControl::new(
        tokio::time::Instant::now() + std::time::Duration::from_mins(1),
        || Ok(()),
    );
    control.invalidate(&crate::RpcBrokerError::OriginRejected);
    // Invalid vault credentials distinguish request rejection from signer construction.
    let personal = walletconnect_sign_personal_message(
        WalletConnectPersonalSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: Some(control.clone()),
            view_session: view_session.clone(),
            vault_store: store.clone(),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new("wrong password".into()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: "missing".into(),
            message: b"hello".to_vec(),
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    )
    .await
    .unwrap_err();
    let typed = walletconnect_sign_typed_data_v4(
        WalletConnectTypedDataSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: Some(control),
            view_session: view_session.clone(),
            vault_store: store.clone(),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new("wrong password".into()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: "missing".into(),
            typed_data: Value::Null,
            hash_fallback_confirmed: false,
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    )
    .await
    .unwrap_err();
    for error in [personal, typed] {
        assert_eq!(
            error.downcast_ref::<crate::RpcBrokerError>(),
            Some(&crate::RpcBrokerError::OriginRejected)
        );
    }
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[tokio::test]
async fn dapp_send_invalidation_stops_before_baseline_or_raw_broadcast() {
    #[derive(Clone, Copy)]
    enum InvalidateAt {
        Entry,
        Preflight,
        Signed,
    }

    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("Dapp send signer"),
            false,
        )
        .expect("import public account");
    for phase in [
        InvalidateAt::Entry,
        InvalidateAt::Preflight,
        InvalidateAt::Signed,
    ] {
        let server = MockRpcServer::spawn(false, 42_000).await;
        let chain = effective_chain_for_rpc(&server.url, 8_000);
        let http = HttpContext::direct_for_tests();
        let control = crate::dapp_request::DappRequestControl::new(
            tokio::time::Instant::now() + std::time::Duration::from_mins(1),
            || Ok(()),
        );
        if matches!(phase, InvalidateAt::Entry) {
            control.invalidate(&crate::RpcBrokerError::OriginRejected);
        }
        let read_control = control.clone();
        let read_http = http.clone();
        let reads = DappRpcReadClient::new(move |endpoint, read| {
            let http = read_http.clone();
            let control = read_control.clone();
            Box::pin(async move {
                let route = RpcChainRoute::new(1, vec![endpoint.into_exposed_url()]);
                let mut results = http
                    .rpc_broker()
                    .submit(RpcSubmission::new(
                        RpcRoute::from(sensitive(&route)),
                        vec![read],
                        crate::RpcOrigin::dapp("test-peer", "https://example.test").unwrap(),
                    ))
                    .await?;
                let result = results.remove(0).map(crate::RpcResult::into_value);
                if matches!(phase, InvalidateAt::Preflight) {
                    control.invalidate(&crate::RpcBrokerError::OriginRejected);
                }
                result
            })
        });
        let (tracker, tracking) = super::transaction_tracker::test_tracking_context();
        let changes = tracker.subscribe();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let invalidation = if matches!(phase, InvalidateAt::Signed) {
            let control = control.clone();
            Some(tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    if matches!(event, PublicActionSessionEvent::AttemptHandoff { .. }) {
                        // The software signer runs before the broadcast checkpoint yields.
                        control.invalidate(&crate::RpcBrokerError::OriginRejected);
                        return;
                    }
                }
                panic!("send must reach signing before invalidation");
            }))
        } else {
            None
        };
        let error = submit_walletconnect_send_transaction(
            WalletConnectSendTransactionRequest {
                executor_owner: None,
                request_control: Some(control.clone()),
                rpc_reads: Some(reads),
                transaction_tracking: Some(tracking),
                chain_id: 1,
                effective_chain: chain,
                view_session: view_session.clone(),
                vault_store: store.clone(),
                authorization: Some(crate::DesktopPrivateSpendAuthorization::VaultPassword(
                    Zeroizing::new(
                        if matches!(phase, InvalidateAt::Entry) {
                            "wrong password"
                        } else {
                            TEST_PASSWORD
                        }
                        .into(),
                    ),
                )),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid.clone(),
                tx_req: TransactionRequest::default()
                    .with_to(Address::repeat_byte(2))
                    .with_value(U256::from(7)),
                decoded_transaction: None,
                reviewed_transaction: None,
                reviewed_fee: None,
                gas_fee: PublicActionGasFeeSelection::Auto,
                expiry_timestamp: None,
                event_tx: Some(event_tx),
            },
            &http,
            &http.operation_http_client().await.unwrap(),
        )
        .await
        .unwrap_err();
        if let Some(invalidation) = invalidation {
            invalidation.await.expect("invalidation task");
        }
        assert_eq!(
            error.downcast_ref::<crate::RpcBrokerError>(),
            Some(&crate::RpcBrokerError::OriginRejected)
        );
        assert!(!control.handed_off());
        assert!(
            !changes.has_changed().expect("tracker remains open"),
            "no pending transaction was registered"
        );
        let calls = server.calls();
        assert!(rpc_calls_for(&calls, "eth_sendRawTransaction").is_empty());
        match phase {
            InvalidateAt::Entry => assert!(calls.is_empty()),
            InvalidateAt::Preflight => {
                assert!(!rpc_calls_for(&calls, "eth_getBalance").is_empty());
                assert!(rpc_calls_for(&calls, "eth_blockNumber").is_empty());
            }
            InvalidateAt::Signed => assert_eq!(rpc_calls_for(&calls, "eth_blockNumber").len(), 1),
        }
        tracker.shutdown().await;
    }
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

/// Captures tracing output for assertions on what was logged.
#[derive(Clone)]
pub(super) struct LogWriter(pub(super) Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn native_preflight_logs_failure_stage_without_sensitive_payloads() {
    use tracing::instrument::WithSubscriber;

    let from = Address::repeat_byte(0xab);
    let to = Address::repeat_byte(0xcd);
    let data = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe]);
    let value = U256::from(12_345_678_901_234_567_890_123_u128);
    let remote_message = "private-remote-error-message";
    let remote_data = "private-remote-error-data";
    for remote_failure in [true, false] {
        let estimate = if remote_failure {
            MockRpcEstimateResult::Error {
                code: -32000,
                message: remote_message.to_owned(),
                data: Some(remote_data.to_owned()),
            }
        } else {
            MockRpcEstimateResult::Success(50_000)
        };
        let server = MockRpcServer::spawn_with_estimate_result(false, estimate).await;
        let mut endpoint = Url::parse(&server.url).unwrap();
        endpoint.set_username("private-rpc-user").unwrap();
        endpoint.set_password(Some("private-rpc-password")).unwrap();
        endpoint.set_path("/private-rpc-path");
        endpoint.set_query(Some("key=private-rpc-key"));
        let mut chain = default_effective_chain(42161);
        let mut second_endpoint = endpoint.clone();
        second_endpoint.set_path("/private-rpc-path/second");
        chain.rpc_route = RpcChainRoute::new(42161, vec![endpoint, second_endpoint]);
        let http = HttpContext::direct_for_tests_with_candidate_order(<[usize]>::reverse);
        let pool = crate::query_rpc_pool_for_route(&sensitive(&chain.rpc_route), &http);
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let error = super::submission::public_action_preflight_from_rpc_pool(
            &pool,
            &http,
            &sensitive(&chain.rpc_route),
            42161,
            from,
            TransactionRequest::default()
                .with_to(to)
                .with_value(value)
                .with_input(data.clone()),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 2,
            },
            &chain.gas,
            PublicShieldTransactionProfile::Railway,
            PublicAssetId::Native,
            super::types::PublicActionGasLimitStrategy::ChainBuffer,
            None,
            None,
            None,
            None,
            false,
            None,
        )
        .with_subscriber(subscriber)
        .await
        .err()
        .expect("preflight fails");
        let super::submission::PublicActionPreflightError::Other(error) = error else {
            panic!("failure must retain the original error");
        };
        if remote_failure {
            let error = error
                .downcast_ref::<alloy::transports::TransportError>()
                .expect("original transport error remains available");
            let payload = error.as_error_resp().expect("original RPC error");
            assert_eq!(payload.code, -32000);
            assert_eq!(payload.message, remote_message);
        } else {
            assert!(
                error
                    .to_string()
                    .contains("all public action query RPC attempts failed")
            );
            assert!(format!("{error:#}").contains("insufficient native balance"));
        }
        let calls = server.calls();
        assert_eq!(rpc_calls_for(&calls, "eth_getTransactionCount").len(), 2);
        assert_eq!(rpc_calls_for(&calls, "eth_estimateGas").len(), 2);
        assert_eq!(
            rpc_calls_for(&calls, "eth_getBalance").len(),
            2 * usize::from(!remote_failure)
        );
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        let failures = logs
            .lines()
            .filter(|line| line.contains("public action preflight failed"))
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2);
        assert!(failures[0].contains("rpc_index=1"));
        assert!(failures[1].contains("rpc_index=0"));
        assert!(failures.iter().all(|line| line.contains(&format!(
            "endpoint={}",
            crate::http::redact_url_for_display(&Url::parse(&server.url).unwrap())
        ))));
        for field in ["chain_id=42161", "attempt=1", "attempts=2", "elapsed_ms="] {
            assert!(logs.contains(field), "missing diagnostic field {field}");
        }
        if remote_failure {
            assert!(logs.contains("stage=\"estimate\""));
            assert!(logs.contains("failure=\"rpc_error\""));
            assert!(logs.contains("rpc_code=-32000"));
        } else {
            assert!(logs.contains("stage=\"native_balance_validation\""));
            assert!(logs.contains("failure=\"validation\""));
        }
        for secret in [
            remote_message.to_owned(),
            remote_data.to_owned(),
            "private-rpc-user".to_owned(),
            "private-rpc-password".to_owned(),
            "private-rpc-path".to_owned(),
            "private-rpc-key".to_owned(),
            alloy::hex::encode_prefixed(from),
            alloy::hex::encode_prefixed(to),
            alloy::hex::encode_prefixed(&data),
            value.to_string(),
            "1000000000000000000000".to_owned(),
        ] {
            assert!(
                !logs.contains(&secret),
                "sensitive data appeared in diagnostics"
            );
        }
    }
}

#[tokio::test]
async fn dapp_raw_broadcast_does_not_log_or_return_remote_payloads() {
    use tracing::instrument::WithSubscriber;

    let server = MockRpcServer::spawn(true, 21_000).await;
    let http = HttpContext::direct_for_tests();
    let pool = crate::query_rpc_pool_for_route(
        &sensitive(&RpcChainRoute::new(
            1,
            vec![Url::parse(&server.url).unwrap()],
        )),
        &http,
    );
    let tx_hash = B256::repeat_byte(0xab);
    for log_transaction_details in [true, false] {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let error = crate::self_broadcast_send_raw_transaction_to_rpc_pool_with_logging(
            &pool,
            WalletNetworkMode::Direct,
            vec![1, 2, 3],
            tx_hash,
            log_transaction_details,
        )
        .with_subscriber(subscriber)
        .await
        .err()
        .expect("RPC rejects broadcast");
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        if log_transaction_details {
            assert!(logs.contains("mock route failure"));
            assert!(logs.contains(&alloy::hex::encode_prefixed(tx_hash)));
            assert!(error.to_string().contains("mock route failure"));
        } else {
            assert!(!logs.contains("mock route failure"));
            assert!(!logs.contains(&alloy::hex::encode_prefixed(tx_hash)));
            assert!(!error.to_string().contains("mock route failure"));
        }
    }
}

#[tokio::test]
async fn ens_recipient_uses_normalized_alloy_calls_on_the_configured_desktop_route() {
    use alloy::ens::{
        EnsResolver, UNIVERSAL_RESOLVER_ADDRESS, UniversalResolver, dns_encode, namehash,
    };
    let target = Address::repeat_byte(9);
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request| {
            assert_eq!(request["method"], "eth_call");
            let transaction: TransactionRequest =
                serde_json::from_value(request["params"][0].clone()).unwrap();
            assert_eq!(transaction.to, Some(UNIVERSAL_RESOLVER_ADDRESS.into()));
            let call =
                UniversalResolver::resolveCall::abi_decode(transaction.input.input().unwrap())
                    .unwrap();
            let assigned = call.name == dns_encode("raffy.eth");
            let name = if assigned {
                "raffy.eth"
            } else {
                "unassigned.eth"
            };
            assert_eq!(call.name, Bytes::from(dns_encode(name)));
            assert_eq!(
                EnsResolver::addrCall::abi_decode(&call.data).unwrap().node,
                namehash(name)
            );
            let address = if assigned { target } else { Address::ZERO };
            let result = UniversalResolver::resolveCall::abi_encode_returns(
                &UniversalResolver::resolveReturn {
                    _0: EnsResolver::addrCall::abi_encode_returns(&address).into(),
                    _1: Address::repeat_byte(8),
                },
            );
            json!({"jsonrpc":"2.0", "id":request["id"], "result":Bytes::from(result)})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let mut chain = effective_chain_for_rpc(endpoint.as_str(), 0);
    chain.rpc_route = RpcChainRoute::new(1, vec![endpoint]);
    let http = HttpContext::direct_for_tests();
    let client = http.operation_http_client().await.unwrap();
    assert_eq!(
        resolve_public_ens_recipient("RaFFY.eth", &chain, &http, &client)
            .await
            .unwrap(),
        target
    );
    assert!(
        resolve_public_ens_recipient("Unassigned.eth", &chain, &http, &client)
            .await
            .is_err()
    );
    assert!(
        resolve_public_ens_recipient("a\u{200d}b.eth", &chain, &http, &client)
            .await
            .is_err()
    );
    server.abort();
}

/// The SDK vector address in `vault/tests/key_export.rs`.
const TEST_SHIELD_RECIPIENT: &str = "0zk1qykzjxctynyz4z43pukckpv43jyzhyvy0ehrd5wuc54l5enqf9qfrrv7j6fe3z53la7enqphqvxys9aqyp9xx0km95ehqslx8apmu8l7anc7emau4tvsultrkvd";

fn test_shield_recipient() -> broadcaster_core::crypto::railgun::AddressData {
    broadcaster_core::crypto::railgun::AddressData::try_from(
        &broadcaster_core::crypto::railgun::Address::from(TEST_SHIELD_RECIPIENT),
    )
    .unwrap()
}

/// A native Mimic Railway preparation with a fixed raw estimate.
fn test_native_preparation(
    chain_id: u64,
    payer: Address,
    recipient: broadcaster_core::crypto::railgun::AddressData,
    amount: U256,
    raw_gas_limit: u64,
) -> NativeShieldPreparation {
    let chain = default_effective_chain(chain_id);
    let contracts =
        stealth_native_shield_contracts(chain_id, &chain, NativeShieldPayer::Software).unwrap();
    NativeShieldPreparation::new(
        contracts,
        PublicShieldTransactionProfile::Railway,
        payer,
        recipient,
        amount,
        raw_gas_limit,
        chain.gas.gas_limit_buffer,
    )
}

#[test]
fn native_shield_eligibility_explains_unavailable_routes() {
    use NativeShieldPayer::{Hardware, HardwareWallet, Software};
    use PublicShieldTransactionProfile::{Railoxide, Railway};

    for chain_id in [1, 56, 137, 42161] {
        let chain = default_effective_chain(chain_id);
        let contracts = stealth_native_shield_contracts(chain_id, &chain, Software).unwrap();
        assert_eq!(
            contracts.delegate,
            broadcaster_core::deployment::RailgunDeployment::for_chain(chain_id)
                .unwrap()
                .relay_adapt_7702_contract
        );
        assert_eq!(Some(contracts.wrapped_native), chain.wrapped_native_token);
    }
    assert_eq!(
        stealth_native_shield_contracts(10, &default_effective_chain(1), Software),
        Err(NativeShieldUnavailable::UnsupportedChain)
    );

    // The route follows the payer, never the profile; only Railoxide has a fallback.
    let chain = default_effective_chain(1);
    let stealth =
        NativeShieldRoute::Stealth(stealth_native_shield_contracts(1, &chain, Software).unwrap());
    let fallback = NativeShieldRoute::WrapFallback {
        wrapped_native: chain.wrapped_native_token.unwrap(),
    };
    let mut no_wrapped_native = default_effective_chain(1);
    no_wrapped_native.wrapped_native_token = None;
    let base_fee = Some(EvmFeeModel::Eip1559);
    let no_base_fee = Some(EvmFeeModel::Legacy);
    for (chain, payer, profile, fee_model, route) in [
        // A software payer, or a stealth payer of a software wallet.
        (&chain, Software, Railoxide, base_fee, Ok(stealth)),
        (&chain, Software, Railway, base_fee, Ok(stealth)),
        // A stealth payer of a Ledger or Trezor wallet.
        (&chain, HardwareWallet, Railoxide, base_fee, Ok(fallback)),
        (
            &chain,
            HardwareWallet,
            Railway,
            base_fee,
            Err(NativeShieldUnavailable::HardwareWallet),
        ),
        // A hardware-native payer.
        (&chain, Hardware, Railoxide, base_fee, Ok(fallback)),
        (
            &chain,
            Hardware,
            Railway,
            base_fee,
            Err(NativeShieldUnavailable::HardwarePayer),
        ),
        // No type-4 envelope without a base fee.
        (&chain, Software, Railoxide, no_base_fee, Ok(fallback)),
        (
            &chain,
            Software,
            Railway,
            no_base_fee,
            Err(NativeShieldUnavailable::NoBaseFee),
        ),
        (
            &no_wrapped_native,
            Hardware,
            Railoxide,
            base_fee,
            Err(NativeShieldUnavailable::NoWrappedNative),
        ),
    ] {
        assert_eq!(
            select_native_shield_route(1, chain, payer, profile, fee_model),
            route,
            "{payer:?} in {profile:?} with {fee_model:?}"
        );
    }
}

#[test]
fn native_preparation_binds_its_inputs_chain_and_profile() {
    use PublicShieldTransactionProfile::{Railoxide, Railway};

    let payer = Address::repeat_byte(0x11);
    let recipient = test_shield_recipient();
    let amount = U256::from(1_000_000_000_000_000_000_u128);
    let preparation = test_native_preparation(56, payer, recipient, amount, 100_000);

    // A changed chain, profile, payer, amount or recipient needs a new preparation.
    let contracts = stealth_native_shield_contracts(
        56,
        &default_effective_chain(56),
        NativeShieldPayer::Software,
    )
    .unwrap();
    preparation
        .ensure_matches(&contracts, Railway, payer, &recipient, amount)
        .unwrap();
    let ethereum = stealth_native_shield_contracts(
        1,
        &default_effective_chain(1),
        NativeShieldPayer::Software,
    )
    .unwrap();
    let mut other_recipient = recipient;
    other_recipient.master_public_key += U256::from(1);
    for (contracts, profile, payer, recipient, amount) in [
        (&ethereum, Railway, payer, &recipient, amount),
        (&contracts, Railoxide, payer, &recipient, amount),
        (
            &contracts,
            Railway,
            Address::repeat_byte(0x12),
            &recipient,
            amount,
        ),
        (
            &contracts,
            Railway,
            payer,
            &recipient,
            amount + U256::from(1),
        ),
        (&contracts, Railway, payer, &other_recipient, amount),
    ] {
        assert!(
            preparation
                .ensure_matches(contracts, profile, payer, recipient, amount)
                .is_err()
        );
    }
}

#[tokio::test]
async fn native_shield_signs_the_prepared_type_4_envelope() {
    use alloy::consensus::TxEnvelope;
    use alloy::eips::Decodable2718;

    let signer =
        VaultedPublicSigner::Software(SoftwareEvmSigner::from_private_key([7; 32]).unwrap());
    let payer = signer.address();
    let amount = U256::from(5_000_u64);
    let preparation = test_native_preparation(1, payer, test_shield_recipient(), amount, 100_000);
    let contracts = stealth_native_shield_contracts(
        1,
        &default_effective_chain(1),
        NativeShieldPayer::Software,
    )
    .unwrap();
    // The shape of the stealth account's issued transaction.
    let account = alloy::signers::local::PrivateKeySigner::random();
    let prepared = super::native_shield::native_shield_transaction_request(
        &contracts,
        payer,
        account.address(),
        amount,
        Bytes::from_static(&[1, 2, 3]),
        super::native_shield::native_shield_authorization(&account, &contracts, 0).unwrap(),
    );
    let input = prepared.input.input().cloned().unwrap();
    let authorization = prepared.authorization_list.clone().unwrap();
    let approved = public_action_eip1559_transaction_request(prepared, 1, payer, 100, 2, 9)
        .with_gas_limit(preparation.gas_limit());
    let signed = signer
        .sign_transaction_request(approved, "test")
        .await
        .unwrap();
    let TxEnvelope::Eip7702(signed) = TxEnvelope::decode_2718(&mut signed.as_slice()).unwrap()
    else {
        panic!("a type-4 transaction");
    };
    let tx = signed.tx();
    assert_eq!(
        (tx.chain_id, tx.to, tx.value, &tx.input, tx.nonce),
        (1, account.address(), amount, &input, 9)
    );
    assert_eq!(tx.authorization_list, authorization);
    assert_eq!(
        (tx.gas_limit, tx.max_fee_per_gas),
        (preparation.gas_limit(), 100)
    );
}

#[tokio::test]
async fn native_max_refits_the_amount_to_each_new_estimate() {
    let payer = Address::repeat_byte(0x11);
    let recipient = test_shield_recipient();
    let balance = U256::from(10_000_000_u64);
    // The second estimate rises, so its first fitted amount no longer leaves enough gas.
    let mut raw_estimates = [100_000_u64, 110_000, 110_000].into_iter();
    let preparation = super::native_shield::fit_native_shield_max(balance, 10, |amount| {
        std::future::ready(Ok(test_native_preparation(
            1,
            payer,
            recipient,
            amount,
            raw_estimates.next().unwrap(),
        )))
    })
    .await
    .unwrap();
    assert_eq!(preparation.gas_limit(), 132_000);
    assert_eq!(preparation.amount(), U256::from(10_000_000_u64 - 1_320_000));
    assert!(preparation.amount() + U256::from(preparation.gas_limit()) * U256::from(10) <= balance);

    assert!(
        super::native_shield::fit_native_shield_max(U256::from(1_200_000), 10, |amount| {
            std::future::ready(Ok(test_native_preparation(
                1, payer, recipient, amount, 100_000,
            )))
        })
        .await
        .is_err(),
        "no amount remains after the maximum gas cost"
    );
}

#[tokio::test]
async fn native_preparation_estimates_the_full_request() {
    let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let logged = calls.clone();
    let responder: crate::rpc_broker::tests::RpcResponder = Arc::new(move |request: Value| {
        logged.lock().unwrap().push(request.clone());
        let result = match request["method"].as_str().unwrap_or_default() {
            "eth_estimateGas" => json!("0x186a0"),
            // The executor's `nonce()` through the delegation, and the request at its estimate.
            "eth_call" => json!(B256::ZERO),
            // eth_chainId and anything else.
            _ => json!("0x1"),
        };
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    });
    let (url, server) =
        crate::rpc_broker::tests::spawn_rpc_mock(responder, Arc::default(), Arc::default()).await;
    let mut chain = default_effective_chain(1);
    chain.rpc_route = RpcChainRoute::new(1, vec![url]);
    let http = HttpContext::direct_for_tests();
    let payer = Address::repeat_byte(0x11);
    let amount = U256::from(5_000_u64);

    let preparation = prepare_native_shield(
        1,
        &chain,
        PublicShieldTransactionProfile::Railway,
        NativeShieldPayer::Software,
        payer,
        test_shield_recipient(),
        amount,
        &http,
        &http.operation_http_client().await.unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        (preparation.raw_gas_limit(), preparation.gas_limit()),
        (100_000, 120_000)
    );
    let calls = calls.lock().unwrap().clone();
    let estimate = calls
        .iter()
        .find(|call| call["method"] == "eth_estimateGas")
        .unwrap();
    let estimate: TransactionRequest =
        serde_json::from_value(estimate["params"][0].clone()).unwrap();
    assert_eq!(estimate.from, Some(payer));
    assert_eq!(estimate.value, Some(amount));
    assert_eq!(estimate.transaction_type, Some(4));
    // The stand-in account delegates itself and is the call's target.
    let [authorization] = estimate.authorization_list.as_deref().unwrap() else {
        panic!("one delegation");
    };
    assert_eq!(*authorization.address(), preparation.delegate());
    assert_eq!(
        estimate.to,
        Some(TxKind::Call(authorization.recover_authority().unwrap()))
    );
    server.abort();
}

#[tokio::test]
async fn native_quote_route_follows_the_profile_and_asset() {
    let (url, server, methods) = logged_fee_endpoint(56, 60_000_000, None, None).await;
    let mut chain = default_effective_chain(56);
    chain.rpc_route = RpcChainRoute::new(56, vec![url]);
    let http = HttpContext::direct_for_tests();
    let native = quote_public_action_gas_fee_bundle_with_profile(
        56,
        &chain,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Native,
        &http,
    )
    .await
    .unwrap();
    // Fee history: floor(5 * 110 / 100) plus the 60th-percentile tip 2.
    assert_eq!(native.standard.suggested_max_fee_per_gas, 7);
    assert_eq!(native.standard.suggested_max_priority_fee_per_gas, 2);
    assert!(
        !methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_gasPrice")
    );
    let erc20 = quote_public_action_gas_fee_bundle_with_profile(
        56,
        &chain,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Erc20(Address::repeat_byte(7)),
        &http,
    )
    .await
    .unwrap();
    assert_eq!(erc20.standard.rpc_gas_price, 55_000_000);
    assert_eq!(erc20.standard.suggested_max_priority_fee_per_gas, 0);
    // Railoxide's native shield keeps the ordinary quote, which observes the fee model.
    let railoxide = quote_public_action_gas_fee_bundle_with_profile(
        56,
        &chain,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        &http,
    )
    .await
    .unwrap();
    assert_eq!(
        railoxide.standard.observed_fee_model,
        Some(EvmFeeModel::Eip1559)
    );
    assert!(
        methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_getBlockByNumber")
    );
    server.abort();
}

#[test]
fn prepared_native_shield_needs_a_base_fee() {
    use super::types::PublicActionGasLimitStrategy;
    use PublicShieldTransactionProfile::{Railoxide, Railway};

    let quote = |observed_fee_model| PublicActionGasFeeQuote {
        observed_fee_model: Some(observed_fee_model),
        rpc_gas_price: 1,
        current_base_fee_per_gas: None,
        suggested_max_fee_per_gas: 1,
        suggested_max_priority_fee_per_gas: 0,
    };
    for (profile, quote, refused) in [
        (Railoxide, Some(quote(EvmFeeModel::Legacy)), true),
        (Railoxide, Some(quote(EvmFeeModel::Eip1559)), false),
        (Railway, None, false),
    ] {
        let legacy_envelope =
            public_action_uses_legacy_envelope(1, profile, PublicAssetId::Native, quote);
        let prepared = PublicActionGasLimitStrategy::NativeShieldPrepared {
            reviewed_gas_limit: 100_000,
            profile,
        };
        assert_eq!(
            super::submission::ensure_native_shield_envelope(prepared, legacy_envelope).is_err(),
            refused,
            "{profile:?}"
        );
    }
    // Other actions follow the observed fee model.
    assert!(
        super::submission::ensure_native_shield_envelope(
            PublicActionGasLimitStrategy::ChainBuffer,
            true
        )
        .is_ok()
    );
}

#[test]
fn native_shield_is_delivered_only_with_its_shield_event() {
    use alloy::sol_types::SolEvent;
    use broadcaster_core::contracts::railgun::Shield;

    use super::native_shield::ensure_native_shield_delivered;

    let contracts = stealth_native_shield_contracts(
        1,
        &default_effective_chain(1),
        NativeShieldPayer::Software,
    )
    .unwrap();
    let shield_log = |address: Address| alloy::rpc::types::Log {
        inner: alloy::primitives::Log {
            address,
            data: Shield {
                treeNumber: U256::ZERO,
                startPosition: U256::ZERO,
                commitments: Vec::new(),
                shieldCiphertext: Vec::new(),
                fees: Vec::new(),
            }
            .encode_log_data(),
        },
        ..Default::default()
    };
    let stealth_account = Address::repeat_byte(3);
    for (logs, delivered) in [
        (Ok(vec![shield_log(contracts.railgun)]), true),
        // Without Railgun's event the value is at the stealth account.
        (Ok(vec![shield_log(Address::repeat_byte(2))]), false),
        (Err(eyre!("logs unavailable")), false),
    ] {
        let settled = ensure_native_shield_delivered(&contracts, stealth_account, "0x01", logs);
        assert_eq!(settled.is_ok(), delivered);
        if let Err(error) = settled {
            // The failure names the account to recover.
            assert!(format!("{error:#}").contains(&stealth_account.to_string()));
        }
    }
}

#[test]
fn native_shield_signing_gas_limit_follows_the_profile_buffer() {
    use PublicShieldTransactionProfile::{Railoxide, Railway};

    const BUFFER: u64 = 30_000;
    // One prepared raw estimate of 1_000_000: Mimic's 120%, or Railoxide's chain buffer.
    for (profile, reviewed, below) in [
        (Railway, 1_200_000, 1_080_000),
        (Railoxide, 1_030_000, 930_000),
    ] {
        assert_eq!(
            super::gas::native_shield_gas_limit(profile, 1_000_000, BUFFER),
            reviewed
        );
        for (raw, signed) in [
            (900_000, Some(below)),
            // The final estimate is slightly above the prepared one.
            (1_000_012, Some(reviewed)),
            (reviewed, Some(reviewed)),
            (reviewed + 1, None),
        ] {
            assert_eq!(
                super::submission::native_shield_signing_gas_limit(profile, BUFFER, raw, reviewed)
                    .ok(),
                signed,
                "{profile:?} raw estimate {raw}"
            );
        }
    }
}
