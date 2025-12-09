use super::{FileType, UpdateResult, Updater};
use crate::registry::Registry;
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use serde_json::Value;
use std::fs;
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
        dry_run: bool,
    ) -> Result<UpdateResult> {
        let content = fs::read_to_string(path)?;
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
                    if latest_version != current_version {
                        let line_num = self.find_package_line(&content, &package);
                        result.updated.push((
                            package.clone(),
                            current_version.clone(),
                            latest_version.clone(),
                            line_num,
                        ));

                        // Update in content preserving formatting
                        new_content = self.update_version_in_content(
                            &new_content,
                            &package,
                            &version_str,
                            &format!("{}{}", prefix, latest_version),
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

        if !result.updated.is_empty() && !dry_run {
            fs::write(path, new_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::PackageJson
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
