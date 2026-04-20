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
use std::collections::HashMap;
use std::path::Path;

pub struct GithubActionsUpdater {
    uses_re: Regex,
}

impl GithubActionsUpdater {
    pub fn new() -> Self {
        let uses_re =
            Regex::new(r#"uses:\s*"?([^@\s"]+)@([^"'\s#]+)"#).expect("Invalid uses regex");
        Self { uses_re }
    }

    /// Returns true if the ref looks like a commit SHA (7+ hex characters)
    fn is_sha_ref(ref_str: &str) -> bool {
        ref_str.len() >= 7 && ref_str.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// Returns true if the ref looks like a branch name (e.g., main, develop)
    fn is_branch_ref(ref_str: &str) -> bool {
        // Must not have a 'v' prefix, no dots, not purely numeric
        if ref_str.starts_with('v') || ref_str.contains('.') {
            return false;
        }
        if ref_str.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        // Must contain at least one non-hex alphabetic character (g-z, G-Z)
        ref_str
            .chars()
            .any(|c| c.is_ascii_alphabetic() && !c.is_ascii_hexdigit())
    }

    /// Returns true if the ref should be skipped (SHA or branch)
    fn should_skip_ref(ref_str: &str) -> bool {
        Self::is_sha_ref(ref_str) || Self::is_branch_ref(ref_str)
    }

    /// Returns true if the action reference should be skipped entirely
    fn should_skip_action(action: &str) -> bool {
        if action.starts_with("./") || action.starts_with("docker://") {
            return true;
        }
        if action.contains(".yml") || action.contains(".yaml") {
            return true;
        }
        let segments: Vec<&str> = action.split('/').collect();
        segments.len() < 2
    }

    /// Returns true if the line starts a YAML block scalar (e.g., `run: |`)
    /// Handles all YAML block scalar forms: `|`, `>`, `|-`, `>+`, `|2`, `>3-`, etc.
    fn is_block_scalar_start(line: &str) -> bool {
        let trimmed = line.trim();
        if let Some(colon_pos) = trimmed.find(':') {
            let after_colon = trimmed[colon_pos + 1..].trim();
            let mut chars = after_colon.chars();
            match chars.next() {
                Some('|' | '>') => {}
                _ => return false,
            }
            // After `|` or `>`, optional digit(s) then optional `-`/`+`, then end
            let rest: String = chars.collect();
            let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            matches!(rest, "" | "-" | "+")
        } else {
            false
        }
    }

    /// Compute the updated version string, preserving the `v` prefix and precision
    fn compute_updated_version(current: &str, latest: &str, full_precision: bool) -> String {
        let has_v = current.starts_with('v');
        let stripped_current = current.strip_prefix('v').unwrap_or(current);
        let stripped_latest = latest.strip_prefix('v').unwrap_or(latest);

        let result = if full_precision {
            stripped_latest.to_string()
        } else {
            match_version_precision(stripped_current, stripped_latest)
        };

        if has_v {
            format!("v{}", result)
        } else {
            result
        }
    }

    /// Extract `owner/repo` from an action reference like `owner/repo/path`
    fn extract_owner_repo(action: &str) -> &str {
        let parts: Vec<&str> = action.splitn(3, '/').collect();
        if parts.len() >= 2 {
            let end = action
                .find('/')
                .and_then(|first| {
                    action[first + 1..]
                        .find('/')
                        .map(|second| first + 1 + second)
                })
                .unwrap_or(action.len());
            &action[..end]
        } else {
            action
        }
    }

    /// Parse dependencies from content string (for testing without file I/O)
    pub fn parse_dependencies_from_content(&self, content: &str) -> Vec<ParsedDependency> {
        let mut deps = Vec::new();
        let mut in_block_scalar = false;
        let mut block_parent_indent: usize = 0;

        for (line_idx, line) in content.lines().enumerate() {
            // Track block scalar context
            if in_block_scalar {
                let current_indent = line.len() - line.trim_start().len();
                // Empty lines stay inside block scalars
                if !line.trim().is_empty() && current_indent <= block_parent_indent {
                    in_block_scalar = false;
                } else {
                    continue;
                }
            }

            if Self::is_block_scalar_start(line) {
                in_block_scalar = true;
                block_parent_indent = line.len() - line.trim_start().len();
                continue;
            }

            let trimmed = line.trim();

            // Skip commented lines
            if trimmed.starts_with('#') {
                continue;
            }

            if let Some(caps) = self.uses_re.captures(line) {
                let action = caps.get(1).unwrap().as_str();
                let version_ref = caps.get(2).unwrap().as_str();

                if Self::should_skip_action(action) || Self::should_skip_ref(version_ref) {
                    continue;
                }

                let owner_repo = Self::extract_owner_repo(action);

                deps.push(ParsedDependency {
                    name: owner_repo.to_string(),
                    version: version_ref.to_string(),
                    line_number: Some(line_idx + 1),
                    has_upper_bound: false,
                });
            }
        }

        deps
    }
}

impl Default for GithubActionsUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for GithubActionsUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut result = UpdateResult::default();

        // Pass 1: Collect actions to check
        // Store: (line_idx, owner_repo, version_ref)
        let mut ignored_actions: Vec<(usize, String, String)> = Vec::new();
        let mut pinned_actions: Vec<(usize, String, String, String)> = Vec::new();
        let mut actions_to_check: Vec<(usize, String, String)> = Vec::new();

        let mut in_block_scalar = false;
        let mut block_parent_indent: usize = 0;

        for (line_idx, line) in content.lines().enumerate() {
            // Track block scalar context
            if in_block_scalar {
                let current_indent = line.len() - line.trim_start().len();
                if !line.trim().is_empty() && current_indent <= block_parent_indent {
                    in_block_scalar = false;
                } else {
                    continue;
                }
            }

            if Self::is_block_scalar_start(line) {
                in_block_scalar = true;
                block_parent_indent = line.len() - line.trim_start().len();
                continue;
            }

            let trimmed = line.trim();

            // Skip commented lines
            if trimmed.starts_with('#') {
                continue;
            }

            if let Some(caps) = self.uses_re.captures(line) {
                let action = caps.get(1).unwrap().as_str();
                let version_ref = caps.get(2).unwrap().as_str();

                if Self::should_skip_action(action) || Self::should_skip_ref(version_ref) {
                    continue;
                }

                let owner_repo = Self::extract_owner_repo(action).to_string();

                // Check config for ignore/pin
                if options.should_ignore(&owner_repo) {
                    ignored_actions.push((line_idx, owner_repo, version_ref.to_string()));
                    continue;
                }

                if let Some(pinned_version) = options.get_pinned_version(&owner_repo) {
                    pinned_actions.push((
                        line_idx,
                        owner_repo,
                        version_ref.to_string(),
                        pinned_version.to_string(),
                    ));
                    continue;
                }

                actions_to_check.push((line_idx, owner_repo, version_ref.to_string()));
            }
        }

        // Record ignored actions
        for (line_idx, owner_repo, version) in ignored_actions {
            result
                .ignored
                .push((owner_repo, version, Some(line_idx + 1)));
        }

        // Pass 2: Fetch versions in parallel (deduplicated by owner_repo)
        let unique_repos: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            actions_to_check
                .iter()
                .filter_map(|(_, owner_repo, _)| {
                    if seen.insert(owner_repo.clone()) {
                        Some(owner_repo.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };

        let version_futures: Vec<_> = unique_repos
            .iter()
            .map(|owner_repo| async { registry.get_latest_version(owner_repo).await })
            .collect();

        let version_results = join_all(version_futures).await;

        // Build a map from owner_repo -> latest version result
        let repo_versions: HashMap<String, Result<String, String>> = unique_repos
            .into_iter()
            .zip(version_results)
            .map(|(repo, result)| (repo, result.map_err(|e| e.to_string())))
            .collect();

        // Build version map per line index, cloning results from the deduplicated map
        let mut version_map: HashMap<usize, Result<String, anyhow::Error>> = HashMap::new();
        for (line_idx, owner_repo, _) in &actions_to_check {
            if let Some(result) = repo_versions.get(owner_repo) {
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

        // Add pinned versions to version map
        for (line_idx, _, _, pinned_version) in &pinned_actions {
            version_map.insert(*line_idx, Ok(pinned_version.clone()));
        }

        // Build action info map: line_idx -> (owner_repo, current_version, is_pinned)
        let mut action_info: HashMap<usize, (String, String, bool)> = actions_to_check
            .into_iter()
            .map(|(idx, owner_repo, version)| (idx, (owner_repo, version, false)))
            .collect();

        for (line_idx, owner_repo, current_version, _) in pinned_actions {
            action_info.insert(line_idx, (owner_repo, current_version, true));
        }

        // Pass 3: Apply updates
        let mut new_lines: Vec<String> = Vec::new();
        in_block_scalar = false;
        block_parent_indent = 0;

        for (line_idx, line) in content.lines().enumerate() {
            let line_num = line_idx + 1;

            // Track block scalar context (for correct line output)
            if in_block_scalar {
                let current_indent = line.len() - line.trim_start().len();
                if !line.trim().is_empty() && current_indent <= block_parent_indent {
                    in_block_scalar = false;
                }
            }

            if !in_block_scalar && Self::is_block_scalar_start(line) {
                in_block_scalar = true;
                block_parent_indent = line.len() - line.trim_start().len();
            }

            if let Some(version_result) = version_map.remove(&line_idx) {
                let Some((owner_repo, current_version, is_pinned)) = action_info.get(&line_idx)
                else {
                    new_lines.push(line.to_string());
                    continue;
                };

                match version_result {
                    Ok(latest_version) => {
                        let new_version = Self::compute_updated_version(
                            current_version,
                            &latest_version,
                            options.full_precision,
                        );

                        if new_version != *current_version {
                            // Refuse to write a downgrade (registry path only; pins are intentional).
                            if !is_pinned
                                && compare_versions(&new_version, current_version, Lang::Actions)
                                    != std::cmp::Ordering::Greater
                            {
                                result.warnings.push(downgrade_warning(
                                    owner_repo,
                                    &new_version,
                                    current_version,
                                ));
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            } else {
                                let new_line = line.replacen(current_version, &new_version, 1);
                                new_lines.push(new_line);

                                if *is_pinned {
                                    result.pinned.push((
                                        owner_repo.clone(),
                                        current_version.clone(),
                                        new_version,
                                        Some(line_num),
                                    ));
                                } else {
                                    result.updated.push((
                                        owner_repo.clone(),
                                        current_version.clone(),
                                        new_version,
                                        Some(line_num),
                                    ));
                                }
                            }
                        } else {
                            new_lines.push(line.to_string());
                            result.unchanged += 1;
                        }
                    }
                    Err(e) => {
                        new_lines.push(line.to_string());
                        result.errors.push(format!("{}: {}", owner_repo, e));
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
        file_type == FileType::GithubActions
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        Ok(self.parse_dependencies_from_content(&content))
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
    fn test_uses_regex_basic() {
        let updater = GithubActionsUpdater::new();
        let caps = updater
            .uses_re
            .captures("      - uses: actions/checkout@v4");
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(1).unwrap().as_str(), "actions/checkout");
        assert_eq!(caps.get(2).unwrap().as_str(), "v4");
    }

    #[test]
    fn test_uses_regex_quoted() {
        let updater = GithubActionsUpdater::new();
        let caps = updater
            .uses_re
            .captures(r#"      - uses: "actions/checkout@v4""#);
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(1).unwrap().as_str(), "actions/checkout");
        assert_eq!(caps.get(2).unwrap().as_str(), "v4");
    }

    #[test]
    fn test_uses_regex_inline_comment() {
        let updater = GithubActionsUpdater::new();
        let caps = updater
            .uses_re
            .captures("      - uses: actions/checkout@v4 # comment");
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(1).unwrap().as_str(), "actions/checkout");
        assert_eq!(caps.get(2).unwrap().as_str(), "v4");
    }

    #[test]
    fn test_is_sha_ref() {
        // Full SHA
        assert!(GithubActionsUpdater::is_sha_ref(
            "a5ac7e51b28d7f9f3091645916e8170a8b5cbc47"
        ));
        // Short SHA (7 chars)
        assert!(GithubActionsUpdater::is_sha_ref("a5ac7e5"));
        // Too short
        assert!(!GithubActionsUpdater::is_sha_ref("a5ac7e"));
        // Contains non-hex
        assert!(!GithubActionsUpdater::is_sha_ref("a5ac7g5"));
        // Version tag
        assert!(!GithubActionsUpdater::is_sha_ref("v4"));
    }

    #[test]
    fn test_is_branch_ref() {
        assert!(GithubActionsUpdater::is_branch_ref("main"));
        assert!(GithubActionsUpdater::is_branch_ref("master"));
        assert!(GithubActionsUpdater::is_branch_ref("develop"));
        // Has 'v' prefix
        assert!(!GithubActionsUpdater::is_branch_ref("v4"));
        // Purely numeric
        assert!(!GithubActionsUpdater::is_branch_ref("1"));
        // Has dots (version-like)
        assert!(!GithubActionsUpdater::is_branch_ref("4.1.0"));
        // All hex chars (could be a short SHA)
        assert!(!GithubActionsUpdater::is_branch_ref("deadbeef"));
    }

    #[test]
    fn test_should_skip() {
        // SHA
        assert!(GithubActionsUpdater::should_skip_ref(
            "a5ac7e51b28d7f9f3091645916e8170a8b5cbc47"
        ));
        // Branch
        assert!(GithubActionsUpdater::should_skip_ref("main"));
        // Version tag
        assert!(!GithubActionsUpdater::should_skip_ref("v4"));
        assert!(!GithubActionsUpdater::should_skip_ref("v4.1.0"));
    }

    #[test]
    fn test_should_skip_action() {
        // Local action
        assert!(GithubActionsUpdater::should_skip_action("./my-action"));
        // Docker action
        assert!(GithubActionsUpdater::should_skip_action(
            "docker://alpine:3.8"
        ));
        // Reusable workflow
        assert!(GithubActionsUpdater::should_skip_action(
            "org/repo/.github/workflows/ci.yml"
        ));
        assert!(GithubActionsUpdater::should_skip_action(
            "org/repo/.github/workflows/ci.yaml"
        ));
        // Malformed (single segment)
        assert!(GithubActionsUpdater::should_skip_action("checkout"));
        // Valid
        assert!(!GithubActionsUpdater::should_skip_action(
            "actions/checkout"
        ));
        assert!(!GithubActionsUpdater::should_skip_action(
            "actions/checkout/sub"
        ));
    }

    #[test]
    fn test_is_block_scalar_start() {
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: |"
        ));
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: >"
        ));
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: |-"
        ));
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: >-"
        ));
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: |+"
        ));
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: >+"
        ));
        // With explicit indentation indicators
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: |2"
        ));
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: >3"
        ));
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: |2-"
        ));
        assert!(GithubActionsUpdater::is_block_scalar_start(
            "        run: >3+"
        ));
        // Not block scalar
        assert!(!GithubActionsUpdater::is_block_scalar_start(
            "        run: echo hello"
        ));
        assert!(!GithubActionsUpdater::is_block_scalar_start(
            "        uses: actions/checkout@v4"
        ));
    }

    #[test]
    fn test_block_scalar_indentation() {
        let updater = GithubActionsUpdater::new();
        let content = r#"jobs:
  build:
    steps:
      - name: Run script
        run: |
          echo "uses: fake/action@v1"
          echo "another line"
      - uses: actions/checkout@v4
"#;
        let deps = updater.parse_dependencies_from_content(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "actions/checkout");
        assert_eq!(deps[0].version, "v4");
    }

    #[test]
    fn test_version_prefix_handling() {
        // v-prefix preserved
        assert_eq!(
            GithubActionsUpdater::compute_updated_version("v4", "v5.1.0", false),
            "v5"
        );
        // v-prefix preserved with full precision
        assert_eq!(
            GithubActionsUpdater::compute_updated_version("v4", "v5.1.0", true),
            "v5.1.0"
        );
        // No prefix
        assert_eq!(
            GithubActionsUpdater::compute_updated_version("4.0.0", "5.1.0", false),
            "5.1.0"
        );
        // v-prefix on current, none on latest
        assert_eq!(
            GithubActionsUpdater::compute_updated_version("v4", "5.1.0", false),
            "v5"
        );
        // Multi-component precision
        assert_eq!(
            GithubActionsUpdater::compute_updated_version("v4.1", "v5.2.3", false),
            "v5.2"
        );
        assert_eq!(
            GithubActionsUpdater::compute_updated_version("v4.1.0", "v5.2.3", false),
            "v5.2.3"
        );
    }

    #[tokio::test]
    async fn test_update_workflow_file() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v3
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v5.0.0")
            .with_version("actions/setup-node", "v4.2.0");

        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("actions/checkout@v5"));
        assert!(content.contains("actions/setup-node@v4"));
    }

    #[tokio::test]
    async fn test_skips_sha_pinned() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"name: CI
on: push
jobs:
  build:
    steps:
      - uses: actions/checkout@a5ac7e51b28d7f9f3091645916e8170a8b5cbc47
      - uses: actions/setup-node@v3
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v5.0.0")
            .with_version("actions/setup-node", "v4.2.0");

        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only setup-node should be updated; checkout is SHA-pinned
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "actions/setup-node");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("a5ac7e51b28d7f9f3091645916e8170a8b5cbc47"));
    }

    #[tokio::test]
    async fn test_skips_block_scalar_content() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"name: CI
on: push
jobs:
  build:
    steps:
      - name: Script
        run: |
          echo "uses: fake/action@v1"
      - uses: actions/checkout@v4
"#
        )
        .unwrap();

        let registry =
            MockRegistry::new("github-releases").with_version("actions/checkout", "v5.0.0");

        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "actions/checkout");

        let content = fs::read_to_string(file.path()).unwrap();
        // The fake action inside the run block should be untouched
        assert!(content.contains(r#"echo "uses: fake/action@v1""#));
        assert!(content.contains("actions/checkout@v5"));
    }

    #[tokio::test]
    async fn test_dry_run_does_not_write() {
        let mut file = NamedTempFile::new().unwrap();
        let original = r#"name: CI
on: push
jobs:
  build:
    steps:
      - uses: actions/checkout@v4
"#;
        write!(file, "{}", original).unwrap();

        let registry =
            MockRegistry::new("github-releases").with_version("actions/checkout", "v5.0.0");

        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified
        let content = fs::read_to_string(file.path()).unwrap();
        assert_eq!(content, original);
    }

    #[tokio::test]
    async fn test_skips_commented_lines() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"name: CI
on: push
jobs:
  build:
    steps:
      # - uses: actions/checkout@v3
      - uses: actions/setup-node@v3
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v5.0.0")
            .with_version("actions/setup-node", "v4.2.0");

        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only setup-node should be updated; checkout line is commented
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "actions/setup-node");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("# - uses: actions/checkout@v3"));
    }

    #[test]
    fn test_version_no_hash_suffix() {
        let updater = GithubActionsUpdater::new();
        let caps = updater
            .uses_re
            .captures("      - uses: actions/checkout@v4#nospacehash");
        assert!(caps.is_some());
        let caps = caps.unwrap();
        assert_eq!(caps.get(1).unwrap().as_str(), "actions/checkout");
        assert_eq!(caps.get(2).unwrap().as_str(), "v4");
    }

    #[test]
    fn test_extract_owner_repo() {
        assert_eq!(
            GithubActionsUpdater::extract_owner_repo("actions/checkout"),
            "actions/checkout"
        );
        assert_eq!(
            GithubActionsUpdater::extract_owner_repo("org/repo/path/to/action"),
            "org/repo"
        );
    }

    #[test]
    fn test_parse_dependencies_from_content() {
        let updater = GithubActionsUpdater::new();
        let content = r#"name: CI
on: push
jobs:
  build:
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v3.8.1
      - uses: ./local-action
      - uses: docker://alpine:3.8
"#;
        let deps = updater.parse_dependencies_from_content(content);
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "actions/checkout");
        assert_eq!(deps[0].version, "v4");
        assert_eq!(deps[1].name, "actions/setup-node");
        assert_eq!(deps[1].version, "v3.8.1");
    }

    #[tokio::test]
    async fn test_full_workflow_integration() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"name: CI
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: actions/setup-node@v4.1.0
      - uses: actions/checkout@a5ac7e51b41094c92402da3b24376905380afc29
      - uses: ./local-action
      - uses: docker://node:20
      - uses: actions/checkout@main
      # uses: commented/action@v1
      - name: Echo
        run: |
          echo "uses: fake/action@v1"
      - uses: jdx/mise-action@v2
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v4.2.0")
            .with_version("actions/setup-node", "v4.2.0")
            .with_version("jdx/mise-action", "v2.1.0");

        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // actions/checkout@v3 -> v4 (major-only precision)
        // actions/setup-node@v4.1.0 -> v4.2.0 (full precision preserved)
        // SHA-pinned: skipped
        // ./local-action: skipped (local ref)
        // docker://node:20: skipped (docker ref)
        // actions/checkout@main: skipped (branch ref)
        // commented line: skipped
        // run: | block content: skipped
        // jdx/mise-action@v2 -> v2 (unchanged, same major)
        assert_eq!(
            result.updated.len(),
            2,
            "Expected 2 updates, got: {:?}",
            result.updated
        );

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("actions/checkout@v4"),
            "checkout should be updated to v4"
        );
        assert!(
            content.contains("actions/setup-node@v4.2.0"),
            "setup-node should be updated to v4.2.0"
        );
        assert!(
            content.contains("a5ac7e51b41094c92402da3b24376905380afc29"),
            "SHA should be unchanged"
        );
        assert!(
            content.contains("actions/checkout@main"),
            "branch ref should be unchanged"
        );
        assert!(
            content.contains(r#"echo "uses: fake/action@v1""#),
            "block scalar content should be unchanged"
        );
        assert!(
            content.contains("# uses: commented/action@v1"),
            "commented line should be unchanged"
        );
        assert!(
            content.contains("jdx/mise-action@v2"),
            "unchanged action should keep version"
        );
    }

    #[test]
    fn test_handles() {
        let updater = GithubActionsUpdater::new();
        assert!(updater.handles(FileType::GithubActions));
        assert!(!updater.handles(FileType::Requirements));
    }

    #[tokio::test]
    async fn test_registry_error_populates_errors() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "steps:\n  - uses: nonexistent/action@v1\n").unwrap();

        // Registry has no entry for nonexistent/action → will error
        let registry = MockRegistry::new("github-releases");
        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(true, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent/action"));
    }

    #[tokio::test]
    async fn test_preserves_crlf_line_endings() {
        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, b"steps:\r\n  - uses: actions/checkout@v3\r\n")
            .unwrap();

        let registry =
            MockRegistry::new("github-releases").with_version("actions/checkout", "v4.2.0");
        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(false, false);
        updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("\r\n"),
            "Should preserve CRLF line endings"
        );
        assert!(content.contains("actions/checkout@v4\r\n"));
    }

    #[tokio::test]
    async fn test_deduplicates_same_action() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: actions/checkout@v3\n  - uses: actions/checkout@v3\n  - uses: actions/checkout@v3\n"
        )
        .unwrap();

        let registry =
            MockRegistry::new("github-releases").with_version("actions/checkout", "v4.2.0");
        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // All 3 occurrences should be updated
        assert_eq!(result.updated.len(), 3);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            !content.contains("@v3"),
            "All occurrences should be updated"
        );
        assert_eq!(content.matches("@v4").count(), 3);
    }

    #[tokio::test]
    async fn test_config_ignore_and_pin() {
        use crate::config::UpdConfig;
        use std::io::Write;
        use std::sync::Arc;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"steps:
  - uses: actions/checkout@v3
  - uses: actions/setup-node@v3
  - uses: jdx/mise-action@v1
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v4.2.0")
            .with_version("actions/setup-node", "v4.2.0")
            .with_version("jdx/mise-action", "v2.1.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("actions/setup-node".to_string(), "v4.0.0".to_string());
        let config = UpdConfig {
            ignore: vec!["actions/checkout".to_string()],
            pin: pins,
        };

        let updater = GithubActionsUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "actions/checkout");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "actions/setup-node");
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "jdx/mise-action");
    }
}
