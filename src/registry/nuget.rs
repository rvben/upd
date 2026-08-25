use super::{Registry, VersionMeta, get_with_retry, http_error_message};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

// NuGet declares no publish dates: the v3-flatcontainer endpoint we query here
// returns only version strings, so cooldown reports NuGet as unsupported.
// Resolving `RegistrationsBaseUrl` from the service index would let us fetch
// catalog entries with publish dates.
pub struct NuGetRegistry {
    client: Client,
    api_url: String,
}

#[derive(Debug, Deserialize)]
struct FlatContainerIndex {
    versions: Vec<String>,
}

impl NuGetRegistry {
    pub fn new() -> Self {
        Self::with_api_url("https://api.nuget.org/v3-flatcontainer".to_string())
    }

    pub fn with_api_url(api_url: String) -> Self {
        let client = crate::http::apply(
            Client::builder()
                .gzip(true)
                .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10)),
        )
        .build()
        .expect("Failed to create HTTP client. This usually indicates a TLS/SSL configuration issue on your system.");

        Self { client, api_url }
    }

    /// Check if a version string represents a pre-release (contains `-`)
    fn is_prerelease(version: &str) -> bool {
        version.contains('-')
    }
}

impl Default for NuGetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for NuGetRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let lower = package.to_lowercase();
        let url = format!("{}/{}/index.json", self.api_url, lower);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "NuGet package",
                package,
                None
            )));
        }

        let index: FlatContainerIndex = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse NuGet response for '{}': {}", package, e))?;

        // Filter out pre-releases, find latest by semver
        let latest = index
            .versions
            .iter()
            .filter(|v| !Self::is_prerelease(v))
            .filter_map(|v| semver::Version::parse(v).ok().map(|sv| (v, sv)))
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(v, _)| v.clone());

        latest.ok_or_else(|| anyhow!("NuGet package '{}' has no stable versions", package))
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let lower = package.to_lowercase();
        let url = format!("{}/{}/index.json", self.api_url, lower);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(http_error_message(
                response.status(),
                "NuGet package",
                package,
                None
            )));
        }

        let index: FlatContainerIndex = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse NuGet response for '{}': {}", package, e))?;

        // Include pre-releases, find latest by semver
        let latest = index
            .versions
            .iter()
            .filter_map(|v| semver::Version::parse(v).ok().map(|sv| (v, sv)))
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(v, _)| v.clone());

        latest.ok_or_else(|| anyhow!("NuGet package '{}' has no versions", package))
    }

    /// The flat container index carries no publish dates, so cooldown cannot
    /// apply here.
    async fn list_versions(&self, _package: &str) -> Result<Vec<VersionMeta>> {
        super::no_version_metadata()
    }

    /// NuGet packages are not pinned to Git refs.
    async fn list_ref_names(&self, _package: &str) -> Result<Vec<String>> {
        super::no_ref_names()
    }

    /// NuGet packages are not pinned to Git refs.
    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        Err(super::ref_resolution_unsupported(
            self.name(),
            package,
            reference,
        ))
    }

    async fn tags_at_commit(&self, _package: &str, _commit: &str) -> Result<super::TagsAtCommit> {
        super::tags_at_commit_unsupported()
    }

    fn name(&self) -> &'static str {
        "nuget"
    }
}

/// Whether `version` falls inside a NuGet version range.
///
/// `Some(true)` when the range admits it, `Some(false)` when it excludes it, and
/// `None` when the range is not NuGet interval notation at all. The third answer
/// exists because a range upd cannot read is a different fact from one that
/// excludes the release, and reporting the first as the second would tell a user
/// their dependency is behind when nothing has actually looked at it.
///
/// Interval notation, from the NuGet docs: `1.0` is a minimum (inclusive),
/// `[1.0]` is exact, a square bracket includes its bound and a parenthesis
/// excludes it, and an omitted bound is unbounded on that side.
pub(crate) fn matches_nuget_range(version: &str, range: &str) -> Option<bool> {
    let range = range.trim();
    let open = range.chars().next()?;

    if open != '[' && open != '(' {
        // A bare version is a minimum, not an exact match. A comma outside
        // brackets is not notation NuGet defines.
        if range.contains(',') {
            return None;
        }
        return Some(nuget_cmp(version, range)? != std::cmp::Ordering::Less);
    }

    let close = range.chars().last()?;
    if close != ']' && close != ')' {
        return None;
    }
    let lower_inclusive = open == '[';
    let upper_inclusive = close == ']';
    let inner = &range[open.len_utf8()..range.len() - close.len_utf8()];
    if inner.matches(',').count() > 1 {
        return None;
    }

    let Some((lower, upper)) = inner.split_once(',') else {
        // No comma: an exact version, which only square brackets express.
        if !(lower_inclusive && upper_inclusive) {
            return None;
        }
        let exact = inner.trim();
        if exact.is_empty() {
            return None;
        }
        return Some(nuget_cmp(version, exact)? == std::cmp::Ordering::Equal);
    };

    let (lower, upper) = (lower.trim(), upper.trim());
    if lower.is_empty() && upper.is_empty() {
        return None;
    }

    if lower.is_empty() {
        // An inclusive bracket over an omitted bound bounds nothing.
        if lower_inclusive {
            return None;
        }
    } else {
        match nuget_cmp(version, lower)? {
            std::cmp::Ordering::Less => return Some(false),
            std::cmp::Ordering::Equal if !lower_inclusive => return Some(false),
            _ => {}
        }
    }

    if upper.is_empty() {
        if upper_inclusive {
            return None;
        }
    } else {
        match nuget_cmp(version, upper)? {
            std::cmp::Ordering::Greater => return Some(false),
            std::cmp::Ordering::Equal if !upper_inclusive => return Some(false),
            _ => {}
        }
    }

    Some(true)
}

/// Order two NuGet versions, or `None` if either is not a version.
///
/// NuGet pads a missing component with zero, so `1.0` and `1.0.0` are the same
/// version, and a pre-release suffix orders below the release it qualifies.
fn nuget_cmp(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let (a_release, a_pre) = split_prerelease(a);
    let (b_release, b_pre) = split_prerelease(b);
    let (mut a_parts, mut b_parts) = (release_parts(a_release)?, release_parts(b_release)?);
    let width = a_parts.len().max(b_parts.len());
    a_parts.resize(width, 0);
    b_parts.resize(width, 0);

    Some(a_parts.cmp(&b_parts).then_with(|| match (a_pre, b_pre) {
        (None, None) => std::cmp::Ordering::Equal,
        // A release outranks any pre-release of the same numbers.
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
    }))
}

/// Split a version into its numeric release and its pre-release suffix.
fn split_prerelease(version: &str) -> (&str, Option<&str>) {
    match version.split_once('-') {
        Some((release, pre)) => (release, Some(pre)),
        None => (version, None),
    }
}

/// The numeric components of a release, or `None` if any component is not a number.
fn release_parts(release: &str) -> Option<Vec<u64>> {
    // Build metadata orders nothing, so it is dropped rather than rejected.
    let release = release.split_once('+').map_or(release, |(v, _)| v);
    if release.is_empty() {
        return None;
    }
    release.split('.').map(|part| part.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn a_bracket_range_admits_only_what_it_bounds() {
        // The bound that closes with a parenthesis is the one release excluded.
        assert_eq!(matches_nuget_range("13.0.3", "[12.0.0,14.0.0)"), Some(true));
        assert_eq!(
            matches_nuget_range("14.0.0", "[12.0.0,14.0.0)"),
            Some(false)
        );
        assert_eq!(
            matches_nuget_range("11.9.9", "[12.0.0,14.0.0)"),
            Some(false)
        );
        assert_eq!(matches_nuget_range("12.0.0", "[12.0.0,14.0.0)"), Some(true));
        assert_eq!(
            matches_nuget_range("12.0.0", "(12.0.0,14.0.0]"),
            Some(false)
        );
        assert_eq!(matches_nuget_range("14.0.0", "(12.0.0,14.0.0]"), Some(true));
    }

    #[test]
    fn an_omitted_bound_is_unbounded_on_that_side() {
        assert_eq!(matches_nuget_range("99.0.0", "[12.0.0,)"), Some(true));
        assert_eq!(matches_nuget_range("11.0.0", "[12.0.0,)"), Some(false));
        assert_eq!(matches_nuget_range("1.0.0", "(,14.0.0)"), Some(true));
        assert_eq!(matches_nuget_range("14.0.0", "(,14.0.0)"), Some(false));
    }

    #[test]
    fn a_bare_version_is_a_minimum_not_an_exact_match() {
        assert_eq!(matches_nuget_range("13.0.3", "12.0.0"), Some(true));
        assert_eq!(matches_nuget_range("11.0.0", "12.0.0"), Some(false));
        assert_eq!(matches_nuget_range("12.0.0", "[12.0.0]"), Some(true));
        assert_eq!(matches_nuget_range("13.0.3", "[12.0.0]"), Some(false));
    }

    #[test]
    fn a_missing_component_is_zero_and_a_prerelease_orders_below_its_release() {
        assert_eq!(matches_nuget_range("1.0", "[1.0.0]"), Some(true));
        assert_eq!(matches_nuget_range("1.0.0-beta", "[1.0.0,)"), Some(false));
        assert_eq!(
            matches_nuget_range("1.0.0-beta", "(1.0.0-alpha,)"),
            Some(true)
        );
        assert_eq!(matches_nuget_range("1.0.0+build.7", "[1.0.0]"), Some(true));
    }

    #[test]
    fn a_range_that_is_not_notation_reads_as_unreadable_not_as_excluded() {
        // Distinguishable from Some(false): nothing has looked at the dependency.
        assert_eq!(matches_nuget_range("1.0.0", "[1.0.0"), None);
        assert_eq!(matches_nuget_range("1.0.0", "(1.0.0)"), None);
        assert_eq!(matches_nuget_range("1.0.0", "[,2.0.0)"), None);
        assert_eq!(matches_nuget_range("1.0.0", "[1.0.0,]"), None);
        assert_eq!(matches_nuget_range("1.0.0", "(,)"), None);
        assert_eq!(matches_nuget_range("1.0.0", "[1.0,2.0,3.0]"), None);
        assert_eq!(matches_nuget_range("1.0.0", "1.0,2.0"), None);
        assert_eq!(matches_nuget_range("1.0.0", "[latest,)"), None);
        assert_eq!(matches_nuget_range("not-a-version", "[1.0.0,)"), None);
        assert_eq!(matches_nuget_range("1.0.0", ""), None);
    }

    #[tokio::test]
    async fn test_get_latest_version() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/newtonsoft.json/index.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"versions": ["12.0.3", "13.0.1", "13.0.2", "13.0.3"]}"#),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = NuGetRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version("Newtonsoft.Json")
            .await
            .unwrap();
        assert_eq!(version, "13.0.3");
    }

    #[tokio::test]
    async fn test_package_not_found() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/nonexistent-pkg-xyz/index.json"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = NuGetRegistry::with_api_url(mock_server.uri());
        let result = registry.get_latest_version("nonexistent-pkg-xyz").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_registry_name() {
        let registry = NuGetRegistry::new();
        assert_eq!(registry.name(), "nuget");
    }

    #[tokio::test]
    async fn test_skips_prereleases() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/xunit/index.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"versions": ["2.6.1", "2.6.2", "2.7.0-pre.1", "2.7.0-beta.2", "2.7.0-rc.1"]}"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = NuGetRegistry::with_api_url(mock_server.uri());
        let version = registry.get_latest_version("xunit").await.unwrap();
        assert_eq!(version, "2.6.2");
    }

    #[tokio::test]
    async fn test_lowercases_package_name() {
        let mock_server = MockServer::start().await;

        // The mock expects the lowercased path
        Mock::given(method("GET"))
            .and(path("/newtonsoft.json/index.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"versions": ["13.0.3"]}"#))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = NuGetRegistry::with_api_url(mock_server.uri());
        // Pass mixed-case name; should still work
        let version = registry
            .get_latest_version("Newtonsoft.Json")
            .await
            .unwrap();
        assert_eq!(version, "13.0.3");
    }

    #[tokio::test]
    async fn test_get_latest_including_prereleases() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/xunit/index.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"versions": ["2.6.1", "2.6.2", "2.7.0-pre.1", "2.7.0-rc.1"]}"#,
            ))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = NuGetRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version_including_prereleases("xunit")
            .await
            .unwrap();
        assert_eq!(version, "2.7.0-rc.1");
    }

    #[tokio::test]
    async fn test_picks_highest_version_even_when_list_is_unsorted() {
        // The NuGet flat-container spec says versions are sorted, but we do
        // not rely on that: max_by(semver) must pick the highest regardless
        // of array order. This guards against broken mirrors.
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/serilog/index.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"versions": ["3.0.1", "2.12.0", "4.0.0", "3.1.1"]}"#),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = NuGetRegistry::with_api_url(mock_server.uri());
        let version = registry.get_latest_version("Serilog").await.unwrap();
        assert_eq!(version, "4.0.0");
    }

    #[tokio::test]
    async fn test_skips_non_semver_version_strings() {
        // Legacy NuGet packages sometimes use 4-segment versions (e.g.
        // `6.0.0.0`) which are not valid SemVer. These should be ignored
        // in favour of SemVer-parseable entries rather than crashing.
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/entityframework/index.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"versions": ["6.0.0.0", "6.4.4", "6.5.0.0"]}"#),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = NuGetRegistry::with_api_url(mock_server.uri());
        let version = registry
            .get_latest_version("EntityFramework")
            .await
            .unwrap();
        assert_eq!(version, "6.4.4");
    }

    #[tokio::test]
    async fn test_empty_versions_list_returns_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/emptypkg/index.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"versions": []}"#))
            .expect(1)
            .mount(&mock_server)
            .await;

        let registry = NuGetRegistry::with_api_url(mock_server.uri());
        let result = registry.get_latest_version("emptypkg").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no stable versions"),
            "error message must mention missing versions, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_nuget_list_versions_is_unsupported_for_now() {
        let registry = NuGetRegistry::with_api_url("http://localhost:0".to_string());
        let versions = registry.list_versions("anything").await.unwrap();
        assert!(
            versions.is_empty(),
            "NuGet list_versions returns empty — flatcontainer exposes no publish dates; resolving RegistrationsBaseUrl would enable catalog-based publish dates"
        );
    }
}
