mod config;
mod dependencies;
#[cfg(test)]
#[path = "pre_commit/tests.rs"]
mod extended_tests;

use super::{
    CooldownOutcome, FileType, ParsedDependency, RegistrySet, UpdateOptions, UpdateResult, Updater,
    apply_cooldown, downgrade_warning, read_file_safe, write_file_atomic,
};
use crate::align::compare_versions;
use crate::registry::Registry;
use crate::updater::Lang;
use crate::version::match_version_precision;
use anyhow::Result;
use config::{Node, Scalar};
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;

pub struct PreCommitUpdater {
    registries: RegistrySet,
}

#[derive(Debug, Clone)]
pub struct PreCommitEdit {
    pub package: String,
    pub current: String,
    pub new: String,
    pub line: Option<usize>,
    pub span: Range<usize>,
    pub original: String,
    pub replacement: String,
    pub source: Option<crate::annotation::AnnotationSource>,
    pub pinned: bool,
    /// An inherited language was read at this planned repository revision.
    pub required_revision: Option<(Range<usize>, String)>,
}

fn record_edit(result: &mut UpdateResult, scalar: &Scalar, replacement: &str) {
    for ((package, current, new, line), pinned) in result
        .updated
        .iter()
        .map(|entry| (entry, false))
        .chain(result.pinned.iter().map(|entry| (entry, true)))
    {
        result.pre_commit_edits.push(PreCommitEdit {
            package: package.clone(),
            current: current.clone(),
            new: new.clone(),
            line: *line,
            span: scalar.span.clone().unwrap(),
            original: scalar.value.clone(),
            replacement: replacement.into(),
            source: result.entry_ecosystem.get(package).copied(),
            pinned,
            required_revision: None,
        });
    }
}

impl PreCommitUpdater {
    pub fn new() -> Self {
        Self {
            registries: RegistrySet::parse_only(),
        }
    }

    pub fn with_registries(registries: RegistrySet) -> Self {
        Self { registries }
    }

    async fn update_hooks(
        &self,
        repo: &Node,
        revision: Option<&str>,
        content: &str,
        registry: &dyn Registry,
        options: &UpdateOptions,
        manifests: &mut HashMap<(String, String), Result<Node, String>>,
    ) -> UpdateResult {
        let mut result = UpdateResult::default();
        if matches!(repo.text("repo"), Some("meta" | "builtin") | None) {
            return result;
        }
        let hooks = repo.get("hooks").map(Node::sequence).unwrap_or_default();
        let needs_manifest = hooks.iter().any(|hook| {
            hook.get("language").is_none()
                && !hook
                    .get("additional_dependencies")
                    .map(Node::sequence)
                    .unwrap_or_default()
                    .is_empty()
        });
        let key = repo
            .text("repo")
            .and_then(Self::extract_github_owner_repo)
            .zip(revision.map(str::to_string));
        if needs_manifest
            && let Some(key) = &key
            && !manifests.contains_key(key)
        {
            let manifest = match registry.pre_commit_manifest(&key.0, &key.1).await {
                Ok(content) => config::yaml(&content).map_err(|e| e.to_string()),
                Err(error) => Err(error.to_string()),
            };
            manifests.insert(key.clone(), manifest);
        }
        for hook in hooks {
            let deps = hook
                .get("additional_dependencies")
                .map(Node::sequence)
                .unwrap_or_default();
            if deps.is_empty() {
                continue;
            }
            let id = hook.text("id").unwrap_or("unknown hook");
            let language = if hook.get("language").is_some() {
                hook.text("language")
            } else {
                key.as_ref()
                    .and_then(|k| manifests.get(k))
                    .and_then(|m| m.as_ref().ok())
                    .and_then(|manifest| {
                        let matches: Vec<_> = manifest
                            .sequence()
                            .iter()
                            .filter(|h| h.text("id") == Some(id))
                            .collect();
                        if matches.len() == 1 {
                            matches[0].text("language")
                        } else {
                            None
                        }
                    })
            };
            let Some(language) = language.filter(|language| dependencies::supported(language))
            else {
                let detail = key
                    .as_ref()
                    .and_then(|k| manifests.get(k))
                    .and_then(|m| m.as_ref().err());
                result.warnings.push(format!(
                    "{id}: additional_dependencies left unchanged: {}{}",
                    language.map_or("cannot determine hook language".to_string(), |l| format!(
                        "unsupported hook language '{l}'"
                    )),
                    detail.map_or(String::new(), |e| format!(" ({e})"))
                ));
                continue;
            };
            for dep in deps {
                let Some(dep) = dep.scalar() else {
                    continue;
                };
                let Some(_) = &dep.span else {
                    result.warnings.push(format!("{id}: additional dependency uses a shared or nonliteral YAML/TOML value; left unchanged"));
                    continue;
                };
                match dependencies::update(&dep.value, language, &self.registries, options).await {
                    Ok(Some((mut update, new))) => {
                        dependencies::relocate(
                            &mut update,
                            dep.line(content),
                            &format!("hooks.{id}.additional_dependencies"),
                        );
                        record_edit(&mut update, dep, &new);
                        if hook.get("language").is_none()
                            && let Some(rev) = repo.get("rev").and_then(Node::scalar)
                            && let Some(revision) = revision.filter(|v| *v != rev.value)
                            && let Some(span) = &rev.span
                        {
                            for edit in &mut update.pre_commit_edits {
                                edit.required_revision = Some((span.clone(), revision.to_string()));
                            }
                        }
                        result.merge(update);
                    }
                    Ok(None) => {}
                    Err(error) => result.errors.push(format!("{id}: {}: {error}", dep.value)),
                }
            }
        }
        result
    }

    fn extract_github_owner_repo(url: &str) -> Option<String> {
        let url = url::Url::parse(url.trim()).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str() != Some("github.com") {
            return None;
        }
        let mut parts = url.path().trim_matches('/').split('/');
        let owner = parts.next().filter(|s| !s.is_empty())?;
        let repo = parts.next()?;
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if repo.is_empty() || parts.next().is_some() {
            return None;
        }
        Some(format!("{owner}/{repo}"))
    }

    fn compute_updated_version(current: &str, latest: &str, full_precision: bool) -> String {
        let version = if full_precision {
            latest.trim_start_matches('v').to_string()
        } else {
            match_version_precision(
                current.trim_start_matches('v'),
                latest.trim_start_matches('v'),
            )
        };
        if current.starts_with('v') {
            format!("v{version}")
        } else {
            version
        }
    }

    fn parse(content: &str, path: &Path) -> Result<Node> {
        if path.file_name().and_then(|n| n.to_str()) == Some("prek.toml") {
            config::toml(content)
        } else {
            config::yaml(content)
        }
    }

    fn revisions(root: &Node, content: &str) -> Vec<ParsedDependency> {
        root.get("repos")
            .map(Node::sequence)
            .unwrap_or_default()
            .iter()
            .filter_map(|repo| {
                let name = Self::extract_github_owner_repo(repo.text("repo")?)?;
                let rev = repo.get("rev")?.scalar()?;
                rev.span.as_ref()?;
                Some(ParsedDependency {
                    name,
                    version: rev.value.clone(),
                    line_number: rev.line(content),
                    has_upper_bound: false,
                    is_bumpable: true,
                })
            })
            .collect()
    }

    pub fn parse_dependencies_from_content(&self, content: &str) -> Vec<ParsedDependency> {
        config::yaml(content)
            .map(|root| Self::revisions(&root, content))
            .unwrap_or_default()
    }

    /// Rewrite one verified repository revision for version alignment.
    pub fn rewrite_revision(
        content: &str,
        package: &str,
        old: &str,
        new: &str,
        line: Option<usize>,
    ) -> Option<String> {
        let root = config::toml(content)
            .or_else(|_| config::yaml(content))
            .ok()?;
        for repo in root.get("repos")?.sequence() {
            if repo
                .text("repo")
                .and_then(Self::extract_github_owner_repo)
                .as_deref()
                != Some(package)
            {
                continue;
            }
            let Some(rev) = repo.get("rev").and_then(Node::scalar) else {
                continue;
            };
            if rev.value != old || line.is_some_and(|line| rev.line(content) != Some(line)) {
                continue;
            }
            let mut rewritten = content.to_string();
            rewritten.replace_range(rev.span.clone()?, new);
            return Some(rewritten);
        }
        None
    }

    async fn update_revision(
        repo: &str,
        rev: &Scalar,
        content: &str,
        registry: &dyn Registry,
        options: &UpdateOptions,
        versions: &mut HashMap<String, Result<String, String>>,
    ) -> (UpdateResult, Option<String>) {
        let mut result = UpdateResult::default();
        let current = &rev.value;
        let line = rev.line(content);
        if rev.span.is_none() {
            return (result, None);
        }
        if options.is_package_filtered_out(repo) {
            result.unchanged += 1;
            return (result, None);
        }
        if options.should_ignore(repo) {
            result.ignored.push((repo.into(), current.clone(), line));
            return (result, None);
        }
        let pin = options.get_pinned_version(repo);
        let latest = if let Some(pin) = pin {
            Ok(pin.to_string())
        } else {
            if !versions.contains_key(repo) {
                versions.insert(
                    repo.into(),
                    registry
                        .get_latest_version(repo)
                        .await
                        .map_err(|e| e.to_string()),
                );
            }
            versions[repo].clone()
        };
        let latest = match latest {
            Ok(latest) => latest,
            Err(error) => {
                result.errors.push(format!("{repo}: {error}"));
                return (result, None);
            }
        };
        let mut held_back = None;
        let latest = if pin.is_some() {
            latest
        } else {
            let (outcome, note) =
                apply_cooldown(registry, repo, current, &latest, None, false, options).await;
            if let Some(note) = note {
                options.note_cooldown_unavailable(&note);
            }
            match outcome {
                CooldownOutcome::Unchanged(v) => v,
                CooldownOutcome::HeldBack {
                    chosen,
                    skipped_version,
                    skipped_published_at,
                } => {
                    held_back = Some((skipped_version, skipped_published_at));
                    chosen
                }
                CooldownOutcome::Skipped {
                    skipped_version,
                    skipped_published_at,
                } => {
                    result.skipped_by_cooldown.push((
                        repo.into(),
                        current.clone(),
                        skipped_version,
                        skipped_published_at,
                    ));
                    return (result, None);
                }
            }
        };
        let new = Self::compute_updated_version(current, &latest, options.full_precision);
        if new == *current {
            result.unchanged += 1;
            return (result, None);
        }
        if pin.is_none()
            && compare_versions(&new, current, Lang::PreCommit) != std::cmp::Ordering::Greater
        {
            result.warnings.push(downgrade_warning(repo, &new, current));
            result.unchanged += 1;
            return (result, None);
        }
        if pin.is_none() && !options.allows_bump(current, &new) {
            result.record_capped(repo, current, &new, line);
            return (result, None);
        }
        let entry = (repo.into(), current.clone(), new.clone(), line);
        if pin.is_some() {
            result.pinned.push(entry);
        } else {
            result.updated.push(entry);
            if let Some((skipped, at)) = held_back {
                result
                    .held_back
                    .push((repo.into(), current.clone(), new.clone(), skipped, at));
            }
        }
        (result, Some(new))
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
        let root = Self::parse(&content, path)?;
        let mut result = UpdateResult::default();
        let repos = root.get("repos").map(Node::sequence).unwrap_or_default();
        let names: std::collections::BTreeSet<_> = repos
            .iter()
            .filter_map(|repo| {
                let rev = repo.get("rev")?.scalar()?;
                rev.span.as_ref()?;
                let name = Self::extract_github_owner_repo(repo.text("repo")?)?;
                (!options.is_package_filtered_out(&name)
                    && !options.should_ignore(&name)
                    && options.get_pinned_version(&name).is_none())
                .then_some(name)
            })
            .collect();
        let mut versions: HashMap<_, _> =
            futures::future::join_all(names.into_iter().map(|name| async move {
                let version = registry
                    .get_latest_version(&name)
                    .await
                    .map_err(|e| e.to_string());
                (name, version)
            }))
            .await
            .into_iter()
            .collect();
        let mut manifests = HashMap::new();
        for repo in root.get("repos").map(Node::sequence).unwrap_or_default() {
            let mut revision = repo.text("rev").map(str::to_string);
            if let Some(name) = repo.text("repo").and_then(Self::extract_github_owner_repo)
                && let Some(rev) = repo.get("rev").and_then(Node::scalar)
            {
                let (mut update, new) =
                    Self::update_revision(&name, rev, &content, registry, &options, &mut versions)
                        .await;
                if let Some(new) = &new {
                    record_edit(&mut update, rev, new);
                }
                result.merge(update);
                if let Some(new) = new {
                    revision = Some(new.clone());
                }
            }
            result.merge(
                self.update_hooks(
                    repo,
                    revision.as_deref(),
                    &content,
                    registry,
                    &options,
                    &mut manifests,
                )
                .await,
            );
        }
        if !options.dry_run && !result.pre_commit_edits.is_empty() {
            let mut edits: Vec<_> = result.pre_commit_edits.iter().collect();
            edits.sort_by_key(|edit| std::cmp::Reverse(edit.span.start));
            let mut updated = content;
            for edit in edits {
                updated.replace_range(edit.span.clone(), &edit.replacement);
            }
            Self::parse(&updated, path)?;
            write_file_atomic(path, &updated)?;
        }
        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::PreCommitConfig
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        Ok(Self::revisions(&Self::parse(&content, path)?, &content))
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
            exclude: Vec::new(),
            ignore: vec!["pre-commit/pre-commit-hooks".to_string()],
            pin: pins,
            cooldown: None,
            ..Default::default()
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
