use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, read_file_safe,
    write_file_atomic,
};
use crate::registry::Registry;
use crate::version::match_version_precision;
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use serde_json::Value;
use std::path::Path;

pub struct PackageJsonUpdater;

impl PackageJsonUpdater {
    pub fn new() -> Self {
        Self
    }

    fn extract_version_info(&self, version_str: &str) -> (String, String) {
        // Extract prefix and version from strings like "^1.0.0", "~2.0.0", ">=3.0.0"
        let prefixes = [">=", "<=", "~>", "^", "~", ">", "<"];

        for prefix in prefixes {
            if let Some(stripped) = version_str.strip_prefix(prefix) {
                return (prefix.to_string(), stripped.to_string());
            }
        }

        // No prefix
        (String::new(), version_str.to_string())
    }

    /// Find the line number where a package is defined
    fn find_package_line(&self, content: &str, package: &str) -> Option<usize> {
        let pattern = format!(r#""{}""#, regex::escape(package));
        let re = Regex::new(&pattern).ok()?;

        for (line_idx, line) in content.lines().enumerate() {
            if re.is_match(line) {
                return Some(line_idx + 1); // 1-indexed
            }
        }
        None
    }

    fn update_version_in_content(
        &self,
        content: &str,
        package: &str,
        old_version: &str,
        new_version: &str,
    ) -> String {
        // Create a pattern that matches this specific package with its version
        let escaped_package = regex::escape(package);
        let escaped_version = regex::escape(old_version);

        // Match: "package": "version" with flexible whitespace
        let pattern = format!(r#""{}"\s*:\s*"{}""#, escaped_package, escaped_version);

        let re = Regex::new(&pattern).expect("Invalid pattern");

        // Replace with new version, preserving the pattern structure
        let replacement = format!(r#""{}": "{}""#, package, new_version);
        re.replace(content, replacement.as_str()).to_string()
    }
}

impl Default for PackageJsonUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for PackageJsonUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let json: Value = serde_json::from_str(&content)?;
        let mut result = UpdateResult::default();
        let mut new_content = content.clone();

        // First pass: collect all packages to check
        let mut packages_to_check: Vec<(String, String, String, String)> = Vec::new();

        for section in [
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
        ] {
            if let Some(deps) = json.get(section).and_then(|v| v.as_object()) {
                for (package, version_value) in deps {
                    if let Some(version_str) = version_value.as_str() {
                        // Skip non-version values (git urls, file paths, etc.)
                        if version_str.starts_with("git")
                            || version_str.starts_with("http")
                            || version_str.starts_with("file:")
                            || version_str.contains('/')
                            || version_str == "*"
                            || version_str == "latest"
                        {
                            continue;
                        }

                        let (prefix, current_version) = self.extract_version_info(version_str);

                        // Skip invalid versions
                        if semver::Version::parse(&current_version).is_err() {
                            continue;
                        }

                        packages_to_check.push((
                            package.clone(),
                            version_str.to_string(),
                            prefix,
                            current_version,
                        ));
                    }
                }
            }
        }

        // Fetch all versions in parallel
        let version_futures: Vec<_> = packages_to_check
            .iter()
            .map(|(package, _, _, _)| registry.get_latest_version(package))
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results
        for ((package, version_str, prefix, current_version), version_result) in
            packages_to_check.into_iter().zip(version_results)
        {
            match version_result {
                Ok(latest_version) => {
                    // Match the precision of the original version (unless full precision requested)
                    let matched_version = if options.full_precision {
                        latest_version.clone()
                    } else {
                        match_version_precision(&current_version, &latest_version)
                    };
                    if matched_version != current_version {
                        let line_num = self.find_package_line(&content, &package);
                        result.updated.push((
                            package.clone(),
                            current_version.clone(),
                            matched_version.clone(),
                            line_num,
                        ));

                        // Update in content preserving formatting
                        new_content = self.update_version_in_content(
                            &new_content,
                            &package,
                            &version_str,
                            &format!("{}{}", prefix, matched_version),
                        );
                    } else {
                        result.unchanged += 1;
                    }
                }
                Err(e) => {
                    result.errors.push(format!("{}: {}", package, e));
                }
            }
        }

        if !result.updated.is_empty() && !options.dry_run {
            write_file_atomic(path, &new_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::PackageJson
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let json: Value = serde_json::from_str(&content)?;
        let mut deps = Vec::new();

        for section in [
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
        ] {
            if let Some(section_deps) = json.get(section).and_then(|v| v.as_object()) {
                for (package, version_value) in section_deps {
                    if let Some(version_str) = version_value.as_str() {
                        // Skip non-version values (git urls, file paths, etc.)
                        if version_str.starts_with("git")
                            || version_str.starts_with("http")
                            || version_str.starts_with("file:")
                            || version_str.contains('/')
                            || version_str == "*"
                            || version_str == "latest"
                        {
                            continue;
                        }

                        let (_, current_version) = self.extract_version_info(version_str);

                        // Skip invalid versions
                        if semver::Version::parse(&current_version).is_err() {
                            continue;
                        }

                        let line_num = self.find_package_line(&content, package);
                        deps.push(ParsedDependency {
                            name: package.clone(),
                            version: current_version,
                            line_number: line_num,
                            has_upper_bound: false, // npm versions don't have explicit upper bounds like Python
                        });
                    }
                }
            }
        }

        Ok(deps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use std::fs;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_extract_version_info() {
        let updater = PackageJsonUpdater::new();

        assert_eq!(
            updater.extract_version_info("^1.0.0"),
            ("^".to_string(), "1.0.0".to_string())
        );

        assert_eq!(
            updater.extract_version_info("~2.0.0"),
            ("~".to_string(), "2.0.0".to_string())
        );

        assert_eq!(
            updater.extract_version_info(">=3.0.0"),
            (">=".to_string(), "3.0.0".to_string())
        );

        assert_eq!(
            updater.extract_version_info("1.0.0"),
            ("".to_string(), "1.0.0".to_string())
        );
    }

    #[tokio::test]
    async fn test_update_package_json_file() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "name": "test-project",
  "dependencies": {{
    "react": "^17.0.0",
    "lodash": "~4.17.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("react", "18.2.0")
            .with_version("lodash", "4.17.21");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);

        // Verify file was updated
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^18.2.0"));
        assert!(content.contains("~4.17.21"));
    }

    #[tokio::test]
    async fn test_update_package_json_dry_run() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        let original = r#"{
  "dependencies": {
    "express": "^4.17.0"
  }
}"#;
        write!(file, "{}", original).unwrap();

        let registry = MockRegistry::new("npm").with_version("express", "4.18.2");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions {
            dry_run: true,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // Verify file was NOT updated (dry run)
        let content = fs::read_to_string(file.path()).unwrap();
        assert_eq!(content, original);
    }

    #[tokio::test]
    async fn test_update_package_json_preserves_prefix() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "caret": "^1.0.0",
    "tilde": "~1.0.0",
    "exact": "1.0.0",
    "gte": ">=1.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("caret", "2.0.0")
            .with_version("tilde", "2.0.0")
            .with_version("exact", "2.0.0")
            .with_version("gte", "2.0.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 4);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"^2.0.0\""));
        assert!(content.contains("\"~2.0.0\""));
        assert!(content.contains("\"2.0.0\"")); // exact version
        assert!(content.contains("\">=2.0.0\""));
    }

    #[tokio::test]
    async fn test_update_package_json_dev_dependencies() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "devDependencies": {{
    "typescript": "^4.9.0",
    "jest": "^29.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("typescript", "5.3.3")
            .with_version("jest", "29.7.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^5.3.3"));
        assert!(content.contains("^29.7.0"));
    }

    #[tokio::test]
    async fn test_update_package_json_skips_special_versions() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "local-pkg": "file:../local",
    "git-pkg": "git+https://github.com/user/repo.git",
    "any-version": "*",
    "latest-version": "latest",
    "normal-pkg": "^1.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_version("normal-pkg", "2.0.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only normal-pkg should be updated
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "normal-pkg");
    }

    #[tokio::test]
    async fn test_update_package_json_line_numbers() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "name": "test",
  "dependencies": {{
    "react": "^17.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_version("react", "18.2.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions {
            dry_run: true,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        // Line number should be found (react is on line 4)
        assert!(result.updated[0].3.is_some());
        assert_eq!(result.updated[0].3, Some(4));
    }

    #[tokio::test]
    async fn test_update_package_json_registry_error() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "nonexistent-pkg": "^1.0.0"
  }}
}}"#
        )
        .unwrap();

        // Registry without the package
        let registry = MockRegistry::new("npm");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions {
            dry_run: true,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent-pkg"));
    }
}
