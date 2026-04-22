use super::{Registry, get_with_retry, http_error_message};
use crate::version::TagVersion;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use std::time::Duration;

pub struct GitHubReleasesRegistry {
    client: Client,
    api_url: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
}

#[derive(Debug, Deserialize)]
struct TagResponse {
    name: String,
}

impl GitHubReleasesRegistry {
    pub fn new() -> Self {
        let token = Self::detect_token();
        Self::with_api_url_and_token("https://api.github.com".to_string(), token)
    }

    #[cfg(test)]
    pub fn with_api_url(api_url: String) -> Self {
        Self::with_api_url_and_token(api_url, None)
    }

    pub fn with_api_url_and_token(api_url: String, token: Option<String>) -> Self {
        let mut headers = HeaderMap::new();

        let accept = HeaderValue::from_static("application/vnd.github+json");
        headers.insert(ACCEPT, accept);

        if let Some(tok) = token
            && let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", tok))
        {
            headers.insert(AUTHORIZATION, value);
        }

        let user_agent = concat!("upd/", env!("CARGO_PKG_VERSION"));

        let client = Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .default_headers(headers)
            .build()
            .expect("Failed to create HTTP client for GitHub API.");

        Self { client, api_url }
    }

    /// Check `GITHUB_TOKEN` then `GH_TOKEN` for an auth token.
    pub fn detect_token() -> Option<String> {
        std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty()))
    }

    /// Extract `owner/repo` from a package string like `owner/repo` or `owner/repo/path/to/action`.
    fn extract_owner_repo(package: &str) -> Result<(&str, &str)> {
        let mut parts = package.splitn(3, '/');
        let owner = parts.next().unwrap_or("");
        let repo = parts.next().unwrap_or("");
        if owner.is_empty() || repo.is_empty() {
            return Err(anyhow!(
                "Invalid GitHub Actions package '{}': expected owner/repo format",
                package
            ));
        }
        Ok((owner, repo))
    }

    /// Fetch all tags for a repo and return them as raw strings.
    async fn fetch_tags(&self, owner: &str, repo: &str) -> Result<Vec<String>> {
        let url = format!(
            "{}/repos/{}/{}/tags?per_page=100",
            self.api_url, owner, repo
        );

        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            let status = response.status();
            let hint = match status.as_u16() {
                403 | 429 => Some("Set GITHUB_TOKEN to increase the API rate limit."),
                _ => None,
            };
            return Err(anyhow!(http_error_message(
                status,
                "Repository",
                &format!("{}/{}", owner, repo),
                hint,
            )));
        }

        let tags: Vec<TagResponse> = response.json().await?;
        Ok(tags.into_iter().map(|t| t.name).collect())
    }
}

impl Default for GitHubReleasesRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for GitHubReleasesRegistry {
    fn name(&self) -> &'static str {
        "github-releases"
    }

    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let (owner, repo) = Self::extract_owner_repo(package)?;

        // Try releases/latest first — it returns the most recent non-prerelease.
        let latest_url = format!("{}/repos/{}/{}/releases/latest", self.api_url, owner, repo);
        let response = get_with_retry(&self.client, &latest_url).await?;

        if response.status().is_success() {
            let release: ReleaseResponse = response.json().await?;
            return Ok(release.tag_name);
        }

        // On 404 (no releases published), fall back to the tags endpoint.
        if response.status().as_u16() != 404 {
            let status = response.status();
            let hint = match status.as_u16() {
                403 | 429 => Some("Set GITHUB_TOKEN to increase the API rate limit."),
                _ => None,
            };
            return Err(anyhow!(http_error_message(
                status,
                "Repository",
                &format!("{}/{}", owner, repo),
                hint,
            )));
        }

        let tags = self.fetch_tags(owner, repo).await?;

        let mut stable: Vec<_> = tags
            .iter()
            .filter_map(|t| TagVersion::parse(t).map(|v| (v, t.clone())))
            .filter(|(v, _)| !v.is_prerelease())
            .collect();

        stable.sort_by(|a, b| b.0.cmp(&a.0));

        stable
            .into_iter()
            .next()
            .map(|(_, tag)| tag)
            .ok_or_else(|| {
                anyhow!(
                    "Repository '{}/{}' has no stable releases or tags.",
                    owner,
                    repo
                )
            })
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let (owner, repo) = Self::extract_owner_repo(package)?;

        let tags = self.fetch_tags(owner, repo).await?;

        let mut all: Vec<_> = tags
            .iter()
            .filter_map(|t| TagVersion::parse(t).map(|v| (v, t.clone())))
            .collect();

        all.sort_by(|a, b| b.0.cmp(&a.0));

        all.into_iter()
            .next()
            .map(|(_, tag)| tag)
            .ok_or_else(|| anyhow!("Repository '{}/{}' has no tags available.", owner, repo))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn registry(server: &MockServer) -> GitHubReleasesRegistry {
        GitHubReleasesRegistry::with_api_url(server.uri())
    }

    #[tokio::test]
    async fn test_get_latest_version_from_releases() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/releases/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"tag_name": "v4.2.0", "name": "v4.2.0"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let version = registry(&server)
            .get_latest_version("actions/checkout")
            .await
            .unwrap();

        assert_eq!(version, "v4.2.0");
    }

    #[tokio::test]
    async fn test_fallback_to_tags_on_404() {
        let server = MockServer::start().await;

        // releases/latest returns 404 (no releases published)
        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/releases/latest"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        // tags endpoint returns a list
        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/tags"))
            .and(query_param("per_page", "100"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"[{"name": "v4.2.0"}, {"name": "v4.1.0"}, {"name": "v3.0.0"}]"#,
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let version = registry(&server)
            .get_latest_version("actions/checkout")
            .await
            .unwrap();

        assert_eq!(version, "v4.2.0");
    }

    #[tokio::test]
    async fn test_extracts_owner_repo_from_subdirectory_action() {
        let server = MockServer::start().await;

        // Package has a subdirectory path: org/repo/path/to/action
        Mock::given(method("GET"))
            .and(path("/repos/hashicorp/setup-terraform/releases/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"tag_name": "v3.1.2", "name": "v3.1.2"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let version = registry(&server)
            .get_latest_version("hashicorp/setup-terraform/some/sub/path")
            .await
            .unwrap();

        assert_eq!(version, "v3.1.2");
    }

    #[tokio::test]
    async fn test_malformed_package_name_errors() {
        let server = MockServer::start().await;
        let reg = registry(&server);

        let result = reg.get_latest_version("singlesegment").await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("owner/repo"),
            "Error should mention owner/repo format, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_registry_name() {
        let server = MockServer::start().await;
        assert_eq!(registry(&server).name(), "github-releases");
    }

    #[tokio::test]
    async fn test_tags_fallback_skips_prereleases() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/releases/latest"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/tags"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                        {"name": "v5.0.0-beta.1"},
                        {"name": "v4.2.0"},
                        {"name": "v4.1.0-rc.1"},
                        {"name": "v4.1.0"}
                    ]"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let version = registry(&server)
            .get_latest_version("actions/checkout")
            .await
            .unwrap();

        // v5.0.0-beta.1 is prerelease; stable latest is v4.2.0
        assert_eq!(version, "v4.2.0");
    }

    #[tokio::test]
    async fn test_rate_limit_error_includes_token_hint() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/releases/latest"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let result = registry(&server)
            .get_latest_version("actions/checkout")
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("403"), "Should mention 403: {}", err);
        assert!(
            err.contains("GITHUB_TOKEN"),
            "Should hint about token: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_tags_with_no_parseable_versions() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/test/repo/releases/latest"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/test/repo/tags"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"[{"name": "nightly"}, {"name": "edge"}, {"name": "latest"}]"#,
                ),
            )
            .mount(&server)
            .await;

        let result = registry(&server).get_latest_version("test/repo").await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("no stable"),
            "Error should mention 'no stable'"
        );
    }

    #[tokio::test]
    async fn test_get_latest_including_prereleases() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/tags"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                        {"name": "v5.0.0-beta.1"},
                        {"name": "v4.2.0"},
                        {"name": "v4.1.0"}
                    ]"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let version = registry(&server)
            .get_latest_version_including_prereleases("actions/checkout")
            .await
            .unwrap();

        // With prereleases included, v5.0.0-beta.1 is newest
        assert_eq!(version, "v5.0.0-beta.1");
    }

    /// Regression for issue #2: shellcheck-py publishes 4-segment tags
    /// (v0.11.0.1, v0.8.0.4, …) and does NOT create GitHub Releases.
    /// Before the fix, semver::Version::parse rejected every 4-segment tag and
    /// the registry collapsed to the lone 3-segment legacy tag v0.0.2.
    #[tokio::test]
    async fn test_four_segment_tags_shellcheck_py_regression() {
        let server = MockServer::start().await;

        // releases/latest returns 404 — shellcheck-py has no GitHub Releases.
        Mock::given(method("GET"))
            .and(path("/repos/shellcheck-py/shellcheck-py/releases/latest"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        // tags endpoint returns the real shellcheck-py tag stream.
        Mock::given(method("GET"))
            .and(path("/repos/shellcheck-py/shellcheck-py/tags"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"name": "v0.11.0.1"},
                    {"name": "v0.10.0.1"},
                    {"name": "v0.9.0.6"},
                    {"name": "v0.9.0.5"},
                    {"name": "v0.8.0.4"},
                    {"name": "v0.8.0.3"},
                    {"name": "v0.7.0.1-1"},
                    {"name": "v0.0.2"}
                ]"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let version = registry(&server)
            .get_latest_version("shellcheck-py/shellcheck-py")
            .await
            .unwrap();

        assert_eq!(version, "v0.11.0.1");
    }

    /// Mixed 3- and 4-segment tags must sort numerically, not lexically.
    #[tokio::test]
    async fn test_tags_fallback_mixed_segment_counts_sort_numerically() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/test/repo/releases/latest"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        // "0.9.0.10" > "0.9.0.2" numerically, but lexically "0.9.0.10" < "0.9.0.2".
        Mock::given(method("GET"))
            .and(path("/repos/test/repo/tags"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"name": "v0.9.0.2"},
                    {"name": "v0.9.0.10"}
                ]"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let version = registry(&server)
            .get_latest_version("test/repo")
            .await
            .unwrap();
        assert_eq!(version, "v0.9.0.10");
    }

    /// get_latest_version_including_prereleases must also handle 4-segment tags.
    #[tokio::test]
    async fn test_prerelease_path_handles_four_segment_tags() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/test/repo/tags"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"name": "v0.11.0.1"},
                    {"name": "v0.12.0.0-rc.1"},
                    {"name": "v0.8.0.4"}
                ]"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let version = registry(&server)
            .get_latest_version_including_prereleases("test/repo")
            .await
            .unwrap();

        assert_eq!(version, "v0.12.0.0-rc.1");
    }
}
