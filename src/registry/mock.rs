//! Mock registry for testing updaters without network calls.

use super::{RefNotFound, Registry, VersionMeta};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

/// A mock registry that returns pre-configured versions for testing.
pub struct MockRegistry {
    /// Map of package name to (stable_version, prerelease_version)
    versions: HashMap<String, (String, Option<String>)>,
    /// Map of package name + constraints to version
    constrained_versions: HashMap<(String, String), String>,
    /// Map of package name to full version metadata entries
    version_metas: HashMap<String, Vec<VersionMeta>>,
    /// Map of package name to the ref names a consumer could pin to
    ref_names: HashMap<String, Vec<String>>,
    /// Packages whose ref-name listing fails rather than answering
    unavailable_ref_names: HashSet<String>,
    /// Map of package name + ref to its immutable commit SHA
    resolved_refs: HashMap<(String, String), String>,
    /// Refs whose lookup fails without answering whether they exist
    unavailable_refs: HashSet<(String, String)>,
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
            ref_names: HashMap::new(),
            unavailable_ref_names: HashSet::new(),
            resolved_refs: HashMap::new(),
            unavailable_refs: HashSet::new(),
            name,
        }
    }

    /// Add a package with its latest stable version.
    pub fn with_version(mut self, package: &str, version: &str) -> Self {
        self.versions
            .insert(package.to_string(), (version.to_string(), None));
        self
    }

    /// Declare the ref names a package publishes, as `list_ref_names` reports
    /// them. Used to model a repo that ships `v4.1.2` without a floating `v4`.
    pub fn with_ref_names(mut self, package: &str, refs: &[&str]) -> Self {
        self.ref_names.insert(
            package.to_string(),
            refs.iter().map(|r| r.to_string()).collect(),
        );
        self
    }

    /// Declare a package whose ref-name listing fails without answering whether
    /// the repo publishes any refs, as a rate limit or an outage does. A package
    /// with no declared ref names answers with an empty list instead, which is
    /// the registry saying it has no ref concept.
    pub fn with_unavailable_ref_names(mut self, package: &str) -> Self {
        self.unavailable_ref_names.insert(package.to_string());
        self
    }

    /// Declare the commit SHA a Git ref resolves to.
    pub fn with_resolved_ref(mut self, package: &str, reference: &str, commit_sha: &str) -> Self {
        self.resolved_refs.insert(
            (package.to_string(), reference.to_string()),
            commit_sha.to_string(),
        );
        self
    }

    /// Declare a ref whose lookup fails without saying whether it exists, as a
    /// rate limit or an outage does. An undeclared ref is reported missing
    /// instead, which is the registry answering the question.
    pub fn with_unavailable_ref(mut self, package: &str, reference: &str) -> Self {
        self.unavailable_refs
            .insert((package.to_string(), reference.to_string()));
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

    async fn list_ref_names(&self, package: &str) -> Result<Vec<String>> {
        if self.unavailable_ref_names.contains(package) {
            return Err(anyhow!("Ref listing failed: {package}"));
        }
        Ok(self.ref_names.get(package).cloned().unwrap_or_default())
    }

    async fn resolve_ref_to_commit(&self, package: &str, reference: &str) -> Result<String> {
        let key = (package.to_string(), reference.to_string());
        if self.unavailable_refs.contains(&key) {
            return Err(anyhow!("Ref lookup failed: {package}@{reference}"));
        }
        self.resolved_refs.get(&key).cloned().ok_or_else(|| {
            anyhow!(RefNotFound::new(format!(
                "Ref not found: {package}@{reference}"
            )))
        })
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
