use super::{
    Clause, FileType, ParsedDependency, PendingVersion, UpdateOptions, UpdateResult, Updater,
    caps_from_above, downgrade_warning, floor_of, operator_is_raisable, parse_clause,
    read_file_safe, unpinnable_error, unrewritable_warning, write_file_atomic,
};
use crate::align::compare_versions;
use crate::registry::{Registry, matches_ruby_constraint};
use crate::updater::Lang;
use crate::version::match_version_precision;
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;

pub struct GemfileUpdater {
    /// Matches: gem 'name', 'constraint version'
    /// Group 1: gem name
    /// Group 2: the first version constraint string (e.g., "~> 7.1", ">= 4.9.0", "1.5.4")
    gem_re: Regex,
    /// Matches one further constraint literal, anchored at the end of the last
    /// one: the `, '< 7.0'` of `gem 'rails', '>= 6.0', '< 7.0'`.
    next_constraint_re: Regex,
}

/// One version constraint of a gem declaration.
///
/// RubyGems takes any number of them and requires all of them at once, so
/// `gem 'rails', '>= 6.0', '< 7.0'` means 6.x only. Reading just the first
/// leaves the rest standing while the first is rewritten, which is how
/// `>= 6.0` became `>= 8.1` beside an untouched `< 7.0` and produced a Gemfile
/// no `bundle install` can resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GemClause {
    /// The comparison operator, empty for an exact version.
    op: String,
    /// The version the operator bounds.
    version: String,
    /// Byte range of `version` within the line, so a rewrite lands on this
    /// clause rather than on the first text that happens to look like it.
    range: std::ops::Range<usize>,
}

/// Parsed gem dependency
struct ParsedGem {
    name: String,
    /// Every constraint, in the order written.
    clauses: Vec<GemClause>,
}

impl ParsedGem {
    /// The constraints as RubyGems requirement text, which is what the registry
    /// and every message about this gem quote.
    fn constraint_text(&self) -> String {
        self.clauses
            .iter()
            .map(|c| {
                if c.op.is_empty() {
                    c.version.clone()
                } else {
                    format!("{} {}", c.op, c.version)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The constraints as the shared clause vocabulary reads them, so which
    /// bound is a floor and which caps from above is decided in one place for
    /// every ecosystem.
    fn as_clauses(&self) -> Vec<Clause<'_>> {
        self.clauses
            .iter()
            .map(|c| Clause {
                op: &c.op,
                version: &c.version,
                range: c.range.clone(),
            })
            .collect()
    }

    /// The clause an update may rewrite, paired with whether it may.
    fn floor(&self) -> Option<(&GemClause, bool)> {
        let raisable = floor_of(&self.as_clauses())?.raisable;
        let clause = if raisable {
            self.clauses.iter().find(|c| operator_is_raisable(&c.op))?
        } else {
            self.clauses.first()?
        };
        Some((clause, raisable))
    }

    /// The version this declaration is anchored at, for reporting and comparison.
    fn anchor_version(&self) -> &str {
        self.floor().map(|(c, _)| c.version.as_str()).unwrap_or("")
    }

    /// Whether a clause can exclude the newest release, so the release to raise
    /// to has to be looked up against the constraints.
    fn caps_from_above(&self) -> bool {
        caps_from_above(&self.as_clauses())
    }

    /// Whether the release to consider has to be looked up against the
    /// constraints rather than taken as the newest one published.
    ///
    /// Only a raisable floor is ever rewritten, and only then does a ceiling
    /// above it decide which release can go in. Where nothing is rewritten the
    /// question is the opposite one - does the newest release there is fit
    /// these constraints - and a lookup made against the constraints cannot
    /// answer it: it comes back with a release that fits by construction, so
    /// every such declaration reads as current no matter how far behind it is.
    fn lookup_is_constrained(&self) -> bool {
        self.floor().is_some_and(|(_, raisable)| raisable) && self.caps_from_above()
    }
}

impl GemfileUpdater {
    pub fn new() -> Self {
        // Matches gem declarations with version constraints:
        //   gem 'rails', '~> 7.1'
        //   gem "devise", ">= 4.9.0"
        //   gem 'puma', '1.5.4'
        // Captures:
        //   1: gem name
        //   2: the whole first constraint, operator included
        let gem_re = Regex::new(
            r#"^\s*gem\s+['"]([^'"]+)['"]\s*,\s*['"]((?:~>|>=|<=|!=|>|<|=)?\s*\d[^'"]*?)['"]"#,
        )
        .expect("Invalid regex");

        // A further constraint literal directly after the previous one. Anchored
        // so a `group: :test` or `require: false` option ends the run rather
        // than being scanned past to something that looks like a version.
        let next_constraint_re =
            Regex::new(r#"^\s*,\s*['"]((?:~>|>=|<=|!=|>|<|=)?\s*\d[^'"]*?)['"]"#)
                .expect("Invalid regex");

        Self {
            gem_re,
            next_constraint_re,
        }
    }

    fn parse_line(&self, line: &str) -> Option<ParsedGem> {
        let trimmed = line.trim();

        // Skip comments
        if trimmed.starts_with('#') {
            return None;
        }

        // Skip gems with path: or git: options (local/git sources)
        if trimmed.contains("path:") || trimmed.contains("git:") {
            return None;
        }

        let caps = self.gem_re.captures(line)?;
        let name = caps.get(1)?.as_str().to_string();
        let first = caps.get(2)?;

        let mut clauses = Vec::new();
        let mut at = first.start();
        let mut text = first.as_str();
        // The literal ends at the closing quote; everything after it is either
        // another constraint or the options the declaration carries.
        let mut rest_at = first.end() + 1;

        loop {
            if let Some(clause) = parse_clause(text, at) {
                clauses.push(GemClause {
                    op: clause.op.to_string(),
                    version: clause.version.to_string(),
                    range: clause.range,
                });
            }

            let Some(remainder) = line.get(rest_at..) else {
                break;
            };
            let Some(next) = self.next_constraint_re.captures(remainder) else {
                break;
            };
            let Some(m) = next.get(1) else {
                break;
            };
            at = rest_at + m.start();
            text = m.as_str();
            rest_at += m.end() + 1;
        }

        if clauses.is_empty() {
            return None;
        }

        Some(ParsedGem { name, clauses })
    }

    /// Write `new_version` over the byte range the old one occupies.
    ///
    /// Positional rather than textual: `gem 'rails', '>= 6.0', '< 7.0'` holds two
    /// versions and a search for the first match of either would land on the
    /// wrong clause whenever the floor is not written first.
    fn update_line(&self, line: &str, range: &std::ops::Range<usize>, new_version: &str) -> String {
        let mut updated = String::with_capacity(line.len() + new_version.len());
        updated.push_str(&line[..range.start]);
        updated.push_str(new_version);
        updated.push_str(&line[range.end..]);
        updated
    }

    /// Check if a Ruby gem version string is a pre-release.
    /// RubyGems pre-releases include a letter component in the version segments,
    /// commonly expressed as `.pre`, `.rc`, `.beta`, `.alpha`, or similar suffixes.
    pub(crate) fn is_prerelease_ruby(version: &str) -> bool {
        let v = version.to_lowercase();
        v.contains(".pre")
            || v.contains(".rc")
            || v.contains(".beta")
            || v.contains(".alpha")
            || v.contains(".dev")
            // RubyGems also treats any version segment that is non-numeric as a pre-release
            // (e.g. "8.0.0.beta1" or "8.0.0.rc2")
            || version.split('.').any(|seg| seg.chars().any(|c| c.is_alphabetic()))
    }
}

impl Default for GemfileUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for GemfileUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut result = UpdateResult::default();

        let lines: Vec<&str> = content.lines().collect();
        let mut parsed_gems: Vec<(usize, &str, ParsedGem)> = Vec::new();

        for (line_idx, line) in lines.iter().enumerate() {
            if let Some(parsed) = self.parse_line(line) {
                parsed_gems.push((line_idx, line, parsed));
            }
        }

        // Separate into ignored, pinned, and to-be-fetched
        let mut ignored_packages: Vec<(usize, String, String)> = Vec::new();
        let mut pinned_packages: Vec<(usize, String, String, String)> = Vec::new();
        let mut fetch_deps: Vec<(usize, &str, &ParsedGem)> = Vec::new();

        for (line_idx, line, parsed) in &parsed_gems {
            if options.is_package_filtered_out(&parsed.name) {
                result.unchanged += 1;
                continue;
            }

            if options.should_ignore(&parsed.name) {
                ignored_packages.push((
                    *line_idx,
                    parsed.name.clone(),
                    parsed.anchor_version().to_string(),
                ));
                continue;
            }

            if let Some(pinned_version) = options.get_pinned_version(&parsed.name) {
                if !parsed.floor().is_some_and(|(_, raisable)| raisable) {
                    // The pin was configured and cannot be written, so the file
                    // does not say what the config says it should. That is a
                    // failed instruction, not a note.
                    result.errors.push(unpinnable_error(
                        &parsed.name,
                        pinned_version,
                        &parsed.constraint_text(),
                    ));
                    continue;
                }
                pinned_packages.push((
                    *line_idx,
                    parsed.name.clone(),
                    parsed.anchor_version().to_string(),
                    pinned_version.to_string(),
                ));
                continue;
            }

            fetch_deps.push((*line_idx, *line, parsed));
        }

        for (line_idx, package, version) in ignored_packages {
            result.ignored.push((package, version, Some(line_idx + 1)));
        }

        // Deduplicate registry lookups: one request per gem and requirement.
        // The same gem can be declared more than once (in different groups, or
        // for disjoint platforms), and where the requirements differ so does
        // the release each declaration can take, so the requirement is part of
        // what identifies a lookup.
        let unique_gems: Vec<(String, String, String, bool)> = {
            let mut seen = std::collections::HashSet::new();
            fetch_deps
                .iter()
                .filter_map(|(_, _, parsed)| {
                    let key = (parsed.name.clone(), parsed.constraint_text());
                    if seen.insert(key) {
                        Some((
                            parsed.name.clone(),
                            parsed.constraint_text(),
                            parsed.anchor_version().to_string(),
                            parsed.lookup_is_constrained(),
                        ))
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Fetch versions in parallel.
        // When the current version is a pre-release, request the latest pre-release
        // to avoid silently promoting the gem to a stable release.
        let version_futures: Vec<_> = unique_gems
            .iter()
            .map(|(name, constraint, version, constrained)| async move {
                if Self::is_prerelease_ruby(version) {
                    registry
                        .get_latest_version_including_prereleases(name)
                        .await
                } else if *constrained {
                    registry.get_latest_version_matching(name, constraint).await
                } else {
                    registry.get_latest_version(name).await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Build a map from gem and requirement -> latest version result
        let gem_versions: HashMap<(String, String), Result<String, String>> = unique_gems
            .into_iter()
            .zip(version_results)
            .map(|((name, constraint, _, _), result)| {
                ((name, constraint), result.map_err(|e| e.to_string()))
            })
            .collect();

        // Map results back to every line index that references each gem
        let mut version_map: HashMap<usize, PendingVersion> = HashMap::new();
        for (line_idx, _, parsed) in &fetch_deps {
            if let Some(result) = gem_versions.get(&(parsed.name.clone(), parsed.constraint_text()))
            {
                match result {
                    Ok(version) => {
                        version_map
                            .insert(*line_idx, PendingVersion::Registry(Ok(version.clone())));
                    }
                    Err(e) => {
                        version_map.insert(
                            *line_idx,
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

        for (line_idx, line) in lines.iter().enumerate() {
            let line_num = line_idx + 1;

            if let Some(parsed) = self.parse_line(line) {
                let Some((floor, raisable)) = parsed.floor() else {
                    new_lines.push(line.to_string());
                    continue;
                };
                let anchor = floor.version.clone();
                let floor_range = floor.range.clone();
                let constraint_text = parsed.constraint_text();

                if let Some(version_result) = version_map.remove(&line_idx) {
                    match version_result {
                        PendingVersion::Pinned(pinned_version) => {
                            let matched_version = if options.full_precision {
                                pinned_version.clone()
                            } else {
                                match_version_precision(&anchor, &pinned_version)
                            };
                            if matched_version != anchor {
                                result.pinned.push((
                                    parsed.name.clone(),
                                    anchor.clone(),
                                    matched_version.clone(),
                                    Some(line_num),
                                ));
                                new_lines.push(self.update_line(
                                    line,
                                    &floor_range,
                                    &matched_version,
                                ));
                                modified = true;
                            } else {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            }
                        }
                        PendingVersion::Registry(Ok(latest_version)) => {
                            // A bound that is not a floor names no release to carry
                            // forward: `> 6.0` names the one version ruled out and a
                            // ceiling or exclusion names none at all. Say what is
                            // available and leave the declaration alone.
                            if !raisable {
                                if matches_ruby_constraint(&latest_version, &constraint_text) {
                                    result.unchanged += 1;
                                } else {
                                    result.warnings.push(unrewritable_warning(
                                        &parsed.name,
                                        &latest_version,
                                        &constraint_text,
                                    ));
                                }
                                new_lines.push(line.to_string());
                                continue;
                            }

                            // When the current version is a pre-release, we fetched the latest
                            // pre-release. If the registry returned a stable version instead
                            // (no newer pre-release exists), refuse silent promotion to stable.
                            let current_is_prerelease = Self::is_prerelease_ruby(&anchor);
                            if current_is_prerelease && !Self::is_prerelease_ruby(&latest_version) {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                                continue;
                            }

                            // Pass the constraints to cooldown's held-back selection so
                            // it cannot pick a version they exclude (e.g. `~> 7.1` must
                            // stay in 7.x). An exact pin constrains nothing: its whole
                            // purpose is to be replaced by the newest release.
                            let constraint_for_cooldown: Option<&str> =
                                if parsed.clauses.iter().all(|c| c.op.is_empty()) {
                                    None
                                } else {
                                    Some(constraint_text.as_str())
                                };
                            let (outcome, note) = crate::updater::apply_cooldown(
                                registry,
                                &parsed.name,
                                &anchor,
                                &latest_version,
                                constraint_for_cooldown,
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
                                        parsed.name.clone(),
                                        anchor.clone(),
                                        skipped_version,
                                        skipped_published_at,
                                    ));
                                    new_lines.push(line.to_string());
                                    continue;
                                }
                            };

                            let matched_version = if options.full_precision {
                                latest_version.clone()
                            } else {
                                match_version_precision(&anchor, &latest_version)
                            };
                            if matched_version != anchor {
                                // Refuse to write a downgrade.
                                if compare_versions(&matched_version, &anchor, Lang::Ruby)
                                    != std::cmp::Ordering::Greater
                                {
                                    result.warnings.push(downgrade_warning(
                                        &parsed.name,
                                        &matched_version,
                                        &anchor,
                                    ));
                                    result.unchanged += 1;
                                    new_lines.push(line.to_string());
                                } else if !options.allows_bump(&anchor, &matched_version) {
                                    // Bump level exceeds the --only-bump/--max-bump ceiling.
                                    result.record_capped(
                                        &parsed.name,
                                        &anchor,
                                        &matched_version,
                                        Some(line_num),
                                    );
                                    new_lines.push(line.to_string());
                                } else {
                                    result.updated.push((
                                        parsed.name.clone(),
                                        anchor.clone(),
                                        matched_version.clone(),
                                        Some(line_num),
                                    ));
                                    if let Some((skipped_version, skipped_published_at)) =
                                        held_back_record
                                    {
                                        result.held_back.push((
                                            parsed.name.clone(),
                                            anchor.clone(),
                                            matched_version.clone(),
                                            skipped_version,
                                            skipped_published_at,
                                        ));
                                    }
                                    new_lines.push(self.update_line(
                                        line,
                                        &floor_range,
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
                            result.errors.push(format!("{}: {}", parsed.name, e));
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
        file_type == FileType::Gemfile
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let mut deps = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            if let Some(parsed) = self.parse_line(line) {
                deps.push(ParsedDependency {
                    version: parsed.anchor_version().to_string(),
                    has_upper_bound: parsed.caps_from_above(),
                    name: parsed.name,
                    line_number: Some(line_idx + 1),
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
    use tempfile::NamedTempFile;

    /// The clauses of a gem line as `(operator, version)` pairs.
    fn clauses_of(updater: &GemfileUpdater, line: &str) -> Vec<(String, String)> {
        updater
            .parse_line(line)
            .expect("line parses")
            .clauses
            .iter()
            .map(|c| (c.op.clone(), c.version.clone()))
            .collect()
    }

    /// Rewrite a line's floor the way `update` does: at the byte range the
    /// clause occupies, not at the first text that looks like it.
    fn rewrite_floor(updater: &GemfileUpdater, line: &str, new_version: &str) -> String {
        let parsed = updater.parse_line(line).expect("line parses");
        let (floor, _) = parsed.floor().expect("line has a floor");
        updater.update_line(line, &floor.range, new_version)
    }

    #[test]
    fn test_parse_gem_line() {
        let updater = GemfileUpdater::new();

        assert_eq!(
            clauses_of(&updater, "gem 'rails', '~> 7.1'"),
            vec![("~>".to_string(), "7.1".to_string())]
        );
        assert_eq!(
            clauses_of(&updater, "gem \"devise\", \">= 4.9.0\""),
            vec![(">=".to_string(), "4.9.0".to_string())]
        );
        assert_eq!(
            clauses_of(&updater, "gem 'pg', '1.5.4'"),
            vec![(String::new(), "1.5.4".to_string())]
        );
    }

    #[test]
    fn every_constraint_of_a_multi_clause_gem_is_read() {
        let updater = GemfileUpdater::new();

        // RubyGems requires all of them at once. Reading only the first is how
        // `>= 6.0` was raised to `>= 8.1` beside an untouched `< 7.0`.
        assert_eq!(
            clauses_of(&updater, "gem 'rails', '>= 6.0', '< 7.0'"),
            vec![
                (">=".to_string(), "6.0".to_string()),
                ("<".to_string(), "7.0".to_string()),
            ]
        );
        let parsed = updater
            .parse_line("gem 'rails', '>= 6.0', '< 7.0'")
            .unwrap();
        assert_eq!(parsed.constraint_text(), ">= 6.0, < 7.0");
        assert_eq!(parsed.anchor_version(), "6.0");
        assert!(parsed.caps_from_above());
    }

    #[test]
    fn an_option_after_the_constraints_ends_the_run() {
        let updater = GemfileUpdater::new();

        // `group: :test` is not a version, so the scan must stop rather than
        // reach past it for the next thing that looks like one.
        assert_eq!(
            clauses_of(&updater, "gem 'rspec', '~> 3.12', group: :test"),
            vec![("~>".to_string(), "3.12".to_string())]
        );
    }

    #[test]
    fn a_multi_clause_rewrite_lands_on_the_floor_and_leaves_the_ceiling() {
        let updater = GemfileUpdater::new();

        assert_eq!(
            rewrite_floor(&updater, "gem 'rails', '>= 6.0', '< 7.0'", "6.1"),
            "gem 'rails', '>= 6.1', '< 7.0'"
        );
        // The ceiling is written first here, so a textual search would rewrite it.
        assert_eq!(
            rewrite_floor(&updater, "gem 'rails', '< 7.0', '>= 6.0'", "6.1"),
            "gem 'rails', '< 7.0', '>= 6.1'"
        );
    }

    #[test]
    fn test_skips_comments_and_no_version() {
        let updater = GemfileUpdater::new();

        assert!(updater.parse_line("# gem 'rails', '~> 7.1'").is_none());
        assert!(updater.parse_line("gem 'sidekiq'").is_none());
        assert!(updater.parse_line("").is_none());
        assert!(
            updater
                .parse_line("  # This is a comment about gems")
                .is_none()
        );
    }

    #[test]
    fn test_skips_path_and_git_gems() {
        let updater = GemfileUpdater::new();

        assert!(
            updater
                .parse_line("gem 'my-gem', path: '../my-gem'")
                .is_none()
        );
        assert!(
            updater
                .parse_line("gem 'my-gem', git: 'https://github.com/user/repo'")
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_update_gemfile() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "source 'https://rubygems.org'").unwrap();
        writeln!(file).unwrap();
        writeln!(file, "gem 'rails', '~> 7.1'").unwrap();
        writeln!(file, "gem 'pg', '1.5.4'").unwrap();
        writeln!(file, "# A comment").unwrap();
        writeln!(file, "gem 'sidekiq'").unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_constrained("rails", "~> 7.1", "7.2.1")
            .with_version("pg", "1.6.0");

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("gem 'rails', '~> 7.2'"));
        assert!(contents.contains("gem 'pg', '1.6.0'"));
        assert!(contents.contains("# A comment"));
        assert!(contents.contains("source 'https://rubygems.org'"));
    }

    #[test]
    fn test_version_precision() {
        let updater = GemfileUpdater::new();

        // ~> 7.1 with latest 7.2.3 should preserve 2-part precision
        let result = rewrite_floor(&updater, "gem 'rails', '~> 7.1'", "7.2");
        assert_eq!(result, "gem 'rails', '~> 7.2'");
    }

    #[tokio::test]
    async fn test_dry_run() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "gem 'rails', '~> 7.1'").unwrap();

        let registry = MockRegistry::new("rubygems").with_constrained("rails", "~> 7.1", "7.2.1");

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified in dry-run mode
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("~> 7.1"));
    }

    #[test]
    fn test_preserves_constraint_operator() {
        let updater = GemfileUpdater::new();

        let result = rewrite_floor(&updater, "gem 'devise', '>= 4.9.0'", "4.10.0");
        assert_eq!(result, "gem 'devise', '>= 4.10.0'");

        let result = rewrite_floor(&updater, "gem 'puma', '~> 6.0'", "6.4");
        assert_eq!(result, "gem 'puma', '~> 6.4'");
    }

    #[tokio::test]
    async fn test_config_ignore_and_pin() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "gem 'rails', '7.0.0'\ngem 'devise', '4.9.0'\ngem 'puma', '6.0.0'\n"
        )
        .unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_version("rails", "7.2.3")
            .with_version("devise", "4.9.5")
            .with_version("puma", "6.5.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("devise".to_string(), "4.9.3".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["rails".to_string()],
            pin: pins,
            cooldown: None,
            ..Default::default()
        };

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "rails");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "devise");
        assert_eq!(result.updated.len(), 1);
        let updated_names: Vec<&str> = result
            .updated
            .iter()
            .map(|(n, _, _, _)| n.as_str())
            .collect();
        assert!(updated_names.contains(&"puma"));
        assert!(!updated_names.contains(&"devise"));
    }

    #[tokio::test]
    async fn a_multi_clause_gem_keeps_its_ceiling() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '>= 6.0', '< 7.0'\n").unwrap();

        // The unconstrained answer is 8.1.0. If the whole constraint set does not
        // reach the registry, the lookup falls back to it and the ceiling is left
        // standing beside a floor above it: a Gemfile no bundle install resolves.
        let registry = MockRegistry::new("rubygems")
            .with_version("rails", "8.1.0")
            .with_constrained("rails", ">= 6.0, < 7.0", "6.1.7");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].2, "6.1");
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(contents, "gem 'rails', '>= 6.1', '< 7.0'\n");
    }

    #[tokio::test]
    async fn an_exclusive_lower_bound_is_not_raised_over_the_release_it_names() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '> 6.0'\n").unwrap();

        // `> 6.0` names the one version the author refuses. Raising it to `> 8.1`
        // would write a constraint that excludes the release it was raised to.
        let registry = MockRegistry::new("rubygems").with_version("rails", "8.1.0");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.unchanged, 1);
        assert!(result.warnings.is_empty());
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(contents, "gem 'rails', '> 6.0'\n");
    }

    #[tokio::test]
    async fn an_exclusion_that_admits_the_release_is_up_to_date() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '!= 7.0.0'\n").unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_version("rails", "8.1.0")
            .with_constrained("rails", "!= 7.0.0", "8.1.0");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        // Rewriting the exclusion made `!= 8.1.0`, which rules out the newest
        // release and was reported as a successful major update.
        assert!(result.updated.is_empty());
        assert_eq!(result.unchanged, 1);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(contents, "gem 'rails', '!= 7.0.0'\n");
    }

    /// A requirement with nothing above it admits every release, so the release
    /// to raise to is the registry's own newest and asking for it is one cheap
    /// request. Routing it through the constrained lookup instead asks the
    /// registry to enumerate and filter, and answers with whatever that
    /// enumeration holds.
    #[tokio::test]
    async fn an_uncapped_requirement_asks_for_the_newest_release_outright() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '>= 6.0.0'\n").unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_version("rails", "8.1.0")
            .with_constrained("rails", ">= 6.0.0", "6.9.9");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(contents, "gem 'rails', '>= 8.1.0'\n");
    }

    /// Two gems can carry the same requirement text, which says nothing about
    /// either of them sharing a release. A lookup identified by requirement
    /// alone drops the second gem from the run entirely: not updated, not
    /// current, not reported.
    #[tokio::test]
    async fn two_gems_at_the_same_requirement_are_each_looked_up() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '>= 7.0.0'\ngem 'puma', '>= 7.0.0'\n").unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_version("rails", "8.1.0")
            .with_version("puma", "7.0.3");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.updated.len(), 2, "{:?}", result.updated);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(
            contents,
            "gem 'rails', '>= 8.1.0'\ngem 'puma', '>= 7.0.3'\n"
        );
    }

    /// Bundler lets one gem be declared twice for disjoint platforms, at
    /// different requirements. The release each declaration can take is then a
    /// different release, and deduplicating the lookup by gem name alone hands
    /// the second declaration the first one's answer: here it writes a floor
    /// above the ceiling on the same line.
    #[tokio::test]
    async fn one_gem_at_two_requirements_is_looked_up_at_each_of_them() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "gem 'nokogiri', '>= 1.15.0', platforms: :ruby\n\
             gem 'nokogiri', '>= 1.14.0', '< 1.15.0', platforms: :jruby\n"
        )
        .unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_version("nokogiri", "1.18.0")
            .with_constrained("nokogiri", ">= 1.14.0, < 1.15.0", "1.14.5");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.updated.len(), 2, "{:?}", result.updated);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(
            contents,
            "gem 'nokogiri', '>= 1.18.0', platforms: :ruby\n\
             gem 'nokogiri', '>= 1.14.5', '< 1.15.0', platforms: :jruby\n"
        );
    }

    /// An exclusion that rules out the newest release there is leaves the
    /// project off that release, and saying so is the only report that is true.
    /// The registry still holds releases the exclusion admits, so a lookup made
    /// against the constraint answers with one of those and the gem reads as
    /// current - which is how a dependency held off the newest release passes
    /// under a green tick.
    #[tokio::test]
    async fn an_exclusion_that_rules_out_the_newest_release_names_it() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '!= 8.1.0'\n").unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_version("rails", "8.1.0")
            .with_constrained("rails", "!= 8.1.0", "8.0.2");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(
            result.warnings[0].contains("8.1.0") && result.warnings[0].contains("!= 8.1.0"),
            "{}",
            result.warnings[0]
        );
        assert_eq!(result.unchanged, 0);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(contents, "gem 'rails', '!= 8.1.0'\n");
    }

    /// A ceiling names no floor to carry forward, so nothing is rewritten. The
    /// release it is behind is still worth naming: no future release will
    /// satisfy it either.
    #[tokio::test]
    async fn a_ceiling_below_every_release_is_left_alone_and_named() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '< 7.0'\n").unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_version("rails", "8.1.0")
            .with_constrained("rails", "< 7.0", "6.1.7");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(
            result.warnings[0].contains("8.1.0") && result.warnings[0].contains("< 7.0"),
            "{}",
            result.warnings[0]
        );
        assert_eq!(result.unchanged, 0);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(contents, "gem 'rails', '< 7.0'\n");
    }

    /// The control for the two above: a ceiling the newest release still fits
    /// under is current, and says nothing.
    #[tokio::test]
    async fn a_ceiling_the_newest_release_fits_under_is_up_to_date() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '< 9.0'\n").unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_version("rails", "8.1.0")
            .with_constrained("rails", "< 9.0", "6.1.7");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.unchanged, 1);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert_eq!(contents, "gem 'rails', '< 9.0'\n");
    }

    #[tokio::test]
    async fn a_bound_no_release_satisfies_names_what_is_available() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '> 9.9.9'\n").unwrap();

        let registry = MockRegistry::new("rubygems").with_version("rails", "8.1.0");

        let updater = GemfileUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].contains("8.1.0") && result.warnings[0].contains("> 9.9.9"),
            "{}",
            result.warnings[0]
        );
        assert!(result.updated.is_empty());
    }

    #[tokio::test]
    async fn a_pin_that_cannot_be_written_is_an_error() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '< 7.0'\n").unwrap();

        let registry = MockRegistry::new("rubygems").with_version("rails", "8.1.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("rails".to_string(), "6.1.7".to_string());
        let config = UpdConfig {
            pin: pins,
            ..Default::default()
        };

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // The pin was configured and cannot be written, so the file does not say
        // what the config says it should. Silence here left the two disagreeing.
        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].contains("cannot pin 'rails'"),
            "{}",
            result.errors[0]
        );
        assert!(result.pinned.is_empty());
    }

    #[test]
    fn test_parse_gem_with_indentation() {
        let updater = GemfileUpdater::new();

        let parsed = updater.parse_line("  gem 'rails', '~> 7.1'").unwrap();
        assert_eq!(parsed.name, "rails");
        assert_eq!(parsed.anchor_version(), "7.1");
    }

    #[test]
    fn test_handles() {
        let updater = GemfileUpdater::new();
        assert!(updater.handles(FileType::Gemfile));
        assert!(!updater.handles(FileType::Requirements));
    }

    #[test]
    fn test_is_prerelease_ruby() {
        // Known RubyGems pre-release version formats
        assert!(GemfileUpdater::is_prerelease_ruby("8.0.0.beta1"));
        assert!(GemfileUpdater::is_prerelease_ruby("8.0.0.rc1"));
        assert!(GemfileUpdater::is_prerelease_ruby("8.0.0.rc2"));
        assert!(GemfileUpdater::is_prerelease_ruby("2.0.0.alpha"));
        assert!(GemfileUpdater::is_prerelease_ruby("1.0.0.pre"));
        assert!(GemfileUpdater::is_prerelease_ruby("1.0.0.dev1"));
        assert!(GemfileUpdater::is_prerelease_ruby("8.0.0.beta1"));

        // Stable versions must return false
        assert!(!GemfileUpdater::is_prerelease_ruby("7.2.3"));
        assert!(!GemfileUpdater::is_prerelease_ruby("1.0.0"));
        assert!(!GemfileUpdater::is_prerelease_ruby("7.1"));
    }

    #[tokio::test]
    async fn test_registry_error_populates_errors() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "gem 'nonexistent-gem', '1.0.0'").unwrap();

        // Registry has no entry for nonexistent-gem → will error
        let registry = MockRegistry::new("rubygems");
        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(true, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent-gem"));
    }

    #[tokio::test]
    async fn test_unchanged_count() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "gem 'rails', '7.2.3'\ngem 'puma', '6.0.0'\n").unwrap();

        let registry = MockRegistry::new("rubygems")
            .with_version("rails", "7.2.3") // Already at latest
            .with_version("puma", "6.5.0"); // Has update

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(true, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.unchanged, 1);
    }

    /// When the current Ruby gem version is a pre-release, the updater must pick
    /// the latest pre-release, not promote to stable.
    #[tokio::test]
    async fn test_ruby_prerelease_stays_on_prerelease() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "gem 'rails', '8.0.0.beta1'").unwrap();

        // stable=7.2.3, prerelease=8.0.0.rc1
        let registry = MockRegistry::new("rubygems").with_prerelease("rails", "7.2.3", "8.0.0.rc1");

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(
            result.updated.len(),
            1,
            "should update to newer pre-release"
        );
        assert_eq!(
            result.updated[0].2, "8.0.0.rc1",
            "should pick pre-release, not stable"
        );

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("8.0.0.rc1"),
            "file must contain the pre-release version"
        );
        assert!(!contents.contains("7.2.3"), "must not promote to stable");
    }

    /// When no newer pre-release exists and only a newer stable is available,
    /// a pre-release-pinned gem must not be silently promoted to stable.
    #[tokio::test]
    async fn test_ruby_prerelease_no_silent_promotion_to_stable() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "gem 'rails', '8.0.0.beta1'").unwrap();

        // Registry only has a stable version - no pre-release at all.
        // get_latest_version_including_prereleases will return "8.1.0" (stable),
        // which is newer than 8.0.0.beta1. Without the guard this would silently promote.
        let registry = MockRegistry::new("rubygems").with_version("rails", "8.1.0");

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(
            result.updated.len(),
            0,
            "should not silently promote pre-release to stable"
        );
        assert_eq!(result.unchanged, 1, "should be counted as unchanged");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("8.0.0.beta1"),
            "version must remain unchanged"
        );
        assert!(!contents.contains("8.1.0"), "must not promote to stable");
    }

    /// Current stable Ruby gem must still skip pre-releases (regression guard).
    #[tokio::test]
    async fn test_ruby_stable_skips_prerelease_regression() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "gem 'rails', '7.0.0'").unwrap();

        let registry = MockRegistry::new("rubygems").with_prerelease("rails", "7.2.3", "8.0.0.rc1");

        let updater = GemfileUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Should update to 7.2.3 (stable), not 8.0.0.rc1 (pre-release)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].2, "7.2.3");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("7.2.3"));
        assert!(!contents.contains("8.0.0.rc1"));
    }
}
