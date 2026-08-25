use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, downgrade_warning,
    read_file_safe, unreadable_error, unrewritable_warning, write_file_atomic,
};
use crate::align::compare_versions;
use crate::npm_range::{SpecShape, admits, classify, lower_bound_anchor, rewrite_lower_bound};
use crate::registry::Registry;
use crate::updater::Lang;
use crate::version::{is_prerelease_semver, match_version_precision};
use anyhow::Result;
use futures::future::join_all;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub struct PackageJsonUpdater;

const DEPENDENCY_SECTIONS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];

#[derive(Default)]
struct PackageJsonLineIndex {
    lines_by_section: HashMap<String, HashMap<String, usize>>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct BraceCounts {
    opening: usize,
    closing: usize,
}

impl PackageJsonUpdater {
    pub fn new() -> Self {
        Self
    }

    fn extract_version_info(&self, version_str: &str) -> (String, String) {
        // Extract prefix and version from strings like "^1.0.0", "~2.0.0", ">=3.0.0"
        let prefixes = [">=", "<=", "~>", "^", "~", ">", "<"];

        for prefix in prefixes {
            if let Some(stripped) = version_str.strip_prefix(prefix) {
                return (prefix.to_string(), stripped.to_string());
            }
        }

        // No prefix
        (String::new(), version_str.to_string())
    }

    fn update_version_in_content(
        &self,
        content: &str,
        package: &str,
        old_version: &str,
        new_version: &str,
    ) -> String {
        // Create a pattern that matches this specific package with its version
        let escaped_package = regex::escape(package);
        let escaped_version = regex::escape(old_version);

        // Match: "package": "version" with flexible whitespace
        let pattern = format!(r#""{}"\s*:\s*"{}""#, escaped_package, escaped_version);

        let re = Regex::new(&pattern).expect("Invalid pattern");

        // Replace with new version, preserving the pattern structure
        let replacement = format!(r#""{}": "{}""#, package, new_version);
        re.replace(content, replacement.as_str()).to_string()
    }
}

impl PackageJsonLineIndex {
    fn record_entries(
        lines_by_section: &mut HashMap<String, HashMap<String, usize>>,
        section: &str,
        line: &str,
        entry_re: &Regex,
        line_num: usize,
    ) {
        for caps in entry_re.captures_iter(line) {
            let package = caps.get(1).unwrap().as_str();
            lines_by_section
                .entry(section.to_string())
                .or_default()
                .entry(package.to_string())
                .or_insert(line_num);
        }
    }

    fn count_structural_braces(line: &str) -> BraceCounts {
        let mut counts = BraceCounts::default();
        let mut in_string = false;
        let mut escaped = false;

        for ch in line.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                    continue;
                }

                match ch {
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }

            match ch {
                '"' => in_string = true,
                '{' => counts.opening += 1,
                '}' => counts.closing += 1,
                _ => {}
            }
        }

        counts
    }

    fn from_content(content: &str) -> Self {
        let section_re = Regex::new(
            r#"^\s*"(dependencies|devDependencies|peerDependencies|optionalDependencies)"\s*:\s*(.*)$"#,
        )
        .expect("Invalid section regex");
        let entry_re =
            Regex::new(r#""([^"]+)"\s*:\s*"[^"]*""#).expect("Invalid dependency entry regex");

        let mut lines_by_section: HashMap<String, HashMap<String, usize>> = HashMap::new();
        let mut pending_section: Option<String> = None;
        let mut current_section: Option<String> = None;
        let mut section_brace_balance = 0isize;

        for (line_idx, line) in content.lines().enumerate() {
            let line_num = line_idx + 1;

            if let Some(section) = current_section.as_ref() {
                Self::record_entries(&mut lines_by_section, section, line, &entry_re, line_num);

                let braces = Self::count_structural_braces(line);
                section_brace_balance += braces.opening as isize - braces.closing as isize;
                if section_brace_balance <= 0 {
                    current_section = None;
                    section_brace_balance = 0;
                }

                continue;
            }

            if let Some(section) = pending_section.clone() {
                let braces = Self::count_structural_braces(line);
                if braces.opening == 0 {
                    continue;
                }

                current_section = Some(section.clone());
                pending_section = None;
                Self::record_entries(&mut lines_by_section, &section, line, &entry_re, line_num);
                section_brace_balance = braces.opening as isize - braces.closing as isize;

                if section_brace_balance <= 0 {
                    current_section = None;
                    section_brace_balance = 0;
                }

                continue;
            }

            if let Some(caps) = section_re.captures(line) {
                let section = caps.get(1).unwrap().as_str().to_string();
                let rest = caps.get(2).unwrap().as_str();
                let braces = Self::count_structural_braces(rest);

                if braces.opening == 0 {
                    if rest.trim().is_empty() {
                        pending_section = Some(section);
                    }
                    continue;
                }

                current_section = Some(section.clone());
                Self::record_entries(&mut lines_by_section, &section, line, &entry_re, line_num);
                section_brace_balance = braces.opening as isize - braces.closing as isize;

                if section_brace_balance <= 0 {
                    current_section = None;
                    section_brace_balance = 0;
                }
            }
        }

        Self { lines_by_section }
    }

    fn line_for(&self, section: &str, package: &str) -> Option<usize> {
        self.lines_by_section
            .get(section)
            .and_then(|section_lines| section_lines.get(package).copied())
    }
}

impl Default for PackageJsonUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for PackageJsonUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let json: Value = serde_json::from_str(&content)?;
        let mut result = UpdateResult::default();
        let mut new_content = content.clone();
        let line_index = PackageJsonLineIndex::from_content(&content);

        // First pass: collect all packages and separate by config status
        let mut ignored_packages: Vec<(String, String, String)> = Vec::new();
        let mut pinned_packages: Vec<(String, String, String, String, String, String)> = Vec::new();
        let mut packages_to_check: Vec<(String, String, String, String, String)> = Vec::new();

        for section in DEPENDENCY_SECTIONS {
            if let Some(deps) = json.get(section).and_then(|v| v.as_object()) {
                for (package, version_value) in deps {
                    if let Some(version_str) = version_value.as_str() {
                        // Classify the spec once. Every branch below routes on
                        // the shape, and re-parsing the string per branch is how
                        // the same spec ends up read two different ways.
                        let spec_shape = classify(version_str);

                        // A spec that names no published version - "*",
                        // "latest", a workspace/file/link protocol, a git
                        // shorthand - resolves somewhere other than the
                        // registry. There is nothing to look up, so there is
                        // nothing to report either: warning about these would
                        // put a line on every dependency of every monorepo.
                        if spec_shape == SpecShape::NoVersion {
                            continue;
                        }

                        let (prefix, current_version) = self.extract_version_info(version_str);

                        // Apply config guards uniformly before any per-shape routing.
                        if options.is_package_filtered_out(package) {
                            result.unchanged += 1;
                            continue;
                        }
                        if options.should_ignore(package) {
                            ignored_packages.push((
                                section.to_string(),
                                package.clone(),
                                current_version,
                            ));
                            continue;
                        }

                        if let Some(pinned_version) = options.get_pinned_version(package) {
                            match spec_shape {
                                SpecShape::BoundedRange | SpecShape::ShapeRange => {
                                    // Rewrite the lower bound of the range to the pinned
                                    // version while preserving the upper bound.  We bypass
                                    // pinned_packages because its later loop uses
                                    // match_version_precision on the extracted current_version
                                    // token, which is garbage for comparator specs.
                                    if let Some(new_spec) =
                                        rewrite_lower_bound(version_str, pinned_version)
                                    {
                                        if new_spec != version_str {
                                            let line_num = line_index.line_for(section, package);
                                            result.pinned.push((
                                                package.clone(),
                                                version_str.to_string(),
                                                new_spec.clone(),
                                                line_num,
                                            ));
                                            new_content = self.update_version_in_content(
                                                &new_content,
                                                package,
                                                version_str,
                                                &new_spec,
                                            );
                                        } else {
                                            result.unchanged += 1;
                                        }
                                    } else {
                                        // The pin was configured and could not be
                                        // written, so the manifest does not say
                                        // what the config says it should. That is
                                        // a failed instruction, not a note.
                                        result.errors.push(format!(
                                            "cannot pin '{package}' to '{pinned_version}': '{version_str}' has no lower bound that version fits"
                                        ));
                                    }
                                    continue;
                                }
                                _ => {
                                    // Non-comparator specs go through the standard
                                    // pinned_packages flow (processed after the loop).
                                    pinned_packages.push((
                                        section.to_string(),
                                        package.clone(),
                                        version_str.to_string(),
                                        prefix,
                                        current_version,
                                        pinned_version.to_string(),
                                    ));
                                    continue;
                                }
                            }
                        }

                        // The path after this loop strips the operator off the
                        // spec, looks up the newest release and writes the
                        // operator back in front of it. That reproduces the
                        // range the author wrote only where the operator names
                        // a floor or a pin: ">=1.2.3" becomes ">=4.5.6" and
                        // still means "at least this", while "<1.2.3" becomes
                        // "<4.5.6", which raises a ceiling the author chose and
                        // reports the loosened bound as an update. So the shape
                        // decides the route; whether the token
                        // extract_version_info pulled out happens to parse only
                        // decides whether that path can use it at all.
                        let names_a_floor = matches!(
                            spec_shape,
                            SpecShape::ExactPin | SpecShape::CaretOrTilde | SpecShape::BoundedRange
                        );
                        if !names_a_floor || semver::Version::parse(&current_version).is_err() {
                            match spec_shape {
                                SpecShape::BoundedRange | SpecShape::ShapeRange => {
                                    // A bounded range carries a ceiling its
                                    // author chose independently of the floor,
                                    // so the replacement has to be picked from
                                    // inside the range or the rewrite silently
                                    // discards that ceiling. A shape range gets
                                    // its ceiling from its own floor, like a
                                    // caret, so it follows the newest release
                                    // and the whole shape moves with it.
                                    let bounded = spec_shape == SpecShape::BoundedRange;
                                    let lookup = if bounded {
                                        registry
                                            .get_latest_version_matching(package, version_str)
                                            .await
                                    } else {
                                        registry.get_latest_version(package).await
                                    };
                                    // Cooldown chooses among the same releases
                                    // the lookup was free to return, so it is
                                    // held to the spec only where the spec
                                    // bounds the lookup. Reading a shape range
                                    // as a constraint would confine the choice
                                    // to the shape the manifest states today
                                    // and report an eligible release outside
                                    // it as one the window is holding back.
                                    let cooldown_constraint =
                                        if bounded { Some(version_str) } else { None };
                                    match lookup {
                                        Ok(matched) => {
                                            // Apply cooldown using the lower-bound anchor as
                                            // the current-version proxy.
                                            // held_back_info carries skipped info if cooldown
                                            // chose an older version; it is pushed to
                                            // result.held_back only after the update is confirmed.
                                            let (effective_version, held_back_info) =
                                                if let Some(anchor) =
                                                    lower_bound_anchor(version_str)
                                                {
                                                    let anchor_is_pre =
                                                        is_prerelease_semver(&anchor);
                                                    let (outcome, note) =
                                                        crate::updater::apply_cooldown(
                                                            registry,
                                                            package,
                                                            &anchor,
                                                            &matched,
                                                            cooldown_constraint,
                                                            anchor_is_pre,
                                                            &options,
                                                        )
                                                        .await;
                                                    if let Some(msg) = note {
                                                        options.note_cooldown_unavailable(&msg);
                                                    }
                                                    match outcome {
                                                    crate::updater::CooldownOutcome::Unchanged(
                                                        v,
                                                    ) => (Some(v), None),
                                                    crate::updater::CooldownOutcome::HeldBack {
                                                        chosen,
                                                        skipped_version,
                                                        skipped_published_at,
                                                    } => (
                                                        Some(chosen),
                                                        Some((
                                                            skipped_version,
                                                            skipped_published_at,
                                                        )),
                                                    ),
                                                    crate::updater::CooldownOutcome::Skipped {
                                                        skipped_version,
                                                        skipped_published_at,
                                                    } => {
                                                        result.skipped_by_cooldown.push((
                                                            package.clone(),
                                                            version_str.to_string(),
                                                            skipped_version,
                                                            skipped_published_at,
                                                        ));
                                                        (None, None)
                                                    }
                                                }
                                                } else {
                                                    // No lower bound anchor — no cooldown possible,
                                                    // proceed with the matched version directly.
                                                    (Some(matched), None)
                                                };

                                            if let Some(effective) = effective_version {
                                                // A shape range is looked up
                                                // without a constraint, and the
                                                // release a registry calls the
                                                // latest is a pointer its
                                                // publisher can move back to an
                                                // earlier one. Rewriting the
                                                // shape to it would walk the
                                                // manifest down and report the
                                                // loss as an update.
                                                if lower_bound_anchor(version_str).is_some_and(
                                                    |anchor| {
                                                        compare_versions(
                                                            &effective,
                                                            &anchor,
                                                            Lang::Node,
                                                        )
                                                        .is_lt()
                                                    },
                                                ) {
                                                    result.warnings.push(downgrade_warning(
                                                        package,
                                                        &effective,
                                                        version_str,
                                                    ));
                                                    result.unchanged += 1;
                                                } else if let Some(new_spec) =
                                                    rewrite_lower_bound(version_str, &effective)
                                                {
                                                    if new_spec != version_str
                                                        && lower_bound_anchor(version_str)
                                                            .is_some_and(|cur| {
                                                                !options
                                                                    .allows_bump(&cur, &effective)
                                                            })
                                                    {
                                                        // Bump level exceeds the
                                                        // --only-bump/--max-bump ceiling: leave the
                                                        // dependency spec untouched.
                                                        result.record_capped(
                                                            package,
                                                            version_str,
                                                            &new_spec,
                                                            line_index.line_for(section, package),
                                                        );
                                                    } else if new_spec != version_str {
                                                        let line_num =
                                                            line_index.line_for(section, package);
                                                        result.updated.push((
                                                            package.clone(),
                                                            version_str.to_string(),
                                                            new_spec.clone(),
                                                            line_num,
                                                        ));
                                                        if let Some((
                                                            skipped_version,
                                                            skipped_published_at,
                                                        )) = held_back_info
                                                        {
                                                            result.held_back.push((
                                                                package.clone(),
                                                                version_str.to_string(),
                                                                new_spec.clone(),
                                                                skipped_version,
                                                                skipped_published_at,
                                                            ));
                                                        }
                                                        new_content = self
                                                            .update_version_in_content(
                                                                &new_content,
                                                                package,
                                                                version_str,
                                                                &new_spec,
                                                            );
                                                    } else {
                                                        result.unchanged += 1;
                                                    }
                                                } else {
                                                    // Classification promised a
                                                    // floor and the rewrite found
                                                    // none, so the two disagree
                                                    // about this spec and neither
                                                    // reading can be trusted.
                                                    result.errors.push(format!(
                                                        "{package}: '{version_str}' was read as a range with a lower bound to raise, but it has none"
                                                    ));
                                                }
                                            }
                                            // If effective_version is None the cooldown Skipped
                                            // branch already pushed to skipped_by_cooldown.
                                        }
                                        Err(e) => {
                                            // The lookup did not happen, which is
                                            // not the same as the dependency being
                                            // current. It has to land in the
                                            // "could not be checked" tally, not in
                                            // a warning the green tick prints over.
                                            result.errors.push(format!("{package}: {e}"));
                                        }
                                    }
                                    continue;
                                }
                                SpecShape::OpaqueRange
                                | SpecShape::ExactPin
                                | SpecShape::CaretOrTilde => {
                                    // Specs with no single floor to raise:
                                    // alternation, where no branch is the obvious
                                    // one to edit, upper-only bounds, which have
                                    // no floor at all, and exclusive lower bounds
                                    // (">1.2.3"), whose version is the one the
                                    // author has ruled out. Exact pins and
                                    // caret/tilde ranges reach here only when
                                    // their anchor is spelled in a way the
                                    // ordinary path cannot read ("=1.2.3",
                                    // "^v1.2.3"), which leaves them equally
                                    // unrewritable. upd can still say whether they
                                    // are current, and saying so is the difference
                                    // between a spec that is doing its job and one
                                    // that has quietly frozen a dependency.
                                    match registry.get_latest_version(package).await {
                                        Ok(latest) => match admits(version_str, &latest) {
                                            Some(true) => result.unchanged += 1,
                                            Some(false) => result.warnings.push(
                                                unrewritable_warning(package, &latest, version_str),
                                            ),
                                            None => result
                                                .errors
                                                .push(unreadable_error(package, version_str)),
                                        },
                                        Err(e) => {
                                            result.errors.push(format!("{package}: {e}"));
                                        }
                                    }
                                    continue;
                                }
                                SpecShape::Unsupported => {
                                    // Reporting this as a warning left the run
                                    // exiting 0 with a green tick over a
                                    // dependency nothing had looked at.
                                    result.errors.push(unreadable_error(package, version_str));
                                    continue;
                                }
                                // Left the loop before the lookup, at the top of
                                // the iteration. Repeated rather than made
                                // unreachable so adding a shape here cannot
                                // silently turn into a panic.
                                SpecShape::NoVersion => {
                                    continue;
                                }
                            }
                        }

                        packages_to_check.push((
                            section.to_string(),
                            package.clone(),
                            version_str.to_string(),
                            prefix,
                            current_version,
                        ));
                    }
                }
            }
        }

        // Record ignored packages
        for (section, package, version) in ignored_packages {
            let line_num = line_index.line_for(&section, &package);
            result.ignored.push((package, version, line_num));
        }

        // Process pinned packages (no registry fetch needed)
        for (section, package, version_str, prefix, current_version, pinned_version) in
            pinned_packages
        {
            let matched_version = if options.full_precision {
                pinned_version.clone()
            } else {
                match_version_precision(&current_version, &pinned_version)
            };

            if matched_version != current_version {
                let line_num = line_index.line_for(&section, &package);
                result.pinned.push((
                    package.clone(),
                    current_version.clone(),
                    matched_version.clone(),
                    line_num,
                ));

                // Update in content preserving formatting
                new_content = self.update_version_in_content(
                    &new_content,
                    &package,
                    &version_str,
                    &format!("{}{}", prefix, matched_version),
                );
            } else {
                result.unchanged += 1;
            }
        }

        // Fetch all versions in parallel for non-ignored, non-pinned packages.
        // When the current version is a semver pre-release, request the latest
        // pre-release to avoid silently promoting the package to stable.
        let version_futures: Vec<_> = packages_to_check
            .iter()
            .map(|(_, package, version_str, prefix, current_version)| async {
                if is_prerelease_semver(current_version) {
                    registry
                        .get_latest_version_including_prereleases(package)
                        .await
                } else if matches!(prefix.as_str(), "^" | "~") {
                    // Honor the caret/tilde bound: select the highest version that
                    // satisfies the original spec, never crossing the implied
                    // range (`^4.0.0` stays <5, `~4.17.0` stays <4.18).
                    registry
                        .get_latest_version_matching(package, version_str)
                        .await
                } else {
                    registry.get_latest_version(package).await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results
        for ((section, package, version_str, prefix, current_version), version_result) in
            packages_to_check.into_iter().zip(version_results)
        {
            match version_result {
                Ok(latest_version) => {
                    // When the current version is a pre-release, we fetched the latest
                    // pre-release. If the registry returned a stable version instead
                    // (no newer pre-release exists), refuse silent promotion to stable.
                    let current_is_prerelease = is_prerelease_semver(&current_version);
                    if current_is_prerelease && !is_prerelease_semver(&latest_version) {
                        result.unchanged += 1;
                        continue;
                    }

                    let (outcome, note) = crate::updater::apply_cooldown(
                        registry,
                        &package,
                        &current_version,
                        &latest_version,
                        None,
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
                                package,
                                current_version,
                                skipped_version,
                                skipped_published_at,
                            ));
                            continue;
                        }
                    };

                    // Match the precision of the original version (unless full precision requested)
                    let matched_version = if options.full_precision {
                        latest_version.clone()
                    } else {
                        match_version_precision(&current_version, &latest_version)
                    };
                    if matched_version != current_version {
                        // Refuse to write a downgrade.
                        if compare_versions(&matched_version, &current_version, Lang::Node)
                            != std::cmp::Ordering::Greater
                        {
                            result.warnings.push(downgrade_warning(
                                &package,
                                &matched_version,
                                &current_version,
                            ));
                            result.unchanged += 1;
                        } else if !options.allows_bump(&current_version, &matched_version) {
                            // Bump level exceeds the --only-bump/--max-bump ceiling.
                            result.record_capped(
                                &package,
                                &current_version,
                                &matched_version,
                                line_index.line_for(&section, &package),
                            );
                        } else {
                            let line_num = line_index.line_for(&section, &package);
                            result.updated.push((
                                package.clone(),
                                current_version.clone(),
                                matched_version.clone(),
                                line_num,
                            ));
                            if let Some((skipped_version, skipped_published_at)) = held_back_record
                            {
                                result.held_back.push((
                                    package.clone(),
                                    current_version,
                                    matched_version.clone(),
                                    skipped_version,
                                    skipped_published_at,
                                ));
                            }

                            // Update in content preserving formatting
                            new_content = self.update_version_in_content(
                                &new_content,
                                &package,
                                &version_str,
                                &format!("{}{}", prefix, matched_version),
                            );
                        }
                    } else {
                        result.unchanged += 1;
                    }
                }
                Err(e) => {
                    result.errors.push(format!("{}: {}", package, e));
                }
            }
        }

        if (!result.updated.is_empty() || !result.pinned.is_empty()) && !options.dry_run {
            write_file_atomic(path, &new_content)?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::PackageJson
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let json: Value = serde_json::from_str(&content)?;
        let mut deps = Vec::new();
        let line_index = PackageJsonLineIndex::from_content(&content);

        for section in DEPENDENCY_SECTIONS {
            if let Some(section_deps) = json.get(section).and_then(|v| v.as_object()) {
                for (package, version_value) in section_deps {
                    if let Some(version_str) = version_value.as_str() {
                        // Skip non-version values (git urls, file paths, etc.)
                        if version_str.starts_with("git")
                            || version_str.starts_with("http")
                            || version_str.starts_with("file:")
                            || version_str.contains('/')
                            || version_str == "*"
                            || version_str == "latest"
                        {
                            continue;
                        }

                        let (_, current_version) = self.extract_version_info(version_str);

                        // Skip invalid versions
                        if semver::Version::parse(&current_version).is_err() {
                            continue;
                        }

                        let line_num = line_index.line_for(section, package);
                        deps.push(ParsedDependency {
                            name: package.clone(),
                            version: current_version,
                            line_number: line_num,
                            has_upper_bound: false, // npm versions don't have explicit upper bounds like Python
                            is_bumpable: true,
                        });
                    }
                }
            }
        }

        Ok(deps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{MockRegistry, NpmRegistry};
    use serial_test::serial;
    use std::fs;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: Test-only mutation of the process environment, serialized where needed.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: Test-only restoration of the process environment, serialized where needed.
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn test_extract_version_info() {
        let updater = PackageJsonUpdater::new();

        assert_eq!(
            updater.extract_version_info("^1.0.0"),
            ("^".to_string(), "1.0.0".to_string())
        );

        assert_eq!(
            updater.extract_version_info("~2.0.0"),
            ("~".to_string(), "2.0.0".to_string())
        );

        assert_eq!(
            updater.extract_version_info(">=3.0.0"),
            (">=".to_string(), "3.0.0".to_string())
        );

        assert_eq!(
            updater.extract_version_info("1.0.0"),
            ("".to_string(), "1.0.0".to_string())
        );
    }

    #[tokio::test]
    async fn test_update_package_json_file() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "name": "test-project",
  "dependencies": {{
    "react": "^17.0.0",
    "lodash": "~4.17.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("react", "18.2.0")
            .with_version("lodash", "4.17.21");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.unchanged, 0);

        // Verify file was updated
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^18.2.0"));
        assert!(content.contains("~4.17.21"));
    }

    #[tokio::test]
    async fn test_update_package_json_dry_run() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        let original = r#"{
  "dependencies": {
    "express": "^4.17.0"
  }
}"#;
        write!(file, "{}", original).unwrap();

        let registry = MockRegistry::new("npm").with_version("express", "4.18.2");

        let updater = PackageJsonUpdater::new();
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
    #[serial]
    async fn test_update_package_json_uses_scoped_registry_from_npmrc() {
        let default_registry = MockServer::start().await;
        let scoped_registry = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/@private/pkg"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{
  "dist-tags": { "latest": "1.2.3" },
  "versions": {
    "1.0.0": {},
    "1.2.3": {}
  }
}"#,
            ))
            .expect(1)
            .mount(&scoped_registry)
            .await;

        let mut npmrc = NamedTempFile::new().unwrap();
        writeln!(npmrc, "@private:registry={}", scoped_registry.uri()).unwrap();
        let _npmrc_guard =
            EnvVarGuard::set("NPM_CONFIG_USERCONFIG", npmrc.path().to_str().unwrap());

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "@private/pkg": "^1.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = NpmRegistry::with_registry_url(default_registry.uri());
        let updater = PackageJsonUpdater::new();

        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.errors.is_empty());
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "@private/pkg");
        assert_eq!(result.updated[0].1, "1.0.0");
        assert_eq!(result.updated[0].2, "1.2.3");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"@private/pkg\": \"^1.2.3\""));
    }

    #[tokio::test]
    async fn test_update_package_json_preserves_prefix() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "caret": "^1.0.0",
    "tilde": "~1.0.0",
    "exact": "1.0.0",
    "gte": ">=1.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("caret", "2.0.0")
            .with_version("tilde", "2.0.0")
            .with_version("exact", "2.0.0")
            .with_version("gte", "2.0.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 4);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"^2.0.0\""));
        assert!(content.contains("\"~2.0.0\""));
        assert!(content.contains("\"2.0.0\"")); // exact version
        assert!(content.contains("\">=2.0.0\""));
    }

    #[tokio::test]
    async fn test_update_package_json_honors_caret_and_tilde_bounds() {
        // A caret/tilde spec must not be bumped past the version range it implies:
        // `^4.0.0` stays in 4.x and `~4.17.0` stays in 4.17.x, even when a newer
        // major/minor exists. The registry's range-matching returns the highest
        // in-range version; the updater must consult it rather than the absolute
        // latest.
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "express": "^4.0.0",
    "lodash": "~4.17.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("express", "5.2.1")
            .with_constrained("express", "^4.0.0", "4.21.2")
            .with_version("lodash", "4.18.1")
            .with_constrained("lodash", "~4.17.0", "4.17.21");

        let updater = PackageJsonUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("\"express\": \"^4.21.2\""),
            "caret must stay within 4.x (expected ^4.21.2); got: {content}"
        );
        assert!(
            content.contains("\"lodash\": \"~4.17.21\""),
            "tilde must stay within 4.17.x (expected ~4.17.21); got: {content}"
        );
        assert!(
            !content.contains("5.2.1"),
            "must not cross the caret major bound; got: {content}"
        );
        assert!(
            !content.contains("4.18.1"),
            "must not cross the tilde minor bound; got: {content}"
        );
    }

    #[tokio::test]
    async fn test_update_package_json_dev_dependencies() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "devDependencies": {{
    "typescript": "^4.9.0",
    "jest": "^29.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("typescript", "5.3.3")
            .with_version("jest", "29.7.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^5.3.3"));
        assert!(content.contains("^29.7.0"));
    }

    #[tokio::test]
    async fn test_update_package_json_skips_special_versions() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "local-pkg": "file:../local",
    "git-pkg": "git+https://github.com/user/repo.git",
    "any-version": "*",
    "latest-version": "latest",
    "normal-pkg": "^1.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_version("normal-pkg", "2.0.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Only normal-pkg should be updated
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "normal-pkg");
    }

    #[tokio::test]
    async fn test_update_package_json_line_numbers() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "name": "test",
  "dependencies": {{
    "react": "^17.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_version("react", "18.2.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        // Line number should be found (react is on line 4)
        assert!(result.updated[0].3.is_some());
        assert_eq!(result.updated[0].3, Some(4));
    }

    #[test]
    fn test_line_index_handles_brace_on_next_line() {
        let content = r#"{
  "dependencies":
  {
    "react": "^18.2.0"
  },
  "devDependencies":
  {
    "react": "^18.2.0"
  }
}"#;

        let line_index = PackageJsonLineIndex::from_content(content);

        assert_eq!(line_index.line_for("dependencies", "react"), Some(4));
        assert_eq!(line_index.line_for("devDependencies", "react"), Some(8));
    }

    #[tokio::test]
    async fn test_update_package_json_duplicate_package_names_keep_section_line_numbers() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "react": "^18.2.0"
  }},
  "devDependencies": {{
    "react": "^18.1.0"
  }}
}}"#
        )
        .unwrap();

        let mut pin = std::collections::HashMap::new();
        pin.insert("react".to_string(), "19.0.0".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: Vec::new(),
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &MockRegistry::new("npm"), options)
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.pinned.len(), 2);

        let mut line_numbers: Vec<_> = result
            .pinned
            .iter()
            .map(|(_, _, _, line_num)| line_num.unwrap())
            .collect();
        line_numbers.sort_unstable();
        assert_eq!(line_numbers, vec![3, 6]);
    }

    #[tokio::test]
    async fn test_update_package_json_duplicate_same_versions_keep_line_numbers_with_split_braces()
    {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies":
  {{
    "react": "^18.2.0"
  }},
  "devDependencies":
  {{
    "react": "^18.2.0"
  }}
}}"#
        )
        .unwrap();

        let mut pin = std::collections::HashMap::new();
        pin.insert("react".to_string(), "19.0.0".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: Vec::new(),
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &MockRegistry::new("npm"), options)
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.pinned.len(), 2);

        let mut line_numbers: Vec<_> = result
            .pinned
            .iter()
            .map(|(_, _, _, line_num)| line_num.unwrap())
            .collect();
        line_numbers.sort_unstable();
        assert_eq!(line_numbers, vec![4, 8]);
    }

    #[tokio::test]
    async fn test_update_package_json_registry_error() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "nonexistent-pkg": "^1.0.0"
  }}
}}"#
        )
        .unwrap();

        // Registry without the package
        let registry = MockRegistry::new("npm");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent-pkg"));
    }

    // Tests for config-based ignore/pin functionality

    #[tokio::test]
    async fn test_update_package_json_with_config_ignore() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "name": "test-project",
  "dependencies": {{
    "react": "^17.0.0",
    "lodash": "~4.17.0",
    "express": "^4.17.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("react", "18.2.0")
            .with_version("lodash", "4.17.21")
            .with_version("express", "4.18.2");

        // Create config that ignores lodash
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["lodash".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
            ..Default::default()
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 packages updated (react, express), 1 ignored (lodash)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "lodash");
        assert_eq!(result.ignored[0].1, "4.17.0");

        // Verify file was updated only for non-ignored packages
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^18.2.0"));
        assert!(content.contains("~4.17.0")); // lodash unchanged
        assert!(content.contains("^4.18.2"));
    }

    #[tokio::test]
    async fn test_update_package_json_with_config_pin() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "name": "test-project",
  "dependencies": {{
    "react": "^17.0.0",
    "lodash": "~4.17.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("react", "18.2.0")
            .with_version("lodash", "4.17.21");

        // Create config that pins react to 17.0.2
        let mut pin = std::collections::HashMap::new();
        pin.insert("react".to_string(), "17.0.2".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: Vec::new(),
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 package updated from registry (lodash), 1 pinned (react)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "lodash");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "react");
        assert_eq!(result.pinned[0].1, "17.0.0"); // old
        assert_eq!(result.pinned[0].2, "17.0.2"); // new (pinned)

        // Verify file was updated with pinned version
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^17.0.2"));
        assert!(content.contains("~4.17.21"));
    }

    #[tokio::test]
    async fn test_update_package_json_with_config_ignore_and_pin() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "name": "test-project",
  "dependencies": {{
    "react": "^17.0.0",
    "lodash": "~4.17.0",
    "express": "^4.17.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("react", "18.2.0")
            .with_version("lodash", "4.17.21")
            .with_version("express", "4.18.2");

        // Config: ignore lodash, pin express to 4.17.3
        let mut pin = std::collections::HashMap::new();
        pin.insert("express".to_string(), "4.17.3".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["lodash".to_string()],
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 updated from registry (react), 1 ignored (lodash), 1 pinned (express)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "react");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "lodash");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "express");
        assert_eq!(result.pinned[0].2, "4.17.3");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^18.2.0")); // react updated from registry
        assert!(content.contains("~4.17.0")); // lodash unchanged (ignored)
        assert!(content.contains("^4.17.3")); // express pinned version
    }

    #[tokio::test]
    async fn test_update_package_json_dev_deps_with_config() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "devDependencies": {{
    "typescript": "^4.9.0",
    "jest": "^29.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("typescript", "5.3.3")
            .with_version("jest", "29.7.0");

        // Config: ignore typescript
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["typescript".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
            ..Default::default()
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 updated (jest), 1 ignored (typescript)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "jest");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "typescript");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^4.9.0")); // typescript unchanged
        assert!(content.contains("^29.7.0"));
    }

    #[tokio::test]
    async fn test_update_package_json_pin_preserves_prefix() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "caret": "^1.0.0",
    "tilde": "~1.0.0",
    "exact": "1.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("caret", "2.0.0")
            .with_version("tilde", "2.0.0")
            .with_version("exact", "2.0.0");

        // Pin all with specific versions
        let mut pin = std::collections::HashMap::new();
        pin.insert("caret".to_string(), "1.5.0".to_string());
        pin.insert("tilde".to_string(), "1.5.0".to_string());
        pin.insert("exact".to_string(), "1.5.0".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: Vec::new(),
            pin,
            cooldown: None,
            ..Default::default()
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.pinned.len(), 3);

        let content = fs::read_to_string(file.path()).unwrap();
        // Prefixes should be preserved
        assert!(content.contains("\"^1.5.0\""));
        assert!(content.contains("\"~1.5.0\""));
        assert!(content.contains("\"1.5.0\"")); // exact version
    }

    #[tokio::test]
    async fn test_update_package_json_peer_and_optional_dependencies() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "peerDependencies": {{
    "react": "^17.0.0"
  }},
  "optionalDependencies": {{
    "fsevents": "^2.3.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("react", "18.2.0")
            .with_version("fsevents", "2.3.3");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let names: std::collections::HashSet<_> = result
            .updated
            .iter()
            .map(|(n, _, _, _)| n.as_str())
            .collect();
        assert!(names.contains("react"), "peerDependencies must be updated");
        assert!(
            names.contains("fsevents"),
            "optionalDependencies must be updated"
        );

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^18.2.0"));
        assert!(content.contains("^2.3.3"));
    }

    #[tokio::test]
    async fn test_update_package_json_skips_workspace_protocol() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "local-lib": "workspace:^",
    "other-lib": "workspace:*",
    "pinned-lib": "workspace:1.0.0",
    "real-pkg": "^1.0.0"
  }}
}}"#
        )
        .unwrap();

        // Only real-pkg has a version in the registry; workspace:* entries
        // must be silently skipped (not treated as errors).
        let registry = MockRegistry::new("npm").with_version("real-pkg", "2.0.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "real-pkg");
        assert!(
            result.errors.is_empty(),
            "workspace: protocol must not produce errors"
        );
        assert!(
            result.warnings.is_empty(),
            "nor warnings: a workspace dependency has no registry version to be \
             behind, so a line about it is noise on every dependency of every \
             monorepo: {:?}",
            result.warnings
        );

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"workspace:^\""));
        assert!(content.contains("\"workspace:*\""));
        assert!(content.contains("\"workspace:1.0.0\""));
    }

    #[tokio::test]
    async fn test_update_package_json_does_not_touch_overrides() {
        // `overrides` is not part of DEPENDENCY_SECTIONS — any pin in there
        // must be left untouched. This guards against accidental drift if
        // the DEPENDENCY_SECTIONS list is reshuffled.
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "lodash": "^4.17.20"
  }},
  "overrides": {{
    "lodash": "4.17.21"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_version("lodash", "4.17.22");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("\"overrides\": {\n    \"lodash\": \"4.17.21\""),
            "overrides section must be preserved verbatim, got:\n{content}"
        );
        assert!(content.contains("\"^4.17.22\""));
    }

    #[tokio::test]
    async fn test_update_package_json_scoped_package_name() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "@types/node": "^18.0.0",
    "@scope/private-thing": "^1.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("@types/node", "20.11.0")
            .with_version("@scope/private-thing", "1.2.3");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let names: Vec<&str> = result
            .updated
            .iter()
            .map(|(n, _, _, _)| n.as_str())
            .collect();
        assert!(names.contains(&"@types/node"));
        assert!(names.contains(&"@scope/private-thing"));

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"@types/node\": \"^20.11.0\""));
        assert!(content.contains("\"@scope/private-thing\": \"^1.2.3\""));
    }

    /// When the current version is a semver pre-release, the updater must seek the
    /// latest pre-release rather than promoting to stable.
    #[tokio::test]
    async fn test_semver_prerelease_stays_on_prerelease() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "my-lib": "1.0.0-beta.1"
  }}
}}"#
        )
        .unwrap();

        // stable=1.0.0, prerelease=1.0.0-rc.1
        let registry = MockRegistry::new("npm").with_prerelease("my-lib", "1.0.0", "1.0.0-rc.1");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1, "should update to pre-release");
        assert_eq!(
            result.updated[0].2, "1.0.0-rc.1",
            "should pick pre-release, not stable"
        );

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("1.0.0-rc.1"),
            "file must contain pre-release version"
        );
        assert!(!content.contains("\"1.0.0\""), "must not promote to stable");
    }

    /// When no newer pre-release exists and only a newer stable is available,
    /// a pre-release-pinned package must not be silently promoted to stable.
    #[tokio::test]
    async fn test_semver_prerelease_no_silent_promotion_to_stable() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "my-lib": "1.0.0-beta.1"
  }}
}}"#
        )
        .unwrap();

        // Registry only has a stable version — no pre-release at all.
        // get_latest_version_including_prereleases will return "2.0.0" (stable),
        // which is newer than 1.0.0-beta.1. Without the guard this would silently promote.
        let registry = MockRegistry::new("npm").with_version("my-lib", "2.0.0");

        let updater = PackageJsonUpdater::new();
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

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("1.0.0-beta.1"),
            "version must remain unchanged"
        );
        assert!(!content.contains("2.0.0"), "must not promote to stable");
    }

    #[tokio::test]
    async fn test_update_package_json_bumps_lower_bound_of_comparator_range() {
        use crate::registry::MockRegistry;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "ranged": ">=1.0.0 <2.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("ranged", "2.0.0")
            .with_constrained("ranged", ">=1.0.0 <2.0.0", "1.5.0");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(
            result.updated.len(),
            1,
            "expected comparator range to be updated"
        );
        assert_eq!(result.updated[0].0, "ranged");
        assert_eq!(result.updated[0].1, ">=1.0.0 <2.0.0");
        assert_eq!(result.updated[0].2, ">=1.5.0 <2.0.0");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("\">=1.5.0 <2.0.0\""),
            "file must contain the rewritten range, got: {content}"
        );
    }

    #[tokio::test]
    async fn test_update_package_json_warns_on_unsupported_range_shape() {
        use crate::registry::MockRegistry;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "orranged": "^1.0.0 || ^2.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_version("orranged", "3.0.0");
        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.updated.is_empty(), "OR ranges must not be rewritten");
        assert_eq!(
            result.warnings.len(),
            1,
            "unsupported shape must surface a warning"
        );
        assert!(
            result.warnings[0].contains("^1.0.0 || ^2.0.0"),
            "warning should mention the offending spec: {}",
            result.warnings[0]
        );
    }

    /// Current stable package must still skip pre-releases (regression guard).
    #[tokio::test]
    async fn test_semver_stable_skips_prerelease_regression() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "my-lib": "^1.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_prerelease("my-lib", "2.0.0", "3.0.0-rc.1");

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Should update to 2.0.0 (stable), not 3.0.0-rc.1 (pre-release)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].2, "2.0.0");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("^2.0.0"));
        assert!(!content.contains("3.0.0-rc.1"));
    }

    /// Regression: a comparator-range spec for an ignored package must not be rewritten.
    #[tokio::test]
    async fn test_update_package_json_respects_ignore_for_comparator_range() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "ranged": ">=1.0.0 <2.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("ranged", "2.0.0")
            .with_constrained("ranged", ">=1.0.0 <2.0.0", "1.5.0");

        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: vec!["ranged".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
            ..Default::default()
        };
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let updater = PackageJsonUpdater::new();
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(
            result.updated.is_empty(),
            "ignored package must not be updated"
        );
        assert_eq!(result.ignored.len(), 1, "ignored package must be recorded");
        assert_eq!(result.ignored[0].0, "ranged");
    }

    /// Regression: a fresh comparator-range release within the cooldown window must not
    /// bump the spec's lower bound.
    #[tokio::test]
    async fn test_update_package_json_comparator_range_respects_cooldown() {
        use crate::cooldown::CooldownPolicy;
        use chrono::{Duration, Utc};

        let now = Utc::now();

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "ranged": ">=1.0.0 <2.0.0"
  }}
}}"#
        )
        .unwrap();

        // Latest matching version was published just 1 day ago — inside a 7-day cooldown.
        let registry = MockRegistry::new("npm")
            .with_version("ranged", "2.0.0")
            .with_constrained("ranged", ">=1.0.0 <2.0.0", "1.5.0")
            .with_version_meta(
                "ranged",
                "1.5.0",
                Some(now - Duration::days(1)),
                false,
                false,
            );

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: std::collections::HashMap::new(),
            force_override: None,
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_cooldown_policy(policy, now);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(
            result.updated.is_empty(),
            "spec must not be rewritten when the only candidate is inside the cooldown window"
        );
        assert_eq!(
            result.skipped_by_cooldown.len(),
            1,
            "fresh release must be recorded in skipped_by_cooldown"
        );
        assert_eq!(result.skipped_by_cooldown[0].0, "ranged");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("\">=1.0.0 <2.0.0\""),
            "file must be unchanged when cooldown prevents the update, got: {content}"
        );
    }

    /// A shape range takes its ceiling from its own floor, so the shape moves
    /// with the release it is rewritten to and every newer release is a
    /// candidate. Read as a constraint, `4.3.x` says the opposite: it pins the
    /// choice inside 4.3, so an eligible 4.4.0 is reported as held back by a
    /// cooldown window it is well outside of.
    #[tokio::test]
    async fn a_shape_range_is_not_confined_to_its_own_shape_by_cooldown() {
        use crate::cooldown::CooldownPolicy;
        use chrono::{Duration, Utc};

        let now = Utc::now();

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "shaped": "4.3.x"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("shaped", "4.4.0")
            .with_version_meta(
                "shaped",
                "4.3.0",
                Some(now - Duration::days(400)),
                false,
                false,
            )
            .with_version_meta(
                "shaped",
                "4.4.0",
                Some(now - Duration::days(30)),
                false,
                false,
            );

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: std::collections::HashMap::new(),
            force_override: None,
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_cooldown_policy(policy, now);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(
            result.skipped_by_cooldown.is_empty(),
            "4.4.0 is 30 days old under a 7 day window: {:?}",
            result.skipped_by_cooldown
        );
        assert_eq!(result.updated.len(), 1, "{:?}", result.updated);
        assert_eq!(result.updated[0].1, "4.3.x");
        assert_eq!(result.updated[0].2, "4.4.x");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"4.4.x\""), "got: {content}");
    }

    /// The mirror of the case above: a bounded range carries a ceiling its
    /// author chose, so cooldown has to be held to it. Left unconstrained,
    /// cooldown steps over the ceiling to find a release old enough to ship
    /// and the floor is rewritten above the bound that is still standing
    /// beside it, leaving a range no version can satisfy.
    #[tokio::test]
    async fn cooldown_may_not_step_over_the_ceiling_a_bounded_range_states() {
        use crate::cooldown::CooldownPolicy;
        use chrono::{Duration, Utc};

        let now = Utc::now();

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "bounded": ">=1.0.0 <2.0.0"
  }}
}}"#
        )
        .unwrap();

        // 1.9.0 is the newest release inside the range and is too new to ship.
        // 2.5.0 is old enough but sits above the ceiling, so it is not a
        // candidate at all.
        let registry = MockRegistry::new("npm")
            .with_constrained("bounded", ">=1.0.0 <2.0.0", "1.9.0")
            .with_version_meta(
                "bounded",
                "1.0.0",
                Some(now - Duration::days(500)),
                false,
                false,
            )
            .with_version_meta(
                "bounded",
                "1.9.0",
                Some(now - Duration::days(2)),
                false,
                false,
            )
            .with_version_meta(
                "bounded",
                "2.5.0",
                Some(now - Duration::days(400)),
                false,
                false,
            );

        let policy = CooldownPolicy {
            default: Duration::days(7),
            per_ecosystem: std::collections::HashMap::new(),
            force_override: None,
        };

        let updater = PackageJsonUpdater::new();
        let options = UpdateOptions::new(false, false).with_cooldown_policy(policy, now);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(
            result.updated.is_empty(),
            "2.5.0 is outside the range and 1.9.0 is inside the window: {:?}",
            result.updated
        );
        assert_eq!(
            result.skipped_by_cooldown.len(),
            1,
            "{:?}",
            result.skipped_by_cooldown
        );
        assert_eq!(result.skipped_by_cooldown[0].2, "1.9.0");

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\">=1.0.0 <2.0.0\""), "got: {content}");
    }

    /// A shape range is looked up unconstrained, and npm's `latest` tag is a
    /// pointer its publisher can move backwards: after a bad release it names
    /// an earlier one. Rewriting the shape to it would walk the manifest down
    /// a major and report the loss as an update.
    #[tokio::test]
    async fn a_shape_range_is_never_rewritten_downwards() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "shaped": "4.3.x"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_version("shaped", "4.2.9");

        let updater = PackageJsonUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty(), "{:?}", result.updated);
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(
            result.warnings[0].contains("shaped") && result.warnings[0].contains("4.2.9"),
            "{}",
            result.warnings[0]
        );

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"4.3.x\""), "got: {content}");
    }

    /// Regression: pinning a comparator-range spec must preserve the upper bound.
    ///
    /// Before the fix, the pinned_packages loop would call match_version_precision on
    /// the garbage token produced by extract_version_info for ">=1.0.0 <2.0.0", causing
    /// the upper bound to be silently dropped (result: ">=1.5.0" instead of
    /// ">=1.5.0 <2.0.0").
    #[tokio::test]
    async fn test_update_package_json_pinned_comparator_range_preserves_upper_bound() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "ranged": ">=1.0.0 <2.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_version("ranged", "2.0.0");

        let mut pin = std::collections::HashMap::new();
        pin.insert("ranged".to_string(), "1.5.0".to_string());
        let config = UpdConfig {
            exclude: Vec::new(),
            ignore: Vec::new(),
            pin,
            cooldown: None,
            ..Default::default()
        };
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let updater = PackageJsonUpdater::new();
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(
            result.pinned.len(),
            1,
            "pinned comparator range must be recorded"
        );
        assert_eq!(result.pinned[0].0, "ranged");
        assert_eq!(
            result.pinned[0].2, ">=1.5.0 <2.0.0",
            "upper bound must be preserved"
        );

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(
            content.contains("\">=1.5.0 <2.0.0\""),
            "file must preserve upper bound, got: {content}"
        );
    }

    /// A hyphen range is npm's other way of writing a floor and a ceiling, and
    /// it has to survive the rewrite as one. Writing the raised floor as a bare
    /// version would drop the ceiling the author chose.
    #[tokio::test]
    async fn a_hyphen_range_keeps_its_ceiling_and_its_shape() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "hyphened": "4.17.0 - 4.18.0"
  }}
}}"#
        )
        .unwrap();

        // The newest release is outside the range, so only a lookup that
        // respects the ceiling can produce a version the range still admits.
        let registry = MockRegistry::new("npm")
            .with_version("hyphened", "5.0.0")
            .with_constrained("hyphened", "4.17.0 - 4.18.0", "4.17.21");

        let result = PackageJsonUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1, "{:?}", result.warnings);
        assert_eq!(result.updated[0].2, "4.17.21 - 4.18.0");
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("\"4.17.21 - 4.18.0\""), "{content}");
    }

    /// A wildcard range has no ceiling of its own: like a caret, it takes one
    /// from wherever its floor lands. So it follows the newest release rather
    /// than the newest release inside itself, and keeps its own width and
    /// wildcard character on the way.
    #[tokio::test]
    async fn a_wildcard_range_follows_the_newest_release_and_keeps_its_shape() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "shaped": "4.3.x",
    "widened": "^1.2"
  }}
}}"#
        )
        .unwrap();

        // The constrained answers are what a ceiling-respecting lookup would
        // return. A wildcard range must not use them: it is not pinned inside
        // itself, and taking 4.3.9 here would freeze it at 4.3 forever.
        let registry = MockRegistry::new("npm")
            .with_version("shaped", "4.4.3")
            .with_constrained("shaped", "4.3.x", "4.3.9")
            .with_version("widened", "3.1.4")
            .with_constrained("widened", "^1.2", "1.9.9");

        let result = PackageJsonUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        let mut written: Vec<(&str, &str)> = result
            .updated
            .iter()
            .map(|(package, _, new, _)| (package.as_str(), new.as_str()))
            .collect();
        written.sort();
        assert_eq!(
            written,
            vec![("shaped", "4.4.x"), ("widened", "^3.1")],
            "warnings: {:?} errors: {:?}",
            result.warnings,
            result.errors
        );
    }

    /// A range with no floor to raise is still a dependency upd can say
    /// something true about. Saying it outright is the difference between a
    /// spec doing its job and one that has quietly frozen a dependency.
    #[tokio::test]
    async fn a_range_that_cannot_be_rewritten_is_still_checked() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "current": "^1.0.0 || ^2.0.0",
    "behind": "^1.0.0 || ^2.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("current", "2.5.0")
            .with_version("behind", "3.0.0");

        let result = PackageJsonUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(
            result.updated.is_empty(),
            "alternation must not be rewritten"
        );
        assert_eq!(
            result.unchanged, 1,
            "the range admits 2.5.0, so 'current' is current"
        );
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(
            result.warnings[0].starts_with("behind: 3.0.0 is available"),
            "the warning has to name what is available: {}",
            result.warnings[0]
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    /// A bound that is not a floor must not be moved as if it were one. Both
    /// forms here were rewritten and reported as updates: `"<4.0.0"` became
    /// `"<4.18.1"`, raising a ceiling its author chose deliberately, and
    /// `">4.0.0 <5.0.0"` became `">4.18.1 <5.0.0"`, a range that excludes the
    /// very release written into it and leaves the next run unable to resolve
    /// the dependency at all.
    #[tokio::test]
    async fn a_bound_that_is_not_a_floor_is_not_raised() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "ceiling": "<4.0.0",
    "ceiling_inclusive": "<=4.0.0",
    "excluded_floor": ">4.0.0",
    "excluded_floor_capped": ">4.0.0 <5.0.0",
    "floor": ">=4.0.0"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm")
            .with_version("ceiling", "4.18.1")
            .with_version("ceiling_inclusive", "6.0.0")
            .with_version("excluded_floor", "4.18.1")
            .with_version("excluded_floor_capped", "6.0.0")
            .with_constrained("excluded_floor_capped", ">4.0.0 <5.0.0", "4.18.1")
            .with_version("floor", "4.18.1");

        let result = PackageJsonUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        // The positive control: the one bound here that does name a floor still
        // moves, so a run that rewrites nothing cannot pass this test.
        let written: Vec<&str> = result
            .updated
            .iter()
            .map(|(package, _, _, _)| package.as_str())
            .collect();
        assert_eq!(
            written,
            vec!["floor"],
            "warnings: {:?} errors: {:?}",
            result.warnings,
            result.errors
        );

        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains(r#""ceiling": "<4.0.0""#), "{content}");
        assert!(
            content.contains(r#""ceiling_inclusive": "<=4.0.0""#),
            "{content}"
        );
        assert!(
            content.contains(r#""excluded_floor": ">4.0.0""#),
            "{content}"
        );
        assert!(
            content.contains(r#""excluded_floor_capped": ">4.0.0 <5.0.0""#),
            "{content}"
        );
        assert!(content.contains(r#""floor": ">=4.18.1""#), "{content}");

        // ">4.0.0" already admits the newest release, so there is nothing to
        // say about it; the other three are held below one and are reported.
        assert_eq!(
            result.unchanged, 1,
            "'>4.0.0' admits 4.18.1, so it is current: {:?}",
            result.warnings
        );
        let mut warned: Vec<&str> = result
            .warnings
            .iter()
            .map(|w| w.split(':').next().unwrap())
            .collect();
        warned.sort();
        assert_eq!(
            warned,
            vec!["ceiling", "ceiling_inclusive", "excluded_floor_capped"]
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    /// A spec upd cannot read is a dependency nothing looked at. It goes in the
    /// error tally, which withholds the green tick and fails the run, because a
    /// warning left the run exiting 0 and claiming everything was checked.
    #[tokio::test]
    async fn a_spec_upd_cannot_read_is_an_error_not_a_warning() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "broken": ">=1.0.0 <<2"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm").with_version("broken", "3.0.0");

        let result = PackageJsonUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
        assert!(
            result.errors[0].contains("broken") && result.errors[0].contains(">=1.0.0 <<2"),
            "the error has to name the package and the spec: {}",
            result.errors[0]
        );
        assert_eq!(
            result.unchanged, 0,
            "an unreadable spec must not be counted as up to date"
        );
    }

    /// The counterpart to the test above: a spec that resolves somewhere other
    /// than the registry, or at the registry but by a name it re-decides daily,
    /// is not unreadable - it is out of scope. Reporting it would put a line on
    /// every dependency of every monorepo, and `latest` is only the most common
    /// of the tags npm resolves this way.
    #[tokio::test]
    async fn a_spec_that_names_no_published_version_is_reported_nowhere() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
  "dependencies": {{
    "floating": "*",
    "tagged": "latest",
    "prereleased": "next",
    "testing": "beta",
    "sibling": "workspace:*",
    "forked": "github:chalk/chalk#v5.3.0",
    "local": "file:../local"
  }}
}}"#
        )
        .unwrap();

        let registry = MockRegistry::new("npm");

        let result = PackageJsonUpdater::new()
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(
            result.unchanged, 0,
            "counting these as checked would let the tick speak for them"
        );
    }
}
