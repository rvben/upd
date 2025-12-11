use super::{Registry, get_with_retry};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct CratesIoRegistry {
    client: Client,
    registry_url: String,
}

#[derive(Debug, Deserialize)]
struct CratesResponse {
    #[serde(rename = "crate")]
    krate: CrateInfo,
    versions: Vec<VersionInfo>,
}

#[derive(Debug, Deserialize)]
struct CrateInfo {
    max_stable_version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VersionInfo {
    num: String,
    yanked: bool,
}

impl CratesIoRegistry {
    pub fn new() -> Self {
        Self::with_registry_url("https://crates.io/api/v1/crates".to_string())
    }

    pub fn with_registry_url(registry_url: String) -> Self {
        let client = Client::builder()
            .gzip(true)
            // crates.io requires a descriptive User-Agent
            .user_agent(concat!(
                "upd/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/rvben/upd)"
            ))
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
        std::env::var("CARGO_REGISTRIES_CRATES_IO_INDEX")
            .ok()
            .filter(|s| !s.is_empty())
    }

    async fn fetch_crate(&self, name: &str) -> Result<CratesResponse> {
        let url = format!("{}/{}", self.registry_url, name);
        let response = get_with_retry(&self.client, &url).await?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to fetch crate '{}': HTTP {}",
                name,
                response.status()
            ));
        }

        Ok(response.json().await?)
    }

    fn get_sorted_versions(
        data: &CratesResponse,
        include_prereleases: bool,
    ) -> Vec<(semver::Version, String)> {
        let mut versions: Vec<_> = data
            .versions
            .iter()
            .filter(|v| !v.yanked)
            .filter_map(|v| {
                semver::Version::parse(&v.num).ok().and_then(|parsed| {
                    if include_prereleases || parsed.pre.is_empty() {
                        Some((parsed, v.num.clone()))
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

impl Default for CratesIoRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Registry for CratesIoRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        let data = self.fetch_crate(package).await?;

        // Fast path: use max_stable_version if available
        if let Some(ref max_stable) = data.krate.max_stable_version {
            return Ok(max_stable.clone());
        }

        // Fallback: find latest stable from versions list
        let versions = Self::get_sorted_versions(&data, false);
        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No stable versions found for crate '{}'", package))
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        let data = self.fetch_crate(package).await?;
        let versions = Self::get_sorted_versions(&data, true);

        versions
            .first()
            .map(|(_, s)| s.clone())
            .ok_or_else(|| anyhow!("No versions found for crate '{}'", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        let data = self.fetch_crate(package).await?;
        let versions = Self::get_sorted_versions(&data, false);

        let req = semver::VersionReq::parse(constraints).map_err(|e| {
            anyhow!(
                "Failed to parse version constraints '{}': {}",
                constraints,
                e
            )
        })?;

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
        "crates.io"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_sorting() {
        // Test that we properly filter and sort versions
        let data = CratesResponse {
            krate: CrateInfo {
                max_stable_version: None,
            },
            versions: vec![
                VersionInfo {
                    num: "1.0.0".to_string(),
                    yanked: false,
                },
                VersionInfo {
                    num: "2.0.0".to_string(),
                    yanked: false,
                },
                VersionInfo {
                    num: "1.5.0".to_string(),
                    yanked: true,
                },
                VersionInfo {
                    num: "3.0.0-alpha.1".to_string(),
                    yanked: false,
                },
            ],
        };

        let stable = CratesIoRegistry::get_sorted_versions(&data, false);
        assert_eq!(stable.len(), 2);
        assert_eq!(stable[0].1, "2.0.0");
        assert_eq!(stable[1].1, "1.0.0");

        let all = CratesIoRegistry::get_sorted_versions(&data, true);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].1, "3.0.0-alpha.1");
    }
}
