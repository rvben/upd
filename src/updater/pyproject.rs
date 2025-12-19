use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, read_file_safe,
    write_file_atomic,
};
use crate::registry::{MultiPyPiRegistry, PyPiRegistry, Registry};
use crate::version::{is_stable_pep440, match_version_precision};
use anyhow::{Result, anyhow};
use futures::future::join_all;
use regex::Regex;
use std::path::Path;
use std::sync::Arc;
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

    /// Check if constraint is an upper-bound-only constraint (e.g., "<6", "<=5.0")
    /// These should never be "updated" because they define a ceiling, not a floor.
    /// Updating them would only make the constraint more restrictive.
    fn is_upper_bound_only(constraint: &str) -> bool {
        let trimmed = constraint.trim();
        (trimmed.starts_with('<') || trimmed.starts_with("<=")) && !trimmed.contains(',') // No other constraints (like >=x,<y)
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
            // Only replace the version number itself, preserving everything else
            // (package name, extras, operator, AND any additional constraints like ,<6)
            let version_match = caps.get(4).unwrap();

            let mut result = dep.to_string();
            result.replace_range(version_match.range(), new_version);
            result
        } else {
            dep.to_string()
        }
    }

    /// Extract index URLs from pyproject.toml for Poetry and PDM configurations
    /// Poetry uses [[tool.poetry.source]] with url field
    /// PDM uses [[tool.pdm.source]] with url field
    /// Returns (primary_url, extra_urls) where primary is the first found or default PyPI
    fn extract_index_urls(doc: &DocumentMut) -> (Option<String>, Vec<String>) {
        let mut urls: Vec<String> = Vec::new();

        // Check [[tool.poetry.source]]
        if let Some(Item::Table(tool)) = doc.get("tool")
            && let Some(Item::Table(poetry)) = tool.get("poetry")
            && let Some(Item::ArrayOfTables(sources)) = poetry.get("source")
        {
            for source in sources.iter() {
                if let Some(Item::Value(Value::String(url))) = source.get("url") {
                    let url_str = url.value().to_string();
                    if !url_str.is_empty() {
                        urls.push(url_str);
                    }
                }
            }
        }

        // Check [[tool.pdm.source]]
        if let Some(Item::Table(tool)) = doc.get("tool")
            && let Some(Item::Table(pdm)) = tool.get("pdm")
            && let Some(Item::ArrayOfTables(sources)) = pdm.get("source")
        {
            for source in sources.iter() {
                if let Some(Item::Value(Value::String(url))) = source.get("url") {
                    let url_str = url.value().to_string();
                    if !url_str.is_empty() && !urls.contains(&url_str) {
                        urls.push(url_str);
                    }
                }
            }
        }

        // Also check for uv's [[tool.uv.index]] format
        if let Some(Item::Table(tool)) = doc.get("tool")
            && let Some(Item::Table(uv)) = tool.get("uv")
            && let Some(Item::ArrayOfTables(indexes)) = uv.get("index")
        {
            for index in indexes.iter() {
                if let Some(Item::Value(Value::String(url))) = index.get("url") {
                    let url_str = url.value().to_string();
                    if !url_str.is_empty() && !urls.contains(&url_str) {
                        urls.push(url_str);
                    }
                }
            }
        }

        if urls.is_empty() {
            (None, Vec::new())
        } else {
            let primary = urls.remove(0);
            (Some(primary), urls)
        }
    }

    /// Create a registry from the pyproject.toml index configuration
    /// If no index URLs are found, returns None to use the default registry
    fn create_registry_from_config(doc: &DocumentMut) -> Option<Arc<dyn Registry + Send + Sync>> {
        let (primary_url, extra_urls) = Self::extract_index_urls(doc);

        if let Some(url) = primary_url {
            let primary = PyPiRegistry::from_url(&url);
            if extra_urls.is_empty() {
                Some(Arc::new(primary))
            } else {
                Some(Arc::new(MultiPyPiRegistry::from_primary_and_extras(
                    primary, extra_urls,
                )))
            }
        } else {
            None
        }
    }

    async fn update_array_deps(
        &self,
        array: &mut toml_edit::Array,
        registry: &dyn Registry,
        result: &mut UpdateResult,
        original_content: &str,
        options: &UpdateOptions,
    ) {
        // First pass: collect all dependencies and separate by config status
        let mut ignored_deps: Vec<(String, String)> = Vec::new();
        let mut pinned_deps: Vec<(usize, String, String, String, String)> = Vec::new();
        let mut deps_to_check: Vec<(usize, String, String, String, String)> = Vec::new();

        for i in 0..array.len() {
            if let Some(item) = array.get(i)
                && let Some(s) = item.as_str()
                && let Some((package, current_version, full_constraint)) = self.parse_dependency(s)
            {
                // Check if package should be ignored
                if options.should_ignore(&package) {
                    ignored_deps.push((package, current_version));
                    continue;
                }

                // Check if package has a pinned version
                if let Some(pinned_version) = options.get_pinned_version(&package) {
                    pinned_deps.push((
                        i,
                        s.to_string(),
                        package,
                        current_version,
                        pinned_version.to_string(),
                    ));
                    continue;
                }

                deps_to_check.push((i, s.to_string(), package, current_version, full_constraint));
            }
        }

        // Record ignored packages
        for (package, version) in ignored_deps {
            let line_num = Self::find_dependency_line(original_content, &package);
            result.ignored.push((package, version, line_num));
        }

        // Process pinned packages (no registry fetch needed)
        let mut updates: Vec<(usize, String)> = Vec::new();
        for (i, dep_str, package, current_version, pinned_version) in pinned_deps {
            let matched_version = if options.full_precision {
                pinned_version.clone()
            } else {
                match_version_precision(&current_version, &pinned_version)
            };

            if matched_version != current_version {
                let updated = self.update_dependency(&dep_str, &matched_version);
                let line_num = Self::find_dependency_line(original_content, &package);
                result
                    .pinned
                    .push((package, current_version, matched_version.clone(), line_num));
                updates.push((i, updated));
            } else {
                result.unchanged += 1;
            }
        }

        // Fetch versions for remaining deps in parallel
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
        for ((i, dep_str, package, current_version, full_constraint), version_result) in
            deps_to_check.into_iter().zip(version_results)
        {
            // Skip upper-bound-only constraints (e.g., "<6", "<=5.0")
            // These define a ceiling, not a floor - updating them would only restrict versions
            if Self::is_upper_bound_only(&full_constraint) {
                result.unchanged += 1;
                continue;
            }

            match version_result {
                Ok(latest_version) => {
                    // Match the precision of the original version (unless full precision requested)
                    let matched_version = if options.full_precision {
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
        options: &UpdateOptions,
    ) {
        // First pass: collect dependencies and separate by config status
        let mut ignored_deps: Vec<(String, String)> = Vec::new();
        let mut pinned_deps: Vec<(String, String, String, String)> = Vec::new();
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

                let package = key.to_string();

                // Check if package should be ignored
                if options.should_ignore(&package) {
                    ignored_deps.push((package, version));
                    continue;
                }

                // Check if package has a pinned version
                if let Some(pinned_version) = options.get_pinned_version(&package) {
                    pinned_deps.push((package, prefix, version, pinned_version.to_string()));
                    continue;
                }

                deps_to_check.push((package, prefix, version));
            }
        }

        // Record ignored packages
        for (package, version) in ignored_deps {
            let line_num = Self::find_dependency_line(original_content, &package);
            result.ignored.push((package, version, line_num));
        }

        // Process pinned packages (no registry fetch needed)
        for (key, prefix, version, pinned_version) in pinned_deps {
            let matched_version = if options.full_precision {
                pinned_version.clone()
            } else {
                match_version_precision(&version, &pinned_version)
            };

            if matched_version != version {
                let new_val = format!("{}{}", prefix, matched_version);
                let line_num = Self::find_dependency_line(original_content, &key);
                result
                    .pinned
                    .push((key.clone(), version, matched_version.clone(), line_num));

                // Preserve decoration when updating
                if let Some(Item::Value(Value::String(formatted))) = deps_table.get_mut(&key) {
                    let decor = formatted.decor().clone();
                    let mut new_formatted = Formatted::new(new_val);
                    *new_formatted.decor_mut() = decor;
                    *formatted = new_formatted;
                }
            } else {
                result.unchanged += 1;
            }
        }

        // Fetch versions for remaining deps in parallel
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
                    let matched_version = if options.full_precision {
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
        let mut doc: DocumentMut = content.parse().map_err(|e: toml_edit::TomlError| {
            anyhow!(
                "Failed to parse {}:\n  {}",
                path.display(),
                e.to_string().replace('\n', "\n  ")
            )
        })?;

        let mut result = UpdateResult::default();

        // Check for inline index configuration (Poetry/PDM/uv)
        // If found, use that registry instead of the default
        let inline_registry = Self::create_registry_from_config(&doc);
        let effective_registry: &dyn Registry = if let Some(ref inline) = inline_registry {
            inline.as_ref()
        } else {
            registry
        };

        // Update [project.dependencies]
        if let Some(Item::Table(project)) = doc.get_mut("project") {
            if let Some(Item::Value(Value::Array(deps))) = project.get_mut("dependencies") {
                self.update_array_deps(deps, effective_registry, &mut result, &content, &options)
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
                            effective_registry,
                            &mut result,
                            &content,
                            &options,
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
                        effective_registry,
                        &mut result,
                        &content,
                        &options,
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
                self.update_poetry_deps(deps, effective_registry, &mut result, &content, &options)
                    .await;
            }

            if let Some(Item::Table(deps)) = poetry.get_mut("dev-dependencies") {
                self.update_poetry_deps(deps, effective_registry, &mut result, &content, &options)
                    .await;
            }
        }

        if (!result.updated.is_empty() || !result.pinned.is_empty()) && !options.dry_run {
            write_file_atomic(path, &doc.to_string())?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::PyProject
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let doc: DocumentMut = content.parse().map_err(|e: toml_edit::TomlError| {
            anyhow!(
                "Failed to parse {}:\n  {}",
                path.display(),
                e.to_string().replace('\n', "\n  ")
            )
        })?;

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
    fn test_is_upper_bound_only() {
        // Upper-bound-only constraints - should not be updated
        assert!(PyProjectUpdater::is_upper_bound_only("<6"));
        assert!(PyProjectUpdater::is_upper_bound_only("<4.2"));
        assert!(PyProjectUpdater::is_upper_bound_only("<=5.0"));
        assert!(PyProjectUpdater::is_upper_bound_only("<=2.0.0"));

        // NOT upper-bound-only (have lower bounds or are pinned)
        assert!(!PyProjectUpdater::is_upper_bound_only(">=1.0.0,<2.0.0")); // Has lower bound
        assert!(!PyProjectUpdater::is_upper_bound_only(">=2.8.0,<9")); // Has lower bound
        assert!(!PyProjectUpdater::is_upper_bound_only("==1.0.0")); // Pinned
        assert!(!PyProjectUpdater::is_upper_bound_only(">=1.0.0")); // Lower bound only
        assert!(!PyProjectUpdater::is_upper_bound_only(">1.0.0")); // Lower bound only
        assert!(!PyProjectUpdater::is_upper_bound_only("~=1.4")); // Compatible release
        assert!(!PyProjectUpdater::is_upper_bound_only("!=1.5.0")); // Exclusion
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

        // Constraint preservation - upper bound should be kept
        assert_eq!(
            updater.update_dependency("django>=4.0,<6", "5.2"),
            "django>=5.2,<6"
        );

        assert_eq!(
            updater.update_dependency("pytest>=2.8.0,<9", "8.0.0"),
            "pytest>=8.0.0,<9"
        );

        // Multiple constraints should all be preserved
        assert_eq!(
            updater.update_dependency("foo>=1.0.0,!=1.5.0,<2.0.0", "1.8.0"),
            "foo>=1.8.0,!=1.5.0,<2.0.0"
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
        let options = UpdateOptions::new(false, false);

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
        let options = UpdateOptions::new(false, false);

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
        let options = UpdateOptions::new(true, false);

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
        let options = UpdateOptions::new(false, false);

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
        let options = UpdateOptions::new(false, false);

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
        let options = UpdateOptions::new(false, false);

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
        let options = UpdateOptions::new(false, false);

        let result = updater.update(file.path(), &registry, options).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to parse"));
    }

    #[tokio::test]
    async fn test_update_pyproject_file_not_found() {
        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

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
        let options = UpdateOptions::new(false, false);

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
        let options = UpdateOptions::new(false, false);

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
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
    }

    // Tests for index URL extraction

    #[test]
    fn test_extract_poetry_source_urls() {
        let content = r#"
[tool.poetry]
name = "myproject"

[[tool.poetry.source]]
name = "private"
url = "https://private.pypi.com/simple"

[[tool.poetry.source]]
name = "extra"
url = "https://extra.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert_eq!(primary, Some("https://private.pypi.com/simple".to_string()));
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0], "https://extra.pypi.com/simple");
    }

    #[test]
    fn test_extract_pdm_source_urls() {
        let content = r#"
[tool.pdm]
name = "myproject"

[[tool.pdm.source]]
name = "private"
url = "https://private.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert_eq!(primary, Some("https://private.pypi.com/simple".to_string()));
        assert!(extras.is_empty());
    }

    #[test]
    fn test_extract_uv_index_urls() {
        let content = r#"
[tool.uv]

[[tool.uv.index]]
name = "pytorch"
url = "https://download.pytorch.org/whl/cpu"

[[tool.uv.index]]
name = "private"
url = "https://private.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert_eq!(
            primary,
            Some("https://download.pytorch.org/whl/cpu".to_string())
        );
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0], "https://private.pypi.com/simple");
    }

    #[test]
    fn test_extract_no_sources() {
        let content = r#"
[project]
name = "myproject"
dependencies = ["requests>=2.0.0"]
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert!(primary.is_none());
        assert!(extras.is_empty());
    }

    #[test]
    fn test_extract_combined_sources() {
        // Poetry and PDM sources in the same file (unlikely but should handle)
        let content = r#"
[[tool.poetry.source]]
name = "poetry-private"
url = "https://poetry.pypi.com/simple"

[[tool.pdm.source]]
name = "pdm-private"
url = "https://pdm.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert_eq!(primary, Some("https://poetry.pypi.com/simple".to_string()));
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0], "https://pdm.pypi.com/simple");
    }

    #[test]
    fn test_extract_skips_duplicate_urls() {
        let content = r#"
[[tool.poetry.source]]
name = "private1"
url = "https://private.pypi.com/simple"

[[tool.pdm.source]]
name = "private2"
url = "https://private.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        // Should only have one unique URL
        assert_eq!(primary, Some("https://private.pypi.com/simple".to_string()));
        assert!(extras.is_empty());
    }

    // Tests for config-based ignore/pin functionality

    #[tokio::test]
    async fn test_update_pyproject_pep621_with_config_ignore() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
    "django>=4.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "5.0.0");

        // Create config that ignores flask
        let config = UpdConfig {
            ignore: vec!["flask".to_string()],
            pin: std::collections::HashMap::new(),
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 packages updated (requests, django), 1 ignored (flask)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "flask");
        assert_eq!(result.ignored[0].1, "2.0.0");

        // Verify file was updated only for non-ignored packages
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=2.0.0")); // unchanged
        assert!(contents.contains("django>=5.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_pep621_with_config_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
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

        // Create config that pins flask to 2.3.0
        let mut pin = std::collections::HashMap::new();
        pin.insert("flask".to_string(), "2.3.0".to_string());
        let config = UpdConfig {
            ignore: Vec::new(),
            pin,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 package updated from registry, 1 pinned
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "requests");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "flask");
        assert_eq!(result.pinned[0].1, "2.0.0"); // old
        assert_eq!(result.pinned[0].2, "2.3.0"); // new (pinned)

        // Verify file was updated with pinned version
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=2.3.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_poetry_with_config_ignore() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.28.0"
flask = "^2.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        // Create config that ignores requests
        let config = UpdConfig {
            ignore: vec!["requests".to_string()],
            pin: std::collections::HashMap::new(),
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 package updated (flask), 1 ignored (requests)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "flask");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "requests");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests = \"^2.28.0\"")); // unchanged
        assert!(contents.contains("flask = \"^3.0.0\""));
    }

    #[tokio::test]
    async fn test_update_pyproject_poetry_with_config_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.28.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        // Create config that pins requests to 2.29.0
        let mut pin = std::collections::HashMap::new();
        pin.insert("requests".to_string(), "2.29.0".to_string());
        let config = UpdConfig {
            ignore: Vec::new(),
            pin,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 pinned
        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "requests");
        assert_eq!(result.pinned[0].2, "2.29.0");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests = \"^2.29.0\""));
    }

    #[tokio::test]
    async fn test_update_pyproject_with_config_ignore_and_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
    "django>=4.0.0",
    "pytest>=7.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "5.0.0")
            .with_version("pytest", "8.0.0");

        // Config: ignore flask, pin django to 4.2.0
        let mut pin = std::collections::HashMap::new();
        pin.insert("django".to_string(), "4.2.0".to_string());
        let config = UpdConfig {
            ignore: vec!["flask".to_string()],
            pin,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 updated from registry (requests, pytest), 1 ignored (flask), 1 pinned (django)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.pinned.len(), 1);

        // Verify ignored
        assert_eq!(result.ignored[0].0, "flask");

        // Verify pinned
        assert_eq!(result.pinned[0].0, "django");
        assert_eq!(result.pinned[0].2, "4.2.0");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=2.0.0")); // unchanged (ignored)
        assert!(contents.contains("django>=4.2.0")); // pinned version
        assert!(contents.contains("pytest>=8.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_optional_deps_with_config() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.28.0"]

[project.optional-dependencies]
dev = ["pytest>=7.0.0", "black>=23.0.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("pytest", "8.0.0")
            .with_version("black", "24.0.0");

        // Config: ignore pytest
        let config = UpdConfig {
            ignore: vec!["pytest".to_string()],
            pin: std::collections::HashMap::new(),
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 updated (requests, black), 1 ignored (pytest)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "pytest");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("pytest>=7.0.0")); // unchanged
        assert!(contents.contains("black>=24.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_pin_preserves_prefix() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.28.0"
flask = "~2.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        // Pin both with different prefixes
        let mut pin = std::collections::HashMap::new();
        pin.insert("requests".to_string(), "2.30.0".to_string());
        pin.insert("flask".to_string(), "2.5.0".to_string());
        let config = UpdConfig {
            ignore: Vec::new(),
            pin,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.pinned.len(), 2);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        // Prefixes should be preserved
        assert!(contents.contains("requests = \"^2.30.0\""));
        assert!(contents.contains("flask = \"~2.5.0\""));
    }
}
