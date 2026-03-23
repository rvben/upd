use super::{Registry, get_with_retry, http_error_message};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

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
        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
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

    fn name(&self) -> &'static str {
        "nuget"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
}
