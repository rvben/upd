use super::{
    RefNotFound, Registry, RegistryRequestError, TagsAtCommit, VersionMeta, get_with_retry,
    http_error_message,
};
use crate::version::TagVersion;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};

type ReleaseAssets = Arc<Vec<ReleaseAsset>>;
type ReleaseCache = HashMap<(String, String), Arc<OnceCell<ReleaseAssets>>>;
type AssetCache = HashMap<(String, String, String), Arc<OnceCell<Arc<Vec<u8>>>>>;
type RepoTags = Arc<Vec<TagResponse>>;
type TagCache = HashMap<(String, String), Arc<OnceCell<RepoTags>>>;

pub struct GitHubReleasesRegistry {
    client: Client,
    api_url: String,
    release_assets: Mutex<ReleaseCache>,
    asset_bytes: Mutex<AssetCache>,
    tags: Mutex<TagCache>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseAsset {
    name: String,
    #[serde(default)]
    url: String,
    digest: Option<String>,
    state: Option<String>,
}

impl ReleaseAsset {
    fn metadata(&self) -> Result<super::ReleaseAssetMetadata> {
        anyhow::ensure!(
            self.state
                .as_deref()
                .is_none_or(|state| state == "uploaded"),
            "release asset {} is not fully uploaded",
            self.name
        );
        let sha256 = self
            .digest
            .as_deref()
            .map(|digest| -> Result<String> {
                let digest = digest
                    .strip_prefix("sha256:")
                    .ok_or_else(|| anyhow!("unsupported digest algorithm for {}", self.name))?;
                anyhow::ensure!(
                    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid SHA-256 digest for {}",
                    self.name
                );
                Ok(digest.to_ascii_lowercase())
            })
            .transpose()?;
        Ok(super::ReleaseAssetMetadata { sha256 })
    }
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
struct AnnotatedTagResponse {
    sha: String,
    object: RefObject,
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
    /// Share one exact-release metadata snapshot across all linked architectures.
    /// This cache lasts only for this registry instance, never on disk.
    async fn release_assets_at_tag(&self, package: &str, tag: &str) -> Result<ReleaseAssets> {
        let key = (package.to_string(), tag.to_string());
        let cell = self
            .release_assets
            .lock()
            .await
            .entry(key)
            .or_default()
            .clone();
        let assets = cell
            .get_or_try_init(|| async {
                let (owner, repo) = Self::extract_owner_repo(package)?;
                let mut url = url::Url::parse(&self.api_url)?;
                url.path_segments_mut()
                    .map_err(|_| anyhow!("Invalid GitHub API URL"))?
                    .pop_if_empty()
                    .extend(["repos", owner, repo, "releases", "tags", tag]);
                let response = get_with_retry(&self.client, url.as_str())
                    .await
                    .map_err(RegistryRequestError::new)?;
                if !response.status().is_success() {
                    let error = anyhow!(
                        "Cannot read release {package}@{tag}: HTTP {}",
                        response.status()
                    );
                    return Err(if response.status() == reqwest::StatusCode::NOT_FOUND {
                        error
                    } else {
                        RegistryRequestError::new(error).into()
                    });
                }
                #[derive(Deserialize)]
                struct Release {
                    assets: Vec<ReleaseAsset>,
                }
                let release: Release = response
                    .json()
                    .await
                    .map_err(|error| RegistryRequestError::new(error.into()))?;
                Ok::<_, anyhow::Error>(Arc::new(release.assets))
            })
            .await?;
        Ok(Arc::clone(assets))
    }

    async fn release_asset_at_tag(
        &self,
        package: &str,
        tag: &str,
        asset_name: &str,
    ) -> Result<ReleaseAsset> {
        let assets = self.release_assets_at_tag(package, tag).await?;
        let matches: Vec<_> = assets.iter().filter(|a| a.name == asset_name).collect();
        anyhow::ensure!(
            matches.len() == 1,
            "release {package}@{tag}: expected exactly one asset named {asset_name}, found {}",
            matches.len()
        );
        Ok(matches[0].clone())
    }

    async fn download_release_asset(
        &self,
        package: &str,
        tag: &str,
        asset_name: &str,
    ) -> Result<Vec<u8>> {
        let asset = self.release_asset_at_tag(package, tag, asset_name).await?;
        let metadata = asset.metadata()?;
        anyhow::ensure!(
            !asset.url.is_empty(),
            "release asset {asset_name} has no download URL"
        );

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/octet-stream"));
        let mut asset_response =
            super::get_with_retry_and_headers(&self.client, &asset.url, Some(&headers))
                .await
                .map_err(RegistryRequestError::new)?;
        if !asset_response.status().is_success() {
            let error = anyhow!(
                "Cannot download asset {asset_name} for {package}@{tag}: HTTP {}",
                asset_response.status()
            );
            return Err(
                if asset_response.status() == reqwest::StatusCode::NOT_FOUND {
                    error
                } else {
                    RegistryRequestError::new(error).into()
                },
            );
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = asset_response
            .chunk()
            .await
            .map_err(|error| RegistryRequestError::new(error.into()))?
        {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err(anyhow!("Release asset {asset_name} response exceeds 2 MiB"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if let Some(expected) = metadata.sha256 {
            let actual: String = Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            anyhow::ensure!(
                actual == expected,
                "SHA-256 mismatch for downloaded release asset {asset_name} from {package}@{tag}"
            );
        }
        Ok(bytes)
    }

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

        Self {
            client,
            api_url,
            release_assets: Default::default(),
            asset_bytes: Default::default(),
            tags: Default::default(),
        }
    }

    /// Check `GITHUB_TOKEN`, then `GH_TOKEN`, then fall back to the token an
    /// authenticated `gh` CLI session already has cached.
    ///
    /// A user who ran `gh auth login` has a working GitHub credential sitting
    /// right there; without this fallback, `upd` ignores it and makes
    /// unauthenticated requests instead, hitting the much lower rate limit for
    /// no reason. Explicit env vars still win, so CI and other automation that
    /// sets `GITHUB_TOKEN`/`GH_TOKEN` deliberately is unaffected.
    pub fn detect_token() -> Option<String> {
        std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty()))
            .or_else(|| gh_cli_token().cloned())
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

    /// Every item of a paginated list, following `rel="next"` links for at most
    /// [`MAX_PAGES`] pages.
    ///
    /// `Ok(None)` means the first page answered 404, which some callers read as
    /// an answer. Anything short of the whole list is an error instead: a later
    /// page that fails, 404 included, a `next` link that cannot be followed,
    /// or a list longer than the cap. Answering from the pages read so far
    /// would be indistinguishable from having read them all, and the newest
    /// version is not guaranteed to be on the first page.
    async fn get_all_pages<T: serde::de::DeserializeOwned>(
        &self,
        first_url: String,
        subject: &str,
        kind: &'static str,
    ) -> Result<Option<Vec<T>>> {
        let mut url = first_url;
        let mut items = Vec::new();

        for page in 0..MAX_PAGES {
            let response = get_with_retry(&self.client, &url).await?;
            let status = response.status();
            if page == 0 && status == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(repository_error(status, subject));
            }

            let next = next_page_url(response.headers(), &self.api_url)?;
            let page_items: Vec<T> = response
                .json()
                .await
                .map_err(|e| anyhow!("Failed to parse GitHub {kind} for '{subject}': {e}"))?;
            items.extend(page_items);

            match next {
                Some(next_url) => url = next_url,
                None => return Ok(Some(items)),
            }
        }

        Err(ListTooLong {
            subject: subject.to_string(),
            kind,
        }
        .into())
    }

    /// The repository's complete tag list, read once per registry instance.
    ///
    /// Latest-version, ref-name, cooldown and commit lookups all read the same
    /// list, and each walk can cost many pages. A failure is not remembered, so
    /// a later lookup asks again rather than inheriting it.
    async fn all_tags(&self, owner: &str, repo: &str) -> Result<RepoTags> {
        let key = (owner.to_string(), repo.to_string());
        let cell = self.tags.lock().await.entry(key).or_default().clone();
        let tags = cell
            .get_or_try_init(|| async {
                let subject = format!("{owner}/{repo}");
                let url = format!(
                    "{}/repos/{}/{}/tags?per_page={}",
                    self.api_url, owner, repo, PAGE_SIZE
                );
                self.get_all_pages::<TagResponse>(url, &subject, "tags")
                    .await?
                    .map(Arc::new)
                    .ok_or_else(|| repository_error(reqwest::StatusCode::NOT_FOUND, &subject))
            })
            .await?;
        Ok(Arc::clone(tags))
    }

    /// Fetch all tags for a repo and return them as raw strings.
    async fn fetch_tags(&self, owner: &str, repo: &str) -> Result<Vec<String>> {
        Ok(self
            .all_tags(owner, repo)
            .await?
            .iter()
            .map(|t| t.name.clone())
            .collect())
    }

    /// Collect every tag naming `commit`, walking the paginated tag list.
    ///
    /// The whole list is read rather than stopping at the first match, because a
    /// commit is routinely named by several tags at once - a release `v7.0.1`
    /// beside a floating `v7` - and only the concrete one is usable. Stopping
    /// early can see the floating tag alone and report a perfectly ordinary
    /// release as unidentifiable.
    ///
    /// Reading the whole list also shows whether the repository publishes any
    /// release at all, which decides what the caller can advise about a commit
    /// no release names.
    async fn fetch_tags_at_commit(
        &self,
        owner: &str,
        repo: &str,
        commit: &str,
    ) -> Result<TagsAtCommit> {
        // The caller writes a version comment from this answer, so a repository
        // too heavily tagged to read whole gets an honest failure instead of a
        // pin annotated from a partial view, with advice fit for the pin.
        let tags = self.all_tags(owner, repo).await.map_err(|error| {
            if error.is::<ListTooLong>() {
                anyhow!(
                    "Repository '{owner}/{repo}' publishes more than {} tags; \
                     upd stopped before identifying commit {commit}. \
                     Annotate this pin by hand with the release it names.",
                    MAX_PAGES * PAGE_SIZE,
                )
            } else {
                error
            }
        })?;

        let mut names = Vec::new();
        let mut publishes_releases = false;
        for tag in tags.iter() {
            publishes_releases |= super::is_release_tag(&tag.name);
            if tag
                .commit
                .as_ref()
                .is_some_and(|c| c.sha.eq_ignore_ascii_case(commit))
            {
                names.push(tag.name.clone());
            }
        }

        Ok(if publishes_releases {
            TagsAtCommit::Known(names)
        } else {
            TagsAtCommit::NoReleases
        })
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

    async fn peel_git_target(
        &self,
        owner: &str,
        repo: &str,
        mut target: RefObject,
        subject: &str,
    ) -> Result<String> {
        // Git permits tags of tags. Peel immutable objects, never a second
        // mutable ref lookup that could disagree with the identity just read.
        for _ in 0..16 {
            anyhow::ensure!(
                target.sha.len() == 40 && target.sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "GitHub returned an invalid tag target for '{subject}'"
            );
            match target.kind.as_str() {
                "commit" => {
                    return Ok(target.sha.to_ascii_lowercase());
                }
                "tag" => {
                    let annotated: AnnotatedTagResponse = self
                        .get_json(&["repos", owner, repo, "git", "tags", &target.sha], subject)
                        .await?;
                    anyhow::ensure!(
                        annotated.sha.eq_ignore_ascii_case(&target.sha),
                        "GitHub returned a different tag object for '{subject}'"
                    );
                    target = annotated.object;
                }
                _ => {
                    return Err(super::UnusableGitTag(format!(
                        "Git tag '{subject}' does not identify a commit"
                    ))
                    .into());
                }
            }
        }
        Err(super::UnusableGitTag(format!(
            "Git tag '{subject}' is nested too deeply to verify"
        ))
        .into())
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
            let message = http_error_message(status, "Git ref", subject, hint);
            return Err(match status.as_u16() {
                404 | 422 => RefNotFound::new(message).into(),
                _ => anyhow!(message),
            });
        }
        Ok(response.json().await?)
    }
}

/// The `gh auth token` result, computed at most once per process.
///
/// A flake.lock update calls `detect_token` once per GitHub input, and a
/// resolving `RegistrySet` can be built more than once per run; without this
/// cache, each of those would spawn its own `gh` subprocess for a value that
/// never changes mid-run.
fn gh_cli_token() -> Option<&'static String> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN.get_or_init(token_from_gh_cli).as_ref()
}

/// Run `gh auth token` and return its trimmed stdout, or `None` if the CLI is
/// missing, unauthenticated, or the subprocess otherwise fails - any of which
/// just means this fallback has nothing to offer, not an error worth
/// surfacing to the caller.
fn token_from_gh_cli() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    parse_gh_token_output(output.status.success(), &output.stdout)
}

/// Pure parsing of a `gh auth token` subprocess result, split out from
/// [`token_from_gh_cli`] so the decision logic is testable without spawning a
/// real `gh` process (which may not be installed or authenticated in CI).
fn parse_gh_token_output(status_success: bool, stdout: &[u8]) -> Option<String> {
    if !status_success {
        return None;
    }
    let token = String::from_utf8_lossy(stdout);
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Items requested per page of a tag or release list. GitHub's maximum, so the
/// common repository is one request.
const PAGE_SIZE: usize = 100;

/// Pages walked before a list read gives up and reports failure.
const MAX_PAGES: usize = 20;

/// A list longer than [`MAX_PAGES`] pages, which is reported rather than
/// answered from the part read.
#[derive(Debug)]
struct ListTooLong {
    subject: String,
    kind: &'static str,
}

impl std::fmt::Display for ListTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Repository '{}' publishes more than {} {}; upd will not answer from a partial list.",
            self.subject,
            MAX_PAGES * PAGE_SIZE,
            self.kind,
        )
    }
}

impl std::error::Error for ListTooLong {}

/// A failed repository request, with the token hint when it was throttled.
fn repository_error(status: reqwest::StatusCode, subject: &str) -> anyhow::Error {
    let hint = match status.as_u16() {
        403 | 429 => Some("Set GITHUB_TOKEN to increase the API rate limit."),
        _ => None,
    };
    anyhow!(http_error_message(status, "Repository", subject, hint))
}

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

/// The `rel="next"` URL from a `Link` header, or `None` on the last page.
///
/// The URL is only followed when it addresses the same origin the request went
/// to, so a redirecting proxy in front of the API cannot walk the client, and
/// its `Authorization` header, onto a host the user never configured. A next
/// link that is unreadable or leaves that origin is an error, not the end of
/// the list: stopping there would answer from the pages read so far.
fn next_page_url(headers: &reqwest::header::HeaderMap, api_url: &str) -> Result<Option<String>> {
    let mut candidate = None;
    for link in headers.get_all(reqwest::header::LINK) {
        let link = link
            .to_str()
            .map_err(|_| anyhow!("GitHub returned an unreadable pagination Link header"))?;
        for entry in link.split(',') {
            let (target, params) = entry
                .split_once(';')
                .ok_or_else(|| anyhow!("GitHub returned a malformed pagination Link header"))?;
            let target = target
                .trim()
                .strip_prefix('<')
                .and_then(|t| t.strip_suffix('>'))
                .ok_or_else(|| anyhow!("GitHub returned a malformed pagination link"))?;
            let mut is_next = false;
            for param in params.split(';') {
                let (name, value) = param.trim().split_once('=').ok_or_else(|| {
                    anyhow!("GitHub returned a malformed pagination link parameter")
                })?;
                let value = value.trim();
                let value = if let Some(quoted) = value.strip_prefix('"') {
                    quoted.strip_suffix('"').ok_or_else(|| {
                        anyhow!("GitHub returned an unterminated pagination link parameter")
                    })?
                } else {
                    value
                };
                is_next |=
                    name.trim() == "rel" && value.split_ascii_whitespace().any(|rel| rel == "next");
            }
            if is_next {
                anyhow::ensure!(
                    candidate.is_none(),
                    "GitHub returned multiple next-page links"
                );
                candidate = Some(target.to_string());
            }
        }
    }
    let Some(candidate) = candidate else {
        return Ok(None);
    };

    let base = reqwest::Url::parse(api_url)?;
    let next = reqwest::Url::parse(&candidate)
        .map_err(|e| anyhow!("GitHub returned a malformed next-page link: {e}"))?;
    anyhow::ensure!(
        next.scheme() == base.scheme()
            && next.host_str() == base.host_str()
            && next.port_or_known_default() == base.port_or_known_default()
            && next.username().is_empty()
            && next.password().is_none(),
        "GitHub returned a next-page link to another origin; refusing to follow it"
    );
    Ok(Some(candidate))
}

impl Default for GitHubReleasesRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for GitHubReleasesRegistry {
    async fn resolve_git_tag(&self, package: &str, tag: &str) -> Result<super::ResolvedGitTag> {
        let (owner, repo) = Self::extract_owner_repo(package)?;
        let subject = format!("{owner}/{repo}@{tag}");
        let reference: RefResponse = self
            .get_json(&["repos", owner, repo, "git", "ref", "tags", tag], &subject)
            .await?;
        let target = reference.object;
        let object = target.sha.to_ascii_lowercase();
        let commit = self.peel_git_target(owner, repo, target, &subject).await?;
        Ok(super::ResolvedGitTag { object, commit })
    }

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
        if response.status() == reqwest::StatusCode::NOT_FOUND
            && reference.len() == 40
            && reference.bytes().all(|b| b.is_ascii_hexdigit())
        {
            // Contents may not accept an annotated tag object's SHA. This is
            // also needed when only a hook dependency is selected: its rev
            // stays untouched, but its language must still be read at that pin.
            let commit = self
                .peel_git_target(
                    owner,
                    repo,
                    RefObject {
                        sha: reference.into(),
                        kind: "tag".into(),
                    },
                    &format!("{package}@{reference}"),
                )
                .await?;
            url.query_pairs_mut().clear().append_pair("ref", &commit);
            response = get_with_retry(&self.client, url.as_str()).await?;
        }
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

    async fn repo_file_at_ref(
        &self,
        package: &str,
        reference: &str,
        path: &str,
    ) -> Result<Vec<u8>> {
        use base64::Engine;
        let (owner, repo) = Self::extract_owner_repo(package)?;
        let mut url = url::Url::parse(&self.api_url)?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow!("Invalid GitHub API URL"))?;
            segments
                .pop_if_empty()
                .extend(["repos", owner, repo, "contents"]);
            for segment in path.split('/') {
                segments.push(segment);
            }
        }
        url.query_pairs_mut().append_pair("ref", reference);
        let mut response = get_with_retry(&self.client, url.as_str()).await?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "Cannot read {path} for {package}@{reference}: HTTP {}",
                response.status()
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err(anyhow!("{path} response exceeds 2 MiB"));
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
            return Err(anyhow!("Unsupported content encoding for {path}"));
        }
        let encoded: String = data
            .content
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        Ok(base64::engine::general_purpose::STANDARD.decode(encoded)?)
    }

    async fn release_asset_metadata(
        &self,
        package: &str,
        tag: &str,
        asset_name: &str,
    ) -> Result<super::ReleaseAssetMetadata> {
        self.release_asset_at_tag(package, tag, asset_name)
            .await?
            .metadata()
    }

    async fn release_asset_digest(
        &self,
        package: &str,
        tag: &str,
        asset_name: &str,
    ) -> Result<String> {
        self.release_asset_metadata(package, tag, asset_name).await?.sha256
            .ok_or_else(|| anyhow!("release {package}@{tag}: SHA-256 digest unavailable for {asset_name}; specify checksums=<release asset>"))
    }

    async fn release_asset(&self, package: &str, tag: &str, asset_name: &str) -> Result<Vec<u8>> {
        let key = (package.to_string(), tag.to_string(), asset_name.to_string());
        let cell = self
            .asset_bytes
            .lock()
            .await
            .entry(key)
            .or_default()
            .clone();
        let bytes = cell
            .get_or_try_init(|| async {
                self.download_release_asset(package, tag, asset_name)
                    .await
                    .map(Arc::new)
            })
            .await?;
        Ok(bytes.as_ref().clone())
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
        self.fetch_tags_at_commit(owner, repo, commit).await
    }

    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        self.list_versions_for_cooldown(package, false).await
    }

    async fn list_versions_for_cooldown(
        &self,
        package: &str,
        strict: bool,
    ) -> Result<Vec<VersionMeta>> {
        let (owner, repo) = Self::extract_owner_repo(package)?;
        let url = format!(
            "{}/repos/{}/{}/releases?per_page={}",
            self.api_url, owner, repo, PAGE_SIZE
        );

        // Only a first page answering 404 says the repository has no release
        // list; a later page failing says nothing about the releases already
        // read, and the walk reports it as an error.
        let Some(items) = self
            .get_all_pages::<ReleaseListEntry>(url, &format!("{owner}/{repo}"), "releases")
            .await?
        else {
            return if strict {
                Ok(Vec::new())
            } else {
                self.tag_versions(owner, repo).await
            };
        };

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
        if strict {
            Ok(Vec::new())
        } else {
            self.tag_versions(owner, repo).await
        }
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

    #[tokio::test]
    async fn exact_tag_resolution_retains_object_identity_and_peels_nested_tags() {
        let server = MockServer::start().await;
        let first = "1111111111111111111111111111111111111111";
        let second = "2222222222222222222222222222222222222222";
        let commit = "3333333333333333333333333333333333333333";
        Mock::given(path("/repos/acme/hook/git/ref/tags/v1.2.3"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"object": {"sha": first, "type": "tag"}})),
            )
            .mount(&server)
            .await;
        for (object, target, kind) in [(first, second, "tag"), (second, commit, "commit")] {
            Mock::given(path(format!("/repos/acme/hook/git/tags/{object}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"sha": object, "object": {"sha": target, "type": kind}}),
                ))
                .mount(&server)
                .await;
        }
        let tag = registry(&server)
            .resolve_git_tag("acme/hook", "v1.2.3")
            .await
            .unwrap();
        assert_eq!(tag.object, first);
        assert_eq!(tag.commit, commit);
    }

    #[tokio::test]
    async fn exact_tag_resolution_rejects_noncommit_targets_and_malformed_objects() {
        for (object, kind, response_sha) in [
            (
                "1111111111111111111111111111111111111111",
                "blob",
                "1111111111111111111111111111111111111111",
            ),
            ("short", "commit", "short"),
            (
                "1111111111111111111111111111111111111111",
                "tag",
                "2222222222222222222222222222222222222222",
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(path("/repos/acme/hook/git/ref/tags/v1.2.3"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({"object": {"sha": object, "type": kind}}),
                    ),
                )
                .mount(&server)
                .await;
            Mock::given(path(format!("/repos/acme/hook/git/tags/{object}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"sha": response_sha, "object": {"sha": response_sha, "type": "commit"}})))
                .mount(&server).await;
            assert!(
                registry(&server)
                    .resolve_git_tag("acme/hook", "v1.2.3")
                    .await
                    .is_err()
            );
        }
    }

    #[test]
    fn pagination_relations_accept_spacing_tokens_and_multiple_headers() {
        let base = "https://api.example.com";
        let mut headers = HeaderMap::new();
        headers.append(
            "link",
            HeaderValue::from_static("<https://api.example.com/tags?page=1>; rel=prev"),
        );
        headers.append(
            "link",
            HeaderValue::from_static("<https://api.example.com/tags?page=3>; rel = \"next last\""),
        );
        assert_eq!(
            next_page_url(&headers, base).unwrap().as_deref(),
            Some("https://api.example.com/tags?page=3")
        );
        for malformed in [
            "garbage",
            "<https://api.example.com/tags>; rel=\"next",
            "<https://api.example.com/tags>; rel=next; broken",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("link", HeaderValue::from_str(malformed).unwrap());
            assert!(next_page_url(&headers, base).is_err(), "{malformed}");
        }
    }

    #[test]
    fn parse_gh_token_output_accepts_a_trimmed_successful_token() {
        // gh prints the token followed by a trailing newline.
        assert_eq!(
            parse_gh_token_output(true, b"gho_abc123\n"),
            Some("gho_abc123".to_string())
        );
    }

    #[test]
    fn parse_gh_token_output_rejects_a_failed_invocation() {
        // Non-zero exit (not installed, not logged in) must not hand back
        // whatever happened to be on stdout.
        assert_eq!(parse_gh_token_output(false, b"gho_abc123\n"), None);
    }

    #[test]
    fn parse_gh_token_output_rejects_empty_or_blank_stdout() {
        assert_eq!(parse_gh_token_output(true, b""), None);
        assert_eq!(parse_gh_token_output(true, b"\n"), None);
        assert_eq!(parse_gh_token_output(true, b"   \n"), None);
    }

    #[test]
    fn detect_token_prefers_github_token_over_the_gh_cli_fallback() {
        // With GITHUB_TOKEN set, `Option::or_else`'s laziness means the gh
        // CLI branch is never even evaluated, so this is safe to assert
        // without a real `gh` binary on PATH.
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "env-token");
        }
        assert_eq!(
            GitHubReleasesRegistry::detect_token(),
            Some("env-token".to_string())
        );
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }
    }

    #[test]
    fn detect_token_falls_back_to_gh_token_over_the_gh_cli_fallback() {
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::set_var("GH_TOKEN", "gh-env-token");
        }
        assert_eq!(
            GitHubReleasesRegistry::detect_token(),
            Some("gh-env-token".to_string())
        );
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::remove_var("GH_TOKEN");
        }
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
                r#"[{"name": "v1", "commit": {"sha": "abcdefabcdefabcdefabcdefabcdefabcdefabcd"}},
                    {"name": "v1.0.0", "commit": {"sha": "abcdefabcdefabcdefabcdefabcdefabcdefabcd"}}]"#,
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

    /// dtolnay/rust-toolchain publishes only a moving `v1` and per-toolchain
    /// branches. With no release anywhere in the repository, re-pinning cannot
    /// help, and the answer has to say so rather than look like a commit that
    /// merely sits between releases.
    #[tokio::test]
    async fn a_repository_without_releases_says_so_wherever_the_commit_is() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let other = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        for (case, body) in [
            (
                "off every tag",
                format!(r#"[{{"name": "v1", "commit": {{"sha": "{other}"}}}}]"#),
            ),
            (
                "at the floating tag",
                format!(r#"[{{"name": "v1", "commit": {{"sha": "{sha}"}}}}]"#),
            ),
            ("no tags at all", "[]".to_string()),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/acme/action/tags"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;

            assert_eq!(
                registry(&server)
                    .tags_at_commit("acme/action", sha)
                    .await
                    .unwrap(),
                TagsAtCommit::NoReleases,
                "{case}"
            );
        }
    }

    /// A release on an earlier page still counts once later pages hold only
    /// aliases, or a heavily tagged repository would be told it has none.
    #[tokio::test]
    async fn a_release_on_any_page_means_the_repository_publishes_releases() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        let other = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/acme/action/tags"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"[{{"name": "v1", "commit": {{"sha": "{other}"}}}}]"#
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
    /// `Authorization` header, onto a host the user never configured. Nor may
    /// the walk stop there and answer, since the unread page can hold the tag
    /// that matters; a next link that cannot be read at all is the same case.
    #[tokio::test]
    async fn pagination_fails_on_a_next_link_it_cannot_follow() {
        let sha = "1234567890abcdef1234567890abcdef12345678";
        for link in [
            r#"<https://attacker.example/repos/acme/action/tags?page=2>; rel="next""#,
            r#"https://attacker.example/repos/acme/action/tags?page=2; rel="next""#,
            r#"<not a url>; rel="next""#,
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/acme/action/tags"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("link", link)
                        .set_body_string(
                            r#"[{"name": "v1.0.0", "commit": {"sha": "abcdefabcdefabcdefabcdefabcdefabcdefabcd"}}]"#,
                        ),
                )
                .expect(2)
                .mount(&server)
                .await;

            let registry = registry(&server);
            assert!(
                registry.tags_at_commit("acme/action", sha).await.is_err(),
                "{link}"
            );
            // The failed walk is not remembered, so this asks again: the mock
            // expects both requests.
            assert!(
                registry.list_ref_names("acme/action").await.is_err(),
                "{link}"
            );
        }
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

        let registry = registry(&server);
        let error = registry
            .tags_at_commit("acme/action", "1234567890abcdef1234567890abcdef12345678")
            .await
            .expect_err("a tag list that never ends must not answer 'no tags'");

        assert!(
            error.to_string().contains("more than"),
            "the error must say the list was too long to read: {error}"
        );

        // The latest-version lookup reads the same list and must not pick a
        // newest tag from the part it read.
        let error = registry
            .get_latest_version_including_prereleases("acme/action")
            .await
            .expect_err("a tag list that never ends must not answer from part of it");
        assert!(error.to_string().contains("more than"), "{error}");
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

    /// The first page of `acme/hook`'s `list` (`tags` or `releases`), answering
    /// `body` and linking to page 2.
    async fn first_page(server: &MockServer, list: &str, body: &str, requests: u64) {
        let next = format!(
            "{}/repos/acme/hook/{list}?per_page=100&page=2",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/hook/{list}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", format!(r#"<{next}>; rel="next""#).as_str())
                    .set_body_string(body),
            )
            .expect(requests)
            .mount(server)
            .await;
    }

    /// Page 2 of `acme/hook`'s `list`, outranking the first page's mock.
    fn second_page(list: &str, response: ResponseTemplate) -> Mock {
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/hook/{list}")))
            .and(query_param("page", "2"))
            .respond_with(response)
            .with_priority(1)
    }

    /// GitHub does not promise the newest tag is on the first page, so every
    /// lookup over the tag list reads all of it. The list is read once however
    /// many lookups ask: each page mock expects a single request.
    #[tokio::test]
    async fn the_newest_tags_on_a_later_page_are_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/hook/releases/latest"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        second_page(
            "tags",
            ResponseTemplate::new(200).set_body_string(
                r#"[{"name": "v2.0.0"}, {"name": "v3.0.0-rc.1"}, {"name": "v1"}]"#,
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
        first_page(&server, "tags", r#"[{"name": "v1.0.0"}]"#, 1).await;

        let registry = registry(&server);
        assert_eq!(
            registry.get_latest_version("acme/hook").await.unwrap(),
            "v2.0.0"
        );
        assert_eq!(
            registry
                .get_latest_version_including_prereleases("acme/hook")
                .await
                .unwrap(),
            "v3.0.0-rc.1"
        );
        assert_eq!(
            registry.list_ref_names("acme/hook").await.unwrap(),
            ["v1.0.0", "v2.0.0", "v3.0.0-rc.1", "v1"]
        );
    }

    /// A later page that fails leaves the list unread, not shorter: answering
    /// from the first page would offer a stale latest version. The failure is
    /// not remembered, so the next lookup reads the list afresh.
    #[tokio::test]
    async fn a_failed_later_tag_page_fails_the_lookup_until_it_succeeds() {
        let server = MockServer::start().await;
        let failure = second_page("tags", ResponseTemplate::new(403))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        first_page(&server, "tags", r#"[{"name": "v1.0.0"}]"#, 2).await;
        let registry = registry(&server);

        assert!(
            registry
                .get_latest_version_including_prereleases("acme/hook")
                .await
                .is_err()
        );

        drop(failure);
        second_page(
            "tags",
            ResponseTemplate::new(200).set_body_string(r#"[{"name": "v2.0.0"}]"#),
        )
        .expect(1)
        .mount(&server)
        .await;
        assert_eq!(
            registry
                .get_latest_version_including_prereleases("acme/hook")
                .await
                .unwrap(),
            "v2.0.0"
        );
    }

    /// Cooldown holds an update back to an older release when the newest is
    /// too new, and that older release can sit past the first page.
    #[tokio::test]
    async fn an_older_release_on_a_later_page_is_listed() {
        let server = MockServer::start().await;
        second_page(
            "releases",
            ResponseTemplate::new(200).set_body_string(
                r#"[{"tag_name":"v1.0.0","published_at":"2026-01-01T00:00:00Z","prerelease":false,"draft":false}]"#,
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
        first_page(
            &server,
            "releases",
            r#"[{"tag_name":"v2.0.0","published_at":"2026-10-14T00:00:00Z","prerelease":false,"draft":false}]"#,
            1,
        )
        .await;

        let versions = registry(&server)
            .list_versions_for_cooldown("acme/hook", true)
            .await
            .unwrap();

        let names: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(names, ["v2.0.0", "v1.0.0"]);
        assert_eq!(
            versions[1].published_at,
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
        );
    }

    /// Only a release list missing outright sends cooldown to the tags. A later
    /// page answering 404 says nothing about the releases already read, so it
    /// is an error rather than licence to date tags instead.
    #[tokio::test]
    async fn a_later_release_page_answering_404_is_an_error() {
        let server = MockServer::start().await;
        second_page("releases", ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        first_page(
            &server,
            "releases",
            r#"[{"tag_name":"v2.0.0","published_at":"2026-10-14T00:00:00Z","prerelease":false,"draft":false}]"#,
            1,
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/hook/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(0)
            .mount(&server)
            .await;

        assert!(registry(&server).list_versions("acme/hook").await.is_err());
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
    async fn strict_metadata_never_requests_tag_dates() {
        for status in [200, 404] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/acme/hook/releases"))
                .respond_with(ResponseTemplate::new(status).set_body_string("[]"))
                .expect(1)
                .mount(&server)
                .await;
            let versions = registry(&server)
                .list_versions_for_cooldown("acme/hook", true)
                .await
                .unwrap();
            assert!(versions.is_empty());
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
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

    /// `repo_file_at_ref` and `release_asset` back the self-pin updater's
    /// checksum verification and, before this test, had never been exercised
    /// against an HTTP response of the shape GitHub actually returns: base64
    /// content from the contents API and a two-hop release/asset lookup.
    #[tokio::test]
    async fn repo_file_at_ref_decodes_base64_content_from_the_contents_api() {
        use base64::Engine;
        let server = MockServer::start().await;
        let body = b"{\"version\":\"v0.14.2\"}";
        let encoded = base64::engine::general_purpose::STANDARD.encode(body);

        Mock::given(method("GET"))
            .and(path("/repos/rvben/upd/contents/release-pins.json"))
            .and(query_param("ref", "v0.14.2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!(r#"{{"content":"{encoded}","encoding":"base64"}}"#)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let bytes = registry(&server)
            .repo_file_at_ref("rvben/upd", "v0.14.2", "release-pins.json")
            .await
            .unwrap();
        assert_eq!(bytes, body);
    }

    #[tokio::test]
    async fn repo_file_at_ref_errors_when_the_contents_api_returns_not_found() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/rvben/upd/contents/release-pins.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let result = registry(&server)
            .repo_file_at_ref("rvben/upd", "v0.14.2", "release-pins.json")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn release_asset_digest_uses_exact_asset_and_shares_release_metadata() {
        let server = MockServer::start().await;
        let digest = "a".repeat(64);
        Mock::given(method("GET"))
            .and(path("/repos/jdx/mise/releases/tags/v2026.9.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "assets": [
                    {"name": "mise-linux-x64.tar.gz", "digest": format!("sha256:{digest}")},
                    {"name": "mise-linux-arm64.tar.gz", "digest": format!("sha256:{}", "b".repeat(64))}
                ]
            }))).expect(1).mount(&server).await;
        let registry = registry(&server);
        assert_eq!(
            registry
                .release_asset_digest("jdx/mise", "v2026.9.1", "mise-linux-x64.tar.gz")
                .await
                .unwrap(),
            digest
        );
        assert_eq!(
            registry
                .release_asset_digest("jdx/mise", "v2026.9.1", "mise-linux-arm64.tar.gz")
                .await
                .unwrap(),
            "b".repeat(64)
        );
        assert!(
            registry
                .release_asset_digest("jdx/mise", "v2026.9.1", "missing.tar.gz")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn concurrent_release_metadata_and_manifest_downloads_share_one_snapshot() {
        let server = MockServer::start().await;
        let bytes = b"published checksums\n";
        let digest = "87d978ba1360408b53459bf7c4ca157fdf5639a4ef89b429ce93a82921290ae3";
        Mock::given(method("GET"))
            .and(path("/repos/acme/tool/releases/tags/v1.2.3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"assets":[
                {"name":"sums.txt", "state":"uploaded", "digest":format!("sha256:{digest}"), "url":format!("{}/sums", server.uri())}
            ]})).set_delay(Duration::from_millis(30)))
            .expect(1).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/sums"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bytes.to_vec())
                    .set_delay(Duration::from_millis(30)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let registry = registry(&server);
        let results = futures::future::join_all((0..16).map(|_| async {
            let metadata = registry
                .release_asset_metadata("acme/tool", "v1.2.3", "sums.txt")
                .await
                .unwrap();
            let downloaded = registry
                .release_asset("acme/tool", "v1.2.3", "sums.txt")
                .await
                .unwrap();
            (metadata.sha256.unwrap(), downloaded)
        }))
        .await;
        for (reported, downloaded) in results {
            assert_eq!(reported, digest);
            assert_eq!(downloaded, bytes);
        }
    }

    #[tokio::test]
    async fn release_asset_refuses_bad_metadata_before_download_and_mismatched_bytes_afterward() {
        for (state, digest, downloads, message) in [
            (
                "starter",
                format!("sha256:{}", "a".repeat(64)),
                0,
                "not fully uploaded",
            ),
            ("uploaded", "sha256:invalid".into(), 0, "invalid SHA-256"),
            (
                "uploaded",
                format!("sha512:{}", "a".repeat(64)),
                0,
                "unsupported digest algorithm",
            ),
            (
                "uploaded",
                format!("sha256:{}", "a".repeat(64)),
                1,
                "SHA-256 mismatch",
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET")).and(path("/repos/acme/tool/releases/tags/v1.2.3"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"assets":[
                    {"name":"sums.txt", "state":state, "digest":digest, "url":format!("{}/sums", server.uri())}
                ]}))).expect(1).mount(&server).await;
            Mock::given(method("GET"))
                .and(path("/sums"))
                .respond_with(ResponseTemplate::new(200).set_body_string("different bytes"))
                .expect(downloads)
                .mount(&server)
                .await;
            let error = registry(&server)
                .release_asset("acme/tool", "v1.2.3", "sums.txt")
                .await
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[tokio::test]
    async fn failed_asset_downloads_are_not_cached_and_can_be_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/tool/releases/tags/v1.2.3"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"assets":[
                    {"name":"sums.txt", "state":"uploaded", "url":format!("{}/sums", server.uri())}
                ]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let registry = registry(&server);
        let failure = Mock::given(method("GET"))
            .and(path("/sums"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        assert!(
            registry
                .release_asset("acme/tool", "v1.2.3", "sums.txt")
                .await
                .is_err()
        );
        drop(failure);
        Mock::given(method("GET"))
            .and(path("/sums"))
            .respond_with(ResponseTemplate::new(200).set_body_string("valid manifest"))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            registry
                .release_asset("acme/tool", "v1.2.3", "sums.txt")
                .await
                .unwrap(),
            b"valid manifest"
        );
        assert_eq!(
            registry
                .release_asset("acme/tool", "v1.2.3", "sums.txt")
                .await
                .unwrap(),
            b"valid manifest"
        );
    }

    #[tokio::test]
    async fn release_asset_metadata_verifies_presence_and_upload_state_without_requiring_a_digest()
    {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/tool/releases/tags/v1.2.3"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"assets":[
                    {"name":"legacy.tar.gz", "state":"uploaded", "digest":null},
                    {"name":"uploading.tar.gz", "state":"starter", "digest":null}
                ]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let registry = registry(&server);
        assert!(
            registry
                .release_asset_metadata("acme/tool", "v1.2.3", "legacy.tar.gz")
                .await
                .unwrap()
                .sha256
                .is_none()
        );
        assert!(
            registry
                .release_asset_metadata("acme/tool", "v1.2.3", "missing.tar.gz")
                .await
                .is_err()
        );
        assert!(
            registry
                .release_asset_metadata("acme/tool", "v1.2.3", "uploading.tar.gz")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn release_asset_digest_refuses_absent_malformed_and_ambiguous_digests() {
        for assets in [
            serde_json::json!([{"name":"asset", "digest":null}]),
            serde_json::json!([{"name":"asset"}]),
            serde_json::json!([{"name":"asset", "digest":"sha256:broken"}]),
            serde_json::json!([{"name":"asset", "digest":format!("sha512:{}", "a".repeat(64))}]),
            serde_json::json!([
                {"name":"asset", "digest":format!("sha256:{}", "a".repeat(64))},
                {"name":"asset", "digest":format!("sha256:{}", "b".repeat(64))}
            ]),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/repos/acme/tool/releases/tags/v1.2.3"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"assets":assets})),
                )
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                registry(&server)
                    .release_asset_digest("acme/tool", "v1.2.3", "asset")
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn release_asset_follows_the_release_lookup_to_the_asset_url() {
        let server = MockServer::start().await;
        let asset_bytes = b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef  upd-v0.14.2.tar.gz\n";
        let asset_url = format!("{}/assets/download-me", server.uri());

        Mock::given(method("GET"))
            .and(path("/repos/rvben/upd/releases/tags/v0.14.2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"assets":[{{"name":"upd-v0.14.2.tar.gz.sha256","url":"{asset_url}"}}]}}"#
            )))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/assets/download-me"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(asset_bytes.to_vec())
                    .insert_header("content-type", "application/octet-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let bytes = registry(&server)
            .release_asset("rvben/upd", "v0.14.2", "upd-v0.14.2.tar.gz.sha256")
            .await
            .unwrap();
        assert_eq!(bytes, asset_bytes);
    }

    #[tokio::test]
    async fn release_asset_errors_when_no_asset_matches_the_requested_name() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/rvben/upd/releases/tags/v0.14.2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"assets":[{"name":"some-other-asset.tar.gz","url":"https://example.invalid/x"}]}"#,
            ))
            .mount(&server)
            .await;

        let result = registry(&server)
            .release_asset("rvben/upd", "v0.14.2", "upd-v0.14.2.tar.gz.sha256")
            .await;
        assert!(result.is_err());
    }
}
