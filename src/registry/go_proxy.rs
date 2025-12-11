use super::{Registry, get_with_retry};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct GoProxyRegistry {
    client: Client,
    proxy_url: String,
}

#[derive(Debug, Deserialize)]
struct LatestResponse {
    #[serde(rename = "Version")]
    version: String,
}

impl GoProxyRegistry {
    pub fn new() -> Self {
        Self::with_proxy_url("https://proxy.golang.org".to_string())
    }

    pub fn with_proxy_url(proxy_url: String) -> Self {
        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, proxy_url }
    }

    /// Detect custom proxy URL from environment (GOPROXY)
    pub fn detect_proxy_url() -> Option<String> {
        std::env::var("GOPROXY")
            .ok()
            .and_then(|s| {
                // GOPROXY can be comma-separated, take the first valid one
                s.split(',')
                    .map(|p| p.trim())
                    .find(|p| !p.is_empty() && *p != "direct" && *p != "off")
                    .map(|p| p.to_string())
            })
            .filter(|s| !s.is_empty())
    }

    /// Fetch list of all versions for a module
    async fn fetch_versions(&self, module: &str) -> Result<Vec<String>> {
        let escaped = Self::escape_module_path(module);
        let url = format!("{}/{}/@v/list", self.proxy_url, escaped);

        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch module '{}': HTTP {}",
                module,
                response.status()
            ));
        }

        let text = response.text().await?;
        Ok(text
            .lines()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect())
    }

    /// Escape module path for URL (Go proxy convention)
    /// Uppercase letters become !lowercase (e.g., GitHub -> !git!hub)
    fn escape_module_path(module: &str) -> String {
        let mut result = String::with_capacity(module.len() * 2);
        for c in module.chars() {
            if c.is_ascii_uppercase() {
                result.push('!');
                result.push(c.to_ascii_lowercase());
            } else {
                result.push(c);
            }
        }
        result
    }

    /// Parse Go version string (removes 'v' prefix for semver parsing)
    fn parse_version(version: &str) -> Option<semver::Version> {
        let stripped = version.strip_prefix('v').unwrap_or(version);
        // Handle +incompatible suffix
        let without_incompatible = stripped.split('+').next().unwrap_or(stripped);
        semver::Version::parse(without_incompatible).ok()
    }

    fn is_prerelease(version: &str) -> bool {
        Self::parse_version(version)
            .map(|v| !v.pre.is_empty())
            .unwrap_or(false)
    }
}

impl Default for GoProxyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for GoProxyRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        // Try @latest endpoint first (returns latest stable)
        let escaped = Self::escape_module_path(package);
        let url = format!("{}/{}/@latest", self.proxy_url, escaped);

        if let Ok(response) = get_with_retry(&self.client, &url).await
            && response.status().is_success()
            && let Ok(data) = response.json::<LatestResponse>().await
        {
            return Ok(data.version);
        }

        // Fallback: get version list and find latest stable
        let versions = self.fetch_versions(package).await?;

        let mut stable: Vec<_> = versions
            .iter()
            .filter(|v| !Self::is_prerelease(v))
            .filter_map(|v| Self::parse_version(v).map(|parsed| (parsed, v.clone())))
            .collect();

        stable.sort_by(|a, b| b.0.cmp(&a.0));

        stable
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No stable versions found for module '{}'", package))
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let versions = self.fetch_versions(package).await?;

        let mut all: Vec<_> = versions
            .iter()
            .filter_map(|v| Self::parse_version(v).map(|parsed| (parsed, v.clone())))
            .collect();

        all.sort_by(|a, b| b.0.cmp(&a.0));

        all.first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No versions found for module '{}'", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let versions = self.fetch_versions(package).await?;

        let req = semver::VersionReq::parse(constraints).map_err(|e| {
            anyhow!(
                "Failed to parse version constraints '{}': {}",
                constraints,
                e
            )
        })?;

        let mut matching: Vec<_> = versions
            .iter()
            .filter(|v| !Self::is_prerelease(v))
            .filter_map(|v| Self::parse_version(v).map(|parsed| (parsed, v.clone())))
            .filter(|(parsed, _)| req.matches(parsed))
            .collect();

        matching.sort_by(|a, b| b.0.cmp(&a.0));

        matching.first().map(|(_, s)| s.clone()).ok_or_else(|| {
            anyhow!(
                "No version of '{}' matches constraints '{}'",
                package,
                constraints
            )
        })
    }

    fn name(&self) -> &'static str {
        "go-proxy"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_module_path() {
        assert_eq!(
            GoProxyRegistry::escape_module_path("github.com/foo/bar"),
            "github.com/foo/bar"
        );
        assert_eq!(
            GoProxyRegistry::escape_module_path("github.com/Azure/azure-sdk"),
            "github.com/!azure/azure-sdk"
        );
        assert_eq!(
            GoProxyRegistry::escape_module_path("github.com/BurntSushi/toml"),
            "github.com/!burnt!sushi/toml"
        );
    }

    #[test]
    fn test_parse_version() {
        assert_eq!(
            GoProxyRegistry::parse_version("v1.2.3"),
            Some(semver::Version::new(1, 2, 3))
        );
        assert_eq!(
            GoProxyRegistry::parse_version("v0.10.0"),
            Some(semver::Version::new(0, 10, 0))
        );
        // Handle +incompatible suffix
        assert_eq!(
            GoProxyRegistry::parse_version("v2.0.0+incompatible"),
            Some(semver::Version::new(2, 0, 0))
        );
    }

    #[test]
    fn test_is_prerelease() {
        assert!(!GoProxyRegistry::is_prerelease("v1.0.0"));
        assert!(GoProxyRegistry::is_prerelease("v1.0.0-alpha.1"));
        assert!(GoProxyRegistry::is_prerelease("v1.0.0-rc1"));
        assert!(GoProxyRegistry::is_prerelease("v1.0.0-beta"));
    }

    #[test]
    fn test_detect_proxy_url() {
        // Test with direct/off values
        // SAFETY: Tests run single-threaded by default, no concurrent reads
        unsafe {
            std::env::set_var("GOPROXY", "direct");
        }
        assert_eq!(GoProxyRegistry::detect_proxy_url(), None);

        unsafe {
            std::env::set_var("GOPROXY", "off");
        }
        assert_eq!(GoProxyRegistry::detect_proxy_url(), None);

        // Test with valid proxy
        unsafe {
            std::env::set_var("GOPROXY", "https://proxy.example.com");
        }
        assert_eq!(
            GoProxyRegistry::detect_proxy_url(),
            Some("https://proxy.example.com".to_string())
        );

        // Test with comma-separated (first valid wins)
        unsafe {
            std::env::set_var("GOPROXY", "https://proxy1.com,https://proxy2.com,direct");
        }
        assert_eq!(
            GoProxyRegistry::detect_proxy_url(),
            Some("https://proxy1.com".to_string())
        );

        // Clean up
        unsafe {
            std::env::remove_var("GOPROXY");
        }
    }
}
