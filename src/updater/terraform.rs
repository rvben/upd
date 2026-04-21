use super::{
    FileType, ParsedDependency, PendingVersion, UpdateOptions, UpdateResult, Updater,
    downgrade_warning, read_file_safe, write_file_atomic,
};
use crate::align::compare_versions;
use crate::registry::Registry;
use crate::updater::Lang;
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

    /// Computes the new `~>` constraint version when the existing constraint no longer
    /// covers `latest` (i.e., `pessimistic_constraint_satisfied` returned `false`).
    ///
    /// The new constraint version is anchored at the same precision as the original,
    /// but with the "pinned prefix" taken from `latest` and the variable tail zeroed:
    ///
    /// - `~> X.Y`   (2 components): `latest_major.0`
    ///   e.g. constraint `4.0`, latest `5.2.1` → `5.0`
    /// - `~> X.Y.Z` (3 components): `latest_major.latest_minor.0`
    ///   e.g. constraint `4.0.5`, latest `4.1.3` → `4.1.0`
    ///
    /// The trailing zero preserves the "start of range" semantics: the constraint
    /// allows any release in the new series, not just from the specific latest version.
    fn pessimistic_constraint_new_version(constraint_version: &str, latest: &str) -> String {
        let constraint_parts: Vec<u64> = constraint_version
            .split('.')
            .filter_map(|s| s.parse().ok())
            .collect();
        let latest_parts: Vec<u64> = latest.split('.').filter_map(|s| s.parse().ok()).collect();

        let precision = constraint_parts.len();

        // Build a new version with `precision` components: take the first (precision - 1)
        // components from latest and set the last component to 0.
        let mut new_parts: Vec<String> = latest_parts
            .iter()
            .take(precision.saturating_sub(1))
            .map(|n| n.to_string())
            .collect();

        // Pad with zeros if latest has fewer components than needed
        while new_parts.len() < precision.saturating_sub(1) {
            new_parts.push("0".to_string());
        }

        new_parts.push("0".to_string());
        new_parts.join(".")
    }

    /// Returns `true` when `latest` falls within the range implied by `~> constraint_version`.
    ///
    /// Terraform's pessimistic-constraint operator `~>` pins all but the rightmost
    /// component of the version and allows any version up to (but not including)
    /// the next increment of the second-to-rightmost component:
    ///
    /// - `~> X.Y`   → `>= X.Y, < X+1.0.0`   (any `X.*`)
    /// - `~> X.Y.Z` → `>= X.Y.Z, < X.Y+1.0` (any `X.Y.*`)
    ///
    /// If the existing constraint already covers `latest`, updating the constraint
    /// would silently raise its floor and block rollback to earlier patch versions.
    fn pessimistic_constraint_satisfied(constraint_version: &str, latest: &str) -> bool {
        let constraint_parts: Vec<u64> = constraint_version
            .split('.')
            .filter_map(|s| s.parse().ok())
            .collect();
        let latest_parts: Vec<u64> = latest.split('.').filter_map(|s| s.parse().ok()).collect();

        if constraint_parts.is_empty() || latest_parts.is_empty() {
            return false;
        }

        match constraint_parts.len() {
            // ~> X.Y.Z or more: all components except the last must match
            n if n >= 3 => {
                // The pinned prefix is all components except the last one.
                // ~> X.Y.Z means >= X.Y.Z, < X.Y+1.0, so the prefix to lock is [X, Y].
                let pinned_len = n - 1;
                constraint_parts[..pinned_len]
                    == *latest_parts.get(..pinned_len).unwrap_or(&latest_parts[..])
            }
            // ~> X.Y: only the major must match
            2 => latest_parts.first() == constraint_parts.first(),
            // ~> X: major must match (uncommon but handle gracefully)
            _ => latest_parts.first() == constraint_parts.first(),
        }
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
            if options.is_package_filtered_out(&dep.source) {
                result.unchanged += 1;
                continue;
            }

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
        let mut version_map: HashMap<usize, PendingVersion> = HashMap::new();
        for (_, dep) in &fetch_deps {
            if let Some(result) = source_versions.get(&dep.source) {
                match result {
                    Ok(version) => {
                        version_map.insert(
                            dep.version_line_idx,
                            PendingVersion::Registry(Ok(version.clone())),
                        );
                    }
                    Err(e) => {
                        version_map.insert(
                            dep.version_line_idx,
                            PendingVersion::Registry(Err(anyhow::anyhow!("{}", e))),
                        );
                    }
                }
            }
        }

        for (line_idx, _package, _current_version, pinned_version) in pinned_packages {
            version_map.insert(line_idx, PendingVersion::Pinned(pinned_version));
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
                        PendingVersion::Pinned(pinned_version) => {
                            let matched_version = if options.full_precision {
                                pinned_version.clone()
                            } else {
                                match_version_precision(&dep.version, &pinned_version)
                            };
                            if matched_version != dep.version {
                                result.pinned.push((
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
                        PendingVersion::Registry(Ok(latest_version)) => {
                            // For `~>` constraints, if the latest version still falls within
                            // the range the constraint already expresses, leave the constraint
                            // untouched. Rewriting it would silently raise the floor (e.g.
                            // `~> 4.0` → `~> 4.67`) and block rollback to earlier releases.
                            if dep.operator == "~>"
                                && Self::pessimistic_constraint_satisfied(
                                    &dep.version,
                                    &latest_version,
                                )
                            {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                                continue;
                            }

                            // For ~> constraints that need bumping, anchor the new
                            // constraint at the start of the new series (e.g. `5.0`)
                            // rather than the exact latest (e.g. `5.2`), so the full
                            // new major/minor range remains accessible.
                            let matched_version = if dep.operator == "~>" {
                                Self::pessimistic_constraint_new_version(
                                    &dep.version,
                                    &latest_version,
                                )
                            } else if options.full_precision {
                                latest_version.clone()
                            } else {
                                match_version_precision(&dep.version, &latest_version)
                            };
                            if matched_version != dep.version {
                                // Refuse to write a downgrade.
                                if compare_versions(&matched_version, &dep.version, Lang::Terraform)
                                    != std::cmp::Ordering::Greater
                                {
                                    result.warnings.push(downgrade_warning(
                                        &dep.source,
                                        &matched_version,
                                        &dep.version,
                                    ));
                                    result.unchanged += 1;
                                    new_lines.push(line.to_string());
                                } else {
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
                                }
                            } else {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            }
                        }
                        PendingVersion::Registry(Err(e)) => {
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
                is_bumpable: true,
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

        // hashicorp/aws uses ~> 5.0 and latest 5.83.0 satisfies that constraint —
        // the constraint floor must not be raised, so aws stays unchanged.
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 1);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("~> 5.0"),
            "~> constraint must not be raised when already satisfied"
        );
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
        assert_eq!(result.updated.len(), 1);
        let updated_names: Vec<&str> = result
            .updated
            .iter()
            .map(|(n, _, _, _)| n.as_str())
            .collect();
        assert!(updated_names.contains(&"hashicorp/null"));
        assert!(!updated_names.contains(&"hashicorp/random"));
    }

    #[test]
    fn test_handles() {
        let updater = TerraformUpdater::new();
        assert!(updater.handles(FileType::TerraformTf));
        assert!(!updater.handles(FileType::Requirements));
        assert!(!updater.handles(FileType::CargoToml));
    }

    #[test]
    fn test_pessimistic_constraint_satisfied_two_components() {
        // ~> 4.0 allows >= 4.0, < 5.0 — any 4.x satisfies it
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0", "4.67.1"
        ));
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0", "4.0.0"
        ));
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0", "4.99.99"
        ));
        // Major version changed — no longer satisfied
        assert!(!TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0", "5.0.0"
        ));
        assert!(!TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0", "5.2.1"
        ));
        assert!(!TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0", "3.99.0"
        ));
    }

    #[test]
    fn test_pessimistic_constraint_satisfied_three_components() {
        // ~> 4.0.5 allows >= 4.0.5, < 4.1.0 — any 4.0.x satisfies it
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0.5", "4.0.9"
        ));
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0.5", "4.0.5"
        ));
        // Minor version changed — no longer satisfied
        assert!(!TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0.5", "4.1.0"
        ));
        assert!(!TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0.5", "5.0.0"
        ));
    }

    #[tokio::test]
    async fn test_tilde_gt_two_components_no_change_when_satisfied() {
        // ~> 4.0 with latest 4.67.1 — constraint already covers latest, leave untouched
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"terraform {{
  required_providers {{
    aws = {{
      source  = "hashicorp/aws"
      version = "~> 4.0"
    }}
  }}
}}
"#
        )
        .unwrap();

        let registry =
            MockRegistry::new("terraform").with_constrained("hashicorp/aws", "~> 4.0", "4.67.1");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(
            result.updated.len(),
            0,
            "should not update when constraint already satisfied"
        );
        assert_eq!(result.unchanged, 1);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("~> 4.0"), "file must remain unchanged");
    }

    #[tokio::test]
    async fn test_tilde_gt_three_components_no_change_when_satisfied() {
        // ~> 4.0.5 with latest 4.0.9 — same minor, leave untouched
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"terraform {{
  required_providers {{
    null = {{
      source  = "hashicorp/null"
      version = "~> 4.0.5"
    }}
  }}
}}
"#
        )
        .unwrap();

        let registry =
            MockRegistry::new("terraform").with_constrained("hashicorp/null", "~> 4.0.5", "4.0.9");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(
            result.updated.len(),
            0,
            "should not update when constraint already satisfied"
        );
        assert_eq!(result.unchanged, 1);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("~> 4.0.5"), "file must remain unchanged");
    }

    #[tokio::test]
    async fn test_tilde_gt_two_components_bumps_on_major_change() {
        // ~> 4.0 with latest 5.2.1 — major changed, bump to ~> 5.0 (preserve precision)
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"terraform {{
  required_providers {{
    aws = {{
      source  = "hashicorp/aws"
      version = "~> 4.0"
    }}
  }}
}}
"#
        )
        .unwrap();

        let registry =
            MockRegistry::new("terraform").with_constrained("hashicorp/aws", "~> 4.0", "5.2.1");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        // Two-component precision: bump anchors at start of new major series → 5.0
        assert!(
            contents.contains("~> 5.0"),
            "should bump to start of new major with two-component precision"
        );
    }

    #[tokio::test]
    async fn test_tilde_gt_three_components_bumps_on_minor_change() {
        // ~> 4.0.5 with latest 4.1.0 — minor changed, bump to ~> 4.1.0
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"terraform {{
  required_providers {{
    null = {{
      source  = "hashicorp/null"
      version = "~> 4.0.5"
    }}
  }}
}}
"#
        )
        .unwrap();

        let registry =
            MockRegistry::new("terraform").with_constrained("hashicorp/null", "~> 4.0.5", "4.1.0");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("~> 4.1.0"),
            "should bump to new minor with three-component precision"
        );
    }

    #[tokio::test]
    async fn test_exact_pin_still_updates() {
        // Exact pin (no operator) keeps existing behavior: always update to latest
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"terraform {{
  required_providers {{
    aws = {{
      source  = "hashicorp/aws"
      version = "4.0"
    }}
  }}
}}
"#
        )
        .unwrap();

        let registry = MockRegistry::new("terraform").with_version("hashicorp/aws", "4.67.1");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("4.67"),
            "exact pin should update to latest"
        );
    }
}
