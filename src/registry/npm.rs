use super::Registry;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

pub struct NpmRegistry {
    client: Client,
    registry_url: String,
}

/// Abbreviated npm response (smaller, faster)
/// Uses Accept: application/vnd.npm.install-v1+json
#[derive(Debug, Deserialize)]
struct NpmAbbreviatedResponse {
    #[serde(rename = "dist-tags")]
    dist_tags: DistTags,
    /// Version keys only (we parse them dynamically to avoid large struct)
    versions: Value,
}

#[derive(Debug, Deserialize)]
struct DistTags {
    latest: Option<String>,
}

impl NpmRegistry {
    pub fn new() -> Self {
        Self::with_registry_url("https://registry.npmjs.org".to_string())
    }

    pub fn with_registry_url(registry_url: String) -> Self {
        let client = Client::builder()
            .gzip(true)
            .user_agent(concat!("upd/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            registry_url,
        }
    }

    /// Detect custom registry URL from environment
    pub fn detect_registry_url() -> Option<String> {
        std::env::var("NPM_REGISTRY").ok().filter(|s| !s.is_empty())
    }

    /// Fetch abbreviated package metadata from npm
    /// Uses the install-v1 format which is smaller and faster
    async fn fetch_package(&self, package: &str) -> Result<NpmAbbreviatedResponse> {
        let url = format!("{}/{}", self.registry_url, package);

        // Use abbreviated metadata format (much smaller for large packages like react)
        let response = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.npm.install-v1+json")
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch package '{}': HTTP {}",
                package,
                response.status()
            ));
        }

        response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse npm response for '{}': {}", package, e))
    }

    /// Get all stable (non-prerelease) versions sorted descending
    /// Note: abbreviated metadata doesn't include deprecated info, but dist-tags.latest
    /// is authoritative for the recommended version
    fn get_stable_versions(data: &NpmAbbreviatedResponse) -> Vec<(semver::Version, String)> {
        let versions_obj = match data.versions.as_object() {
            Some(obj) => obj,
            None => return Vec::new(),
        };

        let mut versions: Vec<_> = versions_obj
            .keys()
            .filter_map(|ver_str| {
                semver::Version::parse(ver_str).ok().and_then(|v| {
                    // Skip pre-release versions
                    if v.pre.is_empty() {
                        Some((v, ver_str.clone()))
                    } else {
                        None
                    }
                })
            })
            .collect();

        versions.sort_by(|a, b| b.0.cmp(&a.0));
        versions
    }
}

impl Default for NpmRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for NpmRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let data = self.fetch_package(package).await?;

        // Use the 'latest' dist-tag (this is the authoritative answer from npm)
        if let Some(latest) = &data.dist_tags.latest
            && let Ok(v) = semver::Version::parse(latest)
            && v.pre.is_empty()
        {
            return Ok(latest.clone());
        }

        // Fall back to finding the latest stable version from the versions list
        let versions = Self::get_stable_versions(&data);
        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No stable versions found for package '{}'", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let data = self.fetch_package(package).await?;
        let versions = Self::get_stable_versions(&data);

        // Parse npm-style version requirements (^1.0.0, ~2.0.0, >=1.0.0 <2.0.0, etc.)
        let req = semver::VersionReq::parse(constraints).map_err(|e| {
            anyhow!(
                "Failed to parse version constraints '{}': {}",
                constraints,
                e
            )
        })?;

        // Find the highest version that matches
        for (version, version_str) in versions {
            if req.matches(&version) {
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
        "npm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_stable_versions() {
        // Create a mock response with versions
        let json = serde_json::json!({
            "dist-tags": {
                "latest": "2.0.0"
            },
            "versions": {
                "1.0.0": {},
                "1.1.0": {},
                "2.0.0": {},
                "2.1.0-beta.1": {},
                "2.1.0-alpha.1": {}
            }
        });

        let response: NpmAbbreviatedResponse = serde_json::from_value(json).unwrap();
        let versions = NpmRegistry::get_stable_versions(&response);

        // Should only include stable versions, sorted descending
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].1, "2.0.0");
        assert_eq!(versions[1].1, "1.1.0");
        assert_eq!(versions[2].1, "1.0.0");
    }

    #[test]
    fn test_get_stable_versions_filters_prereleases() {
        let json = serde_json::json!({
            "dist-tags": {
                "latest": "1.0.0"
            },
            "versions": {
                "1.0.0": {},
                "2.0.0-rc.1": {},
                "2.0.0-beta.5": {},
                "2.0.0-alpha.1": {}
            }
        });

        let response: NpmAbbreviatedResponse = serde_json::from_value(json).unwrap();
        let versions = NpmRegistry::get_stable_versions(&response);

        // Only stable version should be included
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].1, "1.0.0");
    }

    #[test]
    fn test_registry_name() {
        let registry = NpmRegistry::new();
        assert_eq!(registry.name(), "npm");
    }

    #[test]
    fn test_with_registry_url() {
        let registry = NpmRegistry::with_registry_url("https://custom.registry.com".to_string());
        assert_eq!(registry.registry_url, "https://custom.registry.com");
    }
}
