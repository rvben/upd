//! Linked release checksums. Resolve a complete group before changing any line.
use super::*;
use crate::annotation::{ChecksumAnnotation, expand_asset_template, valid_variable};
use anyhow::{Context, ensure};

/// Exact edits and their provenance, retained for interactive approval.
#[derive(Debug, Clone)]
pub struct ChecksumUpdate {
    pub package: String,
    pub current: String,
    pub target: String,
    pub version_line: usize,
    pub line: usize,
    pub original: String,
    pub replacement: String,
    pub asset: String,
    pub tag: String,
    pub source: String,
    pub current_checksum: String,
    pub latest_checksum: String,
    /// An interactive write must still refer to the file that was previewed.
    pub preview: Arc<str>,
}

/// One literal ARG/ENV assignment; the range excludes quotes and whitespace.
pub(crate) fn assignment(raw: &str) -> Option<(&str, Range<usize>)> {
    let trimmed = raw.trim_start();
    let (instruction, rest) = trimmed.split_once(char::is_whitespace)?;
    if !instruction.eq_ignore_ascii_case("ARG") && !instruction.eq_ignore_ascii_case("ENV") {
        return None;
    }
    let (name, value) = rest.trim().split_once('=')?;
    if !valid_variable(name) {
        return None;
    }
    let value = value.trim();
    let value = if value.starts_with('"') || value.starts_with('\'') {
        value
            .strip_prefix(value.chars().next()?)?
            .strip_suffix(value.chars().next()?)?
    } else {
        value
    };
    if value.is_empty()
        || value.contains(char::is_whitespace)
        || value.contains(['$', '\\', '`', '"', '\''])
    {
        return None;
    }
    let start = raw.find('=')? + 1;
    let offset = raw[start..].find(value)? + start;
    Some((name, offset..offset + value.len()))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Recognize a complete SHA-256 token, never a substring of a longer value or
/// annotation prose. Keep its byte range so punctuation and quoting survive.
fn digest_span(raw: &str, code_end: usize) -> Result<Range<usize>> {
    let code = &raw[..code_end];
    let mut spans = Vec::new();
    let mut start = None;
    for (idx, byte) in code.bytes().chain(std::iter::once(b' ')).enumerate() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'_' | b'.' | b'-' | b'+' | b'/' | b'\\')
            || byte >= 128
        {
            start.get_or_insert(idx);
        } else if let Some(begin) = start.take()
            && valid_digest(&code[begin..idx])
        {
            spans.push(begin..idx);
        }
    }
    ensure!(
        spans.len() == 1,
        "checksum line must contain exactly one SHA-256 token before its comment; found {}",
        spans.len()
    );
    Ok(spans.remove(0))
}

/// Infer only an obvious assignment key. This is text matching, not language
/// evaluation; `id=` is the escape hatch for expressions and repeated keys.
fn assignment_key(raw: &str) -> Option<&str> {
    static KEY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"^\s*(?:(?:export|readonly|const|let|var|val|final)\s+)?(?:([A-Za-z_][A-Za-z_0-9]*)|"([A-Za-z_][A-Za-z_0-9]*)"|'([A-Za-z_][A-Za-z_0-9]*)')\s*(?:[?:+]?=|:)"#).unwrap()
    });
    let captures = KEY.captures(raw)?;
    (1..=3).find_map(|idx| captures.get(idx).map(|value| value.as_str()))
}

/// Match exactly one GNU or BSD checksum entry. A bare digest is permitted only
/// for an explicitly named sidecar `<asset>.sha256`.
fn manifest_digest(bytes: &[u8], asset: &str, manifest: &str) -> Result<String> {
    let text = std::str::from_utf8(bytes).context("checksum manifest is not UTF-8")?;
    let mut matches = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if valid_digest(line) && manifest == format!("{asset}.sha256") {
            matches.push(line);
            continue;
        }
        if let Some((name, digest)) = line
            .strip_prefix("SHA256 (")
            .and_then(|s| s.split_once(") = "))
        {
            if name.strip_prefix("./").unwrap_or(name) == asset {
                ensure!(valid_digest(digest), "invalid SHA-256 for {asset}");
                matches.push(digest);
            }
            continue;
        }
        if let Some((digest, name)) = line.split_once(char::is_whitespace) {
            let name = name
                .trim_start()
                .strip_prefix('*')
                .unwrap_or(name.trim_start());
            if name.strip_prefix("./").unwrap_or(name) == asset {
                ensure!(valid_digest(digest), "invalid SHA-256 for {asset}");
                matches.push(digest);
            }
        }
    }
    ensure!(
        matches.len() == 1,
        "expected exactly one checksum entry for {asset} in {manifest}, found {}",
        matches.len()
    );
    Ok(matches[0].to_ascii_lowercase())
}

/// Published SHA-256 and the metadata source that established it.
#[derive(Debug, Clone)]
pub struct ReleaseChecksum {
    pub sha256: String,
    pub source: String,
}

/// Resolve one exact asset without downloading its binary. A manifest is used
/// only when explicitly selected, and must agree with any published asset digest.
pub async fn resolve_release_checksum(
    registry: &dyn Registry,
    package: &str,
    tag: &str,
    asset: &str,
    manifest: Option<&str>,
) -> Result<ReleaseChecksum> {
    resolve_release_checksum_cached(registry, package, tag, asset, manifest, &mut HashMap::new())
        .await
}

async fn resolve_release_checksum_cached(
    registry: &dyn Registry,
    package: &str,
    tag: &str,
    asset: &str,
    manifest: Option<&str>,
    manifests: &mut HashMap<String, Vec<u8>>,
) -> Result<ReleaseChecksum> {
    ensure!(
        manifest.is_none_or(|manifest| manifest != asset),
        "checksum manifest must be a different release asset from the binary being pinned"
    );
    let (digest, source) = if let Some(manifest) = manifest {
        let metadata = registry
            .release_asset_metadata(package, tag, asset)
            .await
            .with_context(|| format!("cannot verify release archive {asset}"))?;
        if !manifests.contains_key(manifest) {
            let bytes = registry
                .release_asset(package, tag, manifest)
                .await
                .with_context(|| {
                    format!("cannot read checksum asset {manifest} for {package}@{tag}")
                })?;
            manifests.insert(manifest.to_string(), bytes);
        }
        let digest = manifest_digest(&manifests[manifest], asset, manifest)?;
        if let Some(published) = metadata.sha256 {
            ensure!(
                published.eq_ignore_ascii_case(&digest),
                "conflicting SHA-256 for {asset}: checksum manifest {manifest} disagrees with GitHub asset digest"
            );
        }
        (digest, manifest.to_string())
    } else {
        (
            registry
                .release_asset_digest(package, tag, asset)
                .await
                .with_context(|| format!("cannot resolve SHA-256 for {asset}"))?,
            "github-asset-digest".into(),
        )
    };
    ensure!(valid_digest(&digest), "invalid SHA-256 for {asset}");
    Ok(ReleaseChecksum {
        sha256: digest.to_ascii_lowercase(),
        source,
    })
}

struct Declaration<'a> {
    line: usize,
    stage: usize,
    name: &'a str,
    import: bool,
}

/// Resolve visible declarations using the same instruction boundaries as the
/// annotation parser. Global ARGs require consumption in a stage; child stages
/// inherit declarations from the stage named by FROM.
fn visible_declaration(
    declarations: &[Declaration<'_>],
    parents: &[Option<usize>],
    stage: usize,
    name: &str,
    before: usize,
) -> Result<usize> {
    let local: Vec<_> = declarations
        .iter()
        .filter(|d| d.stage == stage && d.name == name)
        .collect();
    ensure!(
        local.len() <= 1,
        "variable {name} has multiple declarations in the same Docker stage"
    );
    if let Some(declaration) = local.first() {
        ensure!(
            declaration.line < before,
            "variable {name} must be declared before its checksum"
        );
        if !declaration.import {
            return Ok(declaration.line);
        }
        if let Some(parent) = parents[stage] {
            // An inherited declaration takes precedence over a global default.
            let mut ancestor = Some(parent);
            let mut has_inherited = false;
            while let Some(scope) = ancestor {
                has_inherited |= declarations
                    .iter()
                    .any(|d| d.name == name && d.stage == scope);
                ancestor = parents[scope];
            }
            if has_inherited {
                return visible_declaration(declarations, parents, parent, name, usize::MAX);
            }
        }
        ensure!(stage != 0, "global ARG {name} has no literal default");
        return visible_declaration(declarations, parents, 0, name, usize::MAX);
    }
    if let Some(parent) = parents[stage] {
        return visible_declaration(declarations, parents, parent, name, usize::MAX);
    }
    anyhow::bail!(
        "version variable {name} is not visible in this Docker stage; consume a global ARG with ARG {name}"
    )
}

pub(super) struct Groups {
    linked: HashMap<usize, Vec<Link>>,
    pub errors: HashMap<usize, String>,
    pub warnings: Vec<String>,
}

struct Link {
    line: usize,
    annotation: ChecksumAnnotation,
    span: Range<usize>,
}

impl Groups {
    pub fn scan(content: &str, scan: &AnnotatedScan, docker: bool) -> Self {
        let lines: Vec<_> = content.lines().collect();
        let mut groups = Self {
            linked: HashMap::new(),
            errors: HashMap::new(),
            warnings: Vec::new(),
        };
        if !docker {
            groups.scan_generic(&lines, scan);
            groups.protect_malformed(scan);
            return groups;
        }
        let analysis = super::super::docker::analyze_dockerfile(content);
        let declarations: Vec<_> = analysis
            .variables
            .iter()
            .map(|variable| Declaration {
                line: variable.line,
                stage: variable.stage,
                name: variable.name.as_str(),
                import: variable.import,
            })
            .collect();
        for (idx, annotation) in &scan.checksums {
            let explicit: Vec<_> = scan
                .lines
                .iter()
                .filter(|line| line.id.as_deref() == Some(&annotation.variable))
                .collect();
            let binding = (|| -> Result<usize> {
                ensure!(
                    explicit.len() <= 1,
                    "duplicate annotation id {}",
                    annotation.variable
                );
                let name = if let Some(parent) = explicit.first() {
                    assignment(lines[parent.line_idx])
                        .context("version id must name a literal ARG/ENV assignment")?
                        .0
                } else {
                    &annotation.variable
                };
                let bound = visible_declaration(
                    &declarations,
                    &analysis.parents,
                    analysis.stages[*idx],
                    name,
                    *idx,
                )?;
                ensure!(
                    explicit
                        .first()
                        .is_none_or(|parent| parent.line_idx == bound),
                    "annotation id {} is not visible in this Docker stage",
                    annotation.variable
                );
                Ok(bound)
            })();
            let parent = binding
                .as_ref()
                .ok()
                .and_then(|parent| scan.lines.iter().find(|line| line.line_idx == *parent));
            let problem = if let Err(error) = &binding {
                Some(error.to_string())
            } else if parent.is_none_or(|line| line.source != AnnotationSource::GitHubReleases) {
                Some(format!(
                    "{} must reference an annotated github-releases version",
                    annotation.variable
                ))
            } else if assignment(lines[*idx]).is_none_or(|(name, span)| {
                !valid_digest(&lines[*idx][span])
                    || declarations
                        .iter()
                        .filter(|d| d.stage == analysis.stages[*idx] && d.name == name)
                        .count()
                        != 1
            }) {
                Some("checksum must be a unique literal ARG/ENV assignment in its stage containing 64 hexadecimal digits".to_string())
            } else {
                None
            };
            if let Some(problem) = problem {
                groups.warnings.push(format!("line {}: {problem}", idx + 1));
                let mut scopes = HashSet::from([0, analysis.stages[*idx]]);
                let mut ancestor = analysis.parents[analysis.stages[*idx]];
                while let Some(scope) = ancestor {
                    scopes.insert(scope);
                    ancestor = analysis.parents[scope];
                }
                let unknown_variable = !declarations.iter().any(|d| d.name == annotation.variable);
                for line in &scan.lines {
                    let protects = explicit
                        .iter()
                        .any(|parent| parent.line_idx == line.line_idx)
                        || match &binding {
                            Ok(parent) => *parent == line.line_idx,
                            Err(_) if unknown_variable => {
                                line.source == AnnotationSource::GitHubReleases
                            }
                            Err(_) => declarations.iter().any(|d| {
                                d.name == annotation.variable
                                    && d.line == line.line_idx
                                    && scopes.contains(&d.stage)
                            }),
                        };
                    if protects {
                        groups.errors.insert(line.line_idx, problem.clone());
                    }
                }
            } else if let Some(parent) = parent {
                groups
                    .linked
                    .entry(parent.line_idx)
                    .or_default()
                    .push(Link {
                        line: *idx,
                        annotation: annotation.clone(),
                        span: assignment(lines[*idx]).unwrap().1,
                    });
            }
        }
        groups.protect_malformed(scan);
        groups
    }

    fn scan_generic(&mut self, lines: &[&str], scan: &AnnotatedScan) {
        let mut keys: HashMap<&str, Vec<usize>> = HashMap::new();
        for (idx, raw) in lines.iter().enumerate() {
            if let Some(key) = assignment_key(raw) {
                keys.entry(key).or_default().push(idx);
            }
        }
        let mut ids: HashMap<&str, Vec<&AnnotatedLine>> = HashMap::new();
        let mut annotated = HashMap::new();
        for line in &scan.lines {
            annotated.insert(line.line_idx, line);
            if let Some(id) = &line.id {
                ids.entry(id).or_default().push(line);
            }
        }
        for (idx, annotation) in &scan.checksums {
            let explicit = ids
                .get(annotation.variable.as_str())
                .cloned()
                .unwrap_or_default();
            let inferred = keys
                .get(annotation.variable.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            let candidates: Vec<_> = if explicit.is_empty() {
                inferred
                    .iter()
                    .filter_map(|idx| annotated.get(idx).copied())
                    .collect()
            } else {
                explicit.clone()
            };
            let resolved = (|| -> Result<(&AnnotatedLine, Range<usize>)> {
                ensure!(
                    candidates.len() == 1 && (!explicit.is_empty() || inferred.len() == 1),
                    "checksum reference {} must name one unique annotated version; use id=<name> for repeated keys or arbitrary lines",
                    annotation.variable
                );
                let parent = candidates[0];
                ensure!(
                    parent.source == AnnotationSource::GitHubReleases,
                    "{} must reference an annotated github-releases version",
                    annotation.variable
                );
                Ok((parent, digest_span(lines[*idx], annotation.comment_start)?))
            })();
            match resolved {
                Ok((parent, span)) => {
                    self.linked.entry(parent.line_idx).or_default().push(Link {
                        line: *idx,
                        annotation: annotation.clone(),
                        span,
                    });
                }
                Err(error) => {
                    let problem = error.to_string();
                    self.warnings.push(format!("line {}: {problem}", idx + 1));
                    for line in &scan.lines {
                        if candidates
                            .iter()
                            .any(|parent| parent.line_idx == line.line_idx)
                            || (candidates.is_empty()
                                && line.source == AnnotationSource::GitHubReleases)
                        {
                            self.errors.insert(line.line_idx, problem.clone());
                        }
                    }
                }
            }
        }
    }

    fn protect_malformed(&mut self, scan: &AnnotatedScan) {
        // A malformed directive may have lost its variable name. Fail closed
        // for GitHub version pins rather than guessing which pin it protects.
        if scan.checksum_refused {
            for line in &scan.lines {
                if line.source == AnnotationSource::GitHubReleases {
                    self.errors.insert(line.line_idx, "invalid checksum directive in file; fix the annotation before updating release versions".into());
                }
            }
        }
    }

    pub fn linked(&self, line: usize) -> bool {
        self.linked.contains_key(&line)
    }

    pub async fn resolve(
        &self,
        content: &str,
        line: &AnnotatedLine,
        target: &str,
        tag: &str,
        registry: &dyn Registry,
    ) -> Result<Vec<ChecksumUpdate>> {
        let mut edits = Vec::new();
        let lines: Vec<_> = content.lines().collect();
        let mut manifests: HashMap<String, Vec<u8>> = HashMap::new();
        let preview: Arc<str> = Arc::from(content);
        for Link {
            line: idx,
            annotation,
            span,
        } in self.linked.get(&line.line_idx).into_iter().flatten()
        {
            let asset = expand_asset_template(&annotation.asset, target, tag)?;
            let manifest = annotation
                .checksums
                .as_deref()
                .map(|template| expand_asset_template(template, target, tag))
                .transpose()?;
            let resolved = resolve_release_checksum_cached(
                registry,
                &line.package,
                tag,
                &asset,
                manifest.as_deref(),
                &mut manifests,
            )
            .await
            .with_context(|| format!("line {}: checksum resolution failed", idx + 1))?;
            let source = resolved.source;
            let current_checksum = lines[*idx][span.clone()].to_string();
            let latest_checksum = resolved.sha256;
            let replacement = if current_checksum.eq_ignore_ascii_case(&latest_checksum) {
                lines[*idx].to_string()
            } else {
                rewrite_spans(lines[*idx], std::slice::from_ref(span), &latest_checksum)
            };
            edits.push(ChecksumUpdate {
                package: line.package.clone(),
                current: line.version.clone(),
                target: target.to_string(),
                version_line: line.line_idx + 1,
                line: idx + 1,
                original: lines[*idx].to_string(),
                replacement,
                asset,
                tag: tag.to_string(),
                source,
                current_checksum,
                latest_checksum,
                preview: Arc::clone(&preview),
            });
        }
        Ok(edits)
    }
}

/// Validate a selected interactive plan against the current file using
/// exactly the same relationships as the batch updater. This also catches links
/// added after a preview that originally contained only a version assignment.
pub fn validate_checksum_plan(
    content: &str,
    file_type: FileType,
    selected: &[(&str, &str, &str, Option<usize>)],
    planned: &[ChecksumUpdate],
) -> Result<()> {
    if selected.is_empty() {
        return Ok(());
    }
    let docker = crate::updater::DockerUpdater::new();
    let is_docker = file_type == FileType::Dockerfile;
    let scan = scan_annotated(content, is_docker.then_some(&docker as &dyn OwnsLines));
    let groups = Groups::scan(content, &scan, is_docker);
    for &(package, current, target, line_number) in selected {
        let idx = line_number
            .and_then(|n| n.checked_sub(1))
            .context("annotated version has no line number")?;
        if let Some(error) = groups.errors.get(&idx) {
            anyhow::bail!("Checksum relationship changed or is invalid: {error}; rerun upd");
        }
        if let Some(links) = groups.linked.get(&idx) {
            for Link {
                line: checksum_idx, ..
            } in links
            {
                let edit = planned
                    .iter()
                    .find(|edit| {
                        edit.package == package
                            && edit.current == current
                            && edit.target == target
                            && edit.version_line == idx + 1
                            && edit.line == checksum_idx + 1
                    })
                    .with_context(|| {
                        format!(
                            "Missing resolved checksum for {package} on line {}; rerun upd",
                            checksum_idx + 1
                        )
                    })?;
                ensure!(
                    edit.preview.as_ref() == content,
                    "Checksum-linked file changed since preview; rerun upd"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MockRegistry;
    use crate::updater::DockerUpdater;

    fn fixture() -> String {
        format!(
            "FROM alpine:3.22\r\n# upd: github-releases jdx/mise\r\nARG MISE_VERSION=2025.12.9\r\n\r\n# upd: checksum MISE_VERSION asset=mise-v{{version}}-linux-x64.tar.gz\r\nENV MISE_CHECKSUM=\"{}\"",
            "a".repeat(64)
        )
    }

    fn registry() -> MockRegistry {
        MockRegistry::new("github-releases")
            .with_version("jdx/mise", "v2026.9.1")
            .with_release_digest(
                "jdx/mise",
                "v2026.9.1",
                "mise-v2026.9.1-linux-x64.tar.gz",
                &"b".repeat(64),
            )
    }

    async fn run_generic(
        content: &str,
        name: &str,
        registry: MockRegistry,
        dry_run: bool,
    ) -> (UpdateResult, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        let updater = AnnotatedUpdater::new(RegistrySet::with_single(
            AnnotationSource::GitHubReleases,
            Arc::new(registry),
        ));
        let workflow = crate::updater::GithubActionsUpdater::new();
        let owner = (FileType::detect(&path) == Some(FileType::GithubActions))
            .then_some(&workflow as &dyn OwnsLines);
        let result = updater
            .run(&path, UpdateOptions::new(dry_run, false), owner)
            .await
            .unwrap();
        (result, std::fs::read_to_string(path).unwrap())
    }

    #[tokio::test]
    async fn generic_checksums_update_exact_spans_in_common_annotation_formats() {
        for (name, version, checksum, comment) in [
            (
                "versions.sh",
                "export MISE_VERSION='2025.12.9'",
                "readonly SUM='{hash}'",
                "#",
            ),
            (
                "Makefile",
                "MISE_VERSION ?= 2025.12.9",
                "SUM := {hash}",
                "#",
            ),
            (
                "pins.toml",
                "MISE_VERSION = \"2025.12.9\"",
                "sum = \"{hash}\"",
                "#",
            ),
            (
                "pins.yaml",
                "  MISE_VERSION: '2025.12.9'",
                "  sum: sha256:{hash}",
                "#",
            ),
            (
                "versions.js",
                "const MISE_VERSION = \"2025.12.9\";",
                "const sum = \"{hash}\";",
                "//",
            ),
            (
                ".github/workflows/check.yml",
                "  MISE_VERSION: '2025.12.9'",
                "  sum: '{hash}'",
                "#",
            ),
        ] {
            let original = format!(
                "{version} {comment} upd: github-releases jdx/mise\r\n{} {comment} upd: checksum MISE_VERSION asset=mise-{{tag}}-linux-x64.tar.gz",
                checksum.replace("{hash}", &"a".repeat(64))
            );
            let (preview, unchanged) = run_generic(&original, name, registry(), true).await;
            assert!(
                preview.skipped.is_empty() && preview.warnings.is_empty(),
                "{name}: {preview:?}"
            );
            assert_eq!(unchanged, original);
            assert_eq!(preview.updated.len(), 1);
            assert_eq!(preview.checksum_updates.len(), 1);
            let (applied, written) = run_generic(&original, name, registry(), false).await;
            assert!(applied.skipped.is_empty(), "{name}: {applied:?}");
            assert_eq!(
                written,
                original
                    .replace("2025.12.9", "2026.9.1")
                    .replace(&"a".repeat(64), &"b".repeat(64)),
                "{name}"
            );
            let (repeat, identical) = run_generic(&written, name, registry(), false).await;
            assert!(
                repeat.updated.is_empty() && repeat.skipped.is_empty(),
                "{name}: {repeat:?}"
            );
            assert_eq!(identical, written);
        }
    }

    #[tokio::test]
    async fn explicit_ids_link_arbitrary_lines_and_allow_repeated_keys() {
        let original = format!(
            "fetch(\"https://example.com/download\", \"2025.12.9\"); // upd: github-releases jdx/mise id=mise\nverify(\"sha256:{}\"); // upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz\n[unrelated]\nversion = '1.0.0'\n",
            "a".repeat(64)
        );
        let (result, written) = run_generic(&original, "fetch.js", registry(), false).await;
        assert!(
            result.skipped.is_empty() && result.warnings.is_empty(),
            "{result:?}"
        );
        assert_eq!(
            written,
            original
                .replace("2025.12.9", "2026.9.1")
                .replace(&"a".repeat(64), &"b".repeat(64))
        );

        let original = format!(
            "[one]\nversion = '2025.12.9' # upd: github-releases jdx/mise id=mise\nsha = '{}' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz\n[two]\nversion = '1.0.0'\n",
            "a".repeat(64)
        );
        let (result, written) = run_generic(&original, "pins.toml", registry(), false).await;
        assert_eq!(result.updated.len(), 1, "{result:?}");
        assert!(written.contains("version = '1.0.0'"));
    }

    #[tokio::test]
    async fn standalone_generic_links_can_appear_before_their_version_and_preserve_comments() {
        let old = "a".repeat(64);
        let original = format!(
            "# upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz\nsum = '{old}' # keep {old}\n# upd: github-releases jdx/mise id=mise\nversion = '2025.12.9' # keep 2025.12.9"
        );
        let (result, written) = run_generic(&original, "pins.toml", registry(), false).await;
        assert!(
            result.skipped.is_empty() && result.warnings.is_empty(),
            "{result:?}"
        );
        assert_eq!(result.checksum_updates[0].line, 2);
        assert_eq!(result.checksum_updates[0].version_line, 4);
        assert_eq!(
            written,
            format!(
                "# upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz\nsum = '{}' # keep {old}\n# upd: github-releases jdx/mise id=mise\nversion = '2026.9.1' # keep 2025.12.9",
                "b".repeat(64)
            )
        );
    }

    #[tokio::test]
    async fn ambiguous_or_malformed_generic_links_never_partially_update() {
        let hash = "a".repeat(64);
        for (version, checksum) in [
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise\nversion = '1.0.0'",
                format!(
                    "sha = '{hash}' # upd: checksum version asset=mise-{{tag}}-linux-x64.tar.gz"
                ),
            ),
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise id=mise\nother = '2025.12.9' # upd: github-releases jdx/mise id=mise",
                format!("sha = '{hash}' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz"),
            ),
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise id=mise",
                format!(
                    "sha = '{hash}', '{hash}' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz"
                ),
            ),
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise id=mise",
                format!("sha = 'x{hash}' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz"),
            ),
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise id=mise",
                format!(
                    "sha = '{hash}-extra' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz"
                ),
            ),
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise id=mise",
                format!(
                    "sha = '/tmp/{hash}' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz"
                ),
            ),
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise id=mise",
                format!(
                    "sha = '{hash}.sha256' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz"
                ),
            ),
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise id=mise",
                format!(
                    "sha = 'bad' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz #{hash}"
                ),
            ),
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise id=mise",
                format!("sha = '{hash}' # upd: checksum typo asset=mise-{{tag}}-linux-x64.tar.gz"),
            ),
            (
                "version = '2025.12.9' # upd: github-releases jdx/mise id=mise",
                format!(
                    "sha = '{hash}' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz # upd: github-releases jdx/mise"
                ),
            ),
        ] {
            let original = format!("{version}\n{checksum}\n");
            let (result, written) = run_generic(&original, "pins.toml", registry(), false).await;
            assert_eq!(written, original, "{result:?}");
            assert!(
                result.updated.is_empty() && !result.skipped.is_empty(),
                "{result:?}"
            );
            assert!(
                result
                    .skipped
                    .iter()
                    .all(|skip| skip.reason == "checksum-invalid")
            );
        }
    }

    #[tokio::test]
    async fn one_unavailable_generic_architecture_blocks_the_complete_group() {
        let original = format!(
            "v = '2025.12.9' # upd: github-releases jdx/mise\nx64 = '{}' # upd: checksum v asset=mise-{{tag}}-linux-x64.tar.gz\narm = '{}' # upd: checksum v asset=mise-{{tag}}-linux-arm64.tar.gz\n",
            "a".repeat(64),
            "c".repeat(64)
        );
        let (result, written) = run_generic(&original, "pins.toml", registry(), false).await;
        assert_eq!(written, original);
        assert!(result.updated.is_empty() && result.checksum_updates.is_empty());
        assert_eq!(result.skipped[0].reason, "checksum-unavailable");
    }

    #[tokio::test]
    async fn invalid_generic_hash_protects_its_version_while_unrelated_groups_update() {
        let original = format!(
            "mise = '2025.12.9' # upd: github-releases jdx/mise\nsha = 'invalid' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz\nother = '1.0.0' # upd: github-releases acme/tool\nsha = '{}' # upd: checksum other asset=tool-{{tag}}.zip\n",
            "c".repeat(64)
        );
        let registry = registry()
            .with_version("acme/tool", "v1.1.0")
            .with_release_digest("acme/tool", "v1.1.0", "tool-v1.1.0.zip", &"d".repeat(64));
        let (result, written) = run_generic(&original, "pins.toml", registry, false).await;
        assert_eq!(result.updated.len(), 1, "{result:?}");
        assert_eq!(result.updated[0].0, "acme/tool");
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].reason, "checksum-invalid");
        assert_eq!(
            written,
            original
                .replace("1.0.0", "1.1.0")
                .replace(&"c".repeat(64), &"d".repeat(64))
        );
    }

    #[tokio::test]
    async fn explicit_docker_ids_preserve_visibility_and_reject_duplicate_ids() {
        let original = fixture()
            .replace(
                "github-releases jdx/mise",
                "github-releases jdx/mise id=mise",
            )
            .replace("checksum MISE_VERSION", "checksum mise");
        let (result, written) = run(&original, registry(), UpdateOptions::new(false, false)).await;
        assert_eq!(result.updated.len(), 1, "{result:?}");
        assert!(written.contains("2026.9.1"));
        for content in [
            original.replace(
                "\r\n\r\n# upd: checksum",
                "\r\nFROM alpine:3.22\r\n# upd: checksum",
            ),
            format!(
                "{original}\r\n# upd: github-releases jdx/mise id=mise\r\nARG OTHER_VERSION=2025.12.9\r\n"
            ),
            format!(
                "{original}\r\n# upd: github-releases jdx/mise id=mise\r\nARG mise=2025.12.9\r\n"
            ),
        ] {
            let (result, written) =
                run(&content, registry(), UpdateOptions::new(false, false)).await;
            assert_eq!(written, content, "{result:?}");
            assert!(
                result.updated.is_empty() && !result.skipped.is_empty(),
                "{result:?}"
            );
        }
    }

    #[tokio::test]
    async fn unchanged_generic_versions_never_silently_repair_a_digest() {
        let original = format!(
            "v = '2026.9.1' # upd: github-releases jdx/mise\nsha = '{}' # upd: checksum v asset=mise-{{tag}}-linux-x64.tar.gz",
            "a".repeat(64)
        );
        let (result, written) = run_generic(&original, "pins.toml", registry(), false).await;
        assert_eq!(written, original);
        assert!(result.updated.is_empty() && result.checksum_updates.is_empty());
        assert_eq!(result.skipped[0].reason, "checksum-mismatch");
    }

    #[tokio::test]
    async fn batch_apply_preserves_edits_made_while_checksum_metadata_is_loading() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        for (name, original) in [
            ("Dockerfile", fixture()),
            (
                "pins.toml",
                format!(
                    "version = '2025.12.9' # upd: github-releases jdx/mise id=mise\nsha = '{}' # upd: checksum mise asset=mise-{{tag}}-linux-x64.tar.gz\n",
                    "a".repeat(64)
                ),
            ),
        ] {
            let server = MockServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join(name);
            std::fs::write(&file, &original).unwrap();
            let edited = format!("{original}\n# user edit while network requests are in flight\n");
            let file_for_response = file.clone();
            let edited_for_response = edited.clone();
            Mock::given(method("GET"))
                .and(path("/repos/jdx/mise/releases/latest"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"tag_name":"v2026.9.1"})),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET")).and(path("/repos/jdx/mise/releases/tags/v2026.9.1"))
                .respond_with(move |_: &wiremock::Request| {
                    std::fs::write(&file_for_response, &edited_for_response).unwrap();
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"assets":[
                        {"name":"mise-v2026.9.1-linux-x64.tar.gz", "state":"uploaded", "digest":format!("sha256:{}", "b".repeat(64))}
                    ]}))
                }).expect(1).mount(&server).await;
            let updater = AnnotatedUpdater::new(RegistrySet::with_single(
                AnnotationSource::GitHubReleases,
                Arc::new(GitHubReleasesRegistry::with_api_url(server.uri())),
            ));
            let docker = DockerUpdater::new();
            let owner = (name == "Dockerfile").then_some(&docker as &dyn OwnsLines);
            let result = updater
                .run(&file, UpdateOptions::new(false, false), owner)
                .await
                .unwrap();
            assert_eq!(std::fs::read_to_string(file).unwrap(), edited);
            assert!(
                result.updated.is_empty()
                    && result.pinned.is_empty()
                    && result.checksum_updates.is_empty(),
                "{result:?}"
            );
            assert_eq!(result.errors.len(), 1);
            assert!(
                result.errors[0].contains("changed while resolving"),
                "{result:?}"
            );
        }
    }

    #[tokio::test]
    async fn native_workflow_lines_cannot_be_used_as_generic_checksum_values() {
        let original = format!(
            "env:\n  VERSION: '2025.12.9' # upd: github-releases jdx/mise\nsteps:\n  - uses: jdx/mise@{} # upd: checksum VERSION asset=mise-{{tag}}-linux-x64.tar.gz\n",
            "a".repeat(64)
        );
        let (result, written) =
            run_generic(&original, ".github/workflows/check.yml", registry(), false).await;
        assert_eq!(written, original);
        assert!(
            result.updated.is_empty() && result.checksum_updates.is_empty(),
            "{result:?}"
        );
        assert_eq!(result.skipped[0].reason, "checksum-invalid");
    }

    async fn run(
        content: &str,
        registry: MockRegistry,
        options: UpdateOptions,
    ) -> (UpdateResult, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Dockerfile");
        std::fs::write(&path, content).unwrap();
        let updater = AnnotatedUpdater::new(RegistrySet::with_single(
            AnnotationSource::GitHubReleases,
            Arc::new(registry),
        ));
        let result = updater
            .update_alongside(&path, options, &DockerUpdater::new())
            .await
            .unwrap();
        (result, std::fs::read_to_string(path).unwrap())
    }

    #[tokio::test]
    async fn linked_digest_updates_preserve_quotes_crlf_and_missing_final_newline() {
        let original = fixture();
        let (result, written) = run(
            &original,
            registry(),
            UpdateOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            written,
            original
                .replace("2025.12.9", "2026.9.1")
                .replace(&"a".repeat(64), &"b".repeat(64))
        );
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.checksum_updates.len(), 1);
        assert_eq!(result.checksum_updates[0].tag, "v2026.9.1");
        assert_eq!(result.checksum_updates[0].source, "github-asset-digest");
        let report = crate::output::build_update_file_report(
            Path::new("Dockerfile"),
            FileType::Dockerfile,
            &result,
            None,
            |_, _| "minor",
        );
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["checksum_updates"][0]["version_line"], 3);
        assert_eq!(json["checksum_updates"][0]["line"], 6);
        assert_eq!(json["checksum_updates"][0]["current"], "a".repeat(64));
        assert_eq!(json["checksum_updates"][0]["latest"], "b".repeat(64));
        assert_eq!(json["checksum_updates"][0]["source"], "github-asset-digest");
        assert_eq!(json["checksum_updates"][0]["change"], "changed");
        assert!(json["checksum_updates"][0].get("preview").is_none());
    }

    #[tokio::test]
    async fn verified_unchanged_companion_is_reported_and_still_required_by_the_plan() {
        let original = fixture().replace(&"a".repeat(64), &"b".repeat(64));
        let (result, written) = run(
            &original,
            registry(),
            UpdateOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(written, original.replace("2025.12.9", "2026.9.1"));
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.checksum_updates.len(), 1);
        let json = serde_json::to_value(crate::output::build_update_file_report(
            Path::new("Dockerfile"),
            FileType::Dockerfile,
            &result,
            None,
            |_, _| "minor",
        ))
        .unwrap();
        assert_eq!(json["checksum_updates"][0]["change"], "verified_unchanged");
        assert_eq!(
            json["checksum_updates"][0]["current"],
            json["checksum_updates"][0]["latest"]
        );
        let selected = [("jdx/mise", "2025.12.9", "2026.9.1", Some(3))];
        assert!(
            validate_checksum_plan(
                &original,
                FileType::Dockerfile,
                &selected,
                &result.checksum_updates
            )
            .is_ok()
        );
        assert!(validate_checksum_plan(&original, FileType::Dockerfile, &selected, &[]).is_err());
    }

    #[tokio::test]
    async fn dry_run_resolves_the_same_group_without_writing() {
        let original = fixture();
        let (result, written) = run(
            &original,
            registry(),
            UpdateOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(written, original);
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.checksum_updates.len(), 1);
    }

    #[tokio::test]
    async fn one_missing_architecture_leaves_the_entire_group_untouched() {
        let original = format!(
            "{}\r\n# upd: checksum MISE_VERSION asset=mise-v{{version}}-linux-arm64.tar.gz\r\nARG ARM_CHECKSUM={}\r\n",
            fixture(),
            "c".repeat(64)
        );
        let (result, written) = run(
            &original,
            registry(),
            UpdateOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(written, original);
        assert!(result.updated.is_empty());
        assert!(result.checksum_updates.is_empty());
        assert_eq!(result.skipped[0].reason, "checksum-unavailable");
    }

    #[tokio::test]
    async fn multiple_architectures_move_together() {
        let original = format!(
            "{}\r\n# upd: checksum MISE_VERSION asset=mise-{{tag}}-linux-arm64.tar.gz\r\nARG ARM_CHECKSUM={}\r\n",
            fixture(),
            "c".repeat(64)
        );
        let registry = registry().with_release_digest(
            "jdx/mise",
            "v2026.9.1",
            "mise-v2026.9.1-linux-arm64.tar.gz",
            &"d".repeat(64),
        );
        let (result, written) = run(
            &original,
            registry,
            UpdateOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await;
        assert!(written.contains(&"b".repeat(64)));
        assert!(written.contains(&"d".repeat(64)));
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.checksum_updates.len(), 2);
    }

    #[tokio::test]
    async fn explicit_manifest_selects_exact_filename() {
        let original = fixture().replace(
            "linux-x64.tar.gz\r\n",
            "linux-x64.tar.gz checksums=SHASUMS256.txt\r\n",
        );
        let registry = registry().with_release_asset(
            "jdx/mise",
            "v2026.9.1",
            "SHASUMS256.txt",
            format!(
                "{}  ./mise-v2026.9.1-linux-arm64.tar.gz\n{} *mise-v2026.9.1-linux-x64.tar.gz\n",
                "c".repeat(64),
                "b".repeat(64)
            )
            .as_bytes(),
        );
        let (result, written) = run(
            &original,
            registry,
            UpdateOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(result.checksum_updates.len(), 1);
        assert!(written.contains(&"b".repeat(64)));
        assert!(!written.contains(&"c".repeat(64)));
    }

    #[tokio::test]
    async fn bad_relationships_and_directives_cannot_move_the_version() {
        for original in [
            fixture().replace(
                "asset=mise-v{version}-linux-x64.tar.gz",
                "asset=mise-v{unknown}-linux-x64.tar.gz",
            ),
            fixture().replace("asset=mise-v{version}-linux-x64.tar.gz", "asset=x extra=y"),
            fixture().replace("upd: checksum MISE_VERSION", "upd: checksum MISE_VERSOIN"),
            fixture().replace(
                "asset=mise-v{version}-linux-x64.tar.gz",
                "asset=x # upd: github-releases acme/tool",
            ),
            fixture()
                .replace("upd: checksum", "upd:  checksum")
                .replace("asset=mise-v{version}-linux-x64.tar.gz", "asset=x extra=y"),
            fixture().replace(&"a".repeat(64), "broken"),
            format!("{}\r\nARG MISE_VERSION=2025.12.9\r\n", fixture()),
            format!("{}\r\nARG MISE_VERSION\r\n", fixture()),
            format!("{}\r\nARG MISE_CHECKSUM={}\r\n", fixture(), "a".repeat(64)),
            fixture().replace("\r\nENV MISE_CHECKSUM", "\r\n\r\nENV MISE_CHECKSUM"),
        ] {
            let (result, written) = run(
                &original,
                registry(),
                UpdateOptions {
                    dry_run: false,
                    ..Default::default()
                },
            )
            .await;
            assert_eq!(written, original);
            assert!(result.updated.is_empty());
            assert!(!result.warnings.is_empty(), "{original}");
            assert_eq!(result.skipped[0].reason, "checksum-invalid", "{original}");
        }
    }

    #[tokio::test]
    async fn unchanged_version_reports_mismatch_without_repairing_it() {
        let original = fixture().replace("2025.12.9", "2026.9.1");
        let (result, written) = run(
            &original,
            registry(),
            UpdateOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(written, original);
        assert!(result.updated.is_empty());
        assert_eq!(result.skipped[0].reason, "checksum-mismatch");
        assert_eq!(result.unchanged, 0);
    }

    #[tokio::test]
    async fn pinned_version_resolves_actual_tag_and_checksum() {
        let original = fixture();
        let registry = registry().with_ref_names("jdx/mise", &["v2026.9.1"]);
        let options = UpdateOptions {
            dry_run: false,
            config: Some(Arc::new(crate::config::UpdConfig {
                pin: HashMap::from([("jdx/mise".into(), "2026.9.1".into())]),
                ..Default::default()
            })),
            ..Default::default()
        };
        let (result, written) = run(&original, registry, options).await;
        assert_eq!(result.pinned.len(), 1);
        assert_eq!(result.checksum_updates.len(), 1);
        assert!(written.contains("MISE_VERSION=2026.9.1"));
    }

    #[tokio::test]
    async fn a_configured_pin_cannot_select_a_shortened_release_for_checksums() {
        let original = fixture().replace("2025.12.9", "2025.12");
        let registry = registry().with_ref_names("jdx/mise", &["v2026.9"]);
        let options =
            UpdateOptions::new(false, false).with_config(Arc::new(crate::config::UpdConfig {
                pin: HashMap::from([("jdx/mise".into(), "2026.9.1".into())]),
                ..Default::default()
            }));
        let (result, written) = run(&original, registry, options).await;
        assert_eq!(written, original);
        assert!(result.pinned.is_empty());
        assert_eq!(result.skipped[0].reason, "checksum-unavailable");
        assert!(result.skipped[0].message.contains("--full-precision"));
    }

    #[tokio::test]
    async fn docker_only_selection_does_not_fetch_or_change_checksums() {
        let original = fixture();
        let (result, written) = run(
            &original,
            MockRegistry::new("github-releases"),
            UpdateOptions {
                dry_run: false,
                langs: vec![Lang::Docker],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(written, original);
        assert!(result.skipped.is_empty());
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn shortened_release_version_blocks_until_full_precision_selected() {
        let original = fixture().replace("2025.12.9", "2025.12");
        let (result, written) = run(
            &original,
            registry(),
            UpdateOptions {
                dry_run: false,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(written, original);
        assert_eq!(result.skipped[0].reason, "checksum-unavailable");
        let (result, written) = run(
            &original,
            registry(),
            UpdateOptions {
                dry_run: false,
                full_precision: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(result.updated.len(), 1);
        assert!(written.contains("MISE_VERSION=2026.9.1"));
    }

    #[tokio::test]
    async fn ignored_filtered_and_capped_groups_do_not_attempt_checksum_resolution() {
        let original = fixture();
        let ignored =
            UpdateOptions::new(false, false).with_config(Arc::new(crate::config::UpdConfig {
                ignore: vec!["jdx/mise".into()],
                ..Default::default()
            }));
        let filtered = UpdateOptions::new(false, false).with_packages(vec!["other/tool".into()]);
        let capped =
            UpdateOptions::new(false, false).with_bump_filter(crate::updater::BumpFilter {
                major: false,
                minor: true,
                patch: true,
            });
        for options in [ignored, filtered, capped] {
            let (result, written) = run(
                &original,
                MockRegistry::new("github-releases").with_version("jdx/mise", "v2026.9.1"),
                options,
            )
            .await;
            assert_eq!(written, original);
            assert!(result.checksum_updates.is_empty());
            assert!(result.updated.is_empty());
            assert!(result.skipped.is_empty());
            assert!(result.errors.is_empty());
        }
    }

    #[tokio::test]
    async fn blocked_checksum_group_does_not_prevent_unrelated_versions_from_updating() {
        let original = format!(
            "{}\r\n# upd: github-releases acme/tool\r\nARG TOOL_VERSION=1.0.0\r\n",
            fixture()
        );
        let registry = MockRegistry::new("github-releases")
            .with_version("jdx/mise", "v2026.9.1")
            .with_version("acme/tool", "v1.1.0");
        let (result, written) = run(&original, registry, UpdateOptions::new(false, false)).await;
        assert_eq!(
            written,
            original.replace("TOOL_VERSION=1.0.0", "TOOL_VERSION=1.1.0")
        );
        assert_eq!(result.updated[0].0, "acme/tool");
        assert_eq!(result.skipped[0].package, "jdx/mise");
        assert!(result.checksum_updates.is_empty());
    }

    #[tokio::test]
    async fn heredoc_and_continuation_bodies_cannot_create_variable_declarations_or_stages() {
        for body in [
            "RUN <<'SCRIPT'\nARG MISE_VERSION=0.0.0\nARG MISE_CHECKSUM=broken\nFROM ubuntu AS fake\n# upd: checksum MISE_VERSION asset=x-{unknown}\nSCRIPT\n",
            "RUN echo hello \\\n    ARG MISE_VERSION=0.0.0\n",
            "COPY <<-EOF /tmp/example\n\tARG MISE_VERSION=0.0.0\n\tEOF\n",
        ] {
            let original = fixture().replacen(
                "FROM alpine:3.22\r\n",
                &format!("FROM alpine:3.22\r\n{body}"),
                1,
            );
            let (result, written) =
                run(&original, registry(), UpdateOptions::new(false, false)).await;
            assert_eq!(result.updated.len(), 1, "{result:?}");
            assert!(result.warnings.is_empty(), "{result:?}");
            assert_eq!(
                written,
                original
                    .replace("MISE_VERSION=2025.12.9", "MISE_VERSION=2026.9.1")
                    .replace(&"a".repeat(64), &"b".repeat(64))
            );
        }
    }

    #[tokio::test]
    async fn multi_assignment_and_multiline_env_shadowing_cannot_hide_from_link_validation() {
        for shadow in [
            "ENV OTHER=ok MISE_VERSION=2.0.0\r\n",
            "ENV OTHER=ok \\\n    MISE_VERSION=2.0.0\r\n",
            "ENV OTHER=ok MISE_VER\\\nSION=2.0.0\r\n",
            "ENV MISE_VERSION 2.0.0\r\n",
        ] {
            let original = fixture().replace(
                "ARG MISE_VERSION=2025.12.9\r\n",
                &format!("ARG MISE_VERSION=2025.12.9\r\n{shadow}"),
            );
            let (result, written) =
                run(&original, registry(), UpdateOptions::new(false, false)).await;
            assert_eq!(written, original, "{shadow}");
            assert!(result.updated.is_empty());
            assert_eq!(result.skipped[0].reason, "checksum-invalid", "{shadow}");
        }
        let original = fixture().replace(
            "ARG MISE_VERSION=2025.12.9\r\n",
            "ARG MISE_VERSION=2025.12.9\r\nENV NOTE=\"ignore MISE_VERSION=2.0.0\" OTHER=ok\r\n",
        );
        let (result, _) = run(&original, registry(), UpdateOptions::new(false, false)).await;
        assert_eq!(
            result.updated.len(),
            1,
            "quoted values must not become variable declarations"
        );
    }

    #[tokio::test]
    async fn composed_image_and_annotation_updates_retain_their_distinct_line_sources() {
        let original = fixture().replace("FROM alpine:3.22", "FROM jdx/mise:2025.12.9");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Dockerfile");
        std::fs::write(&path, &original).unwrap();
        let annotated = AnnotatedUpdater::new(RegistrySet::with_single(
            AnnotationSource::GitHubReleases,
            Arc::new(registry()),
        ));
        let image_registry = MockRegistry::new("docker").with_version(
            &crate::registry::DockerRegistry::lookup_key("jdx/mise", "2025.12.9"),
            "2026.9.1",
        );
        let result = crate::updater::update_with_annotations(
            &DockerUpdater::new(),
            &annotated,
            &path,
            &image_registry,
            UpdateOptions::new(true, false),
        )
        .await
        .unwrap();
        assert_eq!(result.updated.len(), 2, "{result:?}");
        assert!(!result.annotation_sources.contains_key(&1));
        assert_eq!(
            result.annotation_sources.get(&3),
            Some(&AnnotationSource::GitHubReleases)
        );
        let report = serde_json::to_value(crate::output::build_update_file_report(
            &path,
            FileType::Dockerfile,
            &result,
            None,
            |_, _| "major",
        ))
        .unwrap();
        assert!(report["updates"][0]["source"].is_null(), "{report:#}");
        assert_eq!(report["updates"][1]["source"], "github-releases");
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn independent_stages_can_reuse_version_and_checksum_names() {
        let original = format!(
            "{}\r\nFROM ubuntu:24.04 AS other\n# upd: github-releases acme/tool\nARG MISE_VERSION=1.0.0\n# upd: checksum MISE_VERSION asset=tool-{{tag}}.tar.gz\nENV MISE_CHECKSUM={}\n",
            fixture(),
            "c".repeat(64)
        );
        let registry = registry()
            .with_version("acme/tool", "v1.1.0")
            .with_release_digest("acme/tool", "v1.1.0", "tool-v1.1.0.tar.gz", &"d".repeat(64));
        let (result, written) = run(&original, registry, UpdateOptions::new(false, false)).await;
        assert_eq!(result.updated.len(), 2);
        assert_eq!(result.checksum_updates.len(), 2);
        assert!(result.skipped.is_empty());
        assert_eq!(
            written,
            original
                .replace("MISE_VERSION=2025.12.9", "MISE_VERSION=2026.9.1")
                .replace("MISE_VERSION=1.0.0", "MISE_VERSION=1.1.0")
                .replace(&"a".repeat(64), &"b".repeat(64))
                .replace(&"c".repeat(64), &"d".repeat(64))
        );
    }

    #[tokio::test]
    async fn an_ambiguous_stage_does_not_block_an_independent_stage_reusing_the_name() {
        let original = format!(
            "{}\r\nARG MISE_VERSION=0.0.0\r\nFROM ubuntu:24.04 AS other\n# upd: github-releases acme/tool\nARG MISE_VERSION=1.0.0\n# upd: checksum MISE_VERSION asset=tool-{{tag}}.tar.gz\nARG OTHER_SUM={}\n",
            fixture(),
            "c".repeat(64)
        );
        let registry = registry()
            .with_version("acme/tool", "v1.1.0")
            .with_release_digest("acme/tool", "v1.1.0", "tool-v1.1.0.tar.gz", &"d".repeat(64));
        let (result, written) = run(&original, registry, UpdateOptions::new(false, false)).await;
        assert_eq!(
            written,
            original
                .replace("MISE_VERSION=1.0.0", "MISE_VERSION=1.1.0")
                .replace(&"c".repeat(64), &"d".repeat(64))
        );
        assert_eq!(result.updated.len(), 1);
        assert_eq!(result.updated[0].0, "acme/tool");
        assert_eq!(result.skipped[0].package, "jdx/mise");
    }

    #[tokio::test]
    async fn consumed_global_args_and_inherited_stages_share_one_atomic_group() {
        for stage in [
            "FROM build AS child\n",
            "FROM alpine:3.22 AS child\nARG MISE_VERSION\n",
            "FROM 0 AS child\n",
            "FROM --platform=linux/amd64 \\\n    build AS child\n",
        ] {
            let original = format!(
                "# upd: github-releases jdx/mise\nARG MISE_VERSION=2025.12.9\nFROM alpine:3.22 AS build\nARG MISE_VERSION\n# upd: checksum MISE_VERSION asset=mise-v{{version}}-linux-x64.tar.gz\nARG SUM_X64={}\n{stage}# upd: checksum MISE_VERSION asset=mise-v{{version}}-linux-arm64.tar.gz\nARG SUM_ARM={}\n",
                "a".repeat(64),
                "c".repeat(64)
            );
            let complete = registry().with_release_digest(
                "jdx/mise",
                "v2026.9.1",
                "mise-v2026.9.1-linux-arm64.tar.gz",
                &"d".repeat(64),
            );
            let (result, written) =
                run(&original, complete, UpdateOptions::new(false, false)).await;
            assert_eq!(result.updated.len(), 1, "{stage}: {result:?}");
            assert_eq!(result.checksum_updates.len(), 2);
            assert!(written.contains(&"b".repeat(64)) && written.contains(&"d".repeat(64)));
            let (result, written) =
                run(&original, registry(), UpdateOptions::new(false, false)).await;
            assert_eq!(written, original);
            assert!(result.updated.is_empty());
            assert!(result.checksum_updates.is_empty());
            assert_eq!(result.skipped[0].reason, "checksum-unavailable");
        }
    }

    #[tokio::test]
    async fn a_global_arg_without_stage_consumption_is_blocked() {
        let original = format!(
            "# upd: github-releases jdx/mise\nARG MISE_VERSION=2025.12.9\nFROM alpine:3.22\n# upd: checksum MISE_VERSION asset=mise-v{{version}}-linux-x64.tar.gz\nARG SUM={}\n",
            "a".repeat(64)
        );
        let (result, written) = run(&original, registry(), UpdateOptions::new(false, false)).await;
        assert_eq!(written, original);
        assert_eq!(result.skipped[0].reason, "checksum-invalid");
        assert!(result.skipped[0].message.contains("not visible"));
    }

    #[tokio::test]
    async fn manifest_entries_cannot_substitute_for_missing_release_archives_or_conflict_with_digests()
     {
        let original = fixture().replace(
            "linux-x64.tar.gz\r\n",
            "linux-x64.tar.gz checksums=SHASUMS256.txt\r\n",
        );
        let manifest = format!("{}  mise-v2026.9.1-linux-x64.tar.gz\n", "c".repeat(64));
        let absent = MockRegistry::new("github-releases")
            .with_version("jdx/mise", "v2026.9.1")
            .with_release_asset(
                "jdx/mise",
                "v2026.9.1",
                "SHASUMS256.txt",
                manifest.as_bytes(),
            );
        let conflicting = registry().with_release_asset(
            "jdx/mise",
            "v2026.9.1",
            "SHASUMS256.txt",
            manifest.as_bytes(),
        );
        for (registry, expected) in [
            (absent, "cannot verify release archive"),
            (conflicting, "conflicting SHA-256"),
        ] {
            let (result, written) =
                run(&original, registry, UpdateOptions::new(false, false)).await;
            assert_eq!(written, original);
            assert!(result.updated.is_empty() && result.checksum_updates.is_empty());
            assert!(result.skipped[0].message.contains(expected), "{result:?}");
        }
    }

    #[tokio::test]
    async fn explicit_manifest_supports_existing_archives_without_github_digest_metadata() {
        let original = fixture().replace(
            "linux-x64.tar.gz\r\n",
            "linux-x64.tar.gz checksums=SHASUMS256.txt\r\n",
        );
        let registry = MockRegistry::new("github-releases")
            .with_version("jdx/mise", "v2026.9.1")
            .with_release_asset(
                "jdx/mise",
                "v2026.9.1",
                "mise-v2026.9.1-linux-x64.tar.gz",
                b"",
            )
            .with_release_asset(
                "jdx/mise",
                "v2026.9.1",
                "SHASUMS256.txt",
                format!("{}  mise-v2026.9.1-linux-x64.tar.gz\n", "b".repeat(64)).as_bytes(),
            );
        let (result, written) = run(&original, registry, UpdateOptions::new(false, false)).await;
        assert_eq!(result.updated.len(), 1);
        assert!(written.contains(&"b".repeat(64)));
    }

    #[test]
    fn manifest_formats_are_strict_and_filename_specific() {
        let digest = "b".repeat(64);
        for text in [
            format!("{digest}  ./asset.tar.gz\n"),
            format!("{digest} *asset.tar.gz\n"),
            format!("SHA256 (asset.tar.gz) = {digest}\n"),
        ] {
            assert_eq!(
                manifest_digest(text.as_bytes(), "asset.tar.gz", "SUMS").unwrap(),
                digest
            );
        }
        assert!(manifest_digest(digest.as_bytes(), "asset.tar.gz", "SUMS").is_err());
        assert_eq!(
            manifest_digest(digest.as_bytes(), "asset.tar.gz", "asset.tar.gz.sha256").unwrap(),
            digest
        );
        assert!(
            manifest_digest(
                format!("{digest}  asset.tar.gz\n{digest}  asset.tar.gz\n").as_bytes(),
                "asset.tar.gz",
                "SUMS"
            )
            .is_err()
        );
        assert!(
            manifest_digest(
                format!("{digest}  other-asset.tar.gz\n").as_bytes(),
                "asset.tar.gz",
                "SUMS"
            )
            .is_err()
        );
        assert!(manifest_digest(b"broken  asset.tar.gz\n", "asset.tar.gz", "SUMS").is_err());
    }
}
