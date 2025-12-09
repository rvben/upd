use super::{FileType, UpdateOptions, UpdateResult, Updater};
use crate::registry::Registry;
use crate::version::match_version_precision;
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use std::fs;
use std::path::Path;

pub struct RequirementsUpdater {
    // Regex to match package specifications
    // Matches: package==1.0.0, package>=1.0.0, package[extra]==1.0.0, etc.
    package_re: Regex,
    // Regex to capture the full version constraint including upper bounds
    // Matches: >=1.0.0,<2 or ==1.0.0 or >=1.0,<2,!=1.5
    constraint_re: Regex,
}

/// Parsed dependency information
struct ParsedDep {
    package: String,
    /// Extras like [standard] - currently stored for future use in constraint reconstruction
    #[cfg_attr(not(test), allow(dead_code))]
    extras: String,
    /// The first version number found (for display purposes)
    first_version: String,
    /// The full constraint string (e.g., ">=2.8.0,<9")
    full_constraint: String,
}

impl RequirementsUpdater {
    pub fn new() -> Self {
        // Match package name (with optional extras), operator, and version
        // Captures: 1=package_name, 2=extras (optional), 3=operator, 4=version
        let package_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)(\[[^\]]+\])?\s*(==|>=|<=|~=|!=|>|<)\s*([^\s,;#]+)",
        )
        .expect("Invalid regex");

        // Match the full constraint including additional constraints after commas
        // E.g., ">=2.8.0,<9" or ">=1.0.0,!=1.5.0,<2.0.0"
        let constraint_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)(\[[^\]]+\])?\s*((?:==|>=|<=|~=|!=|>|<)[^\s#;]+(?:\s*,\s*(?:==|>=|<=|~=|!=|>|<)[^\s#;,]+)*)",
        )
        .expect("Invalid regex");

        Self {
            package_re,
            constraint_re,
        }
    }

    fn parse_line(&self, line: &str) -> Option<ParsedDep> {
        // Skip comments and empty lines
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            return None;
        }

        // Handle inline comments
        let code_part = line.split('#').next().unwrap_or(line);

        // Try to capture the full constraint first
        if let Some(caps) = self.constraint_re.captures(code_part) {
            let package = caps.get(1).unwrap().as_str().to_string();
            let extras = caps.get(2).map_or("", |m| m.as_str()).to_string();
            let full_constraint = caps.get(3).unwrap().as_str().to_string();

            // Extract the first version for display
            let first_version = self
                .package_re
                .captures(code_part)
                .and_then(|c| c.get(4))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();

            return Some(ParsedDep {
                package,
                extras,
                first_version,
                full_constraint,
            });
        }

        None
    }

    /// Check if constraint is a simple single-version constraint that doesn't need
    /// constraint-aware lookup (i.e., no upper bounds that could be violated)
    fn is_simple_constraint(constraint: &str) -> bool {
        // If there are multiple constraints (comma-separated), need constraint-aware lookup
        if constraint.contains(',') {
            return false;
        }

        // If the constraint has an upper-bound operator, need constraint-aware lookup
        // Examples: "<4.2", "<=2.0", "~=1.4" (compatible release - allows only patch updates)
        if constraint.starts_with('<')
            || constraint.starts_with("<=")
            || constraint.starts_with("~=")
        {
            return false;
        }

        // Also check for != which could affect version selection
        if constraint.starts_with("!=") {
            return false;
        }

        // Simple constraints like "==1.0.0", ">=1.0.0", ">1.0.0" are fine
        true
    }

    fn update_line(&self, line: &str, new_version: &str) -> String {
        if let Some(caps) = self.package_re.captures(line) {
            let full_match = caps.get(0).unwrap();
            let package = caps.get(1).unwrap().as_str();
            let extras = caps.get(2).map_or("", |m| m.as_str());
            let operator = caps.get(3).unwrap().as_str();

            // Reconstruct the package spec with new version
            let new_spec = format!("{}{}{}{}", package, extras, operator, new_version);

            // Replace in original line to preserve trailing comments and whitespace
            let mut result = line.to_string();
            result.replace_range(full_match.range(), &new_spec);
            result
        } else {
            line.to_string()
        }
    }
}

impl Default for RequirementsUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for RequirementsUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = fs::read_to_string(path)?;
        let mut result = UpdateResult::default();

        // First pass: collect all packages that need version checks
        let lines: Vec<&str> = content.lines().collect();
        let mut parsed_deps: Vec<(usize, &str, ParsedDep)> = Vec::new();

        for (line_idx, line) in lines.iter().enumerate() {
            if let Some(parsed) = self.parse_line(line) {
                parsed_deps.push((line_idx, line, parsed));
            }
        }

        // Fetch all versions in parallel
        let version_futures: Vec<_> = parsed_deps
            .iter()
            .map(|(_, _, parsed)| async {
                if Self::is_simple_constraint(&parsed.full_constraint) {
                    registry.get_latest_version(&parsed.package).await
                } else {
                    registry
                        .get_latest_version_matching(&parsed.package, &parsed.full_constraint)
                        .await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Build a map of line index to version result
        let mut version_map: std::collections::HashMap<usize, Result<String, anyhow::Error>> =
            std::collections::HashMap::new();
        for ((line_idx, _, _), version_result) in parsed_deps.iter().zip(version_results) {
            version_map.insert(*line_idx, version_result);
        }

        // Second pass: apply updates
        let mut new_lines = Vec::new();
        let mut modified = false;

        for (line_idx, line) in lines.iter().enumerate() {
            let line_num = line_idx + 1; // 1-indexed for display

            if let Some(parsed) = self.parse_line(line) {
                if let Some(version_result) = version_map.remove(&line_idx) {
                    match version_result {
                        Ok(latest_version) => {
                            // Match the precision of the original version (unless full precision requested)
                            let matched_version = if options.full_precision {
                                latest_version.clone()
                            } else {
                                match_version_precision(&parsed.first_version, &latest_version)
                            };
                            if matched_version != parsed.first_version {
                                result.updated.push((
                                    parsed.package.clone(),
                                    parsed.first_version.clone(),
                                    matched_version.clone(),
                                    Some(line_num),
                                ));
                                new_lines.push(self.update_line(line, &matched_version));
                                modified = true;
                            } else {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            }
                        }
                        Err(e) => {
                            result.errors.push(format!("{}: {}", parsed.package, e));
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
            // Preserve original line ending style
            let line_ending = if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };

            let mut new_content = new_lines.join(line_ending);

            // Preserve trailing newline if present
            if content.ends_with('\n') || content.ends_with("\r\n") {
                new_content.push_str(line_ending);
            }

            fs::write(path, new_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::Requirements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_line() {
        let updater = RequirementsUpdater::new();

        let parsed = updater.parse_line("requests==2.28.0").unwrap();
        assert_eq!(parsed.package, "requests");
        assert_eq!(parsed.extras, "");
        assert_eq!(parsed.first_version, "2.28.0");
        assert_eq!(parsed.full_constraint, "==2.28.0");

        let parsed = updater.parse_line("uvicorn[standard]==0.20.0").unwrap();
        assert_eq!(parsed.package, "uvicorn");
        assert_eq!(parsed.extras, "[standard]");
        assert_eq!(parsed.first_version, "0.20.0");

        let parsed = updater.parse_line("django>=4.0.0").unwrap();
        assert_eq!(parsed.package, "django");
        assert_eq!(parsed.first_version, "4.0.0");
        assert_eq!(parsed.full_constraint, ">=4.0.0");

        // Test constraint with upper bound
        let parsed = updater.parse_line("pytest>=2.8.0,<9").unwrap();
        assert_eq!(parsed.package, "pytest");
        assert_eq!(parsed.first_version, "2.8.0");
        assert_eq!(parsed.full_constraint, ">=2.8.0,<9");

        // Test multiple constraints
        let parsed = updater.parse_line("foo>=1.0.0,!=1.5.0,<2.0.0").unwrap();
        assert_eq!(parsed.package, "foo");
        assert_eq!(parsed.first_version, "1.0.0");
        assert_eq!(parsed.full_constraint, ">=1.0.0,!=1.5.0,<2.0.0");

        assert!(updater.parse_line("# comment").is_none());
        assert!(updater.parse_line("").is_none());
        assert!(updater.parse_line("-r other.txt").is_none());
    }

    #[test]
    fn test_is_simple_constraint() {
        // Simple constraints - no upper bound, no exclusions
        assert!(RequirementsUpdater::is_simple_constraint("==1.0.0"));
        assert!(RequirementsUpdater::is_simple_constraint(">=1.0.0"));
        assert!(RequirementsUpdater::is_simple_constraint(">1.0.0"));

        // Multiple constraints with comma
        assert!(!RequirementsUpdater::is_simple_constraint(">=1.0.0,<2.0.0"));
        assert!(!RequirementsUpdater::is_simple_constraint(">=2.8.0,<9"));

        // Upper-bound operators (need constraint-aware lookup)
        assert!(!RequirementsUpdater::is_simple_constraint("<4.2"));
        assert!(!RequirementsUpdater::is_simple_constraint("<=2.0"));
        assert!(!RequirementsUpdater::is_simple_constraint("~=1.4"));

        // Exclusion operator
        assert!(!RequirementsUpdater::is_simple_constraint("!=1.5.0"));
    }

    #[test]
    fn test_update_line() {
        let updater = RequirementsUpdater::new();

        assert_eq!(
            updater.update_line("requests==2.28.0", "2.31.0"),
            "requests==2.31.0"
        );

        assert_eq!(
            updater.update_line("requests==2.28.0  # HTTP library", "2.31.0"),
            "requests==2.31.0  # HTTP library"
        );

        assert_eq!(
            updater.update_line("uvicorn[standard]==0.20.0", "0.24.0"),
            "uvicorn[standard]==0.24.0"
        );
    }
}
