//! Configuration file support for upd
//!
//! Supports `.updrc.toml` and `upd.toml` configuration files.
//!
//! Example configuration:
//! ```toml
//! # Packages to ignore (never update)
//! ignore = [
//!     "some-legacy-package",
//!     "pinned-for-compatibility",
//! ]
//!
//! # Pin packages to specific versions or constraints
//! [pin]
//! requests = "2.28.0"  # Pin to exact version
//! django = ">=3.2,<4"  # Pin to version range
//! ```

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Maximum size for config files (1 MB) to prevent DoS
const MAX_CONFIG_FILE_SIZE: u64 = 1024 * 1024;

/// Configuration loaded from .updrc.toml or upd.toml
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdConfig {
    /// Package names to ignore (never update)
    #[serde(default)]
    pub ignore: Vec<String>,

    /// Package versions to pin (name -> version constraint)
    #[serde(default)]
    pub pin: HashMap<String, String>,
}

impl UpdConfig {
    /// Load configuration by searching for config files in the given directory and parents
    pub fn discover(start_dir: &Path) -> Option<(Self, PathBuf)> {
        let config_names = [".updrc.toml", "upd.toml", ".updrc"];

        let mut current = Some(start_dir);
        while let Some(dir) = current {
            for name in &config_names {
                let config_path = dir.join(name);
                if config_path.exists()
                    && let Some(config) = Self::load_from_path(&config_path)
                {
                    return Some((config, config_path));
                }
            }
            current = dir.parent();
        }

        None
    }

    /// Load configuration from a specific file path
    pub fn load_from_path(path: &Path) -> Option<Self> {
        // Check file size to prevent DoS
        if let Ok(metadata) = std::fs::metadata(path)
            && metadata.len() > MAX_CONFIG_FILE_SIZE
        {
            return None;
        }

        let content = std::fs::read_to_string(path).ok()?;
        toml::from_str(&content).ok()
    }

    /// Check if a package should be ignored
    pub fn should_ignore(&self, package: &str) -> bool {
        self.ignore.iter().any(|p| p == package)
    }

    /// Get the pinned version for a package (if any)
    pub fn get_pinned_version(&self, package: &str) -> Option<&str> {
        self.pin.get(package).map(|s| s.as_str())
    }

    /// Check if any configuration is present
    pub fn has_config(&self) -> bool {
        !self.ignore.is_empty() || !self.pin.is_empty()
    }

    /// Merge another configuration into this one (other takes precedence)
    pub fn merge(&mut self, other: Self) {
        // Extend ignore list
        for pkg in other.ignore {
            if !self.ignore.contains(&pkg) {
                self.ignore.push(pkg);
            }
        }
        // Override pinned versions
        for (pkg, version) in other.pin {
            self.pin.insert(pkg, version);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_load_config_from_toml() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".updrc.toml");

        let content = r#"
ignore = ["legacy-package", "old-lib"]

[pin]
requests = "2.28.0"
django = ">=3.2,<4"
"#;
        fs::write(&config_path, content).unwrap();

        let config = UpdConfig::load_from_path(&config_path).unwrap();

        assert_eq!(config.ignore.len(), 2);
        assert!(config.ignore.contains(&"legacy-package".to_string()));
        assert!(config.ignore.contains(&"old-lib".to_string()));

        assert_eq!(config.pin.len(), 2);
        assert_eq!(config.pin.get("requests"), Some(&"2.28.0".to_string()));
        assert_eq!(config.pin.get("django"), Some(&">=3.2,<4".to_string()));
    }

    #[test]
    fn test_should_ignore() {
        let config = UpdConfig {
            ignore: vec!["pkg-a".to_string(), "pkg-b".to_string()],
            pin: HashMap::new(),
        };

        assert!(config.should_ignore("pkg-a"));
        assert!(config.should_ignore("pkg-b"));
        assert!(!config.should_ignore("pkg-c"));
    }

    #[test]
    fn test_get_pinned_version() {
        let mut pin = HashMap::new();
        pin.insert("requests".to_string(), "2.28.0".to_string());
        pin.insert("django".to_string(), ">=3.2,<4".to_string());

        let config = UpdConfig {
            ignore: vec![],
            pin,
        };

        assert_eq!(config.get_pinned_version("requests"), Some("2.28.0"));
        assert_eq!(config.get_pinned_version("django"), Some(">=3.2,<4"));
        assert_eq!(config.get_pinned_version("flask"), None);
    }

    #[test]
    fn test_discover_config_in_current_dir() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".updrc.toml");

        let content = r#"
ignore = ["test-pkg"]
"#;
        fs::write(&config_path, content).unwrap();

        let result = UpdConfig::discover(temp_dir.path());
        assert!(result.is_some());
        let (config, path) = result.unwrap();
        assert_eq!(config.ignore.len(), 1);
        assert_eq!(path, config_path);
    }

    #[test]
    fn test_discover_config_in_parent_dir() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".updrc.toml");
        let subdir = temp_dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();

        let content = r#"
ignore = ["parent-pkg"]
"#;
        fs::write(&config_path, content).unwrap();

        let result = UpdConfig::discover(&subdir);
        assert!(result.is_some());
        let (config, path) = result.unwrap();
        assert_eq!(config.ignore.len(), 1);
        assert!(config.should_ignore("parent-pkg"));
        assert_eq!(path, config_path);
    }

    #[test]
    fn test_discover_prefers_closer_config() {
        let temp_dir = TempDir::new().unwrap();

        // Parent config
        let parent_config = temp_dir.path().join(".updrc.toml");
        fs::write(&parent_config, "ignore = [\"parent-pkg\"]").unwrap();

        // Child directory with its own config
        let subdir = temp_dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();
        let child_config = subdir.join(".updrc.toml");
        fs::write(&child_config, "ignore = [\"child-pkg\"]").unwrap();

        let result = UpdConfig::discover(&subdir);
        assert!(result.is_some());
        let (config, path) = result.unwrap();
        // Should find the child config, not the parent
        assert!(config.should_ignore("child-pkg"));
        assert!(!config.should_ignore("parent-pkg"));
        assert_eq!(path, child_config);
    }

    #[test]
    fn test_discover_no_config() {
        let temp_dir = TempDir::new().unwrap();
        let result = UpdConfig::discover(temp_dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_empty_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".updrc.toml");

        let content = "";
        fs::write(&config_path, content).unwrap();

        let config = UpdConfig::load_from_path(&config_path).unwrap();
        assert!(config.ignore.is_empty());
        assert!(config.pin.is_empty());
        assert!(!config.has_config());
    }

    #[test]
    fn test_has_config() {
        let empty = UpdConfig::default();
        assert!(!empty.has_config());

        let with_ignore = UpdConfig {
            ignore: vec!["pkg".to_string()],
            pin: HashMap::new(),
        };
        assert!(with_ignore.has_config());

        let mut pin = HashMap::new();
        pin.insert("pkg".to_string(), "1.0".to_string());
        let with_pin = UpdConfig {
            ignore: vec![],
            pin,
        };
        assert!(with_pin.has_config());
    }

    #[test]
    fn test_merge_configs() {
        let mut base = UpdConfig {
            ignore: vec!["pkg-a".to_string()],
            pin: {
                let mut m = HashMap::new();
                m.insert("requests".to_string(), "2.0.0".to_string());
                m
            },
        };

        let other = UpdConfig {
            ignore: vec!["pkg-b".to_string()],
            pin: {
                let mut m = HashMap::new();
                m.insert("requests".to_string(), "2.28.0".to_string()); // Override
                m.insert("django".to_string(), "3.2".to_string()); // New
                m
            },
        };

        base.merge(other);

        assert_eq!(base.ignore.len(), 2);
        assert!(base.should_ignore("pkg-a"));
        assert!(base.should_ignore("pkg-b"));

        // Other's pin takes precedence
        assert_eq!(base.get_pinned_version("requests"), Some("2.28.0"));
        assert_eq!(base.get_pinned_version("django"), Some("3.2"));
    }

    #[test]
    fn test_upd_toml_alternative_name() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("upd.toml");

        let content = r#"
ignore = ["alt-config-pkg"]
"#;
        fs::write(&config_path, content).unwrap();

        let result = UpdConfig::discover(temp_dir.path());
        assert!(result.is_some());
        let (config, _) = result.unwrap();
        assert!(config.should_ignore("alt-config-pkg"));
    }

    #[test]
    fn test_updrc_without_extension() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".updrc");

        let content = r#"
ignore = ["no-ext-pkg"]
"#;
        fs::write(&config_path, content).unwrap();

        let result = UpdConfig::discover(temp_dir.path());
        assert!(result.is_some());
        let (config, _) = result.unwrap();
        assert!(config.should_ignore("no-ext-pkg"));
    }

    // ==================== Integration Tests ====================

    /// Integration test: Config discovery with UpdateOptions
    #[test]
    fn test_config_integration_with_update_options() {
        use crate::updater::UpdateOptions;
        use std::sync::Arc;

        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".updrc.toml");

        let content = r#"
ignore = ["legacy-package", "deprecated-lib"]

[pin]
requests = "2.28.0"
flask = "2.0.0"
"#;
        fs::write(&config_path, content).unwrap();

        // Discover and load config
        let result = UpdConfig::discover(temp_dir.path());
        assert!(result.is_some());
        let (config, discovered_path) = result.unwrap();

        // Verify config was discovered from correct location
        assert_eq!(discovered_path, config_path);

        // Create UpdateOptions with config
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        // Verify UpdateOptions methods work correctly
        assert!(options.should_ignore("legacy-package"));
        assert!(options.should_ignore("deprecated-lib"));
        assert!(!options.should_ignore("some-other-package"));

        assert_eq!(options.get_pinned_version("requests"), Some("2.28.0"));
        assert_eq!(options.get_pinned_version("flask"), Some("2.0.0"));
        assert_eq!(options.get_pinned_version("django"), None);
    }

    /// Integration test: Config in subdirectory inherits from parent
    #[test]
    fn test_config_subdirectory_discovery_integration() {
        use crate::updater::UpdateOptions;
        use std::sync::Arc;

        let temp_dir = TempDir::new().unwrap();

        // Create parent config
        let parent_config = temp_dir.path().join(".updrc.toml");
        fs::write(
            &parent_config,
            r#"
ignore = ["parent-ignored"]
[pin]
parent-pkg = "1.0.0"
"#,
        )
        .unwrap();

        // Create subdirectory for a hypothetical project
        let project_dir = temp_dir.path().join("my-project");
        fs::create_dir(&project_dir).unwrap();

        // Discover from subdirectory (should find parent config)
        let result = UpdConfig::discover(&project_dir);
        assert!(result.is_some());
        let (config, discovered_path) = result.unwrap();

        // Should have found the parent config
        assert_eq!(discovered_path, parent_config);

        // Verify config values
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        assert!(options.should_ignore("parent-ignored"));
        assert_eq!(options.get_pinned_version("parent-pkg"), Some("1.0.0"));
    }

    /// Integration test: Full update cycle with config (using mock updater scenario)
    #[tokio::test]
    async fn test_full_config_workflow_with_requirements() {
        use crate::registry::MockRegistry;
        use crate::updater::{RequirementsUpdater, UpdateOptions, Updater};
        use std::io::Write;
        use std::sync::Arc;
        use tempfile::NamedTempFile;

        // Create requirements file
        let mut req_file = NamedTempFile::new().unwrap();
        writeln!(req_file, "requests>=2.20.0").unwrap();
        writeln!(req_file, "flask>=1.0.0").unwrap();
        writeln!(req_file, "django>=3.0.0").unwrap();
        req_file.flush().unwrap();

        // Create config
        let config = UpdConfig {
            ignore: vec!["requests".to_string()],
            pin: {
                let mut m = HashMap::new();
                m.insert("flask".to_string(), "2.0.0".to_string());
                m
            },
        };

        // Create mock registry
        let registry = MockRegistry::new("pypi")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "4.2.0");

        // Create updater with config
        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(true, false).with_config(Arc::new(config));

        let result = updater
            .update(req_file.path(), &registry, options)
            .await
            .unwrap();

        // Verify results:
        // - requests should be ignored
        // - flask should be pinned to 2.0.0 (appears in both pinned and updated)
        // - django should be updated to 4.2.0

        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "requests");

        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "flask");
        assert_eq!(result.pinned[0].2, "2.0.0"); // New version is pinned version

        // Both flask (pinned) and django (registry) are in updated
        assert_eq!(result.updated.len(), 2);

        // Verify contents by checking all updated packages
        let updated_names: Vec<&str> = result
            .updated
            .iter()
            .map(|(n, _, _, _)| n.as_str())
            .collect();
        assert!(updated_names.contains(&"flask"));
        assert!(updated_names.contains(&"django"));
    }

    /// Integration test: Config with all supported file types
    #[tokio::test]
    async fn test_config_applies_to_multiple_file_types() {
        use crate::registry::MockRegistry;
        use crate::updater::{
            CargoTomlUpdater, PackageJsonUpdater, RequirementsUpdater, UpdateOptions, Updater,
        };
        use std::io::Write;
        use std::sync::Arc;
        use tempfile::NamedTempFile;

        // Shared config
        let config = Arc::new(UpdConfig {
            ignore: vec!["ignored-pkg".to_string()],
            pin: {
                let mut m = HashMap::new();
                m.insert("pinned-pkg".to_string(), "1.5.0".to_string());
                m
            },
        });

        // Test Requirements
        // RequirementsUpdater: pinned packages appear in BOTH pinned AND updated
        {
            let mut file = NamedTempFile::new().unwrap();
            writeln!(file, "ignored-pkg>=1.0.0").unwrap();
            writeln!(file, "pinned-pkg>=1.0.0").unwrap();
            writeln!(file, "other-pkg>=1.0.0").unwrap();
            file.flush().unwrap();

            let registry = MockRegistry::new("pypi")
                .with_version("ignored-pkg", "2.0.0")
                .with_version("pinned-pkg", "2.0.0")
                .with_version("other-pkg", "2.0.0");

            let updater = RequirementsUpdater::new();
            let options = UpdateOptions::new(true, false).with_config(Arc::clone(&config));
            let result = updater
                .update(file.path(), &registry, options)
                .await
                .unwrap();

            assert_eq!(result.ignored.len(), 1);
            assert_eq!(result.pinned.len(), 1);
            // RequirementsUpdater counts pinned packages in both updated and pinned
            assert_eq!(result.updated.len(), 2); // pinned-pkg + other-pkg
        }

        // Test package.json
        // PackageJsonUpdater: pinned packages appear ONLY in pinned
        {
            let mut file = NamedTempFile::new().unwrap();
            write!(
                file,
                r#"{{
  "dependencies": {{
    "ignored-pkg": "^1.0.0",
    "pinned-pkg": "^1.0.0",
    "other-pkg": "^1.0.0"
  }}
}}"#
            )
            .unwrap();
            file.flush().unwrap();

            let registry = MockRegistry::new("npm")
                .with_version("ignored-pkg", "2.0.0")
                .with_version("pinned-pkg", "2.0.0")
                .with_version("other-pkg", "2.0.0");

            let updater = PackageJsonUpdater::new();
            let options = UpdateOptions::new(true, false).with_config(Arc::clone(&config));
            let result = updater
                .update(file.path(), &registry, options)
                .await
                .unwrap();

            assert_eq!(result.ignored.len(), 1);
            assert_eq!(result.pinned.len(), 1);
            // PackageJsonUpdater counts pinned packages only in pinned, not updated
            assert_eq!(result.updated.len(), 1); // only other-pkg
        }

        // Test Cargo.toml
        // CargoTomlUpdater: pinned packages appear ONLY in pinned
        {
            let mut file = NamedTempFile::new().unwrap();
            write!(
                file,
                r#"[package]
name = "test"
version = "0.1.0"

[dependencies]
ignored-pkg = "1.0"
pinned-pkg = "1.0"
other-pkg = "1.0"
"#
            )
            .unwrap();
            file.flush().unwrap();

            let registry = MockRegistry::new("crates-io")
                .with_version("ignored-pkg", "2.0.0")
                .with_version("pinned-pkg", "2.0.0")
                .with_version("other-pkg", "2.0.0");

            let updater = CargoTomlUpdater::new();
            let options = UpdateOptions::new(true, false).with_config(Arc::clone(&config));
            let result = updater
                .update(file.path(), &registry, options)
                .await
                .unwrap();

            assert_eq!(result.ignored.len(), 1);
            assert_eq!(result.pinned.len(), 1);
            // CargoTomlUpdater counts pinned packages only in pinned, not updated
            assert_eq!(result.updated.len(), 1); // only other-pkg
        }
    }
}
