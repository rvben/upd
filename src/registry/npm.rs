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
}

impl Default for NpmRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for NpmRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let url = format!("{}/{}", self.registry_url, package);

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch package '{}': HTTP {}",
                package,
                response.status()
            ));
        }

        let data: NpmResponse = response.json().await?;

        // Use the 'latest' dist-tag
        let latest = data
            .dist_tags
            .latest
            .ok_or_else(|| anyhow!("No 'latest' tag found for package '{}'", package))?;

        // Check if this version is deprecated
        if let Some(version_info) = data.versions.get(&latest) {
            if version_info.deprecated.is_some() {
                // Find the latest non-deprecated version
                let mut versions: Vec<_> = data
                    .versions
                    .iter()
                    .filter(|(_, info)| info.deprecated.is_none())
                    .filter_map(|(ver, _)| semver::Version::parse(ver).ok().map(|v| (v, ver)))
                    .collect();

                versions.sort_by(|a, b| b.0.cmp(&a.0));

                if let Some((_, ver)) = versions.first() {
                    return Ok((*ver).to_string());
                }
            }
        }

        Ok(latest)
    }

    fn name(&self) -> &'static str {
        "npm"
    }
}
