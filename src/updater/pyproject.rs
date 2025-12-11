use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, read_file_safe,
    write_file_atomic,
};
use crate::registry::Registry;
use crate::version::{is_stable_pep440, match_version_precision};
use anyhow::{Result, anyhow};
use futures::future::join_all;
use regex::Regex;
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

    /// Find the line number where a dependency string appears
    fn find_dependency_line(content: &str, dep_substring: &str) -> Option<usize> {
        for (line_idx, line) in content.lines().enumerate() {
            if line.contains(dep_substring) {
                return Some(line_idx + 1); // 1-indexed
            }
        }
        None
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
        original_content: &str,
        full_precision: bool,
    ) {
        // First pass: collect all dependencies that need version checks
        let mut deps_to_check: Vec<(usize, String, String, String, String)> = Vec::new();

        for i in 0..array.len() {
            if let Some(item) = array.get(i)
                && let Some(s) = item.as_str()
                && let Some((package, current_version, full_constraint)) = self.parse_dependency(s)
            {
                deps_to_check.push((i, s.to_string(), package, current_version, full_constraint));
            }
        }

        // Fetch all versions in parallel
        let version_futures: Vec<_> = deps_to_check
            .iter()
            .map(|(_, _, package, current_version, full_constraint)| async {
                if !is_stable_pep440(current_version) {
                    registry
                        .get_latest_version_including_prereleases(package)
                        .await
                } else if Self::is_simple_constraint(full_constraint) {
                    registry.get_latest_version(package).await
                } else {
                    registry
                        .get_latest_version_matching(package, full_constraint)
                        .await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results and collect updates
        let mut updates: Vec<(usize, String)> = Vec::new();

        for ((i, dep_str, package, current_version, _), version_result) in
            deps_to_check.into_iter().zip(version_results)
        {
            match version_result {
                Ok(latest_version) => {
                    // Match the precision of the original version (unless full precision requested)
                    let matched_version = if full_precision {
                        latest_version.clone()
                    } else {
                        match_version_precision(&current_version, &latest_version)
                    };
                    if matched_version != current_version {
                        let updated = self.update_dependency(&dep_str, &matched_version);
                        let line_num = Self::find_dependency_line(original_content, &package);
                        result
                            .updated
                            .push((package, current_version, matched_version, line_num));
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
        original_content: &str,
        full_precision: bool,
    ) {
        // First pass: collect dependencies to check
        let mut deps_to_check: Vec<(String, String, String)> = Vec::new();

        for (key, item) in deps_table.iter() {
            if key == "python" {
                continue;
            }

            if let Item::Value(Value::String(s)) = item {
                let version_str = s.value().to_string();
                let (prefix, version) =
                    if version_str.starts_with('^') || version_str.starts_with('~') {
                        (version_str[..1].to_string(), version_str[1..].to_string())
                    } else {
                        (String::new(), version_str.clone())
                    };

                deps_to_check.push((key.to_string(), prefix, version));
            }
        }

        // Fetch all versions in parallel
        let version_futures: Vec<_> = deps_to_check
            .iter()
            .map(|(key, _, version)| async {
                if is_stable_pep440(version) {
                    registry.get_latest_version(key).await
                } else {
                    registry.get_latest_version_including_prereleases(key).await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results
        for ((key, prefix, version), version_result) in
            deps_to_check.into_iter().zip(version_results)
        {
            match version_result {
                Ok(latest_version) => {
                    // Match the precision of the original version (unless full precision requested)
                    let matched_version = if full_precision {
                        latest_version.clone()
                    } else {
                        match_version_precision(&version, &latest_version)
                    };
                    if matched_version != version {
                        let new_val = format!("{}{}", prefix, matched_version);
                        let line_num = Self::find_dependency_line(original_content, &key);
                        result
                            .updated
                            .push((key.clone(), version, matched_version, line_num));

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
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut doc: DocumentMut = content
            .parse()
            .map_err(|e| anyhow!("Failed to parse TOML: {}", e))?;

        let mut result = UpdateResult::default();

        // Update [project.dependencies]
        if let Some(Item::Table(project)) = doc.get_mut("project") {
            if let Some(Item::Value(Value::Array(deps))) = project.get_mut("dependencies") {
                self.update_array_deps(
                    deps,
                    registry,
                    &mut result,
                    &content,
                    options.full_precision,
                )
                .await;
            }

            // Update [project.optional-dependencies.*]
            if let Some(Item::Table(opt_deps)) = project.get_mut("optional-dependencies") {
                // Collect keys first
                let keys: Vec<String> = opt_deps.iter().map(|(k, _)| k.to_string()).collect();
                for key in keys {
                    if let Some(Item::Value(Value::Array(deps))) = opt_deps.get_mut(&key) {
                        self.update_array_deps(
                            deps,
                            registry,
                            &mut result,
                            &content,
                            options.full_precision,
                        )
                        .await;
                    }
                }
            }
        }

        // Update [dependency-groups.*]
        if let Some(Item::Table(groups)) = doc.get_mut("dependency-groups") {
            let keys: Vec<String> = groups.iter().map(|(k, _)| k.to_string()).collect();
            for key in keys {
                if let Some(Item::Value(Value::Array(deps))) = groups.get_mut(&key) {
                    self.update_array_deps(
                        deps,
                        registry,
                        &mut result,
                        &content,
                        options.full_precision,
                    )
                    .await;
                }
            }
        }

        // Update [tool.poetry.dependencies] and [tool.poetry.dev-dependencies]
        if let Some(Item::Table(tool)) = doc.get_mut("tool")
            && let Some(Item::Table(poetry)) = tool.get_mut("poetry")
        {
            if let Some(Item::Table(deps)) = poetry.get_mut("dependencies") {
                self.update_poetry_deps(
                    deps,
                    registry,
                    &mut result,
                    &content,
                    options.full_precision,
                )
                .await;
            }

            if let Some(Item::Table(deps)) = poetry.get_mut("dev-dependencies") {
                self.update_poetry_deps(
                    deps,
                    registry,
                    &mut result,
                    &content,
                    options.full_precision,
                )
                .await;
            }
        }

        if !result.updated.is_empty() && !options.dry_run {
            write_file_atomic(path, &doc.to_string())?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::PyProject
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let doc: DocumentMut = content
            .parse()
            .map_err(|e| anyhow!("Failed to parse TOML: {}", e))?;

        let mut deps = Vec::new();

        // Parse [project.dependencies]
        if let Some(Item::Table(project)) = doc.get("project") {
            if let Some(Item::Value(Value::Array(arr))) = project.get("dependencies") {
                for item in arr.iter() {
                    if let Some(s) = item.as_str()
                        && let Some((name, version, constraint)) = self.parse_dependency(s)
                    {
                        let has_upper_bound = !Self::is_simple_constraint(&constraint);
                        let line_num = Self::find_dependency_line(&content, &name);
                        deps.push(ParsedDependency {
                            name,
                            version,
                            line_number: line_num,
                            has_upper_bound,
                        });
                    }
                }
            }

            // Parse [project.optional-dependencies.*]
            if let Some(Item::Table(opt_deps)) = project.get("optional-dependencies") {
                for (_, group_deps) in opt_deps.iter() {
                    if let Some(arr) = group_deps.as_array() {
                        for item in arr.iter() {
                            if let Some(s) = item.as_str()
                                && let Some((name, version, constraint)) = self.parse_dependency(s)
                            {
                                let has_upper_bound = !Self::is_simple_constraint(&constraint);
                                let line_num = Self::find_dependency_line(&content, &name);
                                deps.push(ParsedDependency {
                                    name,
                                    version,
                                    line_number: line_num,
                                    has_upper_bound,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Parse [tool.poetry.dependencies] and [tool.poetry.dev-dependencies]
        if let Some(Item::Table(tool)) = doc.get("tool")
            && let Some(Item::Table(poetry)) = tool.get("poetry")
        {
            for section in ["dependencies", "dev-dependencies"] {
                if let Some(Item::Table(section_deps)) = poetry.get(section) {
                    for (key, item) in section_deps.iter() {
                        if key == "python" {
                            continue;
                        }
                        if let Item::Value(Value::String(s)) = item {
                            let version_str = s.value().to_string();
                            let version =
                                if version_str.starts_with('^') || version_str.starts_with('~') {
                                    version_str[1..].to_string()
                                } else {
                                    version_str
                                };
                            let line_num = Self::find_dependency_line(&content, key);
                            deps.push(ParsedDependency {
                                name: key.to_string(),
                                version,
                                line_number: line_num,
                                has_upper_bound: false,
                            });
                        }
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
    use std::io::Write;
    use tempfile::NamedTempFile;

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

    // Integration tests using MockRegistry and temp files

    #[tokio::test]
    async fn test_update_pyproject_pep621() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
version = "1.0.0"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=3.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_poetry() {
        // Poetry uses table format: key = "version"
        // The version can be ^2.28.0, >=2.0.0, 2.0.0, etc.
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.9"
requests = "2.28.0"
flask = "2.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Poetry table-style dependencies with bare versions should be updated
        assert_eq!(result.updated.len(), 2);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("[tool.poetry.dependencies]"));
        // Both should be updated to new versions
        assert!(contents.contains("requests = \"2.31.0\""));
        assert!(contents.contains("flask = \"3.0.0\""));
    }

    #[tokio::test]
    async fn test_update_pyproject_dry_run() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.28.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: true,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified in dry-run mode
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.28.0"));
        assert!(!contents.contains("2.31.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_preserves_formatting() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"  # Project name
version = "1.0.0"

# Main dependencies
dependencies = [
    "requests>=2.28.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let contents = std::fs::read_to_string(file.path()).unwrap();
        // toml_edit should preserve comments
        assert!(contents.contains("# Project name") || contents.contains("# Main dependencies"));
    }

    #[tokio::test]
    async fn test_update_pyproject_optional_dependencies() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.28.0"]

[project.optional-dependencies]
dev = ["pytest>=7.0.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("pytest", "8.0.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("pytest>=8.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_unchanged_packages() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.31.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 1);
    }

    // Error path tests

    #[tokio::test]
    async fn test_update_pyproject_invalid_toml() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project
name = "invalid toml - missing bracket"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater.update(file.path(), &registry, options).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to parse TOML"));
    }

    #[tokio::test]
    async fn test_update_pyproject_file_not_found() {
        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(
                Path::new("/nonexistent/path/pyproject.toml"),
                &registry,
                options,
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update_pyproject_registry_error_for_package() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "nonexistent-pkg>=1.0.0",
]
"#
        )
        .unwrap();

        // Registry only has requests - nonexistent-pkg will cause an error
        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // One package updated successfully
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "requests");

        // One error for the nonexistent package
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent-pkg"));
    }

    #[tokio::test]
    async fn test_update_pyproject_empty_dependencies() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = []
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn test_update_pyproject_no_dependencies_section() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
version = "1.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
    }
}
