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

pub struct PreCommitUpdater {
    repo_re: Regex,
    rev_re: Regex,
}

impl PreCommitUpdater {
    pub fn new() -> Self {
        let repo_re = Regex::new(r"^\s*-?\s*repo:\s*(.+)").expect("Invalid repo regex");
        let rev_re = Regex::new(r##"^\s*rev:\s*['"]?([^'"#\s]+)"##).expect("Invalid rev regex");
        Self { repo_re, rev_re }
    }

    /// Extract `owner/repo` from a GitHub URL.
    /// Handles `https://github.com/owner/repo` and `https://github.com/owner/repo.git`.
    /// Returns None for non-GitHub URLs.
    fn extract_github_owner_repo(url: &str) -> Option<String> {
        let url = url.trim();

        // Must be a GitHub URL
        let path = url
            .strip_prefix("https://github.com/")
            .or_else(|| url.strip_prefix("http://github.com/"))?;

        let mut parts = path.trim_end_matches('/').splitn(3, '/');
        let owner = parts.next().filter(|s| !s.is_empty())?;
        let repo = parts.next().filter(|s| !s.is_empty())?;

        // Strip .git suffix if present
        let repo = repo.strip_suffix(".git").unwrap_or(repo);

        Some(format!("{}/{}", owner, repo))
    }

    /// Returns true if the repo line should be skipped (local, meta, or non-GitHub)
    fn should_skip_repo(repo_url: &str) -> bool {
        let trimmed = repo_url.trim();
        trimmed == "local"
            || trimmed == "meta"
            || Self::extract_github_owner_repo(trimmed).is_none()
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

    /// Parse dependencies from content string (for testing without file I/O)
    pub fn parse_dependencies_from_content(&self, content: &str) -> Vec<ParsedDependency> {
        let mut deps = Vec::new();
        let mut current_repo: Option<String> = None;

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            // Skip commented lines
            if trimmed.starts_with('#') {
                continue;
            }

            // Check for repo: line
            if let Some(caps) = self.repo_re.captures(line) {
                let repo_url = caps.get(1).unwrap().as_str().trim();
                if Self::should_skip_repo(repo_url) {
                    current_repo = None;
                } else {
                    current_repo = Self::extract_github_owner_repo(repo_url);
                }
                continue;
            }

            // Check for rev: line
            if let Some(ref owner_repo) = current_repo
                && let Some(caps) = self.rev_re.captures(line)
            {
                let version = caps.get(1).unwrap().as_str();
                deps.push(ParsedDependency {
                    name: owner_repo.clone(),
                    version: version.to_string(),
                    line_number: Some(line_idx + 1),
                    has_upper_bound: false,
                    is_bumpable: true,
                });
                current_repo = None;
            }
        }

        deps
    }
}

impl Default for PreCommitUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for PreCommitUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut result = UpdateResult::default();

        // Pass 1: Collect repos to check
        let mut ignored_repos: Vec<(usize, String, String)> = Vec::new();
        let mut pinned_repos: Vec<(usize, String, String, String)> = Vec::new();
        let mut repos_to_check: Vec<(usize, String, String)> = Vec::new();

        let mut current_repo: Option<String> = None;

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with('#') {
                continue;
            }

            if let Some(caps) = self.repo_re.captures(line) {
                let repo_url = caps.get(1).unwrap().as_str().trim();
                if Self::should_skip_repo(repo_url) {
                    current_repo = None;
                } else {
                    current_repo = Self::extract_github_owner_repo(repo_url);
                }
                continue;
            }

            if let Some(ref owner_repo) = current_repo
                && let Some(caps) = self.rev_re.captures(line)
            {
                let version = caps.get(1).unwrap().as_str().to_string();
                let owner_repo = owner_repo.clone();

                if options.is_package_filtered_out(&owner_repo) {
                    result.unchanged += 1;
                } else if options.should_ignore(&owner_repo) {
                    ignored_repos.push((line_idx, owner_repo, version));
                } else if let Some(pinned_version) = options.get_pinned_version(&owner_repo) {
                    pinned_repos.push((line_idx, owner_repo, version, pinned_version.to_string()));
                } else {
                    repos_to_check.push((line_idx, owner_repo, version));
                }

                current_repo = None;
            }
        }

        // Record ignored repos
        for (line_idx, owner_repo, version) in ignored_repos {
            result
                .ignored
                .push((owner_repo, version, Some(line_idx + 1)));
        }

        // Pass 2: Fetch versions in parallel (deduplicated)
        let unique_repos: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            repos_to_check
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

        let repo_versions: HashMap<String, Result<String, String>> = unique_repos
            .into_iter()
            .zip(version_results)
            .map(|(repo, result)| (repo, result.map_err(|e| e.to_string())))
            .collect();

        // Build version map per line index
        let mut version_map: HashMap<usize, Result<String, anyhow::Error>> = HashMap::new();
        for (line_idx, owner_repo, _) in &repos_to_check {
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

        // Add pinned versions
        for (line_idx, _, _, pinned_version) in &pinned_repos {
            version_map.insert(*line_idx, Ok(pinned_version.clone()));
        }

        // Build repo info map: line_idx -> (owner_repo, current_version, is_pinned)
        let mut repo_info: HashMap<usize, (String, String, bool)> = repos_to_check
            .into_iter()
            .map(|(idx, owner_repo, version)| (idx, (owner_repo, version, false)))
            .collect();

        for (line_idx, owner_repo, current_version, _) in pinned_repos {
            repo_info.insert(line_idx, (owner_repo, current_version, true));
        }

        // Pass 3: Apply updates
        let mut new_lines: Vec<String> = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            let line_num = line_idx + 1;

            if let Some(version_result) = version_map.remove(&line_idx) {
                let Some((owner_repo, current_version, is_pinned)) = repo_info.get(&line_idx)
                else {
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
                                owner_repo,
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
                                        owner_repo.clone(),
                                        current_version.clone(),
                                        skipped_version,
                                        skipped_published_at,
                                    ));
                                    new_lines.push(line.to_string());
                                    continue;
                                }
                            }
                        };

                        let new_version = Self::compute_updated_version(
                            current_version,
                            &latest_version,
                            options.full_precision,
                        );

                        if new_version != *current_version {
                            // Refuse to write a downgrade (registry path only; pins are intentional).
                            if !is_pinned
                                && compare_versions(&new_version, current_version, Lang::PreCommit)
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
                                        new_version.clone(),
                                        Some(line_num),
                                    ));
                                    if let Some((skipped_version, skipped_published_at)) =
                                        held_back_record
                                    {
                                        result.held_back.push((
                                            owner_repo.clone(),
                                            current_version.clone(),
                                            new_version,
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
        file_type == FileType::PreCommitConfig
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
    fn test_extract_github_owner_repo() {
        assert_eq!(
            PreCommitUpdater::extract_github_owner_repo(
                "https://github.com/pre-commit/pre-commit-hooks"
            ),
            Some("pre-commit/pre-commit-hooks".to_string())
        );
        assert_eq!(
            PreCommitUpdater::extract_github_owner_repo("https://github.com/psf/black.git"),
            Some("psf/black".to_string())
        );
        assert_eq!(
            PreCommitUpdater::extract_github_owner_repo("https://github.com/owner/repo/"),
            Some("owner/repo".to_string())
        );
        // Non-GitHub URLs return None
        assert_eq!(
            PreCommitUpdater::extract_github_owner_repo("https://gitlab.com/owner/repo"),
            None
        );
        assert_eq!(
            PreCommitUpdater::extract_github_owner_repo("https://bitbucket.org/owner/repo"),
            None
        );
        // Invalid URLs
        assert_eq!(
            PreCommitUpdater::extract_github_owner_repo("https://github.com/"),
            None
        );
        assert_eq!(
            PreCommitUpdater::extract_github_owner_repo("https://github.com/owner"),
            None
        );
    }

    #[test]
    fn test_parse_dependencies() {
        let updater = PreCommitUpdater::new();
        let content = r#"repos:
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v5.0.0
    hooks:
      - id: trailing-whitespace
      - id: end-of-file-fixer
  - repo: https://github.com/psf/black
    rev: 24.3.0
    hooks:
      - id: black
"#;
        let deps = updater.parse_dependencies_from_content(content);
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "pre-commit/pre-commit-hooks");
        assert_eq!(deps[0].version, "v5.0.0");
        assert_eq!(deps[1].name, "psf/black");
        assert_eq!(deps[1].version, "24.3.0");
    }

    #[test]
    fn test_skips_local_repos() {
        let updater = PreCommitUpdater::new();
        let content = r#"repos:
  - repo: local
    hooks:
      - id: my-local-hook
  - repo: https://github.com/psf/black
    rev: 24.3.0
    hooks:
      - id: black
"#;
        let deps = updater.parse_dependencies_from_content(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "psf/black");
    }

    #[test]
    fn test_skips_meta_repos() {
        let updater = PreCommitUpdater::new();
        let content = r#"repos:
  - repo: meta
    hooks:
      - id: check-hooks-apply
  - repo: https://github.com/psf/black
    rev: 24.3.0
    hooks:
      - id: black
"#;
        let deps = updater.parse_dependencies_from_content(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "psf/black");
    }

    #[test]
    fn test_skips_non_github_repos() {
        let updater = PreCommitUpdater::new();
        let content = r#"repos:
  - repo: https://gitlab.com/pycqa/flake8
    rev: 7.0.0
    hooks:
      - id: flake8
  - repo: https://github.com/psf/black
    rev: 24.3.0
    hooks:
      - id: black
"#;
        let deps = updater.parse_dependencies_from_content(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "psf/black");
    }

    #[test]
    fn test_preserves_unquoted_and_quoted_revs() {
        let updater = PreCommitUpdater::new();
        let content = r#"repos:
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v5.0.0
    hooks:
      - id: trailing-whitespace
  - repo: https://github.com/psf/black
    rev: "24.3.0"
    hooks:
      - id: black
  - repo: https://github.com/pycqa/isort
    rev: '5.13.2'
    hooks:
      - id: isort
"#;
        let deps = updater.parse_dependencies_from_content(content);
        assert_eq!(deps.len(), 3);
        assert_eq!(deps[0].version, "v5.0.0");
        assert_eq!(deps[1].version, "24.3.0");
        assert_eq!(deps[2].version, "5.13.2");
    }

    #[tokio::test]
    async fn test_update_pre_commit_config() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"repos:
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v4.5.0
    hooks:
      - id: trailing-whitespace
  - repo: https://github.com/psf/black
    rev: 24.3.0
    hooks:
      - id: black
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("pre-commit/pre-commit-hooks", "v5.0.0")
            .with_version("psf/black", "24.10.0");

        let updater = PreCommitUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("rev: v5.0.0"));
        assert!(content.contains("rev: 24.10.0"));
    }

    #[tokio::test]
    async fn test_dry_run() {
        let mut file = NamedTempFile::new().unwrap();
        let original = r#"repos:
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v4.5.0
    hooks:
      - id: trailing-whitespace
"#;
        write!(file, "{}", original).unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("pre-commit/pre-commit-hooks", "v5.0.0");

        let updater = PreCommitUpdater::new();
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

    #[test]
    fn test_version_prefix_handling() {
        // v-prefix preserved
        assert_eq!(
            PreCommitUpdater::compute_updated_version("v4", "v5.1.0", false),
            "v5"
        );
        // v-prefix preserved with full precision
        assert_eq!(
            PreCommitUpdater::compute_updated_version("v4", "v5.1.0", true),
            "v5.1.0"
        );
        // No prefix
        assert_eq!(
            PreCommitUpdater::compute_updated_version("24.3.0", "24.10.0", false),
            "24.10.0"
        );
        // v-prefix on current, none on latest
        assert_eq!(
            PreCommitUpdater::compute_updated_version("v4.5.0", "5.0.0", false),
            "v5.0.0"
        );
        // Multi-component precision
        assert_eq!(
            PreCommitUpdater::compute_updated_version("v4.1", "v5.2.3", false),
            "v5.2"
        );
    }

    #[tokio::test]
    async fn test_config_ignore_and_pin() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"repos:
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v4.0.0
    hooks:
      - id: trailing-whitespace
  - repo: https://github.com/psf/black
    rev: 23.0.0
    hooks:
      - id: black
  - repo: https://github.com/astral-sh/ruff-pre-commit
    rev: v0.1.0
    hooks:
      - id: ruff
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("pre-commit/pre-commit-hooks", "v5.0.0")
            .with_version("psf/black", "24.3.0")
            .with_version("astral-sh/ruff-pre-commit", "v0.4.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("psf/black".to_string(), "24.0.0".to_string());
        let config = UpdConfig {
            ignore: vec!["pre-commit/pre-commit-hooks".to_string()],
            pin: pins,
            cooldown: None,
        };

        let updater = PreCommitUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "pre-commit/pre-commit-hooks");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "psf/black");
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "astral-sh/ruff-pre-commit");
    }

    #[test]
    fn test_skips_commented_lines() {
        let updater = PreCommitUpdater::new();
        let content = r#"repos:
  # - repo: https://github.com/pre-commit/pre-commit-hooks
  #   rev: v4.5.0
  - repo: https://github.com/psf/black
    rev: 24.3.0
    hooks:
      - id: black
"#;
        let deps = updater.parse_dependencies_from_content(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "psf/black");
    }

    #[test]
    fn test_handles() {
        let updater = PreCommitUpdater::new();
        assert!(updater.handles(FileType::PreCommitConfig));
        assert!(!updater.handles(FileType::Requirements));
    }

    #[tokio::test]
    async fn test_registry_error_populates_errors() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "repos:\n  - repo: https://github.com/nonexistent/hook\n    rev: v1.0.0\n    hooks:\n      - id: test\n"
        )
        .unwrap();

        // Registry has no entry for nonexistent/hook → will error
        let registry = MockRegistry::new("github-releases");
        let updater = PreCommitUpdater::new();
        let options = UpdateOptions::new(true, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent/hook"));
    }

    /// End-to-end regression for the N-segment tag fix. PreCommitUpdater must
    /// update shellcheck-py's 4-segment rev to the latest 4-segment tag.
    #[tokio::test]
    async fn test_shellcheck_py_four_segment_rev_is_updated() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"repos:
  - repo: https://github.com/shellcheck-py/shellcheck-py
    rev: v0.8.0.4
    hooks:
      - id: shellcheck
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("shellcheck-py/shellcheck-py", "v0.11.0.1");

        let updater = PreCommitUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1, "expected exactly one update");
        assert_eq!(result.warnings.len(), 0, "no downgrade warning expected");
        assert_eq!(result.updated[0].0, "shellcheck-py/shellcheck-py");
        assert_eq!(result.updated[0].1, "v0.8.0.4");
        assert_eq!(result.updated[0].2, "v0.11.0.1");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("rev: v0.11.0.1"),
            "file should contain new rev, got: {content}",
        );
        assert!(
            !content.contains("rev: v0.8.0.4"),
            "file should no longer contain old rev, got: {content}",
        );
    }

    /// Downgrade guard still fires if the registry returns a lower 4-segment tag.
    /// This pins the guard in place so the primary fix can't silently reintroduce
    /// the downgrade.
    #[tokio::test]
    async fn test_downgrade_guard_refuses_lower_four_segment_rev() {
        let mut file = NamedTempFile::new().unwrap();
        let original = r#"repos:
  - repo: https://github.com/shellcheck-py/shellcheck-py
    rev: v0.8.0.4
    hooks:
      - id: shellcheck
"#;
        write!(file, "{}", original).unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("shellcheck-py/shellcheck-py", "v0.0.2");

        let updater = PreCommitUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0, "must not downgrade");
        assert_eq!(result.warnings.len(), 1, "expected one downgrade warning");
        assert!(result.warnings[0].contains("shellcheck-py/shellcheck-py"));

        let content = fs::read_to_string(file.path()).unwrap();
        assert_eq!(content, original, "file must be unchanged");
    }
}
