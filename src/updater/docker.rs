use super::{
    CooldownOutcome, FileType, ParsedDependency, SkipStatus, SkippedUpdate, UpdateOptions,
    UpdateResult, Updater, apply_cooldown, downgrade_warning, read_file_safe, write_file_atomic,
};
use crate::registry::{DockerRegistry, Registry};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DockerDependency {
    image: String,
    tag: String,
    line_idx: usize,
    tag_range: Range<usize>,
    digest_pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageReference {
    image: String,
    tag: String,
    tag_range: Range<usize>,
    digest_pinned: bool,
}

pub struct DockerUpdater {
    from_re: Regex,
    compose_image_re: Regex,
}

impl DockerUpdater {
    pub fn new() -> Self {
        Self {
            from_re: Regex::new(r"(?i)^\s*FROM\s+(?:--platform=\S+\s+)?(\S+)")
                .expect("valid Dockerfile FROM regex"),
            compose_image_re: Regex::new(r"^(\s*)image\s*:\s*(.*?)\s*(?:#.*)?$")
                .expect("valid Compose image regex"),
        }
    }

    fn parse_reference(value: &str) -> Option<ImageReference> {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("scratch") {
            return None;
        }
        let quote_len = usize::from(
            (trimmed.starts_with('"') && trimmed.ends_with('"'))
                || (trimmed.starts_with('\'') && trimmed.ends_with('\'')),
        );
        let unquoted = if quote_len == 1 {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        };

        // Compose commonly wraps the default reference in ${NAME:-...}. The
        // default is the dependency declaration; a runtime override remains a
        // runtime concern and is deliberately untouched.
        let (reference, wrapper_offset) = if unquoted.starts_with("${")
            && unquoted.ends_with('}')
            && let Some(default_at) = unquoted.find(":-")
        {
            (
                &unquoted[default_at + 2..unquoted.len() - 1],
                default_at + 2,
            )
        } else {
            (unquoted, 0)
        };

        // A variable inside a Dockerfile FROM expression needs its ARG
        // declaration updated, not the FROM line. That composed form is
        // reported separately rather than partly rewritten here.
        if reference.contains('$') || reference.chars().any(char::is_whitespace) {
            return None;
        }

        let (without_digest, digest_pinned) = match reference.rsplit_once('@') {
            Some((base, digest)) if digest.starts_with("sha256:") => (base, true),
            _ => (reference, false),
        };
        let last_slash = without_digest.rfind('/');
        let colon = without_digest.rfind(':')?;
        if last_slash.is_some_and(|slash| colon < slash) {
            return None;
        }
        let image = &without_digest[..colon];
        let tag = &without_digest[colon + 1..];
        if image.is_empty() || tag.is_empty() {
            return None;
        }
        let leading_ws = value.len() - value.trim_start().len();
        let tag_start = leading_ws + quote_len + wrapper_offset + colon + 1;
        Some(ImageReference {
            image: image.to_string(),
            tag: tag.to_string(),
            tag_range: tag_start..tag_start + tag.len(),
            digest_pinned,
        })
    }

    fn parse_dockerfile(&self, content: &str) -> (Vec<DockerDependency>, Vec<String>) {
        let mut dependencies = Vec::new();
        let mut warnings = Vec::new();
        for (line_idx, line) in content.lines().enumerate() {
            let Some(captures) = self.from_re.captures(line) else {
                continue;
            };
            let Some(reference_match) = captures.get(1) else {
                continue;
            };
            let value = reference_match.as_str();
            if value.contains('$') {
                warnings.push(format!(
                    "line {}: variable-based FROM references are not rewritten; pin the complete image reference directly",
                    line_idx + 1
                ));
                continue;
            }
            let Some(reference) = Self::parse_reference(value) else {
                continue;
            };
            dependencies.push(DockerDependency {
                image: reference.image,
                tag: reference.tag,
                line_idx,
                tag_range: (reference_match.start() + reference.tag_range.start)
                    ..(reference_match.start() + reference.tag_range.end),
                digest_pinned: reference.digest_pinned,
            });
        }
        (dependencies, warnings)
    }

    fn indentation(line: &str) -> usize {
        line.chars().take_while(|ch| ch.is_whitespace()).count()
    }

    fn parse_compose(&self, content: &str) -> (Vec<DockerDependency>, Vec<String>) {
        let mut dependencies = Vec::new();
        let mut warnings = Vec::new();
        let mut services_indent = None;
        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = Self::indentation(line);
            if trimmed == "services:" {
                services_indent = Some(indent);
                continue;
            }
            let Some(root_indent) = services_indent else {
                continue;
            };
            if indent <= root_indent {
                services_indent = None;
                continue;
            }
            let Some(captures) = self.compose_image_re.captures(line) else {
                continue;
            };
            let Some(value_match) = captures.get(2) else {
                continue;
            };
            let Some(reference) = Self::parse_reference(value_match.as_str()) else {
                if value_match.as_str().contains('$') && !value_match.as_str().contains(":-") {
                    warnings.push(format!(
                        "line {}: runtime-only Compose image variables have no default version to update",
                        line_idx + 1
                    ));
                }
                continue;
            };
            dependencies.push(DockerDependency {
                image: reference.image,
                tag: reference.tag,
                line_idx,
                tag_range: (value_match.start() + reference.tag_range.start)
                    ..(value_match.start() + reference.tag_range.end),
                digest_pinned: reference.digest_pinned,
            });
        }
        (dependencies, warnings)
    }

    fn parse(&self, content: &str, file_type: FileType) -> (Vec<DockerDependency>, Vec<String>) {
        match file_type {
            FileType::Dockerfile => self.parse_dockerfile(content),
            FileType::DockerCompose => self.parse_compose(content),
            _ => (Vec::new(), Vec::new()),
        }
    }

    fn file_type(path: &Path) -> Result<FileType> {
        match FileType::detect(path) {
            Some(file_type @ (FileType::Dockerfile | FileType::DockerCompose)) => Ok(file_type),
            _ => Err(anyhow!(
                "'{}' is not a Dockerfile or Compose file",
                path.display()
            )),
        }
    }

    fn numeric_segments(tag: &str) -> Option<Vec<u64>> {
        let tag = tag.strip_prefix('v').unwrap_or(tag);
        let core: String = tag
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
            .collect();
        if core.is_empty() || core.ends_with('.') {
            return None;
        }
        core.split('.')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .ok()
    }

    fn is_newer(current: &str, candidate: &str) -> bool {
        match (
            Self::numeric_segments(current),
            Self::numeric_segments(candidate),
        ) {
            (Some(current), Some(candidate)) => candidate > current,
            _ => false,
        }
    }

    fn replace_dependencies(
        content: &str,
        dependencies: &[DockerDependency],
        replacements: &[(usize, String)],
    ) -> String {
        let replacement_map: std::collections::HashMap<usize, &str> = replacements
            .iter()
            .map(|(idx, value)| (*idx, value.as_str()))
            .collect();
        let newline = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let had_trailing_newline = content.ends_with(newline);
        let body = if had_trailing_newline {
            &content[..content.len() - newline.len()]
        } else {
            content
        };
        let mut lines: Vec<String> = body.split(newline).map(str::to_string).collect();
        for (dependency_idx, replacement) in replacement_map {
            let dependency = &dependencies[dependency_idx];
            if let Some(line) = lines.get_mut(dependency.line_idx) {
                line.replace_range(dependency.tag_range.clone(), replacement);
            }
        }
        let mut output = lines.join(newline);
        if had_trailing_newline {
            output.push_str(newline);
        }
        output
    }

    /// Apply one already-approved update. Used by interactive mode so its
    /// writes obey exactly the same parser and location guards as batch mode.
    pub fn apply_approved_update(
        &self,
        content: &str,
        file_type: FileType,
        package: &str,
        current: &str,
        target: &str,
        line_number: Option<usize>,
    ) -> Option<String> {
        let (dependencies, _) = self.parse(content, file_type);
        let index = dependencies.iter().position(|dependency| {
            dependency.image == package
                && dependency.tag == current
                && line_number.is_none_or(|line| dependency.line_idx + 1 == line)
        })?;
        Some(Self::replace_dependencies(
            content,
            &dependencies,
            &[(index, target.to_string())],
        ))
    }
}

impl Default for DockerUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Updater for DockerUpdater {
    async fn update(
        &self,
        path: &Path,
        registry: &dyn Registry,
        options: UpdateOptions,
    ) -> Result<UpdateResult> {
        let file_type = Self::file_type(path)?;
        let content = read_file_safe(path)?;
        let (dependencies, warnings) = self.parse(&content, file_type);
        let mut result = UpdateResult {
            warnings,
            ..UpdateResult::default()
        };
        let mut replacements: Vec<(usize, String)> = Vec::new();
        let mut seen = HashSet::new();

        for (dependency_idx, dependency) in dependencies.iter().enumerate() {
            let occurrence = (dependency.image.clone(), dependency.tag.clone());
            if !seen.insert(occurrence) {
                // Every occurrence is still written independently if an update
                // is found, but one duplicate must not issue a second registry
                // request or inflate the update report.
                let replacement = replacements
                    .iter()
                    .find(|(idx, _)| {
                        dependencies[*idx].image == dependency.image
                            && dependencies[*idx].tag == dependency.tag
                    })
                    .map(|(_, replacement)| replacement.clone());
                if let Some(replacement) = replacement {
                    replacements.push((dependency_idx, replacement));
                }
                continue;
            }
            let line_number = Some(dependency.line_idx + 1);
            if options.is_package_filtered_out(&dependency.image) {
                result.unchanged += 1;
                continue;
            }
            if options.should_ignore(&dependency.image) {
                result.ignored.push((
                    dependency.image.clone(),
                    dependency.tag.clone(),
                    line_number,
                ));
                continue;
            }
            if dependency.digest_pinned {
                result.skipped.push(SkippedUpdate {
                    package: dependency.image.clone(),
                    current: dependency.tag.clone(),
                    status: SkipStatus::Blocked,
                    reason: "digest-pin",
                    message: "tag-and-digest references require verified digest resolution"
                        .to_string(),
                    line_number,
                });
                continue;
            }
            if Self::numeric_segments(&dependency.tag).is_none() {
                result.skipped.push(SkippedUpdate {
                    package: dependency.image.clone(),
                    current: dependency.tag.clone(),
                    status: SkipStatus::NotExamined,
                    reason: "floating-tag",
                    message: "floating or non-numeric tags do not define a safe update channel"
                        .to_string(),
                    line_number,
                });
                continue;
            }
            if let Some(pinned) = options.get_pinned_version(&dependency.image) {
                if pinned == dependency.tag {
                    result.unchanged += 1;
                } else {
                    replacements.push((dependency_idx, pinned.to_string()));
                    result.pinned.push((
                        dependency.image.clone(),
                        dependency.tag.clone(),
                        pinned.to_string(),
                        line_number,
                    ));
                }
                continue;
            }

            let lookup = DockerRegistry::lookup_key(&dependency.image, &dependency.tag);
            let latest = match registry
                .get_latest_version_matching(&lookup, &dependency.tag)
                .await
            {
                Ok(latest) => latest,
                Err(error) => {
                    result
                        .errors
                        .push(format!("{}: {}", dependency.image, error));
                    continue;
                }
            };
            if !Self::is_newer(&dependency.tag, &latest) {
                if latest != dependency.tag {
                    result.warnings.push(downgrade_warning(
                        &dependency.image,
                        &latest,
                        &dependency.tag,
                    ));
                }
                result.unchanged += 1;
                continue;
            }

            let (outcome, note) = apply_cooldown(
                registry,
                &lookup,
                &dependency.tag,
                &latest,
                Some(&dependency.tag),
                false,
                &options,
            )
            .await;
            if let Some(note) = note {
                options.note_cooldown_unavailable(&note);
            }
            let (selected, held_back) = match outcome {
                CooldownOutcome::Unchanged(version) => (version, None),
                CooldownOutcome::HeldBack {
                    chosen,
                    skipped_version,
                    skipped_published_at,
                } => (chosen, Some((skipped_version, skipped_published_at))),
                CooldownOutcome::Skipped {
                    skipped_version,
                    skipped_published_at,
                } => {
                    result.skipped_by_cooldown.push((
                        dependency.image.clone(),
                        dependency.tag.clone(),
                        skipped_version,
                        skipped_published_at,
                    ));
                    continue;
                }
            };
            if !options.allows_bump(&dependency.tag, &selected) {
                result.record_capped(&dependency.image, &dependency.tag, &selected, line_number);
                continue;
            }
            replacements.push((dependency_idx, selected.clone()));
            result.updated.push((
                dependency.image.clone(),
                dependency.tag.clone(),
                selected.clone(),
                line_number,
            ));
            if let Some((skipped, published_at)) = held_back {
                result.held_back.push((
                    dependency.image.clone(),
                    dependency.tag.clone(),
                    selected,
                    skipped,
                    published_at,
                ));
            }
        }

        if !options.dry_run && !replacements.is_empty() {
            let updated = Self::replace_dependencies(&content, &dependencies, &replacements);
            write_file_atomic(path, &updated)?;
        }
        Ok(result)
    }

    fn handles(&self, file_type: FileType) -> bool {
        matches!(file_type, FileType::Dockerfile | FileType::DockerCompose)
    }

    fn parse_dependencies(&self, path: &Path) -> Result<Vec<ParsedDependency>> {
        let file_type = Self::file_type(path)?;
        let content = read_file_safe(path)?;
        let (dependencies, _) = self.parse(&content, file_type);
        Ok(dependencies
            .into_iter()
            .map(|dependency| {
                let is_bumpable =
                    !dependency.digest_pinned && Self::numeric_segments(&dependency.tag).is_some();
                ParsedDependency {
                    name: dependency.image,
                    version: dependency.tag,
                    line_number: Some(dependency.line_idx + 1),
                    has_upper_bound: false,
                    is_bumpable,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::mock::MockRegistry;

    #[test]
    fn parses_multistage_dockerfiles_and_preserves_registry_ports() {
        let updater = DockerUpdater::new();
        let content = "FROM --platform=$BUILDPLATFORM rust:1.90-alpine AS builder\nFROM registry.example.com:5000/team/app:2.4.1\nFROM scratch\n";
        let (dependencies, warnings) = updater.parse_dockerfile(content);
        assert!(warnings.is_empty());
        assert_eq!(dependencies.len(), 2);
        assert_eq!(dependencies[0].image, "rust");
        assert_eq!(dependencies[0].tag, "1.90-alpine");
        assert_eq!(dependencies[1].image, "registry.example.com:5000/team/app");
        assert_eq!(dependencies[1].tag, "2.4.1");
    }

    #[test]
    fn parses_compose_images_only_below_services_and_supports_variable_defaults() {
        let updater = DockerUpdater::new();
        let content = "name: demo\nservices:\n  api:\n    image: ${API_IMAGE:-ghcr.io/acme/api:1.2.3}\n  db:\n    image: 'postgres:17.4' # pinned\nvolumes:\n  image: ignored:1.0\n";
        let (dependencies, warnings) = updater.parse_compose(content);
        assert!(warnings.is_empty());
        assert_eq!(dependencies.len(), 2);
        assert_eq!(dependencies[0].image, "ghcr.io/acme/api");
        assert_eq!(dependencies[0].tag, "1.2.3");
        assert_eq!(dependencies[1].image, "postgres");
        assert_eq!(dependencies[1].tag, "17.4");
    }

    #[test]
    fn approved_rewrite_changes_only_the_version_token() {
        let updater = DockerUpdater::new();
        let content = "services:\n  api:\n    image: ${API_IMAGE:-ghcr.io/acme/api:1.2.3} # keep\n";
        let updated = updater
            .apply_approved_update(
                content,
                FileType::DockerCompose,
                "ghcr.io/acme/api",
                "1.2.3",
                "1.3.0",
                Some(3),
            )
            .unwrap();
        assert_eq!(
            updated,
            "services:\n  api:\n    image: ${API_IMAGE:-ghcr.io/acme/api:1.3.0} # keep\n"
        );
    }

    #[tokio::test]
    async fn update_obeys_bump_ceiling_and_writes_compatible_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Dockerfile");
        std::fs::write(&path, "FROM rust:1.90-alpine\nFROM alpine:3.22\n").unwrap();
        let registry = MockRegistry::new("docker")
            .with_version(
                &DockerRegistry::lookup_key("rust", "1.90-alpine"),
                "1.98-alpine",
            )
            .with_version(&DockerRegistry::lookup_key("alpine", "3.22"), "4.0");
        let options = UpdateOptions::new(false, false).with_bump_filter(super::super::BumpFilter {
            major: false,
            minor: true,
            patch: true,
        });
        let result = DockerUpdater::new()
            .update(&path, &registry, options)
            .await
            .unwrap();
        assert_eq!(result.updated[0].0, "rust");
        assert_eq!(result.capped[0].package, "alpine");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "FROM rust:1.98-alpine\nFROM alpine:3.22\n"
        );
    }

    #[test]
    fn digest_pins_and_variable_froms_are_reported_not_rewritten() {
        let updater = DockerUpdater::new();
        let (dependencies, warnings) = updater.parse_dockerfile(
            "ARG BASE=alpine:3.22\nFROM $BASE\nFROM alpine:3.22@sha256:deadbeef\n",
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(dependencies.len(), 1);
        assert!(dependencies[0].digest_pinned);
    }

    #[test]
    fn approved_rewrite_preserves_crlf() {
        let updater = DockerUpdater::new();
        let content = "FROM alpine:3.22\r\nRUN true\r\n";
        let updated = updater
            .apply_approved_update(
                content,
                FileType::Dockerfile,
                "alpine",
                "3.22",
                "3.23",
                Some(1),
            )
            .unwrap();
        assert_eq!(updated, "FROM alpine:3.23\r\nRUN true\r\n");
    }
}
