//! The JSR registry: version metadata for `jsr:` specifiers in `deno.json`.
//!
//! JSR serves package metadata as JSON at
//! `https://jsr.io/@<scope>/<package>/meta.json`. The document lists every
//! published version with its yanked status and publish time, which is
//! everything version resolution needs: a specifier's range is an npm-style
//! range, so the grammar in [`crate::npm_range`] picks the release and this
//! module only answers *which releases exist*.
//!
//! Requests carry an `Accept` header that does not include `text/html`; the
//! registry returns an HTML rendering of the metadata when it does, and a
//! tooling client must never be handed that page. No authentication is
//! required for public packages, and responses are served with
//! `cache-control: public` plus an `etag`, so the caching layer in front of
//! every registry is the only repetition guard this module needs.

use super::{Registry, VersionMeta, get_with_retry, http_error_message};
use crate::npm_range;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

pub struct JsrRegistry {
    client: Client,
    registry_url: String,
}

/// The `meta.json` document for one JSR package.
#[derive(Debug, Deserialize)]
struct JsrPackageMeta {
    #[serde(default)]
    versions: HashMap<String, JsrVersionMeta>,
}

/// One entry of the `versions` map. The registry documents `yanked`; the
/// publish timestamp is optional and absent on entries published before JSR
/// started recording it.
#[derive(Debug, Deserialize, Default)]
struct JsrVersionMeta {
    #[serde(default)]
    yanked: bool,
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
}

impl Default for JsrRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The `@scope/name` a `jsr:` specifier names, split for the meta.json URL.
///
/// JSR package names are exactly a scope, an slash and a name, and every
/// character of both segments comes from a small alphabet. Validating here
/// keeps a specifier from writing `../` or a query string into the URL the
/// registry is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JsrPackageName {
    pub scope: String,
    pub name: String,
}

impl JsrPackageName {
    pub fn parse(package: &str) -> Result<Self> {
        let rest = package
            .strip_prefix('@')
            .ok_or_else(|| anyhow!("not a JSR package name (expected @scope/name): {package}"))?;
        let (scope, name) = rest
            .split_once('/')
            .ok_or_else(|| anyhow!("not a JSR package name (expected @scope/name): {package}"))?;
        if scope.is_empty() || name.is_empty() {
            bail!("not a JSR package name (expected @scope/name): {package}");
        }
        let valid = |s: &str| {
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        if !valid(scope) || !valid(name) {
            bail!("invalid JSR package name: {package}");
        }
        Ok(Self {
            scope: scope.to_string(),
            name: name.to_string(),
        })
    }

    /// The bare `@scope/name` spelling a specifier and an update report use.
    #[cfg(test)]
    pub fn full(&self) -> String {
        format!("@{}/{}", self.scope, self.name)
    }
}

impl JsrRegistry {
    pub fn new() -> Self {
        Self::with_registry_url(
            Self::detect_registry_url().unwrap_or_else(|| "https://jsr.io".to_string()),
        )
    }

    /// The registry base URL the environment names, if any.
    ///
    /// `JSR_REGISTRY` mirrors `NPM_REGISTRY`: the override integration tests
    /// point at a local server, and self-hosted JSR-compatible registries use
    /// it the same way.
    pub fn detect_registry_url() -> Option<String> {
        match std::env::var("JSR_REGISTRY") {
            Ok(url) if !url.is_empty() => Some(url),
            _ => None,
        }
    }

    pub fn with_registry_url(registry_url: String) -> Self {
        // The registry serves an HTML rendering of the metadata to requests
        // whose `Accept` names `text/html`, so the client defaults to a
        // JSON-only `Accept` and every lookup inherits it.
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let client = crate::http::apply(
            Client::builder()
                .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
                .default_headers(headers)
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10)),
        )
        .build()
        .expect("Failed to create HTTP client");
        Self {
            client,
            registry_url,
        }
    }

    fn meta_url(&self, package: &str) -> Result<String> {
        let parsed = JsrPackageName::parse(package)?;
        Ok(format!(
            "{}/@{}/{}/meta.json",
            self.registry_url.trim_end_matches('/'),
            parsed.scope,
            parsed.name
        ))
    }

    async fn fetch_meta(&self, package: &str) -> Result<JsrPackageMeta> {
        let url = self.meta_url(package)?;
        let response = get_with_retry(&self.client, &url).await?;
        if !response.status().is_success() {
            bail!(
                "{}",
                http_error_message(response.status(), "JSR package", package, None)
            );
        }
        let meta: JsrPackageMeta = response
            .json()
            .await
            .map_err(|e| anyhow!("invalid JSR metadata for {package}: {e}"))?;
        Ok(meta)
    }

    /// Every published version as [`VersionMeta`], ordered by semver so the
    /// "latest" answer is the last survivor of any filter.
    fn metas(meta: JsrPackageMeta) -> Vec<VersionMeta> {
        let mut versions: Vec<VersionMeta> = meta
            .versions
            .into_iter()
            .map(|(version, entry)| {
                let parsed = semver::Version::parse(&version).ok();
                VersionMeta {
                    prerelease: parsed.as_ref().is_some_and(|v| !v.pre.is_empty()),
                    published_at: entry.created_at.as_deref().and_then(parse_timestamp),
                    yanked: entry.yanked,
                    version,
                }
            })
            .collect();
        versions.sort_by(|a, b| {
            match (
                semver::Version::parse(&a.version),
                semver::Version::parse(&b.version),
            ) {
                (Ok(a), Ok(b)) => a.cmp(&b),
                // An unparseable version cannot be ordered; keep it at the end
                // so it is never the answer to a "latest" question.
                (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                (Err(_), Err(_)) => std::cmp::Ordering::Equal,
            }
        });
        versions
    }

    async fn latest(&self, package: &str, prereleases: bool) -> Result<String> {
        let versions = self.list_versions(package).await?;
        versions
            .into_iter()
            .filter(|meta| !meta.yanked)
            .filter(|meta| prereleases || !meta.prerelease)
            .map(|meta| meta.version)
            .next_back()
            .ok_or_else(|| {
                anyhow!(
                    "{package} has no supported {}versions",
                    if prereleases { "" } else { "stable " }
                )
            })
    }
}

/// An RFC 3339 timestamp from the registry's `createdAt` field, parsed with
/// the same tolerant spelling the cooldown reports use.
fn parse_timestamp(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|ts| ts.with_timezone(&chrono::Utc))
}

#[async_trait]
impl Registry for JsrRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        self.latest(package, false).await
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        self.latest(package, true).await
    }

    /// The newest release the npm-style range admits, yanked versions aside.
    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let reqs = npm_range::parse_npm_range(constraints)
            .ok_or_else(|| anyhow!("cannot evaluate JSR range '{constraints}' for {package}"))?;
        let versions = self.list_versions(package).await?;
        versions
            .into_iter()
            .filter(|meta| !meta.yanked)
            .filter(|meta| {
                let parsed = semver::Version::parse(&meta.version);
                match parsed {
                    // A prerelease only satisfies a range when the range names
                    // a prerelease of the same release channel, which is what
                    // `VersionReq::matches` already decides.
                    Ok(parsed) => reqs.iter().any(|req| req.matches(&parsed)),
                    Err(_) => false,
                }
            })
            .map(|meta| meta.version)
            .next_back()
            .ok_or_else(|| anyhow!("no version of {package} satisfies '{constraints}'"))
    }

    async fn python_releases(&self, _: &str) -> Result<Vec<super::PythonRelease>> {
        bail!("registry does not expose Python metadata")
    }

    /// Publish times come from the registry's `createdAt` per version, which
    /// is what cooldown windows need.
    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        let meta = self.fetch_meta(package).await?;
        Ok(Self::metas(meta))
    }

    async fn list_ref_names(&self, _: &str) -> Result<Vec<String>> {
        super::no_ref_names()
    }

    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        Err(super::ref_resolution_unsupported(
            self.name(),
            package,
            reference,
        ))
    }

    async fn tags_at_commit(&self, _: &str, _: &str) -> Result<super::TagsAtCommit> {
        super::tags_at_commit_unsupported()
    }

    fn name(&self) -> &'static str {
        "jsr"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn package_names_split_and_validate() {
        let parsed = JsrPackageName::parse("@std/path").unwrap();
        assert_eq!(parsed.scope, "std");
        assert_eq!(parsed.name, "path");
        assert_eq!(parsed.full(), "@std/path");

        assert!(JsrPackageName::parse("std/path").is_err());
        assert!(JsrPackageName::parse("@std").is_err());
        assert!(JsrPackageName::parse("@std/").is_err());
        assert!(JsrPackageName::parse("@std/../secret").is_err());
        assert!(JsrPackageName::parse("@std/na me").is_err());
        assert!(JsrPackageName::parse("@std/path?query").is_err());
    }

    #[test]
    fn meta_url_has_no_traversal_room() {
        let registry = JsrRegistry::new();
        assert_eq!(
            registry.meta_url("@std/path").unwrap(),
            "https://jsr.io/@std/path/meta.json"
        );
        assert!(registry.meta_url("std/path").is_err());
        assert!(registry.meta_url("@std/../etc").is_err());
    }

    #[tokio::test]
    async fn latest_skips_yanked_and_prereleases() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/@std/path/meta.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "scope": "std",
                "name": "path",
                "versions": {
                    "1.0.0": { "createdAt": "2024-05-01T00:00:00.000000Z" },
                    "1.1.0": { "yanked": true },
                    "1.2.0": { "createdAt": "2025-01-15T12:00:00.000000Z" },
                    "2.0.0-rc.1": { "createdAt": "2025-02-01T00:00:00.000000Z" }
                }
            })))
            .mount(&server)
            .await;
        let registry = JsrRegistry::with_registry_url(server.uri());

        assert_eq!(
            registry.get_latest_version("@std/path").await.unwrap(),
            "1.2.0"
        );
        assert_eq!(
            registry
                .get_latest_version_including_prereleases("@std/path")
                .await
                .unwrap(),
            "2.0.0-rc.1"
        );
    }

    #[tokio::test]
    async fn matching_respects_the_range_and_ignores_yanked() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/@std/path/meta.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": {
                    "1.0.0": {},
                    "1.1.0": { "yanked": true },
                    "1.2.0": {},
                    "2.0.0": { "createdAt": "2025-03-01T00:00:00.000000Z" }
                }
            })))
            .mount(&server)
            .await;
        let registry = JsrRegistry::with_registry_url(server.uri());

        assert_eq!(
            registry
                .get_latest_version_matching("@std/path", "^1.0.0")
                .await
                .unwrap(),
            "1.2.0"
        );
        // A major-only range is npm grammar: `1` admits every 1.x.
        assert_eq!(
            registry
                .get_latest_version_matching("@std/path", "1")
                .await
                .unwrap(),
            "1.2.0"
        );
        assert!(
            registry
                .get_latest_version_matching("@std/path", "^9.0.0")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn list_versions_carries_publish_times_and_yanked_flags() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/@std/path/meta.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": {
                    "1.0.0": { "createdAt": "2024-05-01T00:00:00.000000Z" },
                    "1.1.0": { "yanked": true }
                }
            })))
            .mount(&server)
            .await;
        let registry = JsrRegistry::with_registry_url(server.uri());

        let versions = registry.list_versions("@std/path").await.unwrap();
        assert_eq!(versions.len(), 2);
        let first = versions.iter().find(|v| v.version == "1.0.0").unwrap();
        assert!(!first.yanked);
        assert_eq!(
            first.published_at.map(|t| t.to_rfc3339()),
            Some("2024-05-01T00:00:00+00:00".to_string())
        );
        let yanked = versions.iter().find(|v| v.version == "1.1.0").unwrap();
        assert!(yanked.yanked);
        assert!(yanked.published_at.is_none());
    }

    #[tokio::test]
    async fn http_errors_and_bad_json_are_errors() {
        let server = MockServer::start().await;
        let registry = JsrRegistry::with_registry_url(server.uri());
        assert!(registry.get_latest_version("@std/missing").await.is_err());

        Mock::given(method("GET"))
            .and(path("/@std/broken/meta.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>not json</html>"))
            .mount(&server)
            .await;
        assert!(registry.get_latest_version("@std/broken").await.is_err());
    }

    #[tokio::test]
    async fn requests_accept_json_not_html() {
        // The registry returns an HTML rendering when a request's Accept names
        // text/html. This pins the client to a JSON-only Accept.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/@std/path/meta.json"))
            .and(wiremock::matchers::header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "versions": { "1.0.0": {} }
            })))
            .mount(&server)
            .await;
        let registry = JsrRegistry::with_registry_url(server.uri());
        assert_eq!(
            registry.get_latest_version("@std/path").await.unwrap(),
            "1.0.0"
        );
    }
}
