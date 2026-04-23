use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, downgrade_warning,
    read_file_safe, write_file_atomic,
};
use crate::align::compare_versions;
use crate::registry::Registry;
use crate::updater::Lang;
use crate::version::match_version_precision;
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub struct GoModUpdater {
    // Matches module path and version in require statements
    // e.g., "github.com/foo/bar v1.2.3" or "github.com/foo/bar v1.2.3 // indirect"
    require_re: Regex,
    // Matches replace directives to identify modules we should skip
    replace_re: Regex,
}

impl GoModUpdater {
    pub fn new() -> Self {
        // Match: module_path version [// comment]
        // Version format: v1.2.3, v1.2.3-alpha, v1.2.3+incompatible
        // Pseudo-versions have multiple dash segments: v0.0.0-YYYYMMDDHHMMSS-abcdef012345
        // The pattern allows zero or more additional dash-separated pre-release segments
        // so that pseudo-versions are captured in their entirety.
        let require_re =
            Regex::new(r"^\s*([\w./-]+)\s+(v\d+\.\d+\.\d+(?:-[\w.]+)*(?:\+incompatible)?)")
                .expect("Invalid require regex");

        // Match replace directives: replace old => new
        // or replace ( ... ) blocks
        let replace_re = Regex::new(r"^\s*([\w./-]+)\s+=>\s+").expect("Invalid replace regex");

        Self {
            require_re,
            replace_re,
        }
    }

    /// Parse go.mod content and extract modules that have replace directives
    fn find_replaced_modules(&self, content: &str) -> HashSet<String> {
        let mut replaced = HashSet::new();
        let mut in_replace_block = false;

        for line in content.lines() {
            let trimmed = line.trim();

            // Handle replace block
            if trimmed.starts_with("replace (") || trimmed == "replace (" {
                in_replace_block = true;
                continue;
            }

            if in_replace_block {
                if trimmed == ")" {
                    in_replace_block = false;
                    continue;
                }
                // Inside replace block: "module => replacement"
                if let Some(caps) = self.replace_re.captures(line) {
                    replaced.insert(caps.get(1).unwrap().as_str().to_string());
                }
                continue;
            }

            // Single-line replace: "replace module => replacement"
            if let Some(rest) = trimmed.strip_prefix("replace ")
                && let Some(caps) = self.replace_re.captures(rest)
            {
                replaced.insert(caps.get(1).unwrap().as_str().to_string());
            }
        }

        replaced
    }

    /// Check if a version is a pre-release
    fn is_prerelease(version: &str) -> bool {
        let stripped = version.strip_prefix('v').unwrap_or(version);
        // Remove +incompatible suffix for parsing
        let without_incompatible = stripped.split('+').next().unwrap_or(stripped);

        semver::Version::parse(without_incompatible)
            .map(|v| !v.pre.is_empty())
            .unwrap_or(false)
    }

    /// Check if a version is a pseudo-version (commit-based, not a real tag).
    /// Pseudo-versions have the format: v0.0.0-YYYYMMDDHHMMSS-abcdefabcdef
    /// Or for pre-release: v1.2.4-0.YYYYMMDDHHMMSS-abcdefabcdef
    /// These modules have no semver tags (or point to commits), so updating them via registry fails.
    fn is_pseudo_version(version: &str) -> bool {
        // Pseudo-version patterns:
        // 1. v0.0.0-20241217172646-ca3f786aa774 (base version is 0.0.0)
        // 2. v1.2.4-0.20220331215641-2d8c0ab7ef04 (pre-release pseudo after real version)
        //
        // Look for timestamp pattern: 14 digits (YYYYMMDDHHMMSS)
        let contains_timestamp = |s: &str| s.len() == 14 && s.chars().all(|c| c.is_ascii_digit());

        // Split by dash and look for the timestamp part
        let parts: Vec<&str> = version.split('-').collect();

        for part in &parts {
            if contains_timestamp(part) {
                return true;
            }
            // Handle "0.20220331215641" format (pre-release pseudo)
            if let Some(after_dot) = part.strip_prefix("0.")
                && contains_timestamp(after_dot)
            {
                return true;
            }
        }

        false
    }
}

impl Default for GoModUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for GoModUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut result = UpdateResult::default();

        // Find modules with replace directives (we'll skip these)
        let replaced_modules = self.find_replaced_modules(&content);

        // First pass: collect all modules and separate by config status
        // Store: (line_idx, module, current_version, is_prerelease)
        let mut ignored_modules: Vec<(usize, String, String)> = Vec::new();
        let mut pinned_modules: Vec<(usize, String, String, String)> = Vec::new();
        let mut modules_to_check: Vec<(usize, String, String, bool)> = Vec::new();
        let mut in_require_block = false;

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Track require block state
            if trimmed.starts_with("require (") || trimmed == "require (" {
                in_require_block = true;
                continue;
            }

            if in_require_block && trimmed == ")" {
                in_require_block = false;
                continue;
            }

            // Check if this line is a require statement (inside block or single-line)
            let is_require_line = in_require_block
                || (trimmed.starts_with("require ") && !trimmed.starts_with("require ("));

            if !is_require_line {
                continue;
            }

            // Try to parse as a module requirement
            let line_to_parse = if in_require_block {
                line
            } else {
                // Single-line require: "require github.com/foo/bar v1.0.0"
                &line[line.find("require").map(|i| i + 7).unwrap_or(0)..]
            };

            if let Some(caps) = self.require_re.captures(line_to_parse) {
                let module = caps.get(1).unwrap().as_str();
                let current_version = caps.get(2).unwrap().as_str();

                // Skip replaced modules and pseudo-versions
                if replaced_modules.contains(module) || Self::is_pseudo_version(current_version) {
                    continue;
                }

                if options.is_package_filtered_out(module) {
                    result.unchanged += 1;
                    continue;
                }

                // Check if module should be ignored
                if options.should_ignore(module) {
                    ignored_modules.push((
                        line_idx,
                        module.to_string(),
                        current_version.to_string(),
                    ));
                    continue;
                }

                // Check if module has a pinned version
                if let Some(pinned_version) = options.get_pinned_version(module) {
                    pinned_modules.push((
                        line_idx,
                        module.to_string(),
                        current_version.to_string(),
                        pinned_version.to_string(),
                    ));
                    continue;
                }

                modules_to_check.push((
                    line_idx,
                    module.to_string(),
                    current_version.to_string(),
                    Self::is_prerelease(current_version),
                ));
            }
        }

        // Record ignored modules
        for (line_idx, module, version) in ignored_modules {
            result.ignored.push((module, version, Some(line_idx + 1)));
        }

        // Fetch all versions in parallel for non-ignored, non-pinned modules
        let version_futures: Vec<_> = modules_to_check
            .iter()
            .map(|(_, module, _, is_prerelease)| async {
                if *is_prerelease {
                    registry
                        .get_latest_version_including_prereleases(module)
                        .await
                } else {
                    registry.get_latest_version(module).await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Build a map of line_idx to version result
        let mut version_map: HashMap<usize, Result<String, anyhow::Error>> = HashMap::new();
        for ((line_idx, _, _, _), version_result) in modules_to_check.iter().zip(version_results) {
            version_map.insert(*line_idx, version_result);
        }

        // Add pinned modules to version_map with their pinned version
        for (line_idx, _, _, pinned_version) in &pinned_modules {
            version_map.insert(*line_idx, Ok(pinned_version.clone()));
        }

        // Create a map from line_idx to (module, current_version, is_pinned) for easy lookup
        let mut module_info: HashMap<usize, (String, String, bool)> = modules_to_check
            .into_iter()
            .map(|(idx, module, version, _)| (idx, (module, version, false)))
            .collect();

        // Add pinned modules to module_info
        for (line_idx, module, current_version, _) in pinned_modules {
            module_info.insert(line_idx, (module, current_version, true));
        }

        // Second pass: apply updates while preserving line structure
        let mut new_lines: Vec<String> = Vec::new();
        in_require_block = false;

        for (line_idx, line) in content.lines().enumerate() {
            let line_num = line_idx + 1; // 1-indexed for display
            let trimmed = line.trim();

            // Track require block state
            if trimmed.starts_with("require (") || trimmed == "require (" {
                in_require_block = true;
                new_lines.push(line.to_string());
                continue;
            }

            if in_require_block && trimmed == ")" {
                in_require_block = false;
                new_lines.push(line.to_string());
                continue;
            }

            // Check if we have a version result for this line
            if let Some(version_result) = version_map.remove(&line_idx) {
                let Some((module, current_version, is_pinned)) = module_info.get(&line_idx) else {
                    new_lines.push(line.to_string());
                    continue;
                };

                match version_result {
                    Ok(latest_version) => {
                        // Apply cooldown policy before writing (registry path only; pins bypass it).
                        let (latest_version, held_back_record) = if *is_pinned {
                            (latest_version, None)
                        } else {
                            let (outcome, note) = crate::updater::apply_cooldown(
                                registry,
                                module,
                                current_version,
                                &latest_version,
                                None,
                                false,
                                &options,
                            )
                            .await;
                            if let Some(msg) = note {
                                options.note_cooldown_unavailable(&msg);
                            }
                            match outcome {
                                crate::updater::CooldownOutcome::Unchanged(v) => (v, None),
                                crate::updater::CooldownOutcome::HeldBack {
                                    chosen,
                                    skipped_version,
                                    skipped_published_at,
                                } => (chosen, Some((skipped_version, skipped_published_at))),
                                crate::updater::CooldownOutcome::Skipped {
                                    skipped_version,
                                    skipped_published_at,
                                } => {
                                    result.skipped_by_cooldown.push((
                                        module.clone(),
                                        current_version.clone(),
                                        skipped_version,
                                        skipped_published_at,
                                    ));
                                    new_lines.push(line.to_string());
                                    continue;
                                }
                            }
                        };

                        // Match the precision of the original version (unless full precision requested)
                        let matched_version = if options.full_precision {
                            latest_version.clone()
                        } else {
                            match_version_precision(current_version, &latest_version)
                        };
                        if matched_version != *current_version {
                            // Refuse to write a downgrade (registry path only; pins are intentional).
                            if !is_pinned
                                && compare_versions(&matched_version, current_version, Lang::Go)
                                    != std::cmp::Ordering::Greater
                            {
                                result.warnings.push(downgrade_warning(
                                    module,
                                    &matched_version,
                                    current_version,
                                ));
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            } else {
                                // Replace version in the line, preserving everything else
                                let new_line = line.replacen(current_version, &matched_version, 1);
                                new_lines.push(new_line);

                                if *is_pinned {
                                    // Record as pinned (bypassed registry lookup)
                                    result.pinned.push((
                                        module.clone(),
                                        current_version.clone(),
                                        matched_version,
                                        Some(line_num),
                                    ));
                                } else {
                                    result.updated.push((
                                        module.clone(),
                                        current_version.clone(),
                                        matched_version.clone(),
                                        Some(line_num),
                                    ));
                                    if let Some((skipped_version, skipped_published_at)) =
                                        held_back_record
                                    {
                                        result.held_back.push((
                                            module.clone(),
                                            current_version.clone(),
                                            matched_version,
                                            skipped_version,
                                            skipped_published_at,
                                        ));
                                    }
                                }
                            }
                        } else {
                            new_lines.push(line.to_string());
                            result.unchanged += 1;
                        }
                    }
                    Err(e) => {
                        new_lines.push(line.to_string());
                        result.errors.push(format!("{}: {}", module, e));
                    }
                }
            } else {
                // Line doesn't need version update (not a module, replaced, or pseudo-version)
                new_lines.push(line.to_string());

                // Count skipped modules as unchanged
                let is_require_line = in_require_block
                    || (trimmed.starts_with("require ") && !trimmed.starts_with("require ("));

                if is_require_line {
                    let line_to_parse = if in_require_block {
                        line
                    } else {
                        &line[line.find("require").map(|i| i + 7).unwrap_or(0)..]
                    };

                    if let Some(caps) = self.require_re.captures(line_to_parse) {
                        let module = caps.get(1).unwrap().as_str();
                        let current_version = caps.get(2).unwrap().as_str();
                        if replaced_modules.contains(module)
                            || Self::is_pseudo_version(current_version)
                        {
                            result.unchanged += 1;
                        }
                    }
                }
            }
        }

        if (!result.updated.is_empty() || !result.pinned.is_empty()) && !options.dry_run {
            // Preserve original line ending
            let line_ending = if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let new_content = new_lines.join(line_ending);

            // Preserve trailing newline if original had one
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
        file_type == FileType::GoMod
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let mut deps = Vec::new();

        // Find modules with replace directives (we'll skip these)
        let replaced_modules = self.find_replaced_modules(&content);
        let mut in_require_block = false;

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Track require block state
            if trimmed.starts_with("require (") || trimmed == "require (" {
                in_require_block = true;
                continue;
            }

            if in_require_block && trimmed == ")" {
                in_require_block = false;
                continue;
            }

            // Check if this line is a require statement (inside block or single-line)
            let is_require_line = in_require_block
                || (trimmed.starts_with("require ") && !trimmed.starts_with("require ("));

            if !is_require_line {
                continue;
            }

            // Try to parse as a module requirement
            let line_to_parse = if in_require_block {
                line
            } else {
                // Single-line require: "require github.com/foo/bar v1.0.0"
                &line[line.find("require").map(|i| i + 7).unwrap_or(0)..]
            };

            if let Some(caps) = self.require_re.captures(line_to_parse) {
                let module = caps.get(1).unwrap().as_str();
                let current_version = caps.get(2).unwrap().as_str();

                // Skip replaced modules entirely — they point to a local or forked
                // path and cannot be resolved via the registry.
                if replaced_modules.contains(module) {
                    continue;
                }

                // Pseudo-versions (commit-based, not a release tag) are included so
                // that the audit path can see them, but they must not be bumped.
                let is_bumpable = !Self::is_pseudo_version(current_version);

                deps.push(ParsedDependency {
                    name: module.to_string(),
                    version: current_version.to_string(),
                    line_number: Some(line_idx + 1),
                    has_upper_bound: false, // Go doesn't have explicit upper bounds
                    is_bumpable,
                });
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

    #[test]
    fn test_require_regex() {
        let updater = GoModUpdater::new();

        // Standard version
        let caps = updater.require_re.captures("\tgithub.com/foo/bar v1.2.3");
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(1).unwrap().as_str(), "github.com/foo/bar");
        assert_eq!(caps.get(2).unwrap().as_str(), "v1.2.3");

        // With +incompatible
        let caps = updater
            .require_re
            .captures("\tgithub.com/foo/bar v2.0.0+incompatible");
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(2).unwrap().as_str(), "v2.0.0+incompatible");

        // Pre-release
        let caps = updater
            .require_re
            .captures("\tgithub.com/foo/bar v1.0.0-alpha.1");
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(2).unwrap().as_str(), "v1.0.0-alpha.1");
    }

    #[test]
    fn test_find_replaced_modules() {
        let updater = GoModUpdater::new();

        let content = r#"
module example.com/mymodule

require (
    github.com/foo/bar v1.0.0
    github.com/baz/qux v2.0.0
)

replace github.com/foo/bar => github.com/myfork/bar v1.0.1

replace (
    github.com/old/lib => ../local
)
"#;

        let replaced = updater.find_replaced_modules(content);
        assert!(replaced.contains("github.com/foo/bar"));
        assert!(replaced.contains("github.com/old/lib"));
        assert!(!replaced.contains("github.com/baz/qux"));
    }

    #[test]
    fn test_is_prerelease() {
        assert!(!GoModUpdater::is_prerelease("v1.0.0"));
        assert!(!GoModUpdater::is_prerelease("v1.0.0+incompatible"));
        assert!(GoModUpdater::is_prerelease("v1.0.0-alpha.1"));
        assert!(GoModUpdater::is_prerelease("v1.0.0-rc1"));
        assert!(GoModUpdater::is_prerelease("v1.0.0-beta"));
    }

    #[test]
    fn test_is_pseudo_version() {
        // Standard pseudo-versions (commit-based, no semver tags)
        assert!(GoModUpdater::is_pseudo_version(
            "v0.0.0-20241217172646-ca3f786aa774"
        ));
        assert!(GoModUpdater::is_pseudo_version(
            "v0.0.0-20220331215641-2d8c0ab7ef04"
        ));

        // Pre-release pseudo-versions (e.g., for modules with tagged releases)
        assert!(GoModUpdater::is_pseudo_version(
            "v1.2.4-0.20220331215641-2d8c0ab7ef04"
        ));

        // Normal versions should NOT be detected as pseudo-versions
        assert!(!GoModUpdater::is_pseudo_version("v1.0.0"));
        assert!(!GoModUpdater::is_pseudo_version("v1.0.0-alpha.1"));
        assert!(!GoModUpdater::is_pseudo_version("v1.0.0-rc1"));
        assert!(!GoModUpdater::is_pseudo_version("v2.0.0+incompatible"));
        assert!(!GoModUpdater::is_pseudo_version("v1.0.0-beta"));
    }

    #[tokio::test]
    async fn test_update_go_mod_file() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

go 1.21

require (
	github.com/foo/bar v1.0.0
	github.com/baz/qux v2.0.0
)
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("github.com/foo/bar", "v1.5.0")
            .with_version("github.com/baz/qux", "v2.3.0");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("v1.5.0"));
        assert!(content.contains("v2.3.0"));
    }

    #[tokio::test]
    async fn test_update_go_mod_dry_run() {
        let mut file = NamedTempFile::new().unwrap();
        let original = r#"module example.com/mymodule

require github.com/foo/bar v1.0.0
"#;
        write!(file, "{}", original).unwrap();

        let registry = MockRegistry::new("go-proxy").with_version("github.com/foo/bar", "v1.5.0");

        let updater = GoModUpdater::new();
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
    async fn test_update_go_mod_skips_replaced_modules() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	github.com/foo/bar v1.0.0
	github.com/baz/qux v2.0.0
)

replace github.com/foo/bar => ../local-bar
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("github.com/foo/bar", "v1.5.0")
            .with_version("github.com/baz/qux", "v2.3.0");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only baz/qux should be updated (foo/bar has a replace directive)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "github.com/baz/qux");
        assert_eq!(result.unchanged, 1); // foo/bar is counted as unchanged
    }

    #[tokio::test]
    async fn test_update_go_mod_skips_pseudo_versions() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	github.com/foo/bar v0.0.0-20241217172646-ca3f786aa774
	github.com/baz/qux v2.0.0
)
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("github.com/foo/bar", "v1.5.0")
            .with_version("github.com/baz/qux", "v2.3.0");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only baz/qux should be updated (foo/bar has a pseudo-version)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "github.com/baz/qux");
        assert_eq!(result.unchanged, 1); // foo/bar is counted as unchanged
    }

    #[tokio::test]
    async fn test_update_go_mod_preserves_comments() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	github.com/foo/bar v1.0.0 // indirect
	github.com/baz/qux v2.0.0
)
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("github.com/foo/bar", "v1.5.0")
            .with_version("github.com/baz/qux", "v2.3.0");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false);

        updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        // Comment should be preserved
        assert!(content.contains("// indirect"));
        assert!(content.contains("v1.5.0"));
    }

    #[tokio::test]
    async fn test_update_go_mod_line_numbers() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

go 1.21

require github.com/foo/bar v1.0.0
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy").with_version("github.com/foo/bar", "v1.5.0");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        // Line number should be found (require is on line 5)
        assert!(result.updated[0].3.is_some());
        assert_eq!(result.updated[0].3, Some(5));
    }

    #[tokio::test]
    async fn test_update_go_mod_registry_error() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require github.com/nonexistent/module v1.0.0
"#
        )
        .unwrap();

        // Registry without the module
        let registry = MockRegistry::new("go-proxy");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("github.com/nonexistent/module"));
    }

    // ==================== Config Tests ====================

    #[tokio::test]
    async fn test_config_ignore_module() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	github.com/foo/bar v1.0.0
	github.com/baz/qux v2.0.0
)
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("github.com/foo/bar", "v1.5.0")
            .with_version("github.com/baz/qux", "v2.3.0");

        // Configure to ignore github.com/foo/bar
        let config = UpdConfig {
            ignore: vec!["github.com/foo/bar".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
        };

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // foo/bar should be ignored, baz/qux should be updated
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "github.com/baz/qux");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "github.com/foo/bar");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("v1.0.0")); // foo/bar unchanged
        assert!(content.contains("v2.3.0")); // baz/qux updated
    }

    #[tokio::test]
    async fn test_config_pin_module() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	github.com/foo/bar v1.0.0
	github.com/baz/qux v2.0.0
)
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("github.com/foo/bar", "v1.5.0")
            .with_version("github.com/baz/qux", "v2.3.0");

        // Configure to pin github.com/foo/bar to v1.2.0
        let mut pins = std::collections::HashMap::new();
        pins.insert("github.com/foo/bar".to_string(), "v1.2.0".to_string());

        let config = UpdConfig {
            ignore: vec![],
            pin: pins,
            cooldown: None,
        };

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // foo/bar should be pinned to v1.2.0, baz/qux should be updated from registry
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "github.com/baz/qux");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "github.com/foo/bar");
        assert_eq!(result.pinned[0].2, "v1.2.0"); // new pinned version

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("v1.2.0")); // foo/bar pinned
        assert!(content.contains("v2.3.0")); // baz/qux updated
    }

    #[tokio::test]
    async fn test_config_pin_only_writes_file() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require github.com/foo/bar v1.0.0
"#
        )
        .unwrap();

        // No registry needed since we're only pinning
        let registry = MockRegistry::new("go-proxy");

        let mut pins = std::collections::HashMap::new();
        pins.insert("github.com/foo/bar".to_string(), "v1.2.0".to_string());

        let config = UpdConfig {
            ignore: vec![],
            pin: pins,
            cooldown: None,
        };

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only pinned, no registry updates
        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.pinned.len(), 1);

        // File should still be written with pinned version
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("v1.2.0"));
    }

    #[tokio::test]
    async fn test_config_mixed_ignore_pin_update() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	github.com/ignored/mod v1.0.0
	github.com/pinned/mod v2.0.0
	github.com/updated/mod v3.0.0
)
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("github.com/ignored/mod", "v1.5.0")
            .with_version("github.com/pinned/mod", "v2.5.0")
            .with_version("github.com/updated/mod", "v3.5.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("github.com/pinned/mod".to_string(), "v2.3.0".to_string());

        let config = UpdConfig {
            ignore: vec!["github.com/ignored/mod".to_string()],
            pin: pins,
            cooldown: None,
        };

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "github.com/ignored/mod");

        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "github.com/pinned/mod");
        assert_eq!(result.pinned[0].2, "v2.3.0");

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "github.com/updated/mod");
        assert_eq!(result.updated[0].2, "v3.5.0");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("v1.0.0")); // ignored - unchanged
        assert!(content.contains("v2.3.0")); // pinned
        assert!(content.contains("v3.5.0")); // updated from registry
    }

    #[tokio::test]
    async fn test_config_single_line_require() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require github.com/foo/bar v1.0.0
require github.com/baz/qux v2.0.0
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("github.com/foo/bar", "v1.5.0")
            .with_version("github.com/baz/qux", "v2.3.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("github.com/foo/bar".to_string(), "v1.2.0".to_string());

        let config = UpdConfig {
            ignore: vec!["github.com/baz/qux".to_string()],
            pin: pins,
            cooldown: None,
        };

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "github.com/foo/bar");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "github.com/baz/qux");
        assert_eq!(result.updated.len(), 0);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("v1.2.0")); // pinned
        assert!(content.contains("v2.0.0")); // ignored - unchanged
    }

    #[tokio::test]
    async fn test_config_preserves_comments_with_pin() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	github.com/foo/bar v1.0.0 // indirect
)
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy");

        let mut pins = std::collections::HashMap::new();
        pins.insert("github.com/foo/bar".to_string(), "v1.2.0".to_string());

        let config = UpdConfig {
            ignore: vec![],
            pin: pins,
            cooldown: None,
        };

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.pinned.len(), 1);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("v1.2.0 // indirect")); // version updated, comment preserved
    }

    #[tokio::test]
    async fn test_update_go_mod_ignores_retract_block() {
        // `retract` blocks mention versions that were pulled from the module
        // proxy. They must never be treated as require statements.
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

go 1.21

require github.com/foo/bar v1.0.0

retract (
	v1.0.0 // accidentally published
	v0.9.0
)

retract v0.8.0
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy").with_version("github.com/foo/bar", "v1.1.0");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "github.com/foo/bar");
        assert!(
            result.errors.is_empty(),
            "retract lines must not produce errors"
        );

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("retract (\n\tv1.0.0 // accidentally published\n\tv0.9.0\n)"),
            "retract block must be preserved verbatim, got:\n{content}"
        );
        assert!(content.contains("retract v0.8.0"));
        assert!(content.contains("github.com/foo/bar v1.1.0"));
    }

    #[tokio::test]
    async fn test_update_go_mod_incompatible_version_updates() {
        // `+incompatible` marks modules that haven't adopted Go modules
        // semantics for v2+. They still have valid semver tags and must be
        // updated to newer +incompatible versions from the proxy.
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require github.com/foo/bar v2.0.0+incompatible
"#
        )
        .unwrap();

        let registry =
            MockRegistry::new("go-proxy").with_version("github.com/foo/bar", "v2.1.0+incompatible");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        let (name, old, new, _) = &result.updated[0];
        assert_eq!(name, "github.com/foo/bar");
        assert_eq!(old, "v2.0.0+incompatible");
        assert_eq!(new, "v2.1.0+incompatible");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("v2.1.0+incompatible"));
    }

    // ==================== Pseudo-version audit tests ====================

    /// `parse_dependencies` must include pseudo-version entries so the audit path can see
    /// them. The entry must carry `is_bumpable: false` so alignment and update paths leave
    /// it alone.
    #[test]
    fn test_parse_dependencies_includes_pseudo_version_as_non_bumpable() {
        let updater = GoModUpdater::new();

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

go 1.21

require (
	golang.org/x/crypto v0.0.0-20200115085410-6d4e4cb37c7d
)
"#
        )
        .unwrap();

        let deps = updater.parse_dependencies(file.path()).unwrap();

        assert_eq!(deps.len(), 1, "pseudo-version must appear in parse output");
        assert_eq!(deps[0].name, "golang.org/x/crypto");
        assert_eq!(
            deps[0].version, "v0.0.0-20200115085410-6d4e4cb37c7d",
            "exact pseudo-version string must be preserved"
        );
        assert!(
            !deps[0].is_bumpable,
            "pseudo-version must have is_bumpable == false"
        );
    }

    /// When a go.mod contains both a pseudo-version and a normal release of the same
    /// module, `parse_dependencies` must return both, and only the semver release must
    /// be marked bumpable.
    #[test]
    fn test_parse_dependencies_mixed_pseudo_and_semver() {
        let updater = GoModUpdater::new();

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	golang.org/x/crypto v0.0.0-20200115085410-6d4e4cb37c7d
	github.com/foo/bar v1.2.3
)
"#
        )
        .unwrap();

        let deps = updater.parse_dependencies(file.path()).unwrap();

        assert_eq!(deps.len(), 2);
        let pseudo = deps
            .iter()
            .find(|d| d.name == "golang.org/x/crypto")
            .expect("pseudo-version dep not found");
        assert!(!pseudo.is_bumpable);

        let semver_dep = deps
            .iter()
            .find(|d| d.name == "github.com/foo/bar")
            .expect("semver dep not found");
        assert!(semver_dep.is_bumpable);
    }

    /// The update path must continue to skip pseudo-versions — they must remain
    /// unchanged after `update_file`.
    #[tokio::test]
    async fn test_update_still_skips_pseudo_versions() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	golang.org/x/crypto v0.0.0-20200115085410-6d4e4cb37c7d
	github.com/foo/bar v1.0.0
)
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("golang.org/x/crypto", "v0.31.0")
            .with_version("github.com/foo/bar", "v1.5.0");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only the semver dep should be updated
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "github.com/foo/bar");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("v0.0.0-20200115085410-6d4e4cb37c7d"),
            "pseudo-version must remain unchanged"
        );
        assert!(content.contains("v1.5.0"), "semver dep must be updated");
    }

    #[tokio::test]
    async fn test_update_go_mod_preserves_indirect_comments_on_update() {
        // Regression: `replacen` with count 1 must not eat inline comments.
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module example.com/mymodule

require (
	github.com/foo/bar v1.0.0 // indirect
	github.com/baz/qux v1.0.0 // indirect; kept for compat
)
"#
        )
        .unwrap();

        let registry = MockRegistry::new("go-proxy")
            .with_version("github.com/foo/bar", "v1.1.0")
            .with_version("github.com/baz/qux", "v1.2.0");

        let updater = GoModUpdater::new();
        let options = UpdateOptions::new(false, false);

        updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("v1.1.0 // indirect"));
        assert!(content.contains("v1.2.0 // indirect; kept for compat"));
    }
}
