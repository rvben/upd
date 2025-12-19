use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, read_file_safe,
    write_file_atomic,
};
use crate::registry::{CratesIoRegistry, Registry};
use crate::version::{is_stable_semver, match_version_precision};
use anyhow::{Result, anyhow};
use futures::future::join_all;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use toml_edit::{DocumentMut, Formatted, Item, Table, Value};

pub struct CargoTomlUpdater;

impl CargoTomlUpdater {
    pub fn new() -> Self {
        Self
    }

    /// Extract registries defined in Cargo.toml [registries.name] sections
    /// Returns a map of registry name -> index URL
    fn extract_registries(doc: &DocumentMut) -> HashMap<String, String> {
        let mut registries = HashMap::new();

        if let Some(Item::Table(regs)) = doc.get("registries") {
            for (name, item) in regs.iter() {
                // Handle [registries.name] table format
                if let Item::Table(reg_table) = item
                    && let Some(Item::Value(Value::String(index))) = reg_table.get("index")
                {
                    registries.insert(name.to_string(), index.value().clone());
                // Handle inline table format: name = { index = "url" }
                } else if let Item::Value(Value::InlineTable(inline)) = item
                    && let Some(Value::String(index)) = inline.get("index")
                {
                    registries.insert(name.to_string(), index.value().clone());
                }
            }
        }

        registries
    }

    /// Get the registry name from a dependency item (if specified)
    fn get_registry_name(item: &Item) -> Option<String> {
        match item {
            Item::Value(Value::InlineTable(t)) => t
                .get("registry")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            Item::Table(t) => t
                .get("registry")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            _ => None,
        }
    }

    /// Create a registry for a named registry defined in Cargo.toml or config.toml
    fn create_registry_for_name(
        name: &str,
        cargo_toml_registries: &HashMap<String, String>,
    ) -> Option<Arc<dyn Registry + Send + Sync>> {
        // First check Cargo.toml [registries] section
        if let Some(index_url) = cargo_toml_registries.get(name) {
            let api_url = Self::sparse_index_to_api_url(index_url);
            let credentials = CratesIoRegistry::detect_credentials(name);
            return Some(Arc::new(
                CratesIoRegistry::with_registry_url_and_credentials(api_url, credentials),
            ));
        }

        // Fall back to ~/.cargo/config.toml
        CratesIoRegistry::for_named_registry(name)
            .map(|r| Arc::new(r) as Arc<dyn Registry + Send + Sync>)
    }

    /// Convert a sparse registry index URL to an API URL
    fn sparse_index_to_api_url(index_url: &str) -> String {
        let url = index_url
            .strip_prefix("sparse+")
            .unwrap_or(index_url)
            .trim_end_matches('/');

        // Remove /index suffix if present
        let base = url.strip_suffix("/index").unwrap_or(url);

        format!("{}/api/v1/crates", base)
    }

    /// Extract version string from a dependency item
    /// Handles: "1.0" (string), { version = "1.0", ... } (inline table), [dependencies.foo] table
    fn get_version(item: &Item) -> Option<String> {
        match item {
            Item::Value(Value::String(s)) => Some(s.value().clone()),
            Item::Value(Value::InlineTable(t)) => t
                .get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            Item::Table(t) => t
                .get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            _ => None,
        }
    }

    /// Set version on a dependency item, preserving structure
    fn set_version(item: &mut Item, new_version: &str) {
        match item {
            Item::Value(Value::String(s)) => {
                let decor = s.decor().clone();
                let mut new_formatted = Formatted::new(new_version.to_string());
                *new_formatted.decor_mut() = decor;
                *s = new_formatted;
            }
            Item::Value(Value::InlineTable(t)) => {
                if let Some(Value::String(s)) = t.get_mut("version") {
                    let decor = s.decor().clone();
                    let mut new_formatted = Formatted::new(new_version.to_string());
                    *new_formatted.decor_mut() = decor;
                    *s = new_formatted;
                }
            }
            Item::Table(t) => {
                if let Some(Item::Value(Value::String(s))) = t.get_mut("version") {
                    let decor = s.decor().clone();
                    let mut new_formatted = Formatted::new(new_version.to_string());
                    *new_formatted.decor_mut() = decor;
                    *s = new_formatted;
                }
            }
            _ => {}
        }
    }

    /// Find the line number where a dependency is defined
    fn find_dependency_line(content: &str, dep_name: &str) -> Option<usize> {
        for (line_idx, line) in content.lines().enumerate() {
            // Look for lines that contain the dependency name as a key
            // This handles: dep_name = "version", dep_name = { version = "..." }, [dependencies.dep_name]
            if line.contains(dep_name)
                && (line.trim().starts_with(dep_name)
                    || line.contains(&format!(".{}", dep_name))
                    || line.contains(&format!("[dependencies.{}]", dep_name)))
            {
                return Some(line_idx + 1); // 1-indexed
            }
        }
        None
    }

    /// Parse version requirement to extract the actual version number
    /// Handles: "1.0", "^1.0", "~1.0", ">=1.0", "=1.0", etc.
    fn parse_version_req(version_req: &str) -> (String, String) {
        let trimmed = version_req.trim();

        // Find where the version number starts
        let version_start = trimmed
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(trimmed.len());

        let prefix = &trimmed[..version_start];
        let version = &trimmed[version_start..];

        (prefix.to_string(), version.to_string())
    }

    /// Update dependencies in a table (e.g., [dependencies], [dev-dependencies])
    #[allow(clippy::too_many_arguments)]
    async fn update_deps_table(
        &self,
        table: &mut Table,
        default_registry: &dyn Registry,
        cargo_toml_registries: &HashMap<String, String>,
        registry_cache: &mut HashMap<String, Arc<dyn Registry + Send + Sync>>,
        result: &mut UpdateResult,
        original_content: &str,
        options: &UpdateOptions,
    ) {
        // First pass: collect dependencies and separate by config status
        let mut ignored_deps: Vec<(String, String)> = Vec::new();
        let mut pinned_deps: Vec<(String, String, String, String)> = Vec::new();
        let mut deps_to_check: Vec<(String, String, String, Option<String>)> = Vec::new();

        for (key, item) in table.iter() {
            // Skip path/git dependencies (they have no version to update from registry)
            if let Item::Value(Value::InlineTable(t)) = item
                && (t.contains_key("path") || t.contains_key("git"))
            {
                continue;
            }
            if let Item::Table(t) = item
                && (t.contains_key("path") || t.contains_key("git"))
            {
                continue;
            }

            let Some(version_req) = Self::get_version(item) else {
                continue;
            };

            let registry_name = Self::get_registry_name(item);
            let (prefix, current_version) = Self::parse_version_req(&version_req);
            let package = key.to_string();

            // Check if package should be ignored
            if options.should_ignore(&package) {
                ignored_deps.push((package, current_version));
                continue;
            }

            // Check if package has a pinned version
            if let Some(pinned_version) = options.get_pinned_version(&package) {
                pinned_deps.push((package, prefix, current_version, pinned_version.to_string()));
                continue;
            }

            deps_to_check.push((package, prefix, current_version, registry_name));
        }

        // Record ignored packages
        for (package, version) in ignored_deps {
            let line_num = Self::find_dependency_line(original_content, &package);
            result.ignored.push((package, version, line_num));
        }

        // Process pinned packages (no registry fetch needed)
        for (key, prefix, current_version, pinned_version) in pinned_deps {
            let matched_version = if options.full_precision {
                pinned_version.clone()
            } else {
                match_version_precision(&current_version, &pinned_version)
            };

            if matched_version != current_version {
                let new_version_req = format!("{}{}", prefix, matched_version);
                if let Some(item) = table.get_mut(&key) {
                    Self::set_version(item, &new_version_req);
                }
                let line_num = Self::find_dependency_line(original_content, &key);
                result
                    .pinned
                    .push((key.clone(), current_version, matched_version, line_num));
            } else {
                result.unchanged += 1;
            }
        }

        // Ensure custom registries are created and cached
        for (_, _, _, registry_name) in &deps_to_check {
            if let Some(name) = registry_name
                && !registry_cache.contains_key(name)
                && let Some(reg) = Self::create_registry_for_name(name, cargo_toml_registries)
            {
                registry_cache.insert(name.clone(), reg);
            }
        }

        // Fetch all versions in parallel for non-ignored, non-pinned packages
        let version_futures: Vec<_> = deps_to_check
            .iter()
            .map(|(key, _, current_version, registry_name)| {
                let effective_registry: &dyn Registry = if let Some(name) = registry_name {
                    registry_cache
                        .get(name)
                        .map(|r| r.as_ref())
                        .unwrap_or(default_registry)
                } else {
                    default_registry
                };

                async move {
                    if is_stable_semver(current_version) {
                        effective_registry.get_latest_version(key).await
                    } else {
                        effective_registry
                            .get_latest_version_including_prereleases(key)
                            .await
                    }
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results
        for ((key, prefix, current_version, _registry_name), version_result) in
            deps_to_check.into_iter().zip(version_results)
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
                        let new_version_req = format!("{}{}", prefix, matched_version);
                        if let Some(item) = table.get_mut(&key) {
                            Self::set_version(item, &new_version_req);
                        }
                        let line_num = Self::find_dependency_line(original_content, &key);
                        result.updated.push((
                            key.clone(),
                            current_version,
                            matched_version,
                            line_num,
                        ));
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

    /// Update dependencies in an inline table within another table
    /// Used for [workspace.dependencies]
    #[allow(clippy::too_many_arguments)]
    async fn update_workspace_deps(
        &self,
        deps_item: &mut Item,
        default_registry: &dyn Registry,
        cargo_toml_registries: &HashMap<String, String>,
        registry_cache: &mut HashMap<String, Arc<dyn Registry + Send + Sync>>,
        result: &mut UpdateResult,
        original_content: &str,
        options: &UpdateOptions,
    ) {
        let table = match deps_item {
            Item::Table(t) => t,
            _ => return,
        };

        self.update_deps_table(
            table,
            default_registry,
            cargo_toml_registries,
            registry_cache,
            result,
            original_content,
            options,
        )
        .await;
    }
}

impl Default for CargoTomlUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for CargoTomlUpdater {
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

        // Extract registries defined in Cargo.toml
        let cargo_toml_registries = Self::extract_registries(&doc);
        // Cache for registry instances (reused across dependency tables)
        let mut registry_cache: HashMap<String, Arc<dyn Registry + Send + Sync>> = HashMap::new();

        // Update [dependencies]
        if let Some(Item::Table(deps)) = doc.get_mut("dependencies") {
            self.update_deps_table(
                deps,
                registry,
                &cargo_toml_registries,
                &mut registry_cache,
                &mut result,
                &content,
                &options,
            )
            .await;
        }

        // Update [dev-dependencies]
        if let Some(Item::Table(deps)) = doc.get_mut("dev-dependencies") {
            self.update_deps_table(
                deps,
                registry,
                &cargo_toml_registries,
                &mut registry_cache,
                &mut result,
                &content,
                &options,
            )
            .await;
        }

        // Update [build-dependencies]
        if let Some(Item::Table(deps)) = doc.get_mut("build-dependencies") {
            self.update_deps_table(
                deps,
                registry,
                &cargo_toml_registries,
                &mut registry_cache,
                &mut result,
                &content,
                &options,
            )
            .await;
        }

        // Update [workspace.dependencies]
        if let Some(Item::Table(workspace)) = doc.get_mut("workspace")
            && let Some(deps) = workspace.get_mut("dependencies")
        {
            self.update_workspace_deps(
                deps,
                registry,
                &cargo_toml_registries,
                &mut registry_cache,
                &mut result,
                &content,
                &options,
            )
            .await;
        }

        // Update [target.'cfg(...)'.dependencies] sections
        if let Some(Item::Table(target)) = doc.get_mut("target") {
            let target_keys: Vec<String> = target.iter().map(|(k, _)| k.to_string()).collect();

            for target_key in target_keys {
                if let Some(Item::Table(target_table)) = target.get_mut(&target_key) {
                    // Update dependencies for this target
                    if let Some(Item::Table(deps)) = target_table.get_mut("dependencies") {
                        self.update_deps_table(
                            deps,
                            registry,
                            &cargo_toml_registries,
                            &mut registry_cache,
                            &mut result,
                            &content,
                            &options,
                        )
                        .await;
                    }
                    if let Some(Item::Table(deps)) = target_table.get_mut("dev-dependencies") {
                        self.update_deps_table(
                            deps,
                            registry,
                            &cargo_toml_registries,
                            &mut registry_cache,
                            &mut result,
                            &content,
                            &options,
                        )
                        .await;
                    }
                    if let Some(Item::Table(deps)) = target_table.get_mut("build-dependencies") {
                        self.update_deps_table(
                            deps,
                            registry,
                            &cargo_toml_registries,
                            &mut registry_cache,
                            &mut result,
                            &content,
                            &options,
                        )
                        .await;
                    }
                }
            }
        }

        if (!result.updated.is_empty() || !result.pinned.is_empty()) && !options.dry_run {
            write_file_atomic(path, &doc.to_string())?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::CargoToml
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

        // Helper to parse dependencies from a table
        let parse_table = |table: &toml_edit::Table, deps: &mut Vec<ParsedDependency>| {
            for (key, item) in table.iter() {
                // Skip path/git dependencies
                if let Item::Value(Value::InlineTable(t)) = item
                    && (t.contains_key("path") || t.contains_key("git"))
                {
                    continue;
                }
                if let Item::Table(t) = item
                    && (t.contains_key("path") || t.contains_key("git"))
                {
                    continue;
                }

                if let Some(version_req) = Self::get_version(item) {
                    let (_, version) = Self::parse_version_req(&version_req);
                    let line_num = Self::find_dependency_line(&content, key);
                    deps.push(ParsedDependency {
                        name: key.to_string(),
                        version,
                        line_number: line_num,
                        has_upper_bound: false, // Cargo.toml doesn't use same constraint syntax as Python
                    });
                }
            }
        };

        // Parse [dependencies]
        if let Some(Item::Table(table)) = doc.get("dependencies") {
            parse_table(table, &mut deps);
        }

        // Parse [dev-dependencies]
        if let Some(Item::Table(table)) = doc.get("dev-dependencies") {
            parse_table(table, &mut deps);
        }

        // Parse [build-dependencies]
        if let Some(Item::Table(table)) = doc.get("build-dependencies") {
            parse_table(table, &mut deps);
        }

        // Parse [workspace.dependencies]
        if let Some(Item::Table(workspace)) = doc.get("workspace")
            && let Some(Item::Table(table)) = workspace.get("dependencies")
        {
            parse_table(table, &mut deps);
        }

        // Parse [target.'cfg(...)'.dependencies] sections
        if let Some(Item::Table(target)) = doc.get("target") {
            for (_, target_item) in target.iter() {
                if let Item::Table(target_table) = target_item {
                    if let Some(Item::Table(table)) = target_table.get("dependencies") {
                        parse_table(table, &mut deps);
                    }
                    if let Some(Item::Table(table)) = target_table.get("dev-dependencies") {
                        parse_table(table, &mut deps);
                    }
                    if let Some(Item::Table(table)) = target_table.get("build-dependencies") {
                        parse_table(table, &mut deps);
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
    use toml_edit::InlineTable;

    #[test]
    fn test_parse_version_req() {
        assert_eq!(
            CargoTomlUpdater::parse_version_req("1.0.0"),
            ("".to_string(), "1.0.0".to_string())
        );
        assert_eq!(
            CargoTomlUpdater::parse_version_req("^1.0.0"),
            ("^".to_string(), "1.0.0".to_string())
        );
        assert_eq!(
            CargoTomlUpdater::parse_version_req("~1.0.0"),
            ("~".to_string(), "1.0.0".to_string())
        );
        assert_eq!(
            CargoTomlUpdater::parse_version_req(">=1.0.0"),
            (">=".to_string(), "1.0.0".to_string())
        );
        assert_eq!(
            CargoTomlUpdater::parse_version_req("=1.0.0"),
            ("=".to_string(), "1.0.0".to_string())
        );
    }

    #[test]
    fn test_get_version_string() {
        let item = Item::Value(Value::String(Formatted::new("1.0.0".to_string())));
        assert_eq!(
            CargoTomlUpdater::get_version(&item),
            Some("1.0.0".to_string())
        );
    }

    #[test]
    fn test_get_version_inline_table() {
        let mut table = InlineTable::new();
        table.insert(
            "version",
            Value::String(Formatted::new("1.0.0".to_string())),
        );
        table.insert("features", Value::Array(toml_edit::Array::new()));

        let item = Item::Value(Value::InlineTable(table));
        assert_eq!(
            CargoTomlUpdater::get_version(&item),
            Some("1.0.0".to_string())
        );
    }

    #[tokio::test]
    async fn test_update_cargo_toml_file() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[package]
name = "test-crate"
version = "0.1.0"

[dependencies]
serde = "1.0.0"
tokio = {{ version = "1.28.0", features = ["full"] }}
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("serde", "1.0.195")
            .with_version("tokio", "1.35.0");

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("1.0.195"));
        assert!(content.contains("1.35.0"));
    }

    #[tokio::test]
    async fn test_update_cargo_toml_dry_run() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        let original = r#"[package]
name = "test-crate"
version = "0.1.0"

[dependencies]
serde = "1.0.0"
"#;
        write!(file, "{}", original).unwrap();

        let registry = MockRegistry::new("crates.io").with_version("serde", "1.0.195");

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(true, false);

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
    async fn test_update_cargo_toml_preserves_prefix() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[dependencies]
caret = "^1.0.0"
tilde = "~1.0.0"
exact = "=1.0.0"
gte = ">=1.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("caret", "2.0.0")
            .with_version("tilde", "2.0.0")
            .with_version("exact", "2.0.0")
            .with_version("gte", "2.0.0");

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 4);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"^2.0.0\""));
        assert!(content.contains("\"~2.0.0\""));
        assert!(content.contains("\"=2.0.0\""));
        assert!(content.contains("\">=2.0.0\""));
    }

    #[tokio::test]
    async fn test_update_cargo_toml_dev_dependencies() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[dev-dependencies]
tempfile = "3.8.0"
criterion = "0.5.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("tempfile", "3.10.0")
            .with_version("criterion", "0.5.1");

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("3.10.0"));
        assert!(content.contains("0.5.1"));
    }

    #[tokio::test]
    async fn test_update_cargo_toml_skips_path_and_git() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[dependencies]
local-crate = {{ path = "../local" }}
git-crate = {{ git = "https://github.com/user/repo" }}
normal-crate = "1.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io").with_version("normal-crate", "2.0.0");

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only normal-crate should be updated
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "normal-crate");
    }

    #[tokio::test]
    async fn test_update_cargo_toml_workspace_dependencies() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[workspace]
members = ["crate-a", "crate-b"]

[workspace.dependencies]
serde = "1.0.0"
tokio = "1.28.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("serde", "1.0.195")
            .with_version("tokio", "1.35.0");

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("1.0.195"));
        assert!(content.contains("1.35.0"));
    }

    #[tokio::test]
    async fn test_update_cargo_toml_preserves_formatting() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[package]
name = "test-crate"
version = "0.1.0"

# This is a comment
[dependencies]
serde = "1.0.0"  # inline comment
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io").with_version("serde", "1.0.195");

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false);

        updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        // Comments should be preserved
        assert!(content.contains("# This is a comment"));
    }

    #[tokio::test]
    async fn test_update_cargo_toml_registry_error() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[dependencies]
nonexistent-crate = "1.0.0"
"#
        )
        .unwrap();

        // Registry without the crate
        let registry = MockRegistry::new("crates.io");

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent-crate"));
    }

    #[test]
    fn test_extract_registries_table_format() {
        let content = r#"
[registries.my-registry]
index = "sparse+https://my-registry.example.com/index"

[registries.another-registry]
index = "https://another.example.com/index"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let registries = CargoTomlUpdater::extract_registries(&doc);

        assert_eq!(registries.len(), 2);
        assert_eq!(
            registries.get("my-registry"),
            Some(&"sparse+https://my-registry.example.com/index".to_string())
        );
        assert_eq!(
            registries.get("another-registry"),
            Some(&"https://another.example.com/index".to_string())
        );
    }

    #[test]
    fn test_extract_registries_inline_format() {
        let content = r#"
[registries]
my-registry = { index = "sparse+https://my-registry.example.com/index" }
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let registries = CargoTomlUpdater::extract_registries(&doc);

        assert_eq!(registries.len(), 1);
        assert_eq!(
            registries.get("my-registry"),
            Some(&"sparse+https://my-registry.example.com/index".to_string())
        );
    }

    #[test]
    fn test_extract_registries_empty() {
        let content = r#"
[package]
name = "test"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let registries = CargoTomlUpdater::extract_registries(&doc);

        assert!(registries.is_empty());
    }

    #[test]
    fn test_get_registry_name_inline_table() {
        let mut table = InlineTable::new();
        table.insert(
            "version",
            Value::String(Formatted::new("1.0.0".to_string())),
        );
        table.insert(
            "registry",
            Value::String(Formatted::new("my-registry".to_string())),
        );
        let item = Item::Value(Value::InlineTable(table));

        assert_eq!(
            CargoTomlUpdater::get_registry_name(&item),
            Some("my-registry".to_string())
        );
    }

    #[test]
    fn test_get_registry_name_table() {
        let content = r#"
[dependencies.my-crate]
version = "1.0.0"
registry = "my-registry"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let deps = doc.get("dependencies").unwrap();
        let my_crate = deps.get("my-crate").unwrap();

        assert_eq!(
            CargoTomlUpdater::get_registry_name(my_crate),
            Some("my-registry".to_string())
        );
    }

    #[test]
    fn test_get_registry_name_none() {
        let item = Item::Value(Value::String(Formatted::new("1.0.0".to_string())));
        assert_eq!(CargoTomlUpdater::get_registry_name(&item), None);
    }

    #[test]
    fn test_sparse_index_to_api_url() {
        // Sparse prefix
        assert_eq!(
            CargoTomlUpdater::sparse_index_to_api_url(
                "sparse+https://my-registry.example.com/index"
            ),
            "https://my-registry.example.com/api/v1/crates"
        );

        // No sparse prefix
        assert_eq!(
            CargoTomlUpdater::sparse_index_to_api_url("https://my-registry.example.com/index"),
            "https://my-registry.example.com/api/v1/crates"
        );

        // Trailing slash
        assert_eq!(
            CargoTomlUpdater::sparse_index_to_api_url(
                "sparse+https://my-registry.example.com/index/"
            ),
            "https://my-registry.example.com/api/v1/crates"
        );

        // No /index suffix
        assert_eq!(
            CargoTomlUpdater::sparse_index_to_api_url("sparse+https://my-registry.example.com"),
            "https://my-registry.example.com/api/v1/crates"
        );
    }

    // Tests for config-based ignore/pin functionality

    #[tokio::test]
    async fn test_update_cargo_toml_with_config_ignore() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[package]
name = "test"
version = "0.1.0"

[dependencies]
serde = "1.0.0"
tokio = "1.0.0"
anyhow = "1.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("serde", "1.0.200")
            .with_version("tokio", "1.37.0")
            .with_version("anyhow", "1.0.83");

        // Create config that ignores tokio
        let config = UpdConfig {
            ignore: vec!["tokio".to_string()],
            pin: std::collections::HashMap::new(),
        };

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 packages updated (serde, anyhow), 1 ignored (tokio)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "tokio");
        assert_eq!(result.ignored[0].1, "1.0.0");

        // Verify file was updated only for non-ignored packages
        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("serde = \"1.0.200\""));
        assert!(content.contains("tokio = \"1.0.0\"")); // unchanged
        assert!(content.contains("anyhow = \"1.0.83\""));
    }

    #[tokio::test]
    async fn test_update_cargo_toml_with_config_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[package]
name = "test"
version = "0.1.0"

[dependencies]
serde = "1.0.0"
tokio = "1.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("serde", "1.0.200")
            .with_version("tokio", "1.37.0");

        // Create config that pins serde to 1.0.150
        let mut pin = std::collections::HashMap::new();
        pin.insert("serde".to_string(), "1.0.150".to_string());
        let config = UpdConfig {
            ignore: Vec::new(),
            pin,
        };

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 package updated from registry (tokio), 1 pinned (serde)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "tokio");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "serde");
        assert_eq!(result.pinned[0].1, "1.0.0"); // old
        assert_eq!(result.pinned[0].2, "1.0.150"); // new (pinned)

        // Verify file was updated with pinned version
        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("serde = \"1.0.150\""));
        assert!(content.contains("tokio = \"1.37.0\""));
    }

    #[tokio::test]
    async fn test_update_cargo_toml_with_config_ignore_and_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[package]
name = "test"
version = "0.1.0"

[dependencies]
serde = "1.0.0"
tokio = "1.0.0"
anyhow = "1.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("serde", "1.0.200")
            .with_version("tokio", "1.37.0")
            .with_version("anyhow", "1.0.83");

        // Config: ignore tokio, pin serde to 1.0.150
        let mut pin = std::collections::HashMap::new();
        pin.insert("serde".to_string(), "1.0.150".to_string());
        let config = UpdConfig {
            ignore: vec!["tokio".to_string()],
            pin,
        };

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 updated from registry (anyhow), 1 ignored (tokio), 1 pinned (serde)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "anyhow");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "tokio");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "serde");
        assert_eq!(result.pinned[0].2, "1.0.150");

        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("serde = \"1.0.150\"")); // pinned
        assert!(content.contains("tokio = \"1.0.0\"")); // ignored
        assert!(content.contains("anyhow = \"1.0.83\"")); // registry
    }

    #[tokio::test]
    async fn test_update_cargo_toml_dev_deps_with_config() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[package]
name = "test"
version = "0.1.0"

[dev-dependencies]
assert_cmd = "2.0.0"
tempfile = "3.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("assert_cmd", "2.0.14")
            .with_version("tempfile", "3.10.1");

        // Config: ignore tempfile
        let config = UpdConfig {
            ignore: vec!["tempfile".to_string()],
            pin: std::collections::HashMap::new(),
        };

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 updated (assert_cmd), 1 ignored (tempfile)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "assert_cmd");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "tempfile");

        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("assert_cmd = \"2.0.14\""));
        assert!(content.contains("tempfile = \"3.0.0\"")); // unchanged
    }

    #[tokio::test]
    async fn test_update_cargo_toml_pin_preserves_prefix() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[package]
name = "test"
version = "0.1.0"

[dependencies]
serde = "^1.0.0"
tokio = "~1.0.0"
anyhow = "=1.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("serde", "1.0.200")
            .with_version("tokio", "1.37.0")
            .with_version("anyhow", "1.0.83");

        // Pin all with specific versions
        let mut pin = std::collections::HashMap::new();
        pin.insert("serde".to_string(), "1.0.150".to_string());
        pin.insert("tokio".to_string(), "1.20.0".to_string());
        pin.insert("anyhow".to_string(), "1.0.70".to_string());
        let config = UpdConfig {
            ignore: Vec::new(),
            pin,
        };

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.pinned.len(), 3);

        let content = std::fs::read_to_string(file.path()).unwrap();
        // Prefixes should be preserved
        assert!(content.contains("serde = \"^1.0.150\""));
        assert!(content.contains("tokio = \"~1.20.0\""));
        assert!(content.contains("anyhow = \"=1.0.70\""));
    }

    #[tokio::test]
    async fn test_update_cargo_toml_inline_table_with_config() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[package]
name = "test"
version = "0.1.0"

[dependencies]
serde = {{ version = "1.0.0", features = ["derive"] }}
tokio = {{ version = "1.0.0", features = ["full"] }}
"#
        )
        .unwrap();

        let registry = MockRegistry::new("crates.io")
            .with_version("serde", "1.0.200")
            .with_version("tokio", "1.37.0");

        // Config: ignore serde
        let config = UpdConfig {
            ignore: vec!["serde".to_string()],
            pin: std::collections::HashMap::new(),
        };

        let updater = CargoTomlUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 updated (tokio), 1 ignored (serde)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "tokio");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "serde");

        let content = std::fs::read_to_string(file.path()).unwrap();
        // serde should be unchanged, tokio should be updated
        assert!(content.contains("serde = { version = \"1.0.0\""));
        assert!(content.contains("tokio = { version = \"1.37.0\""));
    }
}
