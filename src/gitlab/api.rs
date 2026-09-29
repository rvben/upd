//! The slice of the GitLab API the automation needs: merge requests, group
//! project listing and repository file reads, behind one retry policy.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};

use super::Error;

/// Longest excerpt of a GitLab error body quoted in a message.
const BODY_EXCERPT: usize = 500;

/// Most group-project pages followed before discovery gives up; at 100 per
/// page that is far past any real group, and it bounds a server that keeps
/// answering with a next page.
const MAX_PAGES: u32 = 1000;

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
    /// How long to wait for GitLab to finish processing a push. GitLab
    /// records a pushed branch and rechecks the merge requests it heads in
    /// background jobs, so for a while after a push it can still report the
    /// branch missing or a merge request unchecked; ten attempts backing off
    /// to 15 seconds wait up to about a minute and a half for that.
    const SETTLE: Self = Self {
        attempts: 10,
        base: Duration::from_secs(1),
        cap: Duration::from_secs(15),
    };

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
    api_url: String,
    merge_requests: String,
    retry: Retry,
    settle: Retry,
}

/// A GitLab answer after retries: its status, headers and body text.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    url: String,
    body: String,
}

impl Client {
    /// A client for group-level calls; [`Client::for_project`] addresses a
    /// project's merge requests.
    pub fn new(api_url: &str, token: &str) -> Result<Self, Error> {
        let builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")));
        let http = crate::http::apply(builder)
            .build()
            .map_err(|error| Error::Io(format!("cannot build the GitLab client: {error}")))?;
        let api_url = api_url.trim_end_matches('/').to_string();
        Ok(Self {
            http,
            token: token.to_string(),
            merge_requests: String::new(),
            api_url,
            retry: Retry::default(),
            settle: Retry::SETTLE,
        })
    }

    /// The same connection and credentials, addressing another project.
    pub fn for_project(&self, project_id: impl std::fmt::Display) -> Self {
        Self {
            merge_requests: format!("{}/projects/{project_id}/merge_requests", self.api_url),
            ..self.clone()
        }
    }

    #[cfg(test)]
    fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    #[cfg(test)]
    fn with_settle(mut self, settle: Retry) -> Self {
        self.settle = settle;
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
        let mut attempt = 0;
        loop {
            attempt += 1;
            let answer = self.execute(Method::POST, url.clone(), Some(&body)).await?;
            if !source_branch_missing(&answer) {
                return parse_json(answer);
            }
            if attempt >= self.settle.attempts {
                return Err(Error::Network(format!(
                    "GitLab still does not list the pushed branch {source}; the merge request was not opened"
                )));
            }
            // A refused POST created nothing, so asking again is safe.
            let wait = self.settle.backoff(attempt);
            eprintln!(
                "GitLab does not list the pushed branch {source} yet; retrying in {:.1}s (attempt {} of {})",
                wait.as_secs_f64(),
                attempt + 1,
                self.settle.attempts
            );
            tokio::time::sleep(wait).await;
        }
    }

    pub async fn edit(&self, iid: u64, body: Value) -> Result<Value, Error> {
        let url = self.merge_request_url(&iid.to_string())?;
        self.send(Method::PUT, url, Some(&body)).await
    }

    /// Asks GitLab to merge once the pipeline for exactly `sha` succeeds,
    /// after GitLab has caught up with the push of `sha`: until the merge
    /// request heads that commit and its mergeability is checked, GitLab
    /// refuses to arm with a stale-SHA 409 or a 422 that reads as a conflict.
    pub async fn enable_auto_merge(&self, iid: u64, sha: &str) -> Result<(), Error> {
        self.wait_until_checked(iid, sha).await?;
        let body = json!({
            "auto_merge": true,
            "sha": sha,
            "should_remove_source_branch": true,
        });
        let url = self.merge_request_url(&format!("{iid}/merge"))?;
        self.send(Method::PUT, url, Some(&body)).await.map(drop)
    }

    async fn wait_until_checked(&self, iid: u64, sha: &str) -> Result<(), Error> {
        let url = self.merge_request_url(&iid.to_string())?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let merge_request = self.send(Method::GET, url.clone(), None).await?;
            let Some(pending) = unsettled(&merge_request, sha) else {
                return Ok(());
            };
            if attempt >= self.settle.attempts {
                return Err(Error::Network(format!(
                    "GitLab has not caught up with the push of {sha} to merge request !{iid}: {pending}; auto-merge was not requested"
                )));
            }
            let wait = self.settle.backoff(attempt);
            eprintln!(
                "Merge request !{iid} is not ready for auto-merge ({pending}); checking again in {:.1}s (attempt {} of {})",
                wait.as_secs_f64(),
                attempt + 1,
                self.settle.attempts
            );
            tokio::time::sleep(wait).await;
        }
    }

    pub async fn cancel_auto_merge(&self, iid: u64) -> Result<(), Error> {
        let url = self.merge_request_url(&format!("{iid}/cancel_merge_when_pipeline_succeeds"))?;
        self.send(Method::POST, url, None).await.map(drop)
    }

    /// Every project in `group` and its subgroups that is not archived,
    /// excluding projects merely shared with it, in project-id order and
    /// without duplicates. `group` is a numeric id or a full path.
    pub async fn group_projects(&self, group: &str) -> Result<Vec<Value>, Error> {
        let base = self.url(&format!(
            "{}/groups/{}/projects",
            self.api_url,
            encode_segment(group)
        ))?;
        let mut projects: Vec<Value> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut page: u32 = 1;
        for _ in 0..MAX_PAGES {
            let mut url = base.clone();
            url.query_pairs_mut()
                .append_pair("include_subgroups", "true")
                .append_pair("with_shared", "false")
                .append_pair("archived", "false")
                .append_pair("order_by", "id")
                .append_pair("sort", "asc")
                .append_pair("per_page", "100")
                .append_pair("page", &page.to_string());
            let answer = self.execute(Method::GET, url, None).await?;
            let next = answer
                .headers
                .get("x-next-page")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            match parse_json(answer)? {
                Value::Array(items) => {
                    for item in items {
                        let Some(id) = item["id"].as_u64() else {
                            return Err(Error::Refused(format!(
                                "GitLab listed a project without a numeric id: {}",
                                excerpt(&item.to_string())
                            )));
                        };
                        if seen.insert(id) {
                            projects.push(item);
                        }
                    }
                }
                other => {
                    return Err(Error::Refused(format!(
                        "GitLab returned a project list that is not a list: {}",
                        excerpt(&other.to_string())
                    )));
                }
            }
            if next.is_empty() {
                return Ok(projects);
            }
            match next.parse::<u32>() {
                Ok(following) if following > page => page = following,
                _ => {
                    return Err(Error::Refused(format!(
                        "GitLab sent an X-Next-Page header of {} after page {page}",
                        excerpt(&next)
                    )));
                }
            }
        }
        Err(Error::Refused(format!(
            "GitLab kept paginating past {MAX_PAGES} pages of projects for {group}"
        )))
    }

    /// The content of `path` at `reference` in `project_id`, or `None` when
    /// the file does not exist there.
    pub async fn raw_file(
        &self,
        project_id: u64,
        path: &str,
        reference: &str,
    ) -> Result<Option<String>, Error> {
        let mut url = self.url(&format!(
            "{}/projects/{project_id}/repository/files/{}/raw",
            self.api_url,
            encode_segment(path)
        ))?;
        url.query_pairs_mut().append_pair("ref", reference);
        let answer = self.execute(Method::GET, url, None).await?;
        if answer.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        check_status(&answer)?;
        Ok(Some(answer.body))
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
                            headers,
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

/// Whether GitLab refused a new merge request only because it does not list
/// the just-pushed source branch yet.
fn source_branch_missing(answer: &Answer) -> bool {
    answer.status == StatusCode::BAD_REQUEST
        && serde_json::from_str::<Value>(&answer.body).is_ok_and(|body| {
            body["message"]["source_branch"]
                .as_array()
                .is_some_and(|reasons| {
                    reasons
                        .iter()
                        .any(|reason| reason.as_str() == Some("does not exist"))
                })
        })
}

/// What GitLab has not yet finished for a merge request that should head
/// `sha`, or `None` once it heads `sha` and its mergeability is checked.
/// Only GitLab's transient states count: a settled refusal such as a
/// conflict or a draft is GitLab's answer to give when auto-merge is asked.
fn unsettled(merge_request: &Value, sha: &str) -> Option<String> {
    match merge_request["sha"].as_str() {
        Some(head) if head == sha => {}
        Some(head) => return Some(format!("it still heads commit {head}")),
        None => return Some("it reports no commit".to_string()),
    }
    let (field, transient): (&str, &[&str]) = if merge_request["detailed_merge_status"].is_string()
    {
        (
            "detailed_merge_status",
            &["unchecked", "checking", "preparing", "approvals_syncing"],
        )
    } else {
        (
            "merge_status",
            &["unchecked", "checking", "cannot_be_merged_recheck"],
        )
    };
    let status = merge_request[field].as_str()?;
    transient
        .contains(&status)
        .then(|| format!("{field} is {status}"))
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

/// Percent-encodes one path segment: a project path or file path becomes a
/// single segment, as the GitLab API expects.
fn encode_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FAST: Retry = Retry {
        attempts: 5,
        base: Duration::from_millis(1),
        cap: Duration::from_millis(4),
    };

    async fn client(server: &MockServer) -> Client {
        Client::new(&format!("{}/api/v4", server.uri()), "token")
            .unwrap()
            .for_project(1)
            .with_retry(FAST)
            .with_settle(FAST)
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
        let error = Client::new(&format!("http://{address}/api/v4"), "token")
            .unwrap()
            .for_project(1)
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

    fn branch_missing() -> ResponseTemplate {
        ResponseTemplate::new(400)
            .set_body_json(json!({"message": {"source_branch": ["does not exist"]}}))
    }

    #[tokio::test]
    async fn creating_waits_for_gitlab_to_list_a_just_pushed_branch() {
        let server = MockServer::start().await;
        failing_then_ok(&server, "POST", MR_LIST, branch_missing(), 2).await;
        let created = client(&server).await.create("b", "main", "t", "d").await;
        assert!(created.is_ok(), "{created:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn creating_repeats_no_other_refusal() {
        for refusal in [
            ResponseTemplate::new(400)
                .set_body_json(json!({"message": {"target_branch": ["does not exist"]}})),
            ResponseTemplate::new(400).set_body_json(json!({"message": {"title": ["is blank"]}})),
            ResponseTemplate::new(409)
                .set_body_json(json!({"message": {"source_branch": ["does not exist"]}})),
        ] {
            let server = MockServer::start().await;
            failing_then_ok(&server, "POST", MR_LIST, refusal, 100).await;
            let error = client(&server)
                .await
                .create("b", "main", "t", "d")
                .await
                .unwrap_err();
            assert_eq!(error.kind(), "api_error", "{error}");
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn creating_stops_waiting_for_a_branch_after_the_settle_budget() {
        let server = MockServer::start().await;
        failing_then_ok(&server, "POST", MR_LIST, branch_missing(), 100).await;
        let error = client(&server)
            .await
            .create("b", "main", "t", "d")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), "network_error", "{error}");
        assert!(error.to_string().contains("pushed branch b"), "{error}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            FAST.attempts as usize
        );
    }

    const PUSHED: &str = "2222222222222222222222222222222222222222";
    const MR_7_MERGE: &str = "/api/v4/projects/1/merge_requests/7/merge";

    /// Answers `GET` for merge request 7 with each state in turn, the last
    /// one from then on, and accepts auto-merge.
    async fn merge_request_states(server: &MockServer, states: &[Value]) {
        for (priority, state) in states.iter().enumerate() {
            let mut body = state.clone();
            body["iid"] = json!(7);
            body["web_url"] = json!("u");
            let mock = Mock::given(method("GET"))
                .and(path(MR_7))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .with_priority(priority as u8 + 1);
            let mock = if priority + 1 < states.len() {
                mock.up_to_n_times(1)
            } else {
                mock
            };
            mock.mount(server).await;
        }
        Mock::given(method("PUT"))
            .and(path(MR_7_MERGE))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"iid": 7, "web_url": "u"})),
            )
            .mount(server)
            .await;
    }

    async fn requests(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| format!("{} {}", request.method, request.url.path()))
            .collect()
    }

    #[tokio::test]
    async fn arming_waits_until_gitlab_has_checked_the_pushed_commit() {
        let server = MockServer::start().await;
        merge_request_states(
            &server,
            &[
                json!({"sha": "1111111111111111111111111111111111111111", "detailed_merge_status": "mergeable"}),
                json!({"sha": PUSHED, "detailed_merge_status": "checking"}),
                json!({"sha": PUSHED, "detailed_merge_status": "preparing"}),
                json!({"sha": PUSHED, "detailed_merge_status": "ci_still_running"}),
            ],
        )
        .await;
        let armed = client(&server).await.enable_auto_merge(7, PUSHED).await;
        assert!(armed.is_ok(), "{armed:?}");
        let get = format!("GET {MR_7}");
        assert_eq!(
            requests(&server).await,
            [
                get.clone(),
                get.clone(),
                get.clone(),
                get,
                format!("PUT {MR_7_MERGE}")
            ]
        );
    }

    #[tokio::test]
    async fn arming_reads_the_legacy_merge_status_when_the_detailed_one_is_absent() {
        let server = MockServer::start().await;
        merge_request_states(
            &server,
            &[
                json!({"sha": PUSHED, "merge_status": "unchecked"}),
                json!({"sha": PUSHED, "merge_status": "cannot_be_merged_recheck"}),
                json!({"sha": PUSHED, "merge_status": "can_be_merged"}),
            ],
        )
        .await;
        let armed = client(&server).await.enable_auto_merge(7, PUSHED).await;
        assert!(armed.is_ok(), "{armed:?}");
        assert_eq!(requests(&server).await.len(), 4);
    }

    #[tokio::test]
    async fn arming_leaves_a_settled_refusal_to_gitlab() {
        let server = MockServer::start().await;
        merge_request_states(
            &server,
            &[json!({"sha": PUSHED, "detailed_merge_status": "conflict"})],
        )
        .await;
        let armed = client(&server).await.enable_auto_merge(7, PUSHED).await;
        assert!(armed.is_ok(), "{armed:?}");
        assert_eq!(
            requests(&server).await,
            [format!("GET {MR_7}"), format!("PUT {MR_7_MERGE}")]
        );
    }

    #[tokio::test]
    async fn arming_gives_up_naming_the_state_gitlab_never_left() {
        for (state, named) in [
            (
                json!({"sha": PUSHED, "detailed_merge_status": "checking"}),
                "detailed_merge_status is checking",
            ),
            (
                json!({"sha": "1111111111111111111111111111111111111111", "detailed_merge_status": "mergeable"}),
                "1111111111111111111111111111111111111111",
            ),
            (json!({"detailed_merge_status": "mergeable"}), "no commit"),
        ] {
            let server = MockServer::start().await;
            merge_request_states(&server, &[state]).await;
            let error = client(&server)
                .await
                .enable_auto_merge(7, PUSHED)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), "network_error", "{error}");
            assert!(error.to_string().contains(named), "{named}: {error}");
            assert!(error.to_string().contains(PUSHED), "{error}");
            let seen = requests(&server).await;
            assert_eq!(seen.len(), FAST.attempts as usize, "{seen:?}");
            assert!(
                seen.iter().all(|request| request.starts_with("GET ")),
                "{seen:?}"
            );
        }
    }

    const GROUP_PROJECTS: &str = "/api/v4/groups/acme%2Fplatform/projects";

    fn projects_page(ids: &[u64], next: Option<&str>) -> ResponseTemplate {
        let body: Vec<Value> = ids
            .iter()
            .map(|id| json!({"id": id, "path_with_namespace": format!("acme/p{id}")}))
            .collect();
        let template = ResponseTemplate::new(200).set_body_json(body);
        match next {
            Some(next) => template.insert_header("X-Next-Page", next),
            None => template.insert_header("X-Next-Page", ""),
        }
    }

    async fn mount_page(server: &MockServer, page: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(GROUP_PROJECTS))
            .and(query_param("page", page))
            .and(query_param("include_subgroups", "true"))
            .and(query_param("with_shared", "false"))
            .and(query_param("archived", "false"))
            .and(query_param("order_by", "id"))
            .respond_with(response)
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn group_projects_follow_every_page_once_and_drop_duplicates() {
        let server = MockServer::start().await;
        mount_page(&server, "1", projects_page(&[1, 2], Some("2"))).await;
        mount_page(&server, "2", projects_page(&[2, 3], Some("3"))).await;
        mount_page(&server, "3", projects_page(&[4], None)).await;
        let projects = client(&server)
            .await
            .group_projects("acme/platform")
            .await
            .unwrap();
        let ids: Vec<u64> = projects.iter().map(|p| p["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn group_projects_refuse_pagination_that_does_not_advance() {
        for next in ["1", "0", "two", "-3"] {
            let server = MockServer::start().await;
            mount_page(&server, "1", projects_page(&[1], Some(next))).await;
            let error = client(&server)
                .await
                .group_projects("acme/platform")
                .await
                .unwrap_err();
            assert_eq!(error.kind(), "refused", "{next}: {error}");
            assert!(error.to_string().contains("X-Next-Page"), "{error}");
        }
    }

    #[tokio::test]
    async fn group_projects_refuse_entries_without_a_numeric_id() {
        for body in [
            json!([{"id": "5"}]),
            json!([{"name": "x"}]),
            json!({"id": 5}),
        ] {
            let server = MockServer::start().await;
            mount_page(
                &server,
                "1",
                ResponseTemplate::new(200).set_body_json(body.clone()),
            )
            .await;
            let error = client(&server)
                .await
                .group_projects("acme/platform")
                .await
                .unwrap_err();
            assert_eq!(error.kind(), "refused", "{body}: {error}");
        }
    }

    #[tokio::test]
    async fn raw_file_distinguishes_a_missing_file_from_a_failure() {
        let server = MockServer::start().await;
        let route = "/api/v4/projects/42/repository/files/config%2F.updrc.toml/raw";
        Mock::given(method("GET"))
            .and(path(route))
            .and(query_param("ref", "main"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[automation]\n"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(route))
            .and(query_param("ref", "gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(route))
            .and(query_param("ref", "denied"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let client = client(&server).await;
        assert_eq!(
            client
                .raw_file(42, "config/.updrc.toml", "main")
                .await
                .unwrap()
                .as_deref(),
            Some("[automation]\n")
        );
        assert_eq!(
            client
                .raw_file(42, "config/.updrc.toml", "gone")
                .await
                .unwrap(),
            None
        );
        let error = client
            .raw_file(42, "config/.updrc.toml", "denied")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), "api_error", "{error}");
    }

    #[test]
    fn segments_encode_everything_but_unreserved_bytes() {
        assert_eq!(encode_segment("acme/platform"), "acme%2Fplatform");
        assert_eq!(encode_segment("a b.c-d_e~f"), "a%20b.c-d_e~f");
        assert_eq!(encode_segment("../x?y#z"), "..%2Fx%3Fy%23z");
        assert_eq!(encode_segment("é"), "%C3%A9");
    }

    #[test]
    fn a_project_client_addresses_that_project() {
        let base = Client::new("https://gitlab.example.test/api/v4/", "t").unwrap();
        assert_eq!(
            base.for_project(99).merge_requests,
            "https://gitlab.example.test/api/v4/projects/99/merge_requests"
        );
    }
}
