//! Compares the installed build with the wallet's GitHub releases.

use std::error::Error;
use std::time::Duration;

use semver::Version;
use serde::Deserialize;

use crate::http::HttpContext;

pub const RAILOXIDE_REPOSITORY_URL: &str = "https://github.com/triamazikamno/railoxide";
const REPOSITORY_OWNER: &str = "triamazikamno";
const REPOSITORY_NAME: &str = "railoxide";

/// Covers opening the operation client through the last body chunk. Matches the Tor RPC
/// request timeout.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const UNREADABLE: &str = "GitHub's response was too large or couldn't be read.";

/// The fields the update check uses from a GitHub release. Other fields are ignored.
#[derive(Clone, Debug, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub html_url: String,
    /// RFC 3339 text as GitHub sends it.
    pub published_at: Option<String>,
    pub prerelease: bool,
    pub draft: bool,
}

/// Where the installed build sits in [`ReleaseTimeline::entries`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstalledMarker {
    /// The build equals `entries[i]`.
    At(usize),
    /// The build sits just above `entries[i]`; `entries.len()` means after the last entry.
    Before(usize),
    /// The build version doesn't parse.
    Unknown,
}

#[derive(Clone, Debug)]
pub struct ReleaseTimeline {
    /// Published releases, newest first.
    pub entries: Vec<GithubRelease>,
    pub installed: InstalledMarker,
    /// Entries newer than the build; 0 when the build version doesn't parse.
    pub newer_count: usize,
}

impl ReleaseTimeline {
    #[must_use]
    pub fn latest(&self) -> Option<&GithubRelease> {
        self.entries.first()
    }
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{reason}")]
pub struct ReleaseCheckError {
    pub reason: String,
}

/// Drops drafts, prereleases and tags that aren't semantic versions, orders the rest by
/// precedence, newest first, and places `current`, the build version, among them.
#[must_use]
pub fn classify(current: &str, releases: Vec<GithubRelease>) -> ReleaseTimeline {
    let mut versioned: Vec<(Version, GithubRelease)> = releases
        .into_iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| Some((parse_version(&release.tag_name)?, release)))
        .collect();
    versioned.sort_by(|(left, _), (right, _)| right.cmp_precedence(left));
    let (installed, newer_count) = match parse_version(current) {
        Some(current) => {
            let newer_count = versioned
                .iter()
                .take_while(|(version, _)| version.cmp_precedence(&current).is_gt())
                .count();
            let installed = match versioned.get(newer_count) {
                Some((version, _)) if version.cmp_precedence(&current).is_eq() => {
                    InstalledMarker::At(newer_count)
                }
                _ => InstalledMarker::Before(newer_count),
            };
            (installed, newer_count)
        }
        None => (InstalledMarker::Unknown, 0),
    };
    ReleaseTimeline {
        entries: versioned.into_iter().map(|(_, release)| release).collect(),
        installed,
        newer_count,
    }
}

fn parse_version(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// Fetches the repository's latest 100 releases on a fresh operation client and classifies
/// them against `current`, the build version.
///
/// # Errors
///
/// Fails with a display reason when the deadline passes, the request fails, GitHub answers
/// with an error status, or the body is too large or doesn't decode.
pub async fn check_releases(
    http: &HttpContext,
    current: &str,
) -> Result<ReleaseTimeline, ReleaseCheckError> {
    let url = format!(
        "https://api.github.com/repos/{REPOSITORY_OWNER}/{REPOSITORY_NAME}/releases?per_page=100"
    );
    check_releases_at(http, &url, current).await
}

async fn check_releases_at(
    http: &HttpContext,
    url: &str,
    current: &str,
) -> Result<ReleaseTimeline, ReleaseCheckError> {
    let releases = tokio::time::timeout(CHECK_TIMEOUT, fetch_releases(http, url))
        .await
        .map_err(|_| failed("The request timed out."))??;
    Ok(classify(current, releases))
}

async fn fetch_releases(
    http: &HttpContext,
    url: &str,
) -> Result<Vec<GithubRelease>, ReleaseCheckError> {
    // Dropped on return, which stops the operation's Tor bridge.
    let operation = http
        .operation_http_client()
        .await
        .map_err(|error| error_chain(&*error))?;
    let mut response = operation
        .client()
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        // GitHub rejects requests without a User-Agent; a generic one doesn't name the app.
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0")
        .send()
        .await
        .map_err(|error| error_chain(&error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(failed(&format!("GitHub returned {status}.")));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| error_chain(&error.without_url()))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(failed(UNREADABLE));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| failed(UNREADABLE))
}

/// `reqwest` puts the underlying cause, such as a proxy or TLS failure, in the source chain
/// rather than in its own message.
fn error_chain(error: &(dyn Error + 'static)) -> ReleaseCheckError {
    let reason = std::iter::successors(Some(error), |&error| error.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    ReleaseCheckError { reason }
}

fn failed(reason: &str) -> ReleaseCheckError {
    ReleaseCheckError {
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::mpsc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn release(tag: &str, prerelease: bool, draft: bool) -> GithubRelease {
        GithubRelease {
            tag_name: tag.to_owned(),
            name: None,
            body: None,
            html_url: format!("{RAILOXIDE_REPOSITORY_URL}/releases/tag/{tag}"),
            published_at: None,
            prerelease,
            draft,
        }
    }

    #[test]
    fn classify_keeps_published_releases_and_places_the_build() {
        let releases = || {
            vec![
                release("v0.2.0-beta.10", false, false),
                release("v0.2.0-beta.12", true, false),
                release("v0.2.0-beta.13", false, false),
                release("v0.2.0-beta.11", true, false),
                release("v0.2.0-beta.14", false, true),
                release("nightly", false, false),
            ]
        };
        for (current, installed, newer_count) in [
            ("0.2.0-beta.10", InstalledMarker::At(1), 1),
            // Flagged as a prerelease on GitHub, so it isn't listed.
            ("v0.2.0-beta.12", InstalledMarker::Before(1), 1),
            ("0.2.0", InstalledMarker::Before(0), 0),
            ("0.1.0", InstalledMarker::Before(2), 2),
            ("not-a-version", InstalledMarker::Unknown, 0),
        ] {
            let timeline = classify(current, releases());
            let tags: Vec<_> = timeline
                .entries
                .iter()
                .map(|release| release.tag_name.as_str())
                .collect();
            assert_eq!(tags, ["v0.2.0-beta.13", "v0.2.0-beta.10"], "{current}");
            assert_eq!(
                timeline.latest().map(|release| release.tag_name.as_str()),
                Some("v0.2.0-beta.13")
            );
            assert_eq!(timeline.installed, installed, "{current}");
            assert_eq!(timeline.newer_count, newer_count, "{current}");
        }
    }

    /// Serves one request with `response`, reports once it is written, and then holds the
    /// connection open without finishing the body.
    async fn serve_stalled(response: Vec<u8>) -> (String, mpsc::Receiver<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind release mock");
        let url = format!(
            "http://{}/releases",
            listener.local_addr().expect("release mock address")
        );
        let (written, written_rx) = mpsc::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept release check");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.expect("read request");
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            if stream.write_all(&response).await.is_ok() {
                let _ = written.send(());
            }
            std::future::pending::<()>().await;
        });
        (url, written_rx)
    }

    #[tokio::test]
    async fn oversized_body_fails_without_waiting_for_the_rest() {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n",
            2 * MAX_RESPONSE_BYTES
        )
        .into_bytes();
        response.resize(response.len() + MAX_RESPONSE_BYTES + 1, b' ');
        let (url, _) = serve_stalled(response).await;

        let error = check_releases_at(&HttpContext::direct_for_tests(), &url, "0.1.0")
            .await
            .expect_err("oversized body fails");
        assert_eq!(error.reason, UNREADABLE);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_body_times_out() {
        let (url, headers_written) =
            serve_stalled(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n[".to_vec()).await;
        // A running blocking task stops paused time from jumping to the deadline before the
        // client has read the headers, so the deadline has to cover the body read.
        let hold_clock = tokio::task::spawn_blocking(move || {
            headers_written
                .recv_timeout(Duration::from_secs(10))
                .expect("server wrote headers");
            std::thread::sleep(Duration::from_millis(200));
        });

        let http = HttpContext::direct_for_tests();
        let check = check_releases_at(&http, &url, "0.1.0");
        let error = tokio::time::timeout(2 * CHECK_TIMEOUT, check)
            .await
            .expect("check ends at its own deadline")
            .expect_err("stalled body fails");
        assert_eq!(error.reason, "The request timed out.");
        hold_clock.await.expect("clock hold ends");
    }
}
