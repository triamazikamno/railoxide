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

use super::contracts::PublicWrappedNative;
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

mod balances;
mod dapp_requests;
mod fees;
mod hardware_signer;
mod native_shielding;
mod public_actions;
mod shield;
mod walletconnect;

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
