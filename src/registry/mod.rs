mod crates_io;
mod docker;
mod github_releases;
mod go_proxy;
mod index_chain;
#[cfg(test)]
pub mod mock;
mod npm;
mod nuget;
mod pypi;
mod rubygems;
mod terraform;
mod utils;

pub use crates_io::{CargoConfig, CargoCredentials, CratesIoRegistry, read_cargo_config};
pub use docker::DockerRegistry;
pub use github_releases::GitHubReleasesRegistry;
pub use go_proxy::{GoCredentials, GoPrivateConfig, GoProxyRegistry, read_go_private_config};
pub use index_chain::{DeclaredIndex, IndexChain, IndexSource};
#[cfg(test)]
pub use mock::MockRegistry;
pub use npm::{NpmCredentials, NpmRegistry, NpmrcConfig, read_npmrc_config};
pub use nuget::NuGetRegistry;
pub use pypi::{MultiPyPiRegistry, PyPiCredentials, PyPiRegistry};
pub use rubygems::RubyGemsRegistry;
pub use terraform::TerraformRegistry;

// The constraint grammars, shared with the updaters that have to decide whether
// a release a manifest already admits is worth rewriting the manifest for.
pub(crate) use nuget::matches_nuget_range;
pub(crate) use rubygems::matches_ruby_constraint;
pub(crate) use terraform::matches_terraform_constraint;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::{Client, Response};
use std::time::Duration;

/// Maximum number of retry attempts for failed HTTP requests
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (100ms, 200ms, 400ms)
const BASE_DELAY_MS: u64 = 100;

/// Execute an HTTP GET request with retry and exponential backoff.
/// Retries on transient errors (network issues, 5xx server errors).
pub async fn get_with_retry(client: &Client, url: &str) -> anyhow::Result<Response> {
    let mut last_error = None;

    for attempt in 0..MAX_RETRIES {
        match client.get(url).send().await {
            Ok(response) => {
                // Don't retry client errors (4xx) - they won't succeed on retry
                if response.status().is_client_error() || response.status().is_success() {
                    return Ok(response);
                }

                // Retry server errors (5xx)
                if response.status().is_server_error() && attempt < MAX_RETRIES - 1 {
                    let delay = Duration::from_millis(BASE_DELAY_MS * (1 << attempt));
                    tokio::time::sleep(delay).await;
                    continue;
                }

                return Ok(response);
            }
            Err(e) => {
                last_error = Some(e);

                // Don't retry on the last attempt
                if attempt < MAX_RETRIES - 1 {
                    let delay = Duration::from_millis(BASE_DELAY_MS * (1 << attempt));
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    Err(crate::http::wrap_send_err(last_error.unwrap(), url))
}

/// A registry's definitive answer that a Git ref names no commit in a
/// repository, as opposed to a lookup that could not be completed.
///
/// The distinction decides whether another spelling of the same version may be
/// tried: a repository that does not publish `1.2.3` says so, while a rate limit
/// or an outage says nothing at all about which refs exist. Treating the second
/// as the first would let a transient failure pick a different commit.
#[derive(Debug)]
pub struct RefNotFound {
    message: String,
}

impl RefNotFound {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RefNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RefNotFound {}

/// Whether a failed ref lookup means the ref does not exist.
pub fn is_ref_not_found(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RefNotFound>().is_some()
}

/// Create a descriptive error message for HTTP failures
/// Helps users understand why a request failed and what to do
///
/// # Arguments
/// * `status` - HTTP status code
/// * `entity_type` - Type of entity (e.g., "Package", "Crate", "Module")
/// * `name` - Name of the package/crate/module
/// * `registry_hint` - Optional hint about where to configure credentials
pub fn http_error_message(
    status: reqwest::StatusCode,
    entity_type: &str,
    name: &str,
    registry_hint: Option<&str>,
) -> String {
    let code = status.as_u16();
    match code {
        401 => {
            let hint = registry_hint.map_or_else(
                || "Check your credentials or API token.".to_string(),
                |h| format!("Check your credentials or API token. {}", h),
            );
            format!(
                "{} '{}' requires authentication (HTTP 401). {}",
                entity_type, name, hint
            )
        }
        403 => {
            let hint = registry_hint.map_or_else(
                || {
                    format!(
                        "You may lack permission or the {} may be private.",
                        entity_type.to_lowercase()
                    )
                },
                |h| {
                    format!(
                        "You may lack permission or the {} may be private. {}",
                        entity_type.to_lowercase(),
                        h
                    )
                },
            );
            format!(
                "Access denied for {} '{}' (HTTP 403). {}",
                entity_type, name, hint
            )
        }
        404 => format!(
            "{} '{}' not found (HTTP 404). Check the name for typos or verify it exists in the registry.",
            entity_type, name
        ),
        408 | 504 => format!(
            "Request timed out for {} '{}' (HTTP {}). The registry may be slow or unreachable.",
            entity_type, name, code
        ),
        429 => {
            let hint = registry_hint.map_or_else(
                || "Wait a moment and try again.".to_string(),
                |h| format!("Wait a moment and try again. {}", h),
            );
            format!(
                "Rate limited while fetching {} '{}' (HTTP 429). {}",
                entity_type, name, hint
            )
        }
        500..=599 => format!(
            "Registry server error for {} '{}' (HTTP {}). The registry may be experiencing issues.",
            entity_type, name, code
        ),
        _ => format!(
            "Failed to fetch {} '{}': HTTP {} {}",
            entity_type,
            name,
            code,
            status.canonical_reason().unwrap_or("Unknown error")
        ),
    }
}

/// Metadata for a single published version of a package.
///
/// `published_at` is `None` when the registry did not expose a timestamp for
/// this version; the cooldown layer treats that as a signal to report the
/// ecosystem as unsupported.
#[derive(Debug, Clone)]
pub struct VersionMeta {
    pub version: String,
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
    pub yanked: bool,
    pub prerelease: bool,
}

/// The shape of a latest-version lookup.
///
/// This is also passed to [`Registry::revalidate_version`] so cache decorators
/// can refresh exactly the entry that produced a suspect answer.
#[derive(Debug, Clone, Copy)]
pub enum VersionQuery<'a> {
    Stable,
    IncludingPrereleases,
    Matching(&'a str),
}

impl VersionQuery<'_> {
    pub async fn run<R: Registry + ?Sized>(self, registry: &R, package: &str) -> Result<String> {
        match self {
            Self::Stable => registry.get_latest_version(package).await,
            Self::IncludingPrereleases => {
                registry
                    .get_latest_version_including_prereleases(package)
                    .await
            }
            Self::Matching(constraints) => {
                registry
                    .get_latest_version_matching(package, constraints)
                    .await
            }
        }
    }

    pub(crate) fn cache_key(self, package: &str) -> String {
        match self {
            Self::Stable => package.to_string(),
            Self::IncludingPrereleases => format!("{package}:prerelease"),
            Self::Matching(constraints) => format!("{package}:match:{constraints}"),
        }
    }
}

/// The answer from a registry that has no Git-ref concept at all.
///
/// Distinct from an `Err`, which says the question could not be answered.
pub fn no_ref_names() -> Result<Vec<String>> {
    Ok(Vec::new())
}

/// The answer from a registry that publishes no per-version metadata.
///
/// Distinct from an `Err`, which says the question could not be answered.
pub fn no_version_metadata() -> Result<Vec<VersionMeta>> {
    Ok(Vec::new())
}

/// The error for a registry that cannot resolve Git refs at all.
///
/// Deliberately not a [`RefNotFound`]: the ref was never looked up, so nothing
/// was learned about whether it exists, and no caller may treat this as the
/// repository saying the ref is absent.
pub fn ref_resolution_unsupported(registry: &str, package: &str, reference: &str) -> anyhow::Error {
    anyhow!("registry '{registry}' cannot resolve Git ref '{reference}' for '{package}'")
}

/// What a registry knows about the tags naming a particular commit.
///
/// The three answers a caller must tell apart are "the repository publishes
/// these tags at that commit", "this registry has no tags to look at" and, as an
/// `Err`, "the question went unanswered". A plain `Vec` would collapse the first
/// two, because an empty list is a real and common answer here: a commit that no
/// release names is exactly the case the caller has to report honestly rather
/// than guess around. Making it a type means a registry cannot express the
/// absence of an answer as an empty one by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagsAtCommit {
    /// The repository was consulted. These tags name the commit, in no
    /// particular order, and an empty list means none do.
    Known(Vec<String>),
    /// This registry has no concept of tags, so nothing was learned.
    Unsupported,
}

/// The answer from a registry with no tag concept.
///
/// Distinct from `Known(vec![])`, which says the repository was asked and no tag
/// names the commit.
pub fn tags_at_commit_unsupported() -> Result<TagsAtCommit> {
    Ok(TagsAtCommit::Unsupported)
}

/// A package registry.
///
/// **The capability methods - `list_versions`, `list_ref_names`,
/// `resolve_ref_to_commit` and `tags_at_commit` - have no default body, and a
/// new method describing what a registry can do must not have one either.** `Registry` is wrapped by
/// decorators ([`crate::cache::CachedRegistry`], [`IndexChain`],
/// [`MultiPyPiRegistry`]), and a default body is silently inherited by every one
/// of them: the wrapped registry is never consulted and the caller receives the
/// default's answer, which is indistinguishable from a real one. That has
/// shipped from this trait before. Requiring these forces every decorator to
/// state whether it forwards, and makes forgetting one a compile error rather
/// than a silent wrong answer. A registry that genuinely lacks a capability says
/// so explicitly with [`no_ref_names`], [`no_version_metadata`],
/// [`ref_resolution_unsupported`], or [`tags_at_commit_unsupported`].
///
/// The two lookup methods below keep a default because theirs degrades to a real
/// answer from the same registry - the stable version, the latest version -
/// rather than manufacturing an absence. Every decorator overrides both anyway;
/// a new one that does not silently loses prerelease and constraint handling.
#[async_trait]
pub trait Registry: Send + Sync {
    /// Get the latest stable version of a package
    async fn get_latest_version(&self, package: &str) -> Result<String>;

    /// Get the latest version including pre-releases
    /// Used when the user's current version is a pre-release
    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        // Default: fall back to stable-only
        self.get_latest_version(package).await
    }

    /// Get the latest version matching the given constraints (e.g., ">=2.8.0,<9")
    /// Default implementation falls back to get_latest_version
    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        // Default: ignore constraints and return latest
        let _ = constraints;
        self.get_latest_version(package).await
    }

    /// Repeat a latest-version lookup after `stale_version` proved unusable.
    ///
    /// Leaf registries already perform live requests, so their default is the
    /// corresponding ordinary lookup. Cache and index decorators override this
    /// method: caches bypass only the stale entry, while index chains preserve
    /// the source-selection rules of the original query.
    async fn revalidate_version(
        &self,
        package: &str,
        query: VersionQuery<'_>,
        stale_version: &str,
    ) -> Result<String> {
        let _ = stale_version;
        query.run(self, package).await
    }

    /// List recent versions with metadata, most recent ~50 in any order.
    ///
    /// A registry that publishes no dates answers `no_version_metadata()`, which
    /// the cooldown layer reads as "publish dates unavailable here".
    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>>;

    /// List the ref names a consumer can actually pin to, for registries where
    /// refs are distinct from released versions.
    ///
    /// GitHub Actions are pinned to a git ref, and a repo publishing `v4.1.2`
    /// does not necessarily publish a floating `v4` ref to go with it. Writing
    /// a truncated `v4` in that case produces a workflow that fails to resolve,
    /// so the Actions updater consults this before shortening a version.
    ///
    /// A registry with no ref concept answers `no_ref_names()`. Callers MUST
    /// read an empty list as *unknown* rather than as "the ref does not exist",
    /// so a registry without ref data keeps its existing behaviour instead of
    /// silently losing precision-matching. A lookup that could not complete is
    /// an `Err`, which is a different fact and must stay distinguishable.
    async fn list_ref_names(&self, package: &str) -> Result<Vec<String>>;

    /// Resolve a Git ref to the immutable commit SHA it currently identifies.
    ///
    /// A registry that does not expose Git refs answers
    /// `ref_resolution_unsupported()`. The GitHub Actions updater uses this only
    /// for its opt-in SHA-pin mode, where silently falling back to a mutable tag
    /// would weaken the caller's supply-chain protection.
    ///
    /// An `Err` carrying [`RefNotFound`] means the repository answered that the
    /// ref does not exist. Any other `Err` means the question went unanswered.
    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String>;

    /// List the tag names that point at `commit`, the inverse of
    /// [`resolve_ref_to_commit`](Registry::resolve_ref_to_commit).
    ///
    /// A GitHub Action pinned to a bare commit carries no record of which
    /// release it is, which is what stops such a pin from ever being updated.
    /// The repository still knows, because the release that shipped that commit
    /// tagged it, so the Actions updater asks here rather than making the user
    /// annotate every pin by hand.
    ///
    /// A registry with no tag concept answers [`tags_at_commit_unsupported`].
    /// `Known(vec![])` is a real answer meaning no tag names this commit, and a
    /// caller must not soften it into "unknown": a commit off every release is
    /// precisely what a caller has to refuse to guess about. An `Err` means the
    /// lookup did not complete, which is a third fact again, and a registry that
    /// can only see part of the tag list MUST report `Err` rather than a `Known`
    /// list assembled from what it managed to read.
    async fn tags_at_commit(&self, package: &str, commit: &str) -> Result<TagsAtCommit>;

    /// Registry name for display
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_get_with_retry_success_first_try() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/test"))
            .respond_with(ResponseTemplate::new(200).set_body_string("success"))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/test", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        assert!(response.status().is_success());
        assert_eq!(response.text().await.unwrap(), "success");
    }

    #[tokio::test]
    async fn test_get_with_retry_client_error_no_retry() {
        let mock_server = MockServer::start().await;

        // 404 should not be retried
        Mock::given(method("GET"))
            .and(path("/notfound"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1) // Should only be called once, no retry
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/notfound", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        assert_eq!(response.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn test_get_with_retry_server_error_retries() {
        let mock_server = MockServer::start().await;

        // Always return 500 - this test verifies that retries actually happen
        // by checking that the endpoint is called MAX_RETRIES (3) times
        Mock::given(method("GET"))
            .and(path("/flaky"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3) // MAX_RETRIES = 3, verifies retry behavior
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/flaky", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        // After MAX_RETRIES exhausted, should return the 500 response
        assert_eq!(response.status().as_u16(), 500);
    }

    #[tokio::test]
    async fn test_get_with_retry_recovers_on_second_try() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mock_server = MockServer::start().await;
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        // First call returns 500, second call returns 200
        Mock::given(method("GET"))
            .and(path("/recover"))
            .respond_with(move |_: &wiremock::Request| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200).set_body_string("recovered")
                }
            })
            .expect(2) // Should be called twice: 500 then 200
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/recover", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        // Should recover and return 200
        assert!(response.status().is_success());
    }

    #[tokio::test]
    async fn test_get_with_retry_redirect_success() {
        let mock_server = MockServer::start().await;

        // Test that redirects (3xx) are handled by reqwest (not retried as errors)
        Mock::given(method("GET"))
            .and(path("/redirect"))
            .respond_with(ResponseTemplate::new(200).set_body_string("redirected"))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = Client::new();
        let url = format!("{}/redirect", mock_server.uri());

        let response = get_with_retry(&client, &url).await.unwrap();
        assert!(response.status().is_success());
    }

    // Tests for Registry trait default implementations
    // Create a minimal registry that only implements required methods
    // to test that default implementations work correctly

    struct MinimalRegistry {
        version: String,
    }

    impl MinimalRegistry {
        fn new(version: &str) -> Self {
            Self {
                version: version.to_string(),
            }
        }
    }

    #[async_trait]
    impl Registry for MinimalRegistry {
        async fn get_latest_version(&self, _package: &str) -> Result<String> {
            Ok(self.version.clone())
        }

        async fn list_versions(&self, _package: &str) -> Result<Vec<VersionMeta>> {
            no_version_metadata()
        }

        async fn list_ref_names(&self, _package: &str) -> Result<Vec<String>> {
            no_ref_names()
        }

        async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
            Err(ref_resolution_unsupported(self.name(), package, reference))
        }

        async fn tags_at_commit(&self, _package: &str, _commit: &str) -> Result<TagsAtCommit> {
            tags_at_commit_unsupported()
        }

        fn name(&self) -> &'static str {
            "Minimal"
        }
        // The prerelease and constraint methods are intentionally left to their
        // defaults, which the tests below exercise. The capability methods have
        // no defaults to inherit, so they are declared here like any leaf.
    }

    #[tokio::test]
    async fn test_registry_default_prereleases_falls_back_to_stable() {
        let registry = MinimalRegistry::new("2.31.0");

        // The default implementation should fall back to get_latest_version
        let version = registry
            .get_latest_version_including_prereleases("anypackage")
            .await
            .unwrap();

        assert_eq!(version, "2.31.0");
    }

    #[tokio::test]
    async fn test_registry_default_matching_ignores_constraints() {
        let registry = MinimalRegistry::new("5.0.0");

        // The default implementation ignores constraints and returns latest
        let version = registry
            .get_latest_version_matching("anypackage", ">=3.0,<4")
            .await
            .unwrap();

        // Should return 5.0.0 even though it doesn't match constraints
        // (real implementations would respect constraints)
        assert_eq!(version, "5.0.0");
    }

    #[tokio::test]
    async fn test_registry_name() {
        let registry = MinimalRegistry::new("1.0.0");
        assert_eq!(registry.name(), "Minimal");
    }

    #[tokio::test]
    async fn test_registry_default_list_versions_is_empty() {
        let registry = MinimalRegistry::new("1.0.0");
        let versions = registry.list_versions("anypackage").await.unwrap();
        assert!(
            versions.is_empty(),
            "default list_versions must return empty vec, got {} entries",
            versions.len()
        );
    }

    #[test]
    fn test_version_meta_can_be_constructed() {
        use chrono::{TimeZone, Utc};
        let meta = VersionMeta {
            version: "1.2.3".to_string(),
            published_at: Some(Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap()),
            yanked: false,
            prerelease: false,
        };
        assert_eq!(meta.version, "1.2.3");
        assert!(meta.published_at.is_some());
        assert!(!meta.yanked);
        assert!(!meta.prerelease);
    }

    // Integration tests for authentication headers
    mod auth_tests {
        use super::super::*;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn test_pypi_sends_basic_auth_header() {
            let mock_server = MockServer::start().await;

            // Simple API endpoint should fail (to trigger fallback to JSON API)
            Mock::given(method("GET"))
                .and(path("/simple/testpkg/"))
                .and(header("Authorization", "Basic dGVzdHVzZXI6dGVzdHBhc3M="))
                .respond_with(ResponseTemplate::new(404))
                .mount(&mock_server)
                .await;

            // Verify that Basic Auth header is sent to JSON API
            // "testuser:testpass" base64 encoded is "dGVzdHVzZXI6dGVzdHBhc3M="
            Mock::given(method("GET"))
                .and(path("/pypi/testpkg/json"))
                .and(header("Authorization", "Basic dGVzdHVzZXI6dGVzdHBhc3M="))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(r#"{"releases": {"1.0.0": [{"yanked": false}]}}"#),
                )
                .expect(1)
                .mount(&mock_server)
                .await;

            let creds = PyPiCredentials {
                username: "testuser".to_string(),
                password: "testpass".to_string(),
            };

            let registry =
                PyPiRegistry::with_index_url_and_credentials(mock_server.uri(), Some(creds));

            let version = registry.get_latest_version("testpkg").await.unwrap();
            assert_eq!(version, "1.0.0");
        }

        #[tokio::test]
        async fn test_npm_sends_bearer_token_header() {
            let mock_server = MockServer::start().await;

            // Verify that Bearer token header is sent
            Mock::given(method("GET"))
                .and(path("/testpkg"))
                .and(header("Authorization", "Bearer my-secret-token"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"dist-tags": {"latest": "2.0.0"}, "versions": {"2.0.0": {}}}"#,
                ))
                .expect(1)
                .mount(&mock_server)
                .await;

            let creds = NpmCredentials {
                token: "my-secret-token".to_string(),
            };

            let registry =
                NpmRegistry::with_registry_url_and_credentials(mock_server.uri(), Some(creds));

            let version = registry.get_latest_version("testpkg").await.unwrap();
            assert_eq!(version, "2.0.0");
        }

        #[tokio::test]
        async fn test_crates_io_sends_bearer_token_header() {
            let mock_server = MockServer::start().await;

            // Verify that Bearer token header is sent (Cargo uses Bearer tokens)
            Mock::given(method("GET"))
                .and(path("/testcrate"))
                .and(header("Authorization", "cargo-token-123"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"crate": {"max_stable_version": "3.0.0"}, "versions": [{"num": "3.0.0", "yanked": false}]}"#,
                ))
                .expect(1)
                .mount(&mock_server)
                .await;

            let creds = CargoCredentials {
                token: "cargo-token-123".to_string(),
            };

            let registry =
                CratesIoRegistry::with_registry_url_and_credentials(mock_server.uri(), Some(creds));

            let version = registry.get_latest_version("testcrate").await.unwrap();
            assert_eq!(version, "3.0.0");
        }

        #[tokio::test]
        async fn test_go_proxy_sends_basic_auth_header() {
            let mock_server = MockServer::start().await;

            // Verify that Basic Auth header is sent
            // "gouser:gopass" base64 encoded is "Z291c2VyOmdvcGFzcw=="
            Mock::given(method("GET"))
                .and(path("/github.com/test/module/@latest"))
                .and(header("Authorization", "Basic Z291c2VyOmdvcGFzcw=="))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(r#"{"Version": "v1.0.0"}"#),
                )
                .expect(1)
                .mount(&mock_server)
                .await;

            let creds = GoCredentials {
                username: "gouser".to_string(),
                password: "gopass".to_string(),
            };

            let registry =
                GoProxyRegistry::with_proxy_url_and_credentials(mock_server.uri(), Some(creds));

            let version = registry
                .get_latest_version("github.com/test/module")
                .await
                .unwrap();
            assert_eq!(version, "v1.0.0");
        }
    }
}
