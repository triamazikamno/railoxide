use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use alloy::primitives::{Address, Bytes, U256, address, b256};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::near_intents::{SentQuote, parse_quote_key, verify_quote};
use super::*;
use crate::http::WalletNetworkMode;
use crate::settings::{WalletSettings, build_effective_chain_configs};

const ACROSS_ROUTES: &str = include_str!("fixtures/across_routes_arb_pol.json");
const ACROSS_SUGGESTED_FEES: &str = include_str!("fixtures/across_suggested_fees.json");
const ACROSS_DEPOSIT_FILLED: &str = include_str!("fixtures/across_deposit_filled.json");
const ONE_CLICK_DRY_QUOTE: &str = include_str!("fixtures/oneclick_dry_quote.json");
const ONE_CLICK_QUOTE: &str = include_str!("fixtures/oneclick_quote.json");
const ONE_CLICK_STATUS_PENDING: &str = include_str!("fixtures/oneclick_status_pending.json");

/// The recipient, refund address and deposit address of the captured 1Click quotes.
const FIXTURE_RECIPIENT: Address = address!("513f28158bf181fb17ac6c349d21c3b763f8a508");
const FIXTURE_REFUND_TO: Address = address!("7ff784b916e6cffe371643a286e186c6d4f3a39f");
const FIXTURE_DEPOSIT_ADDRESS: Address = address!("A423c697bF8e8BFC42ac0f03737A78B898141f33");
/// `2026-09-29T23:47:20.000Z`, the deadline the captured quotes were requested with.
const FIXTURE_DEADLINE_SECS: u64 = 1_790_725_640;
const EXECUTOR: Address = Address::repeat_byte(0xee);

#[derive(Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    body: Value,
}

type Recorded = Arc<Mutex<Vec<RecordedRequest>>>;

/// Answer every request with `respond(request)` and record what was sent.
async fn spawn_mock(
    respond: impl Fn(&RecordedRequest) -> (u16, String) + Send + Sync + 'static,
) -> (Url, Recorded) {
    let respond = Arc::new(respond);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind bridge mock");
    let address = listener.local_addr().expect("bridge mock address");
    let recorded = Recorded::default();
    let sink = Arc::clone(&recorded);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let sink = Arc::clone(&sink);
            let respond = Arc::clone(&respond);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                let (method, path, body_start, content_length) = loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let mut headers = [httparse::EMPTY_HEADER; 32];
                    let mut parsed = httparse::Request::new(&mut headers);
                    if let Ok(httparse::Status::Complete(body_start)) = parsed.parse(&request) {
                        let content_length = parsed
                            .headers
                            .iter()
                            .find(|header| header.name.eq_ignore_ascii_case("content-length"))
                            .and_then(|header| std::str::from_utf8(header.value).ok())
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(0);
                        break (
                            parsed.method.unwrap_or_default().to_owned(),
                            parsed.path.unwrap_or_default().to_owned(),
                            body_start,
                            content_length,
                        );
                    }
                };
                while request.len() < body_start + content_length {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let body = &request[body_start..body_start + content_length];
                let recorded = RecordedRequest {
                    method,
                    path,
                    body: serde_json::from_slice(body).unwrap_or(Value::Null),
                };
                let (status, response) = respond(&recorded);
                sink.lock().expect("record request").push(recorded);
                let reply = format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    let base_url = Url::parse(&format!("http://{address}/api")).expect("bridge mock URL");
    (base_url, recorded)
}

async fn spawn_fixed(status: u16, response: &str) -> (Url, Recorded) {
    let response = response.to_owned();
    spawn_mock(move |_| (status, response.clone())).await
}

fn test_http() -> OperationHttpClient {
    OperationHttpClient::for_tests(
        reqwest::Client::new(),
        OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
    )
}

fn across(base_url: Url) -> AcrossClient {
    AcrossClient::new(test_http(), base_url).expect("Across client")
}

fn only_request(recorded: &Recorded) -> (String, Url, Value) {
    let recorded = recorded.lock().expect("recorded requests");
    let [request] = recorded.as_slice() else {
        panic!("expected one request, got {recorded:?}");
    };
    let url = Url::parse(&format!("http://mock{}", request.path)).expect("request path");
    (request.method.clone(), url, request.body.clone())
}

fn query_keys(url: &Url) -> Vec<String> {
    let mut keys = url
        .query_pairs()
        .map(|(key, _)| key.into_owned())
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

fn pinned_quote_key() -> VerifyingKey {
    let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
    let profile = chains.get(42161).unwrap().bridge_profile().unwrap();
    parse_quote_key(profile.one_click_quote_key()).expect("pinned 1Click key")
}

fn test_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn key_string(key: &VerifyingKey) -> String {
    format!("ed25519:{}", bs58::encode(key.to_bytes()).into_string())
}

/// The captured non-dry quote with one `quote` field replaced.
fn edited_quote(field: &str, value: &str) -> Value {
    let mut response: Value = serde_json::from_str(ONE_CLICK_QUOTE).unwrap();
    response["quote"][field] = Value::from(value);
    response
}

/// The terms the captured 1Click quotes were requested with.
fn fixture_params() -> OneClickQuoteParams {
    OneClickQuoteParams {
        origin_asset: "nep141:arb-0xaf88d065e77c8cc2239327c5edb3a432268e5831.omft.near".to_owned(),
        destination_asset: "nep245:v2_1.omni.hot.tg:137_11111111111111111111".to_owned(),
        amount: U256::from(20_000_000_u64),
        slippage_bps: 100,
        deadline: SystemTime::UNIX_EPOCH + Duration::from_secs(FIXTURE_DEADLINE_SECS),
    }
}

#[tokio::test]
async fn across_routes_skip_native_and_other_chain_entries() {
    let (base_url, recorded) = spawn_fixed(200, ACROSS_ROUTES).await;
    let client = across(base_url);

    let routes = client.available_routes(42161, 137).await.unwrap();
    // The fixture's sixth entry is the `isNative` ETH form of the WETH route.
    assert_eq!(routes.len(), 5);
    assert!(routes.iter().all(|route| route.origin_symbol != "ETH"));
    assert!(routes.contains(&AcrossRoute {
        origin_token: address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1"),
        destination_token: address!("7ceB23fD6bC0adD59E62ac25578270cFf1b9f619"),
        origin_symbol: "WETH".to_owned(),
        destination_symbol: "WETH".to_owned(),
    }));
    let (method, url, _) = only_request(&recorded);
    assert_eq!(method, "GET");
    assert_eq!(
        format!("{}?{}", url.path(), url.query().unwrap()),
        "/api/available-routes?originChainId=42161&destinationChainId=137"
    );

    assert_eq!(client.available_routes(42161, 10).await, Ok(Vec::new()));
}

#[tokio::test]
async fn across_fee_quotes_send_only_tokens_chains_and_amount() {
    let (base_url, recorded) = spawn_fixed(200, ACROSS_SUGGESTED_FEES).await;
    let request = AcrossFeeRequest {
        input_token: address!("af88d065e77c8cC2239327C5EDb3A432268e5831"),
        output_token: address!("3c499c542cEF5E3811e1192ce70d8cC03d5c3359"),
        origin_chain: 42161,
        destination_chain: 137,
        amount: U256::from(10_000_000_u64),
    };

    let quote = across(base_url).suggested_fees(&request).await.unwrap();

    assert_eq!(
        quote,
        AcrossFeeQuote {
            output_amount: U256::from(9_995_714_u64),
            total_relay_fee_total: U256::from(4_286_u64),
            total_relay_fee_pct: U256::from(428_600_000_000_000_u64),
            relayer_gas_fee_total: U256::from(3_286_u64),
            relayer_gas_fee_pct: U256::from(328_600_000_000_000_u64),
            lp_fee_total: U256::ZERO,
            timestamp: 1_790_718_359,
            fill_deadline: 1_790_725_559,
            exclusive_relayer: address!("FD03AbCAdaF3F930fA4E37Eb2f6ea3A44a41b7F0"),
            exclusivity_deadline: 3,
            spoke_pool: address!("e35e9842fceaCA96570B734083f4a58e8F7C5f2A"),
            destination_spoke_pool: address!("9295ee1d8C5b022Be115A2AD3c30C72E34e7F096"),
            is_amount_too_low: false,
            min_deposit: U256::from(500_071_u64),
            max_deposit: U256::from(44_901_649_816_u64),
            estimated_fill_time_sec: 1,
        }
    );
    let (method, url, _) = only_request(&recorded);
    assert_eq!(
        (method.as_str(), url.path()),
        ("GET", "/api/suggested-fees")
    );
    assert_eq!(
        query_keys(&url),
        [
            "allowUnmatchedDecimals",
            "amount",
            "destinationChainId",
            "inputToken",
            "originChainId",
            "outputToken"
        ]
    );
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "amount" && value == "10000000")
    );
    // 9,995,714 / (1 - 0.0004286) is the 10,000,000 input, and 0.03286% of it is the fee the
    // quote states in the input token, which has the output token's decimals here.
    assert_eq!(
        quote.relayer_gas_fee_in_output(),
        Some(U256::from(3_286_u64))
    );
}

#[tokio::test]
async fn across_message_quotes_add_the_recipient_and_message() {
    let (base_url, recorded) = spawn_fixed(200, ACROSS_SUGGESTED_FEES).await;
    let handler = address!("9295ee1d8C5b022Be115A2AD3c30C72E34e7F096");
    let request = AcrossMessageFeeRequest {
        fee: AcrossFeeRequest {
            input_token: address!("af88d065e77c8cC2239327C5EDb3A432268e5831"),
            output_token: address!("3c499c542cEF5E3811e1192ce70d8cC03d5c3359"),
            origin_chain: 42161,
            destination_chain: 137,
            amount: U256::from(10_000_000_u64),
        },
        recipient: handler,
        message: Bytes::from_static(&[0xab, 0xcd, 0x01]),
    };

    across(base_url)
        .suggested_fees_with_message(&request)
        .await
        .unwrap();

    let (_, url, _) = only_request(&recorded);
    assert_eq!(url.path(), "/api/suggested-fees");
    assert_eq!(
        query_keys(&url),
        [
            "allowUnmatchedDecimals",
            "amount",
            "destinationChainId",
            "inputToken",
            "message",
            "originChainId",
            "outputToken",
            "recipient"
        ]
    );
    let sent = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    assert_eq!(
        sent("recipient").as_deref(),
        Some("0x9295ee1d8C5b022Be115A2AD3c30C72E34e7F096")
    );
    assert_eq!(sent("message").as_deref(), Some("0xabcd01"));

    // The preview quote for the same pair sends neither.
    let (base_url, recorded) = spawn_fixed(200, ACROSS_SUGGESTED_FEES).await;
    across(base_url).suggested_fees(&request.fee).await.unwrap();
    let (_, url, _) = only_request(&recorded);
    assert!(
        !url.query_pairs()
            .any(|(key, _)| key == "recipient" || key == "message")
    );

    // A message that reverts in Across's simulation gets no quote, and the echoed fill
    // transaction isn't kept.
    let (base_url, _) = spawn_fixed(
        400,
        r#"{"type":"AcrossApiError","code":"SIMULATION_ERROR","status":400,"message":"execution reverted","transaction":{"to":"0x9295ee1d8C5b022Be115A2AD3c30C72E34e7F096","data":"0xabcd01"}}"#,
    )
    .await;
    assert_eq!(
        across(base_url).suggested_fees_with_message(&request).await,
        Err(BridgeApiError::FillSimulationFailed)
    );
}

#[tokio::test]
async fn across_deposit_lookup_reads_fills_and_missing_deposits() {
    let (base_url, recorded) = spawn_fixed(200, ACROSS_DEPOSIT_FILLED).await;
    let deposit = across(base_url)
        .deposit(42161, U256::from(4_701_902_u64))
        .await
        .unwrap();
    assert_eq!(
        deposit,
        Some(AcrossDeposit {
            status: AcrossDepositStatus::Filled,
            fill_block_number: Some(26_085_850),
            fill_tx: Some(b256!(
                "f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1"
            )),
            deposit_refund_tx: None,
            output_amount: U256::from(378_966_308_170_474_u64),
            recipient: Address::repeat_byte(0x11),
            destination_chain: 1,
        })
    );
    let (_, url, _) = only_request(&recorded);
    assert_eq!(
        format!("{}?{}", url.path(), url.query().unwrap()),
        "/api/deposit?originChainId=42161&depositId=4701902"
    );

    let (base_url, _) = spawn_fixed(
        404,
        r#"{"error":"DepositNotFoundException","message":"Deposit not found given the provided constraints"}"#,
    )
    .await;
    assert_eq!(
        across(base_url).deposit(42161, U256::from(1_u64)).await,
        Ok(None)
    );
    // Any other 404 is an error, not an unindexed deposit.
    let (base_url, _) = spawn_fixed(404, "Not Found").await;
    assert!(matches!(
        across(base_url).deposit(42161, U256::from(1_u64)).await,
        Err(BridgeApiError::Rejected { status: 404, .. })
    ));
}

#[tokio::test]
async fn across_errors_tell_rate_limits_from_outages_and_small_amounts() {
    let request = AcrossFeeRequest {
        input_token: Address::repeat_byte(0xa0),
        output_token: Address::repeat_byte(0xb0),
        origin_chain: 42161,
        destination_chain: 137,
        amount: U256::from(1_u64),
    };
    let too_low_quote =
        ACROSS_SUGGESTED_FEES.replace(r#""isAmountTooLow":false"#, r#""isAmountTooLow":true"#);
    for (status, response, expected) in [
        (
            429,
            "",
            BridgeApiError::RateLimited {
                api: BridgeApi::Across,
            },
        ),
        (
            503,
            "",
            BridgeApiError::Unavailable {
                api: BridgeApi::Across,
                operation: "fee quote",
                endpoint: String::new(),
                status: 503,
            },
        ),
        (
            400,
            r#"{"type":"AcrossApiError","code":"AMOUNT_TOO_LOW","status":400,"message":"Sent amount is too low relative to fees"}"#,
            BridgeApiError::AmountTooLow,
        ),
        (200, too_low_quote.as_str(), BridgeApiError::AmountTooLow),
    ] {
        let (base_url, _) = spawn_fixed(status, response).await;
        let error = across(base_url)
            .suggested_fees(&request)
            .await
            .expect_err("error response");
        // The mock's host differs per case.
        let error = match error {
            BridgeApiError::Unavailable {
                api,
                operation,
                status,
                ..
            } => BridgeApiError::Unavailable {
                api,
                operation,
                endpoint: String::new(),
                status,
            },
            other => other,
        };
        assert_eq!(error, expected, "HTTP {status}");
        assert!(error.to_string().contains("Across"), "{error}");
    }
}

#[tokio::test]
async fn near_dry_quotes_send_fresh_placeholder_addresses() {
    let signing_key = test_signing_key();
    let quote_key = key_string(&signing_key.verifying_key());
    let fixture: Value = serde_json::from_str(ONE_CLICK_DRY_QUOTE).unwrap();
    // 1Click echoes the request it signed, so answer each request with its own terms.
    let (base_url, recorded) = spawn_mock(move |request| {
        let mut quote_request = request.body.clone();
        quote_request["depositMode"] = Value::from("SIMPLE");
        let quote = fixture["quote"].clone();
        let timestamp = fixture["timestamp"].clone();
        let response = serde_json::json!({
            "quote": quote,
            "quoteRequest": quote_request,
            "timestamp": timestamp,
        });
        (200, sign_quote_response_for_tests(response, &signing_key))
    })
    .await;
    let client = NearIntentsClient::new(test_http(), base_url, &quote_key).unwrap();

    for _ in 0..2 {
        let quote = client.dry_quote(&fixture_params()).await.unwrap();
        assert_eq!(
            quote.min_amount_out,
            U256::from_str_radix("168087553224089770016", 10).unwrap()
        );
    }

    let recorded = recorded.lock().expect("recorded requests");
    let mut placeholders = Vec::new();
    for request in recorded.iter() {
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("POST", "/api/v0/quote")
        );
        let body = request.body.as_object().expect("quote body is an object");
        let mut keys = body.keys().map(String::as_str).collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "amount",
                "deadline",
                "depositType",
                "destinationAsset",
                "dry",
                "originAsset",
                "recipient",
                "recipientType",
                "refundTo",
                "refundType",
                "slippageTolerance",
                "swapType",
            ]
        );
        assert_eq!(body["dry"], true);
        assert_eq!(body["deadline"], "2026-09-29T23:47:20.000Z");
        for field in ["recipient", "refundTo"] {
            placeholders.push(body[field].as_str().unwrap().parse::<Address>().unwrap());
        }
    }
    assert_eq!(placeholders.len(), 4);
    for (index, placeholder) in placeholders.iter().enumerate() {
        assert_ne!(*placeholder, Address::ZERO);
        assert!(
            !placeholders[index + 1..].contains(placeholder),
            "reused {placeholder}"
        );
    }
}

#[test]
fn quote_verification_accepts_captured_quotes_and_rejects_tampering() {
    let pinned = pinned_quote_key();
    let params = fixture_params();
    let sent = SentQuote::new(
        &params,
        false,
        FIXTURE_RECIPIENT,
        FIXTURE_REFUND_TO,
        Some(EXECUTOR),
    );

    let terms = verify_quote(ONE_CLICK_QUOTE, &pinned, &sent).expect("captured quote verifies");
    assert_eq!(
        terms.deposit,
        Some((
            FIXTURE_DEPOSIT_ADDRESS,
            "2026-10-02T23:47:20.000Z".to_owned()
        ))
    );
    assert_eq!(
        terms.min_amount_out,
        U256::from_str_radix("167953443098610286739", 10).unwrap()
    );
    let dry_sent = SentQuote::new(&params, true, FIXTURE_RECIPIENT, FIXTURE_REFUND_TO, None);
    let dry_terms =
        verify_quote(ONE_CLICK_DRY_QUOTE, &pinned, &dry_sent).expect("captured dry quote verifies");
    assert_eq!(dry_terms.deposit, None);

    let signing_key = test_signing_key();
    let test_key = signing_key.verifying_key();
    let tampered = edited_quote("minAmountOut", "267953443098610286739").to_string();
    let zero_deposit = sign_quote_response_for_tests(
        edited_quote("depositAddress", &Address::ZERO.to_string()),
        &signing_key,
    );
    let early_deadline = sign_quote_response_for_tests(
        edited_quote("deadline", "2026-09-29T23:47:19.999Z"),
        &signing_key,
    );
    let other_recipient = SentQuote::new(
        &params,
        false,
        Address::repeat_byte(0x51),
        FIXTURE_REFUND_TO,
        Some(EXECUTOR),
    );
    let deposit_to_executor = SentQuote::new(
        &params,
        false,
        FIXTURE_RECIPIENT,
        FIXTURE_REFUND_TO,
        Some(FIXTURE_DEPOSIT_ADDRESS),
    );
    for (case, body, key, sent, expected) in [
        (
            "tampered minAmountOut",
            tampered.as_str(),
            &pinned,
            &sent,
            OneClickVerifyError::Signature,
        ),
        (
            "different key",
            ONE_CLICK_QUOTE,
            &test_key,
            &sent,
            OneClickVerifyError::Signature,
        ),
        (
            "different recipient",
            ONE_CLICK_QUOTE,
            &pinned,
            &other_recipient,
            OneClickVerifyError::TermsMismatch("recipient"),
        ),
        (
            "deposit to the executor",
            ONE_CLICK_QUOTE,
            &pinned,
            &deposit_to_executor,
            OneClickVerifyError::DepositAddress,
        ),
        (
            "zero deposit address",
            zero_deposit.as_str(),
            &test_key,
            &sent,
            OneClickVerifyError::DepositAddress,
        ),
        (
            "deposit address expires early",
            early_deadline.as_str(),
            &test_key,
            &sent,
            OneClickVerifyError::TermsMismatch("quote deadline"),
        ),
    ] {
        assert_eq!(
            verify_quote(body, key, sent).err(),
            Some(expected),
            "{case}"
        );
    }
}

#[tokio::test]
async fn near_status_reads_pending_and_success_reports() {
    let near = |base_url| {
        NearIntentsClient::new(test_http(), base_url, &key_string(&pinned_quote_key())).unwrap()
    };
    let (base_url, recorded) = spawn_fixed(200, ONE_CLICK_STATUS_PENDING).await;
    assert_eq!(
        near(base_url).status(FIXTURE_DEPOSIT_ADDRESS).await,
        Ok(OneClickStatus {
            status: OneClickExecutionStatus::PendingDeposit,
            amount_out: None,
            refunded_amount: Some(U256::ZERO),
            destination_tx_hashes: Vec::new(),
        })
    );
    let (_, url, _) = only_request(&recorded);
    assert_eq!(
        format!("{}?{}", url.path(), url.query().unwrap()),
        "/api/v0/status?depositAddress=0xA423c697bF8e8BFC42ac0f03737A78B898141f33"
    );

    let success = r#"{"status":"SUCCESS","updatedAt":"2026-09-29T22:01:10.000Z","swapDetails":{"intentHashes":["9WmT"],"nearTxHashes":["4HbD"],"amountIn":"20000000","amountOut":"169100000000000000000","refundedAmount":"0","originChainTxHashes":[{"hash":"0x0101010101010101010101010101010101010101010101010101010101010101","explorerUrl":"https://arbiscan.io/tx/0x0101010101010101010101010101010101010101010101010101010101010101"}],"destinationChainTxHashes":[{"hash":"0x0202020202020202020202020202020202020202020202020202020202020202","explorerUrl":"https://polygonscan.com/tx/0x0202020202020202020202020202020202020202020202020202020202020202"}]}}"#;
    let (base_url, _) = spawn_fixed(200, success).await;
    assert_eq!(
        near(base_url).status(FIXTURE_DEPOSIT_ADDRESS).await,
        Ok(OneClickStatus {
            status: OneClickExecutionStatus::Success,
            amount_out: Some(U256::from(169_100_000_000_000_000_000_u128)),
            refunded_amount: Some(U256::ZERO),
            destination_tx_hashes: vec![format!("0x{}", "02".repeat(32))],
        })
    );

    let (base_url, _) = spawn_fixed(404, r#"{"message":"Deposit address not found"}"#).await;
    assert_eq!(
        near(base_url).status(FIXTURE_DEPOSIT_ADDRESS).await,
        Err(BridgeApiError::NotFound {
            api: BridgeApi::NearIntents
        })
    );
}

#[tokio::test]
async fn client_output_never_contains_credential_bearing_base_url() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind closed port");
    let port = listener.local_addr().expect("closed port address").port();
    drop(listener);
    let base_url = Url::parse(&format!(
        "http://bridge-user:bridge-secret@127.0.0.1:{port}/private-path?token=query-secret"
    ))
    .expect("credential URL");
    let client = across(base_url);

    let error = client
        .available_routes(42161, 137)
        .await
        .expect_err("closed port fails");
    let message = error.to_string();
    assert!(message.contains(&format!("127.0.0.1:{port}")), "{message}");
    assert!(message.contains("Across"), "{message}");

    for output in [
        format!("{client:?}"),
        format!("{error}"),
        format!("{error:?}"),
    ] {
        assert!(!output.contains("bridge-user"), "{output}");
        assert!(!output.contains("bridge-secret"), "{output}");
        assert!(!output.contains("private-path"), "{output}");
        assert!(!output.contains("query-secret"), "{output}");
    }
}
