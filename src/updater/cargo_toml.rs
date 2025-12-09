use super::{FileType, UpdateResult, Updater};
use crate::registry::Registry;
use crate::version::is_stable_semver;
use anyhow::{Result, anyhow};
use std::fs;
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
    ) {
        // Collect keys to avoid borrow issues
        let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();

        for key in keys {
            let Some(item) = table.get_mut(&key) else {
                continue;
            };

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

            // If current version is a pre-release, include pre-releases in lookup
            let version_result = if is_stable_semver(&current_version) {
                registry.get_latest_version(&key).await
            } else {
                registry
                    .get_latest_version_including_prereleases(&key)
                    .await
            };

            match version_result {
                Ok(latest_version) => {
                    if latest_version != current_version {
                        let new_version_req = format!("{}{}", prefix, latest_version);
                        Self::set_version(item, &new_version_req);
                        let line_num = Self::find_dependency_line(original_content, &key);
                        result.updated.push((
                            key.clone(),
                            current_version,
                            latest_version,
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
    ) {
        let table = match deps_item {
            Item::Table(t) => t,
            _ => return,
        };

        self.update_deps_table(table, registry, result, original_content)
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
        dry_run: bool,
    ) -> Result<UpdateResult> {
        let content = fs::read_to_string(path)?;
        let mut doc: DocumentMut = content
            .parse()
            .map_err(|e| anyhow!("Failed to parse Cargo.toml: {}", e))?;

        let mut result = UpdateResult::default();

        // Update [dependencies]
        if let Some(Item::Table(deps)) = doc.get_mut("dependencies") {
            self.update_deps_table(deps, registry, &mut result, &content)
                .await;
        }

        // Update [dev-dependencies]
        if let Some(Item::Table(deps)) = doc.get_mut("dev-dependencies") {
            self.update_deps_table(deps, registry, &mut result, &content)
                .await;
        }

        // Update [build-dependencies]
        if let Some(Item::Table(deps)) = doc.get_mut("build-dependencies") {
            self.update_deps_table(deps, registry, &mut result, &content)
                .await;
        }

        // Update [workspace.dependencies]
        if let Some(Item::Table(workspace)) = doc.get_mut("workspace")
            && let Some(deps) = workspace.get_mut("dependencies")
        {
            self.update_workspace_deps(deps, registry, &mut result, &content)
                .await;
        }

        // Update [target.'cfg(...)'.dependencies] sections
        if let Some(Item::Table(target)) = doc.get_mut("target") {
            let target_keys: Vec<String> = target.iter().map(|(k, _)| k.to_string()).collect();

            for target_key in target_keys {
                if let Some(Item::Table(target_table)) = target.get_mut(&target_key) {
                    // Update dependencies for this target
                    if let Some(Item::Table(deps)) = target_table.get_mut("dependencies") {
                        self.update_deps_table(deps, registry, &mut result, &content)
                            .await;
                    }
                    if let Some(Item::Table(deps)) = target_table.get_mut("dev-dependencies") {
                        self.update_deps_table(deps, registry, &mut result, &content)
                            .await;
                    }
                    if let Some(Item::Table(deps)) = target_table.get_mut("build-dependencies") {
                        self.update_deps_table(deps, registry, &mut result, &content)
                            .await;
                    }
                }
            }
        }

        if !result.updated.is_empty() && !dry_run {
            fs::write(path, doc.to_string())?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::CargoToml
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
