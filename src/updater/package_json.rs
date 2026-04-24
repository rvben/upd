use super::npm_range::{SpecShape, classify, lower_bound_anchor, rewrite_lower_bound};
use super::{
    FileType, ParsedDependency, UpdateOptions, UpdateResult, Updater, downgrade_warning,
    read_file_safe, write_file_atomic,
};
use crate::align::compare_versions;
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

                        // Classify the spec once so both the pinned branch and the
                        // comparator branch below can use the same shape without
                        // re-parsing the string.
                        let spec_shape = classify(version_str);

                        if let Some(pinned_version) = options.get_pinned_version(package) {
                            match spec_shape {
                                SpecShape::SingleComparator | SpecShape::TwoComparatorRange => {
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
                                        result.warnings.push(format!(
                                            "cannot pin range spec '{version_str}' for '{package}': no lower bound to rewrite"
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

                        // Route comparator-style specs through the range module.
                        // These fail semver::Version::parse on the extracted token,
                        // so they must be classified before the validity check below.
                        if semver::Version::parse(&current_version).is_err() {
                            match spec_shape {
                                SpecShape::SingleComparator | SpecShape::TwoComparatorRange => {
                                    match registry
                                        .get_latest_version_matching(package, version_str)
                                        .await
                                    {
                                        Ok(matched) => {
                                            // Apply cooldown using the lower-bound anchor as
                                            // the current-version proxy and the original spec
                                            // as the constraint so selection stays in-range.
                                            // held_back_info carries skipped info if cooldown
                                            // chose an older version; it is pushed to
                                            // result.held_back only after the update is confirmed.
                                            let (effective_version, held_back_info) =
                                                if let Some(anchor) =
                                                    lower_bound_anchor(version_str)
                                                {
                                                    let anchor_is_pre =
                                                        is_prerelease_semver(anchor);
                                                    let (outcome, note) =
                                                        crate::updater::apply_cooldown(
                                                            registry,
                                                            package,
                                                            anchor,
                                                            &matched,
                                                            Some(version_str),
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
                                                if let Some(new_spec) =
                                                    rewrite_lower_bound(version_str, &effective)
                                                {
                                                    if new_spec != version_str {
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
                                                    result.warnings.push(format!(
                                                        "skipping range spec '{version_str}' for '{package}': no lower bound to bump"
                                                    ));
                                                }
                                            }
                                            // If effective_version is None the cooldown Skipped
                                            // branch already pushed to skipped_by_cooldown.
                                        }
                                        Err(e) => {
                                            result.warnings.push(format!("{package}: {e}"));
                                        }
                                    }
                                    continue;
                                }
                                SpecShape::Unsupported => {
                                    result.warnings.push(format!(
                                        "skipping unrecognised version spec '{version_str}' for '{package}'"
                                    ));
                                    continue;
                                }
                                SpecShape::ExactPin | SpecShape::CaretOrTilde => {
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
            .map(|(_, package, _, _, current_version)| async {
                if is_prerelease_semver(current_version) {
                    registry
                        .get_latest_version_including_prereleases(package)
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
            ignore: Vec::new(),
            pin,
            cooldown: None,
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
            ignore: Vec::new(),
            pin,
            cooldown: None,
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
            ignore: vec!["lodash".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
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
            ignore: Vec::new(),
            pin,
            cooldown: None,
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
            ignore: vec!["lodash".to_string()],
            pin,
            cooldown: None,
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
            ignore: vec!["typescript".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
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
            ignore: Vec::new(),
            pin,
            cooldown: None,
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
            ignore: vec!["ranged".to_string()],
            pin: std::collections::HashMap::new(),
            cooldown: None,
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
            ignore: Vec::new(),
            pin,
            cooldown: None,
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
}
