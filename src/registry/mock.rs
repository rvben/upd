//! Mock registry for testing updaters without network calls.

use super::{Registry, VersionMeta};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// A mock registry that returns pre-configured versions for testing.
pub struct MockRegistry {
    /// Map of package name to (stable_version, prerelease_version)
    versions: HashMap<String, (String, Option<String>)>,
    /// Map of package name + constraints to version
    constrained_versions: HashMap<(String, String), String>,
    /// Map of package name to full version metadata entries
    version_metas: HashMap<String, Vec<VersionMeta>>,
    /// Registry name
    name: &'static str,
}

impl MockRegistry {
    /// Create a new mock registry with the given name.
    pub fn new(name: &'static str) -> Self {
        Self {
            versions: HashMap::new(),
            constrained_versions: HashMap::new(),
            version_metas: HashMap::new(),
            name,
        }
    }

    /// Add a package with its latest stable version.
    pub fn with_version(mut self, package: &str, version: &str) -> Self {
        self.versions
            .insert(package.to_string(), (version.to_string(), None));
        self
    }

    /// Add a package with both stable and pre-release versions.
    pub fn with_prerelease(mut self, package: &str, stable: &str, prerelease: &str) -> Self {
        self.versions.insert(
            package.to_string(),
            (stable.to_string(), Some(prerelease.to_string())),
        );
        self
    }

    /// Add a full version metadata entry for a package.
    pub fn with_version_meta(
        mut self,
        package: &str,
        version: &str,
        published_at: Option<DateTime<Utc>>,
        yanked: bool,
        prerelease: bool,
    ) -> Self {
        self.version_metas
            .entry(package.to_string())
            .or_default()
            .push(VersionMeta {
                version: version.to_string(),
                published_at,
                yanked,
                prerelease,
            });
        self
    }

    /// Add a constrained version result for a package.
    pub fn with_constrained(mut self, package: &str, constraints: &str, version: &str) -> Self {
        self.constrained_versions.insert(
            (package.to_string(), constraints.to_string()),
            version.to_string(),
        );
        self
    }
}

#[async_trait]
impl Registry for MockRegistry {
    async fn get_latest_version(&self, package: &str) -> Result<String> {
        self.versions
            .get(package)
            .map(|(stable, _)| stable.clone())
            .ok_or_else(|| anyhow!("Package not found: {}", package))
    }

    async fn get_latest_version_including_prereleases(&self, package: &str) -> Result<String> {
        self.versions
            .get(package)
            .map(|(stable, prerelease)| prerelease.clone().unwrap_or_else(|| stable.clone()))
            .ok_or_else(|| anyhow!("Package not found: {}", package))
    }

    async fn get_latest_version_matching(
        &self,
        package: &str,
        constraints: &str,
    ) -> Result<String> {
        // Check for explicit constrained version first
        if let Some(version) = self
            .constrained_versions
            .get(&(package.to_string(), constraints.to_string()))
        {
            return Ok(version.clone());
        }

        // Fall back to stable version
        self.get_latest_version(package).await
    }

    async fn list_versions(&self, package: &str) -> Result<Vec<VersionMeta>> {
        Ok(self.version_metas.get(package).cloned().unwrap_or_default())
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_registry_basic() {
        let registry = MockRegistry::new("test")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        assert_eq!(
            registry.get_latest_version("requests").await.unwrap(),
            "2.31.0"
        );
        assert_eq!(registry.get_latest_version("flask").await.unwrap(), "3.0.0");
        assert!(registry.get_latest_version("nonexistent").await.is_err());
    }

    #[tokio::test]
    async fn test_mock_registry_prerelease() {
        let registry =
            MockRegistry::new("test").with_prerelease("mypackage", "1.0.0", "2.0.0-alpha.1");

        assert_eq!(
            registry.get_latest_version("mypackage").await.unwrap(),
            "1.0.0"
        );
        assert_eq!(
            registry
                .get_latest_version_including_prereleases("mypackage")
                .await
                .unwrap(),
            "2.0.0-alpha.1"
        );
    }

    #[tokio::test]
    async fn test_mock_registry_constrained() {
        let registry = MockRegistry::new("test")
            .with_version("django", "5.0.0")
            .with_constrained("django", ">=3.0,<4", "3.2.23");

        // Without constraints, returns latest
        assert_eq!(
            registry.get_latest_version("django").await.unwrap(),
            "5.0.0"
        );

        // With constraints, returns constrained version
        assert_eq!(
            registry
                .get_latest_version_matching("django", ">=3.0,<4")
                .await
                .unwrap(),
            "3.2.23"
        );
    }

    #[tokio::test]
    async fn test_mock_registry_name() {
        let registry = MockRegistry::new("PyPI");
        assert_eq!(registry.name(), "PyPI");
    }

    #[tokio::test]
    async fn test_mock_registry_list_versions_returns_added_metas() {
        use chrono::TimeZone;
        let published = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap();
        let registry = MockRegistry::new("npm")
            .with_version_meta("lodash", "4.17.21", Some(published), false, false)
            .with_version_meta("lodash", "4.17.22", None, true, false);

        let versions = registry.list_versions("lodash").await.unwrap();
        assert_eq!(versions.len(), 2);
        assert!(versions.iter().any(|v| v.version == "4.17.21" && !v.yanked));
        assert!(versions.iter().any(|v| v.version == "4.17.22" && v.yanked));
        assert!(
            versions
                .iter()
                .any(|v| v.version == "4.17.21" && v.published_at == Some(published))
        );
        assert!(
            versions
                .iter()
                .any(|v| v.version == "4.17.22" && v.published_at.is_none())
        );
    }

    #[tokio::test]
    async fn test_mock_registry_list_versions_empty_for_unknown_package() {
        let registry = MockRegistry::new("npm");
        let versions = registry.list_versions("nonexistent").await.unwrap();
        assert!(versions.is_empty());
    }
}
