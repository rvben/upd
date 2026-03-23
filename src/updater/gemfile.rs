use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, read_file_safe,
    write_file_atomic,
};
use crate::registry::Registry;
use crate::version::match_version_precision;
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use std::path::Path;

pub struct GemfileUpdater {
    /// Matches: gem 'name', 'constraint version'
    /// Group 1: gem name
    /// Group 2: full version constraint string (e.g., "~> 7.1", ">= 4.9.0", "1.5.4")
    gem_re: Regex,
}

/// Parsed gem dependency
struct ParsedGem {
    name: String,
    /// The version constraint operator (e.g., "~>", ">=", ""), empty for exact versions
    operator: String,
    /// The version number (e.g., "7.1", "4.9.0", "1.5.4")
    version: String,
}

impl GemfileUpdater {
    pub fn new() -> Self {
        // Matches gem declarations with version constraints:
        //   gem 'rails', '~> 7.1'
        //   gem "devise", ">= 4.9.0"
        //   gem 'puma', '1.5.4'
        // Captures:
        //   1: gem name
        //   2: operator (optional: ~>, >=, <=, >, <, =, !=)
        //   3: version number
        let gem_re = Regex::new(
            r#"^\s*gem\s+['"]([^'"]+)['"]\s*,\s*['"](~>\s*|>=\s*|<=\s*|>\s*|<\s*|=\s*|!=\s*)?(\d[^'"]*?)['"]"#,
        )
        .expect("Invalid regex");

        Self { gem_re }
    }

    fn parse_line(&self, line: &str) -> Option<ParsedGem> {
        let trimmed = line.trim();

        // Skip comments
        if trimmed.starts_with('#') {
            return None;
        }

        // Skip gems with path: or git: options (local/git sources)
        if trimmed.contains("path:") || trimmed.contains("git:") {
            return None;
        }

        let caps = self.gem_re.captures(line)?;
        let name = caps.get(1)?.as_str().to_string();
        let operator = caps
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        let version = caps.get(3)?.as_str().trim().to_string();

        Some(ParsedGem {
            name,
            operator,
            version,
        })
    }

    fn update_line(&self, line: &str, old_version: &str, new_version: &str) -> String {
        // Replace only the version number, preserving the operator and quotes
        line.replacen(old_version, new_version, 1)
    }

    /// Check if the constraint has an upper bound that requires constraint-aware lookup
    fn has_upper_bound(operator: &str) -> bool {
        matches!(operator, "~>" | "<" | "<=" | "!=")
    }
}

impl Default for GemfileUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for GemfileUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut result = UpdateResult::default();

        let lines: Vec<&str> = content.lines().collect();
        let mut parsed_gems: Vec<(usize, &str, ParsedGem)> = Vec::new();

        for (line_idx, line) in lines.iter().enumerate() {
            if let Some(parsed) = self.parse_line(line) {
                parsed_gems.push((line_idx, line, parsed));
            }
        }

        // Separate into ignored, pinned, and to-be-fetched
        let mut ignored_packages: Vec<(usize, String, String)> = Vec::new();
        let mut pinned_packages: Vec<(usize, String, String, String)> = Vec::new();
        let mut fetch_deps: Vec<(usize, &str, &ParsedGem)> = Vec::new();

        for (line_idx, line, parsed) in &parsed_gems {
            if options.should_ignore(&parsed.name) {
                ignored_packages.push((*line_idx, parsed.name.clone(), parsed.version.clone()));
                continue;
            }

            if let Some(pinned_version) = options.get_pinned_version(&parsed.name) {
                pinned_packages.push((
                    *line_idx,
                    parsed.name.clone(),
                    parsed.version.clone(),
                    pinned_version.to_string(),
                ));
                continue;
            }

            fetch_deps.push((*line_idx, *line, parsed));
        }

        for (line_idx, package, version) in ignored_packages {
            result.ignored.push((package, version, Some(line_idx + 1)));
        }

        // Fetch versions concurrently
        let version_futures: Vec<_> = fetch_deps
            .iter()
            .map(|(_, _, parsed)| async {
                if Self::has_upper_bound(&parsed.operator) {
                    // Build Ruby constraint string for matching
                    let constraint = if parsed.operator.is_empty() {
                        format!("= {}", parsed.version)
                    } else {
                        format!("{} {}", parsed.operator, parsed.version)
                    };
                    registry
                        .get_latest_version_matching(&parsed.name, &constraint)
                        .await
                } else {
                    registry.get_latest_version(&parsed.name).await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        let mut version_map: std::collections::HashMap<usize, Result<String, anyhow::Error>> =
            std::collections::HashMap::new();
        for ((line_idx, _, _), version_result) in fetch_deps.iter().zip(version_results) {
            version_map.insert(*line_idx, version_result);
        }

        for (line_idx, package, current_version, pinned_version) in pinned_packages {
            version_map.insert(line_idx, Ok(pinned_version.clone()));
            result
                .pinned
                .push((package, current_version, pinned_version, Some(line_idx + 1)));
        }

        // Apply updates
        let mut new_lines = Vec::new();
        let mut modified = false;

        for (line_idx, line) in lines.iter().enumerate() {
            let line_num = line_idx + 1;

            if let Some(parsed) = self.parse_line(line) {
                if let Some(version_result) = version_map.remove(&line_idx) {
                    match version_result {
                        Ok(latest_version) => {
                            let matched_version = if options.full_precision {
                                latest_version.clone()
                            } else {
                                match_version_precision(&parsed.version, &latest_version)
                            };
                            if matched_version != parsed.version {
                                result.updated.push((
                                    parsed.name.clone(),
                                    parsed.version.clone(),
                                    matched_version.clone(),
                                    Some(line_num),
                                ));
                                new_lines.push(self.update_line(
                                    line,
                                    &parsed.version,
                                    &matched_version,
                                ));
                                modified = true;
                            } else {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            }
                        }
                        Err(e) => {
                            result.errors.push(format!("{}: {}", parsed.name, e));
                            new_lines.push(line.to_string());
                        }
                    }
                } else {
                    new_lines.push(line.to_string());
                }
            } else {
                new_lines.push(line.to_string());
            }
        }

        if modified && !options.dry_run {
            let line_ending = if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };

            let mut new_content = new_lines.join(line_ending);

            if content.ends_with('\n') || content.ends_with("\r\n") {
                new_content.push_str(line_ending);
            }

            write_file_atomic(path, &new_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::Gemfile
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let mut deps = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            if let Some(parsed) = self.parse_line(line) {
                deps.push(ParsedDependency {
                    name: parsed.name,
                    version: parsed.version,
                    line_number: Some(line_idx + 1),
                    has_upper_bound: Self::has_upper_bound(&parsed.operator),
                });
            }
        }

        Ok(deps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_gem_line() {
        let updater = GemfileUpdater::new();

        let parsed = updater.parse_line("gem 'rails', '~> 7.1'").unwrap();
        assert_eq!(parsed.name, "rails");
        assert_eq!(parsed.operator, "~>");
        assert_eq!(parsed.version, "7.1");

        let parsed = updater.parse_line("gem \"devise\", \">= 4.9.0\"").unwrap();
        assert_eq!(parsed.name, "devise");
        assert_eq!(parsed.operator, ">=");
        assert_eq!(parsed.version, "4.9.0");

        let parsed = updater.parse_line("gem 'pg', '1.5.4'").unwrap();
        assert_eq!(parsed.name, "pg");
        assert_eq!(parsed.operator, "");
        assert_eq!(parsed.version, "1.5.4");
    }

    #[test]
    fn test_skips_comments_and_no_version() {
        let updater = GemfileUpdater::new();

        assert!(updater.parse_line("# gem 'rails', '~> 7.1'").is_none());
        assert!(updater.parse_line("gem 'sidekiq'").is_none());
        assert!(updater.parse_line("").is_none());
        assert!(
            updater
                .parse_line("  # This is a comment about gems")
                .is_none()
        );
    }

    #[test]
    fn test_skips_path_and_git_gems() {
        let updater = GemfileUpdater::new();

        assert!(
            updater
                .parse_line("gem 'my-gem', path: '../my-gem'")
                .is_none()
        );
        assert!(
            updater
                .parse_line("gem 'my-gem', git: 'https://github.com/user/repo'")
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_update_gemfile() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "source 'https://rubygems.org'").unwrap();
        writeln!(file).unwrap();
        writeln!(file, "gem 'rails', '~> 7.1'").unwrap();
        writeln!(file, "gem 'pg', '1.5.4'").unwrap();
        writeln!(file, "# A comment").unwrap();
        writeln!(file, "gem 'sidekiq'").unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_constrained("rails", "~> 7.1", "7.2.1")
            .with_version("pg", "1.6.0");

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("gem 'rails', '~> 7.2'"));
        assert!(contents.contains("gem 'pg', '1.6.0'"));
        assert!(contents.contains("# A comment"));
        assert!(contents.contains("source 'https://rubygems.org'"));
    }

    #[test]
    fn test_version_precision() {
        let updater = GemfileUpdater::new();

        // ~> 7.1 with latest 7.2.3 should preserve 2-part precision
        let result = updater.update_line("gem 'rails', '~> 7.1'", "7.1", "7.2");
        assert_eq!(result, "gem 'rails', '~> 7.2'");
    }

    #[tokio::test]
    async fn test_dry_run() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "gem 'rails', '~> 7.1'").unwrap();

        let registry = MockRegistry::new("rubygems").with_constrained("rails", "~> 7.1", "7.2.1");

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified in dry-run mode
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("~> 7.1"));
    }

    #[test]
    fn test_preserves_constraint_operator() {
        let updater = GemfileUpdater::new();

        let result = updater.update_line("gem 'devise', '>= 4.9.0'", "4.9.0", "4.10.0");
        assert_eq!(result, "gem 'devise', '>= 4.10.0'");

        let result = updater.update_line("gem 'puma', '~> 6.0'", "6.0", "6.4");
        assert_eq!(result, "gem 'puma', '~> 6.4'");
    }

    #[test]
    fn test_parse_gem_with_group_option() {
        let updater = GemfileUpdater::new();

        // Gems with group options after version should still be parsed
        let parsed = updater
            .parse_line("gem 'rspec', '~> 3.12', group: :test")
            .unwrap();
        assert_eq!(parsed.name, "rspec");
        assert_eq!(parsed.operator, "~>");
        assert_eq!(parsed.version, "3.12");
    }

    #[test]
    fn test_parse_gem_with_indentation() {
        let updater = GemfileUpdater::new();

        let parsed = updater.parse_line("  gem 'rails', '~> 7.1'").unwrap();
        assert_eq!(parsed.name, "rails");
        assert_eq!(parsed.version, "7.1");
    }
}
