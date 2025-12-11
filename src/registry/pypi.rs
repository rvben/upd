use super::{Registry, get_with_retry};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use pep440_rs::{Version, VersionSpecifiers};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

pub struct PyPiRegistry {
    client: Client,
    index_url: String,
}

#[derive(Debug, Deserialize)]
struct PyPiResponse {
    releases: HashMap<String, Vec<ReleaseFile>>,
}

#[derive(Debug, Deserialize)]
struct ReleaseFile {
    yanked: Option<bool>,
}

impl PyPiRegistry {
    pub fn new() -> Self {
        Self::with_index_url("https://pypi.org/pypi".to_string())
    }

    pub fn with_index_url(index_url: String) -> Self {
        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, index_url }
    }

    /// Detect custom index URL from environment or config
    pub fn detect_index_url() -> Option<String> {
        // Check environment variables in order of precedence
        for var in ["UV_INDEX_URL", "PIP_INDEX_URL", "PYTHON_INDEX_URL"] {
            if let Ok(url) = std::env::var(var)
                && !url.is_empty()
            {
                return Some(url);
            }
        }
        None
    }

    fn is_stable_version(version_str: &str) -> bool {
        if let Ok(version) = version_str.parse::<Version>() {
            // Exclude pre-releases (alpha, beta, rc, dev)
            !version.is_pre() && !version.is_dev()
        } else {
            false
        }
    }

    /// Fetch all available stable versions for a package
    async fn fetch_versions(&self, package: &str) -> Result<Vec<(Version, String)>> {
        self.fetch_versions_internal(package, false).await
    }

    /// Fetch all versions including pre-releases
    async fn fetch_all_versions(&self, package: &str) -> Result<Vec<(Version, String)>> {
        self.fetch_versions_internal(package, true).await
    }

    /// Internal method to fetch versions with optional pre-release inclusion
    async fn fetch_versions_internal(
        &self,
        package: &str,
        include_prereleases: bool,
    ) -> Result<Vec<(Version, String)>> {
        let normalized = package.to_lowercase().replace('_', "-");
        let url = format!("{}/{}/json", self.index_url, normalized);

        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch package '{}': HTTP {}",
                package,
                response.status()
            ));
        }

        let data: PyPiResponse = response.json().await?;

        let mut versions: Vec<(Version, String)> = data
            .releases
            .iter()
            .filter(|(ver_str, files)| {
                let is_yanked = files.iter().all(|f| f.yanked.unwrap_or(false));
                if is_yanked {
                    return false;
                }
                include_prereleases || Self::is_stable_version(ver_str)
            })
            .filter_map(|(ver_str, _)| {
                ver_str
                    .parse::<Version>()
                    .ok()
                    .map(|v| (v, ver_str.clone()))
            })
            .collect();

        versions.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(versions)
    }
}

impl Default for PyPiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for PyPiRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let versions = self.fetch_versions(package).await?;

        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No stable versions found for package '{}'", package))
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let versions = self.fetch_all_versions(package).await?;

        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No versions found for package '{}'", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let versions = self.fetch_versions(package).await?;

        // Parse constraints (e.g., ">=2.8.0,<9" or ">=1.0.0")
        let specifiers = VersionSpecifiers::from_str(constraints).map_err(|e| {
            anyhow!(
                "Failed to parse version constraints '{}': {}",
                constraints,
                e
            )
        })?;

        // Find the highest version that matches all constraints
        for (version, version_str) in versions {
            if specifiers.contains(&version) {
                return Ok(version_str);
            }
        }

        Err(anyhow!(
            "No version of '{}' matches constraints '{}'",
            package,
            constraints
        ))
    }

    fn name(&self) -> &'static str {
        "pypi"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stable_version_detection() {
        assert!(PyPiRegistry::is_stable_version("1.0.0"));
        assert!(PyPiRegistry::is_stable_version("2.31.0"));
        assert!(!PyPiRegistry::is_stable_version("1.0.0a1"));
        assert!(!PyPiRegistry::is_stable_version("1.0.0b2"));
        assert!(!PyPiRegistry::is_stable_version("1.0.0rc1"));
        assert!(!PyPiRegistry::is_stable_version("1.0.0.dev1"));
    }
}
