use super::Registry;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;

pub struct NpmRegistry {
    client: Client,
    registry_url: String,
}

#[derive(Debug, Deserialize)]
struct NpmResponse {
    #[serde(rename = "dist-tags")]
    dist_tags: DistTags,
    versions: HashMap<String, VersionInfo>,
}

#[derive(Debug, Deserialize)]
struct DistTags {
    latest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VersionInfo {
    deprecated: Option<String>,
}

impl NpmRegistry {
    pub fn new() -> Self {
        Self::with_registry_url("https://registry.npmjs.org".to_string())
    }

    pub fn with_registry_url(registry_url: String) -> Self {
        let client = Client::builder()
            .gzip(true)
            .user_agent("upd/0.1.0")
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

    /// Fetch package metadata from npm
    async fn fetch_package(&self, package: &str) -> Result<NpmResponse> {
        let url = format!("{}/{}", self.registry_url, package);
        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch package '{}': HTTP {}",
                package,
                response.status()
            ));
        }

        Ok(response.json().await?)
    }

    /// Get all stable (non-prerelease, non-deprecated) versions sorted descending
    fn get_stable_versions(data: &NpmResponse) -> Vec<(semver::Version, String)> {
        let mut versions: Vec<_> = data
            .versions
            .iter()
            .filter(|(_, info)| info.deprecated.is_none())
            .filter_map(|(ver_str, _)| {
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

        // Use the 'latest' dist-tag first
        if let Some(latest) = &data.dist_tags.latest {
            // Check if it's a pre-release or deprecated
            if let Some(version_info) = data.versions.get(latest) {
                if version_info.deprecated.is_none() {
                    if let Ok(v) = semver::Version::parse(latest) {
                        if v.pre.is_empty() {
                            return Ok(latest.clone());
                        }
                    }
                }
            }
        }

        // Fall back to finding the latest stable version
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
            anyhow!("Failed to parse version constraints '{}': {}", constraints, e)
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
