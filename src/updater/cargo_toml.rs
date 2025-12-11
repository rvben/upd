use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, read_file_safe,
    write_file_atomic,
};
use crate::registry::Registry;
use crate::version::{is_stable_semver, match_version_precision};
use anyhow::{Result, anyhow};
use futures::future::join_all;
use std::path::Path;
use toml_edit::{DocumentMut, Formatted, Item, Table, Value};

pub struct CargoTomlUpdater;

impl CargoTomlUpdater {
    pub fn new() -> Self {
        Self
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
    async fn update_deps_table(
        &self,
        table: &mut Table,
        registry: &dyn Registry,
        result: &mut UpdateResult,
        original_content: &str,
        full_precision: bool,
    ) {
        // First pass: collect dependencies to check
        let mut deps_to_check: Vec<(String, String, String)> = Vec::new();

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

            let (prefix, current_version) = Self::parse_version_req(&version_req);
            deps_to_check.push((key.to_string(), prefix, current_version));
        }

        // Fetch all versions in parallel
        let version_futures: Vec<_> = deps_to_check
            .iter()
            .map(|(key, _, current_version)| async {
                if is_stable_semver(current_version) {
                    registry.get_latest_version(key).await
                } else {
                    registry.get_latest_version_including_prereleases(key).await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results
        for ((key, prefix, current_version), version_result) in
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
    async fn update_workspace_deps(
        &self,
        deps_item: &mut Item,
        registry: &dyn Registry,
        result: &mut UpdateResult,
        original_content: &str,
        full_precision: bool,
    ) {
        let table = match deps_item {
            Item::Table(t) => t,
            _ => return,
        };

        self.update_deps_table(table, registry, result, original_content, full_precision)
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
        let mut doc: DocumentMut = content
            .parse()
            .map_err(|e| anyhow!("Failed to parse Cargo.toml: {}", e))?;

        let mut result = UpdateResult::default();

        // Update [dependencies]
        if let Some(Item::Table(deps)) = doc.get_mut("dependencies") {
            self.update_deps_table(
                deps,
                registry,
                &mut result,
                &content,
                options.full_precision,
            )
            .await;
        }

        // Update [dev-dependencies]
        if let Some(Item::Table(deps)) = doc.get_mut("dev-dependencies") {
            self.update_deps_table(
                deps,
                registry,
                &mut result,
                &content,
                options.full_precision,
            )
            .await;
        }

        // Update [build-dependencies]
        if let Some(Item::Table(deps)) = doc.get_mut("build-dependencies") {
            self.update_deps_table(
                deps,
                registry,
                &mut result,
                &content,
                options.full_precision,
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
                &mut result,
                &content,
                options.full_precision,
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
                            &mut result,
                            &content,
                            options.full_precision,
                        )
                        .await;
                    }
                    if let Some(Item::Table(deps)) = target_table.get_mut("dev-dependencies") {
                        self.update_deps_table(
                            deps,
                            registry,
                            &mut result,
                            &content,
                            options.full_precision,
                        )
                        .await;
                    }
                    if let Some(Item::Table(deps)) = target_table.get_mut("build-dependencies") {
                        self.update_deps_table(
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

        if !result.updated.is_empty() && !options.dry_run {
            write_file_atomic(path, &doc.to_string())?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::CargoToml
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let doc: DocumentMut = content
            .parse()
            .map_err(|e| anyhow!("Failed to parse Cargo.toml: {}", e))?;

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
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

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
        let options = UpdateOptions {
            dry_run: false,
            full_precision: false,
        };

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
        assert!(result.errors[0].contains("nonexistent-crate"));
    }
}
