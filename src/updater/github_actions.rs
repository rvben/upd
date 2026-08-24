use super::{
    FileType, ParsedDependency, SkipStatus, UpdateOptions, UpdateResult, Updater,
    downgrade_warning, read_file_safe, write_file_atomic,
};
use crate::align::compare_versions;
use crate::registry::{Registry, is_ref_not_found};
use crate::updater::Lang;
use crate::version::match_version_precision;
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;

pub struct GithubActionsUpdater {
    uses_re: Regex,
    version_comment_re: Regex,
}

#[derive(Debug, Clone)]
struct ShaAction {
    line_idx: usize,
    owner_repo: String,
    current_sha: String,
    current_version: String,
    pinned_version: Option<String>,
}

/// Resolve a hand-written version string to the commit it names.
///
/// A version comment and a configured pin are both authored by hand, so their
/// `v` prefix records local style rather than the repo's tag naming: a repo that
/// tags `v7.0.1` is routinely annotated `# 7.0.1`, and a repo that tags `1.3.0`
/// is routinely annotated `# v1.3.0`. Either spelling names one release, so both
/// are accepted.
///
/// The literal spelling is tried first, which decides the rare repo publishing
/// `1.2.3` and `v1.2.3` at different commits by what the file actually says. The
/// other spelling is only reached once the repo has said the literal one does
/// not exist, so a version comment that resolves to the wrong commit is still a
/// mismatch rather than something the second lookup can rescue. A lookup that
/// merely failed is propagated instead: a rate limit against `v1.2.3` is not
/// evidence that the author meant `1.2.3`. The first error is the one reported,
/// because it names the ref the author wrote.
async fn resolve_version_ref(
    registry: &dyn Registry,
    owner_repo: &str,
    version: &str,
) -> Result<String> {
    match registry.resolve_ref_to_commit(owner_repo, version).await {
        Ok(commit) => Ok(commit),
        Err(literal_error) if is_ref_not_found(&literal_error) => {
            let variant = match version.strip_prefix('v') {
                Some(bare) => bare.to_string(),
                None => format!("v{version}"),
            };
            registry
                .resolve_ref_to_commit(owner_repo, &variant)
                .await
                .map_err(|_| literal_error)
        }
        Err(error) => Err(error),
    }
}

impl GithubActionsUpdater {
    pub fn new() -> Self {
        let uses_re =
            Regex::new(r#"uses:\s*["']?([^@\s"']+)@([^"'\s#]+)"#).expect("Invalid uses regex");
        // A concrete SemVer annotation is deliberately required. Floating
        // comments such as `# v4` cannot prove which release the old SHA was
        // intended to represent and therefore cannot be updated safely.
        let version_comment_re = Regex::new(
            r#"^\s*["']?\s*#\s*(v?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)(?:\s|$)"#,
        )
        .expect("Invalid SHA version comment regex");
        Self {
            uses_re,
            version_comment_re,
        }
    }

    /// Returns true if the ref looks like a commit SHA (7+ hex characters)
    fn is_sha_ref(ref_str: &str) -> bool {
        ref_str.len() >= 7 && ref_str.chars().all(|c| c.is_ascii_hexdigit())
    }

    fn is_full_sha_ref(ref_str: &str) -> bool {
        ref_str.len() == 40 && ref_str.chars().all(|c| c.is_ascii_hexdigit())
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
        let segments: Vec<&str> = action.split('/').collect();
        segments.len() < 2
    }

    fn version_comment(&self, line: &str, uses_end: usize) -> Option<String> {
        self.version_comment_re
            .captures(&line[uses_end..])
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
            .filter(|version| Self::is_concrete_version(version))
    }

    fn is_concrete_version(version: &str) -> bool {
        semver::Version::parse(version.strip_prefix('v').unwrap_or(version)).is_ok()
    }

    /// Rewrite one `uses:` line from one verified SHA pin to another, moving the
    /// commit and its version comment together.
    ///
    /// Returns `None` unless the line still carries `current_sha` annotated with
    /// exactly `current_version`, which keeps the pin immutable: a line that
    /// drifted since the scan is left alone rather than rewritten from stale
    /// input. Every path that edits a SHA pin goes through here, so an
    /// interactively approved update cannot rewrite the line differently from
    /// the same update applied in one pass.
    pub fn replace_sha_pin(
        &self,
        line: &str,
        current_sha: &str,
        current_version: &str,
        new_sha: &str,
        new_version: &str,
    ) -> Option<String> {
        // `replacen` leaves the line untouched when the commit is absent, so
        // without this the comment would still be rewritten and the pin would
        // end up advertising a version its commit does not correspond to.
        if !line.contains(current_sha) {
            return None;
        }
        let mut updated = line.replacen(current_sha, new_sha, 1);
        let uses_end = self.uses_re.captures(&updated)?.get(0)?.end();
        let suffix = &updated[uses_end..];
        let caps = self.version_comment_re.captures(suffix)?;
        let version = caps.get(1)?;
        if version.as_str() != current_version {
            return None;
        }
        let start = uses_end + version.start();
        let end = uses_end + version.end();
        updated.replace_range(start..end, new_version);
        Some(updated)
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

    /// Resolve a precision-matched version against the refs a repo actually
    /// publishes.
    ///
    /// An action is pinned to a git ref, not to a released version, so
    /// shortening `v4.1.2` to `v4` is only valid when the repo publishes a
    /// floating `v4`. Most do; some ship only concrete tags (sigstore's
    /// cosign-installer has `v2` and `v3` but no `v4`), and writing the short
    /// form there produces a workflow that cannot resolve the action.
    ///
    /// `refs` empty means the ref list is unknown - a registry that does not
    /// expose refs, or a lookup that failed - and must leave the candidate
    /// alone rather than expanding every action to full precision. The caller
    /// reports the failed-lookup case, since a shortened ref then goes out
    /// unverified.
    fn resolve_against_refs(candidate: String, latest: &str, refs: &[String]) -> String {
        if refs.is_empty() {
            return candidate;
        }
        let same =
            |a: &str, b: &str| a.strip_prefix('v').unwrap_or(a) == b.strip_prefix('v').unwrap_or(b);
        // No truncation happened, so there is nothing to verify.
        if same(&candidate, latest) {
            return candidate;
        }
        if refs.iter().any(|r| same(r, &candidate)) {
            return candidate;
        }
        // Fall back to the concrete version, keeping the candidate's v-prefix style.
        let bare = latest.strip_prefix('v').unwrap_or(latest);
        if candidate.starts_with('v') {
            format!("v{bare}")
        } else {
            bare.to_string()
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
                    is_bumpable: true,
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
        let mut sha_actions: Vec<ShaAction> = Vec::new();

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

                if Self::should_skip_action(action) {
                    continue;
                }

                let owner_repo = Self::extract_owner_repo(action).to_string();

                if options.is_package_filtered_out(&owner_repo) {
                    result.unchanged += 1;
                    continue;
                }

                // Check config for ignore/pin
                if options.should_ignore(&owner_repo) {
                    ignored_actions.push((line_idx, owner_repo, version_ref.to_string()));
                    continue;
                }

                if Self::is_sha_ref(version_ref) {
                    // A SHA pin left alone is recorded either way. Dropping it
                    // silently would let the run report every dependency as up
                    // to date while this one was never looked at.
                    if !options.update_action_shas {
                        result.skipped.push(super::SkippedUpdate {
                            package: owner_repo,
                            current: version_ref.to_string(),
                            status: SkipStatus::NotExamined,
                            reason: "action-sha-updates-off",
                            message: "SHA pin updates are turned off by --no-update-action-shas, or `update_action_shas = false` in .updrc.toml".to_string(),
                            line_number: Some(line_idx + 1),
                        });
                        continue;
                    }
                    if !Self::is_full_sha_ref(version_ref) {
                        result.skipped.push(super::SkippedUpdate {
                            package: owner_repo,
                            current: version_ref.to_string(),
                            status: SkipStatus::Blocked,
                            reason: "short-sha",
                            message: "a full 40-character commit SHA is required".to_string(),
                            line_number: Some(line_idx + 1),
                        });
                        continue;
                    }
                    let Some(current_version) =
                        self.version_comment(line, caps.get(0).unwrap().end())
                    else {
                        result.skipped.push(super::SkippedUpdate {
                            package: owner_repo,
                            current: version_ref.to_string(),
                            status: SkipStatus::Blocked,
                            reason: "missing-version-comment",
                            message: "add a concrete version comment such as `# v4.2.2` to make this SHA pin safely updateable".to_string(),
                            line_number: Some(line_idx + 1),
                        });
                        continue;
                    };
                    let pinned_version =
                        options.get_pinned_version(&owner_repo).map(str::to_string);
                    sha_actions.push(ShaAction {
                        line_idx,
                        owner_repo,
                        current_sha: version_ref.to_ascii_lowercase(),
                        current_version,
                        pinned_version,
                    });
                    continue;
                }

                if Self::is_branch_ref(version_ref) {
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
            let mut repos = Vec::new();
            for (_, owner_repo, _) in &actions_to_check {
                if seen.insert(owner_repo.clone()) {
                    repos.push(owner_repo.clone());
                }
            }
            for action in &sha_actions {
                if action.pinned_version.is_none() && seen.insert(action.owner_repo.clone()) {
                    repos.push(action.owner_repo.clone());
                }
            }
            repos
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

        // Pass 2b: an action pinned to a shortened ref (`v3` against a latest of
        // `v4.1.2`) can only be moved to another shortened ref the repo actually
        // publishes. Fetch the ref list for exactly those repos - shortening is
        // the only case that needs verifying, so a workflow pinned at full
        // precision costs no extra requests.
        //
        // A registry with no ref concept and a lookup that did not complete both
        // leave the shortened candidate alone: expanding to full precision
        // because a request failed would make the file depend on network
        // weather. They differ in what the user is told. The second is reported,
        // but only for a repo whose shortened ref this run actually writes -
        // pass 3 decides that, so the failures are recorded here and warned
        // about there.
        let repos_needing_refs: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            actions_to_check
                .iter()
                .filter(|(_, owner_repo, version)| {
                    repo_versions
                        .get(owner_repo)
                        .and_then(|r| r.as_ref().ok())
                        .is_some_and(|latest| {
                            let bare = |s: &str| s.strip_prefix('v').unwrap_or(s).to_string();
                            bare(version).split('.').count() < bare(latest).split('.').count()
                        })
                })
                .filter(|(_, owner_repo, _)| seen.insert(owner_repo.clone()))
                .map(|(_, owner_repo, _)| owner_repo.clone())
                .collect()
        };
        let ref_results = join_all(
            repos_needing_refs
                .iter()
                .map(|owner_repo| async { registry.list_ref_names(owner_repo).await }),
        )
        .await;
        let mut repo_refs: HashMap<String, Vec<String>> = HashMap::new();
        let mut failed_ref_lookups: HashMap<String, String> = HashMap::new();
        for (repo, ref_result) in repos_needing_refs.into_iter().zip(ref_results) {
            let refs = match ref_result {
                Ok(refs) => refs,
                Err(error) => {
                    failed_ref_lookups.insert(repo.clone(), error.to_string());
                    Vec::new()
                }
            };
            repo_refs.insert(repo, refs);
        }
        // Repos whose shortened ref this run wrote without a ref list to check
        // it against, in first-written order so the report is deterministic.
        let mut unverified_shortenings: Vec<String> = Vec::new();

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

        let sha_action_info: HashMap<usize, ShaAction> = sha_actions
            .into_iter()
            .map(|action| (action.line_idx, action))
            .collect();

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

            if let Some(action) = sha_action_info.get(&line_idx) {
                let expected_current = match resolve_version_ref(
                    registry,
                    &action.owner_repo,
                    &action.current_version,
                )
                .await
                {
                    Ok(commit) => commit.to_ascii_lowercase(),
                    Err(error) => {
                        result.errors.push(format!(
                            "{}@{}: failed to verify current SHA pin: {}",
                            action.owner_repo, action.current_version, error
                        ));
                        new_lines.push(line.to_string());
                        continue;
                    }
                };

                if expected_current != action.current_sha {
                    result.skipped.push(super::SkippedUpdate {
                        package: action.owner_repo.clone(),
                        current: action.current_sha.clone(),
                        status: SkipStatus::Blocked,
                        reason: "version-comment-mismatch",
                        message: format!(
                            "comment {} resolves to {}, not the pinned commit",
                            action.current_version, expected_current
                        ),
                        line_number: Some(line_num),
                    });
                    new_lines.push(line.to_string());
                    continue;
                }

                let is_config_pinned = action.pinned_version.is_some();
                let target_result = match &action.pinned_version {
                    Some(version) => Ok(version.clone()),
                    None => repo_versions
                        .get(&action.owner_repo)
                        .cloned()
                        .unwrap_or_else(|| Err("no release result returned".to_string())),
                };
                let target_version = match target_result {
                    Ok(version) => version,
                    Err(error) => {
                        result
                            .errors
                            .push(format!("{}: {}", action.owner_repo, error));
                        new_lines.push(line.to_string());
                        continue;
                    }
                };
                if !Self::is_concrete_version(&target_version) {
                    result.skipped.push(super::SkippedUpdate {
                        package: action.owner_repo.clone(),
                        current: action.current_version.clone(),
                        status: SkipStatus::Blocked,
                        reason: "non-concrete-target",
                        message: format!(
                            "target {} is not a concrete semantic version",
                            target_version
                        ),
                        line_number: Some(line_num),
                    });
                    new_lines.push(line.to_string());
                    continue;
                }

                let (target_version, held_back_record) = if is_config_pinned {
                    (target_version, None)
                } else {
                    let (outcome, note) = crate::updater::apply_cooldown(
                        registry,
                        &action.owner_repo,
                        &action.current_version,
                        &target_version,
                        None,
                        false,
                        &options,
                    )
                    .await;
                    if let Some(msg) = note {
                        options.note_cooldown_unavailable(&msg);
                    }
                    match outcome {
                        crate::updater::CooldownOutcome::Unchanged(version) => (version, None),
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
                                action.owner_repo.clone(),
                                action.current_version.clone(),
                                skipped_version,
                                skipped_published_at,
                            ));
                            new_lines.push(line.to_string());
                            continue;
                        }
                    }
                };

                // `5.0.0` and `v5.0.0` name one release, so the version
                // comparison decides whether there is anything to do here.
                // String equality would send an up-to-date pin whose comment
                // spells the tag differently into the downgrade branch below.
                let ordering =
                    compare_versions(&target_version, &action.current_version, Lang::Actions);
                if ordering == std::cmp::Ordering::Equal {
                    // A configured pin is an instruction, so a repo publishing
                    // `1.2.3` and `v1.2.3` at different commits makes the pinned
                    // spelling a target of its own rather than a restyling of the
                    // comment, and skipping it would drop the instruction without
                    // a word. Only a differing spelling is resolved, and only for
                    // a pin: doing it for the latest version would cost a request
                    // for every action already up to date.
                    let satisfied = !is_config_pinned
                        || target_version == action.current_version
                        || resolve_version_ref(registry, &action.owner_repo, &target_version)
                            .await
                            .is_ok_and(|commit| commit.to_ascii_lowercase() == action.current_sha);
                    if satisfied {
                        result.unchanged += 1;
                        new_lines.push(line.to_string());
                        continue;
                    }
                }
                if !is_config_pinned && ordering != std::cmp::Ordering::Greater {
                    result.warnings.push(downgrade_warning(
                        &action.owner_repo,
                        &target_version,
                        &action.current_version,
                    ));
                    result.unchanged += 1;
                    new_lines.push(line.to_string());
                    continue;
                }
                if !is_config_pinned
                    && !options.allows_bump(&action.current_version, &target_version)
                {
                    // The update is known and writable; only the ceiling holds it
                    // back, which is `capped` rather than a `Blocked` skip. The
                    // other blocked reasons mean the line cannot be updated at
                    // all, and a reader needs to tell those two apart.
                    result.record_capped(
                        &action.owner_repo,
                        &action.current_version,
                        &target_version,
                        Some(line_num),
                    );
                    new_lines.push(line.to_string());
                    continue;
                }

                let new_sha = match resolve_version_ref(
                    registry,
                    &action.owner_repo,
                    &target_version,
                )
                .await
                {
                    Ok(commit) => commit.to_ascii_lowercase(),
                    Err(error) => {
                        result.errors.push(format!(
                            "{}@{}: failed to resolve target SHA: {}",
                            action.owner_repo, target_version, error
                        ));
                        new_lines.push(line.to_string());
                        continue;
                    }
                };
                if !Self::is_full_sha_ref(&new_sha) {
                    result.errors.push(format!(
                        "{}@{}: registry returned an invalid commit SHA",
                        action.owner_repo, target_version
                    ));
                    new_lines.push(line.to_string());
                    continue;
                }

                // The tag resolves the commit; the comment keeps the prefix style
                // the file already used, the same way the non-pinned path does.
                let styled_version =
                    Self::compute_updated_version(&action.current_version, &target_version, true);

                // A comment has to name the commit written beside it, or the next
                // run reads the line as a forged pin and refuses it. Restyling is
                // safe only while both spellings name one release, which a repo
                // publishing `1.3.0` and `v1.3.0` at different commits breaks. The
                // restyled spelling is looked up literally, without the fallback,
                // so the target tag is resolved exactly once: a spelling the repo
                // does not publish is one the next run reaches through that
                // fallback, and one that resolves elsewhere is a different
                // release. While the answer is unknown the resolved tag's own
                // spelling is written, since style is worth less than a comment
                // that describes the commit beside it.
                let comment_version = if styled_version == target_version {
                    styled_version
                } else {
                    match registry
                        .resolve_ref_to_commit(&action.owner_repo, &styled_version)
                        .await
                    {
                        Ok(commit) if commit.to_ascii_lowercase() == new_sha => styled_version,
                        Err(error) if is_ref_not_found(&error) => styled_version,
                        _ => target_version.clone(),
                    }
                };

                let Some(new_line) = self.replace_sha_pin(
                    line,
                    &action.current_sha,
                    &action.current_version,
                    &new_sha,
                    &comment_version,
                ) else {
                    result.errors.push(format!(
                        "{}: could not safely rewrite SHA pin at line {}",
                        action.owner_repo, line_num
                    ));
                    new_lines.push(line.to_string());
                    continue;
                };
                new_lines.push(new_line);

                let change = super::ActionShaUpdate {
                    package: action.owner_repo.clone(),
                    current_version: action.current_version.clone(),
                    new_version: comment_version.clone(),
                    current_commit: action.current_sha.clone(),
                    new_commit: new_sha,
                    line_number: Some(line_num),
                };
                result.action_sha_updates.push(change);
                if is_config_pinned {
                    result.pinned.push((
                        action.owner_repo.clone(),
                        action.current_version.clone(),
                        comment_version,
                        Some(line_num),
                    ));
                } else {
                    result.updated.push((
                        action.owner_repo.clone(),
                        action.current_version.clone(),
                        comment_version.clone(),
                        Some(line_num),
                    ));
                    if let Some((skipped_version, skipped_published_at)) = held_back_record {
                        result.held_back.push((
                            action.owner_repo.clone(),
                            action.current_version.clone(),
                            comment_version,
                            skipped_version,
                            skipped_published_at,
                        ));
                    }
                }
                continue;
            }

            if let Some(version_result) = version_map.remove(&line_idx) {
                let Some((owner_repo, current_version, is_pinned)) = action_info.get(&line_idx)
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
                        // A shortened ref is only writable if the repo publishes it.
                        let new_version = Self::resolve_against_refs(
                            new_version,
                            &latest_version,
                            repo_refs.get(owner_repo).map_or(&[][..], |v| v.as_slice()),
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
                            } else if !is_pinned
                                && !options.allows_bump(current_version, &new_version)
                            {
                                // Bump level exceeds the --only-bump/--max-bump ceiling.
                                // Configured pins are intentional and bypass the ceiling.
                                result.record_capped(
                                    owner_repo,
                                    current_version,
                                    &new_version,
                                    Some(line_num),
                                );
                                new_lines.push(line.to_string());
                            } else {
                                let new_line = line.replacen(current_version, &new_version, 1);
                                new_lines.push(new_line);

                                // The written ref is shorter than the version it
                                // stands for, and the list that would confirm
                                // the repo publishes it never arrived.
                                fn bare(s: &str) -> &str {
                                    s.strip_prefix('v').unwrap_or(s)
                                }
                                if bare(&new_version) != bare(&latest_version)
                                    && failed_ref_lookups.contains_key(owner_repo)
                                    && !unverified_shortenings.contains(owner_repo)
                                {
                                    unverified_shortenings.push(owner_repo.clone());
                                }

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

        // A dry run - `--check`, `--dry-run`, and the scan interactive mode runs
        // before prompting - reaches here having written nothing, so the warning
        // describes the shortening it proposes rather than one that went out.
        let written = if options.dry_run {
            "would be written"
        } else {
            "was written"
        };
        for repo in unverified_shortenings {
            if let Some(error) = failed_ref_lookups.get(&repo) {
                result.warnings.push(format!(
                    "Could not list the refs {repo} publishes ({error}); its shortened version {written} without verifying the repo publishes that ref"
                ));
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
        // Reusable workflows are valid remote refs and share the owner/repo's tags.
        assert!(!GithubActionsUpdater::should_skip_action(
            "org/repo/.github/workflows/ci.yml"
        ));
        assert!(!GithubActionsUpdater::should_skip_action(
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
    async fn test_updates_verified_sha_pin_without_weakening_it() {
        const OLD_SHA: &str = "1111111111111111111111111111111111111111";
        const NEW_SHA: &str = "2222222222222222222222222222222222222222";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@{OLD_SHA} # v4.2.2\n"
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v5.0.0")
            .with_resolved_ref("actions/checkout", "v4.2.2", OLD_SHA)
            .with_resolved_ref("actions/checkout", "v5.0.0", NEW_SHA);
        let options = UpdateOptions::new(false, false).with_action_sha_updates(true);

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].1, "v4.2.2");
        assert_eq!(result.updated[0].2, "v5.0.0");
        assert_eq!(result.action_sha_updates.len(), 1);
        assert_eq!(result.action_sha_updates[0].current_commit, OLD_SHA);
        assert_eq!(result.action_sha_updates[0].new_commit, NEW_SHA);
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains(&format!("actions/checkout@{NEW_SHA} # v5.0.0")));
        assert!(!content.contains("actions/checkout@v5.0.0"));
    }

    #[tokio::test]
    async fn test_updates_quoted_reusable_workflow_sha_pin() {
        const OLD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const NEW_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "jobs:\n  call:\n    uses: \"rvben/clispec/.github/workflows/conformance.yml@{OLD_SHA}\" # v0.3.0\n"
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("rvben/clispec", "v0.3.1")
            .with_resolved_ref("rvben/clispec", "v0.3.0", OLD_SHA)
            .with_resolved_ref("rvben/clispec", "v0.3.1", NEW_SHA);
        let options = UpdateOptions::new(false, false).with_action_sha_updates(true);

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains(&format!(
            "uses: \"rvben/clispec/.github/workflows/conformance.yml@{NEW_SHA}\" # v0.3.1"
        )));
    }

    #[tokio::test]
    async fn test_sha_pin_opt_out_and_concrete_comment() {
        const SHA: &str = "1111111111111111111111111111111111111111";
        let updater = GithubActionsUpdater::new();

        let mut opt_out = NamedTempFile::new().unwrap();
        write!(
            opt_out,
            "steps:\n  - uses: actions/checkout@{SHA} # v4.2.2\n"
        )
        .unwrap();
        let registry = MockRegistry::new("github-releases");
        let result = updater
            .update(
                opt_out.path(),
                &registry,
                UpdateOptions::new(false, false).with_action_sha_updates(false),
            )
            .await
            .unwrap();
        assert!(result.updated.is_empty());
        // Opting out leaves the pin alone but must still report it. Counting it
        // as unchanged would let the run claim every dependency was checked.
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].status, SkipStatus::NotExamined);
        assert_eq!(result.skipped[0].reason, "action-sha-updates-off");
        assert_eq!(result.skipped[0].line_number, Some(2));
        assert_eq!(
            result.unchanged, 0,
            "a pin that was never examined is not an up-to-date dependency"
        );

        let mut unsafe_pin = NamedTempFile::new().unwrap();
        write!(
            unsafe_pin,
            "steps:\n  - uses: actions/checkout@{SHA} # v4\n"
        )
        .unwrap();
        let result = updater
            .update(
                unsafe_pin.path(),
                &registry,
                UpdateOptions::new(false, false).with_action_sha_updates(true),
            )
            .await
            .unwrap();
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].reason, "missing-version-comment");
    }

    #[tokio::test]
    async fn test_sha_pin_refuses_stale_or_forged_version_comment() {
        const PINNED_SHA: &str = "1111111111111111111111111111111111111111";
        const TAG_SHA: &str = "3333333333333333333333333333333333333333";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: actions/checkout@{PINNED_SHA} # v4.2.2\n"
        )
        .unwrap();
        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v5.0.0")
            .with_resolved_ref("actions/checkout", "v4.2.2", TAG_SHA);

        let result = GithubActionsUpdater::new()
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, false).with_action_sha_updates(true),
            )
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].reason, "version-comment-mismatch");
        assert!(
            fs::read_to_string(file.path())
                .unwrap()
                .contains(PINNED_SHA)
        );
    }

    #[tokio::test]
    async fn test_sha_pin_accepts_bare_version_comment() {
        const OLD_SHA: &str = "1111111111111111111111111111111111111111";
        const NEW_SHA: &str = "2222222222222222222222222222222222222222";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: actions/checkout@{OLD_SHA} # 4.2.2\n"
        )
        .unwrap();

        // The repo publishes `v`-prefixed tags only, as actions/checkout does.
        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v5.0.0")
            .with_resolved_ref("actions/checkout", "v4.2.2", OLD_SHA)
            .with_resolved_ref("actions/checkout", "v5.0.0", NEW_SHA);
        let options = UpdateOptions::new(false, false).with_action_sha_updates(true);

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].1, "4.2.2");
        assert_eq!(result.updated[0].2, "5.0.0");
        // The comment keeps the prefix style the file already used, matching how
        // the non-pinned path treats a bare version.
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains(&format!("actions/checkout@{NEW_SHA} # 5.0.0")),
            "content: {content}"
        );
    }

    #[tokio::test]
    async fn test_bare_version_comment_at_latest_reports_unchanged() {
        const SHA: &str = "1111111111111111111111111111111111111111";
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "steps:\n  - uses: actions/checkout@{SHA} # 5.0.0\n").unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v5.0.0")
            .with_resolved_ref("actions/checkout", "v5.0.0", SHA);
        let options = UpdateOptions::new(false, false).with_action_sha_updates(true);

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        // `5.0.0` and `v5.0.0` are the same release, so nothing was skipped over
        // and there is no downgrade to warn about.
        assert!(
            result.warnings.is_empty(),
            "warnings: {:?}",
            result.warnings
        );
        assert_eq!(result.unchanged, 1);
    }

    #[tokio::test]
    async fn test_sha_pin_accepts_v_prefixed_comment_against_bare_tags() {
        const OLD_SHA: &str = "1111111111111111111111111111111111111111";
        const NEW_SHA: &str = "2222222222222222222222222222222222222222";
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "steps:\n  - uses: acme/action@{OLD_SHA} # v1.2.3\n").unwrap();

        // The mirror image: a repo that tags without the `v` prefix.
        let registry = MockRegistry::new("github-releases")
            .with_version("acme/action", "1.3.0")
            .with_resolved_ref("acme/action", "1.2.3", OLD_SHA)
            .with_resolved_ref("acme/action", "1.3.0", NEW_SHA);
        let options = UpdateOptions::new(false, false).with_action_sha_updates(true);

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.updated.len(), 1);
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains(&format!("acme/action@{NEW_SHA} # v1.3.0")),
            "content: {content}"
        );
    }

    #[tokio::test]
    async fn test_bare_version_comment_still_refuses_forged_pin() {
        const PINNED_SHA: &str = "1111111111111111111111111111111111111111";
        const TAG_SHA: &str = "3333333333333333333333333333333333333333";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: actions/checkout@{PINNED_SHA} # 4.2.2\n"
        )
        .unwrap();
        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v5.0.0")
            .with_resolved_ref("actions/checkout", "v4.2.2", TAG_SHA);

        let result = GithubActionsUpdater::new()
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, false).with_action_sha_updates(true),
            )
            .await
            .unwrap();

        // Accepting the other prefix spelling must not weaken the check that the
        // comment actually describes the pinned commit.
        assert!(result.updated.is_empty());
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].reason, "version-comment-mismatch");
        assert!(
            fs::read_to_string(file.path())
                .unwrap()
                .contains(PINNED_SHA)
        );
    }

    #[tokio::test]
    async fn test_sha_pin_prefers_literal_comment_over_prefix_variant() {
        const LITERAL_SHA: &str = "1111111111111111111111111111111111111111";
        const VARIANT_SHA: &str = "3333333333333333333333333333333333333333";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: acme/action@{VARIANT_SHA} # 1.2.3\n"
        )
        .unwrap();
        // A repo carrying both `1.2.3` and `v1.2.3` at different commits. The
        // comment says `1.2.3`, so that tag decides, and the pin does not match it.
        let registry = MockRegistry::new("github-releases")
            .with_version("acme/action", "1.3.0")
            .with_resolved_ref("acme/action", "1.2.3", LITERAL_SHA)
            .with_resolved_ref("acme/action", "v1.2.3", VARIANT_SHA);

        let result = GithubActionsUpdater::new()
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, false).with_action_sha_updates(true),
            )
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].reason, "version-comment-mismatch");
    }

    #[tokio::test]
    async fn test_config_pin_is_satisfied_by_a_differently_spelled_comment() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        const SHA: &str = "1111111111111111111111111111111111111111";
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "steps:\n  - uses: actions/checkout@{SHA} # v5.0.0\n").unwrap();

        // The pin names a release, not a spelling, and the file already sits on
        // that release. Resolving the pin literally would ask a repo tagging
        // `v5.0.0` for a `5.0.0` it never published.
        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "5.0.0")
            .with_resolved_ref("actions/checkout", "v5.0.0", SHA);
        let mut config = UpdConfig::default();
        config.pin.insert("actions/checkout".into(), "5.0.0".into());
        let options = UpdateOptions::new(false, false)
            .with_action_sha_updates(true)
            .with_config(Arc::new(config));

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert!(result.skipped.is_empty(), "skipped: {:?}", result.skipped);
        assert_eq!(result.unchanged, 1);
        assert!(result.updated.is_empty());
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains(&format!("checkout@{SHA} # v5.0.0")),
            "content: {content}"
        );
    }

    #[tokio::test]
    async fn test_unavailable_literal_ref_does_not_verify_against_the_other_spelling() {
        const PREFIXED_SHA: &str = "1111111111111111111111111111111111111111";
        const BARE_SHA: &str = "2222222222222222222222222222222222222222";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: actions/checkout@{BARE_SHA} # v1.2.3\n"
        )
        .unwrap();

        // `v1.2.3` is the ref the author wrote and it exists, so a rate limit
        // against it is not evidence that `1.2.3` is what they meant. Reading
        // the outage as absence would bless a comment naming another commit.
        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "1.2.3")
            .with_unavailable_ref("actions/checkout", "v1.2.3")
            .with_resolved_ref("actions/checkout", "1.2.3", BARE_SHA);

        let result = GithubActionsUpdater::new()
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, false).with_action_sha_updates(true),
            )
            .await
            .unwrap();

        assert_eq!(result.errors.len(), 1, "errors: {:?}", result.errors);
        assert!(
            result.errors[0].contains("failed to verify current SHA pin"),
            "errors: {:?}",
            result.errors
        );
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains(PREFIXED_SHA) || content.contains(BARE_SHA));
    }

    #[tokio::test]
    async fn test_unavailable_styled_ref_falls_back_to_the_resolved_spelling() {
        const OLD_SHA: &str = "1111111111111111111111111111111111111111";
        const NEW_SHA: &str = "2222222222222222222222222222222222222222";
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "steps:\n  - uses: acme/action@{OLD_SHA} # v1.2.3\n").unwrap();

        // Whether `v1.3.0` names NEW_SHA is unknown while the lookup is failing,
        // so the comment takes the spelling already known to name it. Style is
        // worth less than a comment that describes the commit beside it.
        let registry = MockRegistry::new("github-releases")
            .with_version("acme/action", "1.3.0")
            .with_resolved_ref("acme/action", "v1.2.3", OLD_SHA)
            .with_resolved_ref("acme/action", "1.3.0", NEW_SHA)
            .with_unavailable_ref("acme/action", "v1.3.0");

        let result = GithubActionsUpdater::new()
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, false).with_action_sha_updates(true),
            )
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains(&format!("acme/action@{NEW_SHA} # 1.3.0")),
            "content: {content}"
        );
    }

    #[tokio::test]
    async fn test_config_pin_is_applied_when_its_spelling_names_another_commit() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        const PREFIXED_SHA: &str = "1111111111111111111111111111111111111111";
        const BARE_SHA: &str = "2222222222222222222222222222222222222222";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: actions/checkout@{PREFIXED_SHA} # v1.2.3\n"
        )
        .unwrap();

        // Both spellings exist at different commits, so the pinned one is a
        // release of its own rather than a restyling of the current comment.
        let registry = MockRegistry::new("github-releases")
            .with_resolved_ref("actions/checkout", "v1.2.3", PREFIXED_SHA)
            .with_resolved_ref("actions/checkout", "1.2.3", BARE_SHA);
        let mut config = UpdConfig::default();
        config.pin.insert("actions/checkout".into(), "1.2.3".into());
        let options = UpdateOptions::new(false, false)
            .with_action_sha_updates(true)
            .with_config(Arc::new(config));

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.unchanged, 0, "the pin was silently skipped");
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains(&format!("checkout@{BARE_SHA} # 1.2.3")),
            "content: {content}"
        );
    }

    #[tokio::test]
    async fn test_rewritten_comment_describes_the_commit_it_was_written_with() {
        const OLD_SHA: &str = "1111111111111111111111111111111111111111";
        const NEW_SHA: &str = "2222222222222222222222222222222222222222";
        const OTHER_SHA: &str = "3333333333333333333333333333333333333333";
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "steps:\n  - uses: acme/action@{OLD_SHA} # v1.2.3\n").unwrap();

        // A repo publishing both spellings of its latest release at different
        // commits. Keeping the comment's `v` style here would annotate the
        // commit `1.3.0` names with the tag `v1.3.0`, which names another.
        let registry = MockRegistry::new("github-releases")
            .with_version("acme/action", "1.3.0")
            .with_resolved_ref("acme/action", "v1.2.3", OLD_SHA)
            .with_resolved_ref("acme/action", "1.3.0", NEW_SHA)
            .with_resolved_ref("acme/action", "v1.3.0", OTHER_SHA);
        let options = UpdateOptions::new(false, false).with_action_sha_updates(true);

        let updater = GithubActionsUpdater::new();
        let result = updater
            .update(file.path(), &registry, options.clone())
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.updated.len(), 1);
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains(&format!("acme/action@{NEW_SHA} # 1.3.0")),
            "content: {content}"
        );

        // The property that matters: upd must not reject the line it just wrote.
        let second = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();
        assert!(second.errors.is_empty(), "errors: {:?}", second.errors);
        assert!(
            second.skipped.is_empty(),
            "upd rejected its own output: {:?}",
            second.skipped
        );
        assert_eq!(second.unchanged, 1);
    }

    #[tokio::test]
    async fn test_config_pinned_bare_version_resolves_against_prefixed_tag() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        const OLD_SHA: &str = "1111111111111111111111111111111111111111";
        const NEW_SHA: &str = "2222222222222222222222222222222222222222";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: actions/checkout@{OLD_SHA} # v4.2.2\n"
        )
        .unwrap();
        let registry = MockRegistry::new("github-releases")
            .with_resolved_ref("actions/checkout", "v4.2.2", OLD_SHA)
            .with_resolved_ref("actions/checkout", "v5.0.0", NEW_SHA);
        let mut config = UpdConfig::default();
        // `.updrc.toml` is hand-written too, so a bare pin must resolve the same
        // way a bare comment does.
        config.pin.insert("actions/checkout".into(), "5.0.0".into());
        let options = UpdateOptions::new(false, false)
            .with_action_sha_updates(true)
            .with_config(Arc::new(config));

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.pinned.len(), 1);
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains(&format!("actions/checkout@{NEW_SHA} # v5.0.0")),
            "content: {content}"
        );
    }

    #[tokio::test]
    async fn test_sha_pin_honors_bump_ceiling_before_resolving_target() {
        use crate::updater::BumpFilter;

        const OLD_SHA: &str = "1111111111111111111111111111111111111111";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: actions/checkout@{OLD_SHA} # v4.2.2\n"
        )
        .unwrap();
        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v5.0.0")
            .with_resolved_ref("actions/checkout", "v4.2.2", OLD_SHA);
        let options = UpdateOptions::new(false, false)
            .with_action_sha_updates(true)
            .with_bump_filter(BumpFilter {
                major: false,
                minor: true,
                patch: true,
            });

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        // The ceiling refused a known, writable update, so it is reported as
        // capped rather than as an unchanged dependency or a blocked skip.
        assert!(result.skipped.is_empty(), "skipped: {:?}", result.skipped);
        assert_eq!(result.capped.len(), 1, "capped: {:?}", result.capped);
        assert_eq!(result.capped[0].package, "actions/checkout");
        assert_eq!(result.capped[0].current, "v4.2.2");
        assert_eq!(result.capped[0].available, "v5.0.0");
        // The registry can only resolve `v4.2.2`, so a lookup of the capped
        // target would surface here: the ceiling is honored before resolving.
        assert!(result.errors.is_empty());
        assert!(fs::read_to_string(file.path()).unwrap().contains(OLD_SHA));
    }

    #[tokio::test]
    async fn test_sha_pin_refuses_floating_config_target() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        const OLD_SHA: &str = "1111111111111111111111111111111111111111";
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "steps:\n  - uses: actions/checkout@{OLD_SHA} # v4.2.2\n"
        )
        .unwrap();
        let registry = MockRegistry::new("github-releases").with_resolved_ref(
            "actions/checkout",
            "v4.2.2",
            OLD_SHA,
        );
        let mut config = UpdConfig::default();
        config.pin.insert("actions/checkout".into(), "v5".into());
        let options = UpdateOptions::new(false, false)
            .with_action_sha_updates(true)
            .with_config(Arc::new(config));

        let result = GithubActionsUpdater::new()
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert!(result.pinned.is_empty());
        assert_eq!(result.skipped[0].reason, "non-concrete-target");
        assert!(fs::read_to_string(file.path()).unwrap().contains(OLD_SHA));
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
            exclude: Vec::new(),
            ignore: vec!["actions/checkout".to_string()],
            pin: pins,
            cooldown: None,
            ..Default::default()
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

#[cfg(test)]
mod floating_ref_tests {
    use super::*;
    use crate::registry::MockRegistry;
    use std::fs;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// An action pinned to a floating major must only be moved to another
    /// floating major the repo actually publishes. sigstore/cosign-installer
    /// ships v4.x tags but no `v4` ref, so shortening v4.1.2 to `v4` yields a
    /// workflow that cannot resolve the action - and because that action only
    /// runs in a release job, the breakage surfaces at release time.
    #[tokio::test]
    async fn truncation_requires_the_floating_ref_to_exist() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"name: CI
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: sigstore/cosign-installer@v3
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("sigstore/cosign-installer", "v4.1.2")
            .with_ref_names(
                "sigstore/cosign-installer",
                &["v4.1.2", "v4.1.1", "v4.0.0", "v3.10.1", "v3", "v2"],
            );

        let updater = GithubActionsUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("sigstore/cosign-installer@v4.1.2"),
            "must fall back to the concrete version when no floating v4 exists, got: {content}"
        );
        assert!(
            !content.contains("cosign-installer@v4\n"),
            "must not write a floating ref the repo does not publish, got: {content}"
        );
        assert_eq!(result.updated.len(), 1, "{:?}", result.updated);
    }

    /// The common case must keep working: a repo that does publish the floating
    /// major keeps the short, readable pin instead of being expanded.
    #[tokio::test]
    async fn truncation_is_kept_when_the_floating_ref_exists() {
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
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v4.2.0")
            .with_ref_names("actions/checkout", &["v4.2.0", "v4", "v3", "v2"]);

        let updater = GithubActionsUpdater::new();
        updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("actions/checkout@v4\n"),
            "a published floating major must stay short, got: {content}"
        );
    }

    /// A registry with no ref information must not change behaviour: empty means
    /// unknown, not "the ref is missing". Otherwise every action would suddenly
    /// expand to full precision the moment ref lookup failed or was unsupported.
    #[tokio::test]
    async fn unknown_refs_preserve_existing_truncation() {
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
"#
        )
        .unwrap();

        // No with_ref_names: list_ref_names returns empty.
        let registry =
            MockRegistry::new("github-releases").with_version("actions/checkout", "v4.2.0");

        let updater = GithubActionsUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("actions/checkout@v4\n"),
            "unknown ref data must leave precision-matching alone, got: {content}"
        );
        assert!(
            result.warnings.is_empty(),
            "a registry that simply has no ref concept is not a failure, got: {:?}",
            result.warnings
        );
    }

    /// A ref lookup that fails is not the same as a repo with no refs. The
    /// shortened ref still gets written, because expanding to full precision
    /// based on network weather would make the output depend on whether a
    /// request happened to succeed. But the run must say the check did not
    /// happen, or a workflow pinned to a ref the repo does not publish is
    /// written in silence.
    #[tokio::test]
    async fn a_failed_ref_lookup_is_reported_rather_than_read_as_no_refs() {
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
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v4.2.0")
            .with_unavailable_ref_names("actions/checkout");

        let updater = GithubActionsUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("actions/checkout@v4\n"),
            "a failed lookup must not change which ref is written, got: {content}"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("actions/checkout")),
            "the unverified shortening must be reported, got: {:?}",
            result.warnings
        );
    }

    /// The warning says a shortened ref went out unverified, so it may only
    /// appear when one did. Under `--full-precision` the concrete version is
    /// written and no ref is ever shortened, which makes the ref list irrelevant
    /// and the warning false.
    #[tokio::test]
    async fn a_failed_ref_lookup_is_not_reported_when_nothing_was_shortened() {
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
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v4.2.0")
            .with_unavailable_ref_names("actions/checkout");

        let updater = GithubActionsUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, true))
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("actions/checkout@v4.2.0\n"),
            "full precision writes the concrete version, got: {content}"
        );
        assert!(
            result.warnings.is_empty(),
            "nothing was shortened, so there is no unverified shortening to report, got: {:?}",
            result.warnings
        );
    }

    /// A shortening the bump ceiling rejects is never written, so there is
    /// nothing unverified to report either.
    #[tokio::test]
    async fn a_failed_ref_lookup_is_not_reported_when_the_update_is_capped() {
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
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v4.2.0")
            .with_unavailable_ref_names("actions/checkout");

        let options =
            UpdateOptions::new(false, false).with_bump_filter(crate::updater::BumpFilter {
                major: false,
                minor: true,
                patch: true,
            });

        let updater = GithubActionsUpdater::new();
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("actions/checkout@v3\n"),
            "a major bump above the ceiling is not written, got: {content}"
        );
        assert!(
            result.warnings.is_empty(),
            "no shortened ref was written, so nothing is unverified, got: {:?}",
            result.warnings
        );
    }

    /// `--check`, `--dry-run` and the scan interactive mode runs before it
    /// prompts all write nothing, so the warning has to describe the shortening
    /// it proposes rather than a write that did not happen.
    #[tokio::test]
    async fn a_dry_run_reports_the_unverified_shortening_as_a_proposal() {
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
"#
        )
        .unwrap();

        let registry = MockRegistry::new("github-releases")
            .with_version("actions/checkout", "v4.2.0")
            .with_unavailable_ref_names("actions/checkout");

        let updater = GithubActionsUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(true, false))
            .await
            .unwrap();

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("actions/checkout@v3\n"),
            "a dry run writes nothing, got: {content}"
        );
        let warning = result
            .warnings
            .iter()
            .find(|w| w.contains("actions/checkout"))
            .unwrap_or_else(|| {
                panic!(
                    "the proposed shortening is still unverified and must be reported, got: {:?}",
                    result.warnings
                )
            });
        assert!(
            warning.contains("would be written"),
            "a dry run proposes the shortening, got: {warning}"
        );
        assert!(
            !warning.contains("was written"),
            "nothing was written, so the warning must not claim it was, got: {warning}"
        );
    }
}
