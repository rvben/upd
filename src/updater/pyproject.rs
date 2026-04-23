use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, downgrade_warning,
    read_file_safe, write_file_atomic,
};
use crate::align::compare_versions;
use crate::registry::{MultiPyPiRegistry, PyPiRegistry, Registry};
use crate::updater::Lang;
use crate::version::{is_prerelease_pep440, is_stable_pep440, match_version_precision};
use anyhow::{Result, anyhow};
use futures::future::join_all;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use toml_edit::{DocumentMut, Formatted, Item, Value};

pub struct PyProjectUpdater {
    // Regex to extract version from dependency string
    // Matches: package==1.0.0, package>=1.0.0, package[extra]>=1.0.0, etc.
    version_re: Regex,
    // Regex to capture the full constraint including additional constraints after commas
    // E.g., ">=2.8.0,<9" or ">=1.0.0,!=1.5.0,<2.0.0"
    constraint_re: Regex,
}

#[derive(Default)]
struct PyProjectLineIndex {
    lines_by_section: HashMap<String, HashMap<String, usize>>,
}

#[derive(Clone)]
struct ArraySectionState {
    section_path: String,
    depth: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ArrayBracketCounts {
    opening: usize,
    closing: usize,
}

impl PyProjectUpdater {
    pub fn new() -> Self {
        let version_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)(\[[^\]]+\])?\s*(==|>=|<=|~=|!=|>|<)\s*([^\s,;]+)",
        )
        .expect("Invalid regex");

        // Match the full constraint including additional constraints after commas
        // E.g., ">=2.8.0,<9" or ">=1.0.0,!=1.5.0,<2.0.0"
        let constraint_re = Regex::new(
            r"^([a-zA-Z0-9][-a-zA-Z0-9._]*)(\[[^\]]+\])?\s*((?:==|>=|<=|~=|!=|>|<)[^\s;]+(?:\s*,\s*(?:==|>=|<=|~=|!=|>|<)[^\s;,]+)*)",
        )
        .expect("Invalid regex");

        Self {
            version_re,
            constraint_re,
        }
    }

    /// Parse dependency string and return (package, first_version, full_constraint)
    fn parse_dependency(&self, dep: &str) -> Option<(String, String, String)> {
        // First get the full constraint
        let full_constraint = self
            .constraint_re
            .captures(dep)
            .and_then(|c| c.get(3))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();

        self.version_re.captures(dep).map(|caps| {
            let package = caps.get(1).unwrap().as_str().to_string();
            let version = caps.get(4).unwrap().as_str().to_string();
            (package, version, full_constraint)
        })
    }

    /// Check if constraint is simple (no upper bounds that could be violated)
    fn is_simple_constraint(constraint: &str) -> bool {
        // If there are multiple constraints (comma-separated), need constraint-aware lookup
        if constraint.contains(',') {
            return false;
        }

        // If the constraint has an upper-bound operator, need constraint-aware lookup
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

    /// Check if constraint is an upper-bound-only constraint (e.g., "<6", "<=5.0")
    /// These should never be "updated" because they define a ceiling, not a floor.
    /// Updating them would only make the constraint more restrictive.
    fn is_upper_bound_only(constraint: &str) -> bool {
        let trimmed = constraint.trim();
        (trimmed.starts_with('<') || trimmed.starts_with("<=")) && !trimmed.contains(',') // No other constraints (like >=x,<y)
    }

    fn update_dependency(&self, dep: &str, new_version: &str) -> String {
        if let Some(caps) = self.version_re.captures(dep) {
            // Only replace the version number itself, preserving everything else
            // (package name, extras, operator, AND any additional constraints like ,<6)
            let version_match = caps.get(4).unwrap();

            let mut result = dep.to_string();
            result.replace_range(version_match.range(), new_version);
            result
        } else {
            dep.to_string()
        }
    }

    fn assignment_parts(line: &str) -> Option<(String, &str)> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
            return None;
        }

        let (key, value) = trimmed.split_once('=')?;
        Some((
            key.trim().trim_matches('"').trim_matches('\'').to_string(),
            value.trim(),
        ))
    }

    /// Extract index URLs from pyproject.toml for Poetry and PDM configurations
    /// Poetry uses [[tool.poetry.source]] with url field
    /// PDM uses [[tool.pdm.source]] with url field
    /// Returns (primary_url, extra_urls) where primary is the first found or default PyPI
    fn extract_index_urls(doc: &DocumentMut) -> (Option<String>, Vec<String>) {
        let mut urls: Vec<String> = Vec::new();

        // Check [[tool.poetry.source]]
        if let Some(Item::Table(tool)) = doc.get("tool")
            && let Some(Item::Table(poetry)) = tool.get("poetry")
            && let Some(Item::ArrayOfTables(sources)) = poetry.get("source")
        {
            for source in sources.iter() {
                if let Some(Item::Value(Value::String(url))) = source.get("url") {
                    let url_str = url.value().to_string();
                    if !url_str.is_empty() {
                        urls.push(url_str);
                    }
                }
            }
        }

        // Check [[tool.pdm.source]]
        if let Some(Item::Table(tool)) = doc.get("tool")
            && let Some(Item::Table(pdm)) = tool.get("pdm")
            && let Some(Item::ArrayOfTables(sources)) = pdm.get("source")
        {
            for source in sources.iter() {
                if let Some(Item::Value(Value::String(url))) = source.get("url") {
                    let url_str = url.value().to_string();
                    if !url_str.is_empty() && !urls.contains(&url_str) {
                        urls.push(url_str);
                    }
                }
            }
        }

        // Also check for uv's [[tool.uv.index]] format
        if let Some(Item::Table(tool)) = doc.get("tool")
            && let Some(Item::Table(uv)) = tool.get("uv")
            && let Some(Item::ArrayOfTables(indexes)) = uv.get("index")
        {
            for index in indexes.iter() {
                if let Some(Item::Value(Value::String(url))) = index.get("url") {
                    let url_str = url.value().to_string();
                    if !url_str.is_empty() && !urls.contains(&url_str) {
                        urls.push(url_str);
                    }
                }
            }
        }

        if urls.is_empty() {
            (None, Vec::new())
        } else {
            let primary = urls.remove(0);
            (Some(primary), urls)
        }
    }

    /// Create a registry from the pyproject.toml index configuration
    /// If no index URLs are found, returns None to use the default registry
    fn create_registry_from_config(doc: &DocumentMut) -> Option<Arc<dyn Registry + Send + Sync>> {
        let (primary_url, extra_urls) = Self::extract_index_urls(doc);

        if let Some(url) = primary_url {
            let primary = PyPiRegistry::from_url(&url);
            if extra_urls.is_empty() {
                Some(Arc::new(primary))
            } else {
                Some(Arc::new(MultiPyPiRegistry::from_primary_and_extras(
                    primary, extra_urls,
                )))
            }
        } else {
            None
        }
    }

    async fn update_array_deps(
        &self,
        array: &mut toml_edit::Array,
        registry: &dyn Registry,
        result: &mut UpdateResult,
        line_index: &PyProjectLineIndex,
        section_path: &str,
        options: &UpdateOptions,
    ) {
        // First pass: collect all dependencies and separate by config status
        let mut ignored_deps: Vec<(String, String, Option<usize>)> = Vec::new();
        let mut pinned_deps: Vec<(usize, String, String, String, String, Option<usize>)> =
            Vec::new();
        let mut deps_to_check: Vec<(usize, String, String, String, String, Option<usize>)> =
            Vec::new();

        for i in 0..array.len() {
            if let Some(item) = array.get(i)
                && let Some(s) = item.as_str()
                && let Some((package, current_version, full_constraint)) = self.parse_dependency(s)
            {
                let line_num = line_index.line_for(section_path, &package);

                if options.is_package_filtered_out(&package) {
                    result.unchanged += 1;
                    continue;
                }

                // Check if package should be ignored
                if options.should_ignore(&package) {
                    ignored_deps.push((package, current_version, line_num));
                    continue;
                }

                // Check if package has a pinned version
                if let Some(pinned_version) = options.get_pinned_version(&package) {
                    pinned_deps.push((
                        i,
                        s.to_string(),
                        package,
                        current_version,
                        pinned_version.to_string(),
                        line_num,
                    ));
                    continue;
                }

                deps_to_check.push((
                    i,
                    s.to_string(),
                    package,
                    current_version,
                    full_constraint,
                    line_num,
                ));
            }
        }

        // Record ignored packages
        for (package, version, line_num) in ignored_deps {
            result.ignored.push((package, version, line_num));
        }

        // Process pinned packages (no registry fetch needed)
        let mut updates: Vec<(usize, String)> = Vec::new();
        for (i, dep_str, package, current_version, pinned_version, line_num) in pinned_deps {
            let matched_version = if options.full_precision {
                pinned_version.clone()
            } else {
                match_version_precision(&current_version, &pinned_version)
            };

            if matched_version != current_version {
                let updated = self.update_dependency(&dep_str, &matched_version);
                result
                    .pinned
                    .push((package, current_version, matched_version.clone(), line_num));
                updates.push((i, updated));
            } else {
                result.unchanged += 1;
            }
        }

        // Fetch versions for remaining deps in parallel
        let version_futures: Vec<_> = deps_to_check
            .iter()
            .map(
                |(_, _, package, current_version, full_constraint, _)| async {
                    if !is_stable_pep440(current_version) {
                        registry
                            .get_latest_version_including_prereleases(package)
                            .await
                    } else if Self::is_simple_constraint(full_constraint) {
                        registry.get_latest_version(package).await
                    } else {
                        registry
                            .get_latest_version_matching(package, full_constraint)
                            .await
                    }
                },
            )
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results and collect updates
        for ((i, dep_str, package, current_version, full_constraint, line_num), version_result) in
            deps_to_check.into_iter().zip(version_results)
        {
            // Skip upper-bound-only constraints (e.g., "<6", "<=5.0")
            // These define a ceiling, not a floor - updating them would only restrict versions
            if Self::is_upper_bound_only(&full_constraint) {
                result.unchanged += 1;
                continue;
            }

            match version_result {
                Ok(latest_version) => {
                    // When the current version is a pre-release, we fetched the latest
                    // pre-release. If the registry returned a stable version instead
                    // (no newer pre-release exists), refuse silent promotion to stable.
                    let current_is_prerelease = is_prerelease_pep440(&current_version);
                    if current_is_prerelease && !is_prerelease_pep440(&latest_version) {
                        result.unchanged += 1;
                        continue;
                    }

                    let constraints_for_cooldown = if full_constraint.is_empty() {
                        None
                    } else {
                        Some(full_constraint.as_str())
                    };
                    let (outcome, note) = crate::updater::apply_cooldown(
                        registry,
                        &package,
                        &current_version,
                        &latest_version,
                        constraints_for_cooldown,
                        current_is_prerelease,
                        options,
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
                        if compare_versions(&matched_version, &current_version, Lang::Python)
                            != std::cmp::Ordering::Greater
                        {
                            result.warnings.push(downgrade_warning(
                                &package,
                                &matched_version,
                                &current_version,
                            ));
                            result.unchanged += 1;
                        } else {
                            let updated = self.update_dependency(&dep_str, &matched_version);
                            result.updated.push((
                                package.clone(),
                                current_version.clone(),
                                matched_version.clone(),
                                line_num,
                            ));
                            if let Some((skipped_version, skipped_published_at)) = held_back_record
                            {
                                result.held_back.push((
                                    package,
                                    current_version,
                                    matched_version,
                                    skipped_version,
                                    skipped_published_at,
                                ));
                            }
                            updates.push((i, updated));
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

        // Apply updates, preserving decoration (comments, whitespace)
        for (i, updated) in updates {
            if let Some(item) = array.get_mut(i) {
                // Preserve the original decoration (prefix/suffix whitespace and comments)
                if let Value::String(formatted) = item {
                    let decor = formatted.decor().clone();
                    let mut new_formatted = Formatted::new(updated);
                    *new_formatted.decor_mut() = decor;
                    *formatted = new_formatted;
                } else {
                    *item = Value::from(updated);
                }
            }
        }
    }

    async fn update_poetry_deps(
        &self,
        deps_table: &mut toml_edit::Table,
        registry: &dyn Registry,
        result: &mut UpdateResult,
        line_index: &PyProjectLineIndex,
        section_path: &str,
        options: &UpdateOptions,
    ) {
        // First pass: collect dependencies and separate by config status
        let mut ignored_deps: Vec<(String, String, Option<usize>)> = Vec::new();
        let mut pinned_deps: Vec<(String, String, String, String, Option<usize>)> = Vec::new();
        let mut deps_to_check: Vec<(String, String, String, Option<usize>)> = Vec::new();

        for (key, item) in deps_table.iter() {
            if key == "python" {
                continue;
            }

            if let Item::Value(Value::String(s)) = item {
                let version_str = s.value().to_string();
                let (prefix, version) =
                    if version_str.starts_with('^') || version_str.starts_with('~') {
                        (version_str[..1].to_string(), version_str[1..].to_string())
                    } else {
                        (String::new(), version_str.clone())
                    };

                let package = key.to_string();
                let line_num = line_index.line_for(section_path, &package);

                if options.is_package_filtered_out(&package) {
                    result.unchanged += 1;
                    continue;
                }

                // Check if package should be ignored
                if options.should_ignore(&package) {
                    ignored_deps.push((package, version, line_num));
                    continue;
                }

                // Check if package has a pinned version
                if let Some(pinned_version) = options.get_pinned_version(&package) {
                    pinned_deps.push((
                        package,
                        prefix,
                        version,
                        pinned_version.to_string(),
                        line_num,
                    ));
                    continue;
                }

                deps_to_check.push((package, prefix, version, line_num));
            }
        }

        // Record ignored packages
        for (package, version, line_num) in ignored_deps {
            result.ignored.push((package, version, line_num));
        }

        // Process pinned packages (no registry fetch needed)
        for (key, prefix, version, pinned_version, line_num) in pinned_deps {
            let matched_version = if options.full_precision {
                pinned_version.clone()
            } else {
                match_version_precision(&version, &pinned_version)
            };

            if matched_version != version {
                let new_val = format!("{}{}", prefix, matched_version);
                result
                    .pinned
                    .push((key.clone(), version, matched_version.clone(), line_num));

                // Preserve decoration when updating
                if let Some(Item::Value(Value::String(formatted))) = deps_table.get_mut(&key) {
                    let decor = formatted.decor().clone();
                    let mut new_formatted = Formatted::new(new_val);
                    *new_formatted.decor_mut() = decor;
                    *formatted = new_formatted;
                }
            } else {
                result.unchanged += 1;
            }
        }

        // Fetch versions for remaining deps in parallel
        let version_futures: Vec<_> = deps_to_check
            .iter()
            .map(|(key, _, version, _)| async {
                if is_stable_pep440(version) {
                    registry.get_latest_version(key).await
                } else {
                    registry.get_latest_version_including_prereleases(key).await
                }
            })
            .collect();

        let version_results = join_all(version_futures).await;

        // Process results
        for ((key, prefix, version, line_num), version_result) in
            deps_to_check.into_iter().zip(version_results)
        {
            match version_result {
                Ok(latest_version) => {
                    // When the current version is a pre-release, we fetched the latest
                    // pre-release. If the registry returned a stable version instead
                    // (no newer pre-release exists), refuse silent promotion to stable.
                    let current_is_prerelease = is_prerelease_pep440(&version);
                    if current_is_prerelease && !is_prerelease_pep440(&latest_version) {
                        result.unchanged += 1;
                        continue;
                    }

                    let full_constraint = format!("{}{}", prefix, version);
                    let constraints_for_cooldown = if full_constraint.is_empty() {
                        None
                    } else {
                        Some(full_constraint.as_str())
                    };
                    let (outcome, note) = crate::updater::apply_cooldown(
                        registry,
                        &key,
                        &version,
                        &latest_version,
                        constraints_for_cooldown,
                        current_is_prerelease,
                        options,
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
                                key,
                                version,
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
                        match_version_precision(&version, &latest_version)
                    };
                    if matched_version != version {
                        // Refuse to write a downgrade.
                        if compare_versions(&matched_version, &version, Lang::Python)
                            != std::cmp::Ordering::Greater
                        {
                            result.warnings.push(downgrade_warning(
                                &key,
                                &matched_version,
                                &version,
                            ));
                            result.unchanged += 1;
                        } else {
                            let new_val = format!("{}{}", prefix, matched_version);
                            result.updated.push((
                                key.clone(),
                                version.clone(),
                                matched_version.clone(),
                                line_num,
                            ));
                            if let Some((skipped_version, skipped_published_at)) = held_back_record
                            {
                                result.held_back.push((
                                    key.clone(),
                                    version,
                                    matched_version.clone(),
                                    skipped_version,
                                    skipped_published_at,
                                ));
                            }

                            // Preserve decoration when updating
                            if let Some(Item::Value(Value::String(formatted))) =
                                deps_table.get_mut(&key)
                            {
                                let decor = formatted.decor().clone();
                                let mut new_formatted = Formatted::new(new_val);
                                *new_formatted.decor_mut() = decor;
                                *formatted = new_formatted;
                            }
                        }
                    } else {
                        result.unchanged += 1;
                    }
                }
                Err(e) => {
                    result.errors.push(format!("{}: {}", key, e));
                }
            }
        }
    }
}

impl PyProjectLineIndex {
    fn record_dependency_literals(
        lines_by_section: &mut HashMap<String, HashMap<String, usize>>,
        section_path: &str,
        line: &str,
        literal_re: &Regex,
        updater: &PyProjectUpdater,
        line_num: usize,
    ) {
        for caps in literal_re.captures_iter(line) {
            let dep = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();
            if let Some((package, _, _)) = updater.parse_dependency(dep) {
                lines_by_section
                    .entry(section_path.to_string())
                    .or_default()
                    .entry(package)
                    .or_insert(line_num);
            }
        }
    }

    fn count_structural_array_brackets(line: &str) -> ArrayBracketCounts {
        #[derive(Clone, Copy)]
        enum ScanState {
            Normal,
            BasicString,
            LiteralString,
        }

        let mut counts = ArrayBracketCounts::default();
        let mut state = ScanState::Normal;
        let mut chars = line.chars();

        while let Some(ch) = chars.next() {
            match state {
                ScanState::Normal => match ch {
                    '#' => break,
                    '"' => state = ScanState::BasicString,
                    '\'' => state = ScanState::LiteralString,
                    '[' => counts.opening += 1,
                    ']' => counts.closing += 1,
                    _ => {}
                },
                ScanState::BasicString => match ch {
                    '\\' => {
                        let _ = chars.next();
                    }
                    '"' => state = ScanState::Normal,
                    _ => {}
                },
                ScanState::LiteralString => {
                    if ch == '\'' {
                        state = ScanState::Normal;
                    }
                }
            }
        }

        counts
    }

    fn from_content(content: &str, updater: &PyProjectUpdater) -> Self {
        let section_re =
            Regex::new(r#"^\s*\[([^\]]+)\]\s*$"#).expect("Invalid pyproject section regex");
        let literal_re =
            Regex::new(r#""([^"]+)"|'([^']+)'"#).expect("Invalid dependency literal regex");
        let mut lines_by_section: HashMap<String, HashMap<String, usize>> = HashMap::new();
        let mut current_section: Option<String> = None;
        let mut current_array_section: Option<ArraySectionState> = None;

        for (line_idx, line) in content.lines().enumerate() {
            if let Some(caps) = section_re.captures(line) {
                current_section = Some(caps.get(1).unwrap().as_str().to_string());
                current_array_section = None;
                continue;
            }

            if let Some(array_state) = current_array_section.as_mut() {
                Self::record_dependency_literals(
                    &mut lines_by_section,
                    &array_state.section_path,
                    line,
                    &literal_re,
                    updater,
                    line_idx + 1,
                );

                let brackets = Self::count_structural_array_brackets(line);
                let next_depth = array_state.depth as isize + brackets.opening as isize
                    - brackets.closing as isize;
                if next_depth <= 0 {
                    current_array_section = None;
                } else {
                    array_state.depth = next_depth as usize;
                }

                continue;
            }

            let Some(section) = current_section.as_deref() else {
                continue;
            };

            match section {
                "project" => {
                    if let Some((key, value)) = PyProjectUpdater::assignment_parts(line)
                        && key == "dependencies"
                    {
                        let brackets = Self::count_structural_array_brackets(value);
                        if brackets.opening == 0 {
                            continue;
                        }

                        let section_path = "project.dependencies".to_string();
                        Self::record_dependency_literals(
                            &mut lines_by_section,
                            &section_path,
                            line,
                            &literal_re,
                            updater,
                            line_idx + 1,
                        );

                        let depth = brackets.opening.saturating_sub(brackets.closing);
                        if depth > 0 {
                            current_array_section = Some(ArraySectionState {
                                section_path,
                                depth,
                            });
                        }
                    }
                }
                "project.optional-dependencies" | "dependency-groups" => {
                    if let Some((group, value)) = PyProjectUpdater::assignment_parts(line) {
                        let brackets = Self::count_structural_array_brackets(value);
                        if brackets.opening == 0 {
                            continue;
                        }

                        let section_path = format!("{}.{}", section, group);
                        Self::record_dependency_literals(
                            &mut lines_by_section,
                            &section_path,
                            line,
                            &literal_re,
                            updater,
                            line_idx + 1,
                        );

                        let depth = brackets.opening.saturating_sub(brackets.closing);
                        if depth > 0 {
                            current_array_section = Some(ArraySectionState {
                                section_path,
                                depth,
                            });
                        }
                    }
                }
                "tool.poetry.dependencies" | "tool.poetry.dev-dependencies" => {
                    if let Some((key, value)) = PyProjectUpdater::assignment_parts(line)
                        && key != "python"
                        && (value.starts_with('"') || value.starts_with('\''))
                    {
                        lines_by_section
                            .entry(section.to_string())
                            .or_default()
                            .entry(key)
                            .or_insert(line_idx + 1);
                    }
                }
                _ => {}
            }
        }

        Self { lines_by_section }
    }

    fn line_for(&self, section_path: &str, package: &str) -> Option<usize> {
        self.lines_by_section
            .get(section_path)
            .and_then(|section_lines| section_lines.get(package).copied())
    }
}

impl Default for PyProjectUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for PyProjectUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut doc: DocumentMut = content.parse().map_err(|e: toml_edit::TomlError| {
            anyhow!(
                "Failed to parse {}:\n  {}",
                path.display(),
                e.to_string().replace('\n', "\n  ")
            )
        })?;

        let mut result = UpdateResult::default();
        let line_index = PyProjectLineIndex::from_content(&content, self);

        // Check for inline index configuration (Poetry/PDM/uv)
        // If found, use that registry instead of the default
        let inline_registry = Self::create_registry_from_config(&doc);
        let effective_registry: &dyn Registry = if let Some(ref inline) = inline_registry {
            inline.as_ref()
        } else {
            registry
        };

        // Update [project.dependencies]
        if let Some(Item::Table(project)) = doc.get_mut("project") {
            if let Some(Item::Value(Value::Array(deps))) = project.get_mut("dependencies") {
                self.update_array_deps(
                    deps,
                    effective_registry,
                    &mut result,
                    &line_index,
                    "project.dependencies",
                    &options,
                )
                .await;
            }

            // Update [project.optional-dependencies.*]
            if let Some(Item::Table(opt_deps)) = project.get_mut("optional-dependencies") {
                // Collect keys first
                let keys: Vec<String> = opt_deps.iter().map(|(k, _)| k.to_string()).collect();
                for key in keys {
                    if let Some(Item::Value(Value::Array(deps))) = opt_deps.get_mut(&key) {
                        let section_path = format!("project.optional-dependencies.{}", key);
                        self.update_array_deps(
                            deps,
                            effective_registry,
                            &mut result,
                            &line_index,
                            &section_path,
                            &options,
                        )
                        .await;
                    }
                }
            }
        }

        // Update [dependency-groups.*]
        if let Some(Item::Table(groups)) = doc.get_mut("dependency-groups") {
            let keys: Vec<String> = groups.iter().map(|(k, _)| k.to_string()).collect();
            for key in keys {
                if let Some(Item::Value(Value::Array(deps))) = groups.get_mut(&key) {
                    let section_path = format!("dependency-groups.{}", key);
                    self.update_array_deps(
                        deps,
                        effective_registry,
                        &mut result,
                        &line_index,
                        &section_path,
                        &options,
                    )
                    .await;
                }
            }
        }

        // Update [tool.poetry.dependencies] and [tool.poetry.dev-dependencies]
        if let Some(Item::Table(tool)) = doc.get_mut("tool")
            && let Some(Item::Table(poetry)) = tool.get_mut("poetry")
        {
            if let Some(Item::Table(deps)) = poetry.get_mut("dependencies") {
                self.update_poetry_deps(
                    deps,
                    effective_registry,
                    &mut result,
                    &line_index,
                    "tool.poetry.dependencies",
                    &options,
                )
                .await;
            }

            if let Some(Item::Table(deps)) = poetry.get_mut("dev-dependencies") {
                self.update_poetry_deps(
                    deps,
                    effective_registry,
                    &mut result,
                    &line_index,
                    "tool.poetry.dev-dependencies",
                    &options,
                )
                .await;
            }
        }

        if (!result.updated.is_empty() || !result.pinned.is_empty()) && !options.dry_run {
            write_file_atomic(path, &doc.to_string())?;
        }

        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        file_type == FileType::PyProject
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let doc: DocumentMut = content.parse().map_err(|e: toml_edit::TomlError| {
            anyhow!(
                "Failed to parse {}:\n  {}",
                path.display(),
                e.to_string().replace('\n', "\n  ")
            )
        })?;

        let mut deps = Vec::new();
        let line_index = PyProjectLineIndex::from_content(&content, self);

        // Parse [project.dependencies]
        if let Some(Item::Table(project)) = doc.get("project") {
            if let Some(Item::Value(Value::Array(arr))) = project.get("dependencies") {
                for item in arr.iter() {
                    if let Some(s) = item.as_str()
                        && let Some((name, version, constraint)) = self.parse_dependency(s)
                    {
                        let has_upper_bound = !Self::is_simple_constraint(&constraint);
                        let line_num = line_index.line_for("project.dependencies", &name);
                        deps.push(ParsedDependency {
                            name,
                            version,
                            line_number: line_num,
                            has_upper_bound,
                            is_bumpable: true,
                        });
                    }
                }
            }

            // Parse [project.optional-dependencies.*]
            if let Some(Item::Table(opt_deps)) = project.get("optional-dependencies") {
                for (group_name, group_deps) in opt_deps.iter() {
                    if let Some(arr) = group_deps.as_array() {
                        for item in arr.iter() {
                            if let Some(s) = item.as_str()
                                && let Some((name, version, constraint)) = self.parse_dependency(s)
                            {
                                let has_upper_bound = !Self::is_simple_constraint(&constraint);
                                let line_num = line_index.line_for(
                                    &format!("project.optional-dependencies.{}", group_name),
                                    &name,
                                );
                                deps.push(ParsedDependency {
                                    name,
                                    version,
                                    line_number: line_num,
                                    has_upper_bound,
                                    is_bumpable: true,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Parse [tool.poetry.dependencies] and [tool.poetry.dev-dependencies]
        if let Some(Item::Table(tool)) = doc.get("tool")
            && let Some(Item::Table(poetry)) = tool.get("poetry")
        {
            for section in ["dependencies", "dev-dependencies"] {
                if let Some(Item::Table(section_deps)) = poetry.get(section) {
                    for (key, item) in section_deps.iter() {
                        if key == "python" {
                            continue;
                        }
                        if let Item::Value(Value::String(s)) = item {
                            let version_str = s.value().to_string();
                            let version =
                                if version_str.starts_with('^') || version_str.starts_with('~') {
                                    version_str[1..].to_string()
                                } else {
                                    version_str
                                };
                            let line_num =
                                line_index.line_for(&format!("tool.poetry.{}", section), key);
                            deps.push(ParsedDependency {
                                name: key.to_string(),
                                version,
                                line_number: line_num,
                                has_upper_bound: false,
                                is_bumpable: true,
                            });
                        }
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
    use crate::registry::MockRegistry;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_dependency() {
        let updater = PyProjectUpdater::new();

        let (pkg, ver, constraint) = updater.parse_dependency("requests>=2.28.0").unwrap();
        assert_eq!(pkg, "requests");
        assert_eq!(ver, "2.28.0");
        assert_eq!(constraint, ">=2.28.0");

        let (pkg, ver, constraint) = updater
            .parse_dependency("uvicorn[standard]>=0.20.0")
            .unwrap();
        assert_eq!(pkg, "uvicorn");
        assert_eq!(ver, "0.20.0");
        assert_eq!(constraint, ">=0.20.0");

        // Test constraint with upper bound
        let (pkg, ver, constraint) = updater.parse_dependency("flask>=2.0.0,<3.0.0").unwrap();
        assert_eq!(pkg, "flask");
        assert_eq!(ver, "2.0.0");
        assert_eq!(constraint, ">=2.0.0,<3.0.0");
    }

    #[test]
    fn test_is_simple_constraint() {
        // Simple constraints - no upper bound, no exclusions
        assert!(PyProjectUpdater::is_simple_constraint("==1.0.0"));
        assert!(PyProjectUpdater::is_simple_constraint(">=1.0.0"));
        assert!(PyProjectUpdater::is_simple_constraint(">1.0.0"));

        // Multiple constraints with comma
        assert!(!PyProjectUpdater::is_simple_constraint(">=1.0.0,<2.0.0"));
        assert!(!PyProjectUpdater::is_simple_constraint(">=2.8.0,<9"));

        // Upper-bound operators (need constraint-aware lookup)
        assert!(!PyProjectUpdater::is_simple_constraint("<4.2"));
        assert!(!PyProjectUpdater::is_simple_constraint("<=2.0"));
        assert!(!PyProjectUpdater::is_simple_constraint("~=1.4"));

        // Exclusion operator
        assert!(!PyProjectUpdater::is_simple_constraint("!=1.5.0"));
    }

    #[test]
    fn test_is_upper_bound_only() {
        // Upper-bound-only constraints - should not be updated
        assert!(PyProjectUpdater::is_upper_bound_only("<6"));
        assert!(PyProjectUpdater::is_upper_bound_only("<4.2"));
        assert!(PyProjectUpdater::is_upper_bound_only("<=5.0"));
        assert!(PyProjectUpdater::is_upper_bound_only("<=2.0.0"));

        // NOT upper-bound-only (have lower bounds or are pinned)
        assert!(!PyProjectUpdater::is_upper_bound_only(">=1.0.0,<2.0.0")); // Has lower bound
        assert!(!PyProjectUpdater::is_upper_bound_only(">=2.8.0,<9")); // Has lower bound
        assert!(!PyProjectUpdater::is_upper_bound_only("==1.0.0")); // Pinned
        assert!(!PyProjectUpdater::is_upper_bound_only(">=1.0.0")); // Lower bound only
        assert!(!PyProjectUpdater::is_upper_bound_only(">1.0.0")); // Lower bound only
        assert!(!PyProjectUpdater::is_upper_bound_only("~=1.4")); // Compatible release
        assert!(!PyProjectUpdater::is_upper_bound_only("!=1.5.0")); // Exclusion
    }

    #[test]
    fn test_update_dependency() {
        let updater = PyProjectUpdater::new();

        assert_eq!(
            updater.update_dependency("requests>=2.28.0", "2.31.0"),
            "requests>=2.31.0"
        );

        assert_eq!(
            updater.update_dependency("uvicorn[standard]>=0.20.0", "0.24.0"),
            "uvicorn[standard]>=0.24.0"
        );

        // Constraint preservation - upper bound should be kept
        assert_eq!(
            updater.update_dependency("django>=4.0,<6", "5.2"),
            "django>=5.2,<6"
        );

        assert_eq!(
            updater.update_dependency("pytest>=2.8.0,<9", "8.0.0"),
            "pytest>=8.0.0,<9"
        );

        // Multiple constraints should all be preserved
        assert_eq!(
            updater.update_dependency("foo>=1.0.0,!=1.5.0,<2.0.0", "1.8.0"),
            "foo>=1.8.0,!=1.5.0,<2.0.0"
        );
    }

    #[test]
    fn test_count_structural_array_brackets_ignores_strings_and_comments() {
        assert_eq!(
            PyProjectLineIndex::count_structural_array_brackets(
                r#"[ "requests[socks]>=2.28.0", 'flask[async]>=2.0.0' ] # ]"#,
            ),
            ArrayBracketCounts {
                opening: 1,
                closing: 1,
            }
        );
        assert_eq!(
            PyProjectLineIndex::count_structural_array_brackets(
                r#"  "requests[socks]>=2.28.0", # ] inside a comment"#,
            ),
            ArrayBracketCounts::default()
        );
    }

    #[test]
    fn test_line_index_tracks_entries_after_extras_in_multiline_arrays() {
        let updater = PyProjectUpdater::new();
        let content = r#"[project]
name = "demo"
dependencies = [
  "requests[socks]>=2.28.0", # ] inside a comment should be ignored
  "flask>=2.0.0",
]

[project.optional-dependencies]
dev = [
  "pytest[testing]>=7.0.0",
  "black>=23.0.0", # [comment]
]
"#;

        let line_index = PyProjectLineIndex::from_content(content, &updater);

        assert_eq!(
            line_index.line_for("project.dependencies", "requests"),
            Some(4)
        );
        assert_eq!(
            line_index.line_for("project.dependencies", "flask"),
            Some(5)
        );
        assert_eq!(
            line_index.line_for("project.optional-dependencies.dev", "pytest"),
            Some(10)
        );
        assert_eq!(
            line_index.line_for("project.optional-dependencies.dev", "black"),
            Some(11)
        );
    }

    // Integration tests using MockRegistry and temp files

    #[tokio::test]
    async fn test_update_pyproject_pep621() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
version = "1.0.0"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=3.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_poetry() {
        // Poetry uses table format: key = "version"
        // The version can be ^2.28.0, >=2.0.0, 2.0.0, etc.
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.9"
requests = "2.28.0"
flask = "2.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // Poetry table-style dependencies with bare versions should be updated
        assert_eq!(result.updated.len(), 2);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("[tool.poetry.dependencies]"));
        // Both should be updated to new versions
        assert!(contents.contains("requests = \"2.31.0\""));
        assert!(contents.contains("flask = \"3.0.0\""));
    }

    #[tokio::test]
    async fn test_update_pyproject_dry_run() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.28.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified in dry-run mode
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.28.0"));
        assert!(!contents.contains("2.31.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_preserves_formatting() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"  # Project name
version = "1.0.0"

# Main dependencies
dependencies = [
    "requests>=2.28.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        let contents = std::fs::read_to_string(file.path()).unwrap();
        // toml_edit should preserve comments
        assert!(contents.contains("# Project name") || contents.contains("# Main dependencies"));
    }

    #[tokio::test]
    async fn test_update_pyproject_optional_dependencies() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.28.0"]

[project.optional-dependencies]
dev = ["pytest>=7.0.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("pytest", "8.0.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("pytest>=8.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_unchanged_packages() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.31.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 1);
    }

    // Error path tests

    #[tokio::test]
    async fn test_update_pyproject_invalid_toml() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project
name = "invalid toml - missing bracket"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater.update(file.path(), &registry, options).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Failed to parse"));
    }

    #[tokio::test]
    async fn test_update_pyproject_file_not_found() {
        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(
                Path::new("/nonexistent/path/pyproject.toml"),
                &registry,
                options,
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update_pyproject_registry_error_for_package() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "nonexistent-pkg>=1.0.0",
]
"#
        )
        .unwrap();

        // Registry only has requests - nonexistent-pkg will cause an error
        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // One package updated successfully
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "requests");

        // One error for the nonexistent package
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("nonexistent-pkg"));
    }

    #[tokio::test]
    async fn test_update_pyproject_empty_dependencies() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = []
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn test_update_pyproject_no_dependencies_section() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
version = "1.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI");

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.unchanged, 0);
        assert!(result.errors.is_empty());
    }

    // Tests for index URL extraction

    #[test]
    fn test_extract_poetry_source_urls() {
        let content = r#"
[tool.poetry]
name = "myproject"

[[tool.poetry.source]]
name = "private"
url = "https://private.pypi.com/simple"

[[tool.poetry.source]]
name = "extra"
url = "https://extra.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert_eq!(primary, Some("https://private.pypi.com/simple".to_string()));
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0], "https://extra.pypi.com/simple");
    }

    #[test]
    fn test_extract_pdm_source_urls() {
        let content = r#"
[tool.pdm]
name = "myproject"

[[tool.pdm.source]]
name = "private"
url = "https://private.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert_eq!(primary, Some("https://private.pypi.com/simple".to_string()));
        assert!(extras.is_empty());
    }

    #[test]
    fn test_extract_uv_index_urls() {
        let content = r#"
[tool.uv]

[[tool.uv.index]]
name = "pytorch"
url = "https://download.pytorch.org/whl/cpu"

[[tool.uv.index]]
name = "private"
url = "https://private.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert_eq!(
            primary,
            Some("https://download.pytorch.org/whl/cpu".to_string())
        );
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0], "https://private.pypi.com/simple");
    }

    #[test]
    fn test_extract_no_sources() {
        let content = r#"
[project]
name = "myproject"
dependencies = ["requests>=2.0.0"]
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert!(primary.is_none());
        assert!(extras.is_empty());
    }

    #[test]
    fn test_extract_combined_sources() {
        // Poetry and PDM sources in the same file (unlikely but should handle)
        let content = r#"
[[tool.poetry.source]]
name = "poetry-private"
url = "https://poetry.pypi.com/simple"

[[tool.pdm.source]]
name = "pdm-private"
url = "https://pdm.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        assert_eq!(primary, Some("https://poetry.pypi.com/simple".to_string()));
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0], "https://pdm.pypi.com/simple");
    }

    #[test]
    fn test_extract_skips_duplicate_urls() {
        let content = r#"
[[tool.poetry.source]]
name = "private1"
url = "https://private.pypi.com/simple"

[[tool.pdm.source]]
name = "private2"
url = "https://private.pypi.com/simple"
"#;
        let doc: DocumentMut = content.parse().unwrap();
        let (primary, extras) = PyProjectUpdater::extract_index_urls(&doc);

        // Should only have one unique URL
        assert_eq!(primary, Some("https://private.pypi.com/simple".to_string()));
        assert!(extras.is_empty());
    }

    // Tests for config-based ignore/pin functionality

    #[tokio::test]
    async fn test_update_pyproject_pep621_with_config_ignore() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
    "django>=4.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "5.0.0");

        // Create config that ignores flask
        let config = UpdConfig {
            ignore: vec!["flask".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 packages updated (requests, django), 1 ignored (flask)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "flask");
        assert_eq!(result.ignored[0].1, "2.0.0");

        // Verify file was updated only for non-ignored packages
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=2.0.0")); // unchanged
        assert!(contents.contains("django>=5.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_pep621_with_config_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        // Create config that pins flask to 2.3.0
        let mut pin = std::collections::HashMap::new();
        pin.insert("flask".to_string(), "2.3.0".to_string());
        let config = UpdConfig {
            ignore: Vec::new(),
            pin,
            cooldown: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 package updated from registry, 1 pinned
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "requests");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "flask");
        assert_eq!(result.pinned[0].1, "2.0.0"); // old
        assert_eq!(result.pinned[0].2, "2.3.0"); // new (pinned)

        // Verify file was updated with pinned version
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=2.3.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_poetry_with_config_ignore() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.28.0"
flask = "^2.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        // Create config that ignores requests
        let config = UpdConfig {
            ignore: vec!["requests".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 package updated (flask), 1 ignored (requests)
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "flask");
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "requests");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests = \"^2.28.0\"")); // unchanged
        assert!(contents.contains("flask = \"^3.0.0\""));
    }

    #[tokio::test]
    async fn test_update_pyproject_poetry_with_config_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"
version = "1.0.0"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.28.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI").with_version("requests", "2.31.0");

        // Create config that pins requests to 2.29.0
        let mut pin = std::collections::HashMap::new();
        pin.insert("requests".to_string(), "2.29.0".to_string());
        let config = UpdConfig {
            ignore: Vec::new(),
            pin,
            cooldown: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 1 pinned
        assert_eq!(result.updated.len(), 0);
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "requests");
        assert_eq!(result.pinned[0].2, "2.29.0");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests = \"^2.29.0\""));
    }

    #[tokio::test]
    async fn test_update_pyproject_duplicate_dependency_names_keep_occurrence_line_numbers() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "demo"
version = "0.1.0"
dependencies = [
  "requests>=2.28.0",
]

[project.optional-dependencies]
dev = [
  "requests>=2.27.0",
]
"#
        )
        .unwrap();

        let mut pin = std::collections::HashMap::new();
        pin.insert("requests".to_string(), "2.29.0".to_string());
        let config = UpdConfig {
            ignore: Vec::new(),
            pin,
            cooldown: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &MockRegistry::new("PyPI"), options)
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
        assert_eq!(line_numbers, vec![5, 10]);
    }

    #[tokio::test]
    async fn test_update_pyproject_multiline_arrays_with_extras_keep_following_line_numbers() {
        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "demo"
dependencies = [
  "requests[socks]>=2.28.0", # ] inside a comment should be ignored
  "flask>=2.0.0",
]

[project.optional-dependencies]
dev = [
  "pytest[testing]>=7.0.0",
  "black>=23.0.0", # [comment]
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("pytest", "8.0.0")
            .with_version("black", "24.0.0");

        let updater = PyProjectUpdater::new();
        let result = updater
            .update(file.path(), &registry, UpdateOptions::new(false, false))
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 4);

        let line_for = |package: &str| {
            result
                .updated
                .iter()
                .find(|(name, _, _, _)| name == package)
                .and_then(|(_, _, _, line_num)| *line_num)
        };

        assert_eq!(line_for("requests"), Some(4));
        assert_eq!(line_for("flask"), Some(5));
        assert_eq!(line_for("pytest"), Some(10));
        assert_eq!(line_for("black"), Some(11));

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests[socks]>=2.31.0"));
        assert!(contents.contains("flask>=3.0.0"));
        assert!(contents.contains("pytest[testing]>=8.0.0"));
        assert!(contents.contains("black>=24.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_with_config_ignore_and_pin() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = [
    "requests>=2.28.0",
    "flask>=2.0.0",
    "django>=4.0.0",
    "pytest>=7.0.0",
]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0")
            .with_version("django", "5.0.0")
            .with_version("pytest", "8.0.0");

        // Config: ignore flask, pin django to 4.2.0
        let mut pin = std::collections::HashMap::new();
        pin.insert("django".to_string(), "4.2.0".to_string());
        let config = UpdConfig {
            ignore: vec!["flask".to_string()],
            pin,
            cooldown: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 updated from registry (requests, pytest), 1 ignored (flask), 1 pinned (django)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.pinned.len(), 1);

        // Verify ignored
        assert_eq!(result.ignored[0].0, "flask");

        // Verify pinned
        assert_eq!(result.pinned[0].0, "django");
        assert_eq!(result.pinned[0].2, "4.2.0");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("flask>=2.0.0")); // unchanged (ignored)
        assert!(contents.contains("django>=4.2.0")); // pinned version
        assert!(contents.contains("pytest>=8.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_optional_deps_with_config() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[project]
name = "myproject"
dependencies = ["requests>=2.28.0"]

[project.optional-dependencies]
dev = ["pytest>=7.0.0", "black>=23.0.0"]
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("pytest", "8.0.0")
            .with_version("black", "24.0.0");

        // Config: ignore pytest
        let config = UpdConfig {
            ignore: vec!["pytest".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // 2 updated (requests, black), 1 ignored (pytest)
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "pytest");

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("requests>=2.31.0"));
        assert!(contents.contains("pytest>=7.0.0")); // unchanged
        assert!(contents.contains("black>=24.0.0"));
    }

    #[tokio::test]
    async fn test_update_pyproject_pin_preserves_prefix() {
        use crate::config::UpdConfig;

        let mut file = NamedTempFile::with_suffix(".toml").unwrap();
        write!(
            file,
            r#"[tool.poetry]
name = "myproject"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.28.0"
flask = "~2.0.0"
"#
        )
        .unwrap();

        let registry = MockRegistry::new("PyPI")
            .with_version("requests", "2.31.0")
            .with_version("flask", "3.0.0");

        // Pin both with different prefixes
        let mut pin = std::collections::HashMap::new();
        pin.insert("requests".to_string(), "2.30.0".to_string());
        pin.insert("flask".to_string(), "2.5.0".to_string());
        let config = UpdConfig {
            ignore: Vec::new(),
            pin,
            cooldown: None,
        };

        let updater = PyProjectUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.pinned.len(), 2);

        let contents = std::fs::read_to_string(file.path()).unwrap();
        // Prefixes should be preserved
        assert!(contents.contains("requests = \"^2.30.0\""));
        assert!(contents.contains("flask = \"~2.5.0\""));
    }
}
