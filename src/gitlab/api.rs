//! The slice of the GitLab merge-request API a rolling update needs, behind
//! one retry policy.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};

use super::Error;

/// Longest excerpt of a GitLab error body quoted in a message.
const BODY_EXCERPT: usize = 500;

/// An open merge request as the run needs to see it.
#[derive(Debug, Clone)]
pub struct MergeRequest {
    pub iid: u64,
    pub web_url: String,
    pub raw: Value,
}

/// How often and how long to wait when GitLab or the network cannot answer.
///
/// Rate limits (429) are retried for every method, because GitLab refuses
/// them before doing any work. Gateway errors (502, 503, 504) and transport
/// failures are retried only for GET and PUT, which repeat safely; a POST is
/// otherwise retried only when the connection was never established, so a
/// merge request is never created twice.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 5,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
        }
    }
}

impl Retry {
    /// Full-jitter exponential backoff before retry number `retry` (1-based).
    fn backoff(&self, retry: u32) -> Duration {
        let ceiling = self
            .base
            .saturating_mul(2u32.saturating_pow(retry.saturating_sub(1)))
            .min(self.cap);
        jitter(ceiling)
    }

    /// The wait before retry number `retry`: the server's `Retry-After`
    /// seconds when it sent them, bounded by the cap, else the backoff.
    fn delay(&self, retry: u32, headers: Option<&HeaderMap>) -> Duration {
        headers
            .and_then(retry_after)
            .map(|wait| wait.min(self.cap))
            .unwrap_or_else(|| self.backoff(retry))
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    token: String,
    merge_requests: String,
    retry: Retry,
}

/// A GitLab answer after retries: its status and body text.
struct Answer {
    status: StatusCode,
    url: String,
    body: String,
}

impl Client {
    pub fn new(api_url: &str, project_id: &str, token: &str) -> Result<Self, Error> {
        let builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")));
        let http = crate::http::apply(builder)
            .build()
            .map_err(|error| Error::Io(format!("cannot build the GitLab client: {error}")))?;
        let api_url = api_url.trim_end_matches('/');
        Ok(Self {
            http,
            token: token.to_string(),
            merge_requests: format!("{api_url}/projects/{project_id}/merge_requests"),
            retry: Retry::default(),
        })
    }

    #[cfg(test)]
    fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    /// Open merge requests from `source` into `target`.
    pub async fn open_merge_requests(
        &self,
        source: &str,
        target: &str,
    ) -> Result<Vec<Value>, Error> {
        let mut url = self.url(&self.merge_requests)?;
        url.query_pairs_mut()
            .append_pair("state", "opened")
            .append_pair("source_branch", source)
            .append_pair("target_branch", target);
        match self.send(Method::GET, url, None).await? {
            Value::Array(items) => Ok(items),
            other => Err(Error::Refused(format!(
                "GitLab returned a merge-request list that is not a list: {}",
                excerpt(&other.to_string())
            ))),
        }
    }

    pub async fn create(
        &self,
        source: &str,
        target: &str,
        title: &str,
        description: &str,
    ) -> Result<Value, Error> {
        let body = json!({
            "source_branch": source,
            "target_branch": target,
            "title": title,
            "remove_source_branch": true,
            "description": description,
        });
        let url = self.url(&self.merge_requests)?;
        self.send(Method::POST, url, Some(&body)).await
    }

    pub async fn edit(&self, iid: u64, body: Value) -> Result<Value, Error> {
        let url = self.merge_request_url(&iid.to_string())?;
        self.send(Method::PUT, url, Some(&body)).await
    }

    /// Asks GitLab to merge once the pipeline for exactly `sha` succeeds.
    pub async fn enable_auto_merge(&self, iid: u64, sha: &str) -> Result<(), Error> {
        let body = json!({
            "auto_merge": true,
            "sha": sha,
            "should_remove_source_branch": true,
        });
        let url = self.merge_request_url(&format!("{iid}/merge"))?;
        self.send(Method::PUT, url, Some(&body)).await.map(drop)
    }

    pub async fn cancel_auto_merge(&self, iid: u64) -> Result<(), Error> {
        let url = self.merge_request_url(&format!("{iid}/cancel_merge_when_pipeline_succeeds"))?;
        self.send(Method::POST, url, None).await.map(drop)
    }

    fn url(&self, text: &str) -> Result<Url, Error> {
        Url::parse(text)
            .map_err(|error| Error::Input(format!("invalid GitLab API URL {text}: {error}")))
    }

    fn merge_request_url(&self, suffix: &str) -> Result<Url, Error> {
        self.url(&format!("{}/{suffix}", self.merge_requests))
    }

    async fn send(&self, method: Method, url: Url, body: Option<&Value>) -> Result<Value, Error> {
        parse_json(self.execute(method, url, body).await?)
    }

    /// Sends one request, retrying under [`Retry`]'s rules, and returns the
    /// final answer whatever its status.
    async fn execute(
        &self,
        method: Method,
        url: Url,
        body: Option<&Value>,
    ) -> Result<Answer, Error> {
        let repeatable = matches!(method, Method::GET | Method::PUT);
        let shown = redact_query(&url);
        let mut retry = 0;
        loop {
            retry += 1;
            let last = retry >= self.retry.attempts;
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .header("PRIVATE-TOKEN", &self.token);
            if let Some(body) = body {
                request = request.json(body);
            }
            let failure = match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    let retryable = status == StatusCode::TOO_MANY_REQUESTS
                        || (repeatable
                            && matches!(
                                status,
                                StatusCode::BAD_GATEWAY
                                    | StatusCode::SERVICE_UNAVAILABLE
                                    | StatusCode::GATEWAY_TIMEOUT
                            ));
                    let headers = response.headers().clone();
                    if !retryable || last {
                        let body = response.text().await.map_err(|error| {
                            Error::Network(format!("GitLab response could not be read: {error}"))
                        })?;
                        return Ok(Answer {
                            status,
                            url: shown,
                            body,
                        });
                    }
                    (format!("GitLab answered {status}"), Some(headers))
                }
                Err(error) => {
                    if last || !(repeatable || error.is_connect()) {
                        return Err(Error::Network(format!(
                            "GitLab request to {shown} failed: {error}"
                        )));
                    }
                    (format!("GitLab request failed ({error})"), None)
                }
            };
            let (reason, headers) = failure;
            let wait = self.retry.delay(retry, headers.as_ref());
            eprintln!(
                "{reason} for {method} {shown}; retrying in {:.1}s (attempt {} of {})",
                wait.as_secs_f64(),
                retry + 1,
                self.retry.attempts
            );
            tokio::time::sleep(wait).await;
        }
    }
}

fn check_status(answer: &Answer) -> Result<(), Error> {
    let status = answer.status;
    if status.is_success() {
        return Ok(());
    }
    let message = format!(
        "GitLab answered {status} for {}: {}",
        answer.url,
        excerpt(&answer.body)
    );
    Err(
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            Error::Network(message)
        } else {
            Error::Api(message)
        },
    )
}

fn parse_json(answer: Answer) -> Result<Value, Error> {
    check_status(&answer)?;
    if answer.body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&answer.body).map_err(|error| {
        Error::Refused(format!(
            "GitLab returned a response that is not JSON ({error}) for {}",
            answer.url
        ))
    })
}

/// `Retry-After` in its delta-seconds form; the HTTP-date form falls back
/// to the backoff.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// A uniformly random duration in `[0, ceiling]`, from a splitmix64 stream
/// seeded by the clock, so concurrent runs do not retry in lockstep.
fn jitter(ceiling: Duration) -> Duration {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or_default();
    let mut z = STATE
        .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
        .wrapping_add(nanos);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let span = ceiling.as_nanos().min(u128::from(u64::MAX)) as u64;
    Duration::from_nanos(z % span.saturating_add(1))
}

impl MergeRequest {
    /// Reads the identity of a merge request from a GitLab response,
    /// refusing one that does not name it.
    pub fn from_response(value: &Value) -> Result<Self, Error> {
        let iid = match &value["iid"] {
            Value::Number(number) => number.as_u64(),
            Value::String(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                text.parse().ok()
            }
            _ => None,
        };
        let web_url = value["web_url"].as_str();
        match (iid, web_url) {
            (Some(iid), Some(web_url)) => Ok(Self {
                iid,
                web_url: web_url.to_string(),
                raw: value.clone(),
            }),
            _ => Err(Error::Refused(
                "GitLab returned an invalid merge-request response".to_string(),
            )),
        }
    }

    /// Whether GitLab currently has auto-merge set for this merge request,
    /// under either the current or the legacy field name.
    pub fn auto_merge_enabled(&self) -> bool {
        let first_set = ["auto_merge_enabled", "merge_when_pipeline_succeeds"]
            .iter()
            .map(|key| &self.raw[*key])
            .find(|value| !matches!(value, Value::Null | Value::Bool(false)));
        matches!(first_set, Some(Value::Bool(true)))
            || first_set.and_then(Value::as_str) == Some("true")
    }
}

fn redact_query(url: &Url) -> String {
    let mut url = url.clone();
    url.set_query(None);
    url.to_string()
}

fn excerpt(text: &str) -> String {
    let text = text.trim();
    let mut out: String = text.chars().take(BODY_EXCERPT).collect();
    if out.len() < text.len() {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FAST: Retry = Retry {
        attempts: 5,
        base: Duration::from_millis(1),
        cap: Duration::from_millis(4),
    };

    async fn client(server: &MockServer) -> Client {
        Client::new(&format!("{}/api/v4", server.uri()), "1", "token")
            .unwrap()
            .with_retry(FAST)
    }

    async fn failing_then_ok(
        server: &MockServer,
        verb: &str,
        route: &str,
        failure: ResponseTemplate,
        failures: u64,
    ) {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(failure)
            .up_to_n_times(failures)
            .with_priority(1)
            .mount(server)
            .await;
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"iid": 7, "web_url": "u"})),
            )
            .with_priority(2)
            .mount(server)
            .await;
    }

    const MR_LIST: &str = "/api/v4/projects/1/merge_requests";
    const MR_7: &str = "/api/v4/projects/1/merge_requests/7";

    #[tokio::test]
    async fn gateway_errors_are_retried_for_repeatable_methods() {
        let server = MockServer::start().await;
        failing_then_ok(&server, "PUT", MR_7, ResponseTemplate::new(503), 2).await;
        let answer = client(&server).await.edit(7, json!({"title": "t"})).await;
        assert!(answer.is_ok(), "{answer:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_post_is_not_repeated_after_a_gateway_error() {
        let server = MockServer::start().await;
        failing_then_ok(&server, "POST", MR_LIST, ResponseTemplate::new(502), 1).await;
        let error = client(&server)
            .await
            .create("b", "main", "t", "d")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), "network_error", "{error}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rate_limits_are_retried_for_every_method() {
        let server = MockServer::start().await;
        let limited = ResponseTemplate::new(429).insert_header("Retry-After", "0");
        failing_then_ok(&server, "POST", MR_LIST, limited, 2).await;
        let created = client(&server).await.create("b", "main", "t", "d").await;
        assert!(created.is_ok(), "{created:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn retries_stop_after_the_attempt_budget() {
        let server = MockServer::start().await;
        failing_then_ok(&server, "GET", MR_LIST, ResponseTemplate::new(504), 100).await;
        let error = client(&server)
            .await
            .open_merge_requests("b", "main")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), "network_error", "{error}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            FAST.attempts as usize
        );
    }

    #[tokio::test]
    async fn client_errors_and_internal_errors_are_not_retried() {
        for status in [401, 404, 500] {
            let server = MockServer::start().await;
            failing_then_ok(&server, "GET", MR_LIST, ResponseTemplate::new(status), 100).await;
            let error = client(&server)
                .await
                .open_merge_requests("b", "main")
                .await
                .unwrap_err();
            assert!(error.exit_code() > 0, "{error}");
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                1,
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn a_refused_connection_is_retried_even_for_a_post() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let started = std::time::Instant::now();
        let error = Client::new(&format!("http://{address}/api/v4"), "1", "token")
            .unwrap()
            .with_retry(Retry {
                attempts: 3,
                base: Duration::from_millis(40),
                cap: Duration::from_millis(40),
            })
            .create("b", "main", "t", "d")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), "network_error", "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "connect retries must use the backoff, not a timeout"
        );
    }

    #[test]
    fn retry_after_seconds_win_and_are_capped() {
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        let retry = Retry {
            attempts: 5,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
        };
        assert_eq!(retry.delay(1, Some(&headers)), Duration::from_secs(3));
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(retry.delay(1, Some(&headers)), Duration::from_secs(60));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert!(retry.delay(1, Some(&headers)) <= Duration::from_secs(1));
    }

    #[test]
    fn backoff_grows_exponentially_up_to_the_cap() {
        let retry = Retry {
            attempts: 10,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
        };
        for (attempt, ceiling) in [(1, 1), (2, 2), (3, 4), (4, 8), (7, 60), (40, 60)] {
            let samples: Vec<Duration> = (0..200).map(|_| retry.backoff(attempt)).collect();
            assert!(
                samples
                    .iter()
                    .all(|wait| *wait <= Duration::from_secs(ceiling))
            );
            assert!(
                samples
                    .iter()
                    .any(|wait| *wait > Duration::from_secs(ceiling) / 2),
                "attempt {attempt} never waited past half its ceiling"
            );
        }
    }

    #[test]
    fn merge_request_identity_accepts_integer_or_digit_iids_only() {
        let url = "https://gitlab.example.test/p/-/merge_requests/7";
        assert_eq!(
            MergeRequest::from_response(&json!({"iid": 7, "web_url": url}))
                .unwrap()
                .iid,
            7
        );
        assert_eq!(
            MergeRequest::from_response(&json!({"iid": "7", "web_url": url}))
                .unwrap()
                .iid,
            7
        );
        for invalid in [
            json!({"iid": null, "web_url": url}),
            json!({"iid": "7/../8", "web_url": url}),
            json!({"iid": -1, "web_url": url}),
            json!({"iid": 7}),
            json!([]),
        ] {
            assert!(MergeRequest::from_response(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn auto_merge_state_reads_the_first_set_field() {
        let state = |raw: Value| {
            let mut raw = raw;
            raw["iid"] = json!(1);
            raw["web_url"] = json!("u");
            MergeRequest::from_response(&raw)
                .unwrap()
                .auto_merge_enabled()
        };
        assert!(state(json!({"auto_merge_enabled": true})));
        assert!(state(json!({"merge_when_pipeline_succeeds": true})));
        assert!(state(
            json!({"auto_merge_enabled": false, "merge_when_pipeline_succeeds": true})
        ));
        assert!(state(json!({"auto_merge_enabled": "true"})));
        assert!(!state(json!({"auto_merge_enabled": "yes"})));
        assert!(!state(
            json!({"auto_merge_enabled": 1, "merge_when_pipeline_succeeds": true})
        ));
        assert!(!state(json!({})));
    }
}
