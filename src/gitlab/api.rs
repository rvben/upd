//! The slice of the GitLab merge-request API a rolling update needs.

use std::time::Duration;

use reqwest::{Method, StatusCode};
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

pub struct Client {
    http: reqwest::Client,
    token: String,
    merge_requests: String,
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
        Ok(Self {
            http,
            token: token.to_string(),
            merge_requests: format!(
                "{}/projects/{}/merge_requests",
                api_url.trim_end_matches('/'),
                project_id
            ),
        })
    }

    /// Open merge requests from `source` into `target`.
    pub async fn open_merge_requests(
        &self,
        source: &str,
        target: &str,
    ) -> Result<Vec<Value>, Error> {
        let mut url = reqwest::Url::parse(&self.merge_requests).map_err(|error| {
            Error::Input(format!(
                "invalid GitLab API URL {}: {error}",
                self.merge_requests
            ))
        })?;
        url.query_pairs_mut()
            .append_pair("state", "opened")
            .append_pair("source_branch", source)
            .append_pair("target_branch", target);
        match self.send(self.http.get(url)).await? {
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
        self.send(self.http.post(&self.merge_requests).json(&body))
            .await
    }

    pub async fn edit(&self, iid: u64, body: Value) -> Result<Value, Error> {
        self.send(self.request(Method::PUT, &iid.to_string()).json(&body))
            .await
    }

    /// Asks GitLab to merge once the pipeline for exactly `sha` succeeds.
    pub async fn enable_auto_merge(&self, iid: u64, sha: &str) -> Result<(), Error> {
        let body = json!({
            "auto_merge": true,
            "sha": sha,
            "should_remove_source_branch": true,
        });
        self.send(
            self.request(Method::PUT, &format!("{iid}/merge"))
                .json(&body),
        )
        .await
        .map(drop)
    }

    pub async fn cancel_auto_merge(&self, iid: u64) -> Result<(), Error> {
        self.send(self.request(
            Method::POST,
            &format!("{iid}/cancel_merge_when_pipeline_succeeds"),
        ))
        .await
        .map(drop)
    }

    fn request(&self, method: Method, suffix: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/{suffix}", self.merge_requests))
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, Error> {
        let response = request
            .header("PRIVATE-TOKEN", &self.token)
            .send()
            .await
            .map_err(|error| Error::Network(format!("GitLab request failed: {error}")))?;
        let status = response.status();
        let url = redact_query(response.url());
        let body = response.text().await.map_err(|error| {
            Error::Network(format!("GitLab response could not be read: {error}"))
        })?;
        if !status.is_success() {
            let message = format!("GitLab answered {status} for {url}: {}", excerpt(&body));
            return Err(
                if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    Error::Network(message)
                } else {
                    Error::Api(message)
                },
            );
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).map_err(|error| {
            Error::Refused(format!(
                "GitLab returned a response that is not JSON ({error}) for {url}"
            ))
        })
    }
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

fn redact_query(url: &reqwest::Url) -> String {
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
