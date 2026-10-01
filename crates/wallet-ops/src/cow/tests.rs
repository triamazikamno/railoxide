use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use alloy::primitives::{address, b256};
use broadcaster_core::contracts::cow::{
    AppData, AppDataHook, ORDER_KIND_SELL, TOKEN_BALANCE_ERC20,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::http::WalletNetworkMode;

/// `keccak256("{}")`, which `CoW`'s API documents as the app data hash of an order without
/// metadata.
const EMPTY_APP_DATA_HASH: B256 =
    b256!("0xb48d38f93eaa084033fc5970bf96e559c33c4cdc07d889ab00b4d63f9590739d");

const QUOTE_RESPONSE: &str = r#"{
  "quote": {
    "sellToken": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
    "buyToken": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
    "receiver": "0x1111111111111111111111111111111111111111",
    "sellAmount": "998765432",
    "buyAmount": "401234567890123456",
    "validTo": 1790000600,
    "appData": "{}",
    "appDataHash": "0xb48d38f93eaa084033fc5970bf96e559c33c4cdc07d889ab00b4d63f9590739d",
    "feeAmount": "1234568",
    "gasAmount": "112000",
    "gasPrice": "1210000000",
    "sellTokenPrice": "412345678.9",
    "kind": "sell",
    "partiallyFillable": false,
    "sellTokenBalance": "erc20",
    "buyTokenBalance": "erc20",
    "signingScheme": "eip712"
  },
  "from": "0x1111111111111111111111111111111111111111",
  "expiration": "2026-09-26T12:01:00.000000Z",
  "id": 512345678,
  "verified": true,
  "protocolFeeBps": "2"
}"#;

/// A live mainnet quote of 0.1 WETH for USDC with a nonzero protocol fee, captured on
/// 2026-09-30. Its `buyAmount` is net of both the network fee and the protocol fee.
const PROTOCOL_FEE_QUOTE_RESPONSE: &str = r#"{"quote":{"sellToken":"0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2","buyToken":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48","receiver":"0x1111111111111111111111111111111111111111","sellAmount":"99474893617383870","buyAmount":"267722966","validTo":1790788483,"appData":"{}","appDataHash":"0xb48d38f93eaa084033fc5970bf96e559c33c4cdc07d889ab00b4d63f9590739d","feeAmount":"525106382616130","gasAmount":"232610","gasPrice":"2257454033","sellTokenPrice":"1","kind":"sell","partiallyFillable":false,"sellTokenBalance":"erc20","buyTokenBalance":"erc20","signingScheme":"eip712"},"from":"0x1111111111111111111111111111111111111111","expiration":"2026-09-30T17:06:48.160625682Z","id":1405054097,"verified":true,"protocolFeeBps":"2"}"#;

const APP_DATA_TOO_LARGE_RESPONSE: &str = r#"{"errorType":"InvalidAppData","description":"app data has byte size 90112 which is larger than limit 81920"}"#;

#[derive(Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    body: Value,
}

type Recorded = Arc<Mutex<Vec<RecordedRequest>>>;

/// Serve one recorded orderbook response to every request and record what was sent.
async fn spawn_orderbook_mock(status: u16, response: &str) -> (Url, Recorded) {
    let response = response.to_owned();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind orderbook mock");
    let address = listener.local_addr().expect("orderbook mock address");
    let recorded = Recorded::default();
    let sink = Arc::clone(&recorded);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let sink = Arc::clone(&sink);
            let response = response.clone();
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
                sink.lock().expect("record request").push(RecordedRequest {
                    method,
                    path,
                    body: serde_json::from_slice(body).unwrap_or(Value::Null),
                });
                let reply = format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    let base_url = Url::parse(&format!("http://{address}/mainnet")).expect("orderbook mock URL");
    (base_url, recorded)
}

fn test_client(base_url: Url) -> CowOrderbookClient {
    let http = OperationHttpClient::for_tests(
        reqwest::Client::new(),
        OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
    );
    CowOrderbookClient::new(http, base_url, 1).expect("orderbook client")
}

fn quote_request() -> CowSellQuoteRequest {
    CowSellQuoteRequest {
        sell_token: Address::repeat_byte(0xa0),
        buy_token: Address::repeat_byte(0xc0),
        from: Address::repeat_byte(0x11),
        receiver: Address::repeat_byte(0x11),
        sell_amount_before_fee: U256::from(1_000_000_000_u64),
        valid_to: 1_790_000_600,
    }
}

fn signed_hook_order() -> (Order, EncodedAppData) {
    let app_data = AppData::hooks(
        "railgun".to_owned(),
        vec![AppDataHook {
            call_data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            gas_limit: 900_000,
            target: Address::repeat_byte(0x11),
        }],
        vec![AppDataHook {
            call_data: Bytes::from_static(&[0xfe, 0xed]),
            gas_limit: 250_000,
            target: Address::repeat_byte(0x11),
        }],
    )
    .encode()
    .expect("encode app data");
    let order = Order {
        sellToken: Address::repeat_byte(0xa0),
        buyToken: Address::repeat_byte(0xc0),
        receiver: Address::repeat_byte(0x11),
        sellAmount: U256::from(1_000_000_000_u64),
        buyAmount: U256::from(400_000_000_000_000_000_u64),
        validTo: 1_790_000_600,
        appData: app_data.hash,
        feeAmount: U256::ZERO,
        kind: ORDER_KIND_SELL.to_owned(),
        partiallyFillable: false,
        sellTokenBalance: TOKEN_BALANCE_ERC20.to_owned(),
        buyTokenBalance: TOKEN_BALANCE_ERC20.to_owned(),
    };
    (order, app_data)
}

#[tokio::test]
async fn quote_timeout_covers_a_stalled_response_body() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let base_url = Url::parse(&format!(
        "http://{}/mainnet",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (started, response_started) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n{")
            .await
            .unwrap();
        started.send(()).unwrap();
        std::future::pending::<()>().await;
        drop(stream);
    });
    let quote =
        tokio::spawn(async move { test_client(base_url).quote_sell(&quote_request()).await });
    response_started.await.unwrap();
    tokio::time::pause();
    let error = tokio::time::timeout(Duration::from_secs(20), quote)
        .await
        .expect("an interactive quote must stop waiting promptly")
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        error,
        CowApiError::Timeout {
            operation: "quote",
            ..
        }
    ));
    server.abort();
}

#[tokio::test]
async fn quote_request_carries_no_hooks_or_signed_payloads() {
    let (base_url, recorded) = spawn_orderbook_mock(200, QUOTE_RESPONSE).await;
    let client = test_client(base_url);

    let quote = client
        .quote_sell(&quote_request())
        .await
        .expect("recorded quote parses");

    assert_eq!(quote.quote.sell_amount, U256::from(998_765_432_u64));
    assert_eq!(quote.quote.fee_amount, U256::from(1_234_568_u64));
    assert_eq!(
        quote.quote.buy_amount,
        U256::from(401_234_567_890_123_456_u64)
    );
    assert_eq!(quote.id, Some(512_345_678));
    assert!(quote.verified);
    assert!(!quote.quote.partially_fillable);

    let recorded = recorded.lock().expect("recorded requests");
    let [request] = recorded.as_slice() else {
        panic!("expected one quote request, got {recorded:?}");
    };
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/mainnet/api/v1/quote");
    let body = request.body.as_object().expect("quote body is an object");
    let mut keys = body.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "appData",
            "appDataHash",
            "buyToken",
            "from",
            "kind",
            "receiver",
            "sellAmountBeforeFee",
            "sellToken",
            "signingScheme",
            "validTo",
        ]
    );
    assert_eq!(body["appData"], "{}");
    assert_eq!(
        body["appDataHash"],
        EMPTY_APP_DATA_HASH.to_string().as_str()
    );
    assert_eq!(EMPTY_APP_DATA_HASH, keccak256(b"{}"));
    assert_eq!(body["kind"], "sell");
    assert_eq!(body["sellAmountBeforeFee"], "1000000000");
    let serialized = request.body.to_string();
    assert!(!serialized.contains("hooks"));
    assert!(!serialized.contains("signature"));
    assert!(!serialized.contains("callData"));
}

#[test]
fn live_quote_parses_its_protocol_fee_and_gas() {
    let quote: CowQuote = serde_json::from_str(PROTOCOL_FEE_QUOTE_RESPONSE).unwrap();
    assert_eq!(quote.protocol_fee_bps.as_deref(), Some("2"));
    assert_eq!(quote.quote.gas_amount, "232610");
    assert_eq!(quote.quote.fee_amount, U256::from(525_106_382_616_130_u64));
}

/// `CoW`'s protocol fee is reconstructed from the net `buyAmount`, while the best case adds back
/// only the network fee.
#[test]
fn protocol_fee_is_shown_only_when_the_quote_states_it() {
    let quote: CowQuote = serde_json::from_str(PROTOCOL_FEE_QUOTE_RESPONSE).unwrap();
    // 267,722,966 * 2 / 9,998.
    assert_eq!(quote_protocol_fee(&quote), Some(U256::from(53_555)));
    let limit = price_order_limit(&OrderLimitParams {
        quote: &quote.quote,
        quote_gas_units: quote_gas_units(&quote.quote).unwrap(),
        hook_gas: 0,
        gas_price_wei: 0,
        hook_data_cost_wei: U256::ZERO,
        native_rate: NativeBuyRate::Anchor(U256::ONE),
        price_tolerance_bps: 0,
        gas_share_bps: 0,
        shield_fee_bps: U256::ZERO,
    })
    .unwrap();
    // A network fee of 1,413,251 at the quoted rate.
    assert_eq!(limit.best_case, U256::from(269_136_217));

    // The USDC to USDT quote from the same capture stated a fractional fee:
    // 8,539,024 * 0.3 / 9,999.7.
    let mut fractional = quote.clone();
    fractional.quote.buy_amount = U256::from(8_539_024);
    fractional.protocol_fee_bps = Some("0.3".to_owned());
    assert_eq!(quote_protocol_fee(&fractional), Some(U256::from(256)));

    let omitted = CowQuote {
        protocol_fee_bps: None,
        ..quote
    };
    assert_eq!(quote_protocol_fee(&omitted), None);
}

#[tokio::test]
async fn order_submission_sends_full_app_data_and_returns_uid() {
    let uid = format!("\"0x{}\"", "ab".repeat(56));
    let (base_url, recorded) = spawn_orderbook_mock(201, &uid).await;
    let client = test_client(base_url);
    let (order, app_data) = signed_hook_order();
    let signature = [0x1b_u8; 65];

    let submitted = client
        .submit_order(&CowOrderSubmission {
            order: &order,
            owner: Address::repeat_byte(0x11),
            signature: &signature,
            app_data: &app_data,
            quote_id: Some(512_345_678),
        })
        .await
        .expect("order accepted");

    assert_eq!(submitted.0.as_slice(), [0xab_u8; 56]);
    let recorded = recorded.lock().expect("recorded requests");
    let [request] = recorded.as_slice() else {
        panic!("expected one order request, got {recorded:?}");
    };
    assert_eq!(request.path, "/mainnet/api/v1/orders");
    let body = &request.body;
    assert_eq!(body["appData"], app_data.document.as_str());
    assert_eq!(body["appDataHash"], app_data.hash.to_string().as_str());
    assert_eq!(body["signingScheme"], "eip712");
    assert_eq!(body["signature"], format!("0x{}", "1b".repeat(65)).as_str());
    assert_eq!(body["sellAmount"], "1000000000");
    assert_eq!(body["buyAmount"], "400000000000000000");
    assert_eq!(body["feeAmount"], "0");
    assert_eq!(body["partiallyFillable"], false);
    assert_eq!(body["quoteId"], 512_345_678);
}

#[tokio::test]
async fn order_app_data_size_rejections_require_replanning() {
    let (order, app_data) = signed_hook_order();
    let signature = [0x1b_u8; 65];
    let submission = CowOrderSubmission {
        order: &order,
        owner: Address::repeat_byte(0x11),
        signature: &signature,
        app_data: &app_data,
        quote_id: None,
    };

    // The app-data validator's size check, and the order endpoint's request body limit.
    for (status, response) in [
        (400, APP_DATA_TOO_LARGE_RESPONSE),
        (
            413,
            "Failed to buffer the request body: length limit exceeded",
        ),
    ] {
        let (base_url, _) = spawn_orderbook_mock(status, response).await;
        let error = test_client(base_url)
            .submit_order(&submission)
            .await
            .expect_err("oversized app data is rejected");
        assert_eq!(error, CowApiError::AppDataTooLarge);
        assert!(error.requires_replan());
    }

    let (base_url, _) = spawn_orderbook_mock(
        400,
        r#"{"errorType":"InvalidAppData","description":"invalid type: map, expected a string"}"#,
    )
    .await;
    let error = test_client(base_url)
        .submit_order(&submission)
        .await
        .expect_err("malformed app data is rejected");
    assert!(!error.requires_replan());
    assert!(
        error
            .to_string()
            .contains("invalid type: map, expected a string"),
        "{error}"
    );
}

/// A filled order as `GET /api/v1/orders/{uid}` reports it, trimmed of signature and app data.
fn filled_order_response(uid: &str) -> String {
    format!(
        r#"{{
  "creationDate": "2026-09-26T10:00:00.000000Z",
  "owner": "0x1111111111111111111111111111111111111111",
  "uid": "{uid}",
  "availableBalance": null,
  "executedBuyAmount": "49404100000000000000",
  "executedSellAmount": "49875000",
  "executedSellAmountBeforeFees": "49875000",
  "executedFeeAmount": "0",
  "executedFee": "464572",
  "executedFeeToken": "0xdac17f958d2ee523a2206206994597c13d831ec7",
  "invalidated": false,
  "status": "fulfilled",
  "class": "limit",
  "settlementContract": "0x9008d19f58aabd9ed0d60971565aa8510560ab41",
  "isLiquidityOrder": false,
  "sellToken": "0xdac17f958d2ee523a2206206994597c13d831ec7",
  "buyToken": "0x6b175474e89094c44da98b954eedeac495271d0f",
  "receiver": "0x1111111111111111111111111111111111111111",
  "sellAmount": "49875000",
  "buyAmount": "42617400000000000000",
  "validTo": 1790000600,
  "feeAmount": "0",
  "kind": "sell",
  "partiallyFillable": false,
  "sellTokenBalance": "erc20",
  "buyTokenBalance": "erc20",
  "signingScheme": "eip712"
}}"#
    )
}

#[tokio::test]
async fn order_hints_read_the_status_trade_block_and_fee_from_the_uid_alone() {
    let uid = OrderUid(FixedBytes::repeat_byte(0xba));
    let uid_hex = uid.0.to_string();

    let (base_url, recorded) = spawn_orderbook_mock(200, &filled_order_response(&uid_hex)).await;
    let report = test_client(base_url)
        .order_status_hint(&uid)
        .await
        .expect("recorded order parses");
    assert_eq!(
        report,
        CowOrderStatusReport {
            status: CowOrderStatusHint::Fulfilled,
        }
    );
    let trades = format!(
        r#"[{{"blockNumber":26063930,"logIndex":112,"orderUid":"{uid_hex}","owner":"0x1111111111111111111111111111111111111111","sellToken":"0xdac17f958d2ee523a2206206994597c13d831ec7","buyToken":"0x6b175474e89094c44da98b954eedeac495271d0f","sellAmount":"49875000","sellAmountBeforeFees":"49875000","buyAmount":"49404100000000000000","txHash":"0x517d000000000000000000000000000000000000000000000000000000000000","executedProtocolFees":[]}}]"#
    );
    let (base_url, recorded_trades) = spawn_orderbook_mock(200, &trades).await;
    assert_eq!(
        test_client(base_url).order_trade_block(&uid).await,
        Ok(Some(26_063_930))
    );
    let (base_url, recorded_fee) =
        spawn_orderbook_mock(200, &filled_order_response(&uid_hex)).await;
    assert_eq!(
        test_client(base_url).order_executed_fee(&uid).await,
        Ok(Some(CowExecutedFee {
            amount: U256::from(464_572),
            token: address!("dac17f958d2ee523a2206206994597c13d831ec7"),
        }))
    );

    // Every read is a plain GET identified by the order UID only.
    for (recorded, path) in [
        (recorded, format!("/mainnet/api/v1/orders/{uid_hex}")),
        (
            recorded_trades,
            format!("/mainnet/api/v1/trades?orderUid={uid_hex}"),
        ),
        (recorded_fee, format!("/mainnet/api/v1/orders/{uid_hex}")),
    ] {
        let recorded = recorded.lock().expect("recorded requests");
        let [request] = recorded.as_slice() else {
            panic!("expected one request, got {recorded:?}");
        };
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("GET", path.as_str())
        );
        assert_eq!(request.body, Value::Null);
    }
}

#[tokio::test]
async fn client_output_never_contains_credential_bearing_base_url() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind closed port");
    let port = listener.local_addr().expect("closed port address").port();
    drop(listener);
    let base_url = Url::parse(&format!(
        "http://orderbook-user:orderbook-secret@127.0.0.1:{port}/private-path?token=query-secret"
    ))
    .expect("credential URL");
    let client = test_client(base_url);

    let error = client
        .quote_sell(&quote_request())
        .await
        .expect_err("closed port fails");
    // The host and the underlying cause are shown, so the user can check reachability.
    let message = error.to_string();
    assert!(message.contains(&format!("127.0.0.1:{port}")), "{message}");
    assert!(message.contains("error sending request: "), "{message}");

    for output in [
        format!("{client:?}"),
        format!("{error}"),
        format!("{error:?}"),
    ] {
        assert!(!output.contains("orderbook-user"), "{output}");
        assert!(!output.contains("orderbook-secret"), "{output}");
        assert!(!output.contains("private-path"), "{output}");
        assert!(!output.contains("query-secret"), "{output}");
    }
}
