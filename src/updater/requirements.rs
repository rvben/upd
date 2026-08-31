use super::{
    FileType, ParsedDependency, PendingVersion, UpdateOptions, UpdateResult, Updater,
    downgrade_warning, pep440_admits, python_version_with_revalidation, read_file_safe,
    specifier_floor, unpinnable_error, unreadable_error, unrewritable_warning, write_file_atomic,
};
use crate::align::compare_versions;
use crate::registry::{DeclaredIndex, IndexChain, Registry, VersionQuery};
use crate::updater::Lang;
use crate::version::{is_prerelease_pep440, match_version_precision};
use anyhow::Result;
use futures::future::join_all;
use pep440_rs::Version as Pep440Version;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;

pub struct RequirementsUpdater {
    // Regex to match package specifications
    // Matches: package==1.0.0, package>=1.0.0, package[extra]==1.0.0, etc.
    package_re: Regex,
    // Regex to capture the full version constraint including upper bounds
    // Matches: >=1.0.0,<2 or ==1.0.0 or >=1.0,<2,!=1.5
    constraint_re: Regex,
}

/// Parsed dependency information
struct ParsedDep {
    package: String,
    /// Extras like [standard] - currently stored for future use in constraint reconstruction
    #[cfg_attr(not(test), allow(dead_code))]
    extras: String,
    /// The version the specifier floors the package at (for display purposes)
    first_version: String,
    /// The full constraint string (e.g., ">=2.8.0,<9")
    full_constraint: String,
    /// Whether `first_version` is a floor an update may carry forward, rather
    /// than a ceiling or a version the specifier rules out. See
    /// [`specifier_floor`].
    raisable: bool,
}

impl RequirementsUpdater {
    pub fn new() -> Self {
        // Match package name (with optional extras), operator, and version
        // Captures: 1=package_name, 2=extras (optional), 3=operator, 4=version
        let package_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)(\[[^\]]+\])?\s*(==|>=|<=|~=|!=|>|<)\s*([^\s,;#]+)",
        )
        .expect("Invalid regex");

        // Match the full constraint including additional constraints after commas
        // E.g., ">=2.8.0,<9" or ">=1.0.0,!=1.5.0,<2.0.0". PEP 508 allows
        // whitespace between an operator and its version, so ">= 2.0, < 3" is
        // the same set of clauses; each version runs to the next separator.
        let constraint_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)(\[[^\]]+\])?\s*((?:==|>=|<=|~=|!=|>|<)\s*[^\s#;,]+(?:\s*,\s*(?:==|>=|<=|~=|!=|>|<)\s*[^\s#;,]+)*)",
        )
        .expect("Invalid regex");

        Self {
            package_re,
            constraint_re,
        }
    }

    /// Parse index URL from a line (--index-url or -i)
    fn parse_index_url(line: &str) -> Option<String> {
        let trimmed = line.trim();

        // Check for --index-url URL or --index-url=URL
        if let Some(rest) = trimmed.strip_prefix("--index-url") {
            let rest = rest.trim_start();
            if let Some(url) = rest.strip_prefix('=') {
                return Some(url.trim().to_string());
            }
            if !rest.is_empty() && !rest.starts_with('-') {
                return Some(rest.split_whitespace().next()?.to_string());
            }
        }

        // Check for -i URL
        if let Some(rest) = trimmed.strip_prefix("-i") {
            let rest = rest.trim_start();
            if !rest.is_empty() && !rest.starts_with('-') {
                return Some(rest.split_whitespace().next()?.to_string());
            }
        }

        None
    }

    /// Parse extra index URLs from a line (--extra-index-url)
    fn parse_extra_index_url(line: &str) -> Option<String> {
        let trimmed = line.trim();

        // Check for --extra-index-url URL or --extra-index-url=URL
        if let Some(rest) = trimmed.strip_prefix("--extra-index-url") {
            let rest = rest.trim_start();
            if let Some(url) = rest.strip_prefix('=') {
                return Some(url.trim().to_string());
            }
            if !rest.is_empty() && !rest.starts_with('-') {
                return Some(rest.split_whitespace().next()?.to_string());
            }
        }

        None
    }

    /// Extract all index URLs from a requirements file content
    /// Returns (primary_index_url, extra_index_urls)
    fn extract_index_urls(content: &str) -> (Option<String>, Vec<String>) {
        let mut primary_index: Option<String> = None;
        let mut extra_indexes: Vec<String> = Vec::new();

        for line in content.lines() {
            if let Some(url) = Self::parse_index_url(line) {
                primary_index = Some(url);
            }
            if let Some(url) = Self::parse_extra_index_url(line) {
                extra_indexes.push(url);
            }
        }

        (primary_index, extra_indexes)
    }

    /// The indexes a requirements file declares for itself, in lookup order,
    /// following pip: `--index-url` replaces the default index and is consulted
    /// first, `--extra-index-url` entries are added after it. A file that only
    /// adds extra indexes keeps the default index as its last resort; without
    /// that, every public dependency in such a file would go unresolved.
    /// Empty when the file declares nothing, so the process default applies.
    fn declared_indexes(content: &str) -> Vec<DeclaredIndex> {
        let (primary_index, extra_indexes) = Self::extract_index_urls(content);
        let replaces_default = primary_index.is_some();

        let mut chain: Vec<DeclaredIndex> = primary_index
            .iter()
            .chain(extra_indexes.iter())
            .map(|url| DeclaredIndex::url(None, url))
            .collect();
        if !chain.is_empty() && !replaces_default {
            chain.push(DeclaredIndex::default_registry());
        }
        chain
    }

    fn parse_line(&self, line: &str) -> Option<ParsedDep> {
        // Skip comments and empty lines
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            return None;
        }

        // Handle inline comments
        let code_part = line.split('#').next().unwrap_or(line);

        // Try to capture the full constraint first
        if let Some(caps) = self.constraint_re.captures(code_part) {
            let package = caps.get(1).unwrap().as_str().to_string();
            let extras = caps.get(2).map_or("", |m| m.as_str()).to_string();
            let full_constraint = caps.get(3).unwrap().as_str().to_string();

            // Extract the floor version for display
            let floor = self.floor(code_part);
            let raisable = floor.as_ref().is_some_and(|f| f.raisable);
            let first_version = floor
                .map(|f| code_part[f.range].to_string())
                .unwrap_or_default();

            return Some(ParsedDep {
                package,
                extras,
                first_version,
                full_constraint,
                raisable,
            });
        }

        None
    }

    /// Check if constraint is a simple single-version constraint that doesn't need
    /// constraint-aware lookup (i.e., no upper bounds that could be violated)
    fn is_simple_constraint(constraint: &str) -> bool {
        // If there are multiple constraints (comma-separated), need constraint-aware lookup
        if constraint.contains(',') {
            return false;
        }

        // If the constraint has an upper-bound operator, need constraint-aware lookup
        // Examples: "<4.2", "<=2.0", "~=1.4" (compatible release - allows only patch updates)
        if constraint.starts_with('<')
            || constraint.starts_with("<=")
            || constraint.starts_with("~=")
        {
            return false;
        }

        // Also check for != which could affect version selection
        if constraint.starts_with("!=") {
            return false;
        }

        // Simple constraints like "==1.0.0", ">=1.0.0", ">1.0.0" are fine
        true
    }

    /// Where this line's floor version sits, and whether it is one an update may
    /// carry forward.
    ///
    /// Falls back to the single-operator match for anything [`specifier_floor`]
    /// cannot place, which keeps a line with no recognizable constraint reading
    /// as it always has. That fallback reads its own operator rather than
    /// assuming a floor, so `pkg<v2` is no more rewritable than `pkg<2`.
    fn floor(&self, line: &str) -> Option<super::SpecifierFloor> {
        let code_part = line.split('#').next().unwrap_or(line);
        let constraint = self
            .constraint_re
            .captures(code_part)
            .and_then(|c| c.get(3));
        constraint
            .and_then(|m| specifier_floor(m.as_str(), m.start()))
            .or_else(|| {
                let caps = self.package_re.captures(code_part)?;
                Some(super::SpecifierFloor {
                    range: caps.get(4).unwrap().range(),
                    raisable: matches!(caps.get(3).map(|m| m.as_str()), Some("==" | ">=" | "~=")),
                })
            })
    }

    fn update_line(&self, line: &str, new_version: &str) -> String {
        if let Some(range) = self.floor(line).map(|f| f.range) {
            // Only replace the floor version itself, preserving everything else
            // (package name, extras, operator, AND any other constraints like ,<6).
            // Known limitation: if the new version string is a different length than the
            // old one, any trailing inline `# comment` will shift left or right by that
            // difference. Column-aligned comment blocks are not preserved.
            let mut result = line.to_string();
            result.replace_range(range, new_version);
            result
        } else {
            line.to_string()
        }
    }
}

impl Default for RequirementsUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for RequirementsUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut result = UpdateResult::default();

        // Indexes declared inline take over from the process default only as
        // far as pip would let them: `--index-url` replaces it, an
        // `--extra-index-url` on its own is layered in front of it.
        let chain = IndexChain::new(Self::declared_indexes(&content), &HashMap::new(), registry);
        let effective_registry: &dyn Registry = match &chain {
            Some(chain) => chain,
            None => registry,
        };

        // First pass: collect all packages that need version checks
        let lines: Vec<&str> = content.lines().collect();
        let mut parsed_deps: Vec<(usize, &str, ParsedDep)> = Vec::new();

        for (line_idx, line) in lines.iter().enumerate() {
            if let Some(parsed) = self.parse_line(line) {
                parsed_deps.push((line_idx, line, parsed));
            }
        }

        // Separate packages into ignored, pinned, and to-be-fetched
        let mut ignored_packages: Vec<(usize, String, String)> = Vec::new();
        let mut pinned_packages: Vec<(usize, String, String, String)> = Vec::new();
        let mut fetch_deps: Vec<(usize, &str, &ParsedDep)> = Vec::new();

        for (line_idx, line, parsed) in &parsed_deps {
            if options.is_package_filtered_out(&parsed.package) {
                result.unchanged += 1;
                continue;
            }

            // Check if package should be ignored
            if options.should_ignore(&parsed.package) {
                ignored_packages.push((
                    *line_idx,
                    parsed.package.clone(),
                    parsed.first_version.clone(),
                ));
                continue;
            }

            // Check if package has a pinned version
            if let Some(pinned_version) = options.get_pinned_version(&parsed.package) {
                if !parsed.raisable {
                    // The pin was configured and cannot be written, so the file
                    // does not say what the config says it should. That is a
                    // failed instruction, not a note.
                    result.errors.push(unpinnable_error(
                        &parsed.package,
                        pinned_version,
                        &parsed.full_constraint,
                    ));
                    continue;
                }
                pinned_packages.push((
                    *line_idx,
                    parsed.package.clone(),
                    parsed.first_version.clone(),
                    pinned_version.to_string(),
                ));
                continue;
            }

            // Reject version tokens that are not valid PEP 440 versions (e.g.
            // template placeholders like `%version%` or garbage like `abc`).
            // Updating such a line would silently destroy intentional content.
            if parsed.first_version.parse::<Pep440Version>().is_err() {
                result.warnings.push(format!(
                    "skipping {}: current version \"{}\" is not a valid PEP 440 version",
                    parsed.package, parsed.first_version
                ));
                continue;
            }

            fetch_deps.push((*line_idx, *line, parsed));
        }

        // Add ignored packages to result
        for (line_idx, package, version) in ignored_packages {
            result.ignored.push((package, version, Some(line_idx + 1)));
        }

        // Fetch versions only for non-ignored, non-pinned packages.
        // When the current version is a pre-release, request the latest pre-release
        // so we do not silently promote the user to a stable release.
        let version_futures: Vec<_> = fetch_deps
            .iter()
            .map(|(_, _, parsed)| async {
                if !parsed.raisable {
                    // Nothing here can be rewritten, so the only question left
                    // is whether the newest release is one this specifier
                    // already admits. Asking for the newest release *matching*
                    // it would answer that with itself.
                    effective_registry.get_latest_version(&parsed.package).await
                } else if is_prerelease_pep440(&parsed.first_version) {
                    python_version_with_revalidation(
                        effective_registry,
                        &parsed.package,
                        &parsed.first_version,
                        VersionQuery::IncludingPrereleases,
                    )
                    .await
                } else if Self::is_simple_constraint(&parsed.full_constraint) {
                    python_version_with_revalidation(
                        effective_registry,
                        &parsed.package,
                        &parsed.first_version,
                        VersionQuery::Stable,
                    )
                    .await
                } else {
                    python_version_with_revalidation(
                        effective_registry,
                        &parsed.package,
                        &parsed.first_version,
                        VersionQuery::Matching(&parsed.full_constraint),
                    )
                    .await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Build a map of line index to version result
        let mut version_map: std::collections::HashMap<usize, PendingVersion> =
            std::collections::HashMap::new();
        for ((line_idx, _, _), version_result) in fetch_deps.iter().zip(version_results) {
            version_map.insert(*line_idx, PendingVersion::Registry(version_result));
        }

        // Add pinned versions to version_map; they are recorded during the apply pass.
        for (line_idx, _package, _current_version, pinned_version) in pinned_packages {
            version_map.insert(line_idx, PendingVersion::Pinned(pinned_version));
        }

        // Second pass: apply updates
        let mut new_lines = Vec::new();
        let mut modified = false;

        for (line_idx, line) in lines.iter().enumerate() {
            let line_num = line_idx + 1; // 1-indexed for display

            if let Some(parsed) = self.parse_line(line) {
                // A specifier with no floor to raise (a ceiling like "<6", an
                // exclusive bound like ">2.0", an exclusion like "!=1.5") is
                // still one upd can read: saying whether it is current is the
                // difference between a specifier doing its job and one that has
                // quietly frozen a dependency.
                if !parsed.raisable {
                    new_lines.push(line.to_string());
                    match version_map.remove(&line_idx) {
                        Some(PendingVersion::Registry(Ok(latest))) => {
                            match pep440_admits(&parsed.full_constraint, &latest) {
                                Some(true) => result.unchanged += 1,
                                Some(false) => result.warnings.push(unrewritable_warning(
                                    &parsed.package,
                                    &latest,
                                    &parsed.full_constraint,
                                )),
                                None => result.errors.push(unreadable_error(
                                    &parsed.package,
                                    &parsed.full_constraint,
                                )),
                            }
                        }
                        Some(PendingVersion::Registry(Err(e))) => {
                            result.errors.push(format!("{}: {}", parsed.package, e));
                        }
                        // Filtered out, ignored, or refused as an unpinnable
                        // pin: all of them are already recorded.
                        Some(PendingVersion::Pinned(_)) | None => {}
                    }
                    continue;
                }

                if let Some(version_result) = version_map.remove(&line_idx) {
                    match version_result {
                        PendingVersion::Pinned(pinned_version) => {
                            let matched_version = if options.full_precision {
                                pinned_version.clone()
                            } else {
                                match_version_precision(&parsed.first_version, &pinned_version)
                            };
                            if matched_version != parsed.first_version {
                                result.pinned.push((
                                    parsed.package.clone(),
                                    parsed.first_version.clone(),
                                    matched_version.clone(),
                                    Some(line_num),
                                ));
                                new_lines.push(self.update_line(line, &matched_version));
                                modified = true;
                            } else {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            }
                        }
                        PendingVersion::Registry(Ok(latest_version)) => {
                            // When the current version is a pre-release, we fetched the latest
                            // pre-release. If the registry returned a stable version instead
                            // (no newer pre-release exists), refuse silent promotion to stable.
                            let current_is_prerelease = is_prerelease_pep440(&parsed.first_version);
                            if current_is_prerelease && !is_prerelease_pep440(&latest_version) {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                                continue;
                            }

                            // Apply cooldown policy before precision-matching so we select
                            // from the full version list, not just the pre-matched string.
                            let constraints_for_cooldown = if parsed.full_constraint.is_empty() {
                                None
                            } else {
                                Some(parsed.full_constraint.as_str())
                            };
                            let (outcome, note) = crate::updater::apply_cooldown(
                                effective_registry,
                                &parsed.package,
                                &parsed.first_version,
                                &latest_version,
                                constraints_for_cooldown,
                                current_is_prerelease,
                                &options,
                            )
                            .await;
                            if let Some(msg) = note {
                                options.note_cooldown_unavailable(&msg);
                            }
                            let (latest_version, held_back_record) = match outcome {
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
                                        parsed.package.clone(),
                                        parsed.first_version.clone(),
                                        skipped_version,
                                        skipped_published_at,
                                    ));
                                    new_lines.push(line.to_string());
                                    continue;
                                }
                            };

                            // Match the precision of the original version (unless full precision requested)
                            let matched_version = if options.full_precision {
                                latest_version.clone()
                            } else {
                                match_version_precision(&parsed.first_version, &latest_version)
                            };
                            if matched_version != parsed.first_version {
                                // Refuse to write a downgrade.
                                if compare_versions(
                                    &matched_version,
                                    &parsed.first_version,
                                    Lang::Python,
                                ) != std::cmp::Ordering::Greater
                                {
                                    result.warnings.push(downgrade_warning(
                                        &parsed.package,
                                        &matched_version,
                                        &parsed.first_version,
                                    ));
                                    result.unchanged += 1;
                                    new_lines.push(line.to_string());
                                } else if !options
                                    .allows_bump(&parsed.first_version, &matched_version)
                                {
                                    // Bump level exceeds the --only-bump/--max-bump
                                    // ceiling: leave the line untouched.
                                    result.record_capped(
                                        &parsed.package,
                                        &parsed.first_version,
                                        &matched_version,
                                        Some(line_num),
                                    );
                                    new_lines.push(line.to_string());
                                } else {
                                    result.updated.push((
                                        parsed.package.clone(),
                                        parsed.first_version.clone(),
                                        matched_version.clone(),
                                        Some(line_num),
                                    ));
                                    if let Some((skipped_version, skipped_published_at)) =
                                        held_back_record
                                    {
                                        result.held_back.push((
                                            parsed.package.clone(),
                                            parsed.first_version.clone(),
                                            matched_version.clone(),
                                            skipped_version,
                                            skipped_published_at,
                                        ));
                                    }
                                    new_lines.push(self.update_line(line, &matched_version));
                                    modified = true;
                                }
                            } else {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            }
                        }
                        PendingVersion::Registry(Err(e)) => {
                            result.errors.push(format!("{}: {}", parsed.package, e));
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
            // Preserve original line ending style
            let line_ending = if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };

            let mut new_content = new_lines.join(line_ending);

            // Preserve trailing newline if present
            if content.ends_with('\n') || content.ends_with("\r\n") {
                new_content.push_str(line_ending);
            }

            write_file_atomic(path, &new_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::Requirements
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let mut deps = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            if let Some(parsed) = self.parse_line(line) {
                let has_upper_bound = !Self::is_simple_constraint(&parsed.full_constraint);
                deps.push(ParsedDependency {
                    name: parsed.package,
                    version: parsed.first_version,
                    line_number: Some(line_idx + 1),
                    has_upper_bound,
                    is_bumpable: true,
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
    use std::io::Write;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_line() {
        let updater = RequirementsUpdater::new();

        let parsed = updater.parse_line("requests==2.28.0").unwrap();
        assert_eq!(parsed.package, "requests");
        assert_eq!(parsed.extras, "");
        assert_eq!(parsed.first_version, "2.28.0");
        assert_eq!(parsed.full_constraint, "==2.28.0");

        let parsed = updater.parse_line("uvicorn[standard]==0.20.0").unwrap();
        assert_eq!(parsed.package, "uvicorn");
        assert_eq!(parsed.extras, "[standard]");
        assert_eq!(parsed.first_version, "0.20.0");

        let parsed = updater.parse_line("django>=4.0.0").unwrap();
        assert_eq!(parsed.package, "django");
        assert_eq!(parsed.first_version, "4.0.0");
        assert_eq!(parsed.full_constraint, ">=4.0.0");

        // Test constraint with upper bound
        let parsed = updater.parse_line("pytest>=2.8.0,<9").unwrap();
        assert_eq!(parsed.package, "pytest");
        assert_eq!(parsed.first_version, "2.8.0");
        assert_eq!(parsed.full_constraint, ">=2.8.0,<9");

        // Test multiple constraints
        let parsed = updater.parse_line("foo>=1.0.0,!=1.5.0,<2.0.0").unwrap();
        assert_eq!(parsed.package, "foo");
        assert_eq!(parsed.first_version, "1.0.0");
        assert_eq!(parsed.full_constraint, ">=1.0.0,!=1.5.0,<2.0.0");

        assert!(updater.parse_line("# comment").is_none());
        assert!(updater.parse_line("").is_none());
        assert!(updater.parse_line("-r other.txt").is_none());
    }

    #[test]
    fn test_parse_index_url() {
        // --index-url with space
        assert_eq!(
            RequirementsUpdater::parse_index_url("--index-url https://pypi.example.com/simple"),
            Some("https://pypi.example.com/simple".to_string())
        );

        // --index-url with equals
        assert_eq!(
            RequirementsUpdater::parse_index_url("--index-url=https://pypi.example.com/simple"),
            Some("https://pypi.example.com/simple".to_string())
        );

        // -i short form
        assert_eq!(
            RequirementsUpdater::parse_index_url("-i https://pypi.example.com/simple"),
            Some("https://pypi.example.com/simple".to_string())
        );

        // URL with credentials
        assert_eq!(
            RequirementsUpdater::parse_index_url(
                "--index-url https://user:pass@pypi.example.com/simple"
            ),
            Some("https://user:pass@pypi.example.com/simple".to_string())
        );

        // Non-index lines
        assert!(RequirementsUpdater::parse_index_url("requests==2.28.0").is_none());
        assert!(RequirementsUpdater::parse_index_url("# comment").is_none());
        assert!(RequirementsUpdater::parse_index_url("-r other.txt").is_none());
    }

    #[test]
    fn test_parse_extra_index_url() {
        // --extra-index-url with space
        assert_eq!(
            RequirementsUpdater::parse_extra_index_url(
                "--extra-index-url https://extra.example.com/simple"
            ),
            Some("https://extra.example.com/simple".to_string())
        );

        // --extra-index-url with equals
        assert_eq!(
            RequirementsUpdater::parse_extra_index_url(
                "--extra-index-url=https://extra.example.com/simple"
            ),
            Some("https://extra.example.com/simple".to_string())
        );

        // Non-extra-index lines
        assert!(
            RequirementsUpdater::parse_extra_index_url(
                "--index-url https://pypi.example.com/simple"
            )
            .is_none()
        );
        assert!(RequirementsUpdater::parse_extra_index_url("requests==2.28.0").is_none());
    }

    #[test]
    fn test_extract_index_urls() {
        let content = r#"
--index-url https://pypi.example.com/simple
--extra-index-url https://extra1.example.com/simple
--extra-index-url https://extra2.example.com/simple
requests==2.28.0
flask>=2.0.0
"#;

        let (primary, extra) = RequirementsUpdater::extract_index_urls(content);
        assert_eq!(primary, Some("https://pypi.example.com/simple".to_string()));
        assert_eq!(extra.len(), 2);
        assert_eq!(extra[0], "https://extra1.example.com/simple");
        assert_eq!(extra[1], "https://extra2.example.com/simple");

        // No index URLs
        let content = "requests==2.28.0\nflask>=2.0.0";
        let (primary, extra) = RequirementsUpdater::extract_index_urls(content);
        assert!(primary.is_none());
        assert!(extra.is_empty());
    }

    fn url(u: &str) -> DeclaredIndex {
        DeclaredIndex::url(None, u)
    }

    #[test]
    fn declared_indexes_are_empty_when_the_file_declares_none() {
        assert!(RequirementsUpdater::declared_indexes("requests==2.28.0\nflask>=2.0.0").is_empty());
    }

    #[test]
    fn index_url_replaces_the_default_and_leads_the_extras() {
        let chain = RequirementsUpdater::declared_indexes(
            r#"
--extra-index-url https://extra1.example.com/simple
--index-url https://pypi.example.com/simple
--extra-index-url https://extra2.example.com/simple
requests==2.28.0
"#,
        );
        assert_eq!(
            chain,
            vec![
                url("https://pypi.example.com/simple"),
                url("https://extra1.example.com/simple"),
                url("https://extra2.example.com/simple"),
            ]
        );
    }

    #[test]
    fn extra_index_url_alone_keeps_the_default_index_last() {
        let chain = RequirementsUpdater::declared_indexes(
            r#"
--extra-index-url https://nexus.example.com/repository/private/simple
requests==2.28.0
private-package>=1.0.908
"#,
        );
        assert_eq!(
            chain,
            vec![
                url("https://nexus.example.com/repository/private/simple"),
                DeclaredIndex::default_registry(),
            ]
        );
    }

    /// A file that only adds an `--extra-index-url` still resolves public
    /// packages from the default registry; the private index answers for the
    /// packages it carries.
    #[tokio::test]
    async fn update_layers_extra_index_url_over_the_default_registry() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let private = MockServer::start().await;
        for p in [
            "/simple/requests/",
            "/pypi/requests/json",
            "/simple/private-package/",
        ] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(404))
                .mount(&private)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/pypi/private-package/json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"releases": {"1.0.909": [{"yanked": false}]}}"#),
            )
            .mount(&private)
            .await;

        let mut file = NamedTempFile::with_suffix(".txt").unwrap();
        write!(
            file,
            "--extra-index-url {}/simple/\nrequests==2.28.0\nprivate-package==1.0.908\n",
            private.uri()
        )
        .unwrap();

        let default = MockRegistry::new("pypi").with_version("requests", "2.32.0");
        let updater = RequirementsUpdater::new();
        let result = updater
            .update(file.path(), &default, UpdateOptions::new(true, false))
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        let mut updated: Vec<(&str, &str)> = result
            .updated
            .iter()
            .map(|(p, _, new, _)| (p.as_str(), new.as_str()))
            .collect();
        updated.sort();
        assert_eq!(
            updated,
            vec![("private-package", "1.0.909"), ("requests", "2.32.0")]
        );
    }

    #[tokio::test]
    async fn stale_cached_latest_below_current_is_revalidated() {
        use crate::cache::{Cache, CachedRegistry};
        use std::sync::Mutex;

        let mut file = NamedTempFile::with_suffix(".txt").unwrap();
        writeln!(file, "private-package==1.1.1").unwrap();

        let cache = Arc::new(Mutex::new(Cache::default()));
        cache
            .lock()
            .unwrap()
            .set("pypi", "private-package", "1.1.0".to_string());
        let registry = CachedRegistry::new(
            MockRegistry::new("pypi").with_version("private-package", "1.1.2"),
            Arc::clone(&cache),
            true,
        );

        let result = RequirementsUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(true, false))
            .await
            .unwrap();

        assert!(
            result.warnings.is_empty(),
            "warnings: {:?}",
            result.warnings
        );
        assert_eq!(
            result.updated,
            vec![(
                "private-package".to_string(),
                "1.1.1".to_string(),
                "1.1.2".to_string(),
                Some(1),
            )]
        );
        assert_eq!(
            cache.lock().unwrap().get("pypi", "private-package"),
            Some("1.1.2".to_string())
        );
    }

    #[test]
    fn test_is_simple_constraint() {
        // Simple constraints - no upper bound, no exclusions
        assert!(RequirementsUpdater::is_simple_constraint("==1.0.0"));
        assert!(RequirementsUpdater::is_simple_constraint(">=1.0.0"));
        assert!(RequirementsUpdater::is_simple_constraint(">1.0.0"));

        // Multiple constraints with comma
        assert!(!RequirementsUpdater::is_simple_constraint(">=1.0.0,<2.0.0"));
        assert!(!RequirementsUpdater::is_simple_constraint(">=2.8.0,<9"));

        // Upper-bound operators (need constraint-aware lookup)
        assert!(!RequirementsUpdater::is_simple_constraint("<4.2"));
        assert!(!RequirementsUpdater::is_simple_constraint("<=2.0"));
        assert!(!RequirementsUpdater::is_simple_constraint("~=1.4"));

        // Exclusion operator
        assert!(!RequirementsUpdater::is_simple_constraint("!=1.5.0"));
    }

    /// Which lines offer a floor an update may carry forward. A ceiling never
    /// had one; `>1.0.0` and `!=1.5.0` hold a version each, and each is one the
    /// specifier rules out, so raising either writes a line that excludes the
    /// release it was raised to.
    #[test]
    fn a_line_knows_whether_its_version_is_a_floor_to_raise() {
        let updater = RequirementsUpdater::new();
        let raisable = |line: &str| updater.parse_line(line).unwrap().raisable;

        for line in [
            "pkg>=1.0.0",
            "pkg==1.0.0",
            "pkg~=1.4",
            "pkg>=1.0.0,<2.0.0",
            "pkg<2.0.0,>=1.0.0",
            "pkg>=2.8.0,<9",
        ] {
            assert!(raisable(line), "{line}");
        }
        for line in [
            "pkg<6",
            "pkg<4.2",
            "pkg<=5.0",
            "pkg<=2.0.0",
            "pkg>1.0.0",
            "pkg>1.0.0,<2.0.0",
            "pkg!=1.5.0",
            "pkg<6,!=5.0",
        ] {
            assert!(!raisable(line), "{line}");
        }
    }

    #[test]
    fn test_update_line() {
        let updater = RequirementsUpdater::new();

        assert_eq!(
            updater.update_line("requests==2.28.0", "2.31.0"),
            "requests==2.31.0"
        );

        assert_eq!(
            updater.update_line("requests==2.28.0  # HTTP library", "2.31.0"),
            "requests==2.31.0  # HTTP library"
        );

        assert_eq!(
            updater.update_line("uvicorn[standard]==0.20.0", "0.24.0"),
            "uvicorn[standard]==0.24.0"
        );

        // Constraint preservation - upper bound should be kept
        assert_eq!(
            updater.update_line("django>=4.0,<6", "5.2"),
            "django>=5.2,<6"
        );

        assert_eq!(
            updater.update_line("pytest>=2.8.0,<9", "8.0.0"),
            "pytest>=8.0.0,<9"
        );

        // Multiple constraints should all be preserved
        assert_eq!(
            updater.update_line("foo>=1.0.0,!=1.5.0,<2.0.0", "1.8.0"),
            "foo>=1.8.0,!=1.5.0,<2.0.0"
        );
    }

    #[test]
    fn test_update_line_inline_comment_preserved_same_length() {
        // When the version length does not change, the comment is preserved verbatim.
        let updater = RequirementsUpdater::new();
        let line = "requests==2.28.0  # HTTP library";
        let updated = updater.update_line(line, "2.31.0");
        assert_eq!(updated, "requests==2.31.0  # HTTP library");
    }

    #[test]
    fn test_update_line_inline_comment_shifts_on_length_change() {
        // Known limitation: when the new version is shorter or longer, the trailing
        // comment shifts by the version length delta. Column alignment is not preserved.
        let updater = RequirementsUpdater::new();

        // Shorter version (5 chars → 3 chars): comment moves 2 columns left.
        let line = "requests==2.28.0  # HTTP library";
        let updated = updater.update_line(line, "3.0");
        assert_eq!(updated, "requests==3.0  # HTTP library");

        // Longer version (5 chars → 7 chars): comment moves 2 columns right.
        let line2 = "flask==2.3.0  # web framework";
        let updated2 = updater.update_line(line2, "3.1.0.1");
        assert_eq!(updated2, "flask==3.1.0.1  # web framework");
    }

    // Integration tests using MockRegistry and temp files

    #[tokio::test]
    async fn test_update_requirements_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        writeln!(file, "flask>=2.0.0").unwrap();
        writeln!(file, "# A comment").unwrap();
        writeln!(file, "django>=4.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "5.0.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 3);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
        assert!(result.ignored.is_empty());
        assert!(result.pinned.is_empty());

        // Verify file contents
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests==2.31.0"));
        assert!(contents.contains("flask>=3.0.0"));
        assert!(contents.contains("django>=5.0"));
        assert!(contents.contains("# A comment"));
    }

    #[tokio::test]
    async fn test_update_requirements_dry_run() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified in dry-run mode
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests==2.28.0"));
        assert!(!contents.contains("2.31.0"));
    }

    #[tokio::test]
    async fn test_update_requirements_full_precision() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "flask>=2.0").unwrap();

        let registry = MockRegistry::new("PyPI").with_version("flask", "3.1.5");

        let updater = RequirementsUpdater::new();

        // Without full precision - should preserve 2-component format
        let options = UpdateOptions::new(false, false);
        updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("flask>=3.1"),
            "Should have 2-component version"
        );
        assert!(
            !contents.contains("3.1.5"),
            "Should not have full precision"
        );

        // Reset file content for second test
        std::fs::write(file.path(), "flask>=2.0\n").unwrap();

        // With full precision - should output full version
        let options = UpdateOptions::new(false, true);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();
        assert_eq!(result.updated.len(), 1);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("flask>=3.1.5"),
            "Should have full precision"
        );
    }

    #[tokio::test]
    async fn test_update_requirements_preserves_comments() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "# Python dependencies").unwrap();
        writeln!(file, "requests==2.28.0  # HTTP library").unwrap();
        writeln!(file).unwrap();
        writeln!(file, "# Web framework").unwrap();
        writeln!(file, "flask>=2.0.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);

        updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("# Python dependencies"));
        assert!(contents.contains("# HTTP library"));
        assert!(contents.contains("# Web framework"));
    }

    #[tokio::test]
    async fn test_update_requirements_unchanged_packages() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.31.0").unwrap();

        // Registry returns same version - no update needed
        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 1);
    }

    #[tokio::test]
    async fn test_update_requirements_with_extras() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "uvicorn[standard]==0.20.0").unwrap();

        let registry = MockRegistry::new("PyPI").with_version("uvicorn", "0.24.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("uvicorn[standard]==0.24.0"));
    }

    #[tokio::test]
    async fn test_update_requirements_line_numbers() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "# Header comment").unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        writeln!(file, "flask>=2.0.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Verify line numbers are tracked (1-indexed)
        let requests_update = result
            .updated
            .iter()
            .find(|(name, _, _, _)| name == "requests")
            .unwrap();
        assert_eq!(requests_update.3, Some(2)); // Line 2

        let flask_update = result
            .updated
            .iter()
            .find(|(name, _, _, _)| name == "flask")
            .unwrap();
        assert_eq!(flask_update.3, Some(3)); // Line 3
    }

    #[tokio::test]
    async fn test_update_requirements_registry_error() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "nonexistent-package==1.0.0").unwrap();

        // Registry doesn't have this package
        let registry = MockRegistry::new("PyPI");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent-package"));
    }

    #[tokio::test]
    async fn test_upper_bound_only_constraint_not_updated() {
        // Regression test: upper-bound-only constraints like "<6" should NOT be updated
        // because that would only restrict the version range, not expand it
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "django<6").unwrap();
        writeln!(file, "flask<=3.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("django", "5.2")
            .with_version("flask", "2.3.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Neither package should be updated - they have upper-bound-only constraints
        assert_eq!(
            result.updated.len(),
            0,
            "Upper-bound-only constraints should not be updated"
        );
        assert_eq!(
            result.unchanged, 2,
            "Both packages should be marked as unchanged"
        );

        // Verify file contents were NOT modified
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("django<6"),
            "django constraint should remain unchanged"
        );
        assert!(
            contents.contains("flask<=3.0"),
            "flask constraint should remain unchanged"
        );
    }

    /// A ceiling holds no floor to raise, but upd can still say the dependency
    /// has been frozen below the newest release. Reading a specifier it will not
    /// rewrite is the difference between a ceiling doing its job and one that
    /// has quietly stopped a package receiving updates.
    #[tokio::test]
    async fn a_ceiling_the_newest_release_is_outside_is_reported_not_rewritten() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "django<6").unwrap();

        // The constrained answer is what a lookup *matching* the specifier would
        // return, and it is inside the ceiling by construction: asking that
        // question of a specifier upd cannot rewrite answers it with itself and
        // reports a frozen dependency as current.
        let registry = MockRegistry::new("PyPI")
            .with_version("django", "6.1.0")
            .with_constrained("django", "<6", "5.2.0");

        let result = RequirementsUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 0, "6.1.0 is outside '<6', not current");
        assert_eq!(
            result.warnings,
            vec!["django: 6.1.0 is available, but '<6' is a range upd does not rewrite"]
        );
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("django<6"));
    }

    /// A specifier PEP 440 refuses to read is a third answer, and it is an error
    /// rather than a warning: a warning leaves the run exiting 0 with a green
    /// tick over a dependency nothing has looked at. `<2.*` is not a legal PEP
    /// 440 clause, though the `1.0` beside it is a perfectly good version, so
    /// the line survives the check that the floor itself is readable.
    #[tokio::test]
    async fn a_specifier_pep_440_cannot_read_is_an_error() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "django>1.0,<2.*").unwrap();

        let registry = MockRegistry::new("PyPI").with_version("django", "6.1.0");

        let result = RequirementsUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 0);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(
            result.errors,
            vec!["cannot check 'django': '>1.0,<2.*' is not a version range upd can read"]
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("django>1.0,<2.*"));
    }

    /// `urllib3>2.0` names the one release the author refuses. Raising it wrote
    /// `urllib3>2.7` with 2.7 the newest release, and the next run could not
    /// resolve the file upd had just written.
    #[tokio::test]
    async fn an_exclusive_lower_bound_is_never_raised() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "urllib3>2.0").unwrap();
        writeln!(file, "chardet>4.0,<6.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("urllib3", "2.7.0")
            .with_version("chardet", "5.2.0");

        let result = RequirementsUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 2, "both specifiers admit their newest");
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("urllib3>2.0"));
        assert!(contents.contains("chardet>4.0,<6.0"));
    }

    /// PEP 508 lets whitespace separate an operator from its version, and
    /// `django <= 5.0` is a ceiling like any other. Reading the operator without
    /// the version behind it turned every spaced bound into a specifier upd
    /// could not read, and a valid file into exit 2.
    #[tokio::test]
    async fn a_spaced_bound_is_read_as_the_bound_it_is() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "django <= 5.0").unwrap();
        writeln!(file, "flask < 3.0").unwrap();
        writeln!(file, "pkg != 1.5.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("django", "6.1.0")
            .with_version("flask", "2.3.0")
            .with_version("pkg", "2.0.0");

        let result = RequirementsUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 2, "flask and pkg admit their newest");
        assert_eq!(
            result.warnings,
            vec!["django: 6.1.0 is available, but '<= 5.0' is a range upd does not rewrite"]
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("django <= 5.0"));
        assert!(contents.contains("flask < 3.0"));
        assert!(contents.contains("pkg != 1.5.0"));
    }

    /// A spaced range is still a range. Reading `requests >= 2.0, < 2.20` as a
    /// bare `>=` dropped the ceiling from the lookup, and the run wrote
    /// `requests >= 2.34, < 2.20`: a floor above its own ceiling, which nothing
    /// can install.
    #[tokio::test]
    async fn a_spaced_range_keeps_its_ceiling() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests >= 2.0, < 2.20").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.34.0")
            .with_constrained("requests", ">= 2.0, < 2.20", "2.19.1");

        let result = RequirementsUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "requests");
        assert_eq!(result.updated[0].1, "2.0");
        assert_eq!(
            result.updated[0].2, "2.19",
            "the newest release the range admits"
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests >= 2.19, < 2.20"), "{contents}");
    }

    /// An exclusion names a release to avoid. Writing the newest release into it
    /// turns "anything but 1.5.0" into a line that excludes the very release the
    /// run was meant to move the project to.
    #[tokio::test]
    async fn an_exclusion_is_never_rewritten_to_exclude_the_newest_release() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "pkg!=1.5.0").unwrap();

        let registry = MockRegistry::new("PyPI").with_version("pkg", "2.0.0");

        let result = RequirementsUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 1);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("pkg!=1.5.0"));
    }

    /// A configured pin is an instruction. When the specifier has no floor to
    /// write it into, the file does not say what the config says it should, so
    /// the run reports a failure rather than passing silently.
    #[tokio::test]
    async fn a_pin_a_specifier_cannot_hold_is_an_error() {
        use crate::config::UpdConfig;
        use std::collections::HashMap;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "django<6").unwrap();

        let registry = MockRegistry::new("PyPI").with_version("django", "5.2.0");

        let mut pin = HashMap::new();
        pin.insert("django".to_string(), "5.1.0".to_string());
        let config = UpdConfig {
            pin,
            ..Default::default()
        };

        let result = RequirementsUpdater::new()
            .update(
                file.path(),
                &registry,
                UpdateOptions::new(false, false).with_config(Arc::new(config)),
            )
            .await
            .unwrap();

        assert!(result.pinned.is_empty());
        assert_eq!(
            result.errors,
            vec!["cannot pin 'django' to '5.1.0': '<6' has no lower bound that version fits"]
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("django<6"));
    }

    #[tokio::test]
    async fn test_upper_bound_with_lower_bound_is_updated() {
        // Constraints with BOTH upper AND lower bounds should be updated
        // (the lower bound defines what we're updating)
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "django>=4.0,<6").unwrap();

        let registry = MockRegistry::new("PyPI").with_constrained("django", ">=4.0,<6", "5.2");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // This SHOULD be updated because it has a lower bound (>=4.0)
        assert_eq!(
            result.updated.len(),
            1,
            "Constraint with lower bound should be updated"
        );

        // Verify the version was updated
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("django>=5.2"),
            "Version should be updated to 5.2"
        );
    }

    #[tokio::test]
    async fn test_update_requirements_with_config_ignore() {
        use crate::config::UpdConfig;
        use std::collections::HashMap;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        writeln!(file, "flask>=2.0.0").unwrap();
        writeln!(file, "django>=4.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "5.0.0");

        // Create config that ignores "flask"
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["flask".to_string()],
            pin: HashMap::new(),
            cooldown: None,
            ..Default::default()
        };

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only 2 packages should be updated (requests and django), flask should be ignored
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "flask");

        // Verify file contents - flask should remain unchanged
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests==2.31.0"));
        assert!(contents.contains("flask>=2.0.0")); // Unchanged!
        assert!(contents.contains("django>=5.0"));
    }

    #[tokio::test]
    async fn test_update_requirements_with_config_pin() {
        use crate::config::UpdConfig;
        use std::collections::HashMap;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        writeln!(file, "flask>=2.0.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        // Create config that pins "requests" to 2.30.0
        let mut pin = HashMap::new();
        pin.insert("requests".to_string(), "2.30.0".to_string());

        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec![],
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // flask should be updated from the registry; requests should be recorded only as pinned
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "flask");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "requests");
        assert_eq!(result.pinned[0].2, "2.30.0"); // Pinned version

        // Verify file contents
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests==2.30.0")); // Pinned version, not 2.31.0
        assert!(contents.contains("flask>=3.0.0"));
    }

    #[tokio::test]
    async fn test_update_requirements_with_config_ignore_and_pin() {
        use crate::config::UpdConfig;
        use std::collections::HashMap;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.28.0").unwrap();
        writeln!(file, "flask>=2.0.0").unwrap();
        writeln!(file, "django>=4.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "5.0.0");

        // Create config that ignores "flask" and pins "requests"
        let mut pin = HashMap::new();
        pin.insert("requests".to_string(), "2.29.0".to_string());

        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["flask".to_string()],
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // django should be updated, requests should be pinned, flask should be ignored
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "django");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.pinned.len(), 1);

        // Verify file contents
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests==2.29.0")); // Pinned version
        assert!(contents.contains("flask>=2.0.0")); // Unchanged (ignored)
        assert!(contents.contains("django>=5.0")); // Updated
    }

    /// Lines whose version token is not a valid PEP 440 version (e.g. template
    /// placeholders or typos) must be left byte-for-byte unchanged and must
    /// produce a warning.  Valid lines in the same file must still be updated.
    #[tokio::test]
    async fn test_invalid_pep440_version_skipped() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "flask==%version%").unwrap();
        writeln!(file, "numpy==abc").unwrap();
        writeln!(file, "django==3.0.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("flask", "3.1.0")
            .with_version("numpy", "2.0.0")
            .with_version("django", "5.2.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only django should have been updated - flask and numpy have invalid versions
        assert_eq!(
            result.updated.len(),
            1,
            "Only the valid package (django) should be updated"
        );
        assert_eq!(result.updated[0].0, "django");

        // A warning must be produced for each invalid version
        assert_eq!(
            result.warnings.len(),
            2,
            "Expected one warning per invalid version token"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("flask") && w.contains("%version%")),
            "Warning for flask must mention the package and the raw token"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("numpy") && w.contains("abc")),
            "Warning for numpy must mention the package and the raw token"
        );

        // The file lines with invalid versions must be unchanged byte-for-byte
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("flask==%version%"),
            "flask line must remain unchanged"
        );
        assert!(
            contents.contains("numpy==abc"),
            "numpy line must remain unchanged"
        );
        // The valid django line must have been updated
        assert!(
            contents.contains("django==5.2.0"),
            "django must be updated to 5.2.0"
        );
    }

    /// When the registry returns a version *lower* than the current, the updater must
    /// leave the line unchanged and emit a warning (PEP 440 comparator path).
    #[tokio::test]
    async fn test_no_downgrade() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==5.0.0").unwrap();
        writeln!(file, "flask==3.0.0").unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "4.0.0") // lower - must be refused
            .with_version("flask", "4.0.0"); // higher - normal upgrade

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only flask should be updated; requests downgrade must be refused.
        assert_eq!(
            result.updated.len(),
            1,
            "only the upgrade should be applied"
        );
        assert_eq!(result.updated[0].0, "flask");
        assert_eq!(
            result.unchanged, 1,
            "refused downgrade must count as unchanged"
        );

        assert_eq!(
            result.warnings.len(),
            1,
            "expected exactly one downgrade warning"
        );
        assert!(
            result.warnings[0].contains("requests"),
            "warning must name the package"
        );
        assert!(
            result.warnings[0].contains("4.0.0"),
            "warning must include the rejected latest version"
        );
        assert!(
            result.warnings[0].contains("5.0.0"),
            "warning must include the current version"
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("requests==5.0.0"),
            "requests line must not be modified"
        );
        assert!(contents.contains("flask==4.0.0"), "flask must be updated");
    }

    /// When the current version is a pre-release, the updater must seek the latest
    /// pre-release rather than promoting to stable.
    #[tokio::test]
    async fn test_prerelease_stays_on_prerelease() {
        // black==25.0.0b1 → should go to 26.1a1 (latest pre), not 26.3.1 (stable)
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "black==25.0.0b1").unwrap();

        let registry = MockRegistry::new("PyPI").with_prerelease("black", "26.3.1", "26.1a1");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1, "should have one update");
        assert_eq!(
            result.updated[0].2, "26.1a1",
            "should update to pre-release, not stable"
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("black==26.1a1"),
            "file must contain the pre-release version"
        );
        assert!(!contents.contains("26.3.1"), "must not promote to stable");
    }

    /// Realistic-registry scenario: a newer pre-release exists alongside a newer stable.
    /// The updater must pick the higher pre-release, not the stable, and must not
    /// regress to an older pre-release just because a stable also exists.
    #[tokio::test]
    async fn test_prerelease_picks_higher_prerelease_over_stable() {
        // Current: 25.0.0b1. Available: stable=26.3.1, pre=26.4.0a1 (higher pre-release).
        // Expected: update to 26.4.0a1, not to 26.3.1.
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "black==25.0.0b1").unwrap();

        // with_prerelease configures: get_latest_version → "26.3.1" (stable),
        //                             get_latest_version_including_prereleases → "26.4.0a1"
        let registry = MockRegistry::new("PyPI").with_prerelease("black", "26.3.1", "26.4.0a1");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1, "should have one update");
        assert_eq!(
            result.updated[0].2, "26.4.0a1",
            "should pick the highest pre-release, not the stable"
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("black==26.4.0a1"));
        assert!(!contents.contains("26.3.1"), "must not promote to stable");
    }

    /// When no newer pre-release exists and only a newer stable is available,
    /// a pre-release-pinned package must not be silently promoted to stable.
    #[tokio::test]
    async fn test_prerelease_no_silent_promotion_to_stable() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "mylib==1.0a1").unwrap();

        // Registry only has a stable version - no pre-release at all.
        // get_latest_version_including_prereleases will return "2.0.0" (stable),
        // which is newer than 1.0a1. Without the guard this would silently promote.
        let registry = MockRegistry::new("PyPI").with_version("mylib", "2.0.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // No update should happen - the only available "latest" is a stable, not a pre-release.
        assert_eq!(
            result.updated.len(),
            0,
            "should not silently promote pre-release to stable"
        );
        assert_eq!(result.unchanged, 1, "should be counted as unchanged");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("mylib==1.0a1"),
            "version must remain unchanged"
        );
    }

    /// Current pre-release, higher pre-release available across versions - pick the highest.
    #[tokio::test]
    async fn test_prerelease_picks_highest_prerelease() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "lib==1.0a1").unwrap();

        // Available: stable=1.0.0, prerelease=2.0a1 (highest pre)
        let registry = MockRegistry::new("PyPI").with_prerelease("lib", "1.0.0", "2.0a1");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].2, "2.0a1");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("lib==2.0a1"));
    }

    /// Current stable should still skip pre-releases (regression guard).
    #[tokio::test]
    async fn test_stable_skips_prerelease_regression() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "tool==1.0.0").unwrap();

        let registry = MockRegistry::new("PyPI").with_prerelease("tool", "2.0.0", "3.0.0a1");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Should update to 2.0.0 (stable), not 3.0.0a1 (pre-release)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].2, "2.0.0");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("tool==2.0.0"));
        assert!(!contents.contains("3.0.0a1"));
    }

    /// Equal versions (current == latest after precision-matching) must NOT trigger the
    /// no-downgrade warning - the existing unchanged branch handles them.
    #[tokio::test]
    async fn test_equal_version_no_warning() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "requests==2.31.0").unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = RequirementsUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0, "no update expected");
        assert_eq!(result.unchanged, 1, "should be marked unchanged");
        assert!(
            result.warnings.is_empty(),
            "no warning expected for equal version"
        );
    }
}
