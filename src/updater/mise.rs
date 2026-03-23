use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, read_file_safe,
    write_file_atomic,
};
use crate::registry::Registry;
use crate::version::match_version_precision;
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;

pub struct MiseUpdater {
    /// Matches `tool = "version"` lines in .mise.toml [tools] section
    toml_tool_re: Regex,
    /// Matches `tool version` lines in .tool-versions
    tool_versions_re: Regex,
    /// Matches TOML section headers like [tools], [settings], etc.
    section_re: Regex,
}

/// Map a mise/asdf tool name to its GitHub `owner/repo` for release lookups
fn tool_to_github_repo(tool: &str) -> Option<&'static str> {
    match tool {
        "node" | "nodejs" => Some("nodejs/node"),
        "deno" => Some("denoland/deno"),
        "bun" => Some("oven-sh/bun"),
        "zig" => Some("ziglang/zig"),
        "go" | "golang" => Some("golang/go"),
        "python" => Some("python/cpython"),
        "ruby" => Some("ruby/ruby"),
        "rust" => Some("rust-lang/rust"),
        "terraform" => Some("hashicorp/terraform"),
        "kubectl" => Some("kubernetes/kubernetes"),
        "helm" => Some("helm/helm"),
        "just" => Some("casey/just"),
        "ripgrep" | "rg" => Some("BurntSushi/ripgrep"),
        "fd" => Some("sharkdp/fd"),
        "bat" => Some("sharkdp/bat"),
        "jq" => Some("jqlang/jq"),
        "yq" => Some("mikefarah/yq"),
        "shellcheck" => Some("koalaman/shellcheck"),
        "shfmt" => Some("mvdan/sh"),
        "hugo" => Some("gohugoio/hugo"),
        "act" => Some("nektos/act"),
        "uv" => Some("astral-sh/uv"),
        "ruff" => Some("astral-sh/ruff"),
        _ => None,
    }
}

/// Strip tool-specific version prefixes from GitHub release tags.
/// GitHub tags often have prefixes like `v1.0.0` or `go1.22.1`,
/// but mise/asdf versions are typically bare (e.g., `1.0.0`, `1.22.1`).
fn strip_tool_version_prefix<'a>(tool: &str, version: &'a str) -> &'a str {
    match tool {
        "go" | "golang" => version.strip_prefix("go").unwrap_or(version),
        _ => version.strip_prefix('v').unwrap_or(version),
    }
}

impl MiseUpdater {
    pub fn new() -> Self {
        // Match: tool_name = "version" (with optional quotes around tool name)
        let toml_tool_re =
            Regex::new(r#"^"?([^"=\s]+)"?\s*=\s*"([^"]+)""#).expect("Invalid toml_tool regex");
        // Match: tool_name version (space-delimited)
        let tool_versions_re = Regex::new(r"^(\S+)\s+(\S+)").expect("Invalid tool_versions regex");
        // Match TOML section headers
        let section_re = Regex::new(r"^\[([^\]]+)\]").expect("Invalid section regex");
        Self {
            toml_tool_re,
            tool_versions_re,
            section_re,
        }
    }

    /// Parse dependencies from .mise.toml content
    fn parse_mise_toml(&self, content: &str) -> Vec<ParsedDependency> {
        let mut deps = Vec::new();
        let mut in_tools_section = false;

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            // Check for section headers
            if let Some(caps) = self.section_re.captures(trimmed) {
                let section = caps.get(1).unwrap().as_str().trim();
                in_tools_section = section == "tools";
                continue;
            }

            if !in_tools_section {
                continue;
            }

            // Parse tool = "version" lines
            if let Some(caps) = self.toml_tool_re.captures(trimmed) {
                let tool = caps.get(1).unwrap().as_str();
                let version = caps.get(2).unwrap().as_str();

                // Skip cargo: prefixed tools
                if tool.starts_with("cargo:") {
                    continue;
                }

                // Skip "latest" versions
                if version == "latest" {
                    continue;
                }

                // Only include tools we can look up
                if tool_to_github_repo(tool).is_some() {
                    deps.push(ParsedDependency {
                        name: tool.to_string(),
                        version: version.to_string(),
                        line_number: Some(line_idx + 1),
                        has_upper_bound: false,
                    });
                }
            }
        }

        deps
    }

    /// Parse dependencies from .tool-versions content
    fn parse_tool_versions(&self, content: &str) -> Vec<ParsedDependency> {
        let mut deps = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Skip empty lines and comments
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if let Some(caps) = self.tool_versions_re.captures(trimmed) {
                let tool = caps.get(1).unwrap().as_str();
                let version = caps.get(2).unwrap().as_str();

                // Skip "latest" versions
                if version == "latest" {
                    continue;
                }

                // Only include tools we can look up
                if tool_to_github_repo(tool).is_some() {
                    deps.push(ParsedDependency {
                        name: tool.to_string(),
                        version: version.to_string(),
                        line_number: Some(line_idx + 1),
                        has_upper_bound: false,
                    });
                }
            }
        }

        deps
    }

    /// Parse dependencies based on file type
    fn parse_content(&self, content: &str, file_type: FileType) -> Vec<ParsedDependency> {
        match file_type {
            FileType::MiseToml => self.parse_mise_toml(content),
            FileType::ToolVersions => self.parse_tool_versions(content),
            _ => Vec::new(),
        }
    }

    /// Compute the updated version string, preserving precision
    fn compute_updated_version(
        tool: &str,
        current: &str,
        latest_tag: &str,
        full_precision: bool,
    ) -> String {
        let stripped = strip_tool_version_prefix(tool, latest_tag);

        if full_precision {
            stripped.to_string()
        } else {
            match_version_precision(current, stripped)
        }
    }
}

impl Default for MiseUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for MiseUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let file_type = FileType::detect(path).unwrap_or(FileType::MiseToml);
        let mut result = UpdateResult::default();

        // Pass 1: Collect tools to check
        let deps = self.parse_content(&content, file_type);

        let mut ignored_tools: Vec<(usize, String, String)> = Vec::new();
        let mut pinned_tools: Vec<(usize, String, String, String)> = Vec::new();
        let mut tools_to_check: Vec<(usize, String, String)> = Vec::new();

        for dep in deps {
            let line_idx = dep.line_number.map(|n| n - 1).unwrap_or(0);

            if options.should_ignore(&dep.name) {
                ignored_tools.push((line_idx, dep.name, dep.version));
            } else if let Some(pinned_version) = options.get_pinned_version(&dep.name) {
                pinned_tools.push((line_idx, dep.name, dep.version, pinned_version.to_string()));
            } else {
                tools_to_check.push((line_idx, dep.name, dep.version));
            }
        }

        // Record ignored tools
        for (line_idx, tool_name, version) in ignored_tools {
            result
                .ignored
                .push((tool_name, version, Some(line_idx + 1)));
        }

        // Pass 2: Fetch versions in parallel (deduplicated)
        let unique_tools: Vec<(String, String)> = {
            let mut seen = std::collections::HashSet::new();
            tools_to_check
                .iter()
                .filter_map(|(_, tool_name, _)| {
                    if seen.insert(tool_name.clone()) {
                        // Map tool name to GitHub repo for lookup
                        tool_to_github_repo(tool_name)
                            .map(|repo| (tool_name.clone(), repo.to_string()))
                    } else {
                        None
                    }
                })
                .collect()
        };

        let version_futures: Vec<_> = unique_tools
            .iter()
            .map(|(_, repo)| async { registry.get_latest_version(repo).await })
            .collect();

        let version_results = join_all(version_futures).await;

        let tool_versions: HashMap<String, Result<String, String>> = unique_tools
            .into_iter()
            .zip(version_results)
            .map(|((tool_name, _), result)| (tool_name, result.map_err(|e| e.to_string())))
            .collect();

        // Build version map per line index
        let mut version_map: HashMap<usize, Result<String, anyhow::Error>> = HashMap::new();
        for (line_idx, tool_name, _) in &tools_to_check {
            if let Some(result) = tool_versions.get(tool_name) {
                match result {
                    Ok(version) => {
                        version_map.insert(*line_idx, Ok(version.clone()));
                    }
                    Err(e) => {
                        version_map.insert(*line_idx, Err(anyhow::anyhow!("{}", e)));
                    }
                }
            }
        }

        // Add pinned versions
        for (line_idx, _, _, pinned_version) in &pinned_tools {
            version_map.insert(*line_idx, Ok(pinned_version.clone()));
        }

        // Build tool info map: line_idx -> (tool_name, current_version, is_pinned)
        let mut tool_info: HashMap<usize, (String, String, bool)> = tools_to_check
            .into_iter()
            .map(|(idx, tool_name, version)| (idx, (tool_name, version, false)))
            .collect();

        for (line_idx, tool_name, current_version, _) in pinned_tools {
            tool_info.insert(line_idx, (tool_name, current_version, true));
        }

        // Pass 3: Apply updates
        let mut new_lines: Vec<String> = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            let line_num = line_idx + 1;

            if let Some(version_result) = version_map.remove(&line_idx) {
                let Some((tool_name, current_version, is_pinned)) = tool_info.get(&line_idx) else {
                    new_lines.push(line.to_string());
                    continue;
                };

                match version_result {
                    Ok(latest_tag) => {
                        let new_version = Self::compute_updated_version(
                            tool_name,
                            current_version,
                            &latest_tag,
                            options.full_precision,
                        );

                        if new_version != *current_version {
                            let new_line = line.replacen(current_version, &new_version, 1);
                            new_lines.push(new_line);

                            if *is_pinned {
                                result.pinned.push((
                                    tool_name.clone(),
                                    current_version.clone(),
                                    new_version,
                                    Some(line_num),
                                ));
                            } else {
                                result.updated.push((
                                    tool_name.clone(),
                                    current_version.clone(),
                                    new_version,
                                    Some(line_num),
                                ));
                            }
                        } else {
                            new_lines.push(line.to_string());
                            result.unchanged += 1;
                        }
                    }
                    Err(e) => {
                        new_lines.push(line.to_string());
                        result.errors.push(format!("{}: {}", tool_name, e));
                    }
                }
            } else {
                new_lines.push(line.to_string());
            }
        }

        if (!result.updated.is_empty() || !result.pinned.is_empty()) && !options.dry_run {
            let line_ending = if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let new_content = new_lines.join(line_ending);

            let final_content = if content.ends_with('\n') && !new_content.ends_with('\n') {
                format!("{}{}", new_content, line_ending)
            } else {
                new_content
            };

            write_file_atomic(path, &final_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::MiseToml || file_type == FileType::ToolVersions
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let file_type = FileType::detect(path).unwrap_or(FileType::MiseToml);
        Ok(self.parse_content(&content, file_type))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_tool_to_github_repo() {
        assert_eq!(tool_to_github_repo("node"), Some("nodejs/node"));
        assert_eq!(tool_to_github_repo("nodejs"), Some("nodejs/node"));
        assert_eq!(tool_to_github_repo("deno"), Some("denoland/deno"));
        assert_eq!(tool_to_github_repo("bun"), Some("oven-sh/bun"));
        assert_eq!(tool_to_github_repo("zig"), Some("ziglang/zig"));
        assert_eq!(tool_to_github_repo("go"), Some("golang/go"));
        assert_eq!(tool_to_github_repo("golang"), Some("golang/go"));
        assert_eq!(tool_to_github_repo("python"), Some("python/cpython"));
        assert_eq!(tool_to_github_repo("rust"), Some("rust-lang/rust"));
        assert_eq!(tool_to_github_repo("uv"), Some("astral-sh/uv"));
        assert_eq!(tool_to_github_repo("ruff"), Some("astral-sh/ruff"));
        assert_eq!(tool_to_github_repo("unknown-tool"), None);
        assert_eq!(tool_to_github_repo(""), None);
    }

    #[test]
    fn test_strip_tool_version_prefix() {
        // Go uses "go" prefix
        assert_eq!(strip_tool_version_prefix("go", "go1.22.1"), "1.22.1");
        assert_eq!(strip_tool_version_prefix("golang", "go1.22.1"), "1.22.1");

        // Most tools use "v" prefix
        assert_eq!(strip_tool_version_prefix("node", "v20.11.0"), "20.11.0");
        assert_eq!(strip_tool_version_prefix("python", "v3.12.2"), "3.12.2");
        assert_eq!(strip_tool_version_prefix("rust", "v1.91.1"), "1.91.1");

        // No prefix passes through
        assert_eq!(strip_tool_version_prefix("node", "20.11.0"), "20.11.0");
        assert_eq!(strip_tool_version_prefix("go", "1.22.1"), "1.22.1");
    }

    #[test]
    fn test_parse_mise_toml() {
        let updater = MiseUpdater::new();
        let content = r#"
[env]
RUST_BACKTRACE = "1"

[tools]
rust = "1.91.1"
python = "3.12"
uv = "latest"
"cargo:maturin" = "latest"
zig = "0.13"
node = "20.11.0"

[settings]
cargo_binstall = true
"#;
        let deps = updater.parse_mise_toml(content);
        assert_eq!(deps.len(), 4);
        assert_eq!(deps[0].name, "rust");
        assert_eq!(deps[0].version, "1.91.1");
        assert_eq!(deps[1].name, "python");
        assert_eq!(deps[1].version, "3.12");
        assert_eq!(deps[2].name, "zig");
        assert_eq!(deps[2].version, "0.13");
        assert_eq!(deps[3].name, "node");
        assert_eq!(deps[3].version, "20.11.0");
    }

    #[test]
    fn test_parse_mise_toml_skips_latest() {
        let updater = MiseUpdater::new();
        let content = r#"
[tools]
uv = "latest"
rust = "1.91.1"
"#;
        let deps = updater.parse_mise_toml(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "rust");
    }

    #[test]
    fn test_parse_mise_toml_skips_cargo_prefix() {
        let updater = MiseUpdater::new();
        let content = r#"
[tools]
"cargo:maturin" = "1.0.0"
"cargo:cargo-zigbuild" = "latest"
rust = "1.91.1"
"#;
        let deps = updater.parse_mise_toml(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "rust");
    }

    #[test]
    fn test_parse_mise_toml_skips_unmapped_tools() {
        let updater = MiseUpdater::new();
        let content = r#"
[tools]
rust = "1.91.1"
some-obscure-tool = "2.0.0"
"#;
        let deps = updater.parse_mise_toml(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "rust");
    }

    #[test]
    fn test_parse_tool_versions() {
        let updater = MiseUpdater::new();
        let content = r#"# Development tools
node 20.11.0
python 3.12.2
golang 1.22.1
rust 1.91.1
"#;
        let deps = updater.parse_tool_versions(content);
        assert_eq!(deps.len(), 4);
        assert_eq!(deps[0].name, "node");
        assert_eq!(deps[0].version, "20.11.0");
        assert_eq!(deps[1].name, "python");
        assert_eq!(deps[1].version, "3.12.2");
        assert_eq!(deps[2].name, "golang");
        assert_eq!(deps[2].version, "1.22.1");
        assert_eq!(deps[3].name, "rust");
        assert_eq!(deps[3].version, "1.91.1");
    }

    #[test]
    fn test_parse_tool_versions_skips_comments_and_empty() {
        let updater = MiseUpdater::new();
        let content = r#"
# This is a comment
node 20.11.0

# Another comment
python 3.12.2
"#;
        let deps = updater.parse_tool_versions(content);
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn test_parse_tool_versions_skips_unmapped() {
        let updater = MiseUpdater::new();
        let content = "node 20.11.0\nunknown-tool 1.0.0\n";
        let deps = updater.parse_tool_versions(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "node");
    }

    #[test]
    fn test_parse_tool_versions_skips_latest() {
        let updater = MiseUpdater::new();
        let content = "node latest\npython 3.12.2\n";
        let deps = updater.parse_tool_versions(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "python");
    }

    #[test]
    fn test_compute_updated_version_strips_v_prefix() {
        assert_eq!(
            MiseUpdater::compute_updated_version("node", "20.11.0", "v22.5.0", false),
            "22.5.0"
        );
    }

    #[test]
    fn test_compute_updated_version_strips_go_prefix() {
        assert_eq!(
            MiseUpdater::compute_updated_version("go", "1.22.1", "go1.23.0", false),
            "1.23.0"
        );
        assert_eq!(
            MiseUpdater::compute_updated_version("golang", "1.22", "go1.23.0", false),
            "1.23"
        );
    }

    #[test]
    fn test_compute_updated_version_preserves_precision() {
        assert_eq!(
            MiseUpdater::compute_updated_version("python", "3.12", "v3.13.2", false),
            "3.13"
        );
        assert_eq!(
            MiseUpdater::compute_updated_version("python", "3.12.2", "v3.13.2", false),
            "3.13.2"
        );
    }

    #[test]
    fn test_compute_updated_version_full_precision() {
        assert_eq!(
            MiseUpdater::compute_updated_version("python", "3.12", "v3.13.2", true),
            "3.13.2"
        );
    }

    #[tokio::test]
    async fn test_update_mise_toml() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join(".mise.toml");
        fs::write(
            &file_path,
            r#"[tools]
rust = "1.90.0"
python = "3.12"
uv = "latest"
"#,
        )
        .unwrap();

        // The registry receives GitHub repo names and returns tags
        let registry = MockRegistry::new("github-releases")
            .with_version("rust-lang/rust", "v1.91.1")
            .with_version("python/cpython", "v3.13.2");

        let updater = MiseUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(&file_path, &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains(r#"rust = "1.91.1""#));
        assert!(content.contains(r#"python = "3.13""#)); // precision preserved
        assert!(content.contains(r#"uv = "latest""#)); // unchanged
    }

    #[tokio::test]
    async fn test_update_tool_versions() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join(".tool-versions");
        fs::write(&file_path, "node 20.11.0\npython 3.12.2\ngolang 1.22.1\n").unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("nodejs/node", "v22.5.0")
            .with_version("python/cpython", "v3.13.2")
            .with_version("golang/go", "go1.23.0");

        let updater = MiseUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(&file_path, &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 3);

        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("node 22.5.0"));
        assert!(content.contains("python 3.13.2"));
        assert!(content.contains("golang 1.23.0"));
    }

    #[tokio::test]
    async fn test_dry_run_mise_toml() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join(".mise.toml");
        let original = r#"[tools]
rust = "1.90.0"
"#;
        fs::write(&file_path, original).unwrap();

        let registry =
            MockRegistry::new("github-releases").with_version("rust-lang/rust", "v1.91.1");

        let updater = MiseUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(&file_path, &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, original);
    }
}
