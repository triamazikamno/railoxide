use super::*;
use std::net::{IpAddr, Ipv4Addr};

const BUNDLE: GatewayInstallBundle = GatewayInstallBundle {
    zip: b"PK-test",
    version: "9.9.9",
};

const fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn address(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

fn status(response: &[u8]) -> String {
    String::from_utf8_lossy(response)
        .lines()
        .next()
        .unwrap()
        .to_owned()
}

fn body(response: &[u8]) -> Vec<u8> {
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    response[end..].to_vec()
}

#[test]
fn upgrade_detection_ignores_header_case_and_absence() {
    assert!(is_websocket_upgrade(
        b"GET / HTTP/1.1\r\nHost: x\r\nUPGRADE: WebSocket\r\n\r\n"
    ));
    assert!(is_websocket_upgrade(
        b"GET / HTTP/1.1\r\nUpgrade:  websocket \r\n\r\n"
    ));
    assert!(!is_websocket_upgrade(
        b"GET /install HTTP/1.1\r\nHost: x\r\n\r\n"
    ));
    // The request line never counts as a header, and h2c is not the transport upgrade.
    assert!(!is_websocket_upgrade(b"Upgrade: websocket\r\n\r\n"));
    assert!(!is_websocket_upgrade(
        b"GET / HTTP/1.1\r\nUpgrade: h2c\r\n\r\n"
    ));
}

#[test]
fn upgrade_origin_admits_only_the_extension_or_no_origin() {
    assert!(!is_allowed_upgrade_origin(
        b"GET / HTTP/1.1\r\nUpgrade: websocket\r\nOrigin: https://example.com\r\n\r\n"
    ));
    assert!(is_allowed_upgrade_origin(
        b"GET / HTTP/1.1\r\nUpgrade: websocket\r\nOrigin: chrome-extension://abcdef\r\n\r\n"
    ));
    assert!(is_allowed_upgrade_origin(
        b"GET / HTTP/1.1\r\nUpgrade: websocket\r\n\r\n"
    ));
}

#[test]
fn install_routes_answer_get_and_head_with_matching_headers() {
    let page = respond(
        b"GET /install/?v=1 HTTP/1.1\r\nHost: 127.0.0.1:43110\r\n\r\n",
        BUNDLE,
        loopback(default_port()),
    );
    let text = String::from_utf8_lossy(&page).into_owned();
    assert_eq!(status(&page), "HTTP/1.1 200 OK");
    assert!(text.contains("Content-Type: text/html; charset=utf-8"));
    assert!(text.contains("Cache-Control: no-store"));
    assert!(text.contains("X-Content-Type-Options: nosniff"));
    assert!(text.contains("Referrer-Policy: no-referrer"));
    assert!(text.contains("Connection: close"));

    let head = respond(
        b"HEAD /install HTTP/1.1\r\nHost: 127.0.0.1:43110\r\n\r\n",
        BUNDLE,
        loopback(default_port()),
    );
    assert_eq!(status(&head), "HTTP/1.1 200 OK");
    assert_eq!(body(&head), [] as [u8; 0]);
    // HEAD must still announce the length the matching GET would send.
    let length = format!("Content-Length: {}", body(&page).len());
    assert!(String::from_utf8_lossy(&head).contains(&length));
}

#[test]
fn zip_route_serves_the_bundle_and_disappears_without_one() {
    let served = respond(
        b"GET /install/RailOxide-Extension.zip HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        BUNDLE,
        loopback(default_port()),
    );
    assert_eq!(status(&served), "HTTP/1.1 200 OK");
    let disposition = "Content-Disposition: attachment; filename=\"RailOxide-Extension.zip\"";
    assert!(String::from_utf8_lossy(&served).contains(disposition));
    assert_eq!(body(&served), b"PK-test");

    let missing = respond(
        b"GET /install/RailOxide-Extension.zip HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        GatewayInstallBundle::default(),
        loopback(default_port()),
    );
    assert_eq!(status(&missing), "HTTP/1.1 404 Not Found");
}

#[test]
fn unknown_routes_methods_and_malformed_heads_are_rejected() {
    let local = loopback(default_port());
    let unknown = respond(
        b"GET /admin HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        BUNDLE,
        local,
    );
    assert_eq!(status(&unknown), "HTTP/1.1 404 Not Found");
    assert_eq!(body(&unknown), b"Not found");

    let posted = respond(
        b"POST /install HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        BUNDLE,
        local,
    );
    assert_eq!(status(&posted), "HTTP/1.1 405 Method Not Allowed");
    assert!(String::from_utf8_lossy(&posted).contains("Allow: GET, HEAD"));

    let malformed = [
        b"".as_slice(),
        b"GET /install\r\n\r\n".as_slice(),
        b"hello\r\n\r\n".as_slice(),
    ];
    for malformed in malformed {
        let rejected = respond(malformed, BUNDLE, local);
        assert_eq!(status(&rejected), "HTTP/1.1 400 Bad Request");
    }
}

#[test]
fn page_reports_the_serving_host_and_connection_mode() {
    let default = loopback(default_port());
    let page = render_install_page(BUNDLE, Some("127.0.0.1:43110"), default);
    assert!(page.contains(&default.to_string()));
    assert!(page.contains("9.9.9"));
    assert!(page.contains(r#"class="local bundled""#));
    // An attacker who replaces the zip rewrites the page and its copy buttons too, so the
    // hash and the commands that check it come only from the desktop dialog.
    assert!(!page.contains(&BUNDLE.zip_sha256().unwrap()));
    for tool in ["sha256sum", "shasum", "certutil", "Get-FileHash"] {
        assert!(!page.contains(tool));
    }

    // On loopback the socket decides both the mode and the address, whatever the header says.
    for header in [Some("localhost:43110"), Some("evil\"><b>"), None] {
        let page = render_install_page(BUNDLE, header, default);
        assert!(page.contains(&default.to_string()));
        assert!(page.contains(r#"class="local bundled""#));
        assert!(!page.contains("localhost"));
        assert!(!page.contains("evil"));
    }

    let other = loopback(default_port() + 1);
    let page = render_install_page(BUNDLE, Some("127.0.0.1"), other);
    assert!(page.contains(r#"class="local-port bundled""#));
    assert!(page.contains(&format!("ws://{other}/")));

    let remote = address("192.168.1.5:43110");
    let page = render_install_page(BUNDLE, Some("railoxide.example:8443"), remote);
    assert!(page.contains(r#"class="remote bundled""#));
    assert!(page.contains("ws://railoxide.example:8443/"));

    // The extension only skips its prompt for the literal 127.0.0.1, so `[::1]` is remote.
    let page = render_install_page(BUNDLE, None, address("[::1]:43110"));
    assert!(page.contains(r#"class="remote bundled""#));
    assert!(page.contains("ws://[::1]:43110/"));

    let unbundled = render_install_page(
        GatewayInstallBundle {
            version: "9.9.9",
            ..GatewayInstallBundle::default()
        },
        None,
        default,
    );
    assert!(unbundled.contains(r#"class="local unbundled""#));
    assert!(unbundled.contains("unavailable"));
}

#[test]
fn remote_pages_fall_back_to_the_local_address_when_the_host_is_unusable() {
    let local = address("192.168.1.5:43110");
    let headers = [
        Some("evil\"><b>"),
        Some(""),
        Some("   "),
        Some("hos t"),
        Some("hôte"),
        None,
    ];
    for header in headers {
        let page = render_install_page(BUNDLE, header, local);
        assert!(page.contains("ws://192.168.1.5:43110/"));
        assert!(page.contains(r#"class="remote bundled""#));
        assert!(!page.contains("evil"));
        assert!(!page.contains("hôte"));
    }
}

#[tokio::test]
async fn peek_head_waits_for_a_split_head_and_rejects_an_oversized_one() {
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();

    let head = b"GET /install HTTP/1.1\r\nHost: 127.0.0.1\r\nUser-Agent: split\r\n\r\n";
    let split = head.len() - 12;
    let mut client = TcpStream::connect(address).await.unwrap();
    let (accepted, _) = listener.accept().await.unwrap();
    let writer = tokio::spawn(async move {
        client.write_all(&head[..split]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.write_all(&head[split..]).await.unwrap();
        client
    });
    let peeked = tokio::time::timeout(Duration::from_secs(5), peek_head(&accepted))
        .await
        .expect("peek must finish once the rest of the head arrives");
    assert_eq!(peeked.as_deref(), Some(head.as_slice()));
    writer.await.unwrap();

    let mut client = TcpStream::connect(address).await.unwrap();
    let (accepted, _) = listener.accept().await.unwrap();
    let writer = tokio::spawn(async move {
        client
            .write_all(&vec![b'a'; MAX_HEAD_LEN + 1])
            .await
            .unwrap();
        client
    });
    let peeked = tokio::time::timeout(Duration::from_secs(5), peek_head(&accepted))
        .await
        .expect("a head that cannot terminate in range must be rejected, not awaited");
    assert!(peeked.is_none());
    // Nothing drains the peeked bytes, so the writer may still be blocked on the socket buffer.
    writer.abort();
}

#[test]
fn bundle_size_reads_as_a_rounded_unit() {
    assert_eq!(human_size(0), "unavailable");
    assert_eq!(human_size(700), "700 bytes");
    assert_eq!(human_size(645_000), "630 KB");
    assert_eq!(human_size(3_670_016), "3.5 MB");
}
