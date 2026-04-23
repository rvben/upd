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
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub struct CsprojUpdater {
    /// Matches PackageReference with inline Version attribute (single line)
    /// Group 1: package name, Group 2: version
    pkg_ref_re: Regex,
    /// Matches PackageVersion with inline Version attribute (Directory.Packages.props)
    /// Group 1: package name, Group 2: version
    pkg_ver_re: Regex,
    /// Matches opening tag without inline Version (for multi-line)
    /// Group 1: tag name (PackageReference or PackageVersion), Group 2: package name
    open_tag_re: Regex,
    /// Matches <Version>X.Y.Z</Version> child element
    /// Group 1: version
    version_element_re: Regex,
}

/// Parsed NuGet package reference
struct ParsedPackage {
    name: String,
    version: String,
    /// Whether the version uses range constraint notation
    has_range_constraint: bool,
}

impl CsprojUpdater {
    pub fn new() -> Self {
        let pkg_ref_re =
            Regex::new(r#"<PackageReference\s+Include="([^"]+)"\s+Version="([^"]+)"\s*/?>"#)
                .expect("Invalid regex");

        let pkg_ver_re =
            Regex::new(r#"<PackageVersion\s+Include="([^"]+)"\s+Version="([^"]+)"\s*/?>"#)
                .expect("Invalid regex");

        let open_tag_re = Regex::new(
            r#"<(PackageReference|PackageVersion)\s+Include="([^"]+)"(?:\s+[^V][^>]*)?\s*>"#,
        )
        .expect("Invalid regex");

        let version_element_re =
            Regex::new(r#"<Version>([^<]+)</Version>"#).expect("Invalid regex");

        Self {
            pkg_ref_re,
            pkg_ver_re,
            open_tag_re,
            version_element_re,
        }
    }

    /// Parse a single line for inline PackageReference or PackageVersion
    fn parse_inline(&self, line: &str) -> Option<ParsedPackage> {
        // Try PackageReference first, then PackageVersion
        let caps = self
            .pkg_ref_re
            .captures(line)
            .or_else(|| self.pkg_ver_re.captures(line))?;

        let name = caps.get(1)?.as_str().to_string();
        let version = caps.get(2)?.as_str().to_string();
        let has_range_constraint =
            version.starts_with('[') || version.starts_with('(') || version.contains(',');

        Some(ParsedPackage {
            name,
            version,
            has_range_constraint,
        })
    }

    /// Check if a line is inside an XML comment
    fn is_in_comment(line: &str) -> bool {
        let trimmed = line.trim();
        trimmed.starts_with("<!--") || trimmed.contains("<!--")
    }
}

impl Default for CsprojUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Updater for CsprojUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let content = read_file_safe(path)?;
        let mut result = UpdateResult::default();

        let lines: Vec<&str> = content.lines().collect();

        // First pass: collect all packages (inline + multi-line)
        struct PackageInfo {
            name: String,
            version: String,
            line_idx: usize,
            has_range_constraint: bool,
        }

        let mut packages: Vec<PackageInfo> = Vec::new();
        let mut in_comment = false;
        let mut pending_tag: Option<(String, usize)> = None;

        for (line_idx, line) in lines.iter().enumerate() {
            // Track XML comments (simplified: single-line and block)
            if line.contains("<!--") {
                if !line.contains("-->") {
                    in_comment = true;
                }
                continue;
            }
            if in_comment {
                if line.contains("-->") {
                    in_comment = false;
                }
                continue;
            }

            if Self::is_in_comment(line) {
                continue;
            }

            // Check for multi-line Version element
            if let Some((ref tag_name, tag_line_idx)) = pending_tag {
                if let Some(caps) = self.version_element_re.captures(line) {
                    let version = caps.get(1).unwrap().as_str().to_string();
                    let has_range_constraint = version.starts_with('[')
                        || version.starts_with('(')
                        || version.contains(',');
                    packages.push(PackageInfo {
                        name: tag_name.clone(),
                        version,
                        line_idx: tag_line_idx,
                        has_range_constraint,
                    });
                    pending_tag = None;
                    continue;
                }
                // Check for closing tag (give up on this multi-line)
                let close_pattern = format!(
                    "</{}",
                    tag_name
                        .split_whitespace()
                        .next()
                        .unwrap_or("PackageReference")
                );
                if line.contains("</PackageReference>")
                    || line.contains("</PackageVersion>")
                    || line.contains(&close_pattern)
                {
                    pending_tag = None;
                }
                continue;
            }

            // Try inline parse first
            if let Some(parsed) = self.parse_inline(line) {
                packages.push(PackageInfo {
                    name: parsed.name,
                    version: parsed.version,
                    line_idx,
                    has_range_constraint: parsed.has_range_constraint,
                });
                continue;
            }

            // Check for opening tag without inline Version (multi-line start)
            if let Some(caps) = self.open_tag_re.captures(line)
                && !line.contains("Version=")
            {
                let name = caps.get(2).unwrap().as_str().to_string();
                pending_tag = Some((name, line_idx));
            }
        }

        // Separate into ignored, pinned, and to-be-fetched
        let mut ignored_packages: Vec<(usize, String, String)> = Vec::new();
        let mut pinned_packages: Vec<(usize, String, String, String)> = Vec::new();
        let mut fetch_deps: Vec<(usize, String, String, bool)> = Vec::new();

        for pkg in &packages {
            // Skip range constraints
            if pkg.has_range_constraint {
                continue;
            }

            if options.is_package_filtered_out(&pkg.name) {
                result.unchanged += 1;
                continue;
            }

            if options.should_ignore(&pkg.name) {
                ignored_packages.push((pkg.line_idx, pkg.name.clone(), pkg.version.clone()));
                continue;
            }

            if let Some(pinned_version) = options.get_pinned_version(&pkg.name) {
                pinned_packages.push((
                    pkg.line_idx,
                    pkg.name.clone(),
                    pkg.version.clone(),
                    pinned_version.to_string(),
                ));
                continue;
            }

            fetch_deps.push((
                pkg.line_idx,
                pkg.name.clone(),
                pkg.version.clone(),
                pkg.has_range_constraint,
            ));
        }

        for (line_idx, package, version) in ignored_packages {
            result.ignored.push((package, version, Some(line_idx + 1)));
        }

        // Deduplicate registry lookups
        let unique_packages: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            fetch_deps
                .iter()
                .filter_map(|(_, name, _, _)| {
                    if seen.insert(name.clone()) {
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };

        let version_futures: Vec<_> = unique_packages
            .iter()
            .map(|name| async move { registry.get_latest_version(name).await })
            .collect();

        let version_results = join_all(version_futures).await;

        let pkg_versions: HashMap<String, Result<String, String>> = unique_packages
            .into_iter()
            .zip(version_results)
            .map(|(name, result)| (name, result.map_err(|e| e.to_string())))
            .collect();

        // Build version map for each line
        let mut version_map: HashMap<usize, Result<String, anyhow::Error>> = HashMap::new();
        for (line_idx, name, _, _) in &fetch_deps {
            if let Some(result) = pkg_versions.get(name) {
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

        let mut pinned_lines = HashSet::new();
        for (line_idx, package, current_version, pinned_version) in pinned_packages {
            let matched_version = if options.full_precision {
                pinned_version.clone()
            } else {
                match_version_precision(&current_version, &pinned_version)
            };

            if matched_version != current_version {
                version_map.insert(line_idx, Ok(matched_version.clone()));
                pinned_lines.insert(line_idx);
                result.pinned.push((
                    package,
                    current_version,
                    matched_version,
                    Some(line_idx + 1),
                ));
            } else {
                result.unchanged += 1;
            }
        }

        // Apply updates
        let mut new_lines = Vec::new();
        let mut modified = false;
        // Pending multi-line update: (old_version, new_version) as owned strings
        let mut pending_update: Option<(String, String)> = None;

        // Build lookup from line_idx -> index in packages vec
        let pkg_by_line: HashMap<usize, usize> = packages
            .iter()
            .enumerate()
            .map(|(idx, p)| (p.line_idx, idx))
            .collect();

        for (line_idx, line) in lines.iter().enumerate() {
            let line_num = line_idx + 1;

            // Handle multi-line version element update
            if let Some((ref old_version, ref new_version)) = pending_update {
                if self.version_element_re.is_match(line) {
                    let updated = line.replacen(old_version.as_str(), new_version.as_str(), 1);
                    new_lines.push(updated);
                    modified = true;
                    pending_update = None;
                    continue;
                }
                // If we hit a closing tag without finding <Version>, give up
                if line.contains("</PackageReference>") || line.contains("</PackageVersion>") {
                    pending_update = None;
                }
            }

            if let Some(&pkg_idx) = pkg_by_line.get(&line_idx) {
                let pkg = &packages[pkg_idx];

                if pkg.has_range_constraint {
                    new_lines.push(line.to_string());
                    continue;
                }

                if let Some(version_result) = version_map.remove(&line_idx) {
                    match version_result {
                        Ok(latest_version) => {
                            // Apply cooldown policy before writing (registry path only; pins bypass it).
                            let (latest_version, held_back_record) = if pinned_lines
                                .contains(&line_idx)
                            {
                                (latest_version, None)
                            } else {
                                let (outcome, note) = crate::updater::apply_cooldown(
                                    registry,
                                    &pkg.name,
                                    &pkg.version,
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
                                            pkg.name.clone(),
                                            pkg.version.clone(),
                                            skipped_version,
                                            skipped_published_at,
                                        ));
                                        new_lines.push(line.to_string());
                                        continue;
                                    }
                                }
                            };

                            let matched_version = if options.full_precision {
                                latest_version.clone()
                            } else {
                                match_version_precision(&pkg.version, &latest_version)
                            };
                            if matched_version != pkg.version {
                                // Refuse to write a downgrade: matched_version must be strictly
                                // greater than the current version.
                                if compare_versions(&matched_version, &pkg.version, Lang::DotNet)
                                    != std::cmp::Ordering::Greater
                                {
                                    result.warnings.push(downgrade_warning(
                                        &pkg.name,
                                        &matched_version,
                                        &pkg.version,
                                    ));
                                    result.unchanged += 1;
                                    new_lines.push(line.to_string());
                                } else {
                                    if !pinned_lines.contains(&line_idx) {
                                        result.updated.push((
                                            pkg.name.clone(),
                                            pkg.version.clone(),
                                            matched_version.clone(),
                                            Some(line_num),
                                        ));
                                        if let Some((skipped_version, skipped_published_at)) =
                                            held_back_record
                                        {
                                            result.held_back.push((
                                                pkg.name.clone(),
                                                pkg.version.clone(),
                                                matched_version.clone(),
                                                skipped_version,
                                                skipped_published_at,
                                            ));
                                        }
                                    }

                                    if line.contains("Version=") {
                                        // Inline version attribute
                                        new_lines.push(line.replacen(
                                            &pkg.version,
                                            &matched_version,
                                            1,
                                        ));
                                        modified = true;
                                    } else {
                                        // Multi-line: version is on a subsequent <Version> line
                                        new_lines.push(line.to_string());
                                        pending_update =
                                            Some((pkg.version.clone(), matched_version));
                                    }
                                }
                            } else {
                                result.unchanged += 1;
                                new_lines.push(line.to_string());
                            }
                        }
                        Err(e) => {
                            result.errors.push(format!("{}: {}", pkg.name, e));
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
        file_type == FileType::Csproj
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let content = read_file_safe(path)?;
        let mut deps = Vec::new();
        let mut in_comment = false;
        let mut pending_tag: Option<(String, usize)> = None;

        for (line_idx, line) in content.lines().enumerate() {
            if line.contains("<!--") {
                if !line.contains("-->") {
                    in_comment = true;
                }
                continue;
            }
            if in_comment {
                if line.contains("-->") {
                    in_comment = false;
                }
                continue;
            }

            // Multi-line version element
            if let Some((ref tag_name, tag_line_idx)) = pending_tag {
                if let Some(caps) = self.version_element_re.captures(line) {
                    let version = caps.get(1).unwrap().as_str().to_string();
                    let has_range = version.starts_with('[')
                        || version.starts_with('(')
                        || version.contains(',');
                    deps.push(ParsedDependency {
                        name: tag_name.clone(),
                        version,
                        line_number: Some(tag_line_idx + 1),
                        has_upper_bound: has_range,
                        is_bumpable: true,
                    });
                    pending_tag = None;
                    continue;
                }
                if line.contains("</PackageReference>") || line.contains("</PackageVersion>") {
                    pending_tag = None;
                }
                continue;
            }

            if let Some(parsed) = self.parse_inline(line) {
                deps.push(ParsedDependency {
                    name: parsed.name,
                    version: parsed.version,
                    line_number: Some(line_idx + 1),
                    has_upper_bound: parsed.has_range_constraint,
                    is_bumpable: true,
                });
                continue;
            }

            if let Some(caps) = self.open_tag_re.captures(line)
                && !line.contains("Version=")
            {
                let name = caps.get(2).unwrap().as_str().to_string();
                pending_tag = Some((name, line_idx));
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
    fn test_parse_package_reference() {
        let updater = CsprojUpdater::new();

        let line = r#"    <PackageReference Include="Newtonsoft.Json" Version="13.0.3" />"#;
        let parsed = updater.parse_inline(line).unwrap();
        assert_eq!(parsed.name, "Newtonsoft.Json");
        assert_eq!(parsed.version, "13.0.3");
        assert!(!parsed.has_range_constraint);
    }

    #[test]
    fn test_parse_package_version() {
        let updater = CsprojUpdater::new();

        let line = r#"    <PackageVersion Include="Serilog" Version="3.1.1" />"#;
        let parsed = updater.parse_inline(line).unwrap();
        assert_eq!(parsed.name, "Serilog");
        assert_eq!(parsed.version, "3.1.1");
    }

    #[test]
    fn test_parse_multiline_version() {
        let updater = CsprojUpdater::new();

        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Serilog">
      <Version>3.1.1</Version>
    </PackageReference>
  </ItemGroup>
</Project>
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();

        let deps = updater.parse_dependencies(file.path()).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "Serilog");
        assert_eq!(deps[0].version, "3.1.1");
    }

    #[test]
    fn test_skips_comments() {
        let updater = CsprojUpdater::new();

        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <!-- <PackageReference Include="Old.Package" Version="1.0.0" /> -->
    <PackageReference Include="Active.Package" Version="2.0.0" />
  </ItemGroup>
</Project>
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();

        let deps = updater.parse_dependencies(file.path()).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "Active.Package");
    }

    #[test]
    fn test_skips_range_constraints() {
        let updater = CsprojUpdater::new();

        let line = r#"    <PackageReference Include="Constrained.Pkg" Version="[1.0.0, 2.0.0)" />"#;
        let parsed = updater.parse_inline(line).unwrap();
        assert!(parsed.has_range_constraint);
    }

    #[tokio::test]
    async fn test_update_csproj() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="13.0.1" />
    <PackageReference Include="Serilog" Version="3.1.0" />
  </ItemGroup>
</Project>
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();

        let registry = MockRegistry::new("nuget")
            .with_version("Newtonsoft.Json", "13.0.3")
            .with_version("Serilog", "4.0.0");

        let updater = CsprojUpdater::new();
        let options = UpdateOptions::new(false, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 2);
        assert!(result.errors.is_empty());

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("Version=\"13.0.3\""));
        assert!(contents.contains("Version=\"4.0.0\""));
    }

    #[tokio::test]
    async fn test_dry_run() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="13.0.1" />
  </ItemGroup>
</Project>
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();

        let registry = MockRegistry::new("nuget").with_version("Newtonsoft.Json", "13.0.3");

        let updater = CsprojUpdater::new();
        let options = UpdateOptions::new(true, false);

        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.updated.len(), 1);

        // File should NOT be modified
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(contents.contains("Version=\"13.0.1\""));
    }

    #[tokio::test]
    async fn test_config_ignore_and_pin() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="13.0.1" />
    <PackageReference Include="Serilog" Version="3.1.0" />
    <PackageReference Include="xunit" Version="2.6.1" />
  </ItemGroup>
</Project>
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();

        let registry = MockRegistry::new("nuget")
            .with_version("Newtonsoft.Json", "13.0.3")
            .with_version("Serilog", "4.0.0")
            .with_version("xunit", "2.7.0");

        let mut pins = std::collections::HashMap::new();
        pins.insert("Serilog".to_string(), "3.2.0".to_string());
        let config = UpdConfig {
            ignore: vec!["Newtonsoft.Json".to_string()],
            pin: pins,
            cooldown: None,
        };

        let updater = CsprojUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.ignored.len(), 1);
        assert_eq!(result.ignored[0].0, "Newtonsoft.Json");
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "Serilog");
    }

    #[tokio::test]
    async fn test_config_pin_multiline_reports_only_pinned_change() {
        use crate::config::UpdConfig;
        use std::sync::Arc;

        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Serilog">
      <Version>3.1.0</Version>
    </PackageReference>
  </ItemGroup>
</Project>
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();

        let registry = MockRegistry::new("nuget");

        let mut pins = std::collections::HashMap::new();
        pins.insert("Serilog".to_string(), "3.2.0".to_string());
        let config = UpdConfig {
            ignore: vec![],
            pin: pins,
            cooldown: None,
        };

        let updater = CsprojUpdater::new();
        let options = UpdateOptions::new(false, false).with_config(Arc::new(config));
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.updated.is_empty());
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.pinned[0].0, "Serilog");
        assert_eq!(result.pinned[0].1, "3.1.0");
        assert_eq!(result.pinned[0].2, "3.2.0");
        assert_eq!(result.pinned[0].3, Some(3));

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents
                .contains("<PackageReference Include=\"Serilog\">\n      <Version>3.2.0</Version>")
        );
    }

    #[test]
    fn test_handles() {
        let updater = CsprojUpdater::new();
        assert!(updater.handles(FileType::Csproj));
        assert!(!updater.handles(FileType::Requirements));
    }

    #[tokio::test]
    async fn test_registry_error() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="NonExistent.Package" Version="1.0.0" />
  </ItemGroup>
</Project>
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();

        let registry = MockRegistry::new("nuget");
        let updater = CsprojUpdater::new();
        let options = UpdateOptions::new(true, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("NonExistent.Package"));
    }

    /// When the registry returns a version *lower* than the current (e.g., the NuGet trigger
    /// case where Microsoft.AspNetCore.App was absorbed into the shared framework after 2.x),
    /// the updater must leave the file unchanged and emit a warning.
    #[tokio::test]
    async fn test_no_downgrade_inline() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageVersion Include="Microsoft.AspNetCore.App" Version="6.0.0" />
  </ItemGroup>
</Project>
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();

        // Registry returns an older version — the real trigger case.
        let registry = MockRegistry::new("nuget").with_version("Microsoft.AspNetCore.App", "2.2.8");

        let updater = CsprojUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        // No updates should have been applied.
        assert!(
            result.updated.is_empty(),
            "downgrade must not be written: {:?}",
            result.updated
        );
        assert_eq!(
            result.unchanged, 1,
            "refused downgrade must count as unchanged"
        );

        // A warning must be present mentioning the package and both versions.
        assert_eq!(
            result.warnings.len(),
            1,
            "expected exactly one downgrade warning"
        );
        assert!(
            result.warnings[0].contains("Microsoft.AspNetCore.App"),
            "warning must name the package"
        );
        assert!(
            result.warnings[0].contains("2.2.8"),
            "warning must include the rejected latest version"
        );
        assert!(
            result.warnings[0].contains("6.0.0"),
            "warning must include the current version"
        );

        // File must be byte-for-byte unchanged.
        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("Version=\"6.0.0\""),
            "file must not be modified"
        );
    }

    /// Multi-line <Version> element variant of the same no-downgrade guard.
    #[tokio::test]
    async fn test_no_downgrade_multiline() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Some.Package">
      <Version>5.0.0</Version>
    </PackageReference>
  </ItemGroup>
</Project>
"#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", content).unwrap();

        let registry = MockRegistry::new("nuget").with_version("Some.Package", "3.0.0");

        let updater = CsprojUpdater::new();
        let options = UpdateOptions::new(false, false);
        let result = updater
            .update(file.path(), &registry, options)
            .await
            .unwrap();

        assert!(result.updated.is_empty(), "downgrade must not be written");
        assert_eq!(
            result.unchanged, 1,
            "refused downgrade must count as unchanged"
        );
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("Some.Package"));

        let contents = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            contents.contains("<Version>5.0.0</Version>"),
            "file must not be modified"
        );
    }
}
