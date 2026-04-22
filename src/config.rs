//! Configuration file support for upd
//!
//! Supports `.updrc.toml` and `upd.toml` configuration files.
//!
//! # Schema
//!
//! All configuration fields are top-level keys:
//!
//! ```toml
//! # Packages to ignore (never update) — top-level array
//! ignore = [
//!     "some-legacy-package",
//!     "pinned-for-compatibility",
//! ]
//!
//! # Pin packages to specific versions or constraints — top-level table
//! [pin]
//! requests = "2.28.0"  # Pin to exact version
//! django = ">=3.2,<4"  # Pin to version range
//! ```
//!
//! Unknown top-level keys produce a warning on stderr but do not stop execution.
//! A common mistake is writing `[ignore]` (table) instead of `ignore = [...]` (array).
//!
//! # Minimum release age (cooldown)
//!
//! Opt-in cooldown policy delays updates to versions published
//! within a configurable window (e.g. 7 days). Overrides:
//!
//! ```toml
//! [cooldown]
//! default = "7d"
//!
//! [cooldown.ecosystem]
//! npm = "14d"
//! "crates.io" = "3d"
//! ```
//!
//! Valid duration units: `s`, `m`, `h`, `d`, `w`. Use `"0"` to disable.

use colored::Colorize;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Maximum size for config files (1 MB) to prevent DoS
const MAX_CONFIG_FILE_SIZE: u64 = 1024 * 1024;

/// All valid top-level keys in the config schema.
const KNOWN_KEYS: &[&str] = &["ignore", "pin", "cooldown"];

/// Raw cooldown config as written in the TOML file. Parsed into a
/// `crate::cooldown::CooldownPolicy` at runtime via `UpdConfig::to_cooldown_policy`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CooldownConfig {
    /// Default cooldown applied to all ecosystems (e.g. "7d").
    #[serde(default)]
    pub default: Option<String>,
    /// Per-ecosystem overrides, keyed by registry name.
    #[serde(default)]
    pub ecosystem: HashMap<String, String>,
}

/// Configuration loaded from .updrc.toml or upd.toml
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdConfig {
    /// Package names to ignore (never update)
    #[serde(default)]
    pub ignore: Vec<String>,

    /// Package versions to pin (name -> version constraint)
    #[serde(default)]
    pub pin: HashMap<String, String>,

    /// Optional cooldown (minimum release age) policy.
    #[serde(default)]
    pub cooldown: Option<CooldownConfig>,
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

    /// Load configuration from a specific file path.
    ///
    /// Parse errors are printed to stderr so the user can see that their config
    /// was ignored. Warnings for unknown keys are also printed to stderr.
    pub fn load_from_path(path: &Path) -> Option<Self> {
        match Self::load_with_warnings(path) {
            Ok((config, warnings)) => {
                for w in &warnings {
                    eprintln!("warning: {w}");
                }
                Some(config)
            }
            Err(e) => {
                eprintln!(
                    "{} failed to parse {}: {}",
                    "error:".red(),
                    path.display(),
                    e
                );
                None
            }
        }
    }

    /// Load configuration from a specific file path with detailed error messages.
    ///
    /// Warnings for unknown keys are printed to stderr.
    pub fn load_from_path_with_error(path: &Path) -> Result<Self, String> {
        let (config, warnings) = Self::load_with_warnings(path)?;
        for w in &warnings {
            eprintln!("warning: {w}");
        }
        Ok(config)
    }

    /// Load configuration and return any unknown-key warnings alongside the parsed config.
    ///
    /// The caller decides how to surface warnings (print to stderr, collect, etc.).
    pub fn load_with_warnings(path: &Path) -> Result<(Self, Vec<String>), String> {
        // Check if file exists
        if !path.exists() {
            return Err(format!("Config file not found: {}", path.display()));
        }

        // Check file size to prevent DoS
        match std::fs::metadata(path) {
            Ok(metadata) => {
                if metadata.len() > MAX_CONFIG_FILE_SIZE {
                    return Err(format!(
                        "Config file too large: {} bytes (max {} MB). Consider splitting into multiple files.",
                        metadata.len(),
                        MAX_CONFIG_FILE_SIZE / 1024 / 1024
                    ));
                }
            }
            Err(e) => {
                return Err(format!(
                    "Cannot read config file metadata: {}. Check file permissions.",
                    e
                ));
            }
        }

        // Read file content
        let content = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                format!(
                    "Permission denied reading config file: {}. Check file permissions.",
                    path.display()
                )
            } else {
                format!("Failed to read config file {}: {}", path.display(), e)
            }
        })?;

        Self::parse_with_warnings(&content, path.to_string_lossy().as_ref())
    }

    /// Parse config TOML content, collecting warnings for unknown top-level keys.
    ///
    /// `source_label` is used in warning messages (typically the file path).
    pub fn parse_with_warnings(
        content: &str,
        source_label: &str,
    ) -> Result<(Self, Vec<String>), String> {
        // Parse as generic TOML Value to detect unknown keys before deserializing
        // into the typed struct. This lets us warn without hard-rejecting.
        let raw: toml::Value = toml::from_str(content)
            .map_err(|e| format!("Invalid TOML in config file {}:\n  {}", source_label, e))?;

        let mut warnings = Vec::new();
        if let toml::Value::Table(table) = &raw {
            for key in table.keys() {
                if !KNOWN_KEYS.contains(&key.as_str()) {
                    warnings.push(format!(
                        "unknown key `{}` in config file {}; valid keys are: {}. \
                         Run `upd --show-config` to see the expected schema.",
                        key,
                        source_label,
                        KNOWN_KEYS.join(", ")
                    ));
                }
            }
        }

        // Warn on unknown ecosystem keys inside [cooldown.ecosystem].
        const KNOWN_ECOSYSTEMS: &[&str] = &[
            "pypi",
            "npm",
            "crates.io",
            "go-proxy",
            "github-releases",
            "rubygems",
            "terraform",
            "nuget",
        ];
        if let toml::Value::Table(table) = &raw
            && let Some(toml::Value::Table(cooldown)) = table.get("cooldown")
            && let Some(toml::Value::Table(ecosystem)) = cooldown.get("ecosystem")
        {
            for key in ecosystem.keys() {
                if !KNOWN_ECOSYSTEMS.contains(&key.as_str()) {
                    warnings.push(format!(
                        "unknown ecosystem `{}` in [cooldown.ecosystem] in config file {}; \
                         valid ecosystems are: {}. Run `upd --show-config` for the expected schema.",
                        key,
                        source_label,
                        KNOWN_ECOSYSTEMS.join(", ")
                    ));
                }
            }
        }

        // Parse into typed struct (uses the already-validated TOML)
        let config: Self = raw
            .try_into()
            .map_err(|e| format!("Invalid TOML in config file {}:\n  {}", source_label, e))?;

        Ok((config, warnings))
    }

    /// Return the canonical schema as a TOML string.
    ///
    /// Used by `--show-config` to help users understand the expected format.
    pub fn schema_toml() -> &'static str {
        r#"# upd configuration schema
# File names: .updrc.toml, upd.toml, or .updrc
# Searched from the current directory upward.

# ignore: packages that upd should never update (top-level array of strings)
ignore = []

# pin: packages pinned to a specific version or constraint (top-level table)
[pin]
# example-package = "1.2.3"
# another-package = ">=2.0,<3"

# cooldown: minimum release age before upd will update to a version.
# Accepts durations like "0" (disabled), "72h", "7d", "2w".
[cooldown]
# default = "7d"         # applied to every ecosystem unless overridden below

# Per-ecosystem overrides. Valid keys: pypi, npm, crates.io, go-proxy,
# github-releases, rubygems, terraform, nuget.
[cooldown.ecosystem]
# npm = "14d"
# pypi = "14d"
# "crates.io" = "3d"
"#
    }

    /// Resolve the raw config into a `CooldownPolicy`.
    ///
    /// `cli_override` corresponds to the `--min-age` CLI flag; when set it
    /// becomes the policy's `force_override` and wins over all config values.
    pub fn to_cooldown_policy(
        &self,
        cli_override: Option<&str>,
    ) -> anyhow::Result<crate::cooldown::CooldownPolicy> {
        use crate::cooldown::{CooldownPolicy, parse_duration};

        let default = match self.cooldown.as_ref().and_then(|c| c.default.as_deref()) {
            Some(s) => parse_duration(s)
                .map_err(|e| anyhow::anyhow!("invalid [cooldown] default '{s}': {e}"))?,
            None => chrono::Duration::zero(),
        };

        let mut per_ecosystem = std::collections::HashMap::new();
        if let Some(cc) = self.cooldown.as_ref() {
            for (ecosystem, raw) in &cc.ecosystem {
                let d = parse_duration(raw).map_err(|e| {
                    anyhow::anyhow!("invalid [cooldown.ecosystem.{ecosystem}] '{raw}': {e}")
                })?;
                per_ecosystem.insert(ecosystem.clone(), d);
            }
        }

        let force_override = match cli_override {
            Some(s) => Some(
                parse_duration(s).map_err(|e| anyhow::anyhow!("invalid --min-age '{s}': {e}"))?,
            ),
            None => None,
        };

        Ok(CooldownPolicy {
            default,
            per_ecosystem,
            force_override,
        })
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
        !self.ignore.is_empty() || !self.pin.is_empty() || self.cooldown.is_some()
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
        // Child cooldown overrides parent entirely when set
        if other.cooldown.is_some() {
            self.cooldown = other.cooldown;
        }
    }
}

/// Render the resolved cooldown for human display in `--show-config`.
pub fn render_cooldown_for_show_config(policy: &crate::cooldown::CooldownPolicy) -> String {
    fn fmt_dur(d: chrono::Duration) -> String {
        let secs = d.num_seconds();
        if secs == 0 {
            return "disabled".to_string();
        }
        let days = d.num_days();
        if days * 86_400 == secs {
            return format!("{days}d");
        }
        let hours = d.num_hours();
        if hours * 3600 == secs {
            return format!("{hours}h");
        }
        let minutes = d.num_minutes();
        if minutes * 60 == secs {
            return format!("{minutes}m");
        }
        format!("{secs}s")
    }

    let mut out = String::from("cooldown:\n");
    if let Some(d) = policy.force_override {
        out.push_str(&format!("  (--min-age override active: {})\n", fmt_dur(d)));
    }
    out.push_str(&format!("  default: {}\n", fmt_dur(policy.default)));
    if policy.per_ecosystem.is_empty() {
        out.push_str("  ecosystem: (no overrides)\n");
    } else {
        out.push_str("  ecosystem:\n");
        let mut entries: Vec<_> = policy.per_ecosystem.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (name, dur) in entries {
            out.push_str(&format!("    {name}: {}\n", fmt_dur(*dur)));
        }
    }
    out
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
            cooldown: None,
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
            cooldown: None,
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
            cooldown: None,
        };
        assert!(with_ignore.has_config());

        let mut pin = HashMap::new();
        pin.insert("pkg".to_string(), "1.0".to_string());
        let with_pin = UpdConfig {
            ignore: vec![],
            pin,
            cooldown: None,
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
            cooldown: None,
        };

        let other = UpdConfig {
            ignore: vec!["pkg-b".to_string()],
            pin: {
                let mut m = HashMap::new();
                m.insert("requests".to_string(), "2.28.0".to_string()); // Override
                m.insert("django".to_string(), "3.2".to_string()); // New
                m
            },
            cooldown: None,
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
            cooldown: None,
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
        // - flask should be pinned to 2.0.0
        // - django should be updated to 4.2.0

        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "requests");

        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "flask");
        assert_eq!(result.pinned[0].2, "2.0.0"); // New version is pinned version

        assert_eq!(result.updated.len(), 1);

        // Verify contents by checking all updated packages
        let updated_names: Vec<&str> = result
            .updated
            .iter()
            .map(|(n, _, _, _)| n.as_str())
            .collect();
        assert!(updated_names.contains(&"django"));
        assert!(!updated_names.contains(&"flask"));
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
            cooldown: None,
        });

        // Test Requirements
        // RequirementsUpdater: pinned packages appear only in pinned
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
            assert_eq!(result.updated.len(), 1); // other-pkg only
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

    // ==================== Unknown Key / Schema Validation Tests ====================

    #[test]
    fn test_valid_config_parses_cleanly_no_warnings() {
        // Regression guard: a fully-valid config produces zero warnings.
        let content = r#"
ignore = ["legacy-package", "old-lib"]

[pin]
requests = "2.28.0"
django = ">=3.2,<4"
"#;
        let (config, warnings) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();
        assert!(
            warnings.is_empty(),
            "no warnings expected for valid config; got: {warnings:?}"
        );
        assert_eq!(config.ignore.len(), 2);
        assert_eq!(config.pin.len(), 2);
    }

    #[test]
    fn test_unknown_top_level_key_produces_warning_config_still_loads() {
        // A config with an unknown key should emit a warning but still load correctly.
        // The classic mistake: `[ignore] packages = [...]` creates a TABLE called ignore.
        let content = r#"
ignore = ["valid-pkg"]

[unknown_section]
foo = "bar"
"#;
        let (config, warnings) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();

        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one warning; got: {warnings:?}"
        );
        assert!(
            warnings[0].contains("unknown"),
            "warning should mention 'unknown': {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("unknown_section"),
            "warning should name the offending key: {}",
            warnings[0]
        );
        // Config still loaded the valid parts
        assert_eq!(config.ignore.len(), 1);
        assert!(config.should_ignore("valid-pkg"));
    }

    #[test]
    fn test_multiple_unknown_keys_produce_multiple_warnings() {
        let content = r#"
ignore = ["pkg-a"]

[typo_ignore]
packages = ["pkg-b"]

[extras]
foo = "bar"
"#;
        let (_, warnings) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();
        assert_eq!(
            warnings.len(),
            2,
            "expected two warnings; got: {warnings:?}"
        );
        let all = warnings.join("\n");
        assert!(
            all.contains("typo_ignore"),
            "should mention typo_ignore: {all}"
        );
        assert!(all.contains("extras"), "should mention extras: {all}");
    }

    #[test]
    fn test_show_config_hint_in_warning() {
        // Warnings must reference --show-config so users know how to get help.
        let content = r#"
[wrong_section]
foo = "bar"
"#;
        let (_, warnings) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();
        assert!(!warnings.is_empty());
        assert!(
            warnings[0].contains("show-config"),
            "warning should mention --show-config: {}",
            warnings[0]
        );
    }

    #[test]
    fn test_classic_wrong_format_warns_and_loads_empty() {
        // [ignore] packages = [...] is the documented wrong format.
        // It creates a TABLE under `ignore`, which our struct can't deserialize as Vec<String>.
        // The raw table parse should detect `ignore` is a table (unknown shape) but since
        // `ignore` IS a known key name, it won't produce an unknown-key warning.
        // However, the deserialization into Vec<String> will fail with a type error.
        let content = r#"
[ignore]
packages = ["some-package"]
"#;
        // This should fail to parse since ignore is expected to be an array
        let result = UpdConfig::parse_with_warnings(content, "test.toml");
        assert!(result.is_err(), "wrong format should produce a parse error");
        let err = result.unwrap_err();
        assert!(
            err.contains("Invalid TOML"),
            "error should mention 'Invalid TOML': {err}"
        );
    }

    // ==================== Error Handling Tests ====================

    #[test]
    fn test_load_from_path_with_error_file_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent = temp_dir.path().join("nonexistent.toml");

        let result = UpdConfig::load_from_path_with_error(&nonexistent);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("not found"),
            "Error should mention 'not found': {}",
            err
        );
    }

    #[test]
    fn test_load_from_path_with_error_invalid_toml() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("invalid.toml");

        // Invalid TOML syntax
        fs::write(&config_path, "ignore = [invalid syntax").unwrap();

        let result = UpdConfig::load_from_path_with_error(&config_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("Invalid TOML"),
            "Error should mention 'Invalid TOML': {}",
            err
        );
    }

    #[test]
    fn test_load_from_path_with_error_wrong_type() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("wrong_type.toml");

        // Wrong type: ignore should be array, not string
        fs::write(&config_path, "ignore = \"not-an-array\"").unwrap();

        let result = UpdConfig::load_from_path_with_error(&config_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("Invalid TOML"),
            "Error should mention 'Invalid TOML': {}",
            err
        );
    }

    #[test]
    fn test_load_from_path_with_error_success() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("valid.toml");

        fs::write(&config_path, "ignore = [\"pkg1\", \"pkg2\"]").unwrap();

        let result = UpdConfig::load_from_path_with_error(&config_path);
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.ignore.len(), 2);
    }

    // ==================== Cooldown Config Tests ====================

    #[test]
    fn test_config_parses_cooldown_default() {
        let content = r#"
[cooldown]
default = "7d"
"#;
        let (config, warnings) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        let cooldown = config.cooldown.as_ref().expect("cooldown section");
        assert_eq!(cooldown.default.as_deref(), Some("7d"));
        assert!(cooldown.ecosystem.is_empty());
    }

    #[test]
    fn test_config_parses_cooldown_ecosystem_overrides() {
        let content = r#"
[cooldown]
default = "7d"

[cooldown.ecosystem]
npm = "14d"
"crates.io" = "3d"
"#;
        let (config, warnings) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        let cooldown = config.cooldown.unwrap();
        assert_eq!(cooldown.default.as_deref(), Some("7d"));
        assert_eq!(
            cooldown.ecosystem.get("npm").map(String::as_str),
            Some("14d")
        );
        assert_eq!(
            cooldown.ecosystem.get("crates.io").map(String::as_str),
            Some("3d")
        );
    }

    #[test]
    fn test_config_warns_on_unknown_ecosystem_key() {
        let content = r#"
[cooldown.ecosystem]
pipy = "7d"
"#;
        let (_, warnings) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();
        assert_eq!(warnings.len(), 1, "expected one warning, got: {warnings:?}");
        assert!(
            warnings[0].contains("pipy"),
            "warning should name the bad key: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("ecosystem"),
            "warning should mention ecosystem context: {}",
            warnings[0]
        );
    }

    #[test]
    fn test_config_to_cooldown_policy_empty() {
        let config = UpdConfig::default();
        let policy = config.to_cooldown_policy(None).unwrap();
        assert_eq!(policy.default, chrono::Duration::zero());
        assert!(policy.per_ecosystem.is_empty());
        assert!(policy.force_override.is_none());
    }

    #[test]
    fn test_config_to_cooldown_policy_parses_durations() {
        let content = r#"
[cooldown]
default = "7d"

[cooldown.ecosystem]
npm = "14d"
"#;
        let (config, _) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();
        let policy = config.to_cooldown_policy(None).unwrap();
        assert_eq!(policy.default, chrono::Duration::days(7));
        assert_eq!(
            policy.per_ecosystem.get("npm"),
            Some(&chrono::Duration::days(14))
        );
    }

    #[test]
    fn test_config_to_cooldown_policy_honours_cli_override() {
        let content = r#"
[cooldown]
default = "7d"
"#;
        let (config, _) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();
        let policy = config.to_cooldown_policy(Some("0")).unwrap();
        assert_eq!(policy.force_override, Some(chrono::Duration::zero()));
    }

    #[test]
    fn test_config_to_cooldown_policy_rejects_bad_duration() {
        let content = r#"
[cooldown]
default = "nope"
"#;
        let (config, _) = UpdConfig::parse_with_warnings(content, "test.toml").unwrap();
        let err = config.to_cooldown_policy(None).unwrap_err().to_string();
        assert!(
            err.contains("cooldown"),
            "error should mention cooldown: {err}"
        );
    }

    // ==================== schema_toml / render_cooldown_for_show_config Tests ====================

    #[test]
    fn test_schema_toml_includes_cooldown_section() {
        let schema = UpdConfig::schema_toml();
        assert!(
            schema.contains("[cooldown]"),
            "schema should document [cooldown]: {schema}"
        );
        assert!(
            schema.contains("default ="),
            "schema should show `default =`: {schema}"
        );
        assert!(
            schema.contains("[cooldown.ecosystem]"),
            "schema should document per-ecosystem overrides: {schema}"
        );
    }

    #[test]
    fn test_render_cooldown_for_show_config_formats_durations() {
        let mut per = std::collections::HashMap::new();
        per.insert("npm".to_string(), chrono::Duration::days(14));
        let policy = crate::cooldown::CooldownPolicy {
            default: chrono::Duration::days(7),
            per_ecosystem: per,
            force_override: None,
        };
        let rendered = render_cooldown_for_show_config(&policy);
        assert!(rendered.contains("default: 7d"), "{rendered}");
        assert!(rendered.contains("npm: 14d"), "{rendered}");
    }

    #[test]
    fn test_render_cooldown_for_show_config_shows_override() {
        let policy = crate::cooldown::CooldownPolicy {
            default: chrono::Duration::days(7),
            per_ecosystem: std::collections::HashMap::new(),
            force_override: Some(chrono::Duration::zero()),
        };
        let rendered = render_cooldown_for_show_config(&policy);
        assert!(
            rendered.contains("--min-age override active: disabled"),
            "should note override: {rendered}"
        );
    }
}
