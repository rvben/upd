use super::{
    Clause, FileType, ParsedDependency, PendingVersion, UpdateOptions, UpdateResult, Updater,
    caps_from_above, comma_clauses, downgrade_warning, floor_of, operator_is_raisable,
    read_file_safe, unpinnable_error, unrewritable_warning, write_file_atomic,
};
use crate::align::compare_versions;
use crate::registry::{Registry, matches_terraform_constraint};
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

/// One version constraint of a Terraform dependency.
///
/// Terraform takes a comma-separated set of them and requires all of them at
/// once, so `version = ">= 4.0, < 5.0"` means 4.x only. Reading the set as a
/// single operator plus a single version leaves everything past the first
/// comma inside "the version", which is how `">= 4.0, < 5.0"` was rewritten to
/// `">= 6.61.0"` and the ceiling silently disappeared.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TfClause {
    /// The comparison operator, empty for an exact version.
    op: String,
    /// The version the operator bounds.
    version: String,
    /// Byte range of `version` within the line, so a rewrite lands on this
    /// clause rather than on the first text that happens to look like it.
    range: std::ops::Range<usize>,
}

/// Parsed Terraform dependency (provider or module)
struct ParsedTerraformDep {
    /// The source identifier (e.g., "hashicorp/aws" or "terraform-aws-modules/vpc/aws")
    source: String,
    /// Every constraint of the version attribute, in the order written.
    clauses: Vec<TfClause>,
    /// Line number where the version attribute appears (0-indexed)
    version_line_idx: usize,
}

impl ParsedTerraformDep {
    /// The constraints as Terraform requirement text, which is what the registry
    /// and every message about this dependency quote.
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
    fn floor(&self) -> Option<(&TfClause, bool)> {
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

    /// The operator of the clause a rewrite would land on, which decides whether
    /// the pessimistic `~>` rules apply.
    fn anchor_op(&self) -> &str {
        self.floor().map(|(c, _)| c.op.as_str()).unwrap_or("")
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

impl TerraformUpdater {
    pub fn new() -> Self {
        let source_re = Regex::new(r#"^\s*source\s*=\s*"([^"]+)""#).expect("Invalid regex");
        // Captures the whole constraint set, operators and commas included; the
        // clause vocabulary splits it. A regex that captures one operator plus
        // one version cannot represent `">= 4.0, < 5.0"` at all.
        let version_re = Regex::new(r#"^\s*version\s*=\s*"([^"]+)""#).expect("Invalid regex");

        Self {
            source_re,
            version_re,
        }
    }

    /// Read a version attribute's constraint set, with byte ranges relative to
    /// the line so a rewrite can splice one clause and leave the rest standing.
    fn parse_constraint(caps: &regex::Captures<'_>) -> Vec<TfClause> {
        let m = match caps.get(1) {
            Some(m) => m,
            None => return Vec::new(),
        };
        comma_clauses(m.as_str(), m.start())
            .map(|c| TfClause {
                op: c.op.to_string(),
                version: c.version.to_string(),
                range: c.range,
            })
            .collect()
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
                    let clauses = Self::parse_constraint(&caps);
                    if !clauses.is_empty() {
                        deps.push(ParsedTerraformDep {
                            source: source.clone(),
                            clauses,
                            version_line_idx: line_idx,
                        });
                    }
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
                    let raw_source = caps.get(1).unwrap().as_str();
                    // Skip local and git sources
                    if raw_source.starts_with("./")
                        || raw_source.starts_with("../")
                        || raw_source.starts_with("git::")
                    {
                        continue;
                    }
                    // Strip the explicit registry hostname so the remaining path
                    // matches the standard namespace/name/provider format used for
                    // registry API lookups. The original source string in the HCL
                    // file is never rewritten; only the version line is modified.
                    let source = raw_source
                        .strip_prefix("registry.terraform.io/")
                        .unwrap_or(raw_source)
                        .to_string();
                    // Only track registry module sources (namespace/name/provider format)
                    if source.split('/').count() == 3 {
                        module_source = Some(source);
                    }
                }

                if let Some(caps) = self.version_re.captures(line)
                    && let Some(ref source) = module_source
                {
                    let clauses = Self::parse_constraint(&caps);
                    if !clauses.is_empty() {
                        deps.push(ParsedTerraformDep {
                            source: source.clone(),
                            clauses,
                            version_line_idx: line_idx,
                        });
                    }
                }
            }
        }

        deps
    }

    /// Write `new_version` over the byte range the old one occupies.
    ///
    /// Positional rather than textual: `version = ">= 4.0, < 5.0"` holds two
    /// versions and a search for the first match of either would land on the
    /// wrong clause whenever the floor is not written first.
    fn update_line(&self, line: &str, range: &std::ops::Range<usize>, new_version: &str) -> String {
        let mut updated = String::with_capacity(line.len() + new_version.len());
        updated.push_str(&line[..range.start]);
        updated.push_str(new_version);
        updated.push_str(&line[range.end..]);
        updated
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
                    dep.anchor_version().to_string(),
                ));
                continue;
            }

            if let Some(pinned_version) = options.get_pinned_version(&dep.source) {
                if !dep.floor().is_some_and(|(_, raisable)| raisable) {
                    // The pin was configured and cannot be written, so the file
                    // does not say what the config says it should. That is a
                    // failed instruction, not a note.
                    result.errors.push(unpinnable_error(
                        &dep.source,
                        pinned_version,
                        &dep.constraint_text(),
                    ));
                    continue;
                }
                pinned_packages.push((
                    dep.version_line_idx,
                    dep.source.clone(),
                    dep.anchor_version().to_string(),
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
        let unique_sources: Vec<(String, String, bool)> = {
            let mut seen = std::collections::HashSet::new();
            fetch_deps
                .iter()
                .filter_map(|(_, dep)| {
                    if seen.insert(dep.source.clone()) {
                        Some((
                            dep.source.clone(),
                            dep.constraint_text(),
                            dep.lookup_is_constrained(),
                        ))
                    } else {
                        None
                    }
                })
                .collect()
        };

        let version_futures: Vec<_> = unique_sources
            .iter()
            .map(|(name, constraint, constrained)| async move {
                if *constrained {
                    registry.get_latest_version_matching(name, constraint).await
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
                let Some((floor, raisable)) = dep.floor() else {
                    new_lines.push(line.to_string());
                    continue;
                };
                let anchor = floor.version.clone();
                let floor_range = floor.range.clone();
                let anchor_op = dep.anchor_op().to_string();

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
                                    dep.source.clone(),
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
                            // forward: `> 4.0` names the one version ruled out and a
                            // ceiling or exclusion names none at all. Say what is
                            // available and leave the declaration alone.
                            if !raisable {
                                let constraint_text = dep.constraint_text();
                                if matches_terraform_constraint(&latest_version, &constraint_text) {
                                    result.unchanged += 1;
                                } else {
                                    result.warnings.push(unrewritable_warning(
                                        &dep.source,
                                        &latest_version,
                                        &constraint_text,
                                    ));
                                }
                                new_lines.push(line.to_string());
                                continue;
                            }

                            // For `~>` constraints, if the latest version still falls within
                            // the range the constraint already expresses, leave the constraint
                            // untouched. Rewriting it would silently raise the floor (e.g.
                            // `~> 4.0` → `~> 4.67`) and block rollback to earlier releases.
                            if anchor_op == "~>"
                                && Self::pessimistic_constraint_satisfied(&anchor, &latest_version)
                            {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                                continue;
                            }

                            // For ~> constraints that need bumping, anchor the new
                            // constraint at the start of the new series (e.g. `5.0`)
                            // rather than the exact latest (e.g. `5.2`), so the full
                            // new major/minor range remains accessible.
                            let matched_version = if anchor_op == "~>" {
                                Self::pessimistic_constraint_new_version(&anchor, &latest_version)
                            } else if options.full_precision {
                                latest_version.clone()
                            } else {
                                match_version_precision(&anchor, &latest_version)
                            };
                            if matched_version != anchor {
                                // Refuse to write a downgrade.
                                if compare_versions(&matched_version, &anchor, Lang::Terraform)
                                    != std::cmp::Ordering::Greater
                                {
                                    result.warnings.push(downgrade_warning(
                                        &dep.source,
                                        &matched_version,
                                        &anchor,
                                    ));
                                    result.unchanged += 1;
                                    new_lines.push(line.to_string());
                                } else if !options.allows_bump(&anchor, &matched_version) {
                                    // Bump level exceeds the --only-bump/--max-bump ceiling.
                                    result.record_capped(
                                        &dep.source,
                                        &anchor,
                                        &matched_version,
                                        Some(line_num),
                                    );
                                    new_lines.push(line.to_string());
                                } else {
                                    result.updated.push((
                                        dep.source.clone(),
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
                version: dep.anchor_version().to_string(),
                has_upper_bound: dep.caps_from_above(),
                line_number: Some(dep.version_line_idx + 1),
                name: dep.source,
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

    /// A dependency's clauses as `(operator, version)` pairs.
    fn clauses_of(dep: &ParsedTerraformDep) -> Vec<(String, String)> {
        dep.clauses
            .iter()
            .map(|c| (c.op.clone(), c.version.clone()))
            .collect()
    }

    /// Rewrite a version line's floor the way `update` does: at the byte range
    /// the clause occupies, not at the first text that looks like it.
    fn rewrite_floor(updater: &TerraformUpdater, line: &str, new_version: &str) -> String {
        let caps = updater.version_re.captures(line).expect("line parses");
        let dep = ParsedTerraformDep {
            source: "test/provider".to_string(),
            clauses: TerraformUpdater::parse_constraint(&caps),
            version_line_idx: 0,
        };
        let (floor, _) = dep.floor().expect("line has a floor");
        updater.update_line(line, &floor.range, new_version)
    }

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
        assert_eq!(
            clauses_of(&deps[0]),
            vec![("~>".to_string(), "5.0".to_string())]
        );

        assert_eq!(deps[1].source, "hashicorp/random");
        assert_eq!(
            clauses_of(&deps[1]),
            vec![(String::new(), "3.6.0".to_string())]
        );
    }

    #[test]
    fn every_clause_of_a_multi_clause_constraint_is_read() {
        let updater = TerraformUpdater::new();
        let content = r#"
terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 4.0, < 5.0"
    }
  }
}
"#;
        // Terraform requires all of them at once. Reading the set as one
        // operator plus one version swallowed `, < 5.0` into "the version" and
        // the rewrite then deleted the ceiling.
        let deps = updater.parse_content(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(
            clauses_of(&deps[0]),
            vec![
                (">=".to_string(), "4.0".to_string()),
                ("<".to_string(), "5.0".to_string()),
            ]
        );
        assert_eq!(deps[0].constraint_text(), ">= 4.0, < 5.0");
        assert_eq!(deps[0].anchor_version(), "4.0");
        assert!(deps[0].caps_from_above());
    }

    #[test]
    fn a_multi_clause_rewrite_lands_on_the_floor_and_leaves_the_ceiling() {
        let updater = TerraformUpdater::new();

        assert_eq!(
            rewrite_floor(&updater, r#"  version = ">= 4.0, < 5.0""#, "4.67"),
            r#"  version = ">= 4.67, < 5.0""#
        );
        // The ceiling is written first here, so a textual search would rewrite it.
        assert_eq!(
            rewrite_floor(&updater, r#"  version = "< 5.0, >= 4.0""#, "4.67"),
            r#"  version = "< 5.0, >= 4.67""#
        );
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
        assert_eq!(
            clauses_of(&deps[0]),
            vec![(String::new(), "5.1.0".to_string())]
        );
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

        // hashicorp/aws uses ~> 5.0 and latest 5.83.0 satisfies that constraint -
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

        let result = rewrite_floor(&updater, r#"      version = "~> 5.0""#, "5.83");
        assert_eq!(result, r#"      version = "~> 5.83""#);

        let result = rewrite_floor(&updater, r#"      version = ">= 4.9.0""#, "4.10.0");
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
            exclude: Vec::new(),
            ignore: vec!["hashicorp/aws".to_string()],
            pin: pins,
            cooldown: None,
            ..Default::default()
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

    /// A required_providers block for one provider at one constraint.
    fn provider_file(source: &str, constraint: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            "terraform {{\n  required_providers {{\n    p = {{\n      source  = \"{source}\"\n      version = \"{constraint}\"\n    }}\n  }}\n}}\n"
        )
        .unwrap();
        file
    }

    /// A constraint with nothing above it admits every release, so the release
    /// to raise to is the registry's own newest and asking for it is one cheap
    /// request. Routing it through the constrained lookup instead asks the
    /// registry to enumerate and filter, and answers with whatever that
    /// enumeration holds.
    #[tokio::test]
    async fn an_uncapped_constraint_asks_for_the_newest_release_outright() {
        let file = provider_file("hashicorp/aws", ">= 5.0.0");

        let registry = MockRegistry::new("terraform")
            .with_version("hashicorp/aws", "6.61.0")
            .with_constrained("hashicorp/aws", ">= 5.0.0", "5.9.9");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#"version = ">= 6.61.0""#), "{contents}");
    }

    #[tokio::test]
    async fn a_multi_clause_constraint_keeps_its_ceiling() {
        let file = provider_file("hashicorp/aws", ">= 4.0, < 5.0");

        // The unconstrained answer is 6.61.0. If the whole constraint set does not
        // reach the registry, the lookup falls back to it and the rewrite replaces
        // everything after the operator, ceiling included.
        let registry = MockRegistry::new("terraform")
            .with_version("hashicorp/aws", "6.61.0")
            .with_constrained("hashicorp/aws", ">= 4.0, < 5.0", "4.67.0");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains(r#"version = ">= 4.67, < 5.0""#),
            "{contents}"
        );
    }

    #[tokio::test]
    async fn an_exclusive_lower_bound_is_not_raised_over_the_release_it_names() {
        let file = provider_file("hashicorp/aws", "> 4.0");

        // `> 4.0` names the one version the author refuses. Raising it to `> 6.61.0`
        // would write a constraint that excludes the release it was raised to.
        let registry = MockRegistry::new("terraform").with_version("hashicorp/aws", "6.61.0");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.unchanged, 1);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#"version = "> 4.0""#), "{contents}");
    }

    #[tokio::test]
    async fn an_exclusion_that_admits_the_release_is_up_to_date() {
        let file = provider_file("hashicorp/aws", "!= 4.0.0");

        let registry = MockRegistry::new("terraform")
            .with_version("hashicorp/aws", "6.61.0")
            .with_constrained("hashicorp/aws", "!= 4.0.0", "6.61.0");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        // Rewriting the exclusion made `!= 6.61.0`, which rules out the newest
        // release and was reported as a successful major update.
        assert!(result.updated.is_empty());
        assert_eq!(result.unchanged, 1);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#"version = "!= 4.0.0""#), "{contents}");
    }

    /// An exclusion that rules out the newest release there is leaves the
    /// configuration off that release, and saying so is the only report that is
    /// true. The registry still holds releases the exclusion admits, so a
    /// lookup made against the constraint answers with one of those and the
    /// provider reads as current - which is how a dependency held off the
    /// newest release passes under a green tick.
    #[tokio::test]
    async fn an_exclusion_that_rules_out_the_newest_release_names_it() {
        let file = provider_file("hashicorp/aws", "!= 6.61.0");

        let registry = MockRegistry::new("terraform")
            .with_version("hashicorp/aws", "6.61.0")
            .with_constrained("hashicorp/aws", "!= 6.61.0", "6.60.0");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(
            result.warnings[0].contains("6.61.0") && result.warnings[0].contains("!= 6.61.0"),
            "{}",
            result.warnings[0]
        );
        assert_eq!(result.unchanged, 0);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#"version = "!= 6.61.0""#), "{contents}");
    }

    /// A ceiling names no floor to carry forward, so nothing is rewritten. The
    /// release it is behind is still worth naming: no future release will
    /// satisfy it either.
    #[tokio::test]
    async fn a_ceiling_below_every_release_is_left_alone_and_named() {
        let file = provider_file("hashicorp/aws", "< 5.0");

        let registry = MockRegistry::new("terraform")
            .with_version("hashicorp/aws", "6.61.0")
            .with_constrained("hashicorp/aws", "< 5.0", "4.67.0");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(
            result.warnings[0].contains("6.61.0") && result.warnings[0].contains("< 5.0"),
            "{}",
            result.warnings[0]
        );
        assert_eq!(result.unchanged, 0);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#"version = "< 5.0""#), "{contents}");
    }

    /// The control for the two above: a ceiling the newest release still fits
    /// under is current, and says nothing.
    #[tokio::test]
    async fn a_ceiling_the_newest_release_fits_under_is_up_to_date() {
        let file = provider_file("hashicorp/aws", "< 9.0");

        let registry = MockRegistry::new("terraform")
            .with_version("hashicorp/aws", "6.61.0")
            .with_constrained("hashicorp/aws", "< 9.0", "4.67.0");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.unchanged, 1);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains(r#"version = "< 9.0""#), "{contents}");
    }

    #[tokio::test]
    async fn a_bound_no_release_satisfies_names_what_is_available() {
        let file = provider_file("hashicorp/aws", "> 9.9.9");

        let registry = MockRegistry::new("terraform").with_version("hashicorp/aws", "6.61.0");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.warnings.len(), 1);
        assert!(
            result.warnings[0].contains("6.61.0") && result.warnings[0].contains("> 9.9.9"),
            "{}",
            result.warnings[0]
        );
        assert!(result.updated.is_empty());
    }

    #[tokio::test]
    async fn a_pin_that_cannot_be_written_is_an_error() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let file = provider_file("hashicorp/aws", "< 5.0");
        let registry = MockRegistry::new("terraform").with_version("hashicorp/aws", "6.61.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("hashicorp/aws".to_string(), "4.67.0".to_string());
        let config = UpdConfig {
            pin: pins,
            ..Default::default()
        };

        let updater = TerraformUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].contains("cannot pin 'hashicorp/aws'"),
            "{}",
            result.errors[0]
        );
        assert!(result.pinned.is_empty());
    }

    #[test]
    fn test_handles() {
        let updater = TerraformUpdater::new();
        assert!(updater.handles(FileType::TerraformTf));
        assert!(!updater.handles(FileType::Requirements));
        assert!(!updater.handles(FileType::CargoToml));
    }

    #[test]
    fn test_parse_module_with_registry_prefix() {
        let updater = TerraformUpdater::new();
        let content = r#"
module "aws" {
  source  = "registry.terraform.io/hashicorp/aws/aws"
  version = "3.0.0"
}
"#;
        let deps = updater.parse_content(content);
        assert_eq!(deps.len(), 1, "prefixed module source must be parsed");
        // The prefix must be stripped for the internal lookup key.
        assert_eq!(deps[0].source, "hashicorp/aws/aws");
        assert_eq!(deps[0].anchor_version(), "3.0.0");
    }

    #[tokio::test]
    async fn test_update_module_with_registry_prefix_bumps_version_keeps_source() {
        // The HCL source string must remain verbatim; only the version line changes.
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"module "aws" {{
  source  = "registry.terraform.io/hashicorp/aws/aws"
  version = "3.0.0"
}}
"#
        )
        .unwrap();

        let registry = MockRegistry::new("terraform").with_version("hashicorp/aws/aws", "4.0.0");

        let updater = TerraformUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1, "version must be updated");
        assert!(result.errors.is_empty(), "no errors expected");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        // Source string preserved verbatim.
        assert!(
            contents.contains(r#"source  = "registry.terraform.io/hashicorp/aws/aws""#),
            "source attribute must remain unchanged in file"
        );
        // Version bumped.
        assert!(
            contents.contains("4.0.0"),
            "version must be bumped to 4.0.0"
        );
        assert!(!contents.contains("3.0.0"), "old version must be replaced");
    }

    #[test]
    fn test_parse_module_without_prefix_unchanged() {
        // Regression: bare namespace/name/provider still works.
        let updater = TerraformUpdater::new();
        let content = r#"
module "vpc" {
  source  = "hashicorp/aws/aws"
  version = "3.0.0"
}
"#;
        let deps = updater.parse_content(content);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].source, "hashicorp/aws/aws");
        assert_eq!(deps[0].anchor_version(), "3.0.0");
    }

    #[test]
    fn test_skips_non_registry_sources() {
        // Regression: git:: and other non-registry sources must still be skipped.
        let updater = TerraformUpdater::new();
        let content = r#"
module "from_git" {
  source  = "git::https://github.com/example/module.git"
  version = "1.0.0"
}

module "local" {
  source  = "./modules/mymodule"
  version = "1.0.0"
}
"#;
        let deps = updater.parse_content(content);
        assert!(
            deps.is_empty(),
            "git:: and local sources must not trigger registry lookup"
        );
    }

    #[test]
    fn test_pessimistic_constraint_satisfied_two_components() {
        // ~> 4.0 allows >= 4.0, < 5.0 - any 4.x satisfies it
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0", "4.67.1"
        ));
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0", "4.0.0"
        ));
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0", "4.99.99"
        ));
        // Major version changed - no longer satisfied
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
        // ~> 4.0.5 allows >= 4.0.5, < 4.1.0 - any 4.0.x satisfies it
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0.5", "4.0.9"
        ));
        assert!(TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0.5", "4.0.5"
        ));
        // Minor version changed - no longer satisfied
        assert!(!TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0.5", "4.1.0"
        ));
        assert!(!TerraformUpdater::pessimistic_constraint_satisfied(
            "4.0.5", "5.0.0"
        ));
    }

    #[tokio::test]
    async fn test_tilde_gt_two_components_no_change_when_satisfied() {
        // ~> 4.0 with latest 4.67.1 - constraint already covers latest, leave untouched
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
        // ~> 4.0.5 with latest 4.0.9 - same minor, leave untouched
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
        // ~> 4.0 with latest 5.2.1 - major changed, bump to ~> 5.0 (preserve precision)
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
        // ~> 4.0.5 with latest 4.1.0 - minor changed, bump to ~> 4.1.0
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
