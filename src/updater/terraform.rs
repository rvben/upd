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

pub struct TerraformUpdater {
    /// Matches source = "namespace/type" or source = "namespace/name/provider"
    source_re: Regex,
    /// Matches version = "constraint"
    version_re: Regex,
}

/// Parsed Terraform dependency (provider or module)
struct ParsedTerraformDep {
    /// The source identifier (e.g., "hashicorp/aws" or "terraform-aws-modules/vpc/aws")
    source: String,
    /// The version constraint operator (e.g., "~>", ">=", ""), empty for exact versions
    operator: String,
    /// The version number (e.g., "5.0", "5.1.0")
    version: String,
    /// Line number where the version attribute appears (0-indexed)
    version_line_idx: usize,
}

impl TerraformUpdater {
    pub fn new() -> Self {
        let source_re = Regex::new(r#"^\s*source\s*=\s*"([^"]+)""#).expect("Invalid regex");
        let version_re =
            Regex::new(r#"^\s*version\s*=\s*"(~>\s*|>=\s*|<=\s*|>\s*|<\s*|=\s*|!=\s*)?([^"]+)""#)
                .expect("Invalid regex");

        Self {
            source_re,
            version_re,
        }
    }

    fn parse_content(&self, content: &str) -> Vec<ParsedTerraformDep> {
        let lines: Vec<&str> = content.lines().collect();
        let mut deps = Vec::new();

        // Track block nesting for required_providers and module blocks
        let mut in_required_providers = false;
        let mut provider_source: Option<(String, usize)> = None; // (source, depth when found)
        let mut module_source: Option<String> = None;
        let mut in_module_block = false;
        let mut brace_depth: i32 = 0;
        let mut required_providers_depth: i32 = 0;
        let mut module_depth: i32 = 0;
        let mut provider_block_depth: i32 = 0;

        for (line_idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();

            // Skip comments
            if trimmed.starts_with('#') || trimmed.starts_with("//") {
                continue;
            }

            // Count braces on this line
            let open_braces = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let close_braces = trimmed.chars().filter(|&c| c == '}').count() as i32;

            // Detect required_providers block
            if trimmed.contains("required_providers") && trimmed.contains('{') {
                in_required_providers = true;
                required_providers_depth = brace_depth;
            }

            // Detect module block
            if trimmed.starts_with("module ") && trimmed.contains('{') && !in_module_block {
                in_module_block = true;
                module_depth = brace_depth;
                module_source = None;
            }

            // Update brace depth
            brace_depth += open_braces - close_braces;

            // Check if we've exited required_providers block
            if in_required_providers && brace_depth <= required_providers_depth {
                in_required_providers = false;
                provider_source = None;
            }

            // Check if we've exited module block
            if in_module_block && brace_depth <= module_depth {
                in_module_block = false;
                module_source = None;
            }

            // Inside required_providers: look for source and version
            if in_required_providers {
                if let Some(caps) = self.source_re.captures(line) {
                    let source = caps.get(1).unwrap().as_str().to_string();
                    // Only track registry sources (namespace/type format)
                    if source.contains('/')
                        && !source.starts_with("./")
                        && !source.starts_with("../")
                    {
                        provider_source = Some((source, brace_depth as usize));
                        provider_block_depth = brace_depth;
                    }
                }

                if let Some(caps) = self.version_re.captures(line)
                    && let Some((ref source, _)) = provider_source
                    && brace_depth >= provider_block_depth
                {
                    let operator = caps
                        .get(1)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default();
                    let version = caps.get(2).unwrap().as_str().trim().to_string();

                    deps.push(ParsedTerraformDep {
                        source: source.clone(),
                        operator,
                        version,
                        version_line_idx: line_idx,
                    });
                }

                // Reset provider source when exiting a provider's block
                if let Some((_, depth)) = &provider_source
                    && (brace_depth as usize) < *depth
                {
                    provider_source = None;
                }
            }

            // Inside module block: look for source and version
            if in_module_block {
                if let Some(caps) = self.source_re.captures(line) {
                    let source = caps.get(1).unwrap().as_str().to_string();
                    // Skip local and git sources
                    if source.starts_with("./")
                        || source.starts_with("../")
                        || source.starts_with("git::")
                    {
                        continue;
                    }
                    // Only track registry module sources (namespace/name/provider format)
                    if source.split('/').count() == 3 {
                        module_source = Some(source);
                    }
                }

                if let Some(caps) = self.version_re.captures(line)
                    && let Some(ref source) = module_source
                {
                    let operator = caps
                        .get(1)
                        .map(|m| m.as_str().trim().to_string())
                        .unwrap_or_default();
                    let version = caps.get(2).unwrap().as_str().trim().to_string();

                    deps.push(ParsedTerraformDep {
                        source: source.clone(),
                        operator,
                        version,
                        version_line_idx: line_idx,
                    });
                }
            }
        }

        deps
    }

    fn update_line(&self, line: &str, old_version: &str, new_version: &str) -> String {
        line.replacen(old_version, new_version, 1)
    }

    /// Check if the constraint has an upper bound that requires constraint-aware lookup
    fn has_upper_bound(operator: &str) -> bool {
        matches!(operator, "~>" | "<" | "<=" | "!=")
    }
}

impl Default for TerraformUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for TerraformUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut result = UpdateResult::default();

        let lines: Vec<&str> = content.lines().collect();
        let parsed_deps = self.parse_content(&content);

        // Separate into ignored, pinned, and to-be-fetched
        let mut ignored_packages: Vec<(usize, String, String)> = Vec::new();
        let mut pinned_packages: Vec<(usize, String, String, String)> = Vec::new();
        let mut fetch_deps: Vec<(usize, &ParsedTerraformDep)> = Vec::new();

        for (idx, dep) in parsed_deps.iter().enumerate() {
            if options.should_ignore(&dep.source) {
                ignored_packages.push((
                    dep.version_line_idx,
                    dep.source.clone(),
                    dep.version.clone(),
                ));
                continue;
            }

            if let Some(pinned_version) = options.get_pinned_version(&dep.source) {
                pinned_packages.push((
                    dep.version_line_idx,
                    dep.source.clone(),
                    dep.version.clone(),
                    pinned_version.to_string(),
                ));
                continue;
            }

            fetch_deps.push((idx, dep));
        }

        for (line_idx, package, version) in ignored_packages {
            result.ignored.push((package, version, Some(line_idx + 1)));
        }

        // Deduplicate registry lookups
        let unique_sources: Vec<(String, String, String)> = {
            let mut seen = std::collections::HashSet::new();
            fetch_deps
                .iter()
                .filter_map(|(_, dep)| {
                    if seen.insert(dep.source.clone()) {
                        Some((
                            dep.source.clone(),
                            dep.operator.clone(),
                            dep.version.clone(),
                        ))
                    } else {
                        None
                    }
                })
                .collect()
        };

        let version_futures: Vec<_> = unique_sources
            .iter()
            .map(|(name, operator, version)| async move {
                if Self::has_upper_bound(operator) {
                    let constraint = if operator.is_empty() {
                        format!("= {}", version)
                    } else {
                        format!("{} {}", operator, version)
                    };
                    registry
                        .get_latest_version_matching(name, &constraint)
                        .await
                } else {
                    registry.get_latest_version(name).await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Build a map from source -> latest version result
        let source_versions: HashMap<String, Result<String, String>> = unique_sources
            .into_iter()
            .zip(version_results)
            .map(|((name, _, _), result)| (name, result.map_err(|e| e.to_string())))
            .collect();

        // Map results back to every line index that references each source
        let mut version_map: HashMap<usize, Result<String, anyhow::Error>> = HashMap::new();
        for (_, dep) in &fetch_deps {
            if let Some(result) = source_versions.get(&dep.source) {
                match result {
                    Ok(version) => {
                        version_map.insert(dep.version_line_idx, Ok(version.clone()));
                    }
                    Err(e) => {
                        version_map.insert(dep.version_line_idx, Err(anyhow::anyhow!("{}", e)));
                    }
                }
            }
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

        // Build a map from line index to parsed dep for quick lookup
        let dep_by_line: HashMap<usize, &ParsedTerraformDep> = parsed_deps
            .iter()
            .map(|dep| (dep.version_line_idx, dep))
            .collect();

        for (line_idx, line) in lines.iter().enumerate() {
            let line_num = line_idx + 1;

            if let Some(dep) = dep_by_line.get(&line_idx) {
                if let Some(version_result) = version_map.remove(&line_idx) {
                    match version_result {
                        Ok(latest_version) => {
                            let matched_version = if options.full_precision {
                                latest_version.clone()
                            } else {
                                match_version_precision(&dep.version, &latest_version)
                            };
                            if matched_version != dep.version {
                                result.updated.push((
                                    dep.source.clone(),
                                    dep.version.clone(),
                                    matched_version.clone(),
                                    Some(line_num),
                                ));
                                new_lines.push(self.update_line(
                                    line,
                                    &dep.version,
                                    &matched_version,
                                ));
                                modified = true;
                            } else {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            }
                        }
                        Err(e) => {
                            result.errors.push(format!("{}: {}", dep.source, e));
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
        file_type == FileType::TerraformTf
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let parsed = self.parse_content(&content);

        Ok(parsed
            .into_iter()
            .map(|dep| ParsedDependency {
                name: dep.source,
                version: dep.version,
                line_number: Some(dep.version_line_idx + 1),
                has_upper_bound: Self::has_upper_bound(&dep.operator),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_required_providers() {
        let updater = TerraformUpdater::new();
        let content = r#"
terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "3.6.0"
    }
  }
}
"#;
        let deps = updater.parse_content(content);
        assert_eq!(deps.len(), 2);

        assert_eq!(deps[0].source, "hashicorp/aws");
        assert_eq!(deps[0].operator, "~>");
        assert_eq!(deps[0].version, "5.0");

        assert_eq!(deps[1].source, "hashicorp/random");
        assert_eq!(deps[1].operator, "");
        assert_eq!(deps[1].version, "3.6.0");
    }

    #[test]
    fn test_parse_module_with_version() {
        let updater = TerraformUpdater::new();
        let content = r#"
module "vpc" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "5.1.0"

  name = "my-vpc"
}
"#;
        let deps = updater.parse_content(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].source, "terraform-aws-modules/vpc/aws");
        assert_eq!(deps[0].operator, "");
        assert_eq!(deps[0].version, "5.1.0");
    }

    #[test]
    fn test_skips_local_modules() {
        let updater = TerraformUpdater::new();
        let content = r#"
module "local" {
  source  = "./modules/my-module"
  version = "1.0.0"
}

module "parent" {
  source  = "../shared/module"
  version = "1.0.0"
}
"#;
        let deps = updater.parse_content(content);
        assert!(deps.is_empty());
    }

    #[test]
    fn test_skips_git_modules() {
        let updater = TerraformUpdater::new();
        let content = r#"
module "git_module" {
  source  = "git::https://example.com/module.git"
  version = "1.0.0"
}
"#;
        let deps = updater.parse_content(content);
        assert!(deps.is_empty());
    }

    #[test]
    fn test_skips_without_version() {
        let updater = TerraformUpdater::new();
        let content = r#"
terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
    }
  }
}

module "no_version" {
  source = "terraform-aws-modules/vpc/aws"
  name   = "test"
}
"#;
        let deps = updater.parse_content(content);
        assert!(deps.is_empty());
    }

    #[tokio::test]
    async fn test_update_tf_file() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"terraform {{
  required_providers {{
    aws = {{
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }}
    random = {{
      source  = "hashicorp/random"
      version = "3.5.0"
    }}
  }}
}}

module "vpc" {{
  source  = "terraform-aws-modules/vpc/aws"
  version = "5.0.0"
}}
"#
        )
        .unwrap();

        let registry = MockRegistry::new("terraform")
            .with_constrained("hashicorp/aws", "~> 5.0", "5.83.0")
            .with_version("hashicorp/random", "3.7.0")
            .with_version("terraform-aws-modules/vpc/aws", "5.16.0");

        let updater = TerraformUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 3);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("~> 5.83"));
        assert!(contents.contains("3.7.0"));
        assert!(contents.contains("5.16.0"));
    }

    #[test]
    fn test_preserves_constraint_operator() {
        let updater = TerraformUpdater::new();

        let result = updater.update_line(r#"      version = "~> 5.0""#, "5.0", "5.83");
        assert_eq!(result, r#"      version = "~> 5.83""#);

        let result = updater.update_line(r#"      version = ">= 4.9.0""#, "4.9.0", "4.10.0");
        assert_eq!(result, r#"      version = ">= 4.10.0""#);
    }

    #[tokio::test]
    async fn test_dry_run() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"terraform {{
  required_providers {{
    aws = {{
      source  = "hashicorp/aws"
      version = "5.0.0"
    }}
  }}
}}
"#
        )
        .unwrap();

        let registry = MockRegistry::new("terraform").with_version("hashicorp/aws", "5.83.0");

        let updater = TerraformUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified in dry-run mode
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("5.0.0"));
    }

    #[tokio::test]
    async fn test_config_ignore_and_pin() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"terraform {{
  required_providers {{
    aws = {{
      source  = "hashicorp/aws"
      version = "5.0.0"
    }}
    random = {{
      source  = "hashicorp/random"
      version = "3.5.0"
    }}
    null = {{
      source  = "hashicorp/null"
      version = "3.1.0"
    }}
  }}
}}
"#
        )
        .unwrap();

        let registry = MockRegistry::new("terraform")
            .with_version("hashicorp/aws", "5.83.0")
            .with_version("hashicorp/random", "3.7.0")
            .with_version("hashicorp/null", "3.2.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("hashicorp/random".to_string(), "3.6.0".to_string());
        let config = UpdConfig {
            ignore: vec!["hashicorp/aws".to_string()],
            pin: pins,
        };

        let updater = TerraformUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "hashicorp/aws");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "hashicorp/random");
        // Pinned + null should be updated
        assert_eq!(result.updated.len(), 2);
        let updated_names: Vec<&str> = result
            .updated
            .iter()
            .map(|(n, _, _, _)| n.as_str())
            .collect();
        assert!(updated_names.contains(&"hashicorp/random"));
        assert!(updated_names.contains(&"hashicorp/null"));
    }

    #[test]
    fn test_handles() {
        let updater = TerraformUpdater::new();
        assert!(updater.handles(FileType::TerraformTf));
        assert!(!updater.handles(FileType::Requirements));
        assert!(!updater.handles(FileType::CargoToml));
    }
}
