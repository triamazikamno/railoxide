//! Trustless resolution and immutable caching for governance proposal documents.

use std::io::Read;
use std::str::FromStr;

use cid::Cid;
use local_db::{CanonicalBlobMetaIdentity, DbStore};
use reqwest::Url;
use serde::Deserialize;

use crate::HttpContext;

const PROPOSAL_DOCUMENT_BLOB_KIND: &str = "proposals";
const MAX_PROPOSAL_DOCUMENT_BYTES: u64 = 2 * 1024 * 1024;

/// The user-facing contents of an on-chain governance proposal document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceDocument {
    pub title: String,
    pub description: String,
    pub available: bool,
}

impl GovernanceDocument {
    fn unavailable() -> Self {
        Self {
            title: "Document unavailable".to_string(),
            description: String::new(),
            available: false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GovernanceDocumentWire {
    title: String,
    description: String,
}

/// Resolve a proposal document from its CID, using the configured gateways and wallet HTTP
/// context. Invalid input, unavailable gateways, cache read failures, fetch failures, and
/// malformed documents all degrade to an unavailable placeholder so on-chain proposal data
/// remains usable. Cache persistence is best effort after a verified fetch.
pub async fn resolve_governance_document(
    db: &DbStore,
    http: &HttpContext,
    cid: &str,
    gateway_urls: &[String],
) -> GovernanceDocument {
    let normalized_cid = match Cid::from_str(cid.trim()) {
        Ok(cid) => cid.to_string(),
        Err(_) => return GovernanceDocument::unavailable(),
    };
    let Ok(identity) =
        CanonicalBlobMetaIdentity::from_leaf(PROPOSAL_DOCUMENT_BLOB_KIND, &normalized_cid)
    else {
        return GovernanceDocument::unavailable();
    };

    match read_cached_document(db, &identity) {
        Ok(Some(bytes)) => return parse_document(&bytes),
        Ok(None) => {}
        Err(_) => return GovernanceDocument::unavailable(),
    }

    let gateways: Vec<Url> = gateway_urls
        .iter()
        .filter_map(|gateway| Url::parse(gateway.trim()).ok())
        .filter(|gateway| matches!(gateway.scheme(), "http" | "https"))
        .collect();
    if gateways.is_empty() {
        return GovernanceDocument::unavailable();
    }

    let bytes = match sync_service::fetch_verified_cid_with_pool(
        &http.client,
        &gateways,
        &normalized_cid,
        http.gateway_pool(),
    )
    .await
    {
        Ok(bytes) if (bytes.len() as u64) <= MAX_PROPOSAL_DOCUMENT_BYTES => bytes,
        _ => return GovernanceDocument::unavailable(),
    };
    let _ = db.replace_blob_file_atomic(PROPOSAL_DOCUMENT_BLOB_KIND, &normalized_cid, &bytes);

    parse_document(&bytes)
}

fn read_cached_document(
    db: &DbStore,
    identity: &CanonicalBlobMetaIdentity,
) -> Result<Option<Vec<u8>>, std::io::Error> {
    let Some(file) = db
        .open_blob_meta_file(identity)
        .map_err(|error| std::io::Error::other(error.to_string()))?
    else {
        return Ok(None);
    };
    let length = file.metadata()?.len();
    if length > MAX_PROPOSAL_DOCUMENT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cached proposal document exceeds size limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROPOSAL_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if (bytes.len() as u64) > MAX_PROPOSAL_DOCUMENT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cached proposal document exceeds size limit",
        ));
    }
    Ok(Some(bytes))
}

fn parse_document(bytes: &[u8]) -> GovernanceDocument {
    match serde_json::from_slice::<GovernanceDocumentWire>(bytes) {
        Ok(document) => GovernanceDocument {
            title: document.title,
            description: document.description,
            available: true,
        },
        Err(_) => GovernanceDocument::unavailable(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use cid::multihash::Multihash;
    use local_db::{DbConfig, DbStore};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::build_http_client;

    static TEMP_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_root() -> std::path::PathBuf {
        let base = std::env::temp_dir().join("railoxide-governance-document-tests");
        fs::create_dir_all(&base).expect("create temp db directory");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let id = TEMP_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        base.join(format!("db-{}-{now}-{id}", std::process::id()))
    }

    fn raw_cid(bytes: &[u8]) -> Cid {
        let digest = Sha256::digest(bytes);
        Cid::new_v1(
            0x55,
            Multihash::<64>::wrap(0x12, &digest).expect("sha256 multihash"),
        )
    }

    struct OneShotServer {
        url: Url,
        thread: thread::JoinHandle<()>,
    }

    fn one_shot_server(body: Vec<u8>) -> OneShotServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
        let address = listener.local_addr().expect("server address");
        let thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0_u8; 4096];
            let mut received = 0;
            loop {
                let read = std::io::Read::read(&mut stream, &mut request[received..])
                    .expect("read request");
                received += read;
                if request[..received]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
                {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).expect("headers");
            std::io::Write::write_all(&mut stream, &body).expect("body");
        });
        OneShotServer {
            url: Url::parse(&format!("http://{address}")).expect("server URL"),
            thread,
        }
    }

    async fn resolve_with_server(
        document: &[u8],
    ) -> (
        GovernanceDocument,
        DbStore,
        OneShotServer,
        std::path::PathBuf,
    ) {
        let cid = raw_cid(document);
        let server = one_shot_server(document.to_vec());
        let root = temp_db_root();
        let db = DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .expect("open db");
        let http = build_http_client(None).expect("http context");
        let gateways = vec![server.url.to_string()];
        let result = resolve_governance_document(&db, &http, &cid.to_string(), &gateways).await;
        (result, db, server, root)
    }

    #[tokio::test]
    async fn valid_document_is_verified_and_cached_then_served_without_network() {
        let document = br#"{"title":"A proposal","description":"Details"}"#;
        let (result, db, server, root) = resolve_with_server(document).await;
        assert_eq!(result.title, "A proposal");
        assert_eq!(
            result,
            GovernanceDocument {
                title: "A proposal".to_string(),
                description: "Details".to_string(),
                available: true,
            }
        );
        let cid = raw_cid(document);
        let identity =
            CanonicalBlobMetaIdentity::from_leaf(PROPOSAL_DOCUMENT_BLOB_KIND, &cid.to_string())
                .expect("cache identity");
        let mut file = db
            .open_blob_meta_file(&identity)
            .expect("open cache")
            .expect("cache exists");
        let mut cached = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut cached).expect("read cache");
        assert_eq!(cached, document);
        server.thread.join().expect("server thread");

        let http = build_http_client(None).expect("http context");
        let no_gateways = Vec::new();
        assert_eq!(
            resolve_governance_document(&db, &http, &cid.to_string(), &no_gateways).await,
            result
        );
        drop(db);
        fs::remove_dir_all(root).expect("remove test db");
    }

    #[tokio::test]
    async fn invalid_cid_returns_placeholder() {
        let root = temp_db_root();
        let db = DbStore::open(DbConfig {
            root_dir: root.clone(),
        })
        .expect("open db");
        let http = build_http_client(None).expect("http context");
        let result = resolve_governance_document(&db, &http, "not a cid", &[]).await;
        assert!(!result.available);
        assert_eq!(result.title, "Document unavailable");
        drop(db);
        fs::remove_dir_all(root).expect("remove test db");
    }

    #[tokio::test]
    async fn verified_malformed_document_is_cached_and_returns_placeholder() {
        let document = br#"{"title":7,"description":false}"#;
        let (result, db, server, root) = resolve_with_server(document).await;
        assert!(!result.available);
        let cid = raw_cid(document);
        server.thread.join().expect("server thread");
        let http = build_http_client(None).expect("http context");
        assert!(
            !resolve_governance_document(&db, &http, &cid.to_string(), &[])
                .await
                .available
        );
        drop(db);
        fs::remove_dir_all(root).expect("remove test db");
    }
}
