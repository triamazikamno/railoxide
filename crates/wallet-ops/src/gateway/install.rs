//! Plain HTTP install page and extension download served on the gateway listener.
//!
//! A browser reaches the same port as the authenticated transport, so every accepted connection is
//! classified before the handshake: requests carrying an `Upgrade: websocket` header stay on the
//! transport path, everything else is answered here and closed.

use super::{GatewayConfig, GatewayInstallBundle};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};

/// Bounds the peeked request head, mirroring tungstenite's own handshake byte limit (`MAX_BYTES =
/// 65536` in its handshake machine) so the classifier never rejects a head the transport accepts.
const MAX_HEAD_LEN: usize = 64 * 1024;
/// Header slots for parsing, matching tungstenite's own handshake parser.
const MAX_HEADERS: usize = 124;
/// Re-peek delay while a request head is still incomplete, bounded by the handshake deadline.
const HEAD_POLL_INTERVAL: Duration = Duration::from_millis(5);
const PAGE: &str = include_str!("install/index.html");
const ZIP_ROUTE: &str = "/install/RailOxide-Extension.zip";
const TEXT_TYPE: &str = "text/plain; charset=utf-8";
const HTML_HEADERS: &[&str] = &[
    "Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:",
    "Referrer-Policy: no-referrer",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Method {
    Get,
    Head,
    Other,
}

/// How the browser reaching this page can get to the transport, decided by the address it reached.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// IPv4 loopback on the default port: the extension connects with no prompt and no address.
    Local,
    /// IPv4 loopback on another port: no prompt, but the address has to be entered by hand.
    LocalPort,
    /// Everything else, `[::1]` included: the address has to be entered and the access allowed.
    Remote,
}

impl Mode {
    const fn class(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::LocalPort => "local-port",
            Self::Remote => "remote",
        }
    }
}

/// Peeks the request head, leaving it queued so a `WebSocket` handshake still sees the full request.
///
/// Returns `None` for a closed connection, a head longer than [`MAX_HEAD_LEN`], or a read failure.
pub(super) async fn peek_head(stream: &TcpStream) -> Option<Vec<u8>> {
    let mut buffer = vec![0; MAX_HEAD_LEN];
    let mut peeked = 0;
    loop {
        let read = stream.peek(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        match httparse::Request::new(&mut headers).parse(&buffer[..read]) {
            Ok(httparse::Status::Complete(end)) => {
                buffer.truncate(end);
                return Some(buffer);
            }
            // Not HTTP at all: hand back what arrived so the caller answers 400 and closes.
            Err(_) => {
                buffer.truncate(read);
                return Some(buffer);
            }
            Ok(httparse::Status::Partial) => {}
        }
        if read >= MAX_HEAD_LEN {
            return None;
        }
        if read == peeked {
            // Peeked bytes stay queued, so a socket holding a partial head remains readable.
            tokio::time::sleep(HEAD_POLL_INTERVAL).await;
        }
        peeked = read;
    }
}

/// Consumes the peeked head, writes the response, and closes the connection.
pub(super) async fn serve(
    mut stream: TcpStream,
    head_len: usize,
    response: &[u8],
) -> std::io::Result<()> {
    let mut head = vec![0; head_len];
    stream.read_exact(&mut head).await?;
    stream.write_all(response).await?;
    stream.shutdown().await
}

pub(super) fn is_websocket_upgrade(head: &[u8]) -> bool {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    complete_request(head, &mut headers).is_some_and(|request| {
        header(&request, "upgrade").is_some_and(|value| value.eq_ignore_ascii_case(b"websocket"))
    })
}

/// Builds the response for a non-upgrade request. An empty head answers a malformed request.
///
/// `local` is the accepted socket's own address, which decides what the page tells the reader.
pub(super) fn respond(head: &[u8], bundle: GatewayInstallBundle, local: SocketAddr) -> Vec<u8> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let Some(request) = complete_request(head, &mut headers) else {
        return response("400 Bad Request", TEXT_TYPE, &[], b"Bad request", true);
    };
    let method = match request.method {
        Some("GET") => Method::Get,
        Some("HEAD") => Method::Head,
        _ => Method::Other,
    };
    // A body belongs to GET only; HEAD repeats the same headers.
    let with_body = method == Method::Get;
    let target = request.path.unwrap_or_default();
    let path = target.split('?').next().unwrap_or(target);
    let host = header(&request, "host").and_then(|value| str::from_utf8(value).ok());
    match method {
        Method::Other => response(
            "405 Method Not Allowed",
            TEXT_TYPE,
            &["Allow: GET, HEAD"],
            b"Method not allowed",
            true,
        ),
        Method::Get | Method::Head => match path {
            "/install" | "/install/" => {
                let page = render_install_page(bundle, host, local);
                response(
                    "200 OK",
                    "text/html; charset=utf-8",
                    HTML_HEADERS,
                    page.as_bytes(),
                    with_body,
                )
            }
            ZIP_ROUTE if !bundle.zip.is_empty() => response(
                "200 OK",
                "application/zip",
                &["Content-Disposition: attachment; filename=\"RailOxide-Extension.zip\""],
                bundle.zip,
                with_body,
            ),
            _ => response("404 Not Found", TEXT_TYPE, &[], b"Not found", with_body),
        },
    }
}

/// Renders the page for `local`, the accepted socket's address, and the raw `Host` header value.
///
/// The mode is a server-side fact, so the header only ever picks the address a remote reader is
/// told to type back, and only when it survives [`sanitized_host`].
pub(super) fn render_install_page(
    bundle: GatewayInstallBundle,
    host_header: Option<&str>,
    local: SocketAddr,
) -> String {
    let mode = mode(local);
    let host = match mode {
        Mode::Remote => host_header
            .and_then(sanitized_host)
            .unwrap_or_else(|| local.to_string()),
        Mode::Local | Mode::LocalPort => local.to_string(),
    };
    PAGE.replace("{{HOST}}", &host)
        .replace("{{MODE}}", mode.class())
        .replace(
            "{{BUNDLE}}",
            if bundle.zip.is_empty() {
                "unbundled"
            } else {
                "bundled"
            },
        )
        .replace("{{VERSION}}", bundle.version)
        .replace("{{SIZE}}", &human_size(bundle.zip.len()))
}

/// Parses a complete request head; partial or non-HTTP bytes yield `None`.
fn complete_request<'h, 'b>(
    head: &'b [u8],
    headers: &'h mut [httparse::Header<'b>],
) -> Option<httparse::Request<'h, 'b>> {
    let mut request = httparse::Request::new(headers);
    matches!(request.parse(head), Ok(httparse::Status::Complete(_))).then_some(request)
}

/// The first header named `name`, case-insensitively, with surrounding whitespace removed.
fn header<'b>(request: &httparse::Request<'_, 'b>, name: &str) -> Option<&'b [u8]> {
    request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.trim_ascii())
}

fn response(
    status: &str,
    content_type: &str,
    extra: &[&str],
    body: &[u8],
    with_body: bool,
) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n",
        body.len()
    )
    .into_bytes();
    for line in extra {
        response.extend_from_slice(line.as_bytes());
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(b"\r\n");
    if with_body {
        response.extend_from_slice(body);
    }
    response
}

/// Keeps the only untrusted value on the page to characters that cannot escape an attribute or tag.
fn sanitized_host(host: &str) -> Option<String> {
    let host = host.trim();
    let usable = !host.is_empty()
        && host.len() <= 255
        && host.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'[' | b']' | b'-')
        });
    usable.then(|| host.to_owned())
}

/// The extension skips its access prompt only for the literal host `127.0.0.1` and auto-connects
/// only on the default port, so `[::1]` and every non-loopback address read as [`Mode::Remote`].
fn mode(local: SocketAddr) -> Mode {
    if local.ip().is_ipv4() && local.ip().is_loopback() {
        if local.port() == default_port() {
            Mode::Local
        } else {
            Mode::LocalPort
        }
    } else {
        Mode::Remote
    }
}

/// Port the extension dials on its own, taken from the configuration default it was built against.
fn default_port() -> u16 {
    GatewayConfig::default().port.get()
}

fn human_size(len: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * KB;
    if len == 0 {
        "unavailable".to_owned()
    } else if len < KB {
        format!("{len} bytes")
    } else if len < MB {
        format!("{} KB", len.div_ceil(KB))
    } else {
        format!("{}.{} MB", len / MB, (len % MB) * 10 / MB)
    }
}

#[cfg(test)]
mod tests {
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
        assert!(body(&head).is_empty());
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
}
