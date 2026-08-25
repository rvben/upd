use super::{RefNotFound, Registry, TagsAtCommit, VersionMeta, get_with_retry, http_error_message};
use crate::version::TagVersion;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
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
    #[serde(default)]
    commit: Option<TagCommit>,
}

/// The commit a tag names. GitHub dereferences an annotated tag here, so this is
/// the commit SHA rather than the tag object's own SHA.
#[derive(Debug, Deserialize)]
struct TagCommit {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseListEntry {
    tag_name: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
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

        let client = crate::http::apply(
            Client::builder()
                .user_agent(user_agent)
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .default_headers(headers),
        )
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

    /// Collect every tag naming `commit`, walking the paginated tag list.
    ///
    /// The whole list is read rather than stopping at the first match, because a
    /// commit is routinely named by several tags at once - a release `v7.0.1`
    /// beside a floating `v7` - and only the concrete one is usable. Stopping
    /// early can see the floating tag alone and report a perfectly ordinary
    /// release as unidentifiable.
    async fn fetch_tags_at_commit(
        &self,
        owner: &str,
        repo: &str,
        commit: &str,
    ) -> Result<Vec<String>> {
        let target = commit.to_ascii_lowercase();
        let mut url = format!(
            "{}/repos/{}/{}/tags?per_page={}",
            self.api_url, owner, repo, TAG_PAGE_SIZE
        );
        let mut names = Vec::new();

        for _ in 0..MAX_TAG_PAGES {
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

            let next = next_page_url(response.headers(), &self.api_url);
            let tags: Vec<TagResponse> = response.json().await?;

            for tag in tags {
                if tag
                    .commit
                    .is_some_and(|c| c.sha.eq_ignore_ascii_case(&target))
                {
                    names.push(tag.name);
                }
            }

            match next {
                Some(next_url) => url = next_url,
                None => return Ok(names),
            }
        }

        // Answering with the tags found so far would be indistinguishable from
        // having read the whole list, and the caller writes a version comment
        // from this answer. A repository this heavily tagged gets an honest
        // failure instead of a pin annotated from a partial view.
        Err(anyhow!(
            "Repository '{owner}/{repo}' publishes more than {} tags; \
             upd stopped before identifying commit {commit}. \
             Annotate this pin by hand with the release it names.",
            MAX_TAG_PAGES * TAG_PAGE_SIZE,
        ))
    }
}

/// Tags requested per page. GitHub's maximum, so the common repository is one
/// request.
const TAG_PAGE_SIZE: usize = 100;

/// Pages walked before a commit lookup gives up and reports failure.
const MAX_TAG_PAGES: usize = 20;

/// The `rel="next"` URL from a `Link` header, if the response has one.
///
/// The URL is only followed when it addresses the same origin the request went
/// to, so a redirecting proxy in front of the API cannot walk the client, and
/// its `Authorization` header, onto a host the user never configured.
fn next_page_url(headers: &reqwest::header::HeaderMap, api_url: &str) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;

    let candidate = link.split(',').find_map(|entry| {
        let (target, params) = entry.split_once(';')?;
        if !params
            .split(';')
            .any(|p| matches!(p.trim(), "rel=\"next\"" | "rel=next"))
        {
            return None;
        }
        let target = target.trim();
        target
            .strip_prefix('<')
            .and_then(|t| t.strip_suffix('>'))
            .map(str::to_string)
    })?;

    let base = reqwest::Url::parse(api_url).ok()?;
    let next = reqwest::Url::parse(&candidate).ok()?;
    (next.scheme() == base.scheme()
        && next.host_str() == base.host_str()
        && next.port_or_known_default() == base.port_or_known_default())
    .then_some(candidate)
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

        // Try releases/latest first - it returns the most recent non-prerelease.
        let latest_url = format!("{}/repos/{}/{}/releases/latest", self.api_url, owner, repo);
        let response = get_with_retry(&self.client, &latest_url).await?;
        let status = response.status();

        if status.is_success() {
            let release: ReleaseResponse = response.json().await?;
            // A repository may publish releases whose tags are not versions at
            // all - dated artifact bundles alongside the code releases, say.
            // GitHub calls the newest of those "latest" regardless, so an
            // unparsable tag means this endpoint cannot answer the question,
            // not that the repository has no versions. Fall through to the tag
            // scan below, which filters non-versions out. Returning the tag
            // verbatim instead leaves the caller with an unusable version and
            // the pin silently counted as up to date.
            if TagVersion::parse(&release.tag_name).is_some() {
                return Ok(release.tag_name);
            }
        } else if status.as_u16() != 404 {
            // On 404 (no releases published), fall back to the tags endpoint.
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

    /// Tags are the refs an action can be pinned to, so the tag list is exactly
    /// what decides whether a floating major like `v4` is writable. A repo can
    /// publish `v4.1.2` while its newest floating major is still `v3`.
    async fn list_ref_names(&self, package: &str) -> Result<Vec<String>> {
        let (owner, repo) = Self::extract_owner_repo(package)?;
        self.fetch_tags(owner, repo).await
    }

    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        let (owner, repo) = Self::extract_owner_repo(package)?;
        let mut url = reqwest::Url::parse(&self.api_url)?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow!("invalid GitHub API base URL"))?;
            segments
                .pop_if_empty()
                .extend(["repos", owner, repo, "commits", reference]);
        }

        let response = get_with_retry(&self.client, url.as_str()).await?;
        if !response.status().is_success() {
            let status = response.status();
            let hint = match status.as_u16() {
                403 | 429 => Some("Set GITHUB_TOKEN to increase the API rate limit."),
                _ => None,
            };
            let message = http_error_message(
                status,
                "Git ref",
                &format!("{owner}/{repo}@{reference}"),
                hint,
            );
            // This endpoint answers a ref that names no commit with 422 and the
            // body "No commit found for SHA", for a tag the repo never published
            // and for a string that is not a ref at all alike; 404 is how it
            // reports a repository it cannot see. Both are statements about what
            // was asked for, while every other status is about the request, so
            // narrowing this to 404 would stop the version-comment fallback from
            // ever running. Throttling does not reach here disguised as an
            // absent ref: an exhausted quota on this endpoint answers 403, which
            // falls through to the error below and is reported rather than
            // licensing the other spelling.
            return Err(match status.as_u16() {
                404 | 422 => anyhow!(RefNotFound::new(message)),
                _ => anyhow!(message),
            });
        }

        let commit: CommitResponse = response.json().await?;
        if commit.sha.len() != 40 || !commit.sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(anyhow!(
                "GitHub returned an invalid commit SHA for '{owner}/{repo}@{reference}'"
            ));
        }
        Ok(commit.sha.to_ascii_lowercase())
    }

    /// A release tags the commit it shipped, so the repository can name the
    /// release a bare commit pin refers to even though the workflow file cannot.
    async fn tags_at_commit(&self, package: &str, commit: &str) -> Result<TagsAtCommit> {
        let (owner, repo) = Self::extract_owner_repo(package)?;
        if commit.len() != 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(anyhow!(
                "'{commit}' is not a full 40-character commit SHA for '{owner}/{repo}'"
            ));
        }
        self.fetch_tags_at_commit(owner, repo, commit)
            .await
            .map(TagsAtCommit::Known)
    }

    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        let (owner, repo) = Self::extract_owner_repo(package)?;
        let url = format!("{}/repos/{}/{}/releases", self.api_url, owner, repo);

        let response = get_with_retry(&self.client, &url).await?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            let hint = match status.as_u16() {
                403 | 429 => Some("Set GITHUB_TOKEN to increase the API rate limit."),
                _ => None,
            };
            return Err(anyhow!(http_error_message(
                status,
                "Repository",
                &format!("{owner}/{repo}"),
                hint,
            )));
        }

        let items: Vec<ReleaseListEntry> = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse GitHub releases for '{package}': {e}"))?;

        Ok(items
            .into_iter()
            .filter(|r| !r.draft)
            .map(|r| {
                let published_at = r
                    .published_at
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
                VersionMeta {
                    version: r.tag_name,
                    published_at,
                    yanked: false,
                    prerelease: r.prerelease,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::super::is_ref_not_found;
    use super::*;
    use chrono::TimeZone;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn registry(server: &MockServer) -> GitHubReleasesRegistry {
        GitHubReleasesRegistry::with_api_url(server.uri())
    }

    /// The SHA-pin updater tries the other spelling of a version only when the
    /// repo has said the written one does not exist, so which statuses carry
    /// that meaning is wiring the mock registry cannot check.
    ///
    /// 422 is load-bearing: `GET /repos/actions/checkout/commits/7.0.1` answers
    /// 422 for a repo that tags `v7.0.1`, so a classification narrowed to 404
    /// leaves every bare version comment unresolvable.
    #[tokio::test]
    async fn test_only_a_ref_answer_is_reported_as_a_missing_ref() {
        for (status, missing) in [
            (404, true),
            (422, true),
            (403, false),
            (429, false),
            (500, false),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/acme/action/commits/1.2.3"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;

            let error = registry(&server)
                .resolve_ref_to_commit("acme/action", "1.2.3")
                .await
                .expect_err("HTTP {status} should not resolve");

            assert_eq!(
                is_ref_not_found(&error),
                missing,
                "HTTP {status} was classified wrongly: {error}"
            );
        }
    }

    /// A commit is routinely named by a release tag and a floating major at
    /// once, and the caller needs both to pick the concrete one.
    #[tokio::test]
    async fn every_tag_naming_the_commit_is_returned() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let other = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/action/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"[{{"name": "v2.1.0", "commit": {{"sha": "{sha}"}}}},
                    {{"name": "v2", "commit": {{"sha": "{sha}"}}}},
                    {{"name": "v2.0.9", "commit": {{"sha": "{other}"}}}}]"#
            )))
            .mount(&server)
            .await;

        assert_eq!(
            registry(&server)
                .tags_at_commit("acme/action", sha)
                .await
                .unwrap(),
            TagsAtCommit::Known(vec!["v2.1.0".to_string(), "v2".to_string()])
        );
    }

    /// A commit off every release is a real answer the caller must act on, not
    /// an absence of one: it is what stops upd inventing a version comment for a
    /// pin nobody can identify.
    #[tokio::test]
    async fn a_commit_no_tag_names_is_an_answer_not_an_absence() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/action/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"name": "v1", "commit": {"sha": "abcdefabcdefabcdefabcdefabcdefabcdefabcd"}}]"#,
            ))
            .mount(&server)
            .await;

        assert_eq!(
            registry(&server)
                .tags_at_commit("acme/action", "1234567890abcdef1234567890abcdef12345678")
                .await
                .unwrap(),
            TagsAtCommit::Known(Vec::new())
        );
    }

    /// The tag naming the commit can sit on any page, so a lookup that read only
    /// the first would report an ordinary release as unidentifiable.
    #[tokio::test]
    async fn the_tag_list_is_walked_across_pages() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let other = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/acme/action/tags"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"[{{"name": "v9.9.9", "commit": {{"sha": "{sha}"}}}}]"#
            )))
            .expect(1)
            .mount(&server)
            .await;

        let next = format!(
            "{}/repos/acme/action/tags?per_page=100&page=2",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/repos/acme/action/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", format!(r#"<{next}>; rel="next""#).as_str())
                    .set_body_string(format!(
                        r#"[{{"name": "v1.0.0", "commit": {{"sha": "{other}"}}}}]"#
                    )),
            )
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(
            registry(&server)
                .tags_at_commit("acme/action", sha)
                .await
                .unwrap(),
            TagsAtCommit::Known(vec!["v9.9.9".to_string()])
        );
    }

    /// A `Link` header pointing somewhere else must not walk the client, and its
    /// `Authorization` header, onto a host the user never configured. The walk
    /// stops instead, which for this fixture means the off-origin page's tag is
    /// never seen.
    #[tokio::test]
    async fn pagination_does_not_follow_a_link_to_another_origin() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/acme/action/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "link",
                        r#"<https://attacker.example/repos/acme/action/tags?page=2>; rel="next""#,
                    )
                    .set_body_string("[]"),
            )
            .mount(&server)
            .await;

        assert_eq!(
            registry(&server)
                .tags_at_commit("acme/action", sha)
                .await
                .unwrap(),
            TagsAtCommit::Known(Vec::new())
        );
    }

    /// Answering with the tags read so far would be indistinguishable from
    /// having read them all, and the caller writes a version comment from this
    /// answer. A partial view is reported as a failure instead.
    #[tokio::test]
    async fn a_tag_list_too_long_to_read_fails_rather_than_reporting_none() {
        let server = MockServer::start().await;
        let next = format!(
            "{}/repos/acme/action/tags?per_page=100&page=2",
            server.uri()
        );

        Mock::given(method("GET"))
            .and(path("/repos/acme/action/tags"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", format!(r#"<{next}>; rel="next""#).as_str())
                    .set_body_string("[]"),
            )
            .mount(&server)
            .await;

        let error = registry(&server)
            .tags_at_commit("acme/action", "1234567890abcdef1234567890abcdef12345678")
            .await
            .expect_err("a tag list that never ends must not answer 'no tags'");

        assert!(
            error.to_string().contains("more than"),
            "the error must say the list was too long to read: {error}"
        );
    }

    /// A rate limit is not evidence that a commit belongs to no release.
    #[tokio::test]
    async fn a_failed_tag_lookup_is_an_error_not_an_empty_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/action/tags"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        assert!(
            registry(&server)
                .tags_at_commit("acme/action", "1234567890abcdef1234567890abcdef12345678")
                .await
                .is_err()
        );
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
    async fn a_latest_release_whose_tag_is_not_a_version_falls_back_to_tags() {
        let server = MockServer::start().await;

        // A repository that publishes dated artifact bundles beside its code
        // releases: GitHub names the newest bundle "latest" even though its tag
        // is not a version, so this endpoint cannot answer on its own.
        Mock::given(method("GET"))
            .and(path("/repos/rvben/husker/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"tag_name": "images-2026-08-24T193611Z", "name": "Default images"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/rvben/husker/tags"))
            .and(query_param("per_page", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"name": "images-2026-08-24T193611Z"}, {"name": "v0.4.48"}, {"name": "images-2026-08-23T134321Z"}, {"name": "v0.4.47"}]"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let version = registry(&server)
            .get_latest_version("rvben/husker")
            .await
            .unwrap();

        assert_eq!(version, "v0.4.48");
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

    /// Regression: shellcheck-py publishes 4-segment tags (v0.11.0.1,
    /// v0.8.0.4, …) and does NOT create GitHub Releases. A semver-only
    /// parser rejects every 4-segment tag and collapses the stable set to
    /// the lone 3-segment legacy tag v0.0.2.
    #[tokio::test]
    async fn test_four_segment_tags_shellcheck_py_regression() {
        let server = MockServer::start().await;

        // releases/latest returns 404 - shellcheck-py has no GitHub Releases.
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

    #[tokio::test]
    async fn test_list_versions_returns_publish_dates_and_filters_drafts() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
              {"tag_name": "v4.2.0", "published_at": "2024-10-01T10:00:00Z", "prerelease": false, "draft": false},
              {"tag_name": "v4.2.0-beta", "published_at": "2024-09-20T10:00:00Z", "prerelease": true, "draft": false},
              {"tag_name": "v4.1.0", "published_at": "2024-08-01T10:00:00Z", "prerelease": false, "draft": false},
              {"tag_name": "v5.0.0-draft", "published_at": null, "prerelease": false, "draft": true}
            ]"#,
            ))
            .mount(&server)
            .await;

        let versions = registry(&server)
            .list_versions("actions/checkout")
            .await
            .unwrap();

        assert_eq!(versions.len(), 3, "draft releases must be filtered out");
        assert!(
            versions
                .iter()
                .any(|v| v.version == "v4.2.0" && !v.prerelease)
        );
        assert!(
            versions
                .iter()
                .any(|v| v.version == "v4.2.0-beta" && v.prerelease)
        );
        assert!(versions.iter().all(|v| !v.yanked));

        let v420 = versions.iter().find(|v| v.version == "v4.2.0").unwrap();
        let expected = chrono::Utc.with_ymd_and_hms(2024, 10, 1, 10, 0, 0).unwrap();
        assert_eq!(
            v420.published_at,
            Some(expected),
            "published_at should parse from RFC3339 and convert to UTC"
        );
    }

    #[tokio::test]
    async fn test_resolve_ref_to_commit_uses_commit_endpoint() {
        let server = MockServer::start().await;
        let sha = "1234567890abcdef1234567890abcdef12345678";

        Mock::given(method("GET"))
            .and(path("/repos/actions/checkout/commits/v4.2.2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(format!(r#"{{"sha":"{sha}"}}"#)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let resolved = registry(&server)
            .resolve_ref_to_commit("actions/checkout", "v4.2.2")
            .await
            .unwrap();
        assert_eq!(resolved, sha);
    }
}
