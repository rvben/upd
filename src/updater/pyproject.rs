use super::{FileType, UpdateResult, Updater};
use crate::registry::Registry;
use crate::version::is_stable_pep440;
use anyhow::{Result, anyhow};
use regex::Regex;
use std::fs;
use std::path::Path;
use toml_edit::{DocumentMut, Formatted, Item, Value};

pub struct PyProjectUpdater {
    // Regex to extract version from dependency string
    // Matches: package==1.0.0, package>=1.0.0, package[extra]>=1.0.0, etc.
    version_re: Regex,
    // Regex to capture the full constraint including additional constraints after commas
    // E.g., ">=2.8.0,<9" or ">=1.0.0,!=1.5.0,<2.0.0"
    constraint_re: Regex,
}

impl PyProjectUpdater {
    pub fn new() -> Self {
        let version_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)(\[[^\]]+\])?\s*(==|>=|<=|~=|!=|>|<)\s*([^\s,;]+)",
        )
        .expect("Invalid regex");

        // Match the full constraint including additional constraints after commas
        // E.g., ">=2.8.0,<9" or ">=1.0.0,!=1.5.0,<2.0.0"
        let constraint_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)(\[[^\]]+\])?\s*((?:==|>=|<=|~=|!=|>|<)[^\s;]+(?:\s*,\s*(?:==|>=|<=|~=|!=|>|<)[^\s;,]+)*)",
        )
        .expect("Invalid regex");

        Self {
            version_re,
            constraint_re,
        }
    }

    /// Parse dependency string and return (package, first_version, full_constraint)
    fn parse_dependency(&self, dep: &str) -> Option<(String, String, String)> {
        // First get the full constraint
        let full_constraint = self
            .constraint_re
            .captures(dep)
            .and_then(|c| c.get(3))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();

        self.version_re.captures(dep).map(|caps| {
            let package = caps.get(1).unwrap().as_str().to_string();
            let version = caps.get(4).unwrap().as_str().to_string();
            (package, version, full_constraint)
        })
    }

    /// Check if constraint is simple (no upper bounds that could be violated)
    fn is_simple_constraint(constraint: &str) -> bool {
        // If there are multiple constraints (comma-separated), need constraint-aware lookup
        if constraint.contains(',') {
            return false;
        }

        // If the constraint has an upper-bound operator, need constraint-aware lookup
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

    fn update_dependency(&self, dep: &str, new_version: &str) -> String {
        if let Some(caps) = self.version_re.captures(dep) {
            let full_match = caps.get(0).unwrap();
            let package = caps.get(1).unwrap().as_str();
            let extras = caps.get(2).map_or("", |m| m.as_str());
            let operator = caps.get(3).unwrap().as_str();

            let new_spec = format!("{}{}{}{}", package, extras, operator, new_version);

            let mut result = dep.to_string();
            result.replace_range(full_match.range(), &new_spec);
            result
        } else {
            dep.to_string()
        }
    }

    async fn update_array_deps(
        &self,
        array: &mut toml_edit::Array,
        registry: &dyn Registry,
        result: &mut UpdateResult,
    ) {
        // Collect all updates first, then apply them
        let mut updates: Vec<(usize, String)> = Vec::new();

        for i in 0..array.len() {
            if let Some(item) = array.get(i)
                && let Some(s) = item.as_str()
                && let Some((package, current_version, full_constraint)) = self.parse_dependency(s)
            {
                // Determine which lookup method to use based on constraint complexity
                let version_result = if !is_stable_pep440(&current_version) {
                    // Pre-release: include pre-releases in lookup
                    registry
                        .get_latest_version_including_prereleases(&package)
                        .await
                } else if Self::is_simple_constraint(&full_constraint) {
                    // Simple constraint: just get latest stable
                    registry.get_latest_version(&package).await
                } else {
                    // Complex constraint with upper bounds: use constraint-aware lookup
                    registry
                        .get_latest_version_matching(&package, &full_constraint)
                        .await
                };

                match version_result {
                    Ok(latest_version) => {
                        if latest_version != current_version {
                            let updated = self.update_dependency(s, &latest_version);
                            result
                                .updated
                                .push((package, current_version, latest_version));
                            updates.push((i, updated));
                        } else {
                            result.unchanged += 1;
                        }
                    }
                    Err(e) => {
                        result.errors.push(format!("{}: {}", package, e));
                    }
                }
            }
        }

        // Apply updates, preserving decoration (comments, whitespace)
        for (i, updated) in updates {
            if let Some(item) = array.get_mut(i) {
                // Preserve the original decoration (prefix/suffix whitespace and comments)
                if let Value::String(formatted) = item {
                    let decor = formatted.decor().clone();
                    let mut new_formatted = Formatted::new(updated);
                    *new_formatted.decor_mut() = decor;
                    *formatted = new_formatted;
                } else {
                    *item = Value::from(updated);
                }
            }
        }
    }

    async fn update_poetry_deps(
        &self,
        deps_table: &mut toml_edit::Table,
        registry: &dyn Registry,
        result: &mut UpdateResult,
    ) {
        // Collect keys first to avoid borrow issues
        let keys: Vec<String> = deps_table.iter().map(|(k, _)| k.to_string()).collect();

        for key in keys {
            if key == "python" {
                continue; // Skip python version constraint
            }

            if let Some(Item::Value(Value::String(s))) = deps_table.get(&key) {
                let version_str = s.value().to_string();

                // Poetry uses ^ and ~ prefixes
                let (prefix, version) =
                    if version_str.starts_with('^') || version_str.starts_with('~') {
                        (&version_str[..1], version_str[1..].to_string())
                    } else {
                        ("", version_str.clone())
                    };

                // If current version is a pre-release, include pre-releases in lookup
                let version_result = if is_stable_pep440(&version) {
                    registry.get_latest_version(&key).await
                } else {
                    registry
                        .get_latest_version_including_prereleases(&key)
                        .await
                };

                match version_result {
                    Ok(latest_version) => {
                        if latest_version != version {
                            let new_val = format!("{}{}", prefix, latest_version);
                            result.updated.push((key.clone(), version, latest_version));

                            // Preserve decoration when updating
                            if let Some(Item::Value(Value::String(formatted))) =
                                deps_table.get_mut(&key)
                            {
                                let decor = formatted.decor().clone();
                                let mut new_formatted = Formatted::new(new_val);
                                *new_formatted.decor_mut() = decor;
                                *formatted = new_formatted;
                            }
                        } else {
                            result.unchanged += 1;
                        }
                    }
                    Err(e) => {
                        result.errors.push(format!("{}: {}", key, e));
                    }
                }
            }
        }
    }
}

impl Default for PyProjectUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for PyProjectUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        dry_run: bool,
    ) -> Result<UpdateResult> {
        let content = fs::read_to_string(path)?;
        let mut doc: DocumentMut = content
            .parse()
            .map_err(|e| anyhow!("Failed to parse TOML: {}", e))?;

        let mut result = UpdateResult::default();

        // Update [project.dependencies]
        if let Some(Item::Table(project)) = doc.get_mut("project") {
            if let Some(Item::Value(Value::Array(deps))) = project.get_mut("dependencies") {
                self.update_array_deps(deps, registry, &mut result).await;
            }

            // Update [project.optional-dependencies.*]
            if let Some(Item::Table(opt_deps)) = project.get_mut("optional-dependencies") {
                // Collect keys first
                let keys: Vec<String> = opt_deps.iter().map(|(k, _)| k.to_string()).collect();
                for key in keys {
                    if let Some(Item::Value(Value::Array(deps))) = opt_deps.get_mut(&key) {
                        self.update_array_deps(deps, registry, &mut result).await;
                    }
                }
            }
        }

        // Update [dependency-groups.*]
        if let Some(Item::Table(groups)) = doc.get_mut("dependency-groups") {
            let keys: Vec<String> = groups.iter().map(|(k, _)| k.to_string()).collect();
            for key in keys {
                if let Some(Item::Value(Value::Array(deps))) = groups.get_mut(&key) {
                    self.update_array_deps(deps, registry, &mut result).await;
                }
            }
        }

        // Update [tool.poetry.dependencies] and [tool.poetry.dev-dependencies]
        if let Some(Item::Table(tool)) = doc.get_mut("tool")
            && let Some(Item::Table(poetry)) = tool.get_mut("poetry")
        {
            if let Some(Item::Table(deps)) = poetry.get_mut("dependencies") {
                self.update_poetry_deps(deps, registry, &mut result).await;
            }

            if let Some(Item::Table(deps)) = poetry.get_mut("dev-dependencies") {
                self.update_poetry_deps(deps, registry, &mut result).await;
            }
        }

        if !result.updated.is_empty() && !dry_run {
            fs::write(path, doc.to_string())?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::PyProject
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dependency() {
        let updater = PyProjectUpdater::new();

        let (pkg, ver, constraint) = updater.parse_dependency("requests>=2.28.0").unwrap();
        assert_eq!(pkg, "requests");
        assert_eq!(ver, "2.28.0");
        assert_eq!(constraint, ">=2.28.0");

        let (pkg, ver, constraint) = updater
            .parse_dependency("uvicorn[standard]>=0.20.0")
            .unwrap();
        assert_eq!(pkg, "uvicorn");
        assert_eq!(ver, "0.20.0");
        assert_eq!(constraint, ">=0.20.0");

        // Test constraint with upper bound
        let (pkg, ver, constraint) = updater.parse_dependency("flask>=2.0.0,<3.0.0").unwrap();
        assert_eq!(pkg, "flask");
        assert_eq!(ver, "2.0.0");
        assert_eq!(constraint, ">=2.0.0,<3.0.0");
    }

    #[test]
    fn test_is_simple_constraint() {
        // Simple constraints - no upper bound, no exclusions
        assert!(PyProjectUpdater::is_simple_constraint("==1.0.0"));
        assert!(PyProjectUpdater::is_simple_constraint(">=1.0.0"));
        assert!(PyProjectUpdater::is_simple_constraint(">1.0.0"));

        // Multiple constraints with comma
        assert!(!PyProjectUpdater::is_simple_constraint(">=1.0.0,<2.0.0"));
        assert!(!PyProjectUpdater::is_simple_constraint(">=2.8.0,<9"));

        // Upper-bound operators (need constraint-aware lookup)
        assert!(!PyProjectUpdater::is_simple_constraint("<4.2"));
        assert!(!PyProjectUpdater::is_simple_constraint("<=2.0"));
        assert!(!PyProjectUpdater::is_simple_constraint("~=1.4"));

        // Exclusion operator
        assert!(!PyProjectUpdater::is_simple_constraint("!=1.5.0"));
    }

    #[test]
    fn test_update_dependency() {
        let updater = PyProjectUpdater::new();

        assert_eq!(
            updater.update_dependency("requests>=2.28.0", "2.31.0"),
            "requests>=2.31.0"
        );

        assert_eq!(
            updater.update_dependency("uvicorn[standard]>=0.20.0", "0.24.0"),
            "uvicorn[standard]>=0.24.0"
        );
    }
}
