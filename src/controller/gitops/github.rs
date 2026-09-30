//! Rate-limit-aware GitHub API client for the GitOps deployment engine.
//!
//! The client talks to the GitHub REST API to:
//! - Resolve the head commit SHA of the tracking branch ([`GitHubClient::fetch_latest_commit`])
//! - List the manifest directory tree ([`GitHubClient::fetch_directory`])
//! - Download raw manifest file contents ([`GitHubClient::fetch_raw_url`])
//!
//! # Rate limits & network partitions
//!
//! The engine must never thrash cluster state when GitHub is unhappy, so the
//! client:
//! - Tracks `X-RateLimit-*` response headers and exposes
//!   [`GitHubClient::rate_limit_backoff`] so callers can pause polling until
//!   the quota resets instead of hammering the API (and never touching the
//!   cluster while paused).
//! - Uses `ETag` / `If-None-Match` conditional requests. `304 Not Modified`
//!   responses do **not** count against the GitHub API rate limit, so an
//!   unchanged repository costs nothing.
//! - Translates transport failures into [`Error::NetworkError`] so the
//!   polling loop can back off exponentially while keeping the last-known
//!   good cluster state.

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tracing::debug;

use crate::error::{Error, Result};

/// Default GitHub REST API base URL.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Default raw-content base URL.
pub const DEFAULT_RAW_BASE: &str = "https://raw.githubusercontent.com";

/// HTTP timeout for GitHub API calls. A network partition must fail fast so
/// the engine keeps its polling cadence without piling up requests.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Outcome of a conditional fetch.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetch<T> {
    /// The resource changed (or was never fetched) — payload attached.
    Fresh(T),
    /// `304 Not Modified`: resource unchanged since the last fetch.
    NotModified,
}

impl<T> Fetch<T> {
    /// Return the payload if fresh, or `None` on `NotModified`.
    pub fn fresh(self) -> Option<T> {
        match self {
            Fetch::Fresh(v) => Some(v),
            Fetch::NotModified => None,
        }
    }
}

/// Snapshot of GitHub API rate-limit headers.
///
/// `remaining` defaults to [`i64::MAX`] so that an absent header never
/// triggers a (spurious) backoff.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitState {
    /// Total requests allowed in the current window (`X-RateLimit-Limit`).
    pub limit: i64,
    /// Requests left in the current window (`X-RateLimit-Remaining`).
    pub remaining: i64,
    /// Unix seconds when the window resets (`X-RateLimit-Reset`).
    pub reset_at: u64,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self {
            limit: -1,
            remaining: i64::MAX,
            reset_at: 0,
        }
    }
}

impl RateLimitState {
    /// Extract rate-limit state from response headers, if present.
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Option<Self> {
        let parse_i64 = |key: &str| -> Option<i64> {
            headers
                .get(key)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
        };
        let parse_u64 = |key: &str| -> Option<u64> {
            headers
                .get(key)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
        };

        let remaining = parse_i64("x-ratelimit-remaining")?;
        let reset_at = parse_u64("x-ratelimit-reset").unwrap_or(0);
        let limit = parse_i64("x-ratelimit-limit").unwrap_or(-1);

        Some(Self {
            limit,
            remaining,
            reset_at,
        })
    }

    /// How long the caller should wait before retrying, if at all.
    ///
    /// Returns `Some(wait)` only when the quota is exhausted and the reset
    /// time is still in the future relative to `now` (unix seconds).
    pub fn backoff(&self, now: u64) -> Option<Duration> {
        if self.remaining <= 0 && self.reset_at > now {
            Some(Duration::from_secs(self.reset_at - now + 1))
        } else {
            None
        }
    }
}

/// A single entry of a GitHub contents directory listing.
#[derive(Debug, Clone, Deserialize)]
pub struct ContentEntry {
    /// Base name, e.g. `validator`.
    pub name: String,
    /// Full path within the repository, e.g. `clusters/prod/validator`.
    pub path: String,
    /// `file`, `dir`, `symlink` or `submodule`.
    #[serde(rename = "type")]
    pub entry_type: String,
    /// Absolute URL for the raw content (present for files).
    #[serde(default)]
    pub download_url: Option<String>,
}

impl ContentEntry {
    /// `true` when the entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.entry_type == "dir"
    }

    /// `true` when the entry is a regular file.
    pub fn is_file(&self) -> bool {
        self.entry_type == "file"
    }
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
}

/// Internal mutable client state (rate limit + ETag cache).
#[derive(Default)]
struct Inner {
    rate_limit: RateLimitState,
    /// Cached ETags keyed by logical resource (e.g. `commit:owner/repo@main`).
    etags: std::collections::BTreeMap<String, String>,
}

/// GitHub API client with rate-limit tracking and ETag-based caching.
pub struct GitHubClient {
    http: reqwest::Client,
    api_base: String,
    raw_base: String,
    token: Option<String>,
    inner: Mutex<Inner>,
}

/// Raw outcome of a conditional `GET`: body + ETag, or `304`.
enum ApiGet {
    NotModified,
    Ok { body: String, etag: Option<String> },
}

impl GitHubClient {
    /// Create a client. `api_base` / `raw_base` default to github.com and can
    /// be overridden (used by integration tests against a mock server).
    pub fn new(
        token: Option<String>,
        api_base: Option<String>,
        raw_base: Option<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(concat!("stellar-k8s-gitops/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::ConfigError(format!("Failed to build GitHub HTTP client: {e}")))?;

        Ok(Self {
            http,
            api_base: api_base.unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            raw_base: raw_base.unwrap_or_else(|| DEFAULT_RAW_BASE.to_string()),
            token,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Current rate-limit snapshot.
    pub fn rate_limit(&self) -> RateLimitState {
        self.inner.lock().unwrap().rate_limit.clone()
    }

    /// Wait duration before the next GitHub call, if rate limited.
    pub fn rate_limit_backoff(&self, now: u64) -> Option<Duration> {
        self.inner.lock().unwrap().rate_limit.backoff(now)
    }

    /// Resolve the head commit SHA of `branch`, or `NotModified` when the
    /// branch has not moved since the previous call (ETag cached).
    pub async fn fetch_latest_commit(&self, repo: &str, branch: &str) -> Result<Fetch<String>> {
        let path = format!("/repos/{repo}/commits/{branch}");
        let cache_key = format!("commit:{repo}@{branch}");

        match self.api_get(&path, Some(&cache_key)).await? {
            ApiGet::NotModified => Ok(Fetch::NotModified),
            ApiGet::Ok { body, etag } => {
                let commit: CommitResponse = serde_json::from_str(&body)
                    .map_err(|e| Error::GitOpsError(format!("Invalid commit response: {e}")))?;
                if let Some(etag) = etag {
                    self.inner.lock().unwrap().etags.insert(cache_key, etag);
                }
                Ok(Fetch::Fresh(commit.sha))
            }
        }
    }

    /// List the contents of `path` on `branch`. Returns `NotModified` when the
    /// directory tree is unchanged since the previous call.
    pub async fn fetch_directory(
        &self,
        repo: &str,
        branch: &str,
        path: &str,
    ) -> Result<Fetch<Vec<ContentEntry>>> {
        let path_enc = path.trim_matches('/');
        let url_path = format!("/repos/{repo}/contents/{path_enc}?ref={branch}");
        let cache_key = format!("contents:{repo}@{branch}:{path_enc}");

        match self.api_get(&url_path, Some(&cache_key)).await? {
            ApiGet::NotModified => Ok(Fetch::NotModified),
            ApiGet::Ok { body, etag } => {
                let entries: Vec<ContentEntry> = serde_json::from_str(&body).map_err(|e| {
                    Error::GitOpsError(format!("Invalid contents response for {path}: {e}"))
                })?;
                if let Some(etag) = etag {
                    self.inner.lock().unwrap().etags.insert(cache_key, etag);
                }
                Ok(Fetch::Fresh(entries))
            }
        }
    }

    /// Fetch a raw file from an absolute URL (e.g. a `download_url`).
    pub async fn fetch_raw_url(&self, url: &str) -> Result<String> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| Error::NetworkError(format!("raw fetch failed for {url}: {e}")))?;

        if !resp.status().is_success() {
            return Err(Error::GitOpsError(format!(
                "raw fetch for {url} returned HTTP {}",
                resp.status()
            )));
        }

        resp.text()
            .await
            .map_err(|e| Error::NetworkError(format!("raw body read failed: {e}")))
    }

    /// Fetch a raw file from `https://raw.githubusercontent.com/{repo}/{branch}/{path}`.
    pub async fn fetch_raw(&self, repo: &str, branch: &str, path: &str) -> Result<String> {
        let url = format!(
            "{}/{}/{}/{}",
            self.raw_base.trim_end_matches('/'),
            repo,
            branch,
            path.trim_start_matches('/')
        );
        self.fetch_raw_url(&url).await
    }

    /// Perform a conditional GET against the API, updating rate-limit state.
    async fn api_get(&self, path: &str, etag_key: Option<&str>) -> Result<ApiGet> {
        let url = format!("{}{}", self.api_base.trim_end_matches('/'), path);
        let mut req = self.http.get(&url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        if let Some(key) = etag_key {
            let cached = self.inner.lock().unwrap().etags.get(key).cloned();
            if let Some(etag) = cached {
                req = req.header(reqwest::header::IF_NONE_MATCH, etag);
            }
        }

        let resp = req
            .send()
            .await
            .map_err(|e| Error::NetworkError(format!("GitHub API unreachable ({path}): {e}")))?;

        // Track quota on every response, including errors.
        if let Some(rl) = RateLimitState::from_headers(resp.headers()) {
            debug!(
                remaining = rl.remaining,
                reset_at = rl.reset_at,
                "GitHub rate limit"
            );
            self.inner.lock().unwrap().rate_limit = rl;
        }

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(ApiGet::NotModified);
        }

        if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            let remaining = self.inner.lock().unwrap().rate_limit.remaining;
            if remaining <= 0 {
                // Quota exhausted: the caller must pause until reset. No
                // retries here — retrying would only burn the quota faster.
                return Err(Error::GitOpsError(format!(
                    "GitHub API rate limit exhausted (resets in {}s)",
                    self.inner
                        .lock()
                        .unwrap()
                        .rate_limit
                        .reset_at
                        .saturating_sub(unix_now())
                )));
            }
        }

        if !status.is_success() {
            return Err(Error::GitOpsError(format!(
                "GitHub API returned HTTP {status} for {path}"
            )));
        }

        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let body = resp
            .text()
            .await
            .map_err(|e| Error::NetworkError(format!("GitHub body read failed: {e}")))?;

        Ok(ApiGet::Ok { body, etag })
    }
}

/// Current unix time in seconds.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn rate_limit_from_headers_parses_all_fields() {
        let rl = RateLimitState::from_headers(&headers(&[
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-remaining", "42"),
            ("x-ratelimit-reset", "1759000000"),
        ]))
        .expect("headers present");

        assert_eq!(rl.limit, 5000);
        assert_eq!(rl.remaining, 42);
        assert_eq!(rl.reset_at, 1_759_000_000);
    }

    #[test]
    fn rate_limit_missing_headers_is_none() {
        assert!(RateLimitState::from_headers(&HeaderMap::new()).is_none());
    }

    #[test]
    fn rate_limit_default_never_backs_off() {
        let rl = RateLimitState::default();
        assert!(rl.backoff(1_000).is_none());
    }

    #[test]
    fn rate_limit_backoff_when_exhausted() {
        let rl = RateLimitState {
            limit: 60,
            remaining: 0,
            reset_at: 1_000,
        };
        let wait = rl.backoff(990).expect("should back off");
        assert_eq!(wait, Duration::from_secs(11));

        // After the reset time there is nothing to wait for.
        assert!(rl.backoff(1_000).is_none());
        assert!(rl.backoff(2_000).is_none());
    }

    #[test]
    fn rate_limit_remaining_positive_no_backoff() {
        let rl = RateLimitState {
            limit: 60,
            remaining: 1,
            reset_at: u64::MAX,
        };
        assert!(rl.backoff(0).is_none());
    }

    #[test]
    fn fetch_enum_helpers() {
        assert_eq!(Fetch::Fresh(7u8).fresh(), Some(7));
        let nm: Fetch<u8> = Fetch::NotModified;
        assert_eq!(nm.fresh(), None);
    }

    #[test]
    fn client_builds_with_custom_base() {
        let client = GitHubClient::new(Some("t".into()), Some("http://localhost:1".into()), None);
        assert!(client.is_ok());
        let client = client.unwrap();
        assert_eq!(client.api_base, "http://localhost:1");
        assert_eq!(client.raw_base, DEFAULT_RAW_BASE);
        assert_eq!(client.rate_limit(), RateLimitState::default());
    }

    #[tokio::test]
    async fn fetch_latest_commit_bad_json_is_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/o/r/commits/main"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = GitHubClient::new(None, Some(server.uri()), None).expect("client builds");
        let result = client.fetch_latest_commit("o/r", "main").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_latest_commit_rate_limited_403_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/o/r/commits/main"))
            .respond_with(
                wiremock::ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", "1759000000"),
            )
            .mount(&server)
            .await;

        let client = GitHubClient::new(None, Some(server.uri()), None).expect("client builds");
        let err = client
            .fetch_latest_commit("o/r", "main")
            .await
            .expect_err("rate limited");
        assert!(err.to_string().contains("rate limit exhausted"));

        // The backoff is now active until the reset time.
        let rl = client.rate_limit();
        assert_eq!(rl.remaining, 0);
    }

    #[tokio::test]
    async fn fetch_latest_commit_etag_304_not_modified() {
        use wiremock::matchers::{method, path};

        let server = wiremock::MockServer::start().await;
        // First response carries an ETag; second request must send
        // If-None-Match and get a 304 back.
        wiremock::Mock::given(method("GET"))
            .and(path("/repos/o/r/commits/main"))
            .respond_with(|req: &wiremock::Request| {
                let conditional = req
                    .headers
                    .get("if-none-match")
                    .map(|v| v.to_str().unwrap_or_default())
                    == Some("\"abc\"");
                if conditional {
                    wiremock::ResponseTemplate::new(304)
                } else {
                    wiremock::ResponseTemplate::new(200)
                        .set_body_string(r#"{"sha":"cafe123"}"#)
                        .insert_header("etag", "\"abc\"")
                }
            })
            .mount(&server)
            .await;

        let client = GitHubClient::new(None, Some(server.uri()), None).expect("client builds");

        let first = client.fetch_latest_commit("o/r", "main").await.unwrap();
        assert_eq!(first, Fetch::Fresh("cafe123".to_string()));

        let second = client.fetch_latest_commit("o/r", "main").await.unwrap();
        assert_eq!(second, Fetch::NotModified);
    }

    #[tokio::test]
    async fn fetch_directory_parses_entries() {
        use wiremock::matchers::{method, path, query_param};

        let body = r#"[
            {"name":"validator","path":"clusters/prod/validator","type":"dir","download_url":null},
            {"name":"horizon.horizon.env","path":"clusters/prod/horizon/horizon.env","type":"file","download_url":"http://raw/x"}
        ]"#;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/repos/o/r/contents/clusters/prod"))
            .and(query_param("ref", "main"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = GitHubClient::new(None, Some(server.uri()), None).expect("client builds");
        let entries = client
            .fetch_directory("o/r", "main", "clusters/prod")
            .await
            .unwrap();
        let entries = entries.fresh().expect("fresh");
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir());
        assert!(entries[1].is_file());
        assert_eq!(entries[1].name, "horizon.horizon.env");
    }

    #[tokio::test]
    async fn fetch_raw_url_success_and_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/raw/good.cfg"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_string("LOG_LEVEL=\"info\""),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/raw/bad.cfg"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = GitHubClient::new(None, Some(server.uri()), None).expect("client builds");

        let ok = client
            .fetch_raw_url(&format!("{}/raw/good.cfg", server.uri()))
            .await
            .unwrap();
        assert!(ok.contains("LOG_LEVEL"));

        let err = client
            .fetch_raw_url(&format!("{}/raw/bad.cfg", server.uri()))
            .await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn network_partition_maps_to_network_error() {
        // Port 1 is guaranteed closed — simulates a network partition.
        let client = GitHubClient::new(None, Some("http://127.0.0.1:1".into()), None)
            .expect("client builds");
        let err = client.fetch_latest_commit("o/r", "main").await.unwrap_err();
        assert!(matches!(err, Error::NetworkError(_)));
    }
}
