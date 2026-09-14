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

/// A tag ref. `object.kind` is `"tag"` for an annotated tag, whose own object
/// carries the creation time, and `"commit"` for a lightweight tag, which is a
/// bare pointer git records nothing else about.
#[derive(Debug, Deserialize)]
struct RefResponse {
    object: RefObject,
}

#[derive(Debug, Deserialize)]
struct RefObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct TagObjectResponse {
    tagger: Option<GitSignature>,
}

#[derive(Debug, Deserialize)]
struct CommitDateResponse {
    commit: CommitDetail,
}

#[derive(Debug, Deserialize)]
struct CommitDetail {
    committer: Option<GitSignature>,
    author: Option<GitSignature>,
}

#[derive(Debug, Deserialize)]
struct GitSignature {
    date: Option<String>,
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

    /// Publish dates read from the tag list, for a repository that publishes no
    /// releases to read them from.
    ///
    /// Every date costs two requests, so the walk keeps the newest
    /// [`MAX_DATED_TAGS_PER_TRACK`] tags of each release track and stops. What
    /// truncation drops is the oldest candidates, which cooldown reaches only
    /// once every newer version has been rejected as too new; the update is
    /// then reported as skipped rather than held back to an older tag, so a
    /// version this walk never dated is never offered.
    async fn tag_versions(&self, owner: &str, repo: &str) -> Result<Vec<VersionMeta>> {
        let mut tags: Vec<(TagVersion, String)> = self
            .fetch_tags(owner, repo)
            .await?
            .into_iter()
            .filter_map(|name| TagVersion::parse(&name).map(|v| (v, name)))
            .collect();
        tags.sort_by(|a, b| b.0.cmp(&a.0));

        let mut dated_stable = 0usize;
        let mut dated_prerelease = 0usize;
        let mut versions = Vec::new();
        for (version, name) in tags {
            let prerelease = version.is_prerelease();
            let dated = if prerelease {
                &mut dated_prerelease
            } else {
                &mut dated_stable
            };
            if *dated == MAX_DATED_TAGS_PER_TRACK {
                continue;
            }
            *dated += 1;
            let published_at = self.tag_published_at(owner, repo, &name).await?;
            versions.push(VersionMeta {
                version: name,
                published_at,
                yanked: false,
                prerelease,
            });
        }
        Ok(versions)
    }

    /// When `tag` became available.
    ///
    /// An annotated tag records when it was created, which is the moment the
    /// version could first be used. A lightweight tag is a bare pointer git
    /// stores no timestamp for, so the commit it names is the only date there
    /// is; a tag pushed long after its commit therefore reads as older than it
    /// is, and is the reason the annotated form is not dated by its commit too.
    async fn tag_published_at(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<Option<DateTime<Utc>>> {
        let subject = format!("{owner}/{repo}@{tag}");
        let reference: RefResponse = self
            .get_json(&["repos", owner, repo, "git", "ref", "tags", tag], &subject)
            .await?;
        let sha = reference.object.sha;

        if reference.object.kind == "tag" {
            let object: TagObjectResponse = self
                .get_json(&["repos", owner, repo, "git", "tags", &sha], &subject)
                .await?;
            return Ok(object
                .tagger
                .and_then(|t| t.date)
                .as_deref()
                .and_then(timestamp));
        }

        let commit: CommitDateResponse = self
            .get_json(&["repos", owner, repo, "commits", &sha], &subject)
            .await?;
        Ok(commit
            .commit
            .committer
            .or(commit.commit.author)
            .and_then(|s| s.date)
            .as_deref()
            .and_then(timestamp))
    }

    /// GET one JSON document, reporting a failure against `subject`.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        segments: &[&str],
        subject: &str,
    ) -> Result<T> {
        let mut url = reqwest::Url::parse(&self.api_url)?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("invalid GitHub API base URL"))?
            .pop_if_empty()
            .extend(segments);

        let response = get_with_retry(&self.client, url.as_str()).await?;
        if !response.status().is_success() {
            let status = response.status();
            let hint = match status.as_u16() {
                403 | 429 => Some("Set GITHUB_TOKEN to increase the API rate limit."),
                _ => None,
            };
            return Err(anyhow!(http_error_message(
                status, "Git ref", subject, hint
            )));
        }
        Ok(response.json().await?)
    }
}

/// Tags requested per page. GitHub's maximum, so the common repository is one
/// request.
const TAG_PAGE_SIZE: usize = 100;

/// Pages walked before a commit lookup gives up and reports failure.
const MAX_TAG_PAGES: usize = 20;

/// Tags dated per release track when a repository publishes no releases.
///
/// Five is what a cooldown decision can use: the newest version, plus the four
/// it can be held back to when that one is inside the window. Dating every tag
/// of a long-lived repository would spend hundreds of requests to answer a
/// question the newest handful already settles.
const MAX_DATED_TAGS_PER_TRACK: usize = 5;

/// An RFC 3339 timestamp from the API, in UTC.
fn timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

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
    async fn pre_commit_manifest(&self, package: &str, reference: &str) -> Result<String> {
        use base64::Engine;
        let (owner, repo) = Self::extract_owner_repo(package)?;
        let mut url = url::Url::parse(&self.api_url)?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("Invalid GitHub API URL"))?
            .pop_if_empty()
            .extend(["repos", owner, repo, "contents", ".pre-commit-hooks.yaml"]);
        url.query_pairs_mut().append_pair("ref", reference);
        let mut response = get_with_retry(&self.client, url.as_str()).await?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "Cannot read hook manifest for {package}@{reference}: HTTP {}",
                response.status()
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err(anyhow!("Hook manifest response exceeds 2 MiB"));
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Contents {
            content: String,
            encoding: String,
        }
        let data: Contents = serde_json::from_slice(&bytes)?;
        if data.encoding != "base64" {
            return Err(anyhow!("Unsupported hook manifest encoding"));
        }
        let encoded: String = data
            .content
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        Ok(String::from_utf8(
            base64::engine::general_purpose::STANDARD.decode(encoded)?,
        )?)
    }

    async fn python_releases(
        &self,
        _package: &str,
    ) -> anyhow::Result<Vec<crate::registry::PythonRelease>> {
        anyhow::bail!("registry does not expose Python compatibility metadata")
    }

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
            return self.tag_versions(owner, repo).await;
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

        // A release tag that is not a version names no release of the package:
        // github/codeql-action publishes CodeQL bundles as releases beside its
        // own. Listing one lets cooldown pick it, and version ordering ranks
        // its leading letters above every number, so it would outrank the
        // real releases. `get_latest_version` filters the same way.
        let releases: Vec<VersionMeta> = items
            .into_iter()
            .filter(|r| !r.draft)
            .filter(|r| TagVersion::parse(&r.tag_name).is_some())
            .map(|r| VersionMeta {
                version: r.tag_name,
                published_at: r.published_at.as_deref().and_then(timestamp),
                yanked: false,
                prerelease: r.prerelease,
            })
            .collect();

        if !releases.is_empty() {
            return Ok(releases);
        }

        // A repository can publish tags and no releases at all, which most
        // pre-commit hook mirrors do. An empty release list is a statement
        // about releases, not about whether the repository can say when its
        // versions appeared, and reading it as the latter turns cooldown off
        // for every hook the run touches.
        self.tag_versions(owner, repo).await
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

    /// github/codeql-action publishes its CodeQL bundles as ordinary releases
    /// beside the action's own. A bundle tag is not a version of the action,
    /// and version ordering reads its leading letters as newer than any
    /// number, so a listed bundle outranks every real release.
    #[tokio::test]
    async fn a_release_whose_tag_is_not_a_version_is_not_listed() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/github/codeql-action/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
              {"tag_name": "v4.38.0", "published_at": "2026-09-09T14:04:03Z", "prerelease": false, "draft": false},
              {"tag_name": "codeql-bundle-v2.27.0", "published_at": "2026-09-09T11:31:59Z", "prerelease": false, "draft": false},
              {"tag_name": "v4.37.9", "published_at": "2026-08-26T14:41:23Z", "prerelease": false, "draft": false},
              {"tag_name": "codeql-bundle-v2.26.4", "published_at": "2026-08-26T08:21:37Z", "prerelease": false, "draft": false}
            ]"#,
            ))
            .mount(&server)
            .await;

        let versions = registry(&server)
            .list_versions("github/codeql-action")
            .await
            .unwrap();

        let names: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(names, ["v4.38.0", "v4.37.9"]);
    }

    /// A release list holding only non-version tags says nothing about when the
    /// repository's versions appeared, so the tags are dated instead, exactly
    /// as for a repository with no releases.
    #[tokio::test]
    async fn a_release_list_of_only_non_versions_dates_the_tags() {
        let sha = "1111111111111111111111111111111111111111";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/hook/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"tag_name": "images-2026-08-24T193611Z", "published_at": "2026-08-24T19:36:11Z", "prerelease": false, "draft": false}]"#,
            ))
            .mount(&server)
            .await;
        tag_list(&server, &[("v1.2.0", sha)]).await;
        lightweight_tag(&server, "v1.2.0", sha, "2026-01-02T03:04:05Z", 1).await;

        let versions = registry(&server).list_versions("acme/hook").await.unwrap();

        assert_eq!(versions.len(), 1, "{versions:?}");
        assert_eq!(versions[0].version, "v1.2.0");
        assert_eq!(
            versions[0].published_at,
            Some(chrono::Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap())
        );
    }

    /// `/releases` answering with an empty list for `acme/hook`.
    async fn no_releases(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/repos/acme/hook/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(server)
            .await;
    }

    /// `/tags` answering with `name` -> peeled commit SHA pairs.
    async fn tag_list(server: &MockServer, tags: &[(&str, &str)]) {
        let body = tags
            .iter()
            .map(|(name, sha)| format!(r#"{{"name":"{name}","commit":{{"sha":"{sha}"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        Mock::given(method("GET"))
            .and(path("/repos/acme/hook/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("[{body}]")))
            .mount(server)
            .await;
    }

    /// A lightweight tag: the ref names the commit itself, so the commit's own
    /// date is the only date git records for the version.
    async fn lightweight_tag(server: &MockServer, tag: &str, sha: &str, date: &str, lookups: u64) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/hook/git/ref/tags/{tag}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"ref":"refs/tags/{tag}","object":{{"sha":"{sha}","type":"commit"}}}}"#
            )))
            .expect(lookups)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/hook/commits/{sha}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"sha":"{sha}","commit":{{"committer":{{"date":"{date}"}}}}}}"#
            )))
            .expect(lookups)
            .mount(server)
            .await;
    }

    /// A repository that publishes tags and no releases still knows when each
    /// version appeared. Reading the empty release list as "this registry holds
    /// no publish dates" turns cooldown off for the repository, and most
    /// pre-commit hook mirrors are exactly this shape.
    #[tokio::test]
    async fn a_repository_without_releases_dates_its_tags() {
        let newer = "1111111111111111111111111111111111111111";
        let older = "2222222222222222222222222222222222222222";
        let server = MockServer::start().await;
        no_releases(&server).await;
        tag_list(&server, &[("v1.2.0", newer), ("v1.1.0", older)]).await;
        lightweight_tag(&server, "v1.2.0", newer, "2026-09-01T12:00:00Z", 1).await;
        lightweight_tag(&server, "v1.1.0", older, "2026-08-01T12:00:00Z", 1).await;

        let versions = registry(&server).list_versions("acme/hook").await.unwrap();

        assert_eq!(versions.len(), 2, "both tags are versions: {versions:?}");
        let latest = versions.iter().find(|v| v.version == "v1.2.0").unwrap();
        assert_eq!(
            latest.published_at,
            Some(Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap()),
        );
        assert!(!latest.prerelease);
        assert!(!latest.yanked);
    }

    /// An annotated tag carries its own creation time, and that is when the
    /// version became available. The commit under it can be far older - a
    /// release tagged onto a maintenance branch - and dating the version by the
    /// commit would make a release published minutes ago look weeks old, which
    /// is the one direction cooldown must never get wrong.
    #[tokio::test]
    async fn an_annotated_tag_is_dated_by_when_it_was_created() {
        let commit = "3333333333333333333333333333333333333333";
        let tag_object = "4444444444444444444444444444444444444444";
        let server = MockServer::start().await;
        no_releases(&server).await;
        tag_list(&server, &[("v2.0.0", commit)]).await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/hook/git/ref/tags/v2.0.0"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"ref":"refs/tags/v2.0.0","object":{{"sha":"{tag_object}","type":"tag"}}}}"#
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/hook/git/tags/{tag_object}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"tag":"v2.0.0","tagger":{"date":"2026-09-10T08:00:00Z"}}"#,
                ),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/hook/commits/{commit}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"sha":"{commit}","commit":{{"committer":{{"date":"2026-01-01T08:00:00Z"}}}}}}"#
            )))
            .expect(0)
            .mount(&server)
            .await;

        let versions = registry(&server).list_versions("acme/hook").await.unwrap();

        assert_eq!(
            versions.first().and_then(|v| v.published_at),
            Some(Utc.with_ymd_and_hms(2026, 9, 10, 8, 0, 0).unwrap()),
        );
    }

    /// Every date costs two requests, so the walk stops once it holds enough
    /// candidates for the cooldown decision. The tags left undated are the
    /// oldest ones, which cooldown reaches only after every newer version has
    /// been rejected as too new.
    #[tokio::test]
    async fn only_the_newest_tags_are_dated() {
        let server = MockServer::start().await;
        no_releases(&server).await;
        let shas: Vec<String> = (1..=7).map(|n| n.to_string().repeat(40)).collect();
        let names: Vec<String> = (1..=7).map(|n| format!("v1.{n}.0")).collect();
        let pairs: Vec<(&str, &str)> = names
            .iter()
            .zip(&shas)
            .map(|(n, s)| (n.as_str(), s.as_str()))
            .collect();
        tag_list(&server, &pairs).await;
        for (index, (name, sha)) in pairs.iter().enumerate() {
            // v1.1.0 and v1.2.0 are the two oldest, so they are never dated.
            let lookups = u64::from(index >= 2);
            lightweight_tag(&server, name, sha, "2026-05-01T00:00:00Z", lookups).await;
        }

        let versions = registry(&server).list_versions("acme/hook").await.unwrap();

        let mut dated: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
        dated.sort_unstable();
        assert_eq!(dated, ["v1.3.0", "v1.4.0", "v1.5.0", "v1.6.0", "v1.7.0"]);
    }

    /// Each track carries its own cap, so a prerelease pin is not answered with
    /// the stable tags alone - cooldown admits only candidates on the current
    /// version's own track and would otherwise see none. The prerelease here is
    /// the lowest-ordered tag in the repository, so one shared cap spends the
    /// whole budget on the stable track and never reaches it.
    #[tokio::test]
    async fn the_newest_tags_of_each_track_are_dated() {
        let server = MockServer::start().await;
        no_releases(&server).await;
        let shas: Vec<String> = (1..=7).map(|n| n.to_string().repeat(40)).collect();
        let names = [
            "v1.1.0",
            "v1.2.0",
            "v1.3.0",
            "v1.4.0",
            "v1.5.0",
            "v1.6.0",
            "v0.9.0-rc.1",
        ];
        let pairs: Vec<(&str, &str)> = names
            .iter()
            .zip(&shas)
            .map(|(n, s)| (*n, s.as_str()))
            .collect();
        tag_list(&server, &pairs).await;
        for (index, (name, sha)) in pairs.iter().enumerate() {
            // v1.1.0 is the sixth stable tag, one past the cap; the prerelease
            // is the only one on its own track and is always dated.
            let lookups = u64::from(index != 0);
            lightweight_tag(&server, name, sha, "2026-05-01T00:00:00Z", lookups).await;
        }

        let versions = registry(&server).list_versions("acme/hook").await.unwrap();

        let prerelease = versions
            .iter()
            .find(|v| v.version == "v0.9.0-rc.1")
            .expect("the prerelease track is dated too");
        assert!(prerelease.prerelease);
        assert!(prerelease.published_at.is_some());
        assert_eq!(versions.len(), 6);
    }

    /// A repository whose release list cannot be read at all is dated from its
    /// tags for the same reason an empty one is: absent releases say nothing
    /// about whether the repository knows when its versions appeared. The
    /// latest-release lookup already reads a missing release list this way.
    #[tokio::test]
    async fn a_missing_release_list_is_dated_from_the_tags_too() {
        let sha = "5555555555555555555555555555555555555555";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/hook/releases"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        tag_list(&server, &[("v3.1.0", sha)]).await;
        lightweight_tag(&server, "v3.1.0", sha, "2026-07-04T09:00:00Z", 1).await;

        let versions = registry(&server).list_versions("acme/hook").await.unwrap();

        assert_eq!(
            versions
                .iter()
                .map(|v| v.version.as_str())
                .collect::<Vec<_>>(),
            ["v3.1.0"],
        );
        assert_eq!(
            versions[0].published_at,
            Some(Utc.with_ymd_and_hms(2026, 7, 4, 9, 0, 0).unwrap()),
        );
    }

    /// A repository that publishes releases is answered from them alone. The
    /// tag walk exists for repositories that have no releases at all, and
    /// running it anyway would spend a request per version on every GitHub
    /// Actions pin in the tree.
    #[tokio::test]
    async fn a_repository_with_releases_does_not_walk_its_tags() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/hook/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"tag_name":"v1.2.0","published_at":"2026-09-01T12:00:00Z","prerelease":false,"draft":false}]"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/hook/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(0)
            .mount(&server)
            .await;

        let versions = registry(&server).list_versions("acme/hook").await.unwrap();

        assert_eq!(versions.len(), 1);
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
